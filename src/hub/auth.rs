use anyhow::{bail, Context, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const AUTHORIZE_URL: &str = "https://www.tone3000.com/api/v1/oauth/authorize";
const TOKEN_URL: &str = "https://www.tone3000.com/api/v1/oauth/token";
const DEFAULT_REDIRECT: &str = "http://127.0.0.1:43900/callback";
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Clone, Debug, Default)]
pub struct AuthOptions {
    pub client_id: Option<String>,
    pub redirect_uri: Option<String>,
    pub auth_file: Option<PathBuf>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Session {
    pub client_id: String,
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at_unix: u64,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
    expires_in: u64,
}

pub fn connect(options: &AuthOptions) -> Result<Session> {
    connect_with_progress(
        options,
        &AtomicBool::new(false),
        |handoff, redirect_uri| {
            println!("Connect TONE3000 in a browser or phone on the same LAN:");
            println!("{handoff}");
            println!("Waiting up to five minutes for the callback at {redirect_uri} …");
        },
        true,
    )
}

pub fn connect_with_progress(
    options: &AuthOptions,
    cancelled: &AtomicBool,
    progress: impl FnOnce(&str, &str),
    open_browser: bool,
) -> Result<Session> {
    let client_id = resolve_client_id(options)?;
    let redirect_uri = options
        .redirect_uri
        .clone()
        .or_else(|| std::env::var("TONE3000_REDIRECT_URI").ok())
        .unwrap_or_else(|| DEFAULT_REDIRECT.to_owned());
    let redirect = validate_redirect(&redirect_uri)?;
    let listener = bind_callback(&redirect)?;
    listener
        .set_nonblocking(true)
        .context("configure OAuth callback listener")?;

    let verifier = random_url_token(48)?;
    let state = random_url_token(32)?;
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let authorize_query = form_urlencoded::Serializer::new(String::new())
        .append_pair("client_id", &client_id)
        .append_pair("redirect_uri", &redirect_uri)
        .append_pair("response_type", "code")
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", &state)
        .finish();
    let authorize = format!("{AUTHORIZE_URL}?{authorize_query}");

    progress(&redirect.handoff_url(), &redirect_uri);
    if open_browser
        && (std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some())
    {
        match Command::new("xdg-open").arg(&authorize).status() {
            Ok(status) if status.success() => {}
            Ok(status) => eprintln!("browser returned {status}; use the URL above"),
            Err(error) => eprintln!("could not open a browser: {error}; use the URL above"),
        }
    }

    let callback = wait_for_callback(&listener, &redirect.path, &authorize, &state, cancelled)?;
    if cancelled.load(Ordering::Relaxed) {
        bail!("TONE3000 connection cancelled; credentials were not stored");
    }
    let body = form_urlencoded::Serializer::new(String::new())
        .append_pair("grant_type", "authorization_code")
        .append_pair("code", &callback)
        .append_pair("code_verifier", &verifier)
        .append_pair("redirect_uri", &redirect_uri)
        .append_pair("client_id", &client_id)
        .finish();
    let response = post_form(TOKEN_URL, &body)?;
    let tokens: TokenResponse =
        serde_json::from_slice(&response).context("parse TONE3000 OAuth token response")?;
    let session = Session {
        client_id,
        access_token: tokens.access_token,
        refresh_token: tokens.refresh_token,
        expires_at_unix: now_unix().saturating_add(tokens.expires_in),
    };
    save(options, &session)?;
    Ok(session)
}

pub fn load_or_connect(options: &AuthOptions) -> Result<Session> {
    match load(options)? {
        Some(session) => {
            let configured = if options.client_id.is_some() {
                options.client_id.clone()
            } else {
                configured_client_id()?
            };
            if let Some(requested) = configured.as_deref() {
                if requested != session.client_id {
                    bail!(
                        "stored TONE3000 session belongs to a different client ID; run `models hub connect --client-id {requested}`"
                    );
                }
            }
            Ok(session)
        }
        None if options.client_id.is_some() || std::env::var_os("TONE3000_CLIENT_ID").is_some() => {
            connect(options)
        }
        None => bail!(
            "TONE3000 is not connected; run `models hub connect --client-id t3k_pub_…` or set TONE3000_CLIENT_ID"
        ),
    }
}

pub(super) fn is_connected_for_configuration(options: &AuthOptions) -> Result<bool> {
    let Some(session) = load(options)? else {
        return Ok(false);
    };
    Ok(configured_client_id()?.is_none_or(|configured| configured == session.client_id))
}

pub fn refresh(options: &AuthOptions, session: &Session) -> Result<Session> {
    let body = form_urlencoded::Serializer::new(String::new())
        .append_pair("grant_type", "refresh_token")
        .append_pair("refresh_token", &session.refresh_token)
        .append_pair("client_id", &session.client_id)
        .finish();
    let response = post_form(TOKEN_URL, &body).context("refresh TONE3000 access token")?;
    let tokens: TokenResponse =
        serde_json::from_slice(&response).context("parse refreshed TONE3000 session")?;
    let refreshed = Session {
        client_id: session.client_id.clone(),
        access_token: tokens.access_token,
        refresh_token: tokens.refresh_token,
        expires_at_unix: now_unix().saturating_add(tokens.expires_in),
    };
    save(options, &refreshed)?;
    Ok(refreshed)
}

pub fn expires_soon(session: &Session) -> bool {
    session.expires_at_unix <= now_unix().saturating_add(60)
}

pub fn load(options: &AuthOptions) -> Result<Option<Session>> {
    let path = auth_path(options);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    let mode = fs::metadata(&path)
        .with_context(|| format!("inspect credentials {}", path.display()))?
        .permissions()
        .mode()
        & 0o777;
    if mode & 0o077 != 0 {
        bail!(
            "TONE3000 credentials {} have mode {mode:o}; run `chmod 600 {}` before retrying",
            path.display(),
            path.display()
        );
    }
    let session = serde_json::from_slice(&bytes)
        .with_context(|| format!("parse TONE3000 credentials {}", path.display()))?;
    Ok(Some(session))
}

fn save(options: &AuthOptions, session: &Session) -> Result<()> {
    let path = auth_path(options);
    let parent = path
        .parent()
        .context("TONE3000 credential path needs a parent")?;
    fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    if options.auth_file.is_none() && std::env::var_os("RPI_TONE_HUB_AUTH").is_none() {
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("secure {}", parent.display()))?;
    }
    let temporary = parent.join(format!(".tone3000-auth.tmp-{}", std::process::id()));
    let bytes = serde_json::to_vec(session).context("serialize TONE3000 credentials")?;
    let result = (|| {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&temporary)
            .with_context(|| format!("create staged credentials {}", temporary.display()))?;
        file.write_all(&bytes)
            .with_context(|| format!("write staged credentials {}", temporary.display()))?;
        file.sync_all()
            .with_context(|| format!("sync staged credentials {}", temporary.display()))?;
        fs::rename(&temporary, &path)
            .with_context(|| format!("publish credentials {}", path.display()))?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("secure credentials {}", path.display()))?;
        Ok::<(), anyhow::Error>(())
    })();
    if result.is_err() {
        fs::remove_file(&temporary).ok();
    }
    result
}

fn resolve_client_id(options: &AuthOptions) -> Result<String> {
    let client_id = options
        .client_id
        .clone()
        .or(configured_client_id()?)
        .or_else(|| {
            load(options)
                .ok()
                .flatten()
                .map(|session| session.client_id)
        })
        .context("TONE3000 connect requires a valid --client-id t3k_pub_…")?;
    validate_client_id(&client_id)?;
    Ok(client_id)
}

pub(super) fn configured_client_id() -> Result<Option<String>> {
    if let Some(value) = std::env::var_os("TONE3000_CLIENT_ID").filter(|value| !value.is_empty()) {
        let value = value
            .into_string()
            .map_err(|_| anyhow::anyhow!("TONE3000_CLIENT_ID must be valid UTF-8"))?;
        validate_client_id(&value)?;
        return Ok(Some(value));
    }
    let path = hub_config_path();
    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    parse_config_client_id(&contents)
        .with_context(|| format!("parse TONE3000 hub config {}", path.display()))
}

fn parse_config_client_id(contents: &str) -> Result<Option<String>> {
    let mut client_id = None;
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .context("hub config entries must use key=value")?;
        if key.trim() != "client_id" {
            bail!("unknown hub config key {:?}", key.trim());
        }
        if client_id.is_some() {
            bail!("hub config contains client_id more than once");
        }
        let value = value.trim().to_owned();
        validate_client_id(&value)?;
        client_id = Some(value);
    }
    Ok(client_id)
}

fn validate_client_id(value: &str) -> Result<()> {
    if value.len() <= "t3k_pub_".len()
        || !value.starts_with("t3k_pub_")
        || value.contains(char::is_whitespace)
    {
        bail!("TONE3000 client ID must have the form t3k_pub_…");
    }
    Ok(())
}

fn hub_config_path() -> PathBuf {
    if let Some(path) = std::env::var_os("RPI_TONE_HUB_CONFIG").filter(|path| !path.is_empty()) {
        return PathBuf::from(path);
    }
    if let Some(path) = std::env::var_os("XDG_CONFIG_HOME").filter(|path| !path.is_empty()) {
        return PathBuf::from(path)
            .join("rpi-tone-over-9000")
            .join("hub.conf");
    }
    if let Some(path) = std::env::var_os("HOME").filter(|path| !path.is_empty()) {
        return PathBuf::from(path).join(".config/rpi-tone-over-9000/hub.conf");
    }
    PathBuf::from(".").join(".rpi-tone-over-9000-hub.conf")
}

fn auth_path(options: &AuthOptions) -> PathBuf {
    if let Some(path) = &options.auth_file {
        return path.clone();
    }
    if let Some(path) = std::env::var_os("RPI_TONE_HUB_AUTH").filter(|path| !path.is_empty()) {
        return PathBuf::from(path);
    }
    if let Some(path) = std::env::var_os("XDG_CONFIG_HOME").filter(|path| !path.is_empty()) {
        return PathBuf::from(path)
            .join("rpi-tone-over-9000")
            .join("tone3000-auth.json");
    }
    if let Some(path) = std::env::var_os("HOME").filter(|path| !path.is_empty()) {
        return PathBuf::from(path).join(".config/rpi-tone-over-9000/tone3000-auth.json");
    }
    PathBuf::from(".").join(".rpi-tone-over-9000-tone3000-auth.json")
}

fn random_url_token(bytes: usize) -> Result<String> {
    let mut random = vec![0_u8; bytes];
    File::open("/dev/urandom")
        .context("open operating-system random source")?
        .read_exact(&mut random)
        .context("read operating-system random source")?;
    Ok(URL_SAFE_NO_PAD.encode(random))
}

struct Redirect {
    host: String,
    port: u16,
    path: String,
}

impl Redirect {
    fn handoff_url(&self) -> String {
        format!("http://{}:{}/", self.host, self.port)
    }
}

fn validate_redirect(value: &str) -> Result<Redirect> {
    let remainder = value
        .strip_prefix("http://")
        .context("TONE3000 redirect URI must use HTTP")?;
    if remainder.contains(['?', '#', '@']) {
        bail!("TONE3000 redirect URI must be a plain HTTP callback URL");
    }
    let (authority, path) = remainder
        .split_once('/')
        .context("redirect URI requires a callback path")?;
    if path.is_empty() || authority.contains(['/', '[', ']']) {
        bail!("redirect URI requires a host, explicit port, and callback path");
    }
    if !path
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || "/-._~".contains(character))
    {
        bail!("redirect URI callback path contains unsupported characters");
    }
    let (host, port) = authority
        .rsplit_once(':')
        .context("redirect URI requires an explicit TCP port")?;
    let port = port
        .parse::<u16>()
        .context("redirect URI has an invalid port")?;
    if port == 0 {
        bail!("redirect URI port must be greater than zero");
    }
    let allowed = host == "localhost"
        || host
            .parse::<Ipv4Addr>()
            .is_ok_and(|address| address.is_loopback() || is_private_v4(address));
    if !allowed {
        bail!("redirect URI host must be localhost, IPv4 loopback, or a private IPv4 LAN address");
    }
    Ok(Redirect {
        host: host.to_owned(),
        port,
        path: format!("/{path}"),
    })
}

fn is_private_v4(address: Ipv4Addr) -> bool {
    address.is_private() || address.is_link_local()
}

fn bind_callback(redirect: &Redirect) -> Result<TcpListener> {
    let address = if redirect.host == "localhost" {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), redirect.port)
    } else {
        let host = redirect
            .host
            .parse::<IpAddr>()
            .context("redirect URI host must be an IP address")?;
        SocketAddr::new(host, redirect.port)
    };
    TcpListener::bind(address).with_context(|| format!("listen for OAuth callback on {address}"))
}

fn wait_for_callback(
    listener: &TcpListener,
    callback_path: &str,
    authorize_url: &str,
    expected_state: &str,
    cancelled: &AtomicBool,
) -> Result<String> {
    let deadline = Instant::now() + CALLBACK_TIMEOUT;
    loop {
        if cancelled.load(Ordering::Relaxed) {
            bail!("TONE3000 connection cancelled; credentials were not stored");
        }
        match listener.accept() {
            Ok((mut stream, _)) => {
                let target = match read_request_target(&mut stream) {
                    Ok(target) => target,
                    Err(_) => {
                        respond_status(&mut stream, "400 Bad Request", "Invalid request.");
                        continue;
                    }
                };
                let path = target
                    .split_once('?')
                    .map_or(target.as_str(), |(path, _)| path);
                if path == "/" {
                    respond_with_redirect(&mut stream, authorize_url);
                    continue;
                }
                if path != callback_path {
                    respond_status(&mut stream, "404 Not Found", "Not found.");
                    continue;
                }
                let callback = parse_callback_target(&target, expected_state);
                respond_to_browser(&mut stream, callback.is_ok());
                return callback;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    bail!("timed out waiting for TONE3000; repeat the same search to retry");
                }
                thread::sleep(Duration::from_millis(100));
            }
            Err(error) => return Err(error).context("accept OAuth callback"),
        }
    }
}

fn read_request_target(stream: &mut TcpStream) -> Result<String> {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .context("set OAuth callback read timeout")?;
    let mut request = Vec::new();
    let mut bytes = [0_u8; 1024];
    loop {
        let count = stream.read(&mut bytes).context("read OAuth callback")?;
        if count == 0 {
            bail!("OAuth callback ended before its request line");
        }
        request.extend_from_slice(&bytes[..count]);
        if request.contains(&b'\n') {
            break;
        }
        if request.len() >= 8192 {
            bail!("OAuth callback request line is too long");
        }
    }
    let request = std::str::from_utf8(&request).context("OAuth callback is not UTF-8")?;
    request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .map(str::to_owned)
        .context("OAuth callback has no request target")
}

fn parse_callback_target(target: &str, expected_state: &str) -> Result<String> {
    let query = target
        .split_once('?')
        .map(|(_, query)| query)
        .context("OAuth callback omitted parameters")?;
    let fields =
        form_urlencoded::parse(query.as_bytes()).collect::<std::collections::BTreeMap<_, _>>();
    let returned_state = fields
        .get("state")
        .context("OAuth callback omitted state")?;
    if returned_state.as_ref() != expected_state {
        bail!("TONE3000 OAuth state did not match; credentials were not stored");
    }
    if let Some(error) = fields.get("error") {
        let description = fields
            .get("error_description")
            .map(|value| format!(": {value}"))
            .unwrap_or_default();
        bail!("TONE3000 authorization failed: {error}{description}");
    }
    fields
        .get("code")
        .map(|value| value.to_string())
        .filter(|value| !value.is_empty())
        .context("OAuth callback omitted authorization code")
}

fn respond_to_browser(stream: &mut TcpStream, success: bool) {
    let message = if success {
        "TONE3000 connected. Return to the device."
    } else {
        "TONE3000 connection failed. Return to the device for details."
    };
    let body = format!(
        "<!doctype html><meta name=viewport content='width=device-width'><title>TONE3000</title><p>{message}</p>"
    );
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).ok();
}

fn respond_with_redirect(stream: &mut TcpStream, location: &str) {
    let response = format!(
        "HTTP/1.1 302 Found\r\nLocation: {location}\r\nCache-Control: no-store\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(response.as_bytes()).ok();
}

fn respond_status(stream: &mut TcpStream, status: &str, message: &str) {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{message}",
        message.len()
    );
    stream.write_all(response.as_bytes()).ok();
}

fn post_form(url: &str, body: &str) -> Result<Vec<u8>> {
    let mut child = Command::new("curl")
        .args([
            "--silent",
            "--show-error",
            "--max-time",
            "30",
            "--proto",
            "=https",
            "--request",
            "POST",
            "--header",
            "Content-Type: application/x-www-form-urlencoded",
            "--data-binary",
            "@-",
            "--write-out",
            "\n%{http_code}",
            url,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .context("run curl for TONE3000 authentication")?;
    child
        .stdin
        .take()
        .context("open curl request body")?
        .write_all(body.as_bytes())
        .context("write curl request body")?;
    let output = child
        .wait_with_output()
        .context("wait for TONE3000 authentication")?;
    if !output.status.success() {
        bail!(
            "TONE3000 authentication transport failed with {}",
            output.status
        );
    }
    let (body, status) = split_status(output.stdout)?;
    if !(200..300).contains(&status) {
        bail!("TONE3000 authentication failed with HTTP {status}");
    }
    Ok(body)
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

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_FILE: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn redirect_accepts_loopback_and_private_lan_only() {
        assert!(validate_redirect("http://127.0.0.1:43900/callback").is_ok());
        assert!(validate_redirect("http://192.168.1.8:43900/callback").is_ok());
        assert!(validate_redirect("https://127.0.0.1:43900/callback").is_err());
        assert!(validate_redirect("http://8.8.8.8:43900/callback").is_err());
        assert!(validate_redirect("http://192.168.1.8:43900/call back").is_err());
    }

    #[test]
    fn callback_requires_matching_state_and_surfaces_denial() {
        assert_eq!(
            parse_callback_target("/callback?code=abc&state=expected", "expected").unwrap(),
            "abc"
        );
        assert!(parse_callback_target("/callback?code=abc&state=wrong", "expected").is_err());
        let error = parse_callback_target(
            "/callback?error=access_denied&error_description=no&state=expected",
            "expected",
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("access_denied"));
    }

    #[test]
    fn hub_config_accepts_one_publishable_client_id() {
        assert_eq!(
            parse_config_client_id("# installation-specific\nclient_id=t3k_pub_test\n")
                .unwrap()
                .as_deref(),
            Some("t3k_pub_test")
        );
        assert!(parse_config_client_id("client_id=secret_value").is_err());
        assert!(parse_config_client_id("client_id=t3k_pub_one\nclient_id=t3k_pub_two").is_err());
        assert!(parse_config_client_id("secret_key=never").is_err());
    }

    #[test]
    fn callback_wait_can_be_cancelled_before_authorization() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let cancelled = AtomicBool::new(true);
        let error = wait_for_callback(
            &listener,
            "/callback",
            "https://example.test/authorize",
            "state",
            &cancelled,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("cancelled"));
    }

    #[test]
    fn local_handoff_redirects_then_waits_for_exact_callback() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let client = thread::spawn(move || {
            let unknown = request(address, "/favicon.ico");
            assert!(unknown.starts_with("HTTP/1.1 404 Not Found"));

            let handoff = request(address, "/");
            assert!(handoff.starts_with("HTTP/1.1 302 Found"));
            assert!(handoff.contains("Location: https://example.test/oauth?opaque=long"));

            let callback = request(address, "/callback?code=exact&state=expected");
            assert!(callback.starts_with("HTTP/1.1 200 OK"));
            assert!(callback.contains("TONE3000 connected"));
        });
        let code = wait_for_callback(
            &listener,
            "/callback",
            "https://example.test/oauth?opaque=long",
            "expected",
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(code, "exact");
        client.join().unwrap();
    }

    fn request(address: SocketAddr, target: &str) -> String {
        let mut stream = TcpStream::connect(address).unwrap();
        write!(
            stream,
            "GET {target} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
            address
        )
        .unwrap();
        stream.shutdown(std::net::Shutdown::Write).unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response
    }

    #[test]
    fn credentials_are_owner_only_and_round_trip() {
        let sequence = NEXT_FILE.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("tone9000-auth-{}-{sequence}", std::process::id()));
        let options = AuthOptions {
            auth_file: Some(root.join("auth.json")),
            ..AuthOptions::default()
        };
        let session = Session {
            client_id: "t3k_pub_test".to_owned(),
            access_token: "access".to_owned(),
            refresh_token: "refresh".to_owned(),
            expires_at_unix: u64::MAX,
        };
        save(&options, &session).unwrap();
        assert_eq!(load(&options).unwrap().unwrap().access_token, "access");
        let auth_path = options.auth_file.as_ref().unwrap();
        let mode = fs::metadata(auth_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        fs::set_permissions(auth_path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(load(&options).is_err());
        fs::remove_dir_all(root).ok();
    }
}
