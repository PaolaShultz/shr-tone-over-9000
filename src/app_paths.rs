//! Canonical paths with read compatibility for installations predating the rename.
use std::path::{Path, PathBuf};

pub fn application_path(base: &Path, suffix: &str) -> PathBuf {
    let current = base.join("shr-tone-over-9000").join(suffix);
    let legacy = base.join("rpi-tone-over-9000").join(suffix);
    if !current.exists() && legacy.exists() {
        legacy
    } else {
        current
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn existing_library_survives_rename_and_current_path_wins() {
        let root = std::env::temp_dir().join(format!("shr-tone-paths-{}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        let current = root.join("shr-tone-over-9000/models");
        let legacy = root.join("rpi-tone-over-9000/models");
        assert_eq!(application_path(&root, "models"), current);
        std::fs::create_dir_all(&legacy).unwrap();
        assert_eq!(application_path(&root, "models"), legacy);
        std::fs::create_dir_all(&current).unwrap();
        assert_eq!(application_path(&root, "models"), current);
        std::fs::remove_dir_all(root).unwrap();
    }
}
