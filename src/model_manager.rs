use crate::audio::SAMPLE_RATE;
use crate::catalog::{self, CatalogItem, IrProfile};
use crate::nam::NamChain;
use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::env;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::Command;

pub fn run_if_requested() -> Result<bool> {
    let mut arguments = env::args_os().skip(1);
    if arguments.next().as_deref() != Some(std::ffi::OsStr::new("models")) {
        return Ok(false);
    }
    run(arguments.collect())?;
    Ok(true)
}

#[derive(Default)]
struct ManagerOptions {
    positional: Vec<String>,
    models_dir: Option<PathBuf>,
    name: Option<String>,
    sha256: Option<String>,
    accept_t3k: bool,
    replace: bool,
    cabinet_id: Option<String>,
    cabinet: Option<String>,
    speaker: Option<String>,
    microphone: Option<String>,
    position: Option<String>,
    variant: Option<String>,
    client_id: Option<String>,
    redirect_uri: Option<String>,
    auth_file: Option<PathBuf>,
    page: Option<usize>,
    architecture: Option<String>,
}

fn run(arguments: Vec<OsString>) -> Result<()> {
    let options = parse(arguments)?;
    let Some(command) = options.positional.first().map(String::as_str) else {
        print_help();
        return Ok(());
    };
    let models_dir = options
        .models_dir
        .clone()
        .unwrap_or_else(crate::model_dir::default_path);
    match command {
        "help" | "-h" | "--help" => print_help(),
        "list" => list(options.positional.get(1).map(String::as_str))?,
        "install" => {
            let selector = options
                .positional
                .get(1)
                .map(String::as_str)
                .unwrap_or("starter");
            install_catalog(selector, &models_dir, options.accept_t3k, options.replace)?;
        }
        "verify" => {
            let selector = options
                .positional
                .get(1)
                .map(String::as_str)
                .unwrap_or("starter");
            verify_catalog(selector, &models_dir)?;
        }
        "import" => {
            let source = options
                .positional
                .get(1)
                .context("models import requires a local .nam or .wav path")?;
            let name = options.name.as_deref().unwrap_or_else(|| {
                Path::new(source)
                    .file_name()
                    .and_then(|value| value.to_str())
                    .unwrap_or_default()
            });
            let ir = ir_profile_from_options(&options, name)?;
            import_file(
                Path::new(source),
                options.name.as_deref(),
                &models_dir,
                options.replace,
                ir,
            )?;
        }
        "add-url" => {
            let url = options
                .positional
                .get(1)
                .context("models add-url requires an HTTPS URL")?;
            let name = options
                .name
                .as_deref()
                .context("models add-url requires --name FILE.nam|FILE.wav")?;
            let ir = ir_profile_from_options(&options, name)?;
            install_url(
                url,
                name,
                options.sha256.as_deref(),
                &models_dir,
                options.replace,
                ir,
            )?;
        }
        "browse" => browse(
            options
                .positional
                .get(1)
                .map(String::as_str)
                .unwrap_or("all"),
        )?,
        "hub" => {
            let command = options.positional.get(1).map(String::as_str);
            let arguments = options.positional.get(2..).unwrap_or_default();
            crate::hub::run(
                command,
                arguments,
                crate::hub::HubOptions {
                    models_dir,
                    client_id: options.client_id,
                    redirect_uri: options.redirect_uri,
                    auth_file: options.auth_file,
                    page: options.page.unwrap_or(1),
                    architecture: options.architecture.unwrap_or_else(|| "2".to_owned()),
                    name: options.name,
                    replace: options.replace,
                },
            )?;
        }
        _ => bail!("unknown models command {command:?}; use `models help`"),
    }
    Ok(())
}

fn parse(arguments: Vec<OsString>) -> Result<ManagerOptions> {
    let mut parsed = ManagerOptions::default();
    let mut arguments = arguments.into_iter();
    while let Some(argument) = arguments.next() {
        let argument = argument
            .into_string()
            .map_err(|_| anyhow::anyhow!("model-manager arguments must be valid UTF-8"))?;
        match argument.as_str() {
            "--models-dir" => {
                parsed.models_dir = Some(PathBuf::from(next_utf8(&mut arguments, "--models-dir")?));
            }
            "--name" => parsed.name = Some(next_utf8(&mut arguments, "--name")?),
            "--sha256" => parsed.sha256 = Some(next_utf8(&mut arguments, "--sha256")?),
            "--accept-t3k" => parsed.accept_t3k = true,
            "--replace" => parsed.replace = true,
            "--cab-id" => parsed.cabinet_id = Some(next_utf8(&mut arguments, "--cab-id")?),
            "--cabinet" => parsed.cabinet = Some(next_utf8(&mut arguments, "--cabinet")?),
            "--speaker" => parsed.speaker = Some(next_utf8(&mut arguments, "--speaker")?),
            "--mic" => parsed.microphone = Some(next_utf8(&mut arguments, "--mic")?),
            "--position" => parsed.position = Some(next_utf8(&mut arguments, "--position")?),
            "--variant" => parsed.variant = Some(next_utf8(&mut arguments, "--variant")?),
            "--client-id" => parsed.client_id = Some(next_utf8(&mut arguments, "--client-id")?),
            "--redirect-uri" => {
                parsed.redirect_uri = Some(next_utf8(&mut arguments, "--redirect-uri")?)
            }
            "--auth-file" => {
                parsed.auth_file = Some(PathBuf::from(next_utf8(&mut arguments, "--auth-file")?))
            }
            "--page" => {
                let value = next_utf8(&mut arguments, "--page")?;
                let page = value
                    .parse::<usize>()
                    .with_context(|| format!("invalid --page {value:?}"))?;
                if page == 0 {
                    bail!("--page must be at least 1");
                }
                parsed.page = Some(page);
            }
            "--architecture" => {
                let value = next_utf8(&mut arguments, "--architecture")?;
                if !matches!(value.as_str(), "1" | "2" | "custom") {
                    bail!("--architecture must be 1, 2, or custom");
                }
                parsed.architecture = Some(value);
            }
            value if value.starts_with('-') => bail!("unknown model-manager option {value:?}"),
            value => parsed.positional.push(value.to_owned()),
        }
    }
    Ok(parsed)
}

fn next_utf8(arguments: &mut impl Iterator<Item = OsString>, option: &str) -> Result<String> {
    arguments
        .next()
        .with_context(|| format!("{option} requires a value"))?
        .into_string()
        .map_err(|_| anyhow::anyhow!("{option} value must be valid UTF-8"))
}

fn list(category: Option<&str>) -> Result<()> {
    let items = catalog::items()?;
    println!("ID\tCATEGORY\tLICENSE\tCABINET\tMIC/POSITION\tVARIANT\tFILE");
    for item in items
        .iter()
        .filter(|item| category.is_none_or(|category| item.category == category))
    {
        let (cabinet, microphone, variant) = item.ir.as_ref().map_or(("-", "-", "-"), |ir| {
            (
                ir.cabinet.as_str(),
                if ir.position == "UNDOCUMENTED" {
                    ir.microphone.as_str()
                } else {
                    ir.position.as_str()
                },
                ir.variant.as_str(),
            )
        });
        println!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}",
            item.id, item.category, item.license, cabinet, microphone, variant, item.file
        );
    }
    Ok(())
}

fn install_catalog(
    selector: &str,
    models_dir: &Path,
    accept_t3k: bool,
    replace: bool,
) -> Result<()> {
    let selected = catalog::select(selector)?;
    let needs_t3k = selected.iter().any(|item| {
        item.license == "T3K"
            && !installed_matches(models_dir.join(&item.file).as_path(), &item.sha256)
    });
    if needs_t3k && !accept_t3k {
        bail!(
            "T3K files are local-use downloads and may not be redistributed; review each source page, then repeat with --accept-t3k"
        );
    }
    fs::create_dir_all(models_dir)
        .with_context(|| format!("create model directory {}", models_dir.display()))?;
    let mut installed = 0;
    let mut unchanged = 0;
    for item in &selected {
        if install_item(item, models_dir, replace)? {
            installed += 1;
        } else {
            unchanged += 1;
        }
    }
    println!(
        "catalog complete: {installed} installed, {unchanged} already verified in {}",
        models_dir.display()
    );
    Ok(())
}

fn install_item(item: &CatalogItem, models_dir: &Path, replace: bool) -> Result<bool> {
    let destination = models_dir.join(safe_file_name(&item.file)?);
    if installed_matches(&destination, &item.sha256) {
        println!("ok {}", item.id);
        return Ok(false);
    }
    if destination.exists() && !replace {
        bail!(
            "{} exists but does not match the catalog; preserve it or repeat with --replace",
            destination.display()
        );
    }
    println!("install {} ({}, {})", item.id, item.category, item.license);
    download_and_commit(&item.url, &destination, Some(&item.sha256), replace)?;
    println!("source {}", item.source);
    Ok(true)
}

fn verify_catalog(selector: &str, models_dir: &Path) -> Result<()> {
    let selected = catalog::select(selector)?;
    let mut failures = Vec::new();
    for item in &selected {
        let path = models_dir.join(&item.file);
        if installed_matches(&path, &item.sha256) {
            println!("ok {}", item.id);
        } else {
            failures.push(item.id.as_str());
            println!("missing-or-changed {}", item.id);
        }
    }
    if !failures.is_empty() {
        bail!("{} catalog assets failed verification", failures.len());
    }
    println!("verified {} catalog assets", selected.len());
    Ok(())
}

fn import_file(
    source: &Path,
    name: Option<&str>,
    models_dir: &Path,
    replace: bool,
    ir: Option<IrProfile>,
) -> Result<()> {
    if !source.is_file() {
        bail!("import source {} is not a file", source.display());
    }
    let inferred = source
        .file_name()
        .and_then(|value| value.to_str())
        .context("import source needs a UTF-8 filename")?;
    let destination = models_dir.join(safe_file_name(name.unwrap_or(inferred))?);
    fs::create_dir_all(models_dir)
        .with_context(|| format!("create model directory {}", models_dir.display()))?;
    if destination.exists() && !replace {
        bail!(
            "{} already exists; repeat with --replace to replace it",
            destination.display()
        );
    }
    let temporary = temporary_path(&destination)?;
    let result = (|| {
        fs::copy(source, &temporary)
            .with_context(|| format!("copy {} to {}", source.display(), temporary.display()))?;
        validate_asset(&temporary)?;
        fs::rename(&temporary, &destination)
            .with_context(|| format!("publish imported asset {}", destination.display()))?;
        Ok::<(), anyhow::Error>(())
    })();
    if let Err(error) = result {
        fs::remove_file(&temporary).ok();
        return Err(error);
    }
    if let Some(profile) = ir {
        catalog::upsert_custom_ir_profile(
            models_dir,
            destination
                .file_name()
                .and_then(|value| value.to_str())
                .context("installed IR needs a UTF-8 filename")?,
            profile,
        )?;
    }
    println!(
        "installed {} sha256 {}",
        destination.display(),
        sha256_file(&destination)?
    );
    Ok(())
}

fn install_url(
    url: &str,
    name: &str,
    expected_sha256: Option<&str>,
    models_dir: &Path,
    replace: bool,
    ir: Option<IrProfile>,
) -> Result<()> {
    if !url.starts_with("https://") {
        bail!("model URLs must use HTTPS");
    }
    let destination = models_dir.join(safe_file_name(name)?);
    fs::create_dir_all(models_dir)
        .with_context(|| format!("create model directory {}", models_dir.display()))?;
    download_and_commit(url, &destination, expected_sha256, replace)?;
    if let Some(profile) = ir {
        catalog::upsert_custom_ir_profile(models_dir, safe_file_name(name)?, profile)?;
    }
    println!(
        "installed {} sha256 {}{}",
        destination.display(),
        sha256_file(&destination)?,
        if expected_sha256.is_some() {
            " (verified)"
        } else {
            " (record this checksum; source was not pinned)"
        }
    );
    Ok(())
}

fn ir_profile_from_options(options: &ManagerOptions, name: &str) -> Result<Option<IrProfile>> {
    let supplied = [
        options.cabinet_id.as_ref(),
        options.cabinet.as_ref(),
        options.speaker.as_ref(),
        options.microphone.as_ref(),
        options.position.as_ref(),
        options.variant.as_ref(),
    ]
    .into_iter()
    .any(|value| value.is_some());
    if !supplied {
        return Ok(None);
    }
    if !name
        .rsplit_once('.')
        .is_some_and(|(_, extension)| extension.eq_ignore_ascii_case("wav"))
    {
        bail!("cabinet/microphone metadata is only valid for .wav IRs");
    }
    let cabinet_id = options
        .cabinet_id
        .clone()
        .context("IR metadata requires --cab-id")?;
    let cabinet = options
        .cabinet
        .clone()
        .context("IR metadata requires --cabinet")?;
    let microphone = options
        .microphone
        .clone()
        .context("IR metadata requires --mic")?;
    let position = options
        .position
        .clone()
        .unwrap_or_else(|| "UNSPECIFIED".to_owned());
    let variant = options
        .variant
        .clone()
        .unwrap_or_else(|| format!("{microphone} {position}"));
    let profile = IrProfile {
        cabinet_id,
        cabinet,
        speaker: options
            .speaker
            .clone()
            .unwrap_or_else(|| "UNKNOWN".to_owned()),
        microphone,
        position,
        variant,
    };
    for (label, value) in [
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
    Ok(Some(profile))
}

fn download_and_commit(
    url: &str,
    destination: &Path,
    expected_sha256: Option<&str>,
    replace: bool,
) -> Result<()> {
    if destination.exists() && !replace {
        bail!(
            "{} already exists; repeat with --replace to replace it",
            destination.display()
        );
    }
    if !url.starts_with("https://") {
        bail!("catalog URL must use HTTPS");
    }
    let temporary = temporary_path(destination)?;
    let result = (|| {
        let status = Command::new("curl")
            .args(["--fail", "--location", "--silent", "--show-error"])
            .arg("--output")
            .arg(&temporary)
            .arg(url)
            .status()
            .context("run curl (install it to enable model downloads)")?;
        if !status.success() {
            bail!("download failed with {status}");
        }
        let actual = sha256_file(&temporary)?;
        if let Some(expected) = expected_sha256 {
            if !actual.eq_ignore_ascii_case(expected) {
                bail!("download checksum mismatch: expected {expected}, got {actual}");
            }
        }
        validate_asset(&temporary)?;
        fs::rename(&temporary, destination)
            .with_context(|| format!("publish downloaded asset {}", destination.display()))?;
        Ok::<(), anyhow::Error>(())
    })();
    if let Err(error) = result {
        fs::remove_file(&temporary).ok();
        return Err(error);
    }
    Ok(())
}

pub(crate) fn validate_asset(path: &Path) -> Result<()> {
    NamChain::load(&[path.to_path_buf()], SAMPLE_RATE, 256)
        .with_context(|| format!("validate downloaded asset {}", path.display()))?;
    Ok(())
}

pub(crate) fn safe_file_name(name: &str) -> Result<&str> {
    let path = Path::new(name);
    if name.is_empty() || path.file_name().and_then(|value| value.to_str()) != Some(name) {
        bail!("asset name must be a plain filename");
    }
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if !matches!(extension.as_str(), "nam" | "wav") {
        bail!("asset name must end in .nam or .wav");
    }
    Ok(name)
}

pub(crate) fn temporary_path(destination: &Path) -> Result<PathBuf> {
    let file = destination
        .file_name()
        .and_then(|value| value.to_str())
        .context("destination needs a UTF-8 filename")?;
    let parent = destination
        .parent()
        .context("asset destination needs a parent directory")?;
    let staging = parent.join(".shr-tone-over-9000-staging");
    fs::create_dir_all(&staging)
        .with_context(|| format!("create staging directory {}", staging.display()))?;
    Ok(staging.join(format!("{}-{file}", std::process::id())))
}

fn installed_matches(path: &Path, expected: &str) -> bool {
    path.is_file()
        && sha256_file(path)
            .map(|actual| actual.eq_ignore_ascii_case(expected))
            .unwrap_or(false)
}

pub(crate) fn sha256_file(path: &Path) -> Result<String> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = reader
            .read(&mut buffer)
            .with_context(|| format!("read {}", path.display()))?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    let mut output = String::with_capacity(64);
    for byte in digest.finalize() {
        write!(&mut output, "{byte:02x}").expect("writing to String cannot fail");
    }
    Ok(output)
}

fn browse(category: &str) -> Result<()> {
    let gears = match category {
        "all" => None,
        "pedal" => Some("pedal"),
        "amp" => Some("amp"),
        "cab" => Some("cab"),
        "rig" | "amp-cab" => Some("amp-cab"),
        _ => bail!("browse category must be all, pedal, amp, cab, or rig"),
    };
    let url = gears.map_or_else(
        || "https://www.tone3000.com/search".to_owned(),
        |gear| format!("https://www.tone3000.com/search?gears={gear}"),
    );
    println!("{url}");
    if env::var_os("DISPLAY").is_some() || env::var_os("WAYLAND_DISPLAY").is_some() {
        match Command::new("xdg-open").arg(&url).status() {
            Ok(status) if status.success() => {}
            Ok(status) => eprintln!("browser returned {status}; open the URL above"),
            Err(error) => eprintln!("could not open browser: {error}; open the URL above"),
        }
    }
    Ok(())
}

fn print_help() {
    println!(
        "Model and cabinet manager\n\
         \n\
         shr-tone-over-9000 models list [pedal|amp|cab|rig]\n\
         shr-tone-over-9000 models install [starter|CATEGORY|ID] [--accept-t3k]\n\
         shr-tone-over-9000 models verify [starter|CATEGORY|ID]\n\
         shr-tone-over-9000 models import PATH [--name FILE] [--replace] [IR METADATA]\n\
         shr-tone-over-9000 models add-url HTTPS_URL --name FILE [--sha256 HASH] [--replace] [IR METADATA]\n\
         shr-tone-over-9000 models browse [all|pedal|amp|cab|rig]\n\
         shr-tone-over-9000 models hub COMMAND [OPTIONS]\n\
         \n\
         Common option: --models-dir DIR (default: user data directory)\n\
         IR metadata: --cab-id ID --cabinet LABEL --speaker LABEL --mic LABEL\n\
                      [--position LABEL] [--variant LABEL]\n\
         Downloads and imports are validated before an atomic rename. Existing\n\
         files are preserved unless --replace is explicit. TONE3000 catalog\n\
         files require acknowledgement because T3K permits local use but not\n\
         redistribution. Run `models hub help` for metadata search and exact\n\
         one-model downloads."
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use hound::{SampleFormat, WavSpec, WavWriter};
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

    fn temporary_directory() -> PathBuf {
        let sequence = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "tone9000-manager-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn write_identity_ir(path: &Path) {
        let mut writer = WavWriter::create(
            path,
            WavSpec {
                channels: 1,
                sample_rate: SAMPLE_RATE,
                bits_per_sample: 24,
                sample_format: SampleFormat::Int,
            },
        )
        .unwrap();
        writer.write_sample(4_000_000_i32).unwrap();
        writer.finalize().unwrap();
    }

    #[test]
    fn asset_names_cannot_escape_library() {
        assert!(safe_file_name("amp.nam").is_ok());
        assert!(safe_file_name("cab.wav").is_ok());
        assert!(safe_file_name("../amp.nam").is_err());
        assert!(safe_file_name("nested/cab.wav").is_err());
        assert!(safe_file_name("notes.txt").is_err());
    }

    #[test]
    fn valid_import_is_test_loaded_then_published() {
        let root = temporary_directory();
        let source = root.join("source.wav");
        let library = root.join("library");
        write_identity_ir(&source);

        import_file(&source, Some("cab.wav"), &library, false, None).unwrap();

        assert!(library.join("cab.wav").is_file());
        assert!(!library.join("source.wav").exists());
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn ir_import_persists_pack_and_microphone_metadata() {
        let root = temporary_directory();
        let source = root.join("source.wav");
        let library = root.join("library");
        write_identity_ir(&source);
        let profile = IrProfile {
            cabinet_id: "mesa-412".to_owned(),
            cabinet: "MESA 4X12".to_owned(),
            speaker: "V30".to_owned(),
            microphone: "SM57".to_owned(),
            position: "CAP EDGE".to_owned(),
            variant: "57 EDGE".to_owned(),
        };

        import_file(
            &source,
            Some("mesa-sm57.wav"),
            &library,
            false,
            Some(profile.clone()),
        )
        .unwrap();

        assert_eq!(
            catalog::custom_ir_profiles(&library)
                .unwrap()
                .get("mesa-sm57.wav"),
            Some(&profile)
        );
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn partial_ir_metadata_is_rejected_before_install() {
        let options = ManagerOptions {
            microphone: Some("SM57".to_owned()),
            ..ManagerOptions::default()
        };
        assert!(ir_profile_from_options(&options, "cab.wav").is_err());
        assert!(ir_profile_from_options(&options, "amp.nam").is_err());
    }

    #[test]
    fn invalid_import_leaves_library_unchanged() {
        let root = temporary_directory();
        let source = root.join("broken.wav");
        let library = root.join("library");
        fs::write(&source, b"not a wave file").unwrap();

        assert!(import_file(&source, Some("cab.wav"), &library, false, None).is_err());

        assert!(!library.join("cab.wav").exists());
        let staging = library.join(".shr-tone-over-9000-staging");
        assert_eq!(fs::read_dir(staging).unwrap().count(), 0);
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn hub_options_preserve_query_and_validate_paging() {
        let options = parse(
            [
                "hub",
                "search",
                "marshall",
                "jcm",
                "bass",
                "3-4",
                "--page",
                "2",
                "--architecture",
                "1",
            ]
            .into_iter()
            .map(OsString::from)
            .collect(),
        )
        .unwrap();
        assert_eq!(
            options.positional,
            ["hub", "search", "marshall", "jcm", "bass", "3-4"]
        );
        assert_eq!(options.page, Some(2));
        assert_eq!(options.architecture.as_deref(), Some("1"));
        assert!(parse(vec![OsString::from("--page"), OsString::from("0")]).is_err());
        assert!(parse(vec![OsString::from("--architecture"), OsString::from("3")]).is_err());
    }
}
