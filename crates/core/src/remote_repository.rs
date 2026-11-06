//! Fluent-compatible remote Lua package import.
//!
//! The remote repository publishes `{appid}.zip` packages behind a short-link
//! proxy. This module keeps the network and archive rules behind one small
//! interface: bounded retries, explicit "not indexed" errors, strict ZIP
//! limits, path traversal rejection, and writes through the transactional Lua
//! library module.

use crate::library::{create_lua_entry, ManagedWrite};
use anyhow::{bail, Context, Result};
use reqwest::blocking::{Client, Response};
use reqwest::header::{
    ACCEPT, ACCEPT_LANGUAGE, CONTENT_LENGTH, CONTENT_TYPE, ORIGIN, REFERER, USER_AGENT,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;
use std::fmt;
use std::io::{Cursor, Read};
use std::path::{Component, Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

const DEFAULT_PACKAGE_TEMPLATE: &str =
    "https://steamgames554.s3.us-east-1.amazonaws.com/{appid}.zip";
const DEFAULT_SHORT_CREATE_URL: &str = "https://short.walftech.com/api_create_link.php";
const DEFAULT_PROXY_TEMPLATE: &str = "https://short.walftech.com/proxy.php?short={code}";
const DEFAULT_MAX_ARCHIVE_BYTES: u64 = 512 * 1024 * 1024;
const DEFAULT_MAX_EXPANDED_BYTES: u64 = 1024 * 1024 * 1024;
const DEFAULT_MAX_LUA_BYTES: u64 = 8 * 1024 * 1024;
const DEFAULT_MAX_ENTRIES: usize = 10_000;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", default)]
pub struct RemoteRepositoryConfig {
    pub package_url_template: String,
    pub short_create_url: String,
    pub proxy_url_template: String,
    pub timeout_seconds: u64,
    pub max_attempts: u8,
    pub max_archive_bytes: u64,
    pub max_expanded_bytes: u64,
    pub max_lua_bytes: u64,
    pub max_entries: usize,
    pub user_agent: String,
}

impl Default for RemoteRepositoryConfig {
    fn default() -> Self {
        Self {
            package_url_template: DEFAULT_PACKAGE_TEMPLATE.into(),
            short_create_url: DEFAULT_SHORT_CREATE_URL.into(),
            proxy_url_template: DEFAULT_PROXY_TEMPLATE.into(),
            timeout_seconds: 60,
            max_attempts: 3,
            max_archive_bytes: DEFAULT_MAX_ARCHIVE_BYTES,
            max_expanded_bytes: DEFAULT_MAX_EXPANDED_BYTES,
            max_lua_bytes: DEFAULT_MAX_LUA_BYTES,
            max_entries: DEFAULT_MAX_ENTRIES,
            user_agent: format!("SteamTools-Next/{}", env!("CARGO_PKG_VERSION")),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RemoteImportRequest {
    pub steam_root: PathBuf,
    pub app_id: u32,
    #[serde(default)]
    pub overwrite: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RemoteLuaDocument {
    pub file_name: String,
    pub app_id: u32,
    pub size_bytes: u64,
    pub lua: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RemoteImportIssue {
    pub file_name: String,
    pub reason: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RemoteImportResult {
    pub app_id: u32,
    pub imported: Vec<ManagedWrite>,
    pub skipped: Vec<RemoteImportIssue>,
    pub archive_bytes: u64,
    pub lua_files: usize,
    pub attempts: usize,
    pub steps: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RemotePackageFetch {
    pub app_id: u32,
    pub archive: Vec<u8>,
    pub attempts: usize,
    pub steps: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteRepositoryError {
    NotIndexed { status: u16 },
    InvalidResponse { detail: String },
}

impl RemoteRepositoryError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotIndexed { .. } => "not_indexed",
            Self::InvalidResponse { .. } => "invalid_response",
        }
    }
}

impl fmt::Display for RemoteRepositoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotIndexed { status } => {
                write!(
                    formatter,
                    "remote repository has no package for this AppID (HTTP {status})"
                )
            }
            Self::InvalidResponse { detail } => formatter.write_str(detail),
        }
    }
}

impl std::error::Error for RemoteRepositoryError {}

pub fn remote_repository_error_code(error: &anyhow::Error) -> Option<&'static str> {
    error
        .downcast_ref::<RemoteRepositoryError>()
        .map(RemoteRepositoryError::code)
}

pub struct RemoteRepositoryClient {
    config: RemoteRepositoryConfig,
    http: Client,
}

impl RemoteRepositoryClient {
    pub fn new(config: RemoteRepositoryConfig) -> Result<Self> {
        validate_config(&config)?;
        let http = crate::catalog::shared_catalog_http_client(config.timeout_seconds)
            .context("could not create the remote repository HTTP client")?;
        Ok(Self { config, http })
    }

    pub fn config(&self) -> &RemoteRepositoryConfig {
        &self.config
    }

    pub fn fetch_package(&self, app_id: u32) -> Result<RemotePackageFetch> {
        validate_app_id(app_id)?;
        let package_url = self
            .config
            .package_url_template
            .replace("{appid}", &app_id.to_string());
        let deadline = Instant::now() + Duration::from_secs(self.config.timeout_seconds);
        let (short_code, short_attempts) = self.request_short_code(&package_url, deadline)?;
        let (archive, package_attempts) = self.download_package(&short_code, deadline)?;
        Ok(RemotePackageFetch {
            app_id,
            archive,
            attempts: short_attempts + package_attempts,
            steps: vec![
                format!("resolved remote package for AppID {app_id}"),
                "downloaded remote Lua archive".into(),
            ],
        })
    }

    pub fn import_package(&self, request: &RemoteImportRequest) -> Result<RemoteImportResult> {
        validate_app_id(request.app_id)?;
        let fetch = self.fetch_package(request.app_id)?;
        let documents = extract_remote_lua_documents_with_config(&fetch.archive, &self.config)?;
        if !documents
            .iter()
            .any(|document| document.app_id == request.app_id)
        {
            bail!(
                "remote package does not contain the requested AppID {}",
                request.app_id
            );
        }
        let mut imported = Vec::new();
        let mut skipped = Vec::new();
        for document in &documents {
            match create_lua_entry(
                &request.steam_root,
                document.app_id,
                Some(&document.lua),
                request.overwrite,
            ) {
                Ok(write) => imported.push(write),
                Err(error) => skipped.push(RemoteImportIssue {
                    file_name: document.file_name.clone(),
                    reason: format!("{error:#}"),
                }),
            }
        }
        if imported.is_empty() {
            let detail = skipped
                .iter()
                .map(|issue| format!("{}: {}", issue.file_name, issue.reason))
                .collect::<Vec<_>>()
                .join("; ");
            bail!("remote package contained no importable Lua file: {detail}");
        }
        let mut steps = fetch.steps;
        steps.push(format!(
            "imported {} of {} Lua files",
            imported.len(),
            documents.len()
        ));
        Ok(RemoteImportResult {
            app_id: request.app_id,
            imported,
            skipped,
            archive_bytes: fetch.archive.len() as u64,
            lua_files: documents.len(),
            attempts: fetch.attempts,
            steps,
        })
    }

    fn request_short_code(&self, package_url: &str, deadline: Instant) -> Result<(String, usize)> {
        let body = format!("url={}", percent_encode(package_url));
        let mut last_error = None;
        for attempt in 1..=usize::from(self.config.max_attempts) {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .context("remote import time budget reached during short-link resolution")?;
            let response = self
                .http
                .post(&self.config.short_create_url)
                .timeout(remaining.min(Duration::from_secs(8)))
                .header(USER_AGENT, &self.config.user_agent)
                .header(ACCEPT, "application/json,text/plain,*/*")
                .header(ACCEPT_LANGUAGE, "zh-CN,zh;q=0.9,en;q=0.7")
                .header(ORIGIN, "https://remlua.com")
                .header(REFERER, "https://remlua.com/")
                .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(body.clone())
                .send();
            match response.and_then(|response| response.error_for_status()) {
                Ok(response) => {
                    let bytes = read_limited_response(response, 1024 * 1024)?;
                    return Ok((parse_short_code(&bytes)?, attempt));
                }
                Err(error) => last_error = Some(error),
            }
            retry_delay_until(attempt, usize::from(self.config.max_attempts), deadline);
        }
        let error = last_error
            .map(anyhow::Error::from)
            .unwrap_or_else(|| anyhow::anyhow!("short-link service returned no response"));
        Err(error.context("could not request a remote package short code"))
    }

    fn download_package(&self, short_code: &str, deadline: Instant) -> Result<(Vec<u8>, usize)> {
        let url = self
            .config
            .proxy_url_template
            .replace("{code}", &percent_encode(short_code));
        let mut last_error = None;
        for attempt in 1..=usize::from(self.config.max_attempts) {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .context("remote import time budget reached during package download")?;
            match self
                .http
                .get(&url)
                .timeout(remaining)
                .header(USER_AGENT, &self.config.user_agent)
                .header(ACCEPT, "application/zip,application/octet-stream,*/*")
                .header(ACCEPT_LANGUAGE, "zh-CN,zh;q=0.9,en;q=0.7")
                .header(
                    REFERER,
                    format!("https://short.walftech.com/?id={short_code}"),
                )
                .send()
            {
                Ok(response) if missing_status(response.status().as_u16()) => {
                    return Err(RemoteRepositoryError::NotIndexed {
                        status: response.status().as_u16(),
                    }
                    .into());
                }
                Ok(response) if response.status().is_success() => {
                    let bytes = read_limited_response(response, self.config.max_archive_bytes)?;
                    if !bytes.starts_with(b"PK\x03\x04") && !bytes.starts_with(b"PK\x05\x06") {
                        return Err(RemoteRepositoryError::InvalidResponse {
                            detail: "remote repository returned a non-ZIP payload".into(),
                        }
                        .into());
                    }
                    return Ok((bytes, attempt));
                }
                Ok(response) => {
                    last_error = Some(anyhow::anyhow!(
                        "remote package download returned HTTP {}",
                        response.status().as_u16()
                    ));
                }
                Err(error) => last_error = Some(error.into()),
            }
            retry_delay_until(attempt, usize::from(self.config.max_attempts), deadline);
        }
        Err(last_error
            .context("remote package download returned no response")
            .context("remote package download failed")?)
    }
}

pub fn extract_remote_lua_documents(bytes: &[u8]) -> Result<Vec<RemoteLuaDocument>> {
    extract_remote_lua_documents_with_config(bytes, &RemoteRepositoryConfig::default())
}

fn extract_remote_lua_documents_with_config(
    bytes: &[u8],
    config: &RemoteRepositoryConfig,
) -> Result<Vec<RemoteLuaDocument>> {
    if bytes.is_empty() {
        bail!("remote package is empty");
    }
    if bytes.len() as u64 > config.max_archive_bytes {
        bail!("remote package exceeds the archive byte limit");
    }
    let mut archive =
        zip::ZipArchive::new(Cursor::new(bytes)).context("remote package ZIP is invalid")?;
    if archive.len() > config.max_entries {
        bail!(
            "remote package contains more than {} entries",
            config.max_entries
        );
    }
    let mut output = Vec::new();
    let mut seen_names = BTreeSet::new();
    let mut expanded_bytes = 0u64;
    for index in 0..archive.len() {
        let entry = archive.by_index(index)?;
        let Some(enclosed) = entry.enclosed_name() else {
            bail!("remote package contains a path traversal entry");
        };
        if entry.is_dir() {
            continue;
        }
        if entry
            .unix_mode()
            .is_some_and(|mode| mode & 0o170000 == 0o120000)
        {
            bail!("remote package contains a symbolic-link entry");
        }
        if !extension_is(&enclosed, "lua") {
            continue;
        }
        if !safe_archive_path(&enclosed) {
            bail!("remote package contains an unsafe Lua path");
        }
        let file_name = enclosed
            .file_name()
            .and_then(|value| value.to_str())
            .context("remote Lua file name is not Unicode")?
            .to_string();
        if !seen_names.insert(file_name.to_ascii_lowercase()) {
            continue;
        }
        let app_id = enclosed
            .file_stem()
            .and_then(|value| value.to_str())
            .and_then(|value| value.parse::<u32>().ok())
            .filter(|value| *value != 0)
            .with_context(|| {
                format!("remote Lua file must use a numeric AppID name: {file_name}")
            })?;
        if entry.size() > config.max_lua_bytes {
            bail!("remote Lua file exceeds the per-file byte limit: {file_name}");
        }
        expanded_bytes = expanded_bytes
            .checked_add(entry.size())
            .context("remote package expanded size overflow")?;
        if expanded_bytes > config.max_expanded_bytes {
            bail!("remote package exceeds the expanded byte limit");
        }
        let mut content = Vec::with_capacity(entry.size() as usize);
        entry
            .take(config.max_lua_bytes + 1)
            .read_to_end(&mut content)?;
        if content.len() as u64 > config.max_lua_bytes {
            bail!("remote Lua file expanded beyond the byte limit: {file_name}");
        }
        let lua = String::from_utf8(content)
            .with_context(|| format!("remote Lua file is not UTF-8: {file_name}"))?;
        output.push(RemoteLuaDocument {
            file_name,
            app_id,
            size_bytes: lua.len() as u64,
            lua,
        });
    }
    if output.is_empty() {
        bail!("remote package contains no Lua files");
    }
    output.sort_by_key(|document| document.app_id);
    Ok(output)
}

fn parse_short_code(bytes: &[u8]) -> Result<String> {
    let root: Value =
        serde_json::from_slice(bytes).map_err(|error| RemoteRepositoryError::InvalidResponse {
            detail: format!("short-link service returned invalid JSON: {error}"),
        })?;
    let code = root
        .get("short_code")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| RemoteRepositoryError::InvalidResponse {
            detail: "short-link service response has no short_code".into(),
        })?;
    if code.len() > 512
        || code
            .chars()
            .any(|value| matches!(value, '\r' | '\n' | '\0'))
    {
        return Err(RemoteRepositoryError::InvalidResponse {
            detail: "short-link service returned an invalid short_code".into(),
        }
        .into());
    }
    Ok(code.to_string())
}

fn read_limited_response(response: Response, limit: u64) -> Result<Vec<u8>> {
    if let Some(length) = response
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
    {
        if length > limit {
            bail!("remote response exceeds the {limit} byte limit");
        }
    }
    let mut bytes = Vec::new();
    response.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        bail!("remote response grew beyond the {limit} byte limit");
    }
    Ok(bytes)
}

fn retry_delay_until(attempt: usize, max_attempts: usize, deadline: Instant) {
    if attempt < max_attempts {
        if let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
            thread::sleep(Duration::from_secs(1u64 << attempt.min(5)).min(remaining));
        }
    }
}

fn missing_status(status: u16) -> bool {
    matches!(status, 403 | 404 | 502 | 503 | 504)
}

fn validate_config(config: &RemoteRepositoryConfig) -> Result<()> {
    for (value, placeholder, label) in [
        (
            &config.package_url_template,
            "{appid}",
            "package URL template",
        ),
        (&config.proxy_url_template, "{code}", "proxy URL template"),
    ] {
        validate_http_url(value, label)?;
        if !value.contains(placeholder) {
            bail!("{label} must contain {placeholder}");
        }
    }
    validate_http_url(&config.short_create_url, "short-link URL")?;
    if config.timeout_seconds == 0 || config.timeout_seconds > 300 {
        bail!("remote timeout must be between 1 and 300 seconds");
    }
    if config.max_attempts == 0 || config.max_attempts > 8 {
        bail!("remote attempts must be between 1 and 8");
    }
    if config.max_archive_bytes == 0
        || config.max_expanded_bytes == 0
        || config.max_lua_bytes == 0
        || config.max_entries == 0
    {
        bail!("remote package limits must be greater than zero");
    }
    if config.max_lua_bytes > config.max_expanded_bytes {
        bail!("per-file Lua limit cannot exceed the expanded package limit");
    }
    if config.user_agent.trim().is_empty() || config.user_agent.contains(['\r', '\n', '\0']) {
        bail!("remote user-agent is invalid");
    }
    Ok(())
}

fn validate_http_url(value: &str, label: &str) -> Result<()> {
    let trimmed = value.trim();
    let lower = trimmed.to_ascii_lowercase();
    if (!lower.starts_with("https://") && !lower.starts_with("http://"))
        || trimmed.len() > 4096
        || trimmed.contains(['\r', '\n', '\0'])
    {
        bail!("{label} must be a valid HTTP(S) URL");
    }
    Ok(())
}

fn validate_app_id(app_id: u32) -> Result<()> {
    if app_id == 0 {
        bail!("AppID must be greater than zero");
    }
    Ok(())
}

fn percent_encode(value: &str) -> String {
    value
        .as_bytes()
        .iter()
        .flat_map(|byte| {
            if byte.is_ascii_alphanumeric() || matches!(*byte, b'-' | b'_' | b'.' | b'~') {
                vec![char::from(*byte)]
            } else {
                format!("%{byte:02X}").chars().collect()
            }
        })
        .collect()
}

fn extension_is(path: &Path, expected: &str) -> bool {
    path.extension()
        .and_then(|value| value.to_str())
        .is_some_and(|value| value.eq_ignore_ascii_case(expected))
}

fn safe_archive_path(path: &Path) -> bool {
    !path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_) | Component::CurDir))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn stalled_remote_import_has_one_total_deadline_not_a_deadline_per_retry() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (socket, _) = listener.accept().unwrap();
            std::thread::sleep(Duration::from_millis(1500));
            drop(socket);
        });
        let client = RemoteRepositoryClient::new(RemoteRepositoryConfig {
            short_create_url: format!("http://{address}/short"),
            timeout_seconds: 1,
            max_attempts: 3,
            ..Default::default()
        })
        .unwrap();
        let started = Instant::now();
        assert!(client.fetch_package(42).is_err());
        assert!(started.elapsed() < Duration::from_secs(2));
        server.join().unwrap();
    }

    fn make_zip(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut output = Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut output);
            for (name, content) in files {
                writer
                    .start_file(*name, zip::write::SimpleFileOptions::default())
                    .unwrap();
                writer.write_all(content).unwrap();
            }
            writer.finish().unwrap();
        }
        output.into_inner()
    }

    #[test]
    fn parses_short_code_and_reports_invalid_shape() {
        assert_eq!(
            parse_short_code(br#"{"short_code":"abc-123"}"#).unwrap(),
            "abc-123"
        );
        let error = parse_short_code(br#"{"other":true}"#).unwrap_err();
        assert_eq!(
            remote_repository_error_code(&error),
            Some("invalid_response")
        );
    }

    #[test]
    fn missing_statuses_are_classified_without_retry() {
        for status in [403, 404, 502, 503, 504] {
            assert!(missing_status(status));
        }
        for status in [200, 400, 401, 429, 500] {
            assert!(!missing_status(status));
        }
        let error = anyhow::Error::new(RemoteRepositoryError::NotIndexed { status: 404 });
        assert_eq!(remote_repository_error_code(&error), Some("not_indexed"));
    }

    #[test]
    fn extracts_numeric_lua_files_and_flattens_directories() {
        let bytes = make_zip(&[
            ("nested/42.lua", b"addappid(42)\n"),
            ("7.lua", b"addappid(7)\n"),
            ("ignored.txt", b"ignored"),
        ]);
        let files = extract_remote_lua_documents(&bytes).unwrap();
        assert_eq!(
            files.iter().map(|file| file.app_id).collect::<Vec<_>>(),
            vec![7, 42]
        );
        assert_eq!(files[1].file_name, "42.lua");
    }

    #[test]
    fn rejects_non_numeric_and_traversal_lua_names() {
        let non_numeric = make_zip(&[("game.lua", b"addappid(42)\n")]);
        assert!(extract_remote_lua_documents(&non_numeric).is_err());
        let traversal = make_zip(&[("../42.lua", b"addappid(42)\n")]);
        assert!(extract_remote_lua_documents(&traversal).is_err());
    }

    #[test]
    fn enforces_archive_and_expansion_limits() {
        let bytes = make_zip(&[("42.lua", b"addappid(42)\n")]);
        let config = RemoteRepositoryConfig {
            max_archive_bytes: 1,
            ..Default::default()
        };
        assert!(extract_remote_lua_documents_with_config(&bytes, &config).is_err());
        let config = RemoteRepositoryConfig {
            max_lua_bytes: 4,
            ..Default::default()
        };
        assert!(extract_remote_lua_documents_with_config(&bytes, &config).is_err());
    }

    #[test]
    fn validates_endpoint_templates_and_limits() {
        let config = RemoteRepositoryConfig::default();
        validate_config(&config).unwrap();
        let config = RemoteRepositoryConfig {
            proxy_url_template: "https://example.invalid/no-placeholder".into(),
            ..Default::default()
        };
        assert!(validate_config(&config).is_err());
        let config = RemoteRepositoryConfig {
            max_attempts: 0,
            ..Default::default()
        };
        assert!(validate_config(&config).is_err());
    }
}
