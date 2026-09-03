use anyhow::{bail, Context, Result};
use fft_convolver::{FFTConvolver, TwoStageFFTConvolver};
use hound::{SampleFormat, WavReader};
use std::fs;
use std::path::Path;

const MAX_IR_FRAMES: usize = 96_000;
const TWO_STAGE_THRESHOLD: usize = 16_384;

enum Convolver {
    Uniform(Box<FFTConvolver<f32>>),
    TwoStage(Box<TwoStageFFTConvolver<f32>>),
}

pub struct CabinetIr {
    convolver: Convolver,
    frames: usize,
    channels: u16,
    bits_per_sample: u16,
}

// Initialization, allocation, and WAV decoding happen before this crosses to
// JACK. FFTConvolver::process is allocation-free and exclusively callback-owned.
unsafe impl Send for CabinetIr {}

impl CabinetIr {
    pub fn load(path: &Path, sample_rate: u32, block_size: u32) -> Result<Self> {
        let file_bytes = fs::metadata(path)
            .with_context(|| format!("inspect cabinet IR {}", path.display()))?
            .len();
        if file_bytes == 0 {
            bail!("cabinet IR {} is empty", path.display());
        }
        let mut reader =
            WavReader::open(path).with_context(|| format!("open cabinet IR {}", path.display()))?;
        let spec = reader.spec();
        if spec.sample_rate != sample_rate {
            bail!(
                "cabinet IR {} is {} Hz; JACK is {} Hz",
                path.display(),
                spec.sample_rate,
                sample_rate
            );
        }
        if !matches!(spec.channels, 1 | 2) {
            bail!(
                "cabinet IR {} has {} channels; only mono/stereo WAV is supported",
                path.display(),
                spec.channels
            );
        }

        let interleaved = match spec.sample_format {
            SampleFormat::Float if spec.bits_per_sample == 32 => reader
                .samples::<f32>()
                .collect::<std::result::Result<Vec<_>, _>>()
                .with_context(|| format!("decode float cabinet IR {}", path.display()))?,
            SampleFormat::Int if (1..=32).contains(&spec.bits_per_sample) => {
                let scale = (1_u64 << (spec.bits_per_sample - 1)) as f32;
                reader
                    .samples::<i32>()
                    .map(|sample| sample.map(|value| value as f32 / scale))
                    .collect::<std::result::Result<Vec<_>, _>>()
                    .with_context(|| format!("decode PCM cabinet IR {}", path.display()))?
            }
            _ => bail!(
                "cabinet IR {} has unsupported {}-bit {:?} samples",
                path.display(),
                spec.bits_per_sample,
                spec.sample_format
            ),
        };
        let channels = usize::from(spec.channels);
        if interleaved.len() % channels != 0 {
            bail!("cabinet IR {} has incomplete WAV frames", path.display());
        }
        let frames = interleaved.len() / channels;
        if frames == 0 {
            bail!("cabinet IR {} contains no samples", path.display());
        }
        if frames > MAX_IR_FRAMES {
            bail!(
                "cabinet IR {} is {} frames; maximum is {} (2 seconds at 48 kHz)",
                path.display(),
                frames,
                MAX_IR_FRAMES
            );
        }

        let mut impulse = Vec::with_capacity(frames);
        if channels == 1 {
            impulse.extend(interleaved);
        } else {
            for frame in interleaved.chunks_exact(2) {
                impulse.push((frame[0] + frame[1]) * 0.5);
            }
        }
        if impulse.iter().any(|sample| !sample.is_finite()) {
            bail!("cabinet IR {} contains non-finite samples", path.display());
        }
        let peak = impulse
            .iter()
            .fold(0.0_f32, |maximum, sample| maximum.max(sample.abs()));
        if peak <= f32::EPSILON {
            bail!("cabinet IR {} is silent", path.display());
        }
        // Normalize once on load so swapping independently sourced IRs does not
        // create arbitrary output jumps. This preserves the IR's frequency shape.
        for sample in &mut impulse {
            *sample /= peak;
        }

        let convolver = if frames < TWO_STAGE_THRESHOLD {
            let mut convolver = FFTConvolver::default();
            convolver
                .init(block_size as usize, &impulse)
                .map_err(|error| anyhow::anyhow!("initialize cabinet convolution: {error}"))?;
            Convolver::Uniform(Box::new(convolver))
        } else {
            let mut convolver = TwoStageFFTConvolver::default();
            convolver
                .init_default(block_size as usize, &impulse)
                .map_err(|error| anyhow::anyhow!("initialize cabinet convolution: {error}"))?;
            Convolver::TwoStage(Box::new(convolver))
        };

        Ok(Self {
            convolver,
            frames,
            channels: spec.channels,
            bits_per_sample: spec.bits_per_sample,
        })
    }

    pub fn process(&mut self, input: &[f32], output: &mut [f32]) -> bool {
        if input.len() != output.len() {
            output.fill(0.0);
            return false;
        }
        let result = match &mut self.convolver {
            Convolver::Uniform(convolver) => convolver.process(input, output),
            Convolver::TwoStage(convolver) => convolver.process(input, output),
        };
        if result.is_err() {
            output.fill(0.0);
            return false;
        }
        true
    }

    pub fn frames(&self) -> usize {
        self.frames
    }

    pub fn channels(&self) -> u16 {
        self.channels
    }

    pub fn bits_per_sample(&self) -> u16 {
        self.bits_per_sample
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hound::{WavSpec, WavWriter};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_FILE: AtomicU64 = AtomicU64::new(0);

    fn wav_path(label: &str) -> PathBuf {
        let sequence = NEXT_FILE.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "tone9000-{label}-{}-{sequence}.wav",
            std::process::id()
        ))
    }

    fn write_pcm(path: &Path, channels: u16, samples: &[i32]) {
        let mut writer = WavWriter::create(
            path,
            WavSpec {
                channels,
                sample_rate: 48_000,
                bits_per_sample: 24,
                sample_format: SampleFormat::Int,
            },
        )
        .unwrap();
        for sample in samples {
            writer.write_sample(*sample).unwrap();
        }
        writer.finalize().unwrap();
    }

    #[test]
    fn mono_impulse_is_sample_aligned() {
        let path = wav_path("mono-ir");
        write_pcm(&path, 1, &[4_000_000, 0, 0, 0]);
        let mut cabinet = CabinetIr::load(&path, 48_000, 128).unwrap();
        let mut input = vec![0.0; 128];
        input[0] = 0.25;
        let mut output = vec![0.0; 128];

        assert!(cabinet.process(&input, &mut output));
        assert!((output[0] - 0.25).abs() < 0.000_01);
        assert!(output[1..].iter().all(|sample| sample.abs() < 0.000_01));
        fs::remove_file(path).ok();
    }

    #[test]
    fn stereo_ir_is_downmixed() {
        let path = wav_path("stereo-ir");
        write_pcm(&path, 2, &[4_000_000, 2_000_000, 0, 0]);
        let cabinet = CabinetIr::load(&path, 48_000, 128).unwrap();

        assert_eq!(cabinet.channels(), 2);
        assert_eq!(cabinet.frames(), 2);
        fs::remove_file(path).ok();
    }

    #[test]
    fn long_ir_uses_two_stage_convolution() {
        let path = wav_path("long-ir");
        let mut samples = vec![0; TWO_STAGE_THRESHOLD + 1];
        samples[0] = 4_000_000;
        write_pcm(&path, 1, &samples);
        let mut cabinet = CabinetIr::load(&path, 48_000, 128).unwrap();
        let mut input = vec![0.0; 128];
        input[0] = 0.25;
        let mut output = vec![0.0; 128];

        assert!(cabinet.process(&input, &mut output));
        assert!((output[0] - 0.25).abs() < 0.000_01);
        assert!(output[1..].iter().all(|sample| sample.abs() < 0.000_01));
        fs::remove_file(path).ok();
    }
}
