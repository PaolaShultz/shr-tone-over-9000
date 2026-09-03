use crate::params::{ParamId, Parameters};
use anyhow::{bail, Context, Result};
use midir::{Ignore, MidiInput, MidiInputConnection};
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::mpsc::{self, Receiver, TryRecvError};

const PICKUP_TOLERANCE: f32 = 1.0 / 127.0 + f32::EPSILON;

#[derive(Clone, Copy, Debug)]
struct RawMidi {
    status: u8,
    data1: u8,
    data2: u8,
}

#[derive(Clone, Copy, Debug)]
struct Binding {
    parameter: ParamId,
    relative: bool,
}

#[derive(Clone, Copy, Debug)]
struct PickupControl {
    target: f32,
    previous: Option<f32>,
    caught: bool,
}

#[derive(Debug)]
pub enum MidiAction {
    Select(i8),
    Load,
    Parameter(ParamId),
    Learned {
        parameter: ParamId,
        channel: u8,
        cc: u8,
    },
    OversamplingOff,
}

#[derive(Default)]
struct ControllerConfig {
    input: Option<String>,
    _profile: Option<String>,
    encoder_relative_cc: Option<u8>,
    encoder_relative_reverse: bool,
    encoder_press_cc: Option<u8>,
    encoder_press_note: Option<u8>,
    encoder_press_channel: Option<u8>,
    rotaries: HashMap<u8, u8>,
}

pub struct MidiController {
    config: ControllerConfig,
    _connection: Option<MidiInputConnection<()>>,
    receiver: Receiver<RawMidi>,
    device_name: Option<String>,
    startup_error: Option<String>,
    configured_bindings: HashMap<u8, Binding>,
    learned_bindings: HashMap<(u8, u8), Binding>,
    pickup: HashMap<(u8, u8), PickupControl>,
    learning: Option<ParamId>,
    last_cc: Option<(u8, u8)>,
}

impl MidiController {
    pub fn open(path: &Path, override_input: Option<&str>) -> Result<Self> {
        let config = ControllerConfig::load(path)?;
        let wanted = override_input
            .map(str::to_owned)
            .or_else(|| config.input.clone());
        let (sender, receiver) = mpsc::channel();
        let (connection, device_name, startup_error) = match wanted.as_deref() {
            Some(wanted) => match connect(wanted, sender) {
                Ok((connection, name)) => (Some(connection), Some(name), None),
                Err(error) => (None, None, Some(error.to_string())),
            },
            None => (None, None, None),
        };
        let configured_bindings = config
            .rotaries
            .iter()
            .filter_map(|(&cc, &position)| {
                parameter_for_rotary(position).map(|parameter| {
                    (
                        cc,
                        Binding {
                            parameter,
                            relative: true,
                        },
                    )
                })
            })
            .collect();
        Ok(Self {
            config,
            _connection: connection,
            receiver,
            device_name,
            startup_error,
            configured_bindings,
            learned_bindings: HashMap::new(),
            pickup: HashMap::new(),
            learning: None,
            last_cc: None,
        })
    }

    pub fn device_label(&self) -> &str {
        if self.startup_error.is_some() {
            return "OFF";
        }
        self.device_name.as_deref().unwrap_or("--")
    }

    pub fn startup_error(&self) -> Option<&str> {
        self.startup_error.as_deref()
    }

    pub fn learning(&self) -> Option<ParamId> {
        self.learning
    }

    pub fn last_cc(&self) -> Option<(u8, u8)> {
        self.last_cc
    }

    pub fn enter_learn(&mut self, parameter: ParamId) {
        if parameter != ParamId::Oversampling {
            self.learning = Some(parameter);
        }
    }

    pub fn arm_pickup(&mut self, parameters: &Parameters) {
        for (&(channel, cc), binding) in &self.learned_bindings {
            self.pickup.insert(
                (channel, cc),
                PickupControl {
                    target: parameters.normalized(binding.parameter),
                    previous: None,
                    caught: false,
                },
            );
        }
    }

    pub fn next_action(&mut self, parameters: &Parameters) -> Option<MidiAction> {
        loop {
            let message = match self.receiver.try_recv() {
                Ok(message) => message,
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => return None,
            };
            let kind = message.status & 0xf0;
            let channel = message.status & 0x0f;

            if matches!(kind, 0x80 | 0x90 | 0xa0) {
                if self.encoder_press_note_matches(message) && kind == 0x90 && message.data2 > 0 {
                    return Some(MidiAction::Load);
                }
                // This app has no synth or MIDI output. Note on/off and pressure
                // are intentionally consumed here and never forwarded.
                continue;
            }
            if kind != 0xb0 {
                continue;
            }
            self.last_cc = Some((channel, message.data1));

            if let Some(parameter) = self.learning.take() {
                self.learned_bindings.insert(
                    (channel, message.data1),
                    Binding {
                        parameter,
                        relative: false,
                    },
                );
                self.pickup.insert(
                    (channel, message.data1),
                    PickupControl {
                        target: parameters.normalized(parameter),
                        previous: None,
                        caught: false,
                    },
                );
                return Some(MidiAction::Learned {
                    parameter,
                    channel,
                    cc: message.data1,
                });
            }

            if self.config.encoder_relative_cc == Some(message.data1) {
                if let Some(delta) =
                    relative_delta(message.data2, self.config.encoder_relative_reverse)
                {
                    return Some(MidiAction::Select(delta.signum()));
                }
                continue;
            }
            if self.config.encoder_press_cc == Some(message.data1)
                && channel_matches(self.config.encoder_press_channel, channel)
            {
                if message.data2 > 0 {
                    return Some(MidiAction::Load);
                }
                continue;
            }

            let binding = self
                .learned_bindings
                .get(&(channel, message.data1))
                .copied()
                .or_else(|| self.configured_bindings.get(&message.data1).copied());
            let Some(binding) = binding else {
                continue;
            };
            if binding.parameter == ParamId::Oversampling {
                return Some(MidiAction::OversamplingOff);
            }
            if binding.relative {
                if let Some(delta) =
                    relative_delta(message.data2, self.config.encoder_relative_reverse)
                {
                    apply_relative(parameters, binding.parameter, delta);
                    return Some(MidiAction::Parameter(binding.parameter));
                }
                continue;
            }
            let normalized = f32::from(message.data2) / 127.0;
            if !self.pickup_accepts((channel, message.data1), normalized) {
                continue;
            }
            parameters.set_normalized(binding.parameter, normalized);
            return Some(MidiAction::Parameter(binding.parameter));
        }
    }

    fn encoder_press_note_matches(&self, message: RawMidi) -> bool {
        self.config.encoder_press_note == Some(message.data1)
            && channel_matches(self.config.encoder_press_channel, message.status & 0x0f)
    }

    fn pickup_accepts(&mut self, key: (u8, u8), current: f32) -> bool {
        let Some(state) = self.pickup.get_mut(&key) else {
            return true;
        };
        if state.caught {
            return true;
        }
        let close = (current - state.target).abs() <= PICKUP_TOLERANCE;
        let crossed = state
            .previous
            .is_some_and(|previous| (previous - state.target) * (current - state.target) <= 0.0);
        state.previous = Some(current);
        state.caught = close || crossed;
        state.caught
    }
}

impl ControllerConfig {
    fn load(path: &Path) -> Result<Self> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
        };
        let mut config = Self::default();
        for (line_index, raw_line) in text.lines().enumerate() {
            let line = raw_line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (key, value) = line.split_once('=').with_context(|| {
                format!("{}:{}: expected KEY=VALUE", path.display(), line_index + 1)
            })?;
            let key = key.trim();
            let value = value.trim();
            match key {
                "input" => config.input = nonempty(value),
                "profile" => config._profile = nonempty(value),
                "encoder.relative_cc" => config.encoder_relative_cc = optional_midi(value, key)?,
                "encoder.relative_reverse" => {
                    config.encoder_relative_reverse = parse_bool(value, key)?
                }
                "encoder.press_cc" => config.encoder_press_cc = optional_midi(value, key)?,
                "encoder.press_note" => config.encoder_press_note = optional_midi(value, key)?,
                "encoder.press_channel" => {
                    config.encoder_press_channel = optional_channel(value, key)?
                }
                _ if key.starts_with("rotary.") => {
                    if value.is_empty() {
                        continue;
                    }
                    let position = key["rotary.".len()..]
                        .parse::<u8>()
                        .with_context(|| format!("{key} has an invalid rotary number"))?;
                    if !(2..=16).contains(&position) {
                        bail!("{key} must name physical rotary 2 through 16");
                    }
                    let cc = parse_midi(value, key)?;
                    config.rotaries.insert(cc, position);
                }
                // Preserve the shr-daw v9 shape even where this single-screen
                // app has no corresponding command surface.
                "menu.layout"
                | "encoder.modified_relative_cc"
                | "encoder.modified_relative_reverse"
                | "synth.press_cc"
                | "synth.press_note"
                | "synth.press_channel"
                | "encoder.secondary_press_cc"
                | "encoder.secondary_press_note"
                | "encoder.secondary_press_channel"
                | "encoder.modifier"
                | "lock.cc"
                | "page_cycle.modifier"
                | "page_cycle.trigger" => {}
                _ if key.starts_with("pad.") => {}
                _ => bail!(
                    "{}:{}: unknown controller key {key}",
                    path.display(),
                    line_index + 1
                ),
            }
        }
        Ok(config)
    }
}

fn connect(
    wanted: &str,
    sender: mpsc::Sender<RawMidi>,
) -> Result<(MidiInputConnection<()>, String)> {
    let mut input = MidiInput::new("rpi-tone-over-9000 MIDI input")?;
    input.ignore(Ignore::None);
    let ports = input.ports();
    let names = ports
        .iter()
        .map(|port| input.port_name(port).map_err(anyhow::Error::from))
        .collect::<Result<Vec<_>>>()?;
    let matches = names
        .iter()
        .enumerate()
        .filter_map(|(index, name)| {
            stable_identity(name)
                .eq_ignore_ascii_case(&stable_identity(wanted))
                .then_some(index)
        })
        .collect::<Vec<_>>();
    let index = match matches.as_slice() {
        [index] => *index,
        [] => bail!("MIDI input {wanted:?} is offline"),
        _ => bail!("MIDI input {wanted:?} is ambiguous"),
    };
    let name = stable_identity(&names[index]);
    let connection = input
        .connect(
            &ports[index],
            "rpi-tone-over-9000 monitor",
            move |_stamp, message, _| {
                if message.len() >= 3 {
                    let _ = sender.send(RawMidi {
                        status: message[0],
                        data1: message[1],
                        data2: message[2],
                    });
                }
            },
            (),
        )
        .map_err(|error| anyhow::anyhow!("connect MIDI input: {error}"))?;
    Ok((connection, name))
}

fn stable_identity(name: &str) -> String {
    let trimmed = name.trim();
    let Some((prefix, token)) = trimmed.rsplit_once(char::is_whitespace) else {
        return trimmed.to_owned();
    };
    let numeric_address = token.split_once(':').is_some_and(|(client, port)| {
        !client.is_empty()
            && !port.is_empty()
            && client.chars().all(|value| value.is_ascii_digit())
            && port.chars().all(|value| value.is_ascii_digit())
    });
    if numeric_address {
        prefix.trim_end().to_owned()
    } else {
        trimmed.to_owned()
    }
}

fn parameter_for_rotary(position: u8) -> Option<ParamId> {
    match position {
        2 => Some(ParamId::InputGain),
        3 => Some(ParamId::OutputGain),
        4 => Some(ParamId::Bypass),
        5 => Some(ParamId::Oversampling),
        _ => None,
    }
}

fn relative_delta(value: u8, reverse: bool) -> Option<i8> {
    let delta = if reverse {
        match value {
            125..=127 => i16::from(value) - 128,
            1..=3 => i16::from(value),
            _ => 0,
        }
    } else {
        match value {
            61..=63 | 65..=67 => i16::from(value) - 64,
            _ => 0,
        }
    };
    (delta != 0).then_some(delta as i8)
}

fn apply_relative(parameters: &Parameters, parameter: ParamId, delta: i8) {
    match parameter {
        ParamId::InputGain | ParamId::OutputGain => {
            parameters.adjust_db(parameter, f32::from(delta) * 0.5)
        }
        ParamId::Bypass => parameters.set_normalized(parameter, if delta > 0 { 1.0 } else { 0.0 }),
        ParamId::Oversampling => {}
    }
}

fn nonempty(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_owned())
}

fn optional_midi(value: &str, key: &str) -> Result<Option<u8>> {
    if value.is_empty() {
        Ok(None)
    } else {
        parse_midi(value, key).map(Some)
    }
}

fn parse_midi(value: &str, key: &str) -> Result<u8> {
    let number = value
        .parse::<u8>()
        .with_context(|| format!("{key} must be a MIDI number 0 through 127"))?;
    if number > 127 {
        bail!("{key} must be a MIDI number 0 through 127");
    }
    Ok(number)
}

fn optional_channel(value: &str, key: &str) -> Result<Option<u8>> {
    if value.is_empty() {
        return Ok(None);
    }
    let channel = value
        .parse::<u8>()
        .with_context(|| format!("{key} must be a MIDI channel 1 through 16"))?;
    if !(1..=16).contains(&channel) {
        bail!("{key} must be a MIDI channel 1 through 16");
    }
    Ok(Some(channel - 1))
}

fn parse_bool(value: &str, key: &str) -> Result<bool> {
    match value {
        "true" | "yes" | "1" => Ok(true),
        "false" | "no" | "0" => Ok(false),
        _ => bail!("{key} must be true or false"),
    }
}

fn channel_matches(configured: Option<u8>, actual: u8) -> bool {
    configured.is_none_or(|channel| channel == actual)
}
