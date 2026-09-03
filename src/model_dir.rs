use crate::catalog::{self, IrProfile};
use anyhow::{Context, Result};
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};

const APPLICATION_DIR: &str = "rpi-tone-over-9000";

pub fn default_path() -> PathBuf {
    if let Some(path) = std::env::var_os("RPI_TONE_MODELS_DIR").filter(|path| !path.is_empty()) {
        return PathBuf::from(path);
    }
    if let Some(path) = std::env::var_os("XDG_DATA_HOME").filter(|path| !path.is_empty()) {
        return PathBuf::from(path).join(APPLICATION_DIR).join("models");
    }
    if let Some(path) = std::env::var_os("HOME").filter(|path| !path.is_empty()) {
        return PathBuf::from(path)
            .join(".local/share")
            .join(APPLICATION_DIR)
            .join("models");
    }
    PathBuf::from("models/library")
}

#[derive(Clone, Debug)]
pub struct ModelEntry {
    pub path: PathBuf,
    pub name: String,
    pub file_bytes: u64,
    pub ir: Option<IrProfile>,
}

pub struct ModelDirectory {
    root: PathBuf,
    entries: Vec<ModelEntry>,
    selected: usize,
    metadata_warning: Option<String>,
    events: Receiver<notify::Result<Event>>,
    _watcher: RecommendedWatcher,
}

impl ModelDirectory {
    pub fn open(root: PathBuf) -> Result<Self> {
        fs::create_dir_all(&root)
            .with_context(|| format!("create models directory {}", root.display()))?;
        let (sender, events) = mpsc::channel();
        let mut watcher = notify::recommended_watcher(sender).context("create model watcher")?;
        watcher
            .watch(&root, RecursiveMode::NonRecursive)
            .with_context(|| format!("watch {}", root.display()))?;
        let mut directory = Self {
            root,
            entries: Vec::new(),
            selected: 0,
            metadata_warning: None,
            events,
            _watcher: watcher,
        };
        directory.refresh()?;
        Ok(directory)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn entries(&self) -> &[ModelEntry] {
        &self.entries
    }

    pub fn selected_index(&self) -> usize {
        self.selected
    }

    pub fn selected(&self) -> Option<&ModelEntry> {
        self.entries.get(self.selected)
    }

    pub fn entry_for_path(&self, path: &Path) -> Option<&ModelEntry> {
        self.entries.iter().find(|entry| entry.path == path)
    }

    pub fn ir_profile(&self, path: &Path) -> Option<&IrProfile> {
        self.entry_for_path(path)?.ir.as_ref()
    }

    pub fn ir_variants(&self, path: &Path) -> Vec<&ModelEntry> {
        let Some(profile) = self.ir_profile(path) else {
            return Vec::new();
        };
        self.entries
            .iter()
            .filter(|entry| {
                entry
                    .ir
                    .as_ref()
                    .is_some_and(|candidate| candidate.cabinet_id == profile.cabinet_id)
            })
            .collect()
    }

    pub fn select_path(&mut self, path: &Path) {
        if let Some(index) = self.entries.iter().position(|entry| entry.path == path) {
            self.selected = index;
        }
    }

    pub fn take_metadata_warning(&mut self) -> Option<String> {
        self.metadata_warning.take()
    }

    pub fn select_relative(&mut self, direction: i8) {
        if self.entries.is_empty() {
            self.selected = 0;
            return;
        }
        self.selected = if direction < 0 {
            self.selected
                .checked_sub(1)
                .unwrap_or(self.entries.len() - 1)
        } else {
            (self.selected + 1) % self.entries.len()
        };
    }

    pub fn poll_watch(&mut self) -> Result<bool> {
        let mut changed = false;
        loop {
            match self.events.try_recv() {
                Ok(Ok(_event)) => changed = true,
                Ok(Err(error)) => return Err(error).context("watch models directory"),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    return Err(anyhow::anyhow!("models directory watcher stopped"));
                }
            }
        }
        if changed {
            self.refresh()?;
        }
        Ok(changed)
    }

    pub fn refresh(&mut self) -> Result<()> {
        let previous = self.selected().map(|entry| entry.path.clone());
        let catalog = catalog::items()?;
        let custom_profiles = match catalog::custom_ir_profiles(&self.root) {
            Ok(profiles) => profiles,
            Err(error) => {
                self.metadata_warning = Some(format!("IR METADATA IGNORED · {error}"));
                Default::default()
            }
        };
        let mut entries = Vec::new();
        for item in fs::read_dir(&self.root)
            .with_context(|| format!("read models directory {}", self.root.display()))?
        {
            let item = item.with_context(|| format!("read entry in {}", self.root.display()))?;
            let path = item.path();
            let extension = path
                .extension()
                .and_then(|value| value.to_str())
                .unwrap_or_default()
                .to_ascii_lowercase();
            if !item.file_type()?.is_file() || !matches!(extension.as_str(), "nam" | "wav") {
                continue;
            }
            let metadata = item.metadata()?;
            let built_in_ir = path
                .file_name()
                .and_then(|value| value.to_str())
                .and_then(|file| {
                    catalog
                        .iter()
                        .find(|catalog_item| catalog_item.file == file)
                        .and_then(|catalog_item| catalog_item.ir.clone())
                });
            let ir = path
                .file_name()
                .and_then(|value| value.to_str())
                .and_then(|file| custom_profiles.get(file).cloned())
                .or(built_in_ir);
            entries.push(ModelEntry {
                name: path
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
                path,
                file_bytes: metadata.len(),
                ir,
            });
        }
        entries.sort_by(|left, right| {
            left.name
                .to_ascii_lowercase()
                .cmp(&right.name.to_ascii_lowercase())
                .then_with(|| left.name.cmp(&right.name))
        });
        self.entries = entries;
        self.selected = previous
            .and_then(|path| self.entries.iter().position(|entry| entry.path == path))
            .unwrap_or_else(|| self.selected.min(self.entries.len().saturating_sub(1)));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn malformed_optional_metadata_does_not_block_library() {
        let sequence = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "tone9000-model-dir-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join(catalog::IR_METADATA_FILE), "header\nbroken\n").unwrap();

        let mut directory = ModelDirectory::open(root.clone()).unwrap();

        assert!(directory.entries().is_empty());
        assert!(directory.take_metadata_warning().is_some());
        fs::remove_dir_all(root).ok();
    }
}
