use crate::nam::NamChain;
use crate::params::{db_to_gain, Parameters};
use anyhow::{bail, Context, Result};
use jack::{
    AsyncClient, AudioIn, AudioOut, Client, ClientOptions, ClientStatus, Control,
    NotificationHandler, Port, ProcessHandler, ProcessScope,
};
use rtrb::{Consumer, Producer, PushError, RingBuffer};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

pub const SAMPLE_RATE: u32 = 48_000;

#[derive(Clone)]
pub struct AudioTelemetry {
    inner: Arc<TelemetryInner>,
}

impl AudioTelemetry {
    pub fn snapshot(&self) -> TelemetrySnapshot {
        TelemetrySnapshot {
            input_peak: self.inner.input_peak.load(),
            input_rms: self.inner.input_rms.load(),
            output_peak: self.inner.output_peak.load(),
            output_rms: self.inner.output_rms.load(),
            xruns: self.inner.xruns.load(Ordering::Relaxed),
            process_fault: self.inner.process_fault.load(Ordering::Relaxed),
            server_lost: self.inner.server_lost.load(Ordering::Relaxed),
            chain_len: self.inner.chain_len.load(Ordering::Relaxed),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct TelemetrySnapshot {
    pub input_peak: f32,
    pub input_rms: f32,
    pub output_peak: f32,
    pub output_rms: f32,
    pub xruns: u64,
    pub process_fault: bool,
    pub server_lost: bool,
    pub chain_len: u32,
}

struct TelemetryInner {
    input_peak: AtomicF32,
    input_rms: AtomicF32,
    output_peak: AtomicF32,
    output_rms: AtomicF32,
    xruns: AtomicU64,
    process_fault: AtomicBool,
    server_lost: AtomicBool,
    chain_len: AtomicU32,
}

impl Default for TelemetryInner {
    fn default() -> Self {
        Self {
            input_peak: AtomicF32::new(0.0),
            input_rms: AtomicF32::new(0.0),
            output_peak: AtomicF32::new(0.0),
            output_rms: AtomicF32::new(0.0),
            xruns: AtomicU64::new(0),
            process_fault: AtomicBool::new(false),
            server_lost: AtomicBool::new(false),
            chain_len: AtomicU32::new(0),
        }
    }
}

type Active = AsyncClient<AudioNotifications, AudioProcess>;

pub struct AudioClient {
    active: Option<Active>,
    command_tx: Producer<AudioCommand>,
    retired_rx: Consumer<NamChain>,
    telemetry: AudioTelemetry,
    period_frames: u32,
    input_port_name: String,
    output_port_name: String,
    capture_port: String,
    playback_port: String,
}

impl AudioClient {
    pub fn start(
        client_name: &str,
        requested_period: Option<u32>,
        capture_port: String,
        playback_port: String,
        params: Arc<Parameters>,
    ) -> Result<Self> {
        let (client, status) = Client::new(client_name, ClientOptions::NO_START_SERVER)
            .context("open the already-running JACK server")?;
        if status.contains(ClientStatus::NAME_NOT_UNIQUE) {
            bail!("JACK client name {client_name:?} is already in use");
        }
        if client.sample_rate() != SAMPLE_RATE {
            bail!(
                "JACK is {} Hz; this v1 requires {} Hz",
                client.sample_rate(),
                SAMPLE_RATE
            );
        }
        let period = client.buffer_size();
        if !matches!(period, 128 | 256) {
            bail!("JACK period is {period} frames; supported periods are 128 and 256");
        }
        if let Some(requested_period) = requested_period {
            if period != requested_period {
                bail!(
                    "JACK period is {period} frames; requested {requested_period} (configure JACK before launch or omit --period to detect it automatically)"
                );
            }
        }

        let input = client
            .register_port("input", AudioIn::default())
            .context("register mono JACK input")?;
        let output = client
            .register_port("output", AudioOut::default())
            .context("register mono JACK output")?;
        let input_port_name = input.name().context("read JACK input port name")?;
        let output_port_name = output.name().context("read JACK output port name")?;

        let (command_tx, command_rx) = RingBuffer::new(4);
        let (retired_tx, retired_rx) = RingBuffer::new(2);
        let inner = Arc::new(TelemetryInner::default());
        let telemetry = AudioTelemetry {
            inner: Arc::clone(&inner),
        };
        let process = AudioProcess {
            input,
            output,
            scratch: vec![0.0; period as usize],
            expected_period: period,
            params,
            telemetry: Arc::clone(&inner),
            command_rx,
            retired_tx,
            active_chain: None,
            pending_retire: None,
        };
        let notifications = AudioNotifications {
            telemetry: Arc::clone(&inner),
        };
        let active = client
            .activate_async(notifications, process)
            .context("activate JACK client")?;

        if let Err(error) = active
            .as_client()
            .connect_ports_by_name(&capture_port, &input_port_name)
        {
            let _ = active.deactivate();
            return Err(error)
                .with_context(|| format!("connect JACK {capture_port} -> {input_port_name}"));
        }
        if let Err(error) = active
            .as_client()
            .connect_ports_by_name(&output_port_name, &playback_port)
        {
            let _ = active
                .as_client()
                .disconnect_ports_by_name(&capture_port, &input_port_name);
            let _ = active.deactivate();
            return Err(error)
                .with_context(|| format!("connect JACK {output_port_name} -> {playback_port}"));
        }

        Ok(Self {
            active: Some(active),
            command_tx,
            retired_rx,
            telemetry,
            period_frames: period,
            input_port_name,
            output_port_name,
            capture_port,
            playback_port,
        })
    }

    pub fn telemetry(&self) -> AudioTelemetry {
        self.telemetry.clone()
    }

    pub fn period_frames(&self) -> u32 {
        self.period_frames
    }

    pub fn jack_cpu_load(&self) -> f32 {
        self.active
            .as_ref()
            .map(|active| active.as_client().cpu_load())
            .unwrap_or(0.0)
    }

    pub fn queue_chain(&mut self, chain: NamChain) -> Result<()> {
        self.drain_retired();
        match self.command_tx.push(AudioCommand::ReplaceChain(chain)) {
            Ok(()) => Ok(()),
            Err(PushError::Full(_command)) => {
                bail!("audio chain handoff is busy; try the edit again")
            }
        }
    }

    pub fn queue_cabinet_variant(&mut self, slot: usize, variant: usize) -> Result<()> {
        match self
            .command_tx
            .push(AudioCommand::SelectCabinet { slot, variant })
        {
            Ok(()) => Ok(()),
            Err(PushError::Full(_command)) => {
                bail!("audio control handoff is busy; try the edit again")
            }
        }
    }

    pub fn drain_retired(&mut self) {
        while self.retired_rx.pop().is_ok() {}
    }

    pub fn shutdown(mut self) -> Result<()> {
        self.deactivate()
    }

    fn deactivate(&mut self) -> Result<()> {
        let Some(active) = self.active.take() else {
            return Ok(());
        };
        let client = active.as_client();
        // JACK routes may be removed independently while the client is live.
        // Disconnect is therefore best-effort; deactivation owns shutdown.
        let _ = client.disconnect_ports_by_name(&self.capture_port, &self.input_port_name);
        let _ = client.disconnect_ports_by_name(&self.output_port_name, &self.playback_port);
        active.deactivate().context("deactivate JACK client")?;
        self.drain_retired();
        Ok(())
    }
}

impl Drop for AudioClient {
    fn drop(&mut self) {
        let _ = self.deactivate();
    }
}

struct AudioNotifications {
    telemetry: Arc<TelemetryInner>,
}

impl NotificationHandler for AudioNotifications {
    unsafe fn shutdown(&mut self, _status: ClientStatus, _reason: &str) {
        self.telemetry.server_lost.store(true, Ordering::Relaxed);
    }

    fn sample_rate(&mut self, _client: &Client, sample_rate: u32) -> Control {
        if sample_rate != SAMPLE_RATE {
            self.telemetry.process_fault.store(true, Ordering::Relaxed);
        }
        Control::Continue
    }

    fn xrun(&mut self, _client: &Client) -> Control {
        self.telemetry.xruns.fetch_add(1, Ordering::Relaxed);
        Control::Continue
    }
}

struct AudioProcess {
    input: Port<AudioIn>,
    output: Port<AudioOut>,
    scratch: Vec<f32>,
    expected_period: u32,
    params: Arc<Parameters>,
    telemetry: Arc<TelemetryInner>,
    command_rx: Consumer<AudioCommand>,
    retired_tx: Producer<NamChain>,
    active_chain: Option<NamChain>,
    pending_retire: Option<NamChain>,
}

enum AudioCommand {
    ReplaceChain(NamChain),
    SelectCabinet { slot: usize, variant: usize },
}

impl AudioProcess {
    fn publish_retired(&mut self) {
        let Some(model) = self.pending_retire.take() else {
            return;
        };
        if let Err(PushError::Full(model)) = self.retired_tx.push(model) {
            self.pending_retire = Some(model);
        }
    }

    fn accept_commands(&mut self) {
        self.publish_retired();
        if self.pending_retire.is_some() {
            return;
        }
        while let Ok(command) = self.command_rx.pop() {
            match command {
                AudioCommand::ReplaceChain(chain) => {
                    self.telemetry
                        .chain_len
                        .store(chain.len() as u32, Ordering::Relaxed);
                    self.pending_retire = self.active_chain.replace(chain);
                    self.publish_retired();
                    if self.pending_retire.is_some() {
                        return;
                    }
                }
                AudioCommand::SelectCabinet { slot, variant } => {
                    if !self
                        .active_chain
                        .as_mut()
                        .is_some_and(|chain| chain.select_cabinet_variant(slot, variant))
                    {
                        self.telemetry.process_fault.store(true, Ordering::Relaxed);
                    }
                }
            }
        }
    }
}

impl ProcessHandler for AudioProcess {
    fn process(&mut self, _client: &Client, process_scope: &ProcessScope) -> Control {
        self.accept_commands();
        let input = self.input.as_slice(process_scope);
        let output = self.output.as_mut_slice(process_scope);
        if input.len() != self.expected_period as usize
            || output.len() != input.len()
            || self.scratch.len() < input.len()
        {
            output.fill(0.0);
            self.telemetry.process_fault.store(true, Ordering::Relaxed);
            return Control::Continue;
        }

        let parameters = self.params.snapshot();
        let input_gain = db_to_gain(parameters.input_gain_db);
        for (target, source) in self.scratch.iter_mut().zip(input) {
            *target = *source * input_gain;
        }
        let scratch = &self.scratch[..input.len()];
        let (input_peak, input_rms) = peak_and_rms(scratch);

        let processed = if parameters.bypass {
            output.copy_from_slice(scratch);
            true
        } else if let Some(chain) = self.active_chain.as_mut() {
            chain.process(scratch, output)
        } else {
            output.fill(0.0);
            true
        };

        let output_gain = db_to_gain(parameters.output_gain_db);
        let mut finite = processed;
        for sample in output.iter_mut() {
            *sample *= output_gain;
            if !sample.is_finite() {
                *sample = 0.0;
                finite = false;
            }
        }
        let (output_peak, output_rms) = peak_and_rms(output);
        self.telemetry.input_peak.store(input_peak);
        self.telemetry.input_rms.store(input_rms);
        self.telemetry.output_peak.store(output_peak);
        self.telemetry.output_rms.store(output_rms);
        if !finite {
            self.telemetry.process_fault.store(true, Ordering::Relaxed);
        }
        Control::Continue
    }

    fn buffer_size(&mut self, _client: &Client, size: u32) -> Control {
        if size != self.expected_period {
            self.telemetry.process_fault.store(true, Ordering::Relaxed);
        }
        Control::Continue
    }
}

fn peak_and_rms(buffer: &[f32]) -> (f32, f32) {
    if buffer.is_empty() {
        return (0.0, 0.0);
    }
    let mut peak = 0.0_f32;
    let mut squares = 0.0_f64;
    for sample in buffer {
        peak = peak.max(sample.abs());
        squares += f64::from(*sample) * f64::from(*sample);
    }
    (peak, (squares / buffer.len() as f64).sqrt() as f32)
}

struct AtomicF32(AtomicU32);

impl AtomicF32 {
    fn new(value: f32) -> Self {
        Self(AtomicU32::new(value.to_bits()))
    }

    fn load(&self) -> f32 {
        f32::from_bits(self.0.load(Ordering::Relaxed))
    }

    fn store(&self, value: f32) {
        self.0.store(value.to_bits(), Ordering::Relaxed);
    }
}
