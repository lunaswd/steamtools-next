//! Steam manifest request monitoring and depot-cache delivery.
//!
//! The monitor only tails `logs/content_log.txt`. Network I/O is supplied by a
//! small transport seam so the GUI can share the orchestration while
//! tests remain fully local. Downloaded payloads are validated before an
//! atomic write to `depotcache`.

use anyhow::{bail, Context, Result};
use reqwest::blocking::Client;
use reqwest::header::{HeaderName, HeaderValue, CONTENT_TYPE, USER_AGENT};
use reqwest::Method;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, VecDeque};
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{Cursor, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const LOG_PATTERN: &str = "BYldRequestDepotManifest(";
const MAX_LOG_CHUNK: u64 = 8 * 1024 * 1024;
const MAX_MANIFEST_BYTES: usize = 128 * 1024 * 1024;
const MAX_CODE_RESPONSE_BYTES: usize = 1024 * 1024;
const MAX_SERVER_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const MAX_SEEN_REQUESTS: usize = 4096;
const MANIFEST_MAGIC: [u8; 4] = [0xD0, 0x17, 0xF6, 0x71];
const REPARSE_POINT_ATTRIBUTE: u32 = 0x0400;

const CODE_SOURCES: &[(&str, &str)] = &[
    (
        "20770407",
        "https://20770407.xyz/manifest/{depot}/{manifest}",
    ),
    (
        "steamapi.993499094",
        "https://steamapi.993499094.xyz/manifest/{depot}/{manifest}",
    ),
    ("manifestdex", "https://manifest.manifestdex.com/{depot}"),
    ("wudrm", "http://gmrc.wudrm.com/manifest/{depot}"),
    (
        "steamrun",
        "https://manifest.steam.run/api/manifest/{depot}",
    ),
];

const SERVER_DIRECTORY_URL: &str = "https://api.steampowered.com/IContentServerDirectoryService/GetServersForSteamPipe/v1/?cell_id=0";
const FALLBACK_CDN_HOSTS: &[&str] = &[
    "client-download.steampowered.com",
    "content1.steampowered.com",
];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "camelCase")]
pub struct ManifestRequest {
    pub app_id: u32,
    pub depot_id: u32,
    pub manifest_id: u64,
}

impl ManifestRequest {
    fn key(&self) -> String {
        format!("{}:{}:{}", self.app_id, self.depot_id, self.manifest_id)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", default)]
pub struct ManifestLogCursor {
    pub offset: u64,
    pub prefix_sha256: String,
    pub prefix_len: u64,
    pub partial_line: String,
    pub seen_keys: VecDeque<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ManifestPoll {
    pub log_path: String,
    pub reset: bool,
    pub bytes_read: u64,
    pub requests: Vec<ManifestRequest>,
    pub malformed_lines: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestFetchMode {
    CodeDirect,
    ManifestHub { api_key: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpRequest {
    pub method: &'static str,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub timeout_seconds: u64,
    /// Callers must avoid displaying the URL when this is true.
    pub sensitive: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub content_type: Option<String>,
    pub body: Vec<u8>,
}

/// Adapter implemented by the Tauri network layer.
pub trait ManifestHttpTransport: Send + Sync {
    fn send(&self, request: &HttpRequest) -> Result<HttpResponse>;
}

/// Shared blocking HTTP adapter used by the Tauri bridge.
/// The caller still owns polling cadence and cancellation between requests.
pub struct ReqwestManifestTransport {
    client: Client,
}

impl ReqwestManifestTransport {
    pub fn new() -> Result<Self> {
        let client = Client::builder()
            .connect_timeout(std::time::Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::limited(5))
            .build()
            .context("could not create the manifest HTTP client")?;
        Ok(Self { client })
    }
}

impl ManifestHttpTransport for ReqwestManifestTransport {
    fn send(&self, request: &HttpRequest) -> Result<HttpResponse> {
        let method = Method::from_bytes(request.method.as_bytes())
            .context("manifest request uses an invalid HTTP method")?;
        let mut builder = self
            .client
            .request(method, &request.url)
            .timeout(std::time::Duration::from_secs(
                request.timeout_seconds.clamp(1, 300),
            ))
            .header(
                USER_AGENT,
                format!("SteamTools-Next/{}", env!("CARGO_PKG_VERSION")),
            );
        for (name, value) in &request.headers {
            let name = HeaderName::from_bytes(name.as_bytes())
                .with_context(|| format!("invalid manifest request header name: {name}"))?;
            let value =
                HeaderValue::from_str(value).context("invalid manifest request header value")?;
            builder = builder.header(name, value);
        }
        let response = builder.send().with_context(|| {
            if request.sensitive {
                "sensitive manifest request failed".to_string()
            } else {
                format!("manifest request failed: {}", request.url)
            }
        })?;
        let status = response.status().as_u16();
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        if response
            .content_length()
            .is_some_and(|length| length > MAX_MANIFEST_BYTES as u64)
        {
            bail!("manifest HTTP response exceeds the size limit");
        }
        let mut body = Vec::new();
        response
            .take(MAX_MANIFEST_BYTES as u64 + 1)
            .read_to_end(&mut body)
            .context("could not read the manifest HTTP response")?;
        if body.len() > MAX_MANIFEST_BYTES {
            bail!("manifest HTTP response exceeds the size limit");
        }
        Ok(HttpResponse {
            status,
            content_type,
            body,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ManifestAttempt {
    pub stage: String,
    pub source: String,
    pub status: Option<u16>,
    pub success: bool,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ManifestDownloadReceipt {
    pub request: ManifestRequest,
    pub source: String,
    pub destination: String,
    pub size_bytes: u64,
    pub sha256: String,
    pub replaced: bool,
    pub backup_path: Option<String>,
    pub attempts: Vec<ManifestAttempt>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ManifestBatchResult {
    pub poll: ManifestPoll,
    pub downloaded: Vec<ManifestDownloadReceipt>,
    pub failed: Vec<ManifestBatchFailure>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ManifestBatchFailure {
    pub request: ManifestRequest,
    pub error: String,
}

/// Incrementally read new Steam content-log data and return unseen requests.
///
/// Truncation and rotation are detected using both file length and the SHA-256
/// of the first 4 KiB. A bounded de-duplication queue survives resets.
pub fn poll_manifest_requests(
    steam_root: &Path,
    cursor: &mut ManifestLogCursor,
) -> Result<ManifestPoll> {
    let steam_root = checked_root(steam_root)?;
    let log_path = steam_root.join("logs").join("content_log.txt");
    if !log_path.exists() {
        return Ok(ManifestPoll {
            log_path: display_path(&log_path),
            reset: false,
            bytes_read: 0,
            requests: Vec::new(),
            malformed_lines: 0,
        });
    }
    require_regular_file(&log_path, "Steam content log")?;
    reject_linked_ancestors(&log_path)?;
    let metadata = fs::metadata(&log_path)?;
    let comparison_len = if cursor.prefix_len == 0 {
        metadata.len().min(4096)
    } else {
        cursor.prefix_len.min(metadata.len())
    };
    let mut prefix = file_prefix_sha256(&log_path, comparison_len)?;
    let mut reset = metadata.len() < cursor.offset;
    if cursor.offset > 0 && !cursor.prefix_sha256.is_empty() && cursor.prefix_sha256 != prefix {
        reset = true;
    }
    if reset {
        cursor.offset = 0;
        cursor.partial_line.clear();
        cursor.prefix_len = metadata.len().min(4096);
        prefix = file_prefix_sha256(&log_path, cursor.prefix_len)?;
    } else if cursor.prefix_len == 0 {
        cursor.prefix_len = comparison_len;
    }
    cursor.prefix_sha256 = prefix;

    let available = metadata.len().saturating_sub(cursor.offset);
    if available > MAX_LOG_CHUNK {
        // Bound a first scan of a very large historical log to its newest data.
        cursor.offset = metadata.len() - MAX_LOG_CHUNK;
        cursor.partial_line.clear();
        reset = true;
    }
    let start = cursor.offset;
    let mut file = File::open(&log_path)?;
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.take(MAX_LOG_CHUNK + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_LOG_CHUNK {
        bail!("Steam content log produced more than {MAX_LOG_CHUNK} bytes in one poll");
    }
    cursor.offset = start.saturating_add(bytes.len() as u64);

    let mut text = std::mem::take(&mut cursor.partial_line);
    text.push_str(&String::from_utf8_lossy(&bytes));
    let trailing_complete = text.ends_with('\n') || text.ends_with('\r');
    let mut lines: Vec<&str> = text.lines().collect();
    if !trailing_complete {
        cursor.partial_line = lines.pop().unwrap_or_default().to_string();
    }

    let mut seen: BTreeSet<String> = cursor.seen_keys.iter().cloned().collect();
    let mut requests = Vec::new();
    let mut malformed_lines = 0;
    for line in lines {
        if !line.contains(LOG_PATTERN) {
            continue;
        }
        match parse_manifest_log_line(line) {
            Some(request) => {
                let key = request.key();
                if seen.insert(key.clone()) {
                    cursor.seen_keys.push_back(key);
                    requests.push(request);
                }
            }
            None => malformed_lines += 1,
        }
    }
    while cursor.seen_keys.len() > MAX_SEEN_REQUESTS {
        if let Some(old) = cursor.seen_keys.pop_front() {
            seen.remove(&old);
        }
    }
    Ok(ManifestPoll {
        log_path: display_path(&log_path),
        reset,
        bytes_read: bytes.len() as u64,
        requests,
        malformed_lines,
    })
}

pub fn parse_manifest_log_line(line: &str) -> Option<ManifestRequest> {
    let start = line.find(LOG_PATTERN)? + LOG_PATTERN.len();
    let tail = &line[start..];
    let end = tail.find(')').unwrap_or(tail.len());
    let arguments = &tail[..end];
    let mut app_id = None;
    let mut depot_id = None;
    let mut manifest_id = None;
    for part in arguments.split(',') {
        let (name, value) = part.split_once(':')?;
        let value = value.trim().trim_matches(|c: char| !c.is_ascii_digit());
        match name.trim().to_ascii_lowercase().as_str() {
            "app" => app_id = value.parse::<u32>().ok().filter(|value| *value != 0),
            "depot" => depot_id = value.parse::<u32>().ok().filter(|value| *value != 0),
            "manifest" => manifest_id = value.parse::<u64>().ok().filter(|value| *value != 0),
            _ => {}
        }
    }
    Some(ManifestRequest {
        app_id: app_id?,
        depot_id: depot_id?,
        manifest_id: manifest_id?,
    })
}

/// Poll once, then download at most `limit` unseen requests sequentially.
/// Pacing/concurrency policy remains with the caller so desktop cancellation is
/// not hidden inside a blocking core function.
pub fn process_manifest_log_once<T: ManifestHttpTransport>(
    steam_root: &Path,
    cursor: &mut ManifestLogCursor,
    transport: &T,
    mode: &ManifestFetchMode,
    limit: usize,
) -> Result<ManifestBatchResult> {
    let poll = poll_manifest_requests(steam_root, cursor)?;
    let mut downloaded = Vec::new();
    let mut failed = Vec::new();
    for request in poll.requests.iter().take(limit.min(100)) {
        match fetch_and_store_manifest(steam_root, request, transport, mode) {
            Ok(receipt) => downloaded.push(receipt),
            Err(error) => failed.push(ManifestBatchFailure {
                request: request.clone(),
                error: format!("{error:#}"),
            }),
        }
    }
    Ok(ManifestBatchResult {
        poll,
        downloaded,
        failed,
    })
}

pub fn fetch_and_store_manifest<T: ManifestHttpTransport>(
    steam_root: &Path,
    manifest: &ManifestRequest,
    transport: &T,
    mode: &ManifestFetchMode,
) -> Result<ManifestDownloadReceipt> {
    validate_manifest_request(manifest)?;
    let steam_root = checked_root(steam_root)?;
    let (source, payload, attempts) = match mode {
        ManifestFetchMode::CodeDirect => fetch_code_direct(manifest, transport)?,
        ManifestFetchMode::ManifestHub { api_key } => {
            fetch_manifest_hub(manifest, api_key, transport)?
        }
    };
    let payload = normalize_manifest_payload(&payload)?;
    let depotcache = steam_root.join("depotcache");
    create_directory_checked(&depotcache)?;
    let destination = depotcache.join(format!(
        "{}_{}.manifest",
        manifest.depot_id, manifest.manifest_id
    ));
    let write = atomic_manifest_write(&destination, &payload)?;
    Ok(ManifestDownloadReceipt {
        request: manifest.clone(),
        source,
        destination: display_path(&destination),
        size_bytes: payload.len() as u64,
        sha256: sha256_hex(&payload),
        replaced: write.0,
        backup_path: write.1.as_deref().map(display_path),
        attempts,
    })
}

fn fetch_manifest_hub<T: ManifestHttpTransport>(
    manifest: &ManifestRequest,
    api_key: &str,
    transport: &T,
) -> Result<(String, Vec<u8>, Vec<ManifestAttempt>)> {
    let key = api_key.trim();
    if key.is_empty()
        || key.len() > 4096
        || key.chars().any(|value| matches!(value, '\r' | '\n' | '\0'))
    {
        bail!("ManifestHub API key is empty or invalid");
    }
    let url = format!(
        "https://api.manifesthub2.filegear-sg.me/manifest?apikey={}&depotid={}&manifestid={}",
        percent_encode(key),
        manifest.depot_id,
        manifest.manifest_id
    );
    let request = HttpRequest {
        method: "GET",
        url,
        headers: browser_headers(),
        timeout_seconds: 30,
        sensitive: true,
    };
    let response = transport
        .send(&request)
        .context("ManifestHub request failed")?;
    let success = (200..300).contains(&response.status);
    let attempt = ManifestAttempt {
        stage: "download".into(),
        source: "ManifestHub".into(),
        status: Some(response.status),
        success,
        detail: if success {
            "Manifest payload received".into()
        } else {
            manifest_hub_status_detail(response.status).into()
        },
    };
    if !success {
        bail!("ManifestHub: {}", attempt.detail);
    }
    response_limit(&response, MAX_MANIFEST_BYTES)?;
    Ok(("ManifestHub".into(), response.body, vec![attempt]))
}

fn fetch_code_direct<T: ManifestHttpTransport>(
    manifest: &ManifestRequest,
    transport: &T,
) -> Result<(String, Vec<u8>, Vec<ManifestAttempt>)> {
    let mut attempts = Vec::new();
    let cdn_hosts = load_cdn_hosts(transport, &mut attempts);
    for (source_name, template) in CODE_SOURCES {
        let url = template
            .replace("{depot}", &manifest.depot_id.to_string())
            .replace("{manifest}", &manifest.manifest_id.to_string());
        let request = HttpRequest {
            method: "GET",
            url,
            headers: browser_headers(),
            timeout_seconds: 15,
            sensitive: false,
        };
        let response = match transport.send(&request) {
            Ok(response) => response,
            Err(error) => {
                attempts.push(ManifestAttempt {
                    stage: "code".into(),
                    source: (*source_name).into(),
                    status: None,
                    success: false,
                    detail: format!("Request failed: {error:#}"),
                });
                continue;
            }
        };
        if let Err(error) = response_limit(&response, MAX_CODE_RESPONSE_BYTES) {
            attempts.push(ManifestAttempt {
                stage: "code".into(),
                source: (*source_name).into(),
                status: Some(response.status),
                success: false,
                detail: error.to_string(),
            });
            continue;
        }
        if !(200..300).contains(&response.status) {
            attempts.push(ManifestAttempt {
                stage: "code".into(),
                source: (*source_name).into(),
                status: Some(response.status),
                success: false,
                detail: "Code source returned a non-success status".into(),
            });
            continue;
        }
        let Some(code) = parse_request_code(&response.body) else {
            attempts.push(ManifestAttempt {
                stage: "code".into(),
                source: (*source_name).into(),
                status: Some(response.status),
                success: false,
                detail: "Response did not contain a usable request code".into(),
            });
            continue;
        };
        attempts.push(ManifestAttempt {
            stage: "code".into(),
            source: (*source_name).into(),
            status: Some(response.status),
            success: true,
            detail: "Request code received".into(),
        });

        for host in &cdn_hosts {
            let url = format!(
                "https://{host}/depot/{}/manifest/{}/5/{code}",
                manifest.depot_id, manifest.manifest_id
            );
            let request = HttpRequest {
                method: "GET",
                url,
                headers: browser_headers(),
                timeout_seconds: 45,
                sensitive: false,
            };
            let response = match transport.send(&request) {
                Ok(response) => response,
                Err(error) => {
                    attempts.push(ManifestAttempt {
                        stage: "cdn".into(),
                        source: host.clone(),
                        status: None,
                        success: false,
                        detail: format!("CDN request failed: {error:#}"),
                    });
                    continue;
                }
            };
            let valid = (200..300).contains(&response.status)
                && response_limit(&response, MAX_MANIFEST_BYTES).is_ok()
                && normalize_manifest_payload(&response.body).is_ok();
            attempts.push(ManifestAttempt {
                stage: "cdn".into(),
                source: host.clone(),
                status: Some(response.status),
                success: valid,
                detail: if valid {
                    "Manifest payload received".into()
                } else {
                    "CDN rejected the code or returned an invalid payload".into()
                },
            });
            if valid {
                return Ok((format!("{source_name} via {host}"), response.body, attempts));
            }
        }
    }
    bail!("all manifest code sources and Steam CDN hosts failed")
}

fn load_cdn_hosts<T: ManifestHttpTransport>(
    transport: &T,
    attempts: &mut Vec<ManifestAttempt>,
) -> Vec<String> {
    let request = HttpRequest {
        method: "GET",
        url: SERVER_DIRECTORY_URL.into(),
        headers: browser_headers(),
        timeout_seconds: 10,
        sensitive: false,
    };
    let mut hosts = Vec::new();
    match transport.send(&request) {
        Ok(response)
            if (200..300).contains(&response.status)
                && response.body.len() <= MAX_SERVER_RESPONSE_BYTES =>
        {
            hosts = parse_cdn_hosts(&response.body);
            attempts.push(ManifestAttempt {
                stage: "cdn_directory".into(),
                source: "Steam server directory".into(),
                status: Some(response.status),
                success: !hosts.is_empty(),
                detail: format!("Discovered {} HTTPS host(s)", hosts.len()),
            });
        }
        Ok(response) => attempts.push(ManifestAttempt {
            stage: "cdn_directory".into(),
            source: "Steam server directory".into(),
            status: Some(response.status),
            success: false,
            detail: "Server directory was unavailable or oversized".into(),
        }),
        Err(error) => attempts.push(ManifestAttempt {
            stage: "cdn_directory".into(),
            source: "Steam server directory".into(),
            status: None,
            success: false,
            detail: format!("Server directory request failed: {error:#}"),
        }),
    }
    for fallback in FALLBACK_CDN_HOSTS {
        if !hosts.iter().any(|host| host == fallback) {
            hosts.push((*fallback).into());
        }
    }
    hosts.truncate(12);
    hosts
}

fn parse_cdn_hosts(body: &[u8]) -> Vec<String> {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(body) else {
        return Vec::new();
    };
    let servers = value
        .get("response")
        .and_then(|value| value.get("servers"))
        .and_then(|value| value.as_array())
        .or_else(|| value.get("servers").and_then(|value| value.as_array()));
    let Some(servers) = servers else {
        return Vec::new();
    };
    let mut hosts = BTreeSet::new();
    for server in servers {
        let https = server
            .get("https_support")
            .and_then(|value| value.as_str())
            .map(|value| value != "none")
            .or_else(|| {
                server
                    .get("https_support")
                    .and_then(|value| value.as_bool())
            })
            .unwrap_or(true);
        if !https {
            continue;
        }
        let host = server
            .get("vhost")
            .or_else(|| server.get("host"))
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .trim();
        if valid_host(host) {
            hosts.insert(host.to_ascii_lowercase());
        }
    }
    hosts.into_iter().collect()
}

fn parse_request_code(body: &[u8]) -> Option<String> {
    if body.is_empty() || body.len() > MAX_CODE_RESPONSE_BYTES {
        return None;
    }
    if let Ok(value) = serde_json::from_slice::<serde_json::Value>(body) {
        for key in [
            "code",
            "request_code",
            "requestCode",
            "manifest_request_code",
            "manifestRequestCode",
        ] {
            if let Some(value) = find_json_scalar(&value, key) {
                if valid_decimal(&value) {
                    return Some(value);
                }
            }
        }
    }
    let text = String::from_utf8_lossy(body);
    for token in text.split(|character: char| !character.is_ascii_digit()) {
        if valid_decimal(token) {
            return Some(token.into());
        }
    }
    None
}

fn find_json_scalar(value: &serde_json::Value, key: &str) -> Option<String> {
    match value {
        serde_json::Value::Object(map) => {
            if let Some(value) = map.get(key) {
                match value {
                    serde_json::Value::String(value) => return Some(value.clone()),
                    serde_json::Value::Number(value) => return Some(value.to_string()),
                    _ => {}
                }
            }
            map.values().find_map(|value| find_json_scalar(value, key))
        }
        serde_json::Value::Array(values) => {
            values.iter().find_map(|value| find_json_scalar(value, key))
        }
        _ => None,
    }
}

fn normalize_manifest_payload(body: &[u8]) -> Result<Vec<u8>> {
    if body.is_empty() {
        bail!("manifest payload is empty");
    }
    if body.len() > MAX_MANIFEST_BYTES {
        bail!("manifest payload exceeds the {MAX_MANIFEST_BYTES} byte limit");
    }
    if body.starts_with(&MANIFEST_MAGIC) {
        return Ok(body.to_vec());
    }
    let prefix = String::from_utf8_lossy(&body[..body.len().min(256)]).to_ascii_lowercase();
    if prefix.contains("<html") || prefix.contains("<!doctype") {
        bail!("manifest endpoint returned HTML instead of a manifest");
    }
    if body.starts_with(b"PK\x03\x04") {
        return extract_single_manifest_zip(body);
    }
    bail!("manifest payload has an invalid magic header")
}

fn extract_single_manifest_zip(body: &[u8]) -> Result<Vec<u8>> {
    let reader = Cursor::new(body);
    let mut archive = zip::ZipArchive::new(reader).context("manifest ZIP is invalid")?;
    let file_indices: Vec<_> = (0..archive.len())
        .filter(|index| archive.by_index(*index).is_ok_and(|entry| !entry.is_dir()))
        .collect();
    if file_indices.len() != 1 {
        bail!("manifest ZIP must contain exactly one regular file");
    }
    let entry = archive.by_index(file_indices[0])?;
    if entry.size() as usize > MAX_MANIFEST_BYTES {
        bail!("manifest ZIP entry exceeds the byte limit");
    }
    let mut output = Vec::with_capacity(entry.size() as usize);
    entry
        .take(MAX_MANIFEST_BYTES as u64 + 1)
        .read_to_end(&mut output)?;
    if output.len() > MAX_MANIFEST_BYTES {
        bail!("manifest ZIP expanded beyond the byte limit");
    }
    normalize_manifest_payload(&output)
}

fn atomic_manifest_write(destination: &Path, bytes: &[u8]) -> Result<(bool, Option<PathBuf>)> {
    let parent = destination
        .parent()
        .context("manifest destination has no parent")?;
    create_directory_checked(parent)?;
    reject_linked_ancestors(destination)?;
    let replaced = destination.exists();
    if replaced {
        require_regular_file(destination, "existing manifest")?;
        if fs::read(destination).is_ok_and(|existing| existing == bytes) {
            return Ok((true, None));
        }
    }
    let temporary = unique_sibling_path(destination, "stn-manifest")?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    if let Err(error) = file.write_all(bytes).and_then(|_| file.sync_all()) {
        drop(file);
        let _ = fs::remove_file(&temporary);
        return Err(error).context("could not stage manifest");
    }
    drop(file);
    let backup = if replaced {
        let path = unique_backup_path(destination)?;
        create_directory_checked(path.parent().expect("backup has a parent"))?;
        fs::rename(destination, &path).context("could not back up existing manifest")?;
        Some(path)
    } else {
        None
    };
    if let Err(error) = fs::rename(&temporary, destination) {
        if let Some(path) = &backup {
            let _ = fs::rename(path, destination);
        }
        let _ = fs::remove_file(&temporary);
        return Err(error).context("could not commit manifest");
    }
    Ok((replaced, backup))
}

fn validate_manifest_request(request: &ManifestRequest) -> Result<()> {
    if request.app_id == 0 || request.depot_id == 0 || request.manifest_id == 0 {
        bail!("manifest request identifiers must be greater than zero");
    }
    Ok(())
}

fn response_limit(response: &HttpResponse, limit: usize) -> Result<()> {
    if response.body.len() > limit {
        bail!("HTTP response exceeds the {limit} byte limit");
    }
    Ok(())
}

fn manifest_hub_status_detail(status: u16) -> &'static str {
    match status {
        400 | 404 => "manifest was not found",
        401 | 403 => "API key was rejected or expired",
        429 => "request was rate limited",
        500..=599 => "ManifestHub is temporarily unavailable",
        _ => "request returned a non-success status",
    }
}

fn browser_headers() -> Vec<(String, String)> {
    vec![
        ("Accept".into(), "*/*".into()),
        (
            "User-Agent".into(),
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) SteamToolsNext/0.2".into(),
        ),
    ]
}

fn percent_encode(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            output.push(byte as char);
        } else {
            output.push_str(&format!("%{byte:02X}"));
        }
    }
    output
}

fn valid_decimal(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 20
        && value.bytes().all(|byte| byte.is_ascii_digit())
        && value != "0"
}

fn valid_host(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 253
        && !host.starts_with('.')
        && !host.ends_with('.')
        && host
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
}

fn file_prefix_sha256(path: &Path, length: u64) -> Result<String> {
    let mut file = File::open(path)?;
    let mut bytes = vec![0u8; length.min(4096) as usize];
    let length = file.read(&mut bytes)?;
    bytes.truncate(length);
    Ok(sha256_hex(&bytes))
}

fn checked_root(path: &Path) -> Result<PathBuf> {
    let path = absolute_lexical(path)?;
    require_directory_if_present(&path, "Steam root")?;
    reject_linked_ancestors(&path)?;
    Ok(path)
}

fn create_directory_checked(path: &Path) -> Result<()> {
    reject_linked_ancestors(path)?;
    fs::create_dir_all(path)?;
    reject_linked_ancestors(path)?;
    require_directory_if_present(path, "directory")
}

fn require_directory_if_present(path: &Path, label: &str) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let metadata = fs::symlink_metadata(path)?;
    if metadata_is_link(&metadata) {
        bail!(
            "{label} is a linked or reparse-point path: {}",
            display_path(path)
        );
    }
    if !metadata.is_dir() {
        bail!("{label} is not a directory: {}", display_path(path));
    }
    Ok(())
}

fn require_regular_file(path: &Path, label: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata_is_link(&metadata) {
        bail!(
            "{label} is a linked or reparse-point file: {}",
            display_path(path)
        );
    }
    if !metadata.is_file() {
        bail!("{label} is not a regular file: {}", display_path(path));
    }
    Ok(())
}

fn reject_linked_ancestors(path: &Path) -> Result<()> {
    let mut current = Some(path);
    while let Some(candidate) = current {
        match fs::symlink_metadata(candidate) {
            Ok(metadata) if metadata_is_link(&metadata) => {
                bail!(
                    "linked or reparse-point paths are not allowed: {}",
                    display_path(candidate)
                );
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        current = candidate.parent();
    }
    Ok(())
}

fn metadata_is_link(metadata: &Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & REPARSE_POINT_ATTRIBUTE != 0
    }
    #[cfg(not(windows))]
    {
        let _ = REPARSE_POINT_ATTRIBUTE;
        false
    }
}

fn unique_sibling_path(destination: &Path, tag: &str) -> Result<PathBuf> {
    let parent = destination.parent().context("destination has no parent")?;
    let name = destination
        .file_name()
        .and_then(|value| value.to_str())
        .context("destination file name is not Unicode")?;
    unique_path(parent, &format!(".{name}.{tag}"))
}

fn unique_backup_path(destination: &Path) -> Result<PathBuf> {
    let parent = destination.parent().context("destination has no parent")?;
    let name = destination
        .file_name()
        .and_then(|value| value.to_str())
        .context("destination file name is not Unicode")?;
    unique_path(&parent.join(".stn-backups"), &format!("{name}.replaced"))
}

fn unique_path(parent: &Path, prefix: &str) -> Result<PathBuf> {
    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    for attempt in 0..100u32 {
        let path = parent.join(format!(
            "{prefix}-{}-{timestamp}-{attempt}",
            std::process::id()
        ));
        if !path.exists() {
            return Ok(path);
        }
    }
    bail!("could not allocate a unique path")
}

fn absolute_lexical(path: &Path) -> Result<PathBuf> {
    if path.as_os_str().is_empty() {
        bail!("path is empty");
    }
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut output = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !matches!(output.components().next_back(), Some(Component::Normal(_)))
                    || !output.pop()
                {
                    bail!("path escapes its filesystem root");
                }
            }
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                output.push(component.as_os_str())
            }
        }
    }
    Ok(output)
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn display_path(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeTransport {
        responses: Mutex<VecDeque<Result<HttpResponse>>>,
        requests: Mutex<Vec<HttpRequest>>,
    }

    impl FakeTransport {
        fn with(responses: Vec<Result<HttpResponse>>) -> Self {
            Self {
                responses: Mutex::new(responses.into()),
                requests: Mutex::new(Vec::new()),
            }
        }
    }

    impl ManifestHttpTransport for FakeTransport {
        fn send(&self, request: &HttpRequest) -> Result<HttpResponse> {
            self.requests.lock().unwrap().push(request.clone());
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .context("missing fake response")?
        }
    }

    fn response(status: u16, body: impl Into<Vec<u8>>) -> Result<HttpResponse> {
        Ok(HttpResponse {
            status,
            content_type: None,
            body: body.into(),
        })
    }

    fn raw_manifest() -> Vec<u8> {
        let mut bytes = MANIFEST_MAGIC.to_vec();
        bytes.extend_from_slice(b"fixture");
        bytes
    }

    #[test]
    fn parses_expected_log_shape_and_rejects_incomplete_lines() {
        let line =
            "[2026-11-05] BYldRequestDepotManifest(App: 570, Depot: 571, Manifest: 123456789)";
        assert_eq!(
            parse_manifest_log_line(line),
            Some(ManifestRequest {
                app_id: 570,
                depot_id: 571,
                manifest_id: 123456789
            })
        );
        assert!(parse_manifest_log_line("BYldRequestDepotManifest(App: 0, Depot: 2)").is_none());
    }

    #[test]
    fn poll_handles_append_duplicate_and_truncation() {
        let temp = tempfile::tempdir().unwrap();
        let steam = temp.path().join("Steam");
        fs::create_dir_all(steam.join("logs")).unwrap();
        let log = steam.join("logs/content_log.txt");
        fs::write(
            &log,
            "BYldRequestDepotManifest(App: 1, Depot: 2, Manifest: 3)\n",
        )
        .unwrap();
        let mut cursor = ManifestLogCursor::default();
        let first = poll_manifest_requests(&steam, &mut cursor).unwrap();
        assert_eq!(first.requests.len(), 1);
        fs::OpenOptions::new()
            .append(true)
            .open(&log)
            .unwrap()
            .write_all(
                b"BYldRequestDepotManifest(App: 1, Depot: 2, Manifest: 3)\nBYldRequestDepotManifest(App: 4, Depot: 5, Manifest: 6)\n",
            )
            .unwrap();
        let second = poll_manifest_requests(&steam, &mut cursor).unwrap();
        assert_eq!(second.requests.len(), 1);
        assert_eq!(second.requests[0].app_id, 4);
        fs::write(
            &log,
            "BYldRequestDepotManifest(App: 7, Depot: 8, Manifest: 9)\n",
        )
        .unwrap();
        let third = poll_manifest_requests(&steam, &mut cursor).unwrap();
        assert!(third.reset);
        assert_eq!(third.requests[0].app_id, 7);
    }

    #[test]
    fn request_code_parser_accepts_json_and_plain_text() {
        assert_eq!(
            parse_request_code(br#"{"response":{"request_code":"12345"}}"#).as_deref(),
            Some("12345")
        );
        assert_eq!(
            parse_request_code(b"code=998877").as_deref(),
            Some("998877")
        );
        assert!(parse_request_code(b"no code here").is_none());
    }

    #[test]
    fn payload_validator_rejects_html_and_accepts_raw_manifest() {
        assert_eq!(
            normalize_manifest_payload(&raw_manifest()).unwrap(),
            raw_manifest()
        );
        assert!(normalize_manifest_payload(b"<!doctype html><html>").is_err());
        assert!(normalize_manifest_payload(b"garbage").is_err());
    }

    #[test]
    fn payload_validator_extracts_single_file_zip() {
        let mut output = Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut output);
            writer
                .start_file("1.manifest", zip::write::SimpleFileOptions::default())
                .unwrap();
            writer.write_all(&raw_manifest()).unwrap();
            writer.finish().unwrap();
        }
        assert_eq!(
            normalize_manifest_payload(output.get_ref()).unwrap(),
            raw_manifest()
        );
    }

    #[test]
    fn manifest_hub_download_is_validated_and_written_atomically() {
        let temp = tempfile::tempdir().unwrap();
        let steam = temp.path().join("Steam");
        fs::create_dir_all(&steam).unwrap();
        let transport = FakeTransport::with(vec![response(200, raw_manifest())]);
        let request = ManifestRequest {
            app_id: 1,
            depot_id: 2,
            manifest_id: 3,
        };
        let receipt = fetch_and_store_manifest(
            &steam,
            &request,
            &transport,
            &ManifestFetchMode::ManifestHub {
                api_key: "key with space".into(),
            },
        )
        .unwrap();
        assert_eq!(receipt.source, "ManifestHub");
        assert!(steam.join("depotcache/2_3.manifest").is_file());
        let requests = transport.requests.lock().unwrap();
        assert!(requests[0].sensitive);
        assert!(requests[0].url.contains("key%20with%20space"));
    }

    #[test]
    fn direct_mode_uses_code_then_cdn_and_records_attempts() {
        let temp = tempfile::tempdir().unwrap();
        let steam = temp.path().join("Steam");
        fs::create_dir_all(&steam).unwrap();
        let directory = br#"{"response":{"servers":[{"vhost":"cdn.example.test","https_support":"mandatory"}]}}"#;
        let transport = FakeTransport::with(vec![
            response(200, directory.as_slice()),
            response(200, br#"{"code":"77"}"#.as_slice()),
            response(200, raw_manifest()),
        ]);
        let receipt = fetch_and_store_manifest(
            &steam,
            &ManifestRequest {
                app_id: 1,
                depot_id: 2,
                manifest_id: 3,
            },
            &transport,
            &ManifestFetchMode::CodeDirect,
        )
        .unwrap();
        assert!(receipt.source.contains("20770407"));
        assert!(receipt.source.contains("cdn.example.test"));
        assert!(receipt
            .attempts
            .iter()
            .any(|attempt| attempt.stage == "code"));
        assert!(receipt
            .attempts
            .iter()
            .any(|attempt| attempt.stage == "cdn"));
    }
}
