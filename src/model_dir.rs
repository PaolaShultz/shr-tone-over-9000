use anyhow::{Context, Result};
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};

#[derive(Clone, Debug)]
pub struct ModelEntry {
    pub path: PathBuf,
    pub name: String,
    pub file_bytes: u64,
}

pub struct ModelDirectory {
    root: PathBuf,
    entries: Vec<ModelEntry>,
    selected: usize,
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
        let mut entries = Vec::new();
        for item in fs::read_dir(&self.root)
            .with_context(|| format!("read models directory {}", self.root.display()))?
        {
            let item = item.with_context(|| format!("read entry in {}", self.root.display()))?;
            let path = item.path();
            if !item.file_type()?.is_file()
                || path.extension().and_then(|value| value.to_str()) != Some("nam")
            {
                continue;
            }
            let metadata = item.metadata()?;
            entries.push(ModelEntry {
                name: path
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
                path,
                file_bytes: metadata.len(),
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
