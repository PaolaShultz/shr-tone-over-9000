mod audio;
mod midi;
mod model_dir;
mod nam;
mod params;
mod tui;

use anyhow::{bail, Context, Result};
use signal_hook::consts::signal::{SIGINT, SIGTERM};
use std::env;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

fn main() -> Result<()> {
    let Some(options) = Options::parse()? else {
        return Ok(());
    };
    let shutdown = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(SIGINT, Arc::clone(&shutdown)).context("register SIGINT")?;
    signal_hook::flag::register(SIGTERM, Arc::clone(&shutdown)).context("register SIGTERM")?;

    let params = Arc::new(params::Parameters::default());
    let models = model_dir::ModelDirectory::open(options.models_dir)?;
    let midi =
        midi::MidiController::open(&options.controller_config, options.midi_input.as_deref())?;
    let mut audio = audio::AudioClient::start(
        &options.client_name,
        options.period,
        options.capture_port,
        options.playback_port,
        Arc::clone(&params),
    )?;

    let ui_result = tui::run(&mut audio, models, midi, params, shutdown);
    let shutdown_result = audio.shutdown();
    ui_result?;
    shutdown_result
}

struct Options {
    models_dir: PathBuf,
    controller_config: PathBuf,
    midi_input: Option<String>,
    client_name: String,
    capture_port: String,
    playback_port: String,
    period: u32,
}

impl Options {
    fn parse() -> Result<Option<Self>> {
        let mut options = Self {
            models_dir: PathBuf::from("models"),
            controller_config: PathBuf::from("controller.conf"),
            midi_input: None,
            client_name: "rpi-tone-over-9000".to_owned(),
            capture_port: "system:capture_1".to_owned(),
            playback_port: "system:playback_1".to_owned(),
            period: 256,
        };
        let mut arguments = env::args_os().skip(1);
        while let Some(argument) = arguments.next() {
            let argument = argument.to_string_lossy();
            match argument.as_ref() {
                "-h" | "--help" => {
                    print_help();
                    return Ok(None);
                }
                "--models-dir" => {
                    options.models_dir = PathBuf::from(next_value(&mut arguments, "--models-dir")?)
                }
                "--controller-config" => {
                    options.controller_config =
                        PathBuf::from(next_value(&mut arguments, "--controller-config")?)
                }
                "--midi" => {
                    options.midi_input = Some(
                        next_value(&mut arguments, "--midi")?
                            .to_string_lossy()
                            .into_owned(),
                    )
                }
                "--client-name" => {
                    options.client_name = next_string(&mut arguments, "--client-name")?
                }
                "--capture-port" => {
                    options.capture_port = next_string(&mut arguments, "--capture-port")?
                }
                "--playback-port" => {
                    options.playback_port = next_string(&mut arguments, "--playback-port")?
                }
                "--period" => {
                    let value = next_string(&mut arguments, "--period")?;
                    options.period = value
                        .parse::<u32>()
                        .with_context(|| format!("invalid --period {value:?}"))?;
                    if !matches!(options.period, 128 | 256) {
                        bail!("--period must be 128 or 256 frames");
                    }
                }
                _ => bail!("unknown argument {argument:?}; use --help"),
            }
        }
        Ok(Some(options))
    }
}

fn next_value(
    arguments: &mut impl Iterator<Item = std::ffi::OsString>,
    option: &str,
) -> Result<std::ffi::OsString> {
    arguments
        .next()
        .with_context(|| format!("{option} requires a value"))
}

fn next_string(
    arguments: &mut impl Iterator<Item = std::ffi::OsString>,
    option: &str,
) -> Result<String> {
    let value = next_value(arguments, option)?;
    value
        .into_string()
        .map_err(|_| anyhow::anyhow!("{option} value must be valid UTF-8"))
}

fn print_help() {
    println!(
        "rpi-tone-over-9000\n\
         \n\
         Usage: rpi-tone-over-9000 [OPTIONS]\n\
         \n\
           --models-dir DIR         NAM directory (default: ./models)\n\
           --controller-config FILE shr-daw-shaped mapping (default: controller.conf)\n\
           --midi NAME              exact stable ALSA MIDI input identity\n\
           --client-name NAME       JACK client name\n\
           --capture-port PORT      mono source (default: system:capture_1)\n\
           --playback-port PORT     mono destination (default: system:playback_1)\n\
           --period 128|256         require this JACK period (default: 256)\n\
           -h, --help               show this help"
    );
}
