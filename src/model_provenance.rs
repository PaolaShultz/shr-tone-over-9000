use crate::model_search::CaptureSettings;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

pub const PROVENANCE_FILE: &str = ".rpi-tone-over-9000-models.json";
const SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct HubIdentity {
    pub provider: String,
    pub tone_id: u64,
    pub model_id: u64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ModelProvenance {
    pub file: String,
    pub identity: HubIdentity,
    pub tone_title: String,
    pub model_name: String,
    pub creator: String,
    pub source_url: String,
    pub license: String,
    pub architecture: String,
    pub settings: CaptureSettings,
    pub parser_version: u32,
    pub downloaded_at_unix: u64,
    pub sha256: String,
}

#[derive(Default, Deserialize, Serialize)]
struct Manifest {
    schema_version: u32,
    models: Vec<ModelProvenance>,
}

pub fn records(root: &Path) -> Result<Vec<ModelProvenance>> {
    let path = root.join(PROVENANCE_FILE);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    let manifest: Manifest = serde_json::from_slice(&bytes)
        .with_context(|| format!("parse model provenance {}", path.display()))?;
    if manifest.schema_version != SCHEMA_VERSION {
        bail!(
            "{} uses unsupported provenance schema {}",
            path.display(),
            manifest.schema_version
        );
    }
    Ok(manifest.models)
}

pub fn upsert(root: &Path, record: ModelProvenance) -> Result<()> {
    validate_record(&record)?;
    let mut models = records(root)?;
    models.retain(|candidate| candidate.file != record.file);
    models.push(record);
    models.sort_by(|left, right| left.file.cmp(&right.file));
    let manifest = Manifest {
        schema_version: SCHEMA_VERSION,
        models,
    };
    let bytes = serde_json::to_vec_pretty(&manifest).context("serialize model provenance")?;
    fs::create_dir_all(root).with_context(|| format!("create {}", root.display()))?;
    let path = root.join(PROVENANCE_FILE);
    let temporary = root.join(format!(".{PROVENANCE_FILE}.tmp-{}", std::process::id()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&temporary)
            .with_context(|| format!("create staged provenance {}", temporary.display()))?;
        file.write_all(&bytes)
            .with_context(|| format!("write staged provenance {}", temporary.display()))?;
        file.sync_all()
            .with_context(|| format!("sync staged provenance {}", temporary.display()))?;
        fs::rename(&temporary, &path)
            .with_context(|| format!("publish model provenance {}", path.display()))?;
        Ok::<(), anyhow::Error>(())
    })();
    if result.is_err() {
        fs::remove_file(&temporary).ok();
    }
    result
}

pub fn find_model(root: &Path, provider: &str, model_id: u64) -> Result<Option<ModelProvenance>> {
    Ok(records(root)?.into_iter().find(|record| {
        record.identity.provider == provider && record.identity.model_id == model_id
    }))
}

fn validate_record(record: &ModelProvenance) -> Result<()> {
    if record.file.is_empty()
        || Path::new(&record.file)
            .file_name()
            .and_then(|value| value.to_str())
            != Some(record.file.as_str())
    {
        bail!("provenance file must be a plain filename");
    }
    if record.sha256.len() != 64 || !record.sha256.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("provenance SHA-256 must contain 64 hexadecimal characters");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

    fn temporary_directory() -> std::path::PathBuf {
        let sequence = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "tone9000-provenance-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn record(file: &str, model_id: u64) -> ModelProvenance {
        ModelProvenance {
            file: file.to_owned(),
            identity: HubIdentity {
                provider: "tone3000".to_owned(),
                tone_id: 7,
                model_id,
            },
            tone_title: "JCM800".to_owned(),
            model_name: "B3 M7 T2".to_owned(),
            creator: "creator".to_owned(),
            source_url: "https://www.tone3000.com/tones/7".to_owned(),
            license: "t3k".to_owned(),
            architecture: "2".to_owned(),
            settings: CaptureSettings::from_model("B3_M7_T2", None),
            parser_version: 1,
            downloaded_at_unix: 1,
            sha256: "a".repeat(64),
        }
    }

    #[test]
    fn manifest_round_trips_and_upserts_by_filename() {
        let root = temporary_directory();
        upsert(&root, record("amp.nam", 10)).unwrap();
        upsert(&root, record("amp.nam", 11)).unwrap();
        assert_eq!(records(&root).unwrap().len(), 1);
        assert_eq!(
            find_model(&root, "tone3000", 11).unwrap().unwrap().file,
            "amp.nam"
        );
        assert_eq!(
            fs::metadata(root.join(PROVENANCE_FILE))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn unsafe_filename_is_rejected_without_a_manifest() {
        let root = temporary_directory();
        assert!(upsert(&root, record("../amp.nam", 10)).is_err());
        assert!(!root.join(PROVENANCE_FILE).exists());
        fs::remove_dir_all(root).ok();
    }
}
