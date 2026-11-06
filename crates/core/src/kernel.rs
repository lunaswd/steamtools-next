//! Read-only OpenSteamTool source/configuration adapter.
//!
//! This module never loads a DLL, executes Lua, reads account databases or writes
//! inside Steam. A metadata match is deliberately not a runtime compatibility claim.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, Metadata};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};

const MAX_TEXT_BYTES: u64 = 4 * 1024 * 1024;
const MAX_SOURCE_FILE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_BINARY_BYTES: u64 = 512 * 1024 * 1024;
const MAX_TREE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_TREE_FILES: usize = 10_000;
const MAX_LUA_FILES: usize = 2_000;
const MAX_DEPTH: usize = 16;
const CONFIG_SECTIONS: &[&str] = &[
    "log", "manifest", "stats", "presence", "lua", "inject", "cloud", "remote",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KernelReport {
    pub path: String,
    pub kind: String,
    pub version: Option<String>,
    pub files: Vec<KernelFile>,
    pub config: Option<ConfigSummary>,
    pub diagnostics: Vec<Diagnostic>,
    pub lua_files: Vec<LuaSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KernelFile {
    pub name: String,
    pub size_bytes: u64,
    pub sha256: String,
}

/// Read-only inventory of the OpenSteamTool files in a selected Steam root.
///
/// `status` is one of `notInstalled`, `enabled`, `disabled`, `conflict` or
/// `incomplete`. `injection_method` is one of `none`, `dwmapi`, `version` or
/// `both`. These strings are intentionally stable for GUI report consumers.
/// No file is loaded, executed, renamed or removed while producing this value.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KernelInstallation {
    pub path: String,
    pub status: String,
    pub injection_method: String,
    pub version: Option<String>,
    /// Evidence source for `version`: `peResource`, `artifactManifest`, or `unknown`.
    #[serde(default)]
    pub version_source: String,
    /// SHA-256 of the installed OpenSteamTool primary DLL, when readable.
    #[serde(default)]
    pub installed_sha256: Option<String>,
    /// Version declared by the selected artifact manifest, when supplied.
    #[serde(default)]
    pub artifact_version: Option<String>,
    /// SHA-256 declared by the selected artifact manifest, when supplied.
    #[serde(default)]
    pub artifact_sha256: Option<String>,
    /// Whether installed and selected artifact primary DLL hashes match.
    #[serde(default)]
    pub hash_match: Option<bool>,
    /// Reserved for a future live process/module probe. Always false for disk scans.
    #[serde(default)]
    pub runtime_load_verified: bool,
    pub files: Vec<KernelFile>,
    pub remnants: Vec<String>,
    pub message_key: String,
    #[serde(default)]
    pub message_args: BTreeMap<String, String>,
    pub diagnostics: Vec<Diagnostic>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigSummary {
    pub path: String,
    pub valid: bool,
    pub sections: Vec<String>,
    pub unknown_sections: Vec<String>,
    pub error: Option<String>,
    #[serde(default)]
    pub error_key: String,
    #[serde(default)]
    pub error_args: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Diagnostic {
    pub code: String,
    pub severity: String,
    pub title: String,
    pub detail: String,
    #[serde(default)]
    pub message_key: String,
    #[serde(default)]
    pub message_args: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LuaSummary {
    pub name: String,
    pub size_bytes: u64,
    pub app_ids: Vec<u32>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KernelComparison {
    pub left: KernelReport,
    pub right: KernelReport,
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub changed: Vec<String>,
    pub identical: usize,
}

fn message_template(key: &str) -> &'static str {
    match key {
        "diagnostic.detail.unavailable" => "Detailed localized information is unavailable for this older or unsupported diagnostic. The original technical detail remains available separately.",
        "diagnostic.reason.unavailable" => "The detailed error reason is unavailable.",
        "diagnostic.file_unreadable.body" => "{filename}: {reason}",
        "diagnostic.binary_architecture.body" => "{filename}: {architecture}. Only the on-disk PE header was read.",
        "diagnostic.binary_architecture.unknown" => "{filename}: the PE machine type is not recognized. Only the on-disk PE header was read.",
        "diagnostic.binary_header_invalid.body" => "{filename}: {reason}",
        "diagnostic.binary_version.body" => "{filename}: {version} from the file-version resource. This is not an authenticated release or load-state check.",
        "diagnostic.config_unreadable.body" => "{filename}: {reason}",
        "diagnostic.config_invalid.duplicate" => "Duplicate TOML key or table at line {line}, column {column}; source values omitted.",
        "diagnostic.config_invalid.duplicate_unlocated" => "Duplicate TOML key or table; source location unavailable and values omitted.",
        "diagnostic.config_invalid.syntax" => "Invalid TOML syntax at line {line}, column {column}; source values omitted.",
        "diagnostic.config_invalid.syntax_unlocated" => "Invalid TOML syntax; source location unavailable and values omitted.",
        "diagnostic.config_root_value.body" => "OpenSteamTool settings normally belong to named tables. The unexpected root-level value is omitted.",
        "diagnostic.config_valid.body" => "Configuration was parsed without executing code. Syntax validity is not runtime validation.",
        "diagnostic.config_unknown_sections.body" => "{count} section(s) are outside the known source schema; values are omitted.",
        "diagnostic.config_type_mismatch.body" => "{field} must have type {expected}; its value is omitted.",
        "diagnostic.config_timeout_range.body" => "{field} should be an integer within {minimum}–{maximum} ms; its value is omitted.",
        "diagnostic.config_template_placeholders.body" => "{field} requires {required}; missing placeholders: {missing}. The configured URL is omitted.",
        "diagnostic.config_template_transport.body" => "The configured metadata URL template is not HTTPS. No network request was made.",
        "diagnostic.config_lua_paths_type.body" => "lua.paths should be an array of strings. External configured directories are not followed by this inspector.",
        "diagnostic.config_enum_value.body" => "{field} should be one of {expected}; the configured value is omitted.",
        "diagnostic.installed_kernel_detected.body" => "{path}: a regular OpenSteamTool.dll exists in the selected Steam root. Presence does not prove authenticity or that Steam loaded it. Source-tree versions were not used.",
        "diagnostic.installed_version_unknown.body" => "{filename} has no readable file-version resource. No version was inferred from source files, example configuration, filenames or the selected maintenance baseline.",
        "diagnostic.kernel_installation.not_installed.body" => "{path}: no OpenSteamTool core body was found. Proxy entry files and legacy bin remnants alone are not treated as an installation.",
        "diagnostic.kernel_installation.enabled.body" => "{path}: the OpenSteamTool core body and one active {injectionMethod} injection entry are present. This is an on-disk state only; no DLL was loaded.",
        "diagnostic.kernel_installation.disabled.body" => "{path}: the OpenSteamTool core body and its selected injection entry are present only as .off files. This is an on-disk state only.",
        "diagnostic.kernel_installation.conflict.body" => "{path}: dwmapi.dll and version.dll are both active beside a complete OpenSteamTool core body. The files were not loaded or changed.",
        "diagnostic.kernel_installation.incomplete.body" => "{path}: some OpenSteamTool files are present, but the core body and injection-entry layout is incomplete or mixed. The files were not changed.",
        "diagnostic.kernel_installation.remnants.body" => "{count} recognized inactive, stale or legacy file(s) were inventoried separately: {files}.",
        "diagnostic.disk_state_scan.body" => "Installation status was determined from files on disk; runtime module loading is reported separately.",
        "diagnostic.kernel_missing.body" => "{path} does not exist. Select a source tree, installation directory, TOML or Lua file.",
        "diagnostic.binary_identity_unverified.body" => "A filename and content hash do not prove a DLL’s origin, version, signature or compatibility. It was not loaded.",
        "diagnostic.steam_path_unset.body" => "Select a Steam installation to inspect its client/UI hashes and matching local metadata.",
        "diagnostic.runtime_unverified.body" => "Version detection, file fingerprints and matching metadata are separate from runtime validation. No injection or game-level test was performed.",
        "diagnostic.source_version.body" => "{filename} declares OpenSteamTool {version}. This is a source declaration, not a verified release tag.",
        "diagnostic.license_detected.gpl3" => "GNU GPL version 3 text detected. Check rights-holder notices before redistribution; this does not distinguish only/or-later grants.",
        "diagnostic.license_detected.unknown" => "A license file exists; no supported license identity was inferred automatically.",
        "diagnostic.example_configuration.body" => "{filename} is a source example, not evidence that Steam is using this configuration.",
        "diagnostic.binary_present.body" => "{filename} exists. Presence is not proof of origin, load state or compatibility; it was not loaded.",
        "diagnostic.metadata_source.fork" => "Source references {repository} with the fork metadata layout. No upstream network request was made.",
        "diagnostic.metadata_source.original" => "Source references {repository} with the original metadata layout. No upstream network request was made.",
        "diagnostic.metadata_source.unknown" => "No recognized metadata upstream was inferred from this source file.",
        "diagnostic.lua_unreadable.body" => "{filename}: {reason}",
        "diagnostic.kernel_identity_unknown.body" => "No OpenSteamTool source version or named primary DLL was found. Configuration files alone do not prove installation.",
        "diagnostic.linked_path_skipped.body" => "{path}: a symbolic link or reparse point was not followed.",
        "diagnostic.steam_fingerprint.body" => "{filename}: SHA-256 {sha256} was acquired from the file on disk, not from a loaded process.",
        "diagnostic.steam_fingerprint_unavailable.body" => "{filename}: {reason}",
        "diagnostic.metadata_missing.body" => "{channel}/{component}: no local TOML for SHA-256 {sha256} was found at {path}. No remote check was made.",
        "diagnostic.metadata_unreadable.body" => "{channel}/{component} at {path}: {reason}",
        "diagnostic.metadata_invalid.body" => "{channel}/{component} at {path}: {reason}",
        "diagnostic.metadata_match.body" => "{channel}/{component}: the local filename matches SHA-256 {sha256} and minimum table checks passed. Completeness, authenticity and runtime behavior remain unverified.",
        "diagnostic.metadata_structure_invalid.ipc" => "{channel}/{component}: required IPC interface entries are absent or invalid: {missing}. Each required interface needs at least one valid funcHash method. No metadata was modified.",
        "diagnostic.metadata_structure_invalid.pattern" => "{channel}/{component}: no valid hexadecimal function table with a nonzero rva or valid sig was found. No metadata was modified.",
        "diagnostic.comparison_scope.body" => "Compared {count} regular source/document/asset files ({bytes} bytes). Build outputs, account directories and {skippedLinks} linked paths were excluded.",
        "diagnostic.reason.not_regular" => "A regular file was expected.",
        "diagnostic.reason.linked_path" => "Linked files, symbolic links and reparse-point paths are not inspected.",
        "diagnostic.reason.text_size_limit" => "Text file exceeds the {limitBytes}-byte inspection limit.",
        "diagnostic.reason.text_grew_limit" => "Text file grew beyond the {limitBytes}-byte inspection limit.",
        "diagnostic.reason.text_invalid_utf8" => "Text file is not valid UTF-8.",
        "diagnostic.reason.file_size_limit" => "File exceeds the {limitBytes}-byte fingerprint limit.",
        "diagnostic.reason.file_grew_limit" => "File grew beyond the {limitBytes}-byte fingerprint limit.",
        "diagnostic.reason.file_disappeared" => "File disappeared while being fingerprinted.",
        "diagnostic.reason.file_changed" => "File changed while being fingerprinted; refresh the inspection.",
        "diagnostic.reason.io_not_found" => "File or directory was not found (OS error {osCode}).",
        "diagnostic.reason.io_permission_denied" => "Permission to read the file or directory was denied (OS error {osCode}).",
        "diagnostic.reason.io_invalid_data" => "File data is invalid for the requested operation.",
        "diagnostic.reason.io_unexpected_eof" => "File ended before the required data was read.",
        "diagnostic.reason.io_os_error" => "File I/O failed with OS error {osCode}.",
        "diagnostic.reason.io_error" => "File I/O failed; the system provided no numeric error code.",
        "diagnostic.reason.pe_header_incomplete" => "File has no complete DOS/PE header.",
        "diagnostic.reason.pe_dos_signature" => "File does not have a PE/DOS signature.",
        "diagnostic.reason.pe_header_offset" => "PE header offset is outside the bounded range {minimum}–{maximum} bytes.",
        "diagnostic.reason.pe_header_truncated" => "PE header is truncated.",
        "diagnostic.reason.pe_signature_invalid" => "File does not have a valid PE signature.",
        "diagnostic.reason.toml_duplicate" => "Duplicate TOML key or table at line {line}, column {column}; source values omitted.",
        "diagnostic.reason.toml_duplicate_unlocated" => "Duplicate TOML key or table; source location unavailable and values omitted.",
        "diagnostic.reason.toml_syntax" => "Invalid TOML syntax at line {line}, column {column}; source values omitted.",
        "diagnostic.reason.toml_syntax_unlocated" => "Invalid TOML syntax; source location unavailable and values omitted.",
        _ => "Detailed localized information is unavailable.",
    }
}

fn render_english_message(key: &str, arguments: &BTreeMap<String, String>) -> String {
    let mut arguments = arguments.clone();
    if let Some(reason_key) = arguments.get("reasonKey") {
        let reason = interpolate_message(message_template(reason_key), &arguments);
        arguments.insert("reason".into(), reason);
    }
    interpolate_message(message_template(key), &arguments)
}

fn interpolate_message(template: &str, arguments: &BTreeMap<String, String>) -> String {
    // Single-pass interpolation: an argument containing braces is never recursively
    // interpreted as another placeholder. No HTML is created here.
    let mut output = String::new();
    let mut remaining = template;
    while let Some(start) = remaining.find('{') {
        output.push_str(&remaining[..start]);
        let Some(end) = remaining[start + 1..].find('}') else {
            output.push_str(&remaining[start..]);
            return output;
        };
        let end = start + 1 + end;
        let token = &remaining[start + 1..end];
        if let Some(value) = arguments.get(token) {
            output.push_str(value);
        } else {
            output.push_str(&remaining[start..=end]);
        }
        remaining = &remaining[end + 1..];
    }
    output.push_str(remaining);
    output
}

fn message_diagnostic<const N: usize>(
    code: &str,
    severity: &str,
    title: &str,
    message_key: &str,
    arguments: [(&str, String); N],
) -> Diagnostic {
    diagnostic_from_args(
        code,
        severity,
        title,
        message_key,
        arguments
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect(),
    )
}

fn diagnostic_from_args(
    code: &str,
    severity: &str,
    title: &str,
    message_key: &str,
    message_args: BTreeMap<String, String>,
) -> Diagnostic {
    Diagnostic {
        code: code.into(),
        severity: severity.into(),
        title: title.into(),
        detail: render_english_message(message_key, &message_args),
        message_key: message_key.into(),
        message_args,
    }
}

#[derive(Debug, Clone)]
struct InspectionIssue {
    key: &'static str,
    arguments: BTreeMap<String, String>,
}

impl std::fmt::Display for InspectionIssue {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&render_english_message(self.key, &self.arguments))
    }
}
impl std::error::Error for InspectionIssue {}

fn issue<const N: usize>(key: &'static str, arguments: [(&str, String); N]) -> InspectionIssue {
    InspectionIssue {
        key,
        arguments: arguments
            .into_iter()
            .map(|(key, value)| (key.into(), value))
            .collect(),
    }
}

fn issue_for_error(error: &anyhow::Error) -> InspectionIssue {
    if let Some(issue) = error.downcast_ref::<InspectionIssue>() {
        return issue.clone();
    }
    if let Some(error) = error.downcast_ref::<std::io::Error>() {
        let key = match error.kind() {
            std::io::ErrorKind::NotFound => "diagnostic.reason.io_not_found",
            std::io::ErrorKind::PermissionDenied => "diagnostic.reason.io_permission_denied",
            std::io::ErrorKind::InvalidData => "diagnostic.reason.io_invalid_data",
            std::io::ErrorKind::UnexpectedEof => "diagnostic.reason.io_unexpected_eof",
            _ if error.raw_os_error().is_some() => "diagnostic.reason.io_os_error",
            _ => "diagnostic.reason.io_error",
        };
        return issue(
            key,
            [(
                "osCode",
                error
                    .raw_os_error()
                    .map(|code| code.to_string())
                    .unwrap_or_else(|| "—".into()),
            )],
        );
    }
    issue("diagnostic.reason.unavailable", [])
}

fn issue_diagnostic<const N: usize>(
    code: &str,
    severity: &str,
    title: &str,
    message_key: &str,
    reason: &InspectionIssue,
    arguments: [(&str, String); N],
) -> Diagnostic {
    let mut args = reason.arguments.clone();
    args.extend(
        arguments
            .into_iter()
            .map(|(key, value)| (key.into(), value)),
    );
    args.insert("reasonKey".into(), reason.key.into());
    diagnostic_from_args(code, severity, title, message_key, args)
}

fn display_path(path: &Path) -> String {
    let value = path.to_string_lossy();
    value.strip_prefix(r"\\?\").unwrap_or(&value).to_owned()
}

fn absolute_path(path: &Path) -> Result<PathBuf> {
    if path.as_os_str().is_empty() {
        bail!("A kernel source, installation, TOML or Lua path is required");
    }
    if forbidden_path_namespace(path) {
        bail!("Network shares and device namespace paths are outside local inspection scope");
    }
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        bail!("Parent-directory traversal is not accepted; provide the absolute target path");
    }
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    if path.components().any(|c| {
        let s = c.as_os_str().to_string_lossy().to_ascii_lowercase();
        matches!(
            s.as_str(),
            "userdata" | "loginusers.vdf" | "localconfig.vdf" | "ssfn"
        ) || s.starts_with("ssfn")
    }) {
        bail!("Account and credential locations are outside the inspection scope");
    }
    reject_link_ancestors(&path)?;
    Ok(path)
}

fn forbidden_path_namespace(path: &Path) -> bool {
    // canonicalize() on Windows emits \\?\C:\... for ordinary local paths.
    // Accept only that specific extended namespace, never arbitrary \\?\
    // prefixes such as UNC or GLOBALROOT. Inspect the prefix before any stat.
    #[cfg(windows)]
    if let Some(Component::Prefix(prefix)) = path.components().next() {
        return !matches!(
            prefix.kind(),
            std::path::Prefix::Disk(_) | std::path::Prefix::VerbatimDisk(_)
        );
    }
    let raw = path.to_string_lossy();
    raw.starts_with(r"\\") || raw.starts_with("//") || raw.starts_with(r"\??\")
}

fn is_link(meta: &Metadata) -> bool {
    if meta.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        // Includes junctions, mount points and other reparse-point aliases.
        if meta.file_attributes() & 0x400 != 0 {
            return true;
        }
    }
    false
}

fn reject_link_ancestors(path: &Path) -> Result<()> {
    for ancestor in path.ancestors() {
        match fs::symlink_metadata(ancestor) {
            Ok(meta) if is_link(&meta) => bail!(issue("diagnostic.reason.linked_path", [])),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).context("Could not inspect path metadata"),
        }
    }
    Ok(())
}

fn regular_file(path: &Path) -> Result<Option<Metadata>> {
    reject_link_ancestors(path)?;
    match fs::symlink_metadata(path) {
        Ok(meta) if is_link(&meta) => bail!(issue("diagnostic.reason.linked_path", [])),
        Ok(meta) if meta.is_file() => Ok(Some(meta)),
        Ok(_) => Ok(None),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).context("Could not inspect file metadata"),
    }
}

fn read_text(path: &Path) -> Result<String> {
    let meta = regular_file(path)?.context(issue("diagnostic.reason.not_regular", []))?;
    if meta.len() > MAX_TEXT_BYTES {
        bail!(issue(
            "diagnostic.reason.text_size_limit",
            [("limitBytes", MAX_TEXT_BYTES.to_string())]
        ));
    }
    let mut text = String::new();
    File::open(path)?
        .take(MAX_TEXT_BYTES + 1)
        .read_to_string(&mut text)
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::InvalidData {
                anyhow::Error::new(issue("diagnostic.reason.text_invalid_utf8", []))
            } else {
                error.into()
            }
        })?;
    if text.len() as u64 > MAX_TEXT_BYTES {
        bail!(issue(
            "diagnostic.reason.text_grew_limit",
            [("limitBytes", MAX_TEXT_BYTES.to_string())]
        ));
    }
    Ok(text.trim_start_matches('\u{feff}').to_owned())
}

fn fingerprint(path: &Path, name: String, max_bytes: u64) -> Result<KernelFile> {
    let before = regular_file(path)?.context(issue("diagnostic.reason.not_regular", []))?;
    if before.len() > max_bytes {
        bail!(issue(
            "diagnostic.reason.file_size_limit",
            [("limitBytes", max_bytes.to_string())]
        ));
    }
    let mut reader = File::open(path)?.take(max_bytes + 1);
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    let mut count = 0_u64;
    loop {
        let n = reader.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        count += n as u64;
        if count > max_bytes {
            bail!(issue(
                "diagnostic.reason.file_grew_limit",
                [("limitBytes", max_bytes.to_string())]
            ));
        }
        digest.update(&buffer[..n]);
    }
    let after = regular_file(path)?.context(issue("diagnostic.reason.file_disappeared", []))?;
    if count != before.len()
        || after.len() != before.len()
        || before.modified().ok() != after.modified().ok()
    {
        bail!(issue("diagnostic.reason.file_changed", []));
    }
    Ok(KernelFile {
        name,
        size_bytes: count,
        sha256: format!("{:x}", digest.finalize()),
    })
}

fn add_fingerprint(report: &mut KernelReport, path: &Path, name: &str, limit: u64) {
    match fingerprint(path, name.to_owned(), limit) {
        Ok(file) => report.files.push(file),
        Err(error) => report.diagnostics.push(issue_diagnostic(
            "file_unreadable",
            "warning",
            "File fingerprint unavailable",
            "diagnostic.file_unreadable.body",
            &issue_for_error(&error),
            [("filename", name.into())],
        )),
    }
}

fn extract_version(text: &str) -> Option<String> {
    let clean = text
        .lines()
        .map(|line| line.split('#').next().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n");
    let lower = clean.to_ascii_lowercase();
    for (start, _) in lower.match_indices("project") {
        if start > 0
            && (lower.as_bytes()[start - 1].is_ascii_alphanumeric()
                || lower.as_bytes()[start - 1] == b'_')
        {
            continue;
        }
        let tail = clean[start + "project".len()..].trim_start();
        let Some(body) = tail
            .strip_prefix('(')
            .and_then(|v| v.split_once(')').map(|v| v.0))
        else {
            continue;
        };
        let declaration: Vec<_> = body.split_whitespace().collect();
        if declaration.first() != Some(&"OpenSteamTool") {
            continue;
        }
        if let Some(index) = declaration.iter().position(|s| *s == "VERSION") {
            if let Some(value) = declaration.get(index + 1) {
                if value.len() <= 64
                    && value.split('.').count() >= 2
                    && value
                        .split('.')
                        .all(|s| !s.is_empty() && s.bytes().all(|c| c.is_ascii_digit()))
                {
                    return Some((*value).to_string());
                }
            }
        }
    }
    None
}

fn binary_architecture(path: &Path) -> Result<&'static str> {
    regular_file(path)?.context(issue("diagnostic.reason.not_regular", []))?;
    let mut file = File::open(path)?;
    let mut dos = [0_u8; 64];
    file.read_exact(&mut dos)
        .context(issue("diagnostic.reason.pe_header_incomplete", []))?;
    if &dos[..2] != b"MZ" {
        bail!(issue("diagnostic.reason.pe_dos_signature", []));
    }
    let offset = u32::from_le_bytes(dos[60..64].try_into().unwrap()) as u64;
    if !(64..=1024 * 1024).contains(&offset) {
        bail!(issue(
            "diagnostic.reason.pe_header_offset",
            [
                ("minimum", "64".into()),
                ("maximum", (1024 * 1024).to_string())
            ]
        ));
    }
    file.seek(SeekFrom::Start(offset))?;
    let mut pe = [0_u8; 6];
    file.read_exact(&mut pe)
        .context(issue("diagnostic.reason.pe_header_truncated", []))?;
    if &pe[..4] != b"PE\0\0" {
        bail!(issue("diagnostic.reason.pe_signature_invalid", []));
    }
    Ok(match u16::from_le_bytes([pe[4], pe[5]]) {
        0x8664 => "x86-64",
        0x014c => "x86",
        0xaa64 => "ARM64",
        _ => "unknown PE machine",
    })
}

fn inspect_binary_identity(path: &Path, name: &str, report: &mut KernelReport, set_version: bool) {
    match binary_architecture(path) {
        Ok(architecture) => report.diagnostics.push(message_diagnostic(
            "binary_architecture",
            "info",
            "PE architecture identified",
            if architecture == "unknown PE machine" {
                "diagnostic.binary_architecture.unknown"
            } else {
                "diagnostic.binary_architecture.body"
            },
            [
                ("filename", name.into()),
                ("architecture", architecture.into()),
            ],
        )),
        Err(error) => report.diagnostics.push(issue_diagnostic(
            "binary_header_invalid",
            "warning",
            "Binary header could not be identified",
            "diagnostic.binary_header_invalid.body",
            &issue_for_error(&error),
            [("filename", name.into())],
        )),
    }
    if let Some(version) = crate::steam::file_version(path) {
        if set_version {
            report.version = Some(version.clone());
        }
        report.diagnostics.push(message_diagnostic(
            "binary_version",
            "info",
            "Binary resource version identified",
            "diagnostic.binary_version.body",
            [("filename", name.into()), ("version", version)],
        ));
    }
}

/// Parser errors intentionally omit source lines, key names and values.
fn toml_issue(text: &str, error: &toml::de::Error) -> InspectionIssue {
    let duplicate = error.message().to_ascii_lowercase().contains("duplicate");
    match error.span() {
        Some(span) => {
            let mut pos = span.start.min(text.len());
            while !text.is_char_boundary(pos) {
                pos -= 1;
            }
            let prefix = &text[..pos];
            let line = prefix.bytes().filter(|&c| c == b'\n').count() + 1;
            let column = prefix.rsplit('\n').next().unwrap_or("").chars().count() + 1;
            issue(
                if duplicate {
                    "diagnostic.reason.toml_duplicate"
                } else {
                    "diagnostic.reason.toml_syntax"
                },
                [("line", line.to_string()), ("column", column.to_string())],
            )
        }
        None => issue(
            if duplicate {
                "diagnostic.reason.toml_duplicate_unlocated"
            } else {
                "diagnostic.reason.toml_syntax_unlocated"
            },
            [],
        ),
    }
}

fn safe_section_name(name: &str) -> String {
    if name.len() <= 48
        && !name.is_empty()
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
    {
        name.to_owned()
    } else {
        "[nonstandard section name omitted]".to_owned()
    }
}

fn inspect_config(path: &Path, diagnostics: &mut Vec<Diagnostic>) -> ConfigSummary {
    let mut summary = ConfigSummary {
        path: display_path(path),
        valid: false,
        sections: Vec::new(),
        unknown_sections: Vec::new(),
        error: None,
        error_key: String::new(),
        error_args: BTreeMap::new(),
    };
    let text = match read_text(path) {
        Ok(text) => text,
        Err(error) => {
            summary.error = Some(error.to_string());
            let item = issue_diagnostic(
                "config_unreadable",
                "error",
                "Configuration unreadable",
                "diagnostic.config_unreadable.body",
                &issue_for_error(&error),
                [("filename", display_path(path))],
            );
            summary.error_key = item.message_key.clone();
            summary.error_args = item.message_args.clone();
            diagnostics.push(item);
            return summary;
        }
    };
    match text.parse::<toml::Table>() {
        Err(error) => {
            let reason = toml_issue(&text, &error);
            let key = reason
                .key
                .replace("diagnostic.reason.toml_", "diagnostic.config_invalid.");
            let item = diagnostic_from_args(
                "config_invalid",
                "error",
                "Configuration syntax error",
                &key,
                reason.arguments,
            );
            summary.error = Some(item.detail.clone());
            summary.error_key = item.message_key.clone();
            summary.error_args = item.message_args.clone();
            diagnostics.push(item);
        }
        Ok(table) => {
            summary.valid = true;
            for (key, value) in &table {
                if value.is_table() {
                    summary.sections.push(safe_section_name(key));
                    if !CONFIG_SECTIONS.contains(&key.as_str()) {
                        summary.unknown_sections.push(safe_section_name(key));
                    }
                } else {
                    diagnostics.push(message_diagnostic(
                        "config_root_value",
                        "warning",
                        "Unexpected root-level setting",
                        "diagnostic.config_root_value.body",
                        [],
                    ));
                }
            }
            summary.sections.sort();
            summary.sections.dedup();
            summary.unknown_sections.sort();
            summary.unknown_sections.dedup();
            diagnostics.push(message_diagnostic(
                "config_valid",
                "success",
                "TOML syntax is valid",
                "diagnostic.config_valid.body",
                [],
            ));
            if !summary.unknown_sections.is_empty() {
                diagnostics.push(message_diagnostic(
                    "config_unknown_sections",
                    "warning",
                    "Unrecognized configuration sections",
                    "diagnostic.config_unknown_sections.body",
                    [("count", summary.unknown_sections.len().to_string())],
                ));
            }
            validate_config_values(&table, diagnostics);
        }
    }
    summary
}

fn validate_config_values(table: &toml::Table, diagnostics: &mut Vec<Diagnostic>) {
    // Return field identifiers only, never interpolated values (URLs may carry tokens).
    for (section, field, kind) in [
        ("log", "level", "string"),
        ("manifest", "url", "string"),
        ("manifest", "failover", "bool"),
        ("stats", "enable_api", "bool"),
        ("presence", "display", "string"),
        ("inject", "enabled", "bool"),
        ("cloud", "enabled", "bool"),
        ("remote", "order", "string"),
        ("remote", "url_template", "string"),
    ] {
        if let Some(value) = table.get(section).and_then(|v| v.get(field)) {
            let valid = match kind {
                "bool" => value.is_bool(),
                _ => value.is_str(),
            };
            if !valid {
                diagnostics.push(message_diagnostic(
                    "config_type_mismatch",
                    "warning",
                    "Configuration type mismatch",
                    "diagnostic.config_type_mismatch.body",
                    [
                        ("field", format!("{section}.{field}")),
                        ("expected", kind.into()),
                    ],
                ));
            }
        }
    }
    for field in [
        "timeout_resolve_ms",
        "timeout_connect_ms",
        "timeout_send_ms",
        "timeout_recv_ms",
    ] {
        if let Some(value) = table.get("manifest").and_then(|v| v.get(field)) {
            if !matches!(value.as_integer(), Some(1..=15000)) {
                diagnostics.push(message_diagnostic(
                    "config_timeout_range",
                    "warning",
                    "HTTP timeout outside the source range",
                    "diagnostic.config_timeout_range.body",
                    [
                        ("field", format!("manifest.{field}")),
                        ("minimum", "1".into()),
                        ("maximum", "15000".into()),
                    ],
                ));
            }
        }
    }
    if let Some(value) = table
        .get("remote")
        .and_then(|v| v.get("url_template"))
        .and_then(|v| v.as_str())
    {
        if !["{channel}", "{component}", "{sha256}"]
            .iter()
            .all(|part| value.contains(part))
        {
            diagnostics.push(message_diagnostic(
                "config_template_placeholders",
                "warning",
                "Metadata URL template is incomplete",
                "diagnostic.config_template_placeholders.body",
                [
                    ("field", "remote.url_template".into()),
                    ("required", "{channel}, {component}, {sha256}".into()),
                    (
                        "missing",
                        ["{channel}", "{component}", "{sha256}"]
                            .iter()
                            .filter(|part| !value.contains(**part))
                            .copied()
                            .collect::<Vec<_>>()
                            .join(", "),
                    ),
                ],
            ));
        }
        if !value.starts_with("https://") {
            diagnostics.push(message_diagnostic(
                "config_template_transport",
                "warning",
                "Metadata URL uses an unexpected transport",
                "diagnostic.config_template_transport.body",
                [],
            ));
        }
    }
    if let Some(value) = table.get("lua").and_then(|v| v.get("paths")) {
        if !value
            .as_array()
            .map(|a| a.iter().all(|v| v.is_str()))
            .unwrap_or(false)
        {
            diagnostics.push(message_diagnostic(
                "config_lua_paths_type",
                "warning",
                "Lua directory list has an unexpected type",
                "diagnostic.config_lua_paths_type.body",
                [],
            ));
        }
    }
    for (section, field, values) in [
        (
            "log",
            "level",
            &["trace", "debug", "info", "warn", "error"][..],
        ),
        ("presence", "display", &["spacewar", "none"][..]),
        ("remote", "order", &["gh-proxy-first", "github-first"][..]),
    ] {
        if let Some(value) = table
            .get(section)
            .and_then(|v| v.get(field))
            .and_then(|v| v.as_str())
        {
            if !values.contains(&value) {
                diagnostics.push(message_diagnostic(
                    "config_enum_value",
                    "warning",
                    "Unrecognized configuration option",
                    "diagnostic.config_enum_value.body",
                    [
                        ("field", format!("{section}.{field}")),
                        ("expected", values.join(", ")),
                    ],
                ));
            }
        }
    }
}

const KERNEL_INSTALLATION_FILES: &[&str] = &[
    "OpenSteamTool.dll",
    "OpenSteamTool.dll.disabled",
    "OpenSteamTool.dll.off",
    "xinput1_4.dll",
    "xinput1_4.dll.disabled",
    "xinput1_4.dll.off",
    "dwmapi.dll",
    "dwmapi.dll.disabled",
    "dwmapi.dll.off",
    "version.dll",
    "version.dll.disabled",
    "version.dll.off",
    "bin/dwmapi.dll",
    "bin/dwmapi.dll.disabled",
    "bin/dwmapi.dll.off",
];

fn empty_kernel_installation(root: &Path) -> KernelInstallation {
    let path = display_path(root);
    let message_key = "diagnostic.kernel_installation.not_installed.body".to_owned();
    let message_args = BTreeMap::from([("path".into(), path.clone())]);
    KernelInstallation {
        path,
        status: "notInstalled".into(),
        injection_method: "none".into(),
        version: None,
        version_source: "unknown".into(),
        installed_sha256: None,
        artifact_version: None,
        artifact_sha256: None,
        hash_match: None,
        runtime_load_verified: false,
        files: Vec::new(),
        remnants: Vec::new(),
        message_key: message_key.clone(),
        message_args: message_args.clone(),
        diagnostics: vec![
            message_diagnostic(
                "disk_state_scan",
                "info",
                "On-disk state scan",
                "diagnostic.disk_state_scan.body",
                [],
            ),
            diagnostic_from_args(
                "kernel_installation_not_installed",
                "info",
                "OpenSteamTool is not installed",
                &message_key,
                message_args,
            ),
        ],
    }
}

/// Inventory the recognized OpenSteamTool installation files in a Steam root.
///
/// This routine only reads directory metadata, file-version resources and file
/// bytes for bounded SHA-256 fingerprints. It never loads a DLL or changes a file.
pub fn detect_kernel_installation(steam_path: &Path) -> Result<KernelInstallation> {
    detect_kernel_installation_with_artifact(steam_path, None)
}

/// Inventory an installation and, when an artifact directory is provided,
/// attach version/hash evidence from its BUILD_INFO.json or SHA256SUMS.json.
/// The artifact version is accepted only when the primary DLL hash matches.
pub fn detect_kernel_installation_with_artifact(
    steam_path: &Path,
    artifact_path: Option<&Path>,
) -> Result<KernelInstallation> {
    let root = absolute_path(steam_path)?;
    match fs::symlink_metadata(&root) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => bail!("Installed-kernel detection requires a Steam directory"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(empty_kernel_installation(&root));
        }
        Err(error) => return Err(error).context("Could not inspect the Steam directory"),
    }

    let mut present = BTreeSet::new();
    let mut files = Vec::new();
    let mut diagnostics = vec![message_diagnostic(
        "disk_state_scan",
        "info",
        "On-disk state scan",
        "diagnostic.disk_state_scan.body",
        [],
    )];
    for name in KERNEL_INSTALLATION_FILES {
        let path = root.join(name);
        if regular_file(&path)?.is_none() {
            continue;
        }
        present.insert((*name).to_owned());
        match fingerprint(&path, (*name).to_owned(), MAX_BINARY_BYTES) {
            Ok(file) => files.push(file),
            Err(error) => diagnostics.push(issue_diagnostic(
                "file_unreadable",
                "warning",
                "File fingerprint unavailable",
                "diagnostic.file_unreadable.body",
                &issue_for_error(&error),
                [("filename", (*name).to_owned())],
            )),
        }
    }

    let has = |name: &str| present.contains(name);
    let body_present = [
        "OpenSteamTool.dll",
        "OpenSteamTool.dll.disabled",
        "OpenSteamTool.dll.off",
        "xinput1_4.dll",
        "xinput1_4.dll.disabled",
        "xinput1_4.dll.off",
    ]
    .iter()
    .any(|name| has(name));
    let active_body = has("OpenSteamTool.dll") && has("xinput1_4.dll");
    let disabled_body = has("OpenSteamTool.dll.off")
        && has("xinput1_4.dll.off")
        && !has("OpenSteamTool.dll")
        && !has("xinput1_4.dll");
    let active_dwmapi = has("dwmapi.dll");
    let active_version = has("version.dll");
    let disabled_dwmapi = has("dwmapi.dll.off");
    let disabled_version = has("version.dll.off");

    let injection_method = match (active_dwmapi, active_version) {
        (true, true) => "both",
        (true, false) => "dwmapi",
        (false, true) => "version",
        (false, false) => match (disabled_dwmapi, disabled_version) {
            (true, true) => "both",
            (true, false) => "dwmapi",
            (false, true) => "version",
            (false, false) => "none",
        },
    };

    let status = if !body_present {
        "notInstalled"
    } else if active_body && active_dwmapi && active_version {
        "conflict"
    } else if active_body && active_dwmapi != active_version {
        "enabled"
    } else if disabled_body
        && !active_dwmapi
        && !active_version
        && (disabled_dwmapi || disabled_version)
    {
        "disabled"
    } else {
        "incomplete"
    };

    let version_source = [
        "OpenSteamTool.dll",
        "OpenSteamTool.dll.off",
        "OpenSteamTool.dll.disabled",
    ]
    .into_iter()
    .find(|name| has(name));
    let installed_primary = root.join("OpenSteamTool.dll");
    let installed_sha256 = if regular_file(&installed_primary)?.is_some() {
        fingerprint(
            &installed_primary,
            "OpenSteamTool.dll".into(),
            MAX_BINARY_BYTES,
        )
        .ok()
        .map(|file| file.sha256)
    } else {
        None
    };
    let pe_version = version_source.and_then(|name| crate::steam::file_version(&root.join(name)));
    let (artifact_version, artifact_sha256) = artifact_metadata(artifact_path);
    let hash_match = installed_sha256
        .as_ref()
        .zip(artifact_sha256.as_ref())
        .map(|(installed, artifact)| installed.eq_ignore_ascii_case(artifact));
    let (version, version_source_name) = if let Some(version) = pe_version {
        (Some(version), "peResource")
    } else if hash_match == Some(true) {
        (artifact_version.clone(), "artifactManifest")
    } else {
        (None, "unknown")
    };

    let mut remnants = Vec::new();
    for name in &present {
        let legacy = name.starts_with("bin/");
        let remnant = match status {
            "notInstalled" | "incomplete" => true,
            "enabled" => {
                legacy
                    || name.ends_with(".off")
                    || matches!(
                        name.as_str(),
                        "OpenSteamTool.dll.disabled" | "xinput1_4.dll.disabled"
                    )
                    || (injection_method == "dwmapi" && name == "dwmapi.dll.disabled")
                    || (injection_method == "version" && name == "version.dll.disabled")
            }
            "conflict" => legacy || name.ends_with(".off") || name.ends_with(".disabled"),
            "disabled" => {
                legacy
                    || matches!(
                        name.as_str(),
                        "OpenSteamTool.dll"
                            | "xinput1_4.dll"
                            | "OpenSteamTool.dll.disabled"
                            | "xinput1_4.dll.disabled"
                            | "dwmapi.dll"
                            | "version.dll"
                    )
                    || (injection_method == "dwmapi" && name == "dwmapi.dll.disabled")
                    || (injection_method == "version" && name == "version.dll.disabled")
                    || (injection_method == "both"
                        && matches!(
                            name.as_str(),
                            "dwmapi.dll.disabled" | "version.dll.disabled"
                        ))
            }
            _ => false,
        };
        if remnant {
            remnants.push(name.clone());
        }
    }

    let (diagnostic_code, message_key, severity, title) = match status {
        "enabled" => (
            "kernel_installation_enabled",
            "diagnostic.kernel_installation.enabled.body",
            "success",
            "OpenSteamTool is enabled",
        ),
        "disabled" => (
            "kernel_installation_disabled",
            "diagnostic.kernel_installation.disabled.body",
            "info",
            "OpenSteamTool is disabled",
        ),
        "conflict" => (
            "kernel_installation_conflict",
            "diagnostic.kernel_installation.conflict.body",
            "warning",
            "OpenSteamTool injection entries conflict",
        ),
        "incomplete" => (
            "kernel_installation_incomplete",
            "diagnostic.kernel_installation.incomplete.body",
            "warning",
            "OpenSteamTool installation is incomplete",
        ),
        _ => (
            "kernel_installation_not_installed",
            "diagnostic.kernel_installation.not_installed.body",
            "info",
            "OpenSteamTool is not installed",
        ),
    };
    let message_args = BTreeMap::from([
        ("path".into(), display_path(&root)),
        ("injectionMethod".into(), injection_method.into()),
    ]);
    diagnostics.push(diagnostic_from_args(
        diagnostic_code,
        severity,
        title,
        message_key,
        message_args.clone(),
    ));
    if !remnants.is_empty() {
        diagnostics.push(message_diagnostic(
            "kernel_installation_remnants",
            "warning",
            "Inactive or legacy kernel files found",
            "diagnostic.kernel_installation.remnants.body",
            [
                ("count", remnants.len().to_string()),
                ("files", remnants.join(", ")),
            ],
        ));
    }
    if body_present && version.is_none() {
        let filename = version_source
            .map(|name| display_path(&root.join(name)))
            .unwrap_or_else(|| display_path(&root.join("OpenSteamTool.dll")));
        diagnostics.push(message_diagnostic(
            "installed_version_unknown",
            "warning",
            "Installed kernel version is unknown",
            "diagnostic.installed_version_unknown.body",
            [("filename", filename)],
        ));
    }

    files.sort_by(|left, right| left.name.cmp(&right.name));
    remnants.sort();
    Ok(KernelInstallation {
        path: display_path(&root),
        status: status.into(),
        injection_method: injection_method.into(),
        version,
        version_source: version_source_name.into(),
        installed_sha256,
        artifact_version,
        artifact_sha256,
        hash_match,
        runtime_load_verified: false,
        files,
        remnants,
        message_key: message_key.into(),
        message_args,
        diagnostics,
    })
}

fn artifact_metadata(path: Option<&Path>) -> (Option<String>, Option<String>) {
    let Some(path) = path else {
        return (None, None);
    };
    let root = match absolute_path(path) {
        Ok(path) => path,
        Err(_) => return (None, None),
    };
    let mut version = None;
    let mut sha256 = None;
    if let Ok(bytes) = fs::read(root.join("BUILD_INFO.json")) {
        if let Ok(value) = serde_json::from_slice::<Value>(&bytes) {
            version = value
                .get("version")
                .and_then(Value::as_str)
                .map(str::to_owned);
            sha256 = value
                .get("files")
                .and_then(Value::as_array)
                .and_then(|files| {
                    files.iter().find_map(|file| {
                        let name = file.get("name").and_then(Value::as_str)?;
                        (name.eq_ignore_ascii_case("OpenSteamTool.dll"))
                            .then(|| file.get("sha256").and_then(Value::as_str))
                            .flatten()
                            .map(str::to_owned)
                    })
                });
        }
    }
    if sha256.is_none() {
        if let Ok(bytes) = fs::read(root.join("SHA256SUMS.json")) {
            if let Ok(value) = serde_json::from_slice::<Value>(&bytes) {
                sha256 = value
                    .get("files")
                    .and_then(Value::as_array)
                    .and_then(|files| {
                        files.iter().find_map(|file| {
                            let name = file.get("path").and_then(Value::as_str)?;
                            (name.eq_ignore_ascii_case("OpenSteamTool.dll"))
                                .then(|| file.get("sha256").and_then(Value::as_str))
                                .flatten()
                                .map(str::to_owned)
                        })
                    });
            }
        }
    }
    (version, sha256)
}

/// Backward-compatible installed-kernel report used by the existing GUI.
/// The richer on-disk state is provided by [`detect_kernel_installation`].
pub fn detect_installed_kernel(steam_path: &Path) -> Result<Option<KernelReport>> {
    let installation = detect_kernel_installation(steam_path)?;
    if installation.status == "notInstalled" {
        return Ok(None);
    }
    let root = absolute_path(steam_path)?;
    let mut report = inspect_kernel(&root, Some(&root))?;
    report.kind = "installed".into();
    // inspect_directory may encounter a nearby CMake project. It describes a
    // different artifact and must never supply this installed DLL's version.
    report.version = installation.version.clone();
    report
        .diagnostics
        .retain(|item| item.code != "source_version");
    for file in installation.files {
        if !report
            .files
            .iter()
            .any(|existing| existing.name == file.name)
        {
            report.files.push(file);
        }
    }
    for diagnostic in installation.diagnostics {
        if !report
            .diagnostics
            .iter()
            .any(|existing| existing.code == diagnostic.code)
        {
            report.diagnostics.push(diagnostic);
        }
    }
    let primary = root.join("OpenSteamTool.dll");
    if regular_file(&primary)?.is_some() {
        report.diagnostics.push(message_diagnostic(
            "installed_kernel_detected",
            "info",
            "Installed primary kernel file detected",
            "diagnostic.installed_kernel_detected.body",
            [("path", display_path(&primary))],
        ));
    }
    report
        .files
        .sort_by(|left, right| left.name.cmp(&right.name));
    Ok(Some(report))
}

/// Inspect a source tree, installation directory, TOML or Lua without loading code.
pub fn inspect_kernel(path: &Path, steam_path: Option<&Path>) -> Result<KernelReport> {
    let path = absolute_path(path)?;
    let mut report = KernelReport {
        path: display_path(&path),
        kind: "absent".into(),
        version: None,
        files: Vec::new(),
        config: None,
        diagnostics: vec![message_diagnostic(
            "disk_state_scan",
            "info",
            "On-disk state scan",
            "diagnostic.disk_state_scan.body",
            [],
        )],
        lua_files: Vec::new(),
    };
    let meta = match fs::symlink_metadata(&path) {
        Ok(meta) => Some(meta),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).context("Could not inspect the kernel path"),
    };
    match meta {
        None => report.diagnostics.push(message_diagnostic(
            "kernel_missing",
            "error",
            "Kernel path not found",
            "diagnostic.kernel_missing.body",
            [("path", display_path(&path))],
        )),
        Some(meta) if meta.is_dir() => inspect_directory(&path, &mut report)?,
        Some(meta) if meta.is_file() => {
            let ext = path
                .extension()
                .and_then(|v| v.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            match ext.as_str() {
                "toml" => { report.kind = "configuration".into(); report.config = Some(inspect_config(&path, &mut report.diagnostics)); },
                "lua" => {
                    report.kind = "lua".into();
                    report.lua_files.push(inspect_lua(&path, path.file_name().unwrap_or_default().to_string_lossy().as_ref())?);
                },
                "dll" => {
                    report.kind = "binary".into();
                    inspect_binary_identity(&path, path.file_name().unwrap_or_default().to_string_lossy().as_ref(), &mut report, true);
                    report.diagnostics.push(message_diagnostic("binary_identity_unverified", "warning", "DLL identity is not authenticated", "diagnostic.binary_identity_unverified.body", []));
                },
                _ => bail!("Supported input files are TOML, Lua and DLL; source comparisons require directories"),
            }
            add_fingerprint(
                &mut report,
                &path,
                path.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .as_ref(),
                MAX_BINARY_BYTES,
            );
        }
        Some(_) => bail!("Special filesystem objects are not inspected"),
    }
    if let Some(steam) = steam_path {
        inspect_steam_metadata(&absolute_path(steam)?, &mut report);
    } else {
        report.diagnostics.push(message_diagnostic(
            "steam_path_unset",
            "info",
            "Steam fingerprints not requested",
            "diagnostic.steam_path_unset.body",
            [],
        ));
    }
    report.diagnostics.push(message_diagnostic(
        "runtime_unverified",
        "warning",
        "Runtime compatibility is unverified",
        "diagnostic.runtime_unverified.body",
        [],
    ));
    report.files.sort_by(|a, b| a.name.cmp(&b.name));
    report.lua_files.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(report)
}

fn inspect_directory(root: &Path, report: &mut KernelReport) -> Result<()> {
    report.kind = "directory".into();
    for candidate in ["src/CMakeLists.txt", "CMakeLists.txt"] {
        let path = root.join(candidate);
        if regular_file(&path)?.is_some() {
            let text = read_text(&path)?;
            if let Some(version) = extract_version(&text) {
                report.version = Some(version.clone());
                report.kind = "source".into();
                report.diagnostics.push(message_diagnostic(
                    "source_version",
                    "success",
                    "Source version identified",
                    "diagnostic.source_version.body",
                    [("filename", candidate.into()), ("version", version)],
                ));
                add_fingerprint(report, &path, candidate, MAX_SOURCE_FILE_BYTES);
                break;
            }
        }
    }
    let license = root.join("LICENSE");
    if regular_file(&license)?.is_some() {
        let text = read_text(&license)?;
        let message_key =
            if text.contains("GNU GENERAL PUBLIC LICENSE") && text.contains("Version 3") {
                "diagnostic.license_detected.gpl3"
            } else {
                "diagnostic.license_detected.unknown"
            };
        report.diagnostics.push(message_diagnostic(
            "license_detected",
            "info",
            "License file found",
            message_key,
            [],
        ));
        add_fingerprint(report, &license, "LICENSE", MAX_SOURCE_FILE_BYTES);
    }
    for candidate in ["opensteamtool.toml", "opensteamtool.example.toml"] {
        let path = root.join(candidate);
        if regular_file(&path)?.is_some() {
            report.config = Some(inspect_config(&path, &mut report.diagnostics));
            add_fingerprint(report, &path, candidate, MAX_SOURCE_FILE_BYTES);
            if candidate.ends_with(".example.toml") {
                report.diagnostics.push(message_diagnostic(
                    "example_configuration",
                    "info",
                    "Inspecting an example configuration",
                    "diagnostic.example_configuration.body",
                    [("filename", candidate.into())],
                ));
            }
            break;
        }
    }
    for candidate in [
        "OpenSteamTool.dll",
        "dwmapi.dll",
        "winmm.dll",
        "version.dll",
        "version.dll.disabled",
    ] {
        let path = root.join(candidate);
        if regular_file(&path)?.is_some() {
            if candidate == "OpenSteamTool.dll" && report.kind != "source" {
                report.kind = "installed".into();
            }
            let set_version = candidate == "OpenSteamTool.dll" && report.kind != "source";
            inspect_binary_identity(&path, candidate, report, set_version);
            add_fingerprint(report, &path, candidate, MAX_BINARY_BYTES);
            report.diagnostics.push(message_diagnostic(
                "binary_present",
                "info",
                "Possible kernel binary found",
                "diagnostic.binary_present.body",
                [("filename", candidate.into())],
            ));
        }
    }
    let remote = root.join("src/Utils/SteamMetadata/RemoteToml.cpp");
    if regular_file(&remote)?.is_some() {
        let source = read_text(&remote)?;
        let (message_key, repository) = if source.contains("huanyuejue/steam-monitor") {
            (
                "diagnostic.metadata_source.fork",
                "huanyuejue/steam-monitor",
            )
        } else if source.contains("OpenSteam001/steam-monitor") {
            (
                "diagnostic.metadata_source.original",
                "OpenSteam001/steam-monitor",
            )
        } else {
            ("diagnostic.metadata_source.unknown", "")
        };
        report.diagnostics.push(message_diagnostic(
            "metadata_source",
            "info",
            "Metadata source inspected",
            message_key,
            [("repository", repository.into())],
        ));
        add_fingerprint(
            report,
            &remote,
            "src/Utils/SteamMetadata/RemoteToml.cpp",
            MAX_SOURCE_FILE_BYTES,
        );
    }
    let mut paths = BTreeSet::new();
    collect_lua(root, root, 0, false, &mut paths, &mut report.diagnostics)?;
    for directory in ["config/lua", "lua"] {
        let candidate = root.join(directory);
        if candidate.exists() {
            reject_link_ancestors(&candidate)?;
            collect_lua(
                root,
                &candidate,
                0,
                true,
                &mut paths,
                &mut report.diagnostics,
            )?;
        }
    }
    for path in paths {
        let name = relative_name(root, &path)?;
        match inspect_lua(&path, &name) {
            Ok(summary) => report.lua_files.push(summary),
            Err(error) => report.diagnostics.push(issue_diagnostic(
                "lua_unreadable",
                "warning",
                "Lua source not inspected",
                "diagnostic.lua_unreadable.body",
                &issue_for_error(&error),
                [("filename", name)],
            )),
        }
    }
    if report.kind == "directory" {
        report.diagnostics.push(message_diagnostic(
            "kernel_identity_unknown",
            "warning",
            "Kernel identity not established",
            "diagnostic.kernel_identity_unknown.body",
            [],
        ));
    }
    Ok(())
}

fn relative_name(root: &Path, path: &Path) -> Result<String> {
    Ok(path
        .strip_prefix(root)
        .context("Inspected file escaped the selected root")?
        .to_string_lossy()
        .replace('\\', "/"))
}

fn skip_directory(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        ".git"
            | ".deps"
            | "build"
            | "target"
            | "node_modules"
            | "dist"
            | "out"
            | "debug"
            | "release"
            | "userdata"
            | "appcache"
            | "depotcache"
            | "steamapps"
            | "htmlcache"
            | "logs"
            | "dumps"
    )
}

fn sorted_entries(path: &Path) -> Result<Vec<fs::DirEntry>> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(path).context("Could not enumerate the inspection directory")? {
        if entries.len() >= MAX_TREE_FILES {
            bail!("Directory exceeds the 10,000 entry inspection limit");
        }
        entries.push(entry?);
    }
    entries.sort_by_key(|entry| entry.file_name());
    Ok(entries)
}

fn collect_lua(
    root: &Path,
    directory: &Path,
    depth: usize,
    recurse: bool,
    paths: &mut BTreeSet<PathBuf>,
    diagnostics: &mut Vec<Diagnostic>,
) -> Result<()> {
    if depth > MAX_DEPTH {
        bail!("Lua directory depth exceeds the inspection limit");
    }
    reject_link_ancestors(directory)?;
    for entry in sorted_entries(directory)? {
        let path = entry.path();
        let meta = fs::symlink_metadata(&path)?;
        if is_link(&meta) {
            diagnostics.push(message_diagnostic(
                "linked_path_skipped",
                "warning",
                "Linked path skipped",
                "diagnostic.linked_path_skipped.body",
                [("path", display_path(&path))],
            ));
            continue;
        }
        if meta.is_dir() && recurse && !skip_directory(&entry.file_name().to_string_lossy()) {
            collect_lua(root, &path, depth + 1, recurse, paths, diagnostics)?;
        } else if meta.is_file()
            && path
                .extension()
                .and_then(|s| s.to_str())
                .map(|s| s.eq_ignore_ascii_case("lua"))
                .unwrap_or(false)
        {
            relative_name(root, &path)?;
            paths.insert(path);
            if paths.len() > MAX_LUA_FILES {
                bail!("Lua inventory exceeds the 2,000 file inspection limit");
            }
        }
    }
    Ok(())
}

// Minimal lexical scanner: it does not interpret Lua or expose string contents.
// It recognizes only literal integer first arguments to addappid().
fn long_bracket_end(bytes: &[u8], start: usize) -> Option<usize> {
    if bytes.get(start) != Some(&b'[') {
        return None;
    }
    let mut p = start + 1;
    while bytes.get(p) == Some(&b'=') {
        p += 1;
    }
    if bytes.get(p) != Some(&b'[') {
        return None;
    }
    let equals = p - start - 1;
    let mut i = p + 1;
    while i < bytes.len() {
        if bytes[i] == b']' {
            let mut j = i + 1;
            while j < bytes.len() && bytes[j] == b'=' {
                j += 1;
            }
            if j - i - 1 == equals && bytes.get(j) == Some(&b']') {
                return Some(j + 1);
            }
        }
        i += 1;
    }
    Some(bytes.len())
}

fn skip_lua_trivia(bytes: &[u8], mut position: usize) -> usize {
    loop {
        while bytes
            .get(position)
            .map(|b| b.is_ascii_whitespace())
            .unwrap_or(false)
        {
            position += 1;
        }
        if bytes.get(position) == Some(&b'-') && bytes.get(position + 1) == Some(&b'-') {
            position += 2;
            if let Some(end) = long_bracket_end(bytes, position) {
                position = end;
            } else {
                while position < bytes.len() && bytes[position] != b'\n' {
                    position += 1;
                }
            }
        } else {
            return position;
        }
    }
}

fn literal_app_ids(source: &str) -> (Vec<u32>, bool, bool) {
    let bytes = source.as_bytes();
    let mut ids = BTreeSet::new();
    let mut dynamic = false;
    let mut credentials = false;
    let mut i = 0;
    while i < bytes.len() {
        i = skip_lua_trivia(bytes, i);
        if i >= bytes.len() {
            break;
        }
        if bytes[i] == b'\'' || bytes[i] == b'"' {
            let quote = bytes[i];
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\\' {
                    i = (i + 2).min(bytes.len());
                } else if bytes[i] == quote {
                    i += 1;
                    break;
                } else {
                    i += 1;
                }
            }
            continue;
        }
        if let Some(end) = long_bracket_end(bytes, i) {
            i = end;
            continue;
        }
        if bytes[i].is_ascii_alphabetic() || bytes[i] == b'_' {
            let start = i;
            i += 1;
            while bytes
                .get(i)
                .map(|b| b.is_ascii_alphanumeric() || *b == b'_')
                .unwrap_or(false)
            {
                i += 1;
            }
            let name = &source[start..i]; // ASCII identifier boundaries are UTF-8 boundaries.
            if matches!(
                name.to_ascii_lowercase().as_str(),
                "setappticket" | "seteticket" | "setdepotkey"
            ) {
                credentials = true;
            }
            if name.eq_ignore_ascii_case("addappid") {
                let mut p = skip_lua_trivia(bytes, i);
                if bytes.get(p) != Some(&b'(') {
                    continue;
                }
                p = skip_lua_trivia(bytes, p + 1);
                let start = p;
                while bytes.get(p).map(|b| b.is_ascii_digit()).unwrap_or(false) {
                    p += 1;
                }
                let end = p;
                p = skip_lua_trivia(bytes, p);
                if start != end && matches!(bytes.get(p), Some(b',') | Some(b')')) {
                    match source[start..end].parse::<u32>() {
                        Ok(id) if id > 0 => {
                            ids.insert(id);
                        }
                        _ => dynamic = true,
                    }
                } else {
                    dynamic = true;
                }
            }
        } else {
            i += 1;
        }
    }
    (ids.into_iter().collect(), dynamic, credentials)
}

fn inspect_lua(path: &Path, name: &str) -> Result<LuaSummary> {
    let meta = regular_file(path)?.context(issue("diagnostic.reason.not_regular", []))?;
    let source = read_text(path)?;
    let (app_ids, dynamic, credentials) = literal_app_ids(&source);
    let mut warnings = vec!["Static literal IDs only; Lua was not executed and IDs do not establish ownership or distinguish games from depots.".into()];
    if dynamic {
        warnings.push(
            "Some addappid arguments are dynamic, invalid or out of range and were omitted.".into(),
        );
    }
    if credentials {
        warnings.push(
            "Credential-related directive names were detected; all argument values are omitted."
                .into(),
        );
    }
    Ok(LuaSummary {
        name: name.into(),
        size_bytes: meta.len(),
        app_ids,
        warnings,
    })
}

fn parse_hex_u32(value: &str) -> Option<u32> {
    let value = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
        .unwrap_or(value);
    if value.is_empty() || value.len() > 8 || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    u32::from_str_radix(value, 16).ok()
}

fn valid_signature(value: &str) -> bool {
    let tokens: Vec<_> = value
        .split(|c: char| c.is_ascii_whitespace() || c == ',')
        .filter(|s| !s.is_empty())
        .collect();
    !tokens.is_empty()
        && tokens.iter().all(|token| {
            *token == "??" || (token.len() == 2 && token.bytes().all(|b| b.is_ascii_hexdigit()))
        })
}

fn valid_pattern_metadata(table: &toml::Table) -> bool {
    table.iter().any(|(key, entry)| {
        if parse_hex_u32(key).is_none() {
            return false;
        }
        let Some(entry) = entry.as_table() else {
            return false;
        };
        let rva = entry
            .get("rva")
            .and_then(|v| v.as_str())
            .and_then(parse_hex_u32)
            .map(|n| n != 0)
            .unwrap_or(false);
        let signature = entry
            .get("sig")
            .and_then(|v| v.as_str())
            .map(valid_signature)
            .unwrap_or(false);
        rva || signature
    })
}

fn valid_ipc_metadata(table: &toml::Table) -> bool {
    invalid_ipc_interfaces(table).is_empty()
}

fn invalid_ipc_interfaces(table: &toml::Table) -> Vec<&'static str> {
    ["IClientUser", "IClientUtils"]
        .into_iter()
        .filter(|name| {
            let Some(interface) = table.get(*name).and_then(|v| v.as_table()) else {
                return true;
            };
            if let Some(value) = interface.get("interface_id") {
                if !matches!(value.as_integer(), Some(0..=255)) {
                    return true;
                }
            }
            !interface
                .values()
                .filter_map(|v| v.as_table())
                .any(|method| {
                    method
                        .get("funcHash")
                        .and_then(|v| v.as_str())
                        .and_then(parse_hex_u32)
                        .is_some()
                })
        })
        .collect()
}

fn inspect_steam_metadata(steam: &Path, report: &mut KernelReport) {
    let mut hashes = BTreeMap::new();
    for (filename, component) in [
        ("steamclient64.dll", "steamclient"),
        ("steamui.dll", "steamui"),
    ] {
        let path = steam.join(filename);
        match fingerprint(&path, format!("Steam/{filename}"), MAX_BINARY_BYTES) {
            Ok(file) => {
                hashes.insert(component, file.sha256.clone());
                report.files.push(file);
                report.diagnostics.push(message_diagnostic(
                    "steam_fingerprint",
                    "success",
                    "Steam component fingerprinted",
                    "diagnostic.steam_fingerprint.body",
                    [
                        ("filename", filename.into()),
                        ("sha256", hashes[component].clone()),
                    ],
                ));
            }
            Err(error) => report.diagnostics.push(issue_diagnostic(
                "steam_fingerprint_unavailable",
                "warning",
                "Steam component fingerprint unavailable",
                "diagnostic.steam_fingerprint_unavailable.body",
                &issue_for_error(&error),
                [("filename", filename.into())],
            )),
        }
    }
    for (channel, component) in [
        ("pattern", "steamclient"),
        ("pattern", "steamui"),
        ("ipc", "steamclient"),
    ] {
        let Some(hash) = hashes.get(component) else {
            continue;
        };
        let name = format!("opensteamtool/{channel}/{component}/{hash}.toml");
        let path = steam.join(&name);
        match regular_file(&path) {
            Ok(None) => report.diagnostics.push(message_diagnostic(
                "metadata_missing",
                "warning",
                "Matching local metadata not found",
                "diagnostic.metadata_missing.body",
                [
                    ("channel", channel.into()),
                    ("component", component.into()),
                    ("sha256", hash.clone()),
                    ("path", display_path(&path)),
                ],
            )),
            Err(error) => report.diagnostics.push(issue_diagnostic(
                "metadata_unreadable",
                "warning",
                "Local metadata unreadable",
                "diagnostic.metadata_unreadable.body",
                &issue_for_error(&error),
                [
                    ("channel", channel.into()),
                    ("component", component.into()),
                    ("path", display_path(&path)),
                ],
            )),
            Ok(Some(_)) => {
                let parsed = match read_text(&path) {
                    Ok(text) => match text.parse::<toml::Table>() {
                        Ok(table) => Ok(table),
                        Err(error) => Err(toml_issue(&text, &error)),
                    },
                    Err(error) => Err(issue_for_error(&error)),
                };
                match parsed {
                    Err(reason) => report.diagnostics.push(issue_diagnostic(
                        "metadata_invalid",
                        "error",
                        "Matching metadata failed parsing",
                        "diagnostic.metadata_invalid.body",
                        &reason,
                        [
                            ("channel", channel.into()),
                            ("component", component.into()),
                            ("path", display_path(&path)),
                        ],
                    )),
                    Ok(table) => {
                        let valid = if channel == "ipc" {
                            valid_ipc_metadata(&table)
                        } else {
                            valid_pattern_metadata(&table)
                        };
                        if valid {
                            report.diagnostics.push(message_diagnostic(
                                "metadata_match",
                                "success",
                                "Hash-named metadata passes structural checks",
                                "diagnostic.metadata_match.body",
                                [
                                    ("channel", channel.into()),
                                    ("component", component.into()),
                                    ("sha256", hash.clone()),
                                ],
                            ));
                            add_fingerprint(
                                report,
                                &path,
                                &format!("Steam/{name}"),
                                MAX_TEXT_BYTES,
                            );
                        } else {
                            report.diagnostics.push(message_diagnostic(
                                "metadata_structure_invalid",
                                "error",
                                "Matching metadata has an unexpected structure",
                                if channel == "ipc" {
                                    "diagnostic.metadata_structure_invalid.ipc"
                                } else {
                                    "diagnostic.metadata_structure_invalid.pattern"
                                },
                                [
                                    ("channel", channel.into()),
                                    ("component", component.into()),
                                    (
                                        "missing",
                                        if channel == "ipc" {
                                            invalid_ipc_interfaces(&table).join(", ")
                                        } else {
                                            String::new()
                                        },
                                    ),
                                ],
                            ));
                        }
                    }
                }
            }
        }
    }
}

fn is_source_file(path: &Path) -> bool {
    let name = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_ascii_lowercase();
    if matches!(
        name.as_str(),
        "license" | "copying" | ".gitignore" | ".gitattributes" | ".gitkeep"
    ) {
        return true;
    }
    matches!(
        path.extension()
            .and_then(|v| v.to_str())
            .unwrap_or("")
            .to_ascii_lowercase()
            .as_str(),
        "c" | "cc"
            | "cpp"
            | "h"
            | "hpp"
            | "rs"
            | "cmake"
            | "txt"
            | "md"
            | "toml"
            | "lua"
            | "proto"
            | "steamd"
            | "in"
            | "def"
            | "bat"
            | "ps1"
            | "yml"
            | "yaml"
            | "svg"
            | "png"
    )
}

fn collect_source_files(
    root: &Path,
    directory: &Path,
    depth: usize,
    files: &mut BTreeMap<String, KernelFile>,
    bytes: &mut u64,
    skipped_links: &mut usize,
) -> Result<()> {
    if depth > MAX_DEPTH {
        bail!("Source directory depth exceeds the comparison limit");
    }
    reject_link_ancestors(directory)?;
    for entry in sorted_entries(directory)? {
        let path = entry.path();
        let meta = fs::symlink_metadata(&path)?;
        if is_link(&meta) {
            *skipped_links += 1;
            continue;
        }
        if meta.is_dir() {
            if !skip_directory(&entry.file_name().to_string_lossy()) {
                collect_source_files(root, &path, depth + 1, files, bytes, skipped_links)?;
            }
        } else if meta.is_file() && is_source_file(&path) {
            if files.len() >= MAX_TREE_FILES {
                bail!("Source comparison exceeds the 10,000 file limit");
            }
            *bytes = bytes
                .checked_add(meta.len())
                .context("Source comparison size overflow")?;
            if *bytes > MAX_TREE_BYTES {
                bail!("Source comparison exceeds the 256 MiB limit per tree");
            }
            let name = relative_name(root, &path)?;
            let file = fingerprint(&path, name.clone(), MAX_SOURCE_FILE_BYTES)?;
            files.insert(name, file);
        }
    }
    Ok(())
}

/// Compare bounded, regular source/assets files. Build products and credential
/// directories are excluded; links are never followed. Results are sorted.
pub fn compare_kernels(left: &Path, right: &Path) -> Result<KernelComparison> {
    let left_path = absolute_path(left)?;
    let right_path = absolute_path(right)?;
    if !left_path.is_dir() || !right_path.is_dir() {
        bail!("Kernel source comparison requires two existing directories");
    }
    let mut left = inspect_kernel(&left_path, None)?;
    let mut right = inspect_kernel(&right_path, None)?;
    let mut left_files = BTreeMap::new();
    let mut right_files = BTreeMap::new();
    for (path, report, files) in [
        (&left_path, &mut left, &mut left_files),
        (&right_path, &mut right, &mut right_files),
    ] {
        let mut bytes = 0;
        let mut skipped_links = 0;
        collect_source_files(path, path, 0, files, &mut bytes, &mut skipped_links)?;
        report.diagnostics.push(message_diagnostic(
            "comparison_scope",
            "info",
            "Bounded source comparison",
            "diagnostic.comparison_scope.body",
            [
                ("count", files.len().to_string()),
                ("bytes", bytes.to_string()),
                ("skippedLinks", skipped_links.to_string()),
            ],
        ));
    }
    let mut added = Vec::new();
    let mut removed = Vec::new();
    let mut changed = Vec::new();
    let mut identical = 0;
    for (name, file) in &left_files {
        match right_files.get(name) {
            None => removed.push(name.clone()),
            Some(other) if file.sha256 == other.sha256 => identical += 1,
            Some(_) => changed.push(name.clone()),
        }
    }
    for name in right_files.keys() {
        if !left_files.contains_key(name) {
            added.push(name.clone());
        }
    }
    Ok(KernelComparison {
        left,
        right,
        added,
        removed,
        changed,
        identical,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let name = format!(
                "stn-kernel-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            );
            let path = std::env::temp_dir().join(name);
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        fn write(&self, relative: &str, bytes: impl AsRef<[u8]>) -> PathBuf {
            let path = self.0.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, bytes).unwrap();
            path
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn minimal_pe_fixture() -> Vec<u8> {
        // Only a bounded header fixture, not an executable implementation.
        let mut bytes = vec![0_u8; 256];
        bytes[..2].copy_from_slice(b"MZ");
        bytes[60..64].copy_from_slice(&128_u32.to_le_bytes());
        bytes[128..132].copy_from_slice(b"PE\0\0");
        bytes[132..134].copy_from_slice(&0x8664_u16.to_le_bytes());
        bytes
    }

    #[test]
    fn kernel_installation_detects_dwmapi_enabled_layout() {
        let fixture = Fixture::new();
        for name in [
            "OpenSteamTool.dll",
            "xinput1_4.dll",
            "dwmapi.dll",
            "version.dll.disabled",
        ] {
            fixture.write(name, minimal_pe_fixture());
        }
        let state = detect_kernel_installation(&fixture.0).unwrap();
        assert_eq!(state.status, "enabled");
        assert_eq!(state.injection_method, "dwmapi");
        assert!(state.remnants.is_empty());
        assert_eq!(state.files.len(), 4);
        assert!(state
            .files
            .iter()
            .all(|file| file.sha256.len() == 64 && file.size_bytes == 256));
        assert_eq!(
            state.message_key,
            "diagnostic.kernel_installation.enabled.body"
        );
        let json = serde_json::to_value(&state).unwrap();
        assert_eq!(json["status"], "enabled");
        assert_eq!(json["injectionMethod"], "dwmapi");
        assert_eq!(
            json["messageKey"],
            "diagnostic.kernel_installation.enabled.body"
        );
    }

    #[test]
    fn artifact_manifest_supplies_version_only_when_primary_hash_matches() {
        let fixture = Fixture::new();
        let artifact = fixture.0.join("artifact");
        fs::create_dir_all(&artifact).unwrap();
        let bytes = minimal_pe_fixture();
        fixture.write("OpenSteamTool.dll", &bytes);
        fixture.write("xinput1_4.dll", minimal_pe_fixture());
        fixture.write("dwmapi.dll", minimal_pe_fixture());
        fixture.write("version.dll.disabled", minimal_pe_fixture());
        fs::write(artifact.join("OpenSteamTool.dll"), &bytes).unwrap();
        let mut hasher = Sha256::new();
        hasher.update(&bytes);
        let hash = format!("{:x}", hasher.finalize());
        fs::write(
            artifact.join("BUILD_INFO.json"),
            serde_json::json!({"version":"1.5.4.1","files":[{"name":"OpenSteamTool.dll","sha256":hash}]}).to_string(),
        )
        .unwrap();
        let state = detect_kernel_installation_with_artifact(&fixture.0, Some(&artifact)).unwrap();
        assert_eq!(state.version.as_deref(), Some("1.5.4.1"));
        assert_eq!(state.version_source, "artifactManifest");
        assert_eq!(state.hash_match, Some(true));
        assert_eq!(state.artifact_sha256.as_deref(), Some(hash.as_str()));
    }

    #[test]
    fn kernel_installation_detects_version_enabled_layout() {
        let fixture = Fixture::new();
        for name in [
            "OpenSteamTool.dll",
            "xinput1_4.dll",
            "version.dll",
            "dwmapi.dll.disabled",
        ] {
            fixture.write(name, minimal_pe_fixture());
        }
        let state = detect_kernel_installation(&fixture.0).unwrap();
        assert_eq!(state.status, "enabled");
        assert_eq!(state.injection_method, "version");
        assert!(state.remnants.is_empty());
    }

    #[test]
    fn kernel_installation_detects_disabled_layout() {
        let fixture = Fixture::new();
        for name in [
            "OpenSteamTool.dll.off",
            "xinput1_4.dll.off",
            "dwmapi.dll.off",
            "version.dll.disabled",
        ] {
            fixture.write(name, minimal_pe_fixture());
        }
        let state = detect_kernel_installation(&fixture.0).unwrap();
        assert_eq!(state.status, "disabled");
        assert_eq!(state.injection_method, "dwmapi");
        assert!(state.remnants.is_empty());
        assert!(detect_installed_kernel(&fixture.0).unwrap().is_some());
    }

    #[test]
    fn kernel_installation_detects_both_entry_conflict() {
        let fixture = Fixture::new();
        for name in [
            "OpenSteamTool.dll",
            "xinput1_4.dll",
            "dwmapi.dll",
            "version.dll",
        ] {
            fixture.write(name, minimal_pe_fixture());
        }
        let state = detect_kernel_installation(&fixture.0).unwrap();
        assert_eq!(state.status, "conflict");
        assert_eq!(state.injection_method, "both");
        assert_eq!(
            state.message_key,
            "diagnostic.kernel_installation.conflict.body"
        );
    }

    #[test]
    fn kernel_installation_detects_incomplete_layout() {
        let fixture = Fixture::new();
        fixture.write("OpenSteamTool.dll", minimal_pe_fixture());
        fixture.write("dwmapi.dll", minimal_pe_fixture());
        let state = detect_kernel_installation(&fixture.0).unwrap();
        assert_eq!(state.status, "incomplete");
        assert_eq!(state.injection_method, "dwmapi");
        assert_eq!(
            state.remnants,
            vec!["OpenSteamTool.dll".to_owned(), "dwmapi.dll".to_owned()]
        );
    }

    #[test]
    fn kernel_installation_keeps_uninstalled_remnants_read_only() {
        let fixture = Fixture::new();
        fixture.write("bin/dwmapi.dll", minimal_pe_fixture());
        fixture.write("version.dll.disabled", minimal_pe_fixture());
        let state = detect_kernel_installation(&fixture.0).unwrap();
        assert_eq!(state.status, "notInstalled");
        assert_eq!(state.injection_method, "none");
        assert_eq!(
            state.remnants,
            vec![
                "bin/dwmapi.dll".to_owned(),
                "version.dll.disabled".to_owned()
            ]
        );
        assert!(state
            .diagnostics
            .iter()
            .any(|item| item.code == "kernel_installation_remnants"));
        assert!(state
            .diagnostics
            .iter()
            .any(|item| item.code == "kernel_installation_not_installed"));
        assert!(detect_installed_kernel(&fixture.0).unwrap().is_none());
        assert!(fixture.0.join("bin/dwmapi.dll").is_file());
    }

    #[test]
    fn kernel_installation_not_installed_code_is_stable_for_empty_and_missing_roots() {
        let fixture = Fixture::new();
        for root in [&fixture.0, &fixture.0.join("missing")] {
            let state = detect_kernel_installation(root).unwrap();
            assert_eq!(state.status, "notInstalled");
            assert!(state
                .diagnostics
                .iter()
                .any(|item| item.code == "kernel_installation_not_installed"));
            assert!(!state
                .diagnostics
                .iter()
                .any(|item| item.code == "kernel_installation_notInstalled"));
        }
    }

    #[test]
    fn installed_detection_does_not_confuse_source_config_or_proxy_dlls() {
        let fixture = Fixture::new();
        fixture.write(
            "src/CMakeLists.txt",
            "project(OpenSteamTool VERSION 9.8.7 LANGUAGES CXX)\n",
        );
        fixture.write("opensteamtool.example.toml", "[log]\nlevel = 'info'\n");
        assert!(detect_installed_kernel(&fixture.0).unwrap().is_none());
        fixture.write("dwmapi.dll", minimal_pe_fixture());
        fixture.write("version.dll", minimal_pe_fixture());
        fixture.write("opensteamtool.toml", "[log]\nlevel = 'info'\n");
        assert!(detect_installed_kernel(&fixture.0).unwrap().is_none());
        fs::create_dir(fixture.0.join("OpenSteamTool.dll")).unwrap();
        assert!(detect_installed_kernel(&fixture.0).unwrap().is_none());
    }

    #[test]
    fn installed_primary_dll_without_version_never_inherits_source_version() {
        let fixture = Fixture::new();
        fixture.write(
            "src/CMakeLists.txt",
            "project(OpenSteamTool VERSION 9.8.7 LANGUAGES CXX)\n",
        );
        fixture.write("opensteamtool.example.toml", "[log]\nlevel = 'info'\n");
        fixture.write("OpenSteamTool.dll", minimal_pe_fixture());
        let report = detect_installed_kernel(&fixture.0).unwrap().unwrap();
        assert_eq!(report.kind, "installed");
        assert_eq!(report.version, None);
        assert!(report
            .files
            .iter()
            .any(|file| file.name == "OpenSteamTool.dll"));
        assert!(report
            .diagnostics
            .iter()
            .any(|item| item.code == "installed_version_unknown"));
        assert!(!report
            .diagnostics
            .iter()
            .any(|item| item.code == "source_version"));
        assert!(report
            .diagnostics
            .iter()
            .any(|item| item.code == "binary_architecture" && item.detail.contains("x86-64")));
        assert!(report
            .diagnostics
            .iter()
            .any(|item| item.code == "runtime_unverified"));
    }

    #[test]
    fn installed_detection_rejects_nonlocal_namespaces_before_access() {
        for path in [
            r"\\server\share\Steam",
            r"\\?\UNC\server\share\Steam",
            r"\\.\PhysicalDrive0",
            r"\\?\GLOBALROOT\Device\HarddiskVolume1\Steam",
            r"\??\C:\Steam",
        ] {
            assert!(detect_installed_kernel(Path::new(path)).is_err());
        }
        let fixture = Fixture::new();
        assert!(detect_installed_kernel(&fixture.0.join("missing"))
            .unwrap()
            .is_none());
        let file = fixture.write("not-directory.toml", "");
        assert!(detect_installed_kernel(&file).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn installed_detection_accepts_canonical_local_directory() {
        let fixture = Fixture::new();
        fixture.write("OpenSteamTool.dll", minimal_pe_fixture());
        let canonical = fs::canonicalize(&fixture.0).unwrap();
        let report = detect_installed_kernel(&canonical).unwrap().unwrap();
        assert_eq!(report.kind, "installed");
        assert!(report.version.is_none());
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "Requires Windows symbolic-link creation privilege; run explicitly on a capable test host"]
    fn installed_detection_rejects_windows_reparse_primary_and_root() {
        use std::os::windows::fs::{symlink_dir, symlink_file};
        let fixture = Fixture::new();
        let target = Fixture::new();
        let binary = target.write("OpenSteamTool.dll", minimal_pe_fixture());
        symlink_file(&binary, fixture.0.join("OpenSteamTool.dll")).unwrap();
        assert!(detect_installed_kernel(&fixture.0).is_err());
        symlink_dir(&target.0, fixture.0.join("linked-root")).unwrap();
        assert!(detect_installed_kernel(&fixture.0.join("linked-root")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn installed_detection_rejects_symlinked_primary_and_root() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new();
        let target = Fixture::new();
        let binary = target.write("OpenSteamTool.dll", minimal_pe_fixture());
        symlink(&binary, fixture.0.join("OpenSteamTool.dll")).unwrap();
        assert!(detect_installed_kernel(&fixture.0).is_err());
        symlink(&target.0, fixture.0.join("linked-root")).unwrap();
        assert!(detect_installed_kernel(&fixture.0.join("linked-root")).is_err());
    }

    #[test]
    fn source_version_is_declaration_not_folder_name() {
        let fixture = Fixture::new();
        fixture.write(
            "src/CMakeLists.txt",
            "project(OpenSteamTool VERSION 1.5.4.1 LANGUAGES C CXX)\n",
        );
        let report = inspect_kernel(&fixture.0, None).unwrap();
        assert_eq!(report.kind, "source");
        assert_eq!(report.version.as_deref(), Some("1.5.4.1"));
        let version_message = report
            .diagnostics
            .iter()
            .find(|item| item.code == "source_version")
            .unwrap();
        assert_eq!(
            version_message.message_key,
            "diagnostic.source_version.body"
        );
        assert_eq!(
            version_message.message_args["filename"],
            "src/CMakeLists.txt"
        );
        assert_eq!(version_message.message_args["version"], "1.5.4.1");
        assert!(report
            .diagnostics
            .iter()
            .any(|d| d.code == "runtime_unverified"));
        assert_eq!(
            extract_version("cmake_minimum_required(VERSION 3.20)"),
            None
        );
        assert_eq!(
            extract_version(
                "project(OpenSteamTool LANGUAGES CXX)\ncmake_minimum_required(VERSION 3.20)"
            ),
            None
        );
        assert_eq!(
            extract_version("project (\nOpenSteamTool\nVERSION 2.3.4\nLANGUAGES CXX\n)"),
            Some("2.3.4".into())
        );
    }

    #[test]
    fn duplicate_config_never_echoes_source_values() {
        let fixture = Fixture::new();
        let file = fixture.write(
            "opensteamtool.toml",
            "[manifest]\nurl = 'DUMMY_SECRET_DO_NOT_EXPORT'\nurl = 'second'\n",
        );
        let report = inspect_kernel(&file, None).unwrap();
        assert!(!report.config.as_ref().unwrap().valid);
        let config = report.config.as_ref().unwrap();
        assert_eq!(config.error_key, "diagnostic.config_invalid.duplicate");
        assert_eq!(config.error_args["line"], "3");
        assert!(config.error_args["column"].parse::<usize>().unwrap() >= 1);
        let diagnostic = report
            .diagnostics
            .iter()
            .find(|item| item.code == "config_invalid")
            .unwrap();
        assert_eq!(diagnostic.message_key, config.error_key);
        assert_eq!(diagnostic.message_args, config.error_args);
        assert!(report
            .config
            .as_ref()
            .unwrap()
            .error
            .as_ref()
            .unwrap()
            .contains("Duplicate"));
        let json = serde_json::to_string(&report).unwrap();
        assert!(!json.contains("DUMMY_SECRET"));
        assert!(!json.contains("second"));
    }

    #[test]
    fn syntax_error_only_exposes_location() {
        let fixture = Fixture::new();
        let file = fixture.write(
            "invalid.toml",
            "[remote]\nurl_template = DUMMY_SECRET_DO_NOT_EXPORT\n",
        );
        let json = serde_json::to_string(&inspect_kernel(&file, None).unwrap()).unwrap();
        assert!(json.contains("Invalid TOML syntax"));
        assert!(!json.contains("DUMMY_SECRET"));
    }

    #[test]
    fn config_values_are_not_serialized() {
        let fixture = Fixture::new();
        let file = fixture.write("settings.toml", "[remote]\nurl_template = 'https://example.invalid/TOKEN/{channel}/{component}/{sha256}'\n[custom]\npassword = 'DUMMY_SECRET_DO_NOT_EXPORT'\n");
        let report = inspect_kernel(&file, None).unwrap();
        assert!(report.config.as_ref().unwrap().valid);
        assert_eq!(
            report.config.as_ref().unwrap().unknown_sections,
            vec!["custom"]
        );
        let json = serde_json::to_string(&report).unwrap();
        assert!(!json.contains("DUMMY_SECRET"));
        assert!(!json.contains("example.invalid"));
        assert!(!json.contains("TOKEN"));
    }

    #[test]
    fn structured_error_causes_never_use_raw_error_text_as_arguments() {
        let raw = anyhow::Error::new(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "DUMMY_SECRET_DO_NOT_EXPORT",
        ));
        let message = issue_diagnostic(
            "file_unreadable",
            "warning",
            "File fingerprint unavailable",
            "diagnostic.file_unreadable.body",
            &issue_for_error(&raw),
            [("filename", "safe.dll".into())],
        );
        assert_eq!(
            message.message_args["reasonKey"],
            "diagnostic.reason.io_permission_denied"
        );
        assert_eq!(message.message_args["filename"], "safe.dll");
        assert!(!serde_json::to_string(&message)
            .unwrap()
            .contains("DUMMY_SECRET"));
        assert!(message.detail.contains("safe.dll"));
        assert!(message.detail.contains("denied"));

        let fixture = Fixture::new();
        let path = fixture.write("invalid-utf8.toml", [0xff_u8]);
        let report = inspect_kernel(&path, None).unwrap();
        let config = report.config.unwrap();
        assert_eq!(config.error_key, "diagnostic.config_unreadable.body");
        assert_eq!(
            config.error_args["reasonKey"],
            "diagnostic.reason.text_invalid_utf8"
        );
    }

    #[test]
    fn diagnostic_and_config_fields_accept_legacy_json() {
        let old_diagnostic = serde_json::json!({"code":"old", "severity":"info", "title":"Old", "detail":"Original technical evidence"});
        let diagnostic: Diagnostic = serde_json::from_value(old_diagnostic).unwrap();
        assert!(diagnostic.message_key.is_empty());
        assert!(diagnostic.message_args.is_empty());
        assert_eq!(diagnostic.detail, "Original technical evidence");
        let old_config = serde_json::json!({"path":"config.toml", "valid":false, "sections":[], "unknownSections":[], "error":"Old error"});
        let config: ConfigSummary = serde_json::from_value(old_config).unwrap();
        assert!(config.error_key.is_empty());
        assert!(config.error_args.is_empty());
        assert_eq!(config.error.as_deref(), Some("Old error"));
    }

    #[test]
    fn structured_interpolation_is_single_pass_and_serializes_camel_case() {
        let message = message_diagnostic(
            "source_version",
            "success",
            "Source version identified",
            "diagnostic.source_version.body",
            [
                ("filename", "dir/{version}/CMakeLists.txt".into()),
                ("version", "7.8.9".into()),
            ],
        );
        assert!(message
            .detail
            .starts_with("dir/{version}/CMakeLists.txt declares OpenSteamTool 7.8.9"));
        let json = serde_json::to_value(&message).unwrap();
        assert_eq!(json["messageKey"], "diagnostic.source_version.body");
        assert_eq!(json["messageArgs"]["version"], "7.8.9");
        assert!(json.get("message_key").is_none());
    }

    #[test]
    fn dynamic_configuration_findings_preserve_expected_and_missing_parameters() {
        let fixture = Fixture::new();
        let file = fixture.write("schema.toml", "[remote]\nurl_template='https://example.invalid/DUMMY_SECRET_DO_NOT_EXPORT/{channel}'\norder='DUMMY_SECRET_DO_NOT_EXPORT'\n[manifest]\ntimeout_recv_ms=0\nfailover='DUMMY_SECRET_DO_NOT_EXPORT'\n");
        let report = inspect_kernel(&file, None).unwrap();
        let missing = report
            .diagnostics
            .iter()
            .find(|item| item.code == "config_template_placeholders")
            .unwrap();
        assert_eq!(missing.message_args["field"], "remote.url_template");
        assert_eq!(missing.message_args["missing"], "{component}, {sha256}");
        let kind = report
            .diagnostics
            .iter()
            .find(|item| item.code == "config_type_mismatch")
            .unwrap();
        assert_eq!(kind.message_args["expected"], "bool");
        let range = report
            .diagnostics
            .iter()
            .find(|item| item.code == "config_timeout_range")
            .unwrap();
        assert_eq!(range.message_args["minimum"], "1");
        assert_eq!(range.message_args["maximum"], "15000");
        assert!(!serde_json::to_string(&report)
            .unwrap()
            .contains("DUMMY_SECRET"));
        for item in &report.diagnostics {
            assert!(
                !item.message_key.is_empty(),
                "Every new diagnostic must carry a message key"
            );
            assert_ne!(
                message_template(&item.message_key),
                "Detailed localized information is unavailable."
            );
            if let Some(reason_key) = item.message_args.get("reasonKey") {
                assert!(reason_key.starts_with("diagnostic.reason."));
                assert_ne!(
                    message_template(reason_key),
                    "Detailed localized information is unavailable."
                );
            }
        }
    }

    #[test]
    fn lua_scanner_skips_strings_comments_and_expressions() {
        let source = r#"
-- addappid(111)
--[=[ addappid(222) ]=]
local x = "addappid(333)"
local y = [==[addappid(444)]==]
addappid(480, 1, "DUMMY_SECRET_DO_NOT_EXPORT")
addappid -- comment
( -- comment
  570)
addappid(480)
addappid(600 + 1)
addappid(variable)
addappid(4294967296)
setAppTicket(480, "DUMMY_SECRET_DO_NOT_EXPORT")
os.execute("DUMMY_MUST_NOT_RUN")
"#;
        let (ids, dynamic, credentials) = literal_app_ids(source);
        assert_eq!(ids, vec![480, 570]);
        assert!(dynamic && credentials);
        let fixture = Fixture::new();
        let file = fixture.write("input.lua", source);
        let report = inspect_kernel(&file, None).unwrap();
        let json = serde_json::to_string(&report).unwrap();
        assert!(!json.contains("DUMMY_SECRET"));
        assert!(!json.contains("DUMMY_MUST_NOT_RUN"));
    }

    #[test]
    fn lua_unicode_never_breaks_identifier_slices() {
        let (ids, _, _) =
            literal_app_ids("-- 日本語\nlocal 文 = '文本'; addappid(123) -- 한국어\n");
        assert_eq!(ids, vec![123]);
    }

    #[test]
    fn metadata_validation_requires_meaningful_entries() {
        assert!(!valid_pattern_metadata(&"[empty]".parse().unwrap()));
        assert!(valid_pattern_metadata(
            &"['0x1234']\nrva='0x24'\n".parse().unwrap()
        ));
        assert!(!valid_pattern_metadata(
            &"['0x1234']\nsig='garbage'\n".parse().unwrap()
        ));
        assert!(!valid_ipc_metadata(
            &"[IClientUser]\n[IClientUtils]\n".parse().unwrap()
        ));
        assert!(valid_ipc_metadata(
            &"[IClientUser.Test]\nfuncHash='0xABCD'\n[IClientUtils.Test]\nfuncHash='1234'\n"
                .parse()
                .unwrap()
        ));
    }

    #[test]
    fn local_hash_metadata_match_does_not_claim_runtime() {
        let fixture = Fixture::new();
        let client = fixture.write("steamclient64.dll", "synthetic client fixture");
        let ui = fixture.write("steamui.dll", "synthetic UI fixture");
        let client_hash = fingerprint(&client, "client".into(), MAX_BINARY_BYTES)
            .unwrap()
            .sha256;
        let ui_hash = fingerprint(&ui, "ui".into(), MAX_BINARY_BYTES)
            .unwrap()
            .sha256;
        fixture.write(
            &format!("opensteamtool/pattern/steamclient/{client_hash}.toml"),
            "['0x1']\nrva='0x24'\n",
        );
        fixture.write(
            &format!("opensteamtool/pattern/steamui/{ui_hash}.toml"),
            "['0x2']\nsig='AA ?? BB'\n",
        );
        fixture.write(
            &format!("opensteamtool/ipc/steamclient/{client_hash}.toml"),
            "[IClientUser.Method]\nfuncHash='AA'\n[IClientUtils.Method]\nfuncHash='BB'\n",
        );
        let report = inspect_kernel(&fixture.0, Some(&fixture.0)).unwrap();
        assert_eq!(
            report
                .diagnostics
                .iter()
                .filter(|d| d.code == "metadata_match")
                .count(),
            3
        );
        assert!(report
            .diagnostics
            .iter()
            .any(|d| d.code == "runtime_unverified" && d.severity == "warning"));
    }

    #[test]
    fn source_comparison_is_sorted_bounded_and_excludes_builds() {
        let left = Fixture::new();
        let right = Fixture::new();
        left.write("src/same.cpp", "same");
        right.write("src/same.cpp", "same");
        left.write("src/change.cpp", "old");
        right.write("src/change.cpp", "new");
        left.write("src/removed.h", "old");
        right.write("src/added.h", "new");
        left.write("build/ignore.cpp", "different");
        left.write("userdata/account/secret.txt", "DUMMY_SECRET_DO_NOT_EXPORT");
        let result = compare_kernels(&left.0, &right.0).unwrap();
        assert_eq!(result.identical, 1);
        assert_eq!(result.added, vec!["src/added.h"]);
        assert_eq!(result.removed, vec!["src/removed.h"]);
        assert_eq!(result.changed, vec!["src/change.cpp"]);
    }

    #[test]
    fn traversal_and_account_paths_are_rejected() {
        assert!(inspect_kernel(Path::new("source/../userdata"), None).is_err());
        assert!(inspect_kernel(Path::new("userdata/file.lua"), None).is_err());
        assert!(inspect_kernel(Path::new("loginusers.vdf"), None).is_err());
        assert!(inspect_kernel(Path::new(""), None).is_err());
        assert!(inspect_kernel(Path::new(r"\\server\share\sample.lua"), None).is_err());
        for path in [
            r"\\?\UNC\server\share\sample.lua",
            r"\\.\PhysicalDrive0",
            r"\\?\GLOBALROOT\Device\HarddiskVolume1\sample.lua",
            r"\??\C:\sample.lua",
        ] {
            assert!(
                forbidden_path_namespace(Path::new(path)),
                "namespace must be rejected before filesystem access"
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn canonicalized_verbatim_local_paths_are_accepted() {
        let left = Fixture::new();
        let right = Fixture::new();
        for fixture in [&left, &right] {
            fixture.write(
                "src/CMakeLists.txt",
                "project(OpenSteamTool VERSION 1.5.4.1 LANGUAGES CXX)\n",
            );
        }
        let left_path = fs::canonicalize(&left.0).unwrap();
        let right_path = fs::canonicalize(&right.0).unwrap();
        assert!(
            matches!(left_path.components().next(), Some(Component::Prefix(prefix)) if matches!(prefix.kind(), std::path::Prefix::VerbatimDisk(_)))
        );
        assert!(!forbidden_path_namespace(&left_path));
        let report = inspect_kernel(&left_path, None).unwrap();
        assert_eq!(report.kind, "source");
        assert_eq!(report.version.as_deref(), Some("1.5.4.1"));
        let comparison = compare_kernels(&left_path, &right_path).unwrap();
        assert_eq!(comparison.identical, 1);
        assert!(comparison.changed.is_empty());
    }

    #[test]
    fn absent_path_is_not_a_successful_installation() {
        let fixture = Fixture::new();
        let report = inspect_kernel(&fixture.0.join("missing"), None).unwrap();
        assert_eq!(report.kind, "absent");
        assert!(report
            .diagnostics
            .iter()
            .any(|d| d.code == "kernel_missing"));
    }

    #[test]
    fn oversized_text_is_not_parsed() {
        let fixture = Fixture::new();
        let file = fixture.write("large.toml", vec![b' '; MAX_TEXT_BYTES as usize + 1]);
        let report = inspect_kernel(&file, None).unwrap();
        assert!(!report.config.unwrap().valid);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_is_not_followed() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new();
        let outside = Fixture::new();
        outside.write("secret.lua", "addappid(999)");
        symlink(&outside.0, fixture.0.join("alias")).unwrap();
        assert!(inspect_kernel(&fixture.0.join("alias/secret.lua"), None).is_err());
        let result = compare_kernels(&fixture.0, &outside.0).unwrap();
        assert!(result
            .left
            .diagnostics
            .iter()
            .any(|d| d.code == "comparison_scope" && d.detail.contains("1 linked paths")));
    }
}
