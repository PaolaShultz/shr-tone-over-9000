use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

const CATALOG: &str = include_str!("../assets/model-catalog.tsv");
pub const IR_METADATA_FILE: &str = ".rpi-tone-over-9000-ir.tsv";

#[derive(Clone, Debug)]
pub struct CatalogItem {
    pub id: String,
    pub category: String,
    pub file: String,
    pub url: String,
    pub sha256: String,
    pub license: String,
    pub source: String,
    pub ir: Option<IrProfile>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IrProfile {
    pub cabinet_id: String,
    pub cabinet: String,
    pub speaker: String,
    pub microphone: String,
    pub position: String,
    pub variant: String,
}

pub fn items() -> Result<Vec<CatalogItem>> {
    CATALOG
        .lines()
        .enumerate()
        .skip(1)
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(index, line)| {
            let fields = line.split('\t').collect::<Vec<_>>();
            if !matches!(fields.len(), 7 | 13) {
                bail!(
                    "catalog line {} has {} fields, expected 7 or 13",
                    index + 1,
                    fields.len()
                );
            }
            let ir = if fields.len() == 7 {
                None
            } else {
                Some(IrProfile {
                    cabinet_id: fields[7].to_owned(),
                    cabinet: fields[8].to_owned(),
                    speaker: fields[9].to_owned(),
                    microphone: fields[10].to_owned(),
                    position: fields[11].to_owned(),
                    variant: fields[12].to_owned(),
                })
            };
            Ok(CatalogItem {
                id: fields[0].to_owned(),
                category: fields[1].to_owned(),
                file: fields[2].to_owned(),
                url: fields[3].to_owned(),
                sha256: fields[4].to_owned(),
                license: fields[5].to_owned(),
                source: fields[6].to_owned(),
                ir,
            })
        })
        .collect::<Result<Vec<_>>>()
        .context("parse built-in model catalog")
}

pub fn select(selector: &str) -> Result<Vec<CatalogItem>> {
    let items = items()?;
    let selected = match selector {
        "all" | "starter" => items,
        "pedal" | "amp" | "cab" | "rig" => items
            .into_iter()
            .filter(|item| item.category == selector)
            .collect(),
        id => items.into_iter().filter(|item| item.id == id).collect(),
    };
    if selected.is_empty() {
        bail!("no catalog item or category named {selector:?}");
    }
    Ok(selected)
}

pub fn custom_ir_profiles(root: &Path) -> Result<BTreeMap<String, IrProfile>> {
    let path = root.join(IR_METADATA_FILE);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    let mut profiles = BTreeMap::new();
    for (index, line) in text.lines().enumerate().skip(1) {
        if line.trim().is_empty() {
            continue;
        }
        let fields = line.split('\t').collect::<Vec<_>>();
        if fields.len() != 7 || fields.iter().any(|field| field.is_empty()) {
            bail!(
                "{} line {} is not valid IR metadata",
                path.display(),
                index + 1
            );
        }
        profiles.insert(
            fields[0].to_owned(),
            IrProfile {
                cabinet_id: fields[1].to_owned(),
                cabinet: fields[2].to_owned(),
                speaker: fields[3].to_owned(),
                microphone: fields[4].to_owned(),
                position: fields[5].to_owned(),
                variant: fields[6].to_owned(),
            },
        );
    }
    Ok(profiles)
}

pub fn upsert_custom_ir_profile(root: &Path, file: &str, profile: IrProfile) -> Result<()> {
    for (label, value) in [
        ("file", file),
        ("cabinet id", &profile.cabinet_id),
        ("cabinet", &profile.cabinet),
        ("speaker", &profile.speaker),
        ("microphone", &profile.microphone),
        ("position", &profile.position),
        ("variant", &profile.variant),
    ] {
        if value.is_empty() || value.contains(['\t', '\r', '\n']) {
            bail!("IR metadata {label} must be one non-empty line without tabs");
        }
    }
    let mut profiles = custom_ir_profiles(root)?;
    profiles.insert(file.to_owned(), profile);
    let mut output =
        String::from("file\tcabinet_id\tcabinet\tspeaker\tmicrophone\tposition\tvariant\n");
    for (file, profile) in profiles {
        output.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
            file,
            profile.cabinet_id,
            profile.cabinet,
            profile.speaker,
            profile.microphone,
            profile.position,
            profile.variant
        ));
    }
    fs::create_dir_all(root).with_context(|| format!("create {}", root.display()))?;
    let path = root.join(IR_METADATA_FILE);
    let temporary = root.join(format!(".{IR_METADATA_FILE}.tmp-{}", std::process::id()));
    let result = (|| {
        fs::write(&temporary, output)
            .with_context(|| format!("write staged IR metadata {}", temporary.display()))?;
        fs::rename(&temporary, &path)
            .with_context(|| format!("publish IR metadata {}", path.display()))?;
        Ok::<(), anyhow::Error>(())
    })();
    if result.is_err() {
        fs::remove_file(&temporary).ok();
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn catalog_has_unique_ids_files_and_five_core_categories() {
        let items = items().unwrap();
        let ids = items
            .iter()
            .map(|item| item.id.as_str())
            .collect::<HashSet<_>>();
        let files = items
            .iter()
            .map(|item| item.file.as_str())
            .collect::<HashSet<_>>();
        assert_eq!(ids.len(), items.len());
        assert_eq!(files.len(), items.len());
        for category in ["pedal", "amp", "cab", "rig"] {
            assert_eq!(
                items
                    .iter()
                    .filter(|item| item.category == category)
                    .count(),
                5
            );
        }
        assert!(items
            .iter()
            .filter(|item| item.category == "cab")
            .all(|item| item.ir.is_some()));
        assert!(items
            .iter()
            .filter(|item| item.category != "cab")
            .all(|item| item.ir.is_none()));
    }

    #[test]
    fn cabinet_profiles_form_two_multi_variant_packs() {
        let items = items().unwrap();
        for cabinet_id in ["brutal-412", "greenback-1960ax"] {
            assert!(
                items
                    .iter()
                    .filter_map(|item| item.ir.as_ref())
                    .filter(|profile| profile.cabinet_id == cabinet_id)
                    .count()
                    >= 2
            );
        }
    }

    #[test]
    fn custom_ir_metadata_round_trips_and_replaces_atomically() {
        let sequence = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "tone9000-ir-metadata-{}-{sequence}",
            std::process::id()
        ));
        let first = IrProfile {
            cabinet_id: "mesa-412".to_owned(),
            cabinet: "Mesa 4x12".to_owned(),
            speaker: "V30".to_owned(),
            microphone: "SM57".to_owned(),
            position: "CAP EDGE".to_owned(),
            variant: "SM57 EDGE".to_owned(),
        };
        upsert_custom_ir_profile(&root, "mesa-sm57.wav", first.clone()).unwrap();
        assert_eq!(
            custom_ir_profiles(&root).unwrap().get("mesa-sm57.wav"),
            Some(&first)
        );

        let mut replacement = first;
        replacement.microphone = "R121".to_owned();
        upsert_custom_ir_profile(&root, "mesa-sm57.wav", replacement.clone()).unwrap();
        assert_eq!(
            custom_ir_profiles(&root).unwrap().get("mesa-sm57.wav"),
            Some(&replacement)
        );
        fs::remove_dir_all(root).ok();
    }
}
