use super::auth::{self, AuthOptions, Session};
use anyhow::{bail, Context, Result};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

const API_ROOT: &str = "https://www.tone3000.com";
const MAX_MODEL_BYTES: u64 = 100 * 1024 * 1024;
const MAX_JSON_BYTES: u64 = 5 * 1024 * 1024;

#[derive(Clone, Debug, Deserialize)]
pub struct EmbeddedUser {
    pub username: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct NamedValue {
    pub name: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Tone {
    pub id: u64,
    pub user: EmbeddedUser,
    pub title: String,
    pub description: Option<String>,
    pub gear: String,
    pub format: String,
    pub license: String,
    #[serde(default)]
    pub makes: Vec<NamedValue>,
    #[serde(default)]
    pub tags: Vec<NamedValue>,
    pub models_count: usize,
    pub url: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Model {
    pub id: u64,
    pub model_url: String,
    pub name: String,
    pub size: String,
    pub tone_id: u64,
    #[serde(default)]
    pub architecture_version: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
pub struct Page<T> {
    pub data: Vec<T>,
    pub page: usize,
    pub total: usize,
    pub total_pages: usize,
}

pub trait ToneApi {
    fn search_tones(&mut self, query: &str, page: usize, architecture: &str) -> Result<Page<Tone>>;
    fn list_models(&mut self, tone_id: u64, architecture: &str) -> Result<Page<Model>>;
    fn get_tone(&mut self, tone_id: u64) -> Result<Tone>;
    fn get_model(&mut self, model_id: u64) -> Result<Model>;
}

pub struct Client {
    auth_options: AuthOptions,
    session: Option<Session>,
}

impl Client {
    pub fn new(auth_options: AuthOptions) -> Self {
        Self {
            auth_options,
            session: None,
        }
    }

    pub fn connect(&mut self) -> Result<()> {
        self.session = Some(auth::connect(&self.auth_options)?);
        Ok(())
    }

    pub fn download_model(&mut self, model_url: &str, destination: &Path) -> Result<()> {
        validate_model_url(model_url)?;
        let mut session = self.current_session()?;
        let mut status = authenticated_download(model_url, &session.access_token, destination)?;
        if status == 401 {
            fs::remove_file(destination).ok();
            session = auth::refresh(&self.auth_options, &session)?;
            self.session = Some(session.clone());
            status = authenticated_download(model_url, &session.access_token, destination)?;
        }
        if !(200..300).contains(&status) {
            fs::remove_file(destination).ok();
            bail!("TONE3000 model download failed with HTTP {status}");
        }
        let bytes = fs::metadata(destination)
            .with_context(|| format!("inspect downloaded model {}", destination.display()))?
            .len();
        if bytes == 0 || bytes > MAX_MODEL_BYTES {
            fs::remove_file(destination).ok();
            bail!("TONE3000 model download has invalid size {bytes} bytes");
        }
        Ok(())
    }

    fn current_session(&mut self) -> Result<Session> {
        let mut session = match self.session.clone() {
            Some(session) => session,
            None => auth::load_or_connect(&self.auth_options)?,
        };
        if auth::expires_soon(&session) {
            session = auth::refresh(&self.auth_options, &session)?;
        }
        self.session = Some(session.clone());
        Ok(session)
    }

    fn get_json<T: DeserializeOwned>(&mut self, path: &str) -> Result<T> {
        let url = api_url(path)?;
        let mut session = self.current_session()?;
        let mut response = authenticated_get(&url, &session.access_token)?;
        if response.status == 401 {
            session = auth::refresh(&self.auth_options, &session)?;
            self.session = Some(session.clone());
            response = authenticated_get(&url, &session.access_token)?;
        }
        if !(200..300).contains(&response.status) {
            match response.status {
                403 => bail!(
                    "TONE3000 denied this API operation (HTTP 403); confirm that this client is approved for custom search"
                ),
                429 => bail!("TONE3000 rate limit reached (HTTP 429); wait before retrying"),
                status => bail!("TONE3000 API request failed with HTTP {status}"),
            }
        }
        serde_json::from_slice(&response.body)
            .with_context(|| format!("parse TONE3000 response for {path}"))
    }
}

impl ToneApi for Client {
    fn search_tones(&mut self, query: &str, page: usize, architecture: &str) -> Result<Page<Tone>> {
        let query = form_urlencoded::Serializer::new(String::new())
            .append_pair("query", query)
            .append_pair("page", &page.to_string())
            .append_pair("page_size", "10")
            .append_pair(
                "sort",
                if query.is_empty() {
                    "trending"
                } else {
                    "best-match"
                },
            )
            .append_pair("gears", "amp")
            .append_pair("format", "nam")
            .append_pair("architecture", architecture)
            .finish();
        self.get_json(&format!("/api/v1/tones/search?{query}"))
    }

    fn list_models(&mut self, tone_id: u64, architecture: &str) -> Result<Page<Model>> {
        self.get_json(&format!(
            "/api/v1/models?tone_id={tone_id}&page=1&page_size=300&architecture={architecture}"
        ))
    }

    fn get_tone(&mut self, tone_id: u64) -> Result<Tone> {
        self.get_json(&format!("/api/v1/tones/{tone_id}"))
    }

    fn get_model(&mut self, model_id: u64) -> Result<Model> {
        self.get_json(&format!("/api/v1/models/{model_id}"))
    }
}

struct HttpResponse {
    status: u16,
    body: Vec<u8>,
}

fn api_url(path: &str) -> Result<String> {
    if !path.starts_with("/api/v1/") || path.contains(['\r', '\n']) {
        bail!("refusing a TONE3000 API path outside /api/v1");
    }
    Ok(format!("{API_ROOT}{path}"))
}

fn validate_model_url(value: &str) -> Result<()> {
    if !value.starts_with("https://www.tone3000.com/") || value.contains(['\r', '\n']) {
        bail!("refusing to send TONE3000 credentials outside www.tone3000.com");
    }
    Ok(())
}

fn authenticated_get(url: &str, token: &str) -> Result<HttpResponse> {
    let mut child = Command::new("curl")
        .args([
            "--silent",
            "--show-error",
            "--location",
            "--max-time",
            "30",
            "--max-filesize",
            &MAX_JSON_BYTES.to_string(),
            "--proto",
            "=https",
            "--proto-redir",
            "=https",
            "--header",
            "@-",
            "--write-out",
            "\n%{http_code}",
            url,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .context("run curl for TONE3000 API")?;
    write_bearer(child.stdin.take(), token)?;
    let output = child.wait_with_output().context("wait for TONE3000 API")?;
    if !output.status.success() {
        bail!("TONE3000 API transport failed with {}", output.status);
    }
    let (body, status) = split_status(output.stdout)?;
    Ok(HttpResponse { status, body })
}

fn authenticated_download(url: &str, token: &str, destination: &Path) -> Result<u16> {
    let mut child = Command::new("curl")
        .args([
            "--silent",
            "--show-error",
            "--location",
            "--max-time",
            "180",
            "--max-filesize",
            &MAX_MODEL_BYTES.to_string(),
            "--proto",
            "=https",
            "--proto-redir",
            "=https",
            "--header",
            "@-",
            "--output",
        ])
        .arg(destination)
        .args(["--write-out", "%{http_code}", url])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .context("run curl for one TONE3000 model")?;
    write_bearer(child.stdin.take(), token)?;
    let output = child
        .wait_with_output()
        .context("wait for TONE3000 model download")?;
    if !output.status.success() {
        fs::remove_file(destination).ok();
        bail!("TONE3000 model transport failed with {}", output.status);
    }
    std::str::from_utf8(&output.stdout)
        .context("curl returned a non-UTF-8 download status")?
        .trim()
        .parse::<u16>()
        .context("curl returned an invalid download status")
}

fn write_bearer(stdin: Option<std::process::ChildStdin>, token: &str) -> Result<()> {
    if token.bytes().any(|byte| matches!(byte, b'\r' | b'\n')) {
        bail!("stored TONE3000 access token is malformed");
    }
    let mut stdin = stdin.context("open curl authentication input")?;
    writeln!(stdin, "Authorization: Bearer {token}").context("write curl authentication")?;
    writeln!(stdin, "Content-Type: application/json").context("write curl content type")?;
    Ok(())
}

fn split_status(mut output: Vec<u8>) -> Result<(Vec<u8>, u16)> {
    let newline = output
        .iter()
        .rposition(|byte| *byte == b'\n')
        .context("curl response omitted HTTP status")?;
    let status = std::str::from_utf8(&output[newline + 1..])
        .context("curl returned a non-UTF-8 HTTP status")?
        .trim()
        .parse::<u16>()
        .context("curl returned an invalid HTTP status")?;
    output.truncate(newline);
    Ok((output, status))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_download_credentials_are_origin_bound() {
        assert!(validate_model_url("https://www.tone3000.com/api/v1/models/4/download").is_ok());
        assert!(validate_model_url("http://www.tone3000.com/api/v1/models/4/download").is_err());
        assert!(validate_model_url("https://example.org/model.nam").is_err());
    }

    #[test]
    fn documented_response_shapes_deserialize() {
        let tone: Tone = serde_json::from_str(
            r#"{"id":7,"user":{"username":"maker"},"title":"JCM","description":null,"gear":"amp","format":"nam","license":"t3k","makes":[{"name":"Marshall JCM800"}],"tags":[],"models_count":1,"url":"https://www.tone3000.com/tones/7"}"#,
        )
        .unwrap();
        let model: Model = serde_json::from_str(
            r#"{"id":8,"model_url":"https://www.tone3000.com/api/v1/models/8/download","name":"G6 B3 M7 T2","size":"standard","tone_id":7,"architecture_version":"2"}"#,
        )
        .unwrap();
        assert_eq!(tone.user.username, "maker");
        assert_eq!(model.tone_id, tone.id);
    }
}
