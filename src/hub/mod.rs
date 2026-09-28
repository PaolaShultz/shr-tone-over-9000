mod auth;
mod tone3000;

use crate::model_provenance::{self, HubIdentity, ModelProvenance};
use crate::model_search::{CaptureSettings, ModelSearch, SettingEvidence};
use anyhow::{bail, Context, Result};
use auth::AuthOptions;
use std::net::{Ipv4Addr, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::{SystemTime, UNIX_EPOCH};
use tone3000::{Client, Model, Tone, ToneApi};

#[derive(Clone, Debug)]
pub struct HubOptions {
    pub models_dir: PathBuf,
    pub client_id: Option<String>,
    pub redirect_uri: Option<String>,
    pub auth_file: Option<PathBuf>,
    pub page: usize,
    pub architecture: String,
    pub name: Option<String>,
    pub replace: bool,
}

pub fn run(command: Option<&str>, arguments: &[String], options: HubOptions) -> Result<()> {
    let auth_options = AuthOptions {
        client_id: options.client_id.clone(),
        redirect_uri: options.redirect_uri.clone(),
        auth_file: options.auth_file.clone(),
    };
    let mut client = Client::new(auth_options);
    match command.unwrap_or("help") {
        "help" | "-h" | "--help" => print_help(),
        "connect" => {
            if !arguments.is_empty() {
                bail!("models hub connect takes no positional arguments");
            }
            client.connect()?;
            println!("TONE3000 connected; credentials stored with owner-only permissions");
        }
        "search" => {
            let query = arguments.join(" ");
            search(&mut client, &query, options.page, &options.architecture)?;
        }
        "show" => {
            let model_id = one_model_id(arguments, "show")?;
            show(&mut client, model_id, &options.models_dir)?;
        }
        "download" => {
            let model_id = one_model_id(arguments, "download")?;
            download(
                &mut client,
                model_id,
                &options.models_dir,
                options.name.as_deref(),
                options.replace,
            )?;
        }
        other => bail!("unknown hub command {other:?}; use `models hub help`"),
    }
    Ok(())
}

#[derive(Clone, Debug)]
struct Candidate {
    tone: Tone,
    model: Model,
    settings: CaptureSettings,
}

#[derive(Debug)]
struct SearchReport {
    candidates: Vec<Candidate>,
    tones_total: usize,
    tones_page: usize,
    tones_pages: usize,
    models_scanned: usize,
    models_missing_settings: usize,
}

#[derive(Clone, Debug)]
pub(crate) struct HubSearchItem {
    pub model_id: u64,
    pub tone_title: String,
    pub model_name: String,
    pub make: String,
    pub creator: String,
    pub license: String,
    pub source_url: String,
    pub size: String,
    pub architecture: String,
    pub settings: String,
    pub setting_sources: String,
    pub matched: String,
}

#[derive(Clone, Debug)]
pub(crate) struct HubSearchPage {
    pub items: Vec<HubSearchItem>,
    pub page: usize,
    pub total_pages: usize,
    pub tones_total: usize,
    pub models_scanned: usize,
    pub models_missing_settings: usize,
}

pub(crate) fn is_connected() -> Result<bool> {
    auth::is_connected_for_configuration(&AuthOptions::default())
}

pub(crate) fn configured_client_id() -> Result<String> {
    Ok(auth::configured_client_id()?.unwrap_or_default())
}

pub(crate) fn suggested_redirect_uri() -> Result<String> {
    if let Ok(value) = std::env::var("TONE3000_REDIRECT_URI") {
        return Ok(value);
    }
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
        .context("inspect this device's Wi-Fi address")?;
    socket
        .connect((Ipv4Addr::new(1, 1, 1, 1), 80))
        .context("find this device's active network route")?;
    redirect_uri_for_address(socket.local_addr()?.ip())
}

fn redirect_uri_for_address(address: std::net::IpAddr) -> Result<String> {
    match address {
        std::net::IpAddr::V4(address) if address.is_private() || address.is_link_local() => {
            Ok(format!("http://{address}:43900/callback"))
        }
        _ => bail!("connect this device to Wi-Fi before starting TONE3000 authorization"),
    }
}

pub(crate) fn connect_for_tui(
    client_id: String,
    redirect_uri: String,
    cancelled: &AtomicBool,
    progress: impl FnOnce(String, String),
) -> Result<()> {
    let options = AuthOptions {
        client_id: Some(client_id),
        redirect_uri: Some(redirect_uri),
        auth_file: None,
    };
    auth::connect_with_progress(
        &options,
        cancelled,
        |handoff, redirect| progress(handoff.to_owned(), redirect.to_owned()),
        false,
    )?;
    Ok(())
}

pub(crate) fn search_for_tui(
    query: &str,
    page: usize,
    architecture_filter: &str,
    cancelled: &AtomicBool,
) -> Result<HubSearchPage> {
    let search = ModelSearch::parse(query)?;
    let mut client = Client::new(AuthOptions::default());
    let report = find_candidates_inner(
        &mut client,
        &search,
        page,
        architecture_filter,
        Some(cancelled),
    )?;
    let items = report
        .candidates
        .into_iter()
        .map(|candidate| HubSearchItem {
            model_id: candidate.model.id,
            tone_title: safe_display(&candidate.tone.title),
            model_name: safe_display(&candidate.model.name),
            make: names(&candidate.tone.makes),
            creator: safe_display(&candidate.tone.user.username),
            license: safe_display(&candidate.tone.license),
            source_url: safe_display(&candidate.tone.url),
            size: safe_display(&candidate.model.size),
            architecture: architecture(&candidate.model),
            settings: candidate.settings.compact(),
            setting_sources: setting_sources(&candidate.settings),
            matched: matched_settings(&search, &candidate.settings),
        })
        .collect();
    Ok(HubSearchPage {
        items,
        page: report.tones_page,
        total_pages: report.tones_pages.max(1),
        tones_total: report.tones_total,
        models_scanned: report.models_scanned,
        models_missing_settings: report.models_missing_settings,
    })
}

pub(crate) fn download_for_tui(model_id: u64, models_dir: &Path) -> Result<PathBuf> {
    let mut client = Client::new(AuthOptions::default());
    download_one(&mut client, model_id, models_dir, None, false)
}

pub(crate) fn installed_model_path(model_id: u64, models_dir: &Path) -> Result<Option<PathBuf>> {
    Ok(
        model_provenance::find_model(models_dir, "tone3000", model_id)?
            .map(|record| models_dir.join(record.file))
            .filter(|path| path.is_file()),
    )
}

fn search(api: &mut impl ToneApi, query: &str, page: usize, architecture: &str) -> Result<()> {
    let search = ModelSearch::parse(query)?;
    print_search(&search, architecture);
    let report = find_candidates(api, &search, page, architecture)?;
    for (index, candidate) in report.candidates.iter().enumerate() {
        println!();
        println!(
            "{}  t3k:model:{}  {}",
            index + 1,
            candidate.model.id,
            safe_display(&candidate.tone.title)
        );
        println!("   {}", safe_display(&candidate.model.name));
        println!("   {}", candidate.settings.compact());
        println!("   SETTINGS FROM {}", setting_sources(&candidate.settings));
        let matched = matched_settings(&search, &candidate.settings);
        if !matched.is_empty() {
            println!("   MATCH {matched}");
        }
        println!(
            "   @{} · {} · {}",
            safe_display(&candidate.tone.user.username),
            safe_display(&candidate.tone.license),
            safe_display(&candidate.tone.url)
        );
    }
    println!();
    if report.candidates.is_empty() {
        println!(
            "0 exact parameter matches; {} model records scanned, {} lacked one or more requested settings",
            report.models_scanned, report.models_missing_settings
        );
        println!("Constraints were not relaxed. Change the query explicitly to broaden it.");
    } else {
        println!("{} exact parameter match(es)", report.candidates.len());
    }
    println!(
        "Tone page {}/{} ({} hub tones total); use --page N to inspect another page",
        report.tones_page,
        report.tones_pages.max(1),
        report.tones_total
    );
    Ok(())
}

fn matched_settings(search: &ModelSearch, settings: &CaptureSettings) -> String {
    search
        .settings
        .iter()
        .map(|(parameter, constraint)| {
            let value = settings.values[parameter].value;
            format!(
                "{} {} ✓ {}",
                parameter.label(),
                crate::model_search::display_number(value),
                constraint
            )
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

fn find_candidates(
    api: &mut impl ToneApi,
    search: &ModelSearch,
    page: usize,
    architecture: &str,
) -> Result<SearchReport> {
    find_candidates_inner(api, search, page, architecture, None)
}

fn find_candidates_inner(
    api: &mut impl ToneApi,
    search: &ModelSearch,
    page: usize,
    architecture: &str,
    cancelled: Option<&AtomicBool>,
) -> Result<SearchReport> {
    ensure_not_cancelled(cancelled)?;
    let tones = api.search_tones(&search.identity, page, architecture)?;
    let mut candidates = Vec::new();
    let mut models_scanned = 0;
    let mut models_missing_settings = 0;
    for tone in tones.data {
        ensure_not_cancelled(cancelled)?;
        if tone.gear != "amp" || tone.format != "nam" {
            continue;
        }
        let models = api.list_models(tone.id, architecture)?;
        if models.total > models.data.len() {
            bail!(
                "TONE3000 returned only {} of {} model metadata records for tone {}; narrow the search rather than accepting incomplete results",
                models.data.len(),
                models.total,
                tone.id
            );
        }
        for model in models.data {
            if model.tone_id != tone.id {
                bail!(
                    "TONE3000 model {} belongs to tone {}, not requested tone {}",
                    model.id,
                    model.tone_id,
                    tone.id
                );
            }
            models_scanned += 1;
            let settings = capture_settings(&tone, &model);
            let has_all = search
                .settings
                .keys()
                .all(|parameter| settings.values.contains_key(parameter));
            if !has_all {
                models_missing_settings += 1;
                continue;
            }
            if search.matches(&settings) {
                candidates.push(Candidate {
                    tone: tone.clone(),
                    model,
                    settings,
                });
            }
        }
    }
    Ok(SearchReport {
        candidates,
        tones_total: tones.total,
        tones_page: tones.page,
        tones_pages: tones.total_pages,
        models_scanned,
        models_missing_settings,
    })
}

fn ensure_not_cancelled(cancelled: Option<&AtomicBool>) -> Result<()> {
    if cancelled.is_some_and(|value| value.load(std::sync::atomic::Ordering::Relaxed)) {
        bail!("TONE3000 search cancelled; query was kept for retry");
    }
    Ok(())
}

fn print_search(search: &ModelSearch, architecture: &str) {
    println!("SEARCH");
    println!(
        "  make/model: {}",
        if search.identity.is_empty() {
            "ANY"
        } else {
            &search.identity
        }
    );
    println!("  gear/format: amp / NAM architecture {architecture}");
    for (parameter, constraint) in &search.settings {
        println!("  {:<11} {constraint}", format!("{}:", parameter.label()));
    }
}

fn show(api: &mut impl ToneApi, model_id: u64, models_dir: &Path) -> Result<()> {
    let candidate = get_candidate(api, model_id)?;
    println!("t3k:model:{}", candidate.model.id);
    println!(
        "tone       {} (t3k:tone:{})",
        safe_display(&candidate.tone.title),
        candidate.tone.id
    );
    println!("model      {}", safe_display(&candidate.model.name));
    println!("make       {}", names(&candidate.tone.makes));
    println!("tags       {}", names(&candidate.tone.tags));
    println!(
        "creator    @{}",
        safe_display(&candidate.tone.user.username)
    );
    println!(
        "gear       {} / {}",
        safe_display(&candidate.tone.gear),
        safe_display(&candidate.tone.format)
    );
    println!("size       {}", safe_display(&candidate.model.size));
    println!("architecture {}", architecture(&candidate.model));
    println!("license    {}", safe_display(&candidate.tone.license));
    println!("source     {}", safe_display(&candidate.tone.url));
    println!("settings   {}", candidate.settings.compact());
    for (parameter, setting) in &candidate.settings.values {
        let (source, fragment) = match &setting.evidence {
            SettingEvidence::ModelName { fragment } => ("model name", fragment),
            SettingEvidence::ToneDescription { fragment } => ("tone description", fragment),
        };
        println!(
            "  {:<9} {} from {source}: {:?}",
            parameter.label(),
            crate::model_search::display_number(setting.value),
            safe_display(fragment)
        );
    }
    match model_provenance::find_model(models_dir, "tone3000", model_id)? {
        Some(record) if models_dir.join(&record.file).is_file() => {
            println!("installed  {}", models_dir.join(record.file).display())
        }
        _ => println!("installed  no"),
    }
    Ok(())
}

fn download(
    client: &mut Client,
    model_id: u64,
    models_dir: &Path,
    requested_name: Option<&str>,
    replace: bool,
) -> Result<()> {
    let candidate = get_candidate(client, model_id)?;
    println!(
        "download t3k:model:{} · {} · {}",
        candidate.model.id,
        safe_display(&candidate.tone.title),
        safe_display(&candidate.model.name)
    );
    println!("settings {}", candidate.settings.compact());
    println!(
        "license {} · source {}",
        safe_display(&candidate.tone.license),
        safe_display(&candidate.tone.url)
    );
    let destination = download_candidate(client, candidate, models_dir, requested_name, replace)?;
    let sha256 = crate::model_manager::sha256_file(&destination)?;
    println!("installed {} sha256 {sha256}", destination.display());
    Ok(())
}

fn download_one(
    client: &mut Client,
    model_id: u64,
    models_dir: &Path,
    requested_name: Option<&str>,
    replace: bool,
) -> Result<PathBuf> {
    let candidate = get_candidate(client, model_id)?;
    download_candidate(client, candidate, models_dir, requested_name, replace)
}

fn download_candidate(
    client: &mut Client,
    candidate: Candidate,
    models_dir: &Path,
    requested_name: Option<&str>,
    replace: bool,
) -> Result<PathBuf> {
    let model_id = candidate.model.id;
    if candidate.tone.gear != "amp" || candidate.tone.format != "nam" {
        bail!("t3k:model:{model_id} is not an amp NAM capture");
    }
    let generated = generated_filename(&candidate.model);
    let file = crate::model_manager::safe_file_name(requested_name.unwrap_or(&generated))?;
    if !file
        .rsplit_once('.')
        .is_some_and(|(_, extension)| extension.eq_ignore_ascii_case("nam"))
    {
        bail!("hub amp model filename must end in .nam");
    }
    let destination = models_dir.join(file);
    std::fs::create_dir_all(models_dir)
        .with_context(|| format!("create model directory {}", models_dir.display()))?;
    if destination.exists() && !replace {
        bail!(
            "{} already exists; preserve it or repeat with --replace",
            destination.display()
        );
    }
    model_provenance::records(models_dir)
        .context("read existing hub provenance before download")?;
    let had_existing = destination.exists();
    let temporary = crate::model_manager::temporary_path(&destination)?;
    let backup = temporary.with_extension("previous");
    let mut published = false;
    let mut backup_active = false;
    let result = (|| {
        client.download_model(&candidate.model.model_url, &temporary)?;
        crate::model_manager::validate_asset(&temporary)?;
        let sha256 = crate::model_manager::sha256_file(&temporary)?;
        let model_architecture = architecture(&candidate.model);
        if had_existing {
            std::fs::rename(&destination, &backup).with_context(|| {
                format!(
                    "stage existing model {} for replacement",
                    destination.display()
                )
            })?;
            backup_active = true;
        }
        if let Err(error) = std::fs::rename(&temporary, &destination) {
            if backup_active {
                std::fs::rename(&backup, &destination).ok();
                backup_active = false;
            }
            return Err(error)
                .with_context(|| format!("publish downloaded model {}", destination.display()));
        }
        published = true;
        let record = ModelProvenance {
            file: file.to_owned(),
            identity: HubIdentity {
                provider: "tone3000".to_owned(),
                tone_id: candidate.tone.id,
                model_id: candidate.model.id,
            },
            tone_title: candidate.tone.title,
            model_name: candidate.model.name,
            creator: candidate.tone.user.username,
            source_url: candidate.tone.url,
            license: candidate.tone.license,
            architecture: model_architecture,
            settings: candidate.settings,
            parser_version: 1,
            downloaded_at_unix: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            sha256: sha256.clone(),
        };
        if let Err(error) = model_provenance::upsert(models_dir, record) {
            std::fs::remove_file(&destination).ok();
            published = false;
            if backup_active {
                std::fs::rename(&backup, &destination).with_context(|| {
                    format!("restore {} after provenance failure", destination.display())
                })?;
                backup_active = false;
            }
            return Err(error).context("record downloaded model provenance");
        }
        if backup_active {
            backup_active = false;
            if let Err(error) = std::fs::remove_file(&backup) {
                eprintln!(
                    "installed model, but could not remove replacement backup {}: {error}",
                    backup.display()
                );
            }
        }
        Ok::<(), anyhow::Error>(())
    })();
    if result.is_err() {
        std::fs::remove_file(&temporary).ok();
        if published && !had_existing {
            std::fs::remove_file(&destination).ok();
        }
        if backup_active && !destination.exists() {
            std::fs::rename(&backup, &destination).ok();
        }
    }
    result?;
    Ok(destination)
}

fn get_candidate(api: &mut impl ToneApi, model_id: u64) -> Result<Candidate> {
    let model = api.get_model(model_id)?;
    let tone = api.get_tone(model.tone_id)?;
    let settings = capture_settings(&tone, &model);
    Ok(Candidate {
        tone,
        model,
        settings,
    })
}

fn capture_settings(tone: &Tone, model: &Model) -> CaptureSettings {
    let description = if tone.models_count == 1 {
        tone.description.as_deref()
    } else {
        tone.description
            .as_deref()
            .and_then(|description| description_line_for_model(description, &model.name))
    };
    CaptureSettings::from_model(&model.name, description)
}

fn description_line_for_model<'a>(description: &'a str, model_name: &str) -> Option<&'a str> {
    let name = model_name.trim().to_ascii_lowercase();
    if name.len() < 3 {
        return None;
    }
    description.lines().find(|line| {
        let line = line.trim().to_ascii_lowercase();
        line.strip_prefix(&name).is_some_and(|suffix| {
            suffix.is_empty()
                || suffix.starts_with(char::is_whitespace)
                || suffix.starts_with([':', '-', '–', '—'])
        })
    })
}

fn one_model_id(arguments: &[String], action: &str) -> Result<u64> {
    if arguments.len() != 1 {
        bail!("models hub {action} requires exactly one t3k:model:ID");
    }
    arguments[0]
        .strip_prefix("t3k:model:")
        .context("hub handle must have the form t3k:model:ID")?
        .parse::<u64>()
        .context("hub model ID must be a positive integer")
        .and_then(|id| {
            if id == 0 {
                bail!("hub model ID must be positive")
            } else {
                Ok(id)
            }
        })
}

fn generated_filename(model: &Model) -> String {
    let mut slug = String::new();
    let mut separator = false;
    for character in model.name.chars().flat_map(char::to_lowercase) {
        if character.is_ascii_alphanumeric() {
            if separator && !slug.is_empty() {
                slug.push('-');
            }
            slug.push(character);
            separator = false;
        } else {
            separator = true;
        }
    }
    let slug = slug.trim_matches('-').chars().take(80).collect::<String>();
    if slug.is_empty() {
        format!("t3k-{}.nam", model.id)
    } else {
        format!("t3k-{}-{slug}.nam", model.id)
    }
}

fn names(values: &[tone3000::NamedValue]) -> String {
    let names = values
        .iter()
        .map(|value| safe_display(&value.name))
        .collect::<Vec<_>>();
    if names.is_empty() {
        "UNKNOWN".to_owned()
    } else {
        names.join(", ")
    }
}

fn safe_display(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect()
}

fn architecture(model: &Model) -> String {
    let value = model
        .architecture_version
        .as_ref()
        .map(|value| match value {
            serde_json::Value::String(value) => value.clone(),
            other => other.to_string(),
        })
        .unwrap_or_else(|| "unknown".to_owned());
    if matches!(value.as_str(), "1" | "2" | "custom") {
        value
    } else {
        "unknown".to_owned()
    }
}

fn setting_sources(settings: &CaptureSettings) -> String {
    let model_name = settings
        .values
        .values()
        .any(|setting| matches!(&setting.evidence, SettingEvidence::ModelName { .. }));
    let description = settings
        .values
        .values()
        .any(|setting| matches!(&setting.evidence, SettingEvidence::ToneDescription { .. }));
    match (model_name, description) {
        (true, true) => "MODEL NAME + TONE DESCRIPTION",
        (true, false) => "MODEL NAME",
        (false, true) => "TONE DESCRIPTION",
        (false, false) => "NO DOCUMENTED SETTINGS",
    }
    .to_owned()
}

fn print_help() {
    println!(
        "TONE3000 metadata search and one-model download\n\
         \n\
         shr-tone-over-9000 models hub connect --client-id t3k_pub_…\n\
         shr-tone-over-9000 models hub search QUERY [--page N] [--architecture 1|2|custom]\n\
         shr-tone-over-9000 models hub show t3k:model:ID\n\
         shr-tone-over-9000 models hub download t3k:model:ID [--name FILE.nam] [--replace]\n\
         \n\
         Search reads tone/model metadata only. A download command requires one\n\
         exact model ID and downloads one .nam file. Settings are extracted only\n\
         from explicit model names, a named model-description line, or a global\n\
         description for a one-model tone. Unknown settings never satisfy a\n\
         search constraint.\n\
         \n\
         Headless OAuth: pass --redirect-uri http://PRIVATE_PI_IP:PORT/callback\n\
         and open the printed URL on a phone connected to the same LAN."
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MockApi {
        tones: Vec<Tone>,
        models: Vec<Model>,
        model_lists: usize,
    }

    impl ToneApi for MockApi {
        fn search_tones(
            &mut self,
            _query: &str,
            page: usize,
            _architecture: &str,
        ) -> Result<tone3000::Page<Tone>> {
            Ok(tone3000::Page {
                data: self.tones.clone(),
                page,
                total: self.tones.len(),
                total_pages: 1,
            })
        }

        fn list_models(
            &mut self,
            tone_id: u64,
            _architecture: &str,
        ) -> Result<tone3000::Page<Model>> {
            self.model_lists += 1;
            let data = self
                .models
                .iter()
                .filter(|model| model.tone_id == tone_id)
                .cloned()
                .collect::<Vec<_>>();
            Ok(tone3000::Page {
                total: data.len(),
                data,
                page: 1,
                total_pages: 1,
            })
        }

        fn get_tone(&mut self, tone_id: u64) -> Result<Tone> {
            self.tones
                .iter()
                .find(|tone| tone.id == tone_id)
                .cloned()
                .context("missing mock tone")
        }

        fn get_model(&mut self, model_id: u64) -> Result<Model> {
            self.models
                .iter()
                .find(|model| model.id == model_id)
                .cloned()
                .context("missing mock model")
        }
    }

    fn tone(models_count: usize) -> Tone {
        Tone {
            id: 7,
            user: tone3000::EmbeddedUser {
                username: "maker".to_owned(),
            },
            title: "Marshall JCM800".to_owned(),
            description: Some("Bass 9 Mid 9 Treble 9".to_owned()),
            gear: "amp".to_owned(),
            format: "nam".to_owned(),
            license: "t3k".to_owned(),
            makes: vec![tone3000::NamedValue {
                name: "Marshall JCM800".to_owned(),
            }],
            tags: Vec::new(),
            models_count,
            url: "https://www.tone3000.com/tones/7".to_owned(),
        }
    }

    fn model(id: u64, name: &str) -> Model {
        Model {
            id,
            model_url: format!("https://www.tone3000.com/api/v1/models/{id}/download"),
            name: name.to_owned(),
            size: "standard".to_owned(),
            tone_id: 7,
            architecture_version: Some(serde_json::Value::String("2".to_owned())),
        }
    }

    #[test]
    fn exact_search_returns_only_model_level_matches() {
        let mut api = MockApi {
            tones: vec![tone(3)],
            models: vec![
                model(1, "G6 B3 M7 T2"),
                model(2, "G6 B5 M7 T2"),
                model(3, "G6 B3 M5 T2"),
            ],
            model_lists: 0,
        };
        let query = ModelSearch::parse("Marshall JCM bass 3-4 mid 6+ treble 2-3").unwrap();
        let report = find_candidates(&mut api, &query, 1, "2").unwrap();
        assert_eq!(report.candidates.len(), 1);
        assert_eq!(report.candidates[0].model.id, 1);
        assert_eq!(api.model_lists, 1);
    }

    #[test]
    fn multi_model_tone_description_is_not_assigned_to_each_capture() {
        let mut api = MockApi {
            tones: vec![tone(2)],
            models: vec![model(1, "capture one"), model(2, "capture two")],
            model_lists: 0,
        };
        let query = ModelSearch::parse("Marshall bass 9").unwrap();
        let report = find_candidates(&mut api, &query, 1, "2").unwrap();
        assert!(report.candidates.is_empty());
        assert_eq!(report.models_missing_settings, 2);
    }

    #[test]
    fn explicitly_named_description_line_can_supply_that_models_settings() {
        let mut source = tone(2);
        source.description = Some(
            "Clean: Gain 2 Bass 4 Mid 5 Treble 6\nLead: Gain 7 Bass 3 Mid 7 Treble 2".to_owned(),
        );
        let mut api = MockApi {
            tones: vec![source],
            models: vec![model(1, "Clean"), model(2, "Lead")],
            model_lists: 0,
        };
        let query = ModelSearch::parse("Marshall gain 7 bass 3 mid 7 treble 2").unwrap();
        let report = find_candidates(&mut api, &query, 1, "2").unwrap();
        assert_eq!(report.candidates.len(), 1);
        assert_eq!(report.candidates[0].model.id, 2);
    }

    #[test]
    fn handles_are_model_scoped_and_filename_is_safe() {
        assert_eq!(
            one_model_id(&["t3k:model:42".to_owned()], "show").unwrap(),
            42
        );
        assert!(one_model_id(&["t3k:tone:42".to_owned()], "show").is_err());
        assert_eq!(
            generated_filename(&model(42, "JCM 800 / B3")),
            "t3k-42-jcm-800-b3.nam"
        );
        assert!(generated_filename(&model(42, &"a".repeat(500))).len() < 100);
        assert_eq!(safe_display("amp\u{1b}[31m"), "amp [31m");
    }

    #[test]
    fn tui_search_cancellation_stops_before_api_work() {
        let mut api = MockApi {
            tones: vec![tone(1)],
            models: vec![model(1, "B3 M7 T2")],
            model_lists: 0,
        };
        let query = ModelSearch::parse("Marshall bass 3").unwrap();
        let cancelled = AtomicBool::new(true);
        let error = find_candidates_inner(&mut api, &query, 1, "2", Some(&cancelled))
            .unwrap_err()
            .to_string();
        assert!(error.contains("cancelled"));
        assert_eq!(api.model_lists, 0);
    }

    #[test]
    fn phone_callback_uses_only_private_lan_addresses() {
        assert_eq!(
            redirect_uri_for_address("192.168.4.20".parse().unwrap()).unwrap(),
            "http://192.168.4.20:43900/callback"
        );
        assert!(redirect_uri_for_address("8.8.8.8".parse().unwrap()).is_err());
        assert!(redirect_uri_for_address("2001:db8::1".parse().unwrap()).is_err());
    }
}
