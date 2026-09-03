use anyhow::{bail, Context, Result};
use std::ffi::{c_char, c_void, CStr, CString};
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::time::{SystemTime, UNIX_EPOCH};

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
    pub weight_count: usize,
    pub expected_sample_rate: Option<u32>,
    pub input_level_dbu: Option<f64>,
    pub output_level_dbu: Option<f64>,
    pub loaded_at_utc: String,
}

pub struct NamModel {
    raw: NonNull<c_void>,
    metadata: ModelMetadata,
}

// A loaded instance is constructed and prewarmed before transfer, then owned
// exclusively by the JACK process handler until it is returned for retirement.
unsafe impl Send for NamModel {}

impl NamModel {
    pub fn load(path: &Path, sample_rate: u32, maximum_frames: u32) -> Result<Self> {
        if path.extension().and_then(|value| value.to_str()) != Some("nam") {
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

        let metadata = ModelMetadata {
            path: path.to_path_buf(),
            name: path
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            architecture: unsafe { text_value(raw, nam_model_architecture) },
            version: unsafe { text_value(raw, nam_model_version) },
            file_bytes,
            weight_count: unsafe { nam_model_weight_count(raw.as_ptr()) },
            expected_sample_rate: finite_positive(unsafe {
                nam_model_expected_sample_rate(raw.as_ptr())
            })
            .map(|value| value.round() as u32),
            input_level_dbu: finite(unsafe { nam_model_input_level_dbu(raw.as_ptr()) }),
            output_level_dbu: finite(unsafe { nam_model_output_level_dbu(raw.as_ptr()) }),
            loaded_at_utc: utc_clock(SystemTime::now()),
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

fn utc_clock(now: SystemTime) -> String {
    let seconds = now.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() % 86_400;
    format!(
        "{:02}:{:02}:{:02}Z",
        seconds / 3_600,
        (seconds / 60) % 60,
        seconds % 60
    )
}
