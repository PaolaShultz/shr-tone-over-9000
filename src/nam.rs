use crate::cab::CabinetIr;
use anyhow::{bail, Context, Result};
use std::ffi::{c_char, c_void, CStr, CString};
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr::NonNull;

const ERROR_CAPACITY: usize = 512;
const TEXT_CAPACITY: usize = 96;

unsafe extern "C" {
    fn nam_model_load(
        path: *const c_char,
        sample_rate: u32,
        maximum_frames: u32,
        error: *mut c_char,
        error_capacity: usize,
    ) -> *mut c_void;
    fn nam_model_free(model: *mut c_void);
    fn nam_model_process(
        model: *mut c_void,
        input: *const f32,
        output: *mut f32,
        frames: usize,
    ) -> bool;
    fn nam_model_expected_sample_rate(model: *const c_void) -> f64;
    fn nam_model_input_level_dbu(model: *const c_void) -> f64;
    fn nam_model_output_level_dbu(model: *const c_void) -> f64;
    fn nam_model_weight_count(model: *const c_void) -> usize;
    fn nam_model_architecture(model: *const c_void, output: *mut c_char, capacity: usize);
    fn nam_model_version(model: *const c_void, output: *mut c_char, capacity: usize);
}

#[derive(Clone, Debug)]
pub struct ModelMetadata {
    pub path: PathBuf,
    pub name: String,
    pub architecture: String,
    pub version: String,
    pub file_bytes: u64,
    pub detail: String,
    pub expected_sample_rate: Option<u32>,
    pub input_level_dbu: Option<f64>,
    pub output_level_dbu: Option<f64>,
}

pub struct NamModel {
    raw: NonNull<c_void>,
    metadata: ModelMetadata,
}

// A loaded instance is constructed and prewarmed before transfer, then owned
// exclusively by the JACK process handler until it is returned for retirement.
unsafe impl Send for NamModel {}

pub struct NamChain {
    models: Vec<ChainProcessor>,
    scratch_a: Vec<f32>,
    scratch_b: Vec<f32>,
}

// Every model in the chain has exclusive callback-thread ownership. The
// scratch buffers are allocated before the chain crosses into JACK.
unsafe impl Send for NamChain {}

impl NamChain {
    pub fn load(paths: &[PathBuf], sample_rate: u32, maximum_frames: u32) -> Result<Self> {
        let cabinet_variants = vec![Vec::new(); paths.len()];
        Self::load_with_cabinet_variants(paths, &cabinet_variants, sample_rate, maximum_frames)
    }

    pub fn load_with_cabinet_variants(
        paths: &[PathBuf],
        cabinet_variants: &[Vec<PathBuf>],
        sample_rate: u32,
        maximum_frames: u32,
    ) -> Result<Self> {
        if paths.len() != cabinet_variants.len() {
            bail!("cabinet variant map does not match the chain");
        }
        let mut models = Vec::with_capacity(paths.len());
        for (index, path) in paths.iter().enumerate() {
            models.push(
                ChainProcessor::load(path, &cabinet_variants[index], sample_rate, maximum_frames)
                    .with_context(|| {
                    format!("load chain slot {} from {}", index + 1, path.display())
                })?,
            );
        }
        Ok(Self {
            models,
            scratch_a: vec![0.0; maximum_frames as usize],
            scratch_b: vec![0.0; maximum_frames as usize],
        })
    }

    pub fn metadata(&self) -> Vec<ModelMetadata> {
        self.models
            .iter()
            .map(|model| model.metadata().clone())
            .collect()
    }

    pub fn cabinet_variant_metadata(&self) -> Vec<Vec<ModelMetadata>> {
        self.models
            .iter()
            .map(|processor| match processor {
                ChainProcessor::Cabinet { variants, .. } => variants
                    .iter()
                    .map(|variant| variant.metadata.clone())
                    .collect(),
                ChainProcessor::Nam(_) => Vec::new(),
            })
            .collect()
    }

    pub fn len(&self) -> usize {
        self.models.len()
    }

    pub fn select_cabinet_variant(&mut self, slot: usize, variant: usize) -> bool {
        self.models
            .get_mut(slot)
            .is_some_and(|processor| processor.select_cabinet_variant(variant))
    }

    pub fn process(&mut self, input: &[f32], output: &mut [f32]) -> bool {
        if input.len() != output.len()
            || self.scratch_a.len() < input.len()
            || self.scratch_b.len() < input.len()
        {
            output.fill(0.0);
            return false;
        }
        match self.models.as_mut_slice() {
            [] => {
                output.fill(0.0);
                true
            }
            [model] => model.process(input, output),
            models => {
                let frames = input.len();
                let (first, rest) = models
                    .split_first_mut()
                    .expect("multi-model chain has a first model");
                let (last, middle) = rest
                    .split_last_mut()
                    .expect("multi-model chain has a last model");
                if !first.process(input, &mut self.scratch_a[..frames]) {
                    output.fill(0.0);
                    return false;
                }
                let mut current_is_a = true;
                for model in middle {
                    let processed = if current_is_a {
                        model.process(&self.scratch_a[..frames], &mut self.scratch_b[..frames])
                    } else {
                        model.process(&self.scratch_b[..frames], &mut self.scratch_a[..frames])
                    };
                    if !processed {
                        output.fill(0.0);
                        return false;
                    }
                    current_is_a = !current_is_a;
                }
                if current_is_a {
                    last.process(&self.scratch_a[..frames], output)
                } else {
                    last.process(&self.scratch_b[..frames], output)
                }
            }
        }
    }
}

enum ChainProcessor {
    Nam(NamModel),
    Cabinet {
        variants: Vec<CabinetVariant>,
        selected: usize,
    },
}

struct CabinetVariant {
    cabinet: Box<CabinetIr>,
    metadata: ModelMetadata,
}

impl ChainProcessor {
    fn load(
        path: &Path,
        cabinet_variants: &[PathBuf],
        sample_rate: u32,
        maximum_frames: u32,
    ) -> Result<Self> {
        let extension = path
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        match extension.as_str() {
            "nam" => NamModel::load(path, sample_rate, maximum_frames).map(Self::Nam),
            "wav" => {
                let paths = if cabinet_variants.is_empty() {
                    vec![path.to_path_buf()]
                } else {
                    cabinet_variants.to_vec()
                };
                let selected = paths
                    .iter()
                    .position(|candidate| candidate == path)
                    .with_context(|| {
                        format!(
                            "cabinet pack does not contain selected IR {}",
                            path.display()
                        )
                    })?;
                let mut variants = Vec::with_capacity(paths.len());
                for variant_path in paths {
                    variants.push(CabinetVariant::load(
                        &variant_path,
                        sample_rate,
                        maximum_frames,
                    )?);
                }
                Ok(Self::Cabinet { variants, selected })
            }
            _ => bail!("{} is not a supported .nam or .wav file", path.display()),
        }
    }

    fn metadata(&self) -> &ModelMetadata {
        match self {
            Self::Nam(model) => model.metadata(),
            Self::Cabinet { variants, selected } => &variants[*selected].metadata,
        }
    }

    fn process(&mut self, input: &[f32], output: &mut [f32]) -> bool {
        match self {
            Self::Nam(model) => model.process(input, output),
            Self::Cabinet { variants, selected } => {
                variants[*selected].cabinet.process(input, output)
            }
        }
    }

    fn select_cabinet_variant(&mut self, variant: usize) -> bool {
        let Self::Cabinet { variants, selected } = self else {
            return false;
        };
        if variant >= variants.len() {
            return false;
        }
        *selected = variant;
        true
    }
}

impl CabinetVariant {
    fn load(path: &Path, sample_rate: u32, maximum_frames: u32) -> Result<Self> {
        let cabinet = CabinetIr::load(path, sample_rate, maximum_frames)?;
        let file_bytes = fs::metadata(path)
            .with_context(|| format!("inspect {}", path.display()))?
            .len();
        let metadata = ModelMetadata {
            path: path.to_path_buf(),
            name: file_name(path),
            architecture: "CAB-FFT".to_owned(),
            version: format!("WAV{}", cabinet.bits_per_sample()),
            file_bytes,
            detail: format!("{}F {}CH", cabinet.frames(), cabinet.channels()),
            expected_sample_rate: Some(sample_rate),
            input_level_dbu: None,
            output_level_dbu: None,
        };
        Ok(Self {
            cabinet: Box::new(cabinet),
            metadata,
        })
    }
}

impl NamModel {
    pub fn load(path: &Path, sample_rate: u32, maximum_frames: u32) -> Result<Self> {
        if !path
            .extension()
            .and_then(|value| value.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("nam"))
        {
            bail!("{} is not a .nam file", path.display());
        }
        let file_bytes = fs::metadata(path)
            .with_context(|| format!("inspect {}", path.display()))?
            .len();
        let c_path = CString::new(path.as_os_str().as_bytes())
            .with_context(|| format!("NAM path contains a NUL byte: {}", path.display()))?;
        let mut error = [0 as c_char; ERROR_CAPACITY];
        // SAFETY: the path and error storage remain valid for the duration of
        // the call; the returned pointer is uniquely owned by this wrapper.
        let raw = unsafe {
            nam_model_load(
                c_path.as_ptr(),
                sample_rate,
                maximum_frames,
                error.as_mut_ptr(),
                error.len(),
            )
        };
        let Some(raw) = NonNull::new(raw) else {
            // SAFETY: the C++ bridge always NUL-terminates this fixed buffer.
            let message = unsafe { CStr::from_ptr(error.as_ptr()) }
                .to_string_lossy()
                .into_owned();
            bail!(
                "load {}: {}",
                path.display(),
                if message.is_empty() {
                    "unknown NAM loader failure"
                } else {
                    &message
                }
            );
        };

        let weight_count = unsafe { nam_model_weight_count(raw.as_ptr()) };
        let metadata = ModelMetadata {
            path: path.to_path_buf(),
            name: file_name(path),
            architecture: unsafe { text_value(raw, nam_model_architecture) },
            version: unsafe { text_value(raw, nam_model_version) },
            file_bytes,
            detail: format!("{weight_count}W"),
            expected_sample_rate: finite_positive(unsafe {
                nam_model_expected_sample_rate(raw.as_ptr())
            })
            .map(|value| value.round() as u32),
            input_level_dbu: finite(unsafe { nam_model_input_level_dbu(raw.as_ptr()) }),
            output_level_dbu: finite(unsafe { nam_model_output_level_dbu(raw.as_ptr()) }),
        };
        Ok(Self { raw, metadata })
    }

    pub fn metadata(&self) -> &ModelMetadata {
        &self.metadata
    }

    pub fn process(&mut self, input: &[f32], output: &mut [f32]) -> bool {
        if input.len() != output.len() {
            output.fill(0.0);
            return false;
        }
        // SAFETY: the model has exclusive callback-thread ownership and both
        // slices are valid for exactly `input.len()` mono frames.
        unsafe {
            nam_model_process(
                self.raw.as_ptr(),
                input.as_ptr(),
                output.as_mut_ptr(),
                input.len(),
            )
        }
    }
}

fn file_name(path: &Path) -> String {
    path.file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

impl Drop for NamModel {
    fn drop(&mut self) {
        // SAFETY: `raw` is the unique handle returned by nam_model_load.
        unsafe { nam_model_free(self.raw.as_ptr()) }
    }
}

unsafe fn text_value(
    raw: NonNull<c_void>,
    getter: unsafe extern "C" fn(*const c_void, *mut c_char, usize),
) -> String {
    let mut text = [0 as c_char; TEXT_CAPACITY];
    // SAFETY: caller supplies a live handle and fixed writable buffer.
    unsafe { getter(raw.as_ptr(), text.as_mut_ptr(), text.len()) };
    // SAFETY: the bridge always writes a NUL-terminated string.
    unsafe { CStr::from_ptr(text.as_ptr()) }
        .to_string_lossy()
        .into_owned()
}

fn finite(value: f64) -> Option<f64> {
    value.is_finite().then_some(value)
}

fn finite_positive(value: f64) -> Option<f64> {
    (value.is_finite() && value > 0.0).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hound::{SampleFormat, WavSpec, WavWriter};
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_FILE: AtomicU64 = AtomicU64::new(0);

    fn example(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("vendor/tone3000-plugin/plugin/NeuralAmpModelerCore/example_models")
            .join(name)
    }

    fn input() -> Vec<f32> {
        (0..256)
            .map(|index| ((index as f32 * 0.071).sin() * 0.1) + 0.01)
            .collect()
    }

    fn test_ir(label: &str, samples: &[i32]) -> PathBuf {
        let sequence = NEXT_FILE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "tone9000-chain-{label}-{}-{sequence}.wav",
            std::process::id(),
        ));
        let mut writer = WavWriter::create(
            &path,
            WavSpec {
                channels: 1,
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
        path
    }

    fn identity_ir() -> PathBuf {
        let mut samples = vec![0; 128];
        samples[0] = 4_000_000;
        test_ir("identity", &samples)
    }

    #[test]
    fn empty_chain_outputs_silence() {
        let mut chain = NamChain::load(&[], 48_000, 256).unwrap();
        let mut output = vec![1.0; 256];

        assert!(chain.process(&input(), &mut output));
        assert!(output.iter().all(|sample| *sample == 0.0));
    }

    #[test]
    fn two_model_chain_processes_in_order() {
        let first = example("wavenet.nam");
        let second = example("lstm.nam");
        let input = input();
        let mut forward = NamChain::load(&[first.clone(), second.clone()], 48_000, 256).unwrap();
        let mut reverse = NamChain::load(&[second, first], 48_000, 256).unwrap();
        let mut forward_output = vec![0.0; 256];
        let mut reverse_output = vec![0.0; 256];

        assert!(forward.process(&input, &mut forward_output));
        assert!(reverse.process(&input, &mut reverse_output));
        assert!(forward_output.iter().all(|sample| sample.is_finite()));
        assert!(reverse_output.iter().all(|sample| sample.is_finite()));
        assert!(forward_output.iter().any(|sample| sample.abs() > 0.000_001));
        assert!(forward_output
            .iter()
            .zip(&reverse_output)
            .any(|(left, right)| (left - right).abs() > 0.000_001));
    }

    #[test]
    fn nam_then_identity_cabinet_is_one_ordered_chain() {
        let model = example("wavenet.nam");
        let cabinet = identity_ir();
        let input = input();
        let mut nam_only = NamChain::load(std::slice::from_ref(&model), 48_000, 256).unwrap();
        let mut with_cab = NamChain::load(&[model, cabinet.clone()], 48_000, 256).unwrap();
        let mut expected = vec![0.0; 256];
        let mut actual = vec![0.0; 256];

        assert!(nam_only.process(&input, &mut expected));
        assert!(with_cab.process(&input, &mut actual));
        assert!(expected
            .iter()
            .zip(actual)
            .all(|(left, right)| (left - right).abs() < 0.000_01));
        fs::remove_file(cabinet).ok();
    }

    #[test]
    fn preloaded_cabinet_pack_switches_without_reloading_chain() {
        let identity = identity_ir();
        let mut delayed_samples = vec![0; 128];
        delayed_samples[1] = 4_000_000;
        let delayed = test_ir("delayed", &delayed_samples);
        let paths = vec![identity.clone()];
        let packs = vec![vec![identity.clone(), delayed.clone()]];
        let mut chain = NamChain::load_with_cabinet_variants(&paths, &packs, 48_000, 128).unwrap();
        let mut input = vec![0.0; 128];
        input[0] = 0.25;
        let mut output = vec![0.0; 128];

        assert!(chain.process(&input, &mut output));
        assert!((output[0] - 0.25).abs() < 0.000_01);
        assert!(chain.select_cabinet_variant(0, 1));
        assert_eq!(chain.metadata()[0].path, delayed);
        assert!(chain.process(&input, &mut output));
        assert!(output[0].abs() < 0.000_01);
        assert!((output[1] - 0.25).abs() < 0.000_01);
        assert!(!chain.select_cabinet_variant(0, 2));

        fs::remove_file(identity).ok();
        fs::remove_file(delayed).ok();
    }
}
