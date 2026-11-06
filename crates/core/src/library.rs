//! OpenSteamTool Lua library management.
//!
//! The public functions in this module deliberately concentrate every Steam
//! filesystem mutation behind a small interface.  Writes are staged beside the
//! destination, existing files are moved to a recoverable backup, and linked or
//! reparse-point paths are rejected before use.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_LUA_BYTES: u64 = 4 * 1024 * 1024;
const MAX_IMPORT_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_IMPORT_FILES: usize = 10_000;
const MAX_IMPORT_DEPTH: usize = 16;
const REPARSE_POINT_ATTRIBUTE: u32 = 0x0400;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct LuaDirectoryLayout {
    pub steam_root: String,
    pub lua_root: String,
    pub disabled_root: String,
    pub configured_paths: Vec<String>,
    /// `opensteamtool.toml` or `default`.
    pub source: String,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ParsedLuaManifest {
    pub app_ids: Vec<u32>,
    pub primary_app_id: Option<u32>,
    pub pinned_manifest_count: usize,
    pub pinned_depot_ids: Vec<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct LuaLibraryEntry {
    pub app_id: u32,
    pub file_name: String,
    pub enabled: bool,
    pub conflict: bool,
    pub active_path: Option<String>,
    pub disabled_path: Option<String>,
    pub size_bytes: u64,
    pub sha256: String,
    pub app_ids: Vec<u32>,
    pub pinned_manifest_count: usize,
    pub pinned_depot_ids: Vec<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct LuaLibrarySnapshot {
    pub layout: LuaDirectoryLayout,
    pub entries: Vec<LuaLibraryEntry>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ManagedWrite {
    pub destination: String,
    pub backup_path: Option<String>,
    pub size_bytes: u64,
    pub sha256: String,
    pub replaced: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct LuaMutationReceipt {
    pub app_id: u32,
    pub action: String,
    pub source: Option<String>,
    pub destination: Option<String>,
    pub backup_paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct LibraryImportRequest {
    pub steam_root: PathBuf,
    pub sources: Vec<PathBuf>,
    #[serde(default)]
    pub overwrite: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ImportIssue {
    pub path: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ImportSummary {
    pub lua_count: usize,
    pub bin_count: usize,
    pub manifest_count: usize,
    pub imported: Vec<ManagedWrite>,
    pub skipped: Vec<ImportIssue>,
    pub failed: Vec<ImportIssue>,
}

#[derive(Debug, Clone)]
struct ScannedFile {
    app_id: u32,
    enabled: bool,
    path: PathBuf,
    size_bytes: u64,
    sha256: String,
    parsed: ParsedLuaManifest,
}

/// Resolve OpenSteamTool's active Lua directory.
///
/// Relative entries in `[lua].paths` are rooted at the Steam directory.  The
/// first configured directory that already exists wins; when none exists, the
/// first syntactically valid configured path is selected.  Invalid TOML falls
/// back to `<Steam>/config/lua` with a warning rather than making the library
/// unusable.
pub fn resolve_lua_layout(steam_root: &Path) -> Result<LuaDirectoryLayout> {
    let steam_root = absolute_lexical(steam_root)?;
    require_directory_if_present(&steam_root, "Steam root")?;
    reject_linked_ancestors(&steam_root)?;

    let config_path = steam_root.join("opensteamtool.toml");
    let mut warnings = Vec::new();
    let mut configured = Vec::new();
    if config_path.exists() {
        require_regular_file(&config_path, "OpenSteamTool configuration")?;
        match read_utf8_limited(&config_path, MAX_LUA_BYTES) {
            Ok(text) => match text.parse::<toml::Value>() {
                Ok(root) => match root
                    .get("lua")
                    .and_then(|value| value.get("paths"))
                {
                    None => {}
                    Some(toml::Value::Array(values)) => {
                        for value in values {
                            match value.as_str().map(str::trim) {
                                Some("") | None => warnings.push(
                                    "Ignored an empty or non-string entry in [lua].paths.".into(),
                                ),
                                Some(value) => match configured_lua_path(&steam_root, value) {
                                    Ok(path) => configured.push(path),
                                    Err(error) => warnings.push(format!(
                                        "Ignored invalid [lua].paths entry: {error:#}"
                                    )),
                                },
                            }
                        }
                    }
                    Some(_) => warnings.push(
                        "[lua].paths is not an array; using the default Lua directory.".into(),
                    ),
                },
                Err(error) => warnings.push(format!(
                    "opensteamtool.toml could not be parsed; using the default Lua directory: {error}"
                )),
            },
            Err(error) => warnings.push(format!(
                "opensteamtool.toml could not be read; using the default Lua directory: {error:#}"
            )),
        }
    }

    let mut seen_paths = BTreeSet::new();
    configured.retain(|path| seen_paths.insert(path.clone()));
    let selected = configured
        .iter()
        .find(|path| path.is_dir())
        .cloned()
        .or_else(|| configured.first().cloned())
        .unwrap_or_else(|| steam_root.join("config").join("lua"));
    reject_linked_ancestors(&selected)?;
    require_directory_if_present(&selected, "Lua root")?;
    let source = if configured.is_empty() {
        "default"
    } else {
        "opensteamtool.toml"
    };
    Ok(LuaDirectoryLayout {
        steam_root: display_path(&steam_root),
        lua_root: display_path(&selected),
        disabled_root: display_path(&selected.join("Disable")),
        configured_paths: configured.iter().map(|path| display_path(path)).collect(),
        source: source.into(),
        warnings,
    })
}

/// Scan enabled and disabled Lua entries without executing Lua.
pub fn scan_lua_library(steam_root: &Path) -> Result<LuaLibrarySnapshot> {
    let layout = resolve_lua_layout(steam_root)?;
    let lua_root = PathBuf::from(&layout.lua_root);
    let disabled_root = PathBuf::from(&layout.disabled_root);
    let mut warnings = layout.warnings.clone();
    let mut scanned = Vec::new();
    scan_lua_directory(&lua_root, true, &mut scanned, &mut warnings)?;
    scan_lua_directory(&disabled_root, false, &mut scanned, &mut warnings)?;

    let mut grouped: BTreeMap<u32, Vec<ScannedFile>> = BTreeMap::new();
    for file in scanned {
        grouped.entry(file.app_id).or_default().push(file);
    }
    let mut entries = Vec::new();
    for (app_id, mut files) in grouped {
        files.sort_by(|left, right| {
            right
                .enabled
                .cmp(&left.enabled)
                .then_with(|| left.path.cmp(&right.path))
        });
        let active = files.iter().find(|file| file.enabled);
        let disabled = files.iter().find(|file| !file.enabled);
        let representative = active.or(disabled).expect("group contains a file");
        let mut app_ids = BTreeSet::new();
        let mut pinned_depots = BTreeSet::new();
        let mut pinned_manifest_count = 0;
        let mut size_bytes = 0u64;
        for file in &files {
            size_bytes = size_bytes.saturating_add(file.size_bytes);
            app_ids.extend(file.parsed.app_ids.iter().copied());
            pinned_depots.extend(file.parsed.pinned_depot_ids.iter().copied());
            pinned_manifest_count += file.parsed.pinned_manifest_count;
        }
        entries.push(LuaLibraryEntry {
            app_id,
            file_name: format!("{app_id}.lua"),
            enabled: active.is_some(),
            conflict: active.is_some() && disabled.is_some(),
            active_path: active.map(|file| display_path(&file.path)),
            disabled_path: disabled.map(|file| display_path(&file.path)),
            size_bytes,
            sha256: representative.sha256.clone(),
            app_ids: app_ids.into_iter().collect(),
            pinned_manifest_count,
            pinned_depot_ids: pinned_depots.into_iter().collect(),
        });
    }
    Ok(LuaLibrarySnapshot {
        layout,
        entries,
        warnings,
    })
}

/// Parse the limited declarative calls consumed by OpenSteamTool.
pub fn parse_lua_manifest(text: &str) -> ParsedLuaManifest {
    let mut app_ids = BTreeSet::new();
    let mut primary_app_id = None;
    let mut pinned_depots = BTreeSet::new();
    let mut pinned_manifest_count = 0;
    for line in text.lines() {
        let line = strip_lua_comment(line);
        if let Some(arguments) = call_arguments(&line, "addappid") {
            if let Some(value) =
                first_u64_argument(arguments).and_then(|value| u32::try_from(value).ok())
            {
                if value != 0 {
                    primary_app_id.get_or_insert(value);
                    app_ids.insert(value);
                }
            }
        }
        if let Some(arguments) = call_arguments(&line, "setManifestid") {
            if let Some(value) =
                first_u64_argument(arguments).and_then(|value| u32::try_from(value).ok())
            {
                if value != 0 {
                    pinned_depots.insert(value);
                    pinned_manifest_count += 1;
                }
            }
        }
    }
    let app_ids: Vec<_> = app_ids.into_iter().collect();
    ParsedLuaManifest {
        primary_app_id,
        app_ids,
        pinned_manifest_count,
        pinned_depot_ids: pinned_depots.into_iter().collect(),
    }
}

/// Create or replace `<lua-root>/<appid>.lua`.
///
/// A custom document must contain `addappid(APP_ID)` for the requested primary
/// AppID.  Passing `None` produces a deterministic minimal document.
pub fn create_lua_entry(
    steam_root: &Path,
    app_id: u32,
    custom_lua: Option<&str>,
    overwrite: bool,
) -> Result<ManagedWrite> {
    validate_app_id(app_id)?;
    let layout = resolve_lua_layout(steam_root)?;
    let lua_root = PathBuf::from(layout.lua_root);
    create_directory_checked(&lua_root)?;
    let document = match custom_lua {
        Some(value) => {
            validate_lua_document(value)?;
            let parsed = parse_lua_manifest(value);
            if !parsed.app_ids.contains(&app_id) {
                bail!("custom Lua does not contain addappid({app_id})");
            }
            normalize_text_file(value)
        }
        None => format!("-- Managed by SteamTools Next\r\naddappid({app_id})\r\n"),
    };
    atomic_write(
        &lua_root.join(format!("{app_id}.lua")),
        document.as_bytes(),
        overwrite,
    )
}

/// Import `.lua`, `.bin` and `.manifest` files from files or directories.
///
/// Directory imports are recursive but bounded.  Paths are flattened to their
/// file names, matching Fluent Steam Lua's import behavior.  Archives should be
/// unpacked by the caller into a temporary directory before using this function.
pub fn import_library_files(request: &LibraryImportRequest) -> Result<ImportSummary> {
    if request.sources.is_empty() {
        bail!("at least one import source is required");
    }
    let steam_root = absolute_lexical(&request.steam_root)?;
    require_directory_if_present(&steam_root, "Steam root")?;
    reject_linked_ancestors(&steam_root)?;
    let layout = resolve_lua_layout(&steam_root)?;
    let lua_root = PathBuf::from(layout.lua_root);
    let bin_root = steam_root.join("appcache").join("stats");
    let manifest_root = steam_root.join("depotcache");
    let mut summary = ImportSummary {
        lua_count: 0,
        bin_count: 0,
        manifest_count: 0,
        imported: Vec::new(),
        skipped: Vec::new(),
        failed: Vec::new(),
    };
    let mut candidates = Vec::new();
    let mut total_bytes = 0u64;
    for source in &request.sources {
        if let Err(error) = collect_import_candidates(
            source,
            0,
            &mut candidates,
            &mut total_bytes,
            &mut summary.skipped,
        ) {
            summary.failed.push(ImportIssue {
                path: display_path(source),
                reason: format!("{error:#}"),
            });
        }
    }
    candidates.sort();
    candidates.dedup();
    for source in candidates {
        let extension = source
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        let Some(file_name) = source.file_name() else {
            summary.failed.push(ImportIssue {
                path: display_path(&source),
                reason: "source has no file name".into(),
            });
            continue;
        };
        if !safe_file_name(file_name) {
            summary.failed.push(ImportIssue {
                path: display_path(&source),
                reason: "source file name is not safe to flatten".into(),
            });
            continue;
        }
        let target_root = match extension.as_str() {
            "lua" => &lua_root,
            "bin" => &bin_root,
            "manifest" => &manifest_root,
            _ => {
                summary.skipped.push(ImportIssue {
                    path: display_path(&source),
                    reason: "unsupported extension".into(),
                });
                continue;
            }
        };
        if let Err(error) = create_directory_checked(target_root) {
            summary.failed.push(ImportIssue {
                path: display_path(target_root),
                reason: format!("{error:#}"),
            });
            continue;
        }
        let destination = target_root.join(file_name);
        if same_existing_file(&source, &destination).unwrap_or(false) {
            summary.skipped.push(ImportIssue {
                path: display_path(&source),
                reason: "source is already the destination".into(),
            });
            continue;
        }
        match read_binary_limited(&source, MAX_IMPORT_BYTES)
            .and_then(|bytes| atomic_write(&destination, &bytes, request.overwrite))
        {
            Ok(write) => {
                match extension.as_str() {
                    "lua" => summary.lua_count += 1,
                    "bin" => summary.bin_count += 1,
                    "manifest" => summary.manifest_count += 1,
                    _ => unreachable!(),
                }
                summary.imported.push(write);
            }
            Err(error) => summary.failed.push(ImportIssue {
                path: display_path(&source),
                reason: format!("{error:#}"),
            }),
        }
    }
    Ok(summary)
}

/// Move an AppID between the active Lua root and `Disable`.
pub fn set_lua_entry_enabled(
    steam_root: &Path,
    app_id: u32,
    enabled: bool,
    overwrite: bool,
) -> Result<LuaMutationReceipt> {
    validate_app_id(app_id)?;
    let layout = resolve_lua_layout(steam_root)?;
    let lua_root = PathBuf::from(layout.lua_root);
    let disabled_root = PathBuf::from(layout.disabled_root);
    create_directory_checked(&lua_root)?;
    create_directory_checked(&disabled_root)?;
    let active = lua_root.join(format!("{app_id}.lua"));
    let disabled = disabled_root.join(format!("{app_id}.lua"));
    let (source, destination, action) = if enabled {
        (&disabled, &active, "enable")
    } else {
        (&active, &disabled, "disable")
    };
    require_regular_file(source, "Lua entry")?;
    reject_linked_ancestors(destination)?;
    let mut backups = Vec::new();
    if destination.exists() {
        if !overwrite {
            bail!("destination already exists: {}", display_path(destination));
        }
        require_regular_file(destination, "existing Lua entry")?;
        let backup = unique_backup_path(destination, "replaced")?;
        create_directory_checked(backup.parent().expect("backup has a parent"))?;
        fs::rename(destination, &backup).with_context(|| {
            format!(
                "could not back up existing destination {}",
                display_path(destination)
            )
        })?;
        backups.push(display_path(&backup));
    }
    if let Err(error) = fs::rename(source, destination) {
        if let Some(backup) = backups.first() {
            let _ = fs::rename(backup, destination);
        }
        return Err(error).with_context(|| {
            format!(
                "could not move {} to {}",
                display_path(source),
                display_path(destination)
            )
        });
    }
    Ok(LuaMutationReceipt {
        app_id,
        action: action.into(),
        source: Some(display_path(source)),
        destination: Some(display_path(destination)),
        backup_paths: backups,
    })
}

/// Remove both active and disabled copies by moving them into recoverable
/// `.stn-backups` storage.
pub fn delete_lua_entry(steam_root: &Path, app_id: u32) -> Result<LuaMutationReceipt> {
    validate_app_id(app_id)?;
    let layout = resolve_lua_layout(steam_root)?;
    let lua_root = PathBuf::from(layout.lua_root);
    let disabled_root = PathBuf::from(layout.disabled_root);
    let candidates = [
        lua_root.join(format!("{app_id}.lua")),
        disabled_root.join(format!("{app_id}.lua")),
    ];
    let mut backups = Vec::new();
    for candidate in candidates {
        if !candidate.exists() {
            continue;
        }
        require_regular_file(&candidate, "Lua entry")?;
        let backup = unique_backup_path(&candidate, "deleted")?;
        create_directory_checked(backup.parent().expect("backup has a parent"))?;
        fs::rename(&candidate, &backup)
            .with_context(|| format!("could not archive {}", display_path(&candidate)))?;
        backups.push(display_path(&backup));
    }
    if backups.is_empty() {
        bail!("Lua entry {app_id} was not found");
    }
    Ok(LuaMutationReceipt {
        app_id,
        action: "delete".into(),
        source: None,
        destination: None,
        backup_paths: backups,
    })
}

fn scan_lua_directory(
    directory: &Path,
    enabled: bool,
    output: &mut Vec<ScannedFile>,
    warnings: &mut Vec<String>,
) -> Result<()> {
    if !directory.exists() {
        return Ok(());
    }
    require_directory_if_present(directory, "Lua directory")?;
    reject_linked_ancestors(directory)?;
    let entries = fs::read_dir(directory)
        .with_context(|| format!("could not list {}", display_path(directory)))?;
    let mut paths = Vec::new();
    for entry in entries {
        match entry {
            Ok(entry) => paths.push(entry.path()),
            Err(error) => warnings.push(format!(
                "Could not inspect an entry in {}: {error}",
                display_path(directory)
            )),
        }
    }
    paths.sort();
    for path in paths {
        if !extension_is(&path, "lua") {
            continue;
        }
        let result = (|| -> Result<ScannedFile> {
            require_regular_file(&path, "Lua entry")?;
            let text = read_utf8_limited(&path, MAX_LUA_BYTES)?;
            let parsed = parse_lua_manifest(&text);
            let app_id = numeric_file_stem(&path)
                .or(parsed.primary_app_id)
                .context("file name and content do not contain an AppID")?;
            let metadata = fs::metadata(&path)?;
            Ok(ScannedFile {
                app_id,
                enabled,
                path: path.clone(),
                size_bytes: metadata.len(),
                sha256: sha256_hex(text.as_bytes()),
                parsed,
            })
        })();
        match result {
            Ok(file) => output.push(file),
            Err(error) => warnings.push(format!("{}: {error:#}", display_path(&path))),
        }
    }
    Ok(())
}

fn collect_import_candidates(
    path: &Path,
    depth: usize,
    output: &mut Vec<PathBuf>,
    total_bytes: &mut u64,
    skipped: &mut Vec<ImportIssue>,
) -> Result<()> {
    if depth > MAX_IMPORT_DEPTH {
        bail!("import directory exceeds maximum depth {MAX_IMPORT_DEPTH}");
    }
    if output.len() >= MAX_IMPORT_FILES {
        bail!("import contains more than {MAX_IMPORT_FILES} supported files");
    }
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("could not inspect {}", display_path(path)))?;
    if metadata_is_link(&metadata) {
        bail!("linked or reparse-point sources are not imported");
    }
    if metadata.is_file() {
        if !matches!(
            path.extension()
                .and_then(|value| value.to_str())
                .map(|value| value.to_ascii_lowercase())
                .as_deref(),
            Some("lua" | "bin" | "manifest")
        ) {
            skipped.push(ImportIssue {
                path: display_path(path),
                reason: "unsupported extension".into(),
            });
            return Ok(());
        }
        *total_bytes = total_bytes
            .checked_add(metadata.len())
            .context("import size overflow")?;
        if *total_bytes > MAX_IMPORT_BYTES {
            bail!("import exceeds the {} byte limit", MAX_IMPORT_BYTES);
        }
        output.push(absolute_lexical(path)?);
        return Ok(());
    }
    if !metadata.is_dir() {
        bail!("source is not a regular file or directory");
    }
    let mut children = Vec::new();
    for entry in
        fs::read_dir(path).with_context(|| format!("could not list {}", display_path(path)))?
    {
        children.push(entry?.path());
    }
    children.sort();
    for child in children {
        collect_import_candidates(&child, depth + 1, output, total_bytes, skipped)?;
    }
    Ok(())
}

fn configured_lua_path(steam_root: &Path, value: &str) -> Result<PathBuf> {
    if value.contains('\0') {
        bail!("path contains a NUL character");
    }
    let value = Path::new(value);
    let path = if value.is_absolute() {
        lexical_normalize(value)?
    } else {
        lexical_normalize(&steam_root.join(value))?
    };
    if path.as_os_str().is_empty() {
        bail!("path is empty");
    }
    reject_linked_ancestors(&path)?;
    require_directory_if_present(&path, "configured Lua path")?;
    Ok(path)
}

fn validate_app_id(app_id: u32) -> Result<()> {
    if app_id == 0 {
        bail!("AppID must be greater than zero");
    }
    Ok(())
}

fn validate_lua_document(value: &str) -> Result<()> {
    if value.as_bytes().contains(&0) {
        bail!("Lua document contains a NUL byte");
    }
    if value.len() as u64 > MAX_LUA_BYTES {
        bail!("Lua document exceeds the {MAX_LUA_BYTES} byte limit");
    }
    Ok(())
}

fn normalize_text_file(value: &str) -> String {
    let normalized = value.replace("\r\n", "\n").replace('\r', "\n");
    let mut output = normalized.replace('\n', "\r\n");
    if !output.ends_with("\r\n") {
        output.push_str("\r\n");
    }
    output
}

fn atomic_write(destination: &Path, bytes: &[u8], overwrite: bool) -> Result<ManagedWrite> {
    if bytes.len() as u64 > MAX_IMPORT_BYTES {
        bail!("write exceeds the {} byte limit", MAX_IMPORT_BYTES);
    }
    let parent = destination.parent().context("destination has no parent")?;
    create_directory_checked(parent)?;
    reject_linked_ancestors(destination)?;
    if destination.exists() {
        require_regular_file(destination, "existing destination")?;
        if !overwrite {
            bail!("destination already exists: {}", display_path(destination));
        }
    }
    let temporary = unique_sibling_path(destination, "stn-tmp")?;
    let mut temporary_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .with_context(|| format!("could not create {}", display_path(&temporary)))?;
    let write_result = temporary_file
        .write_all(bytes)
        .and_then(|_| temporary_file.sync_all());
    drop(temporary_file);
    if let Err(error) = write_result {
        let _ = fs::remove_file(&temporary);
        return Err(error)
            .with_context(|| format!("could not stage {}", display_path(destination)));
    }

    let mut backup = None;
    let replaced = destination.exists();
    if replaced {
        let backup_path = unique_backup_path(destination, "replaced")?;
        create_directory_checked(backup_path.parent().expect("backup has a parent"))?;
        if let Err(error) = fs::rename(destination, &backup_path) {
            let _ = fs::remove_file(&temporary);
            return Err(error)
                .with_context(|| format!("could not back up {}", display_path(destination)));
        }
        backup = Some(backup_path);
    }
    if let Err(error) = fs::rename(&temporary, destination) {
        if let Some(path) = &backup {
            let _ = fs::rename(path, destination);
        }
        let _ = fs::remove_file(&temporary);
        return Err(error)
            .with_context(|| format!("could not commit {}", display_path(destination)));
    }
    Ok(ManagedWrite {
        destination: display_path(destination),
        backup_path: backup.as_deref().map(display_path),
        size_bytes: bytes.len() as u64,
        sha256: sha256_hex(bytes),
        replaced,
    })
}

fn unique_backup_path(destination: &Path, action: &str) -> Result<PathBuf> {
    let parent = destination.parent().context("destination has no parent")?;
    let name = destination
        .file_name()
        .and_then(|value| value.to_str())
        .context("destination file name is not Unicode")?;
    let backup_root = parent.join(".stn-backups");
    unique_path(&backup_root, &format!("{name}.{action}"))
}

fn unique_sibling_path(destination: &Path, tag: &str) -> Result<PathBuf> {
    let parent = destination.parent().context("destination has no parent")?;
    let name = destination
        .file_name()
        .and_then(|value| value.to_str())
        .context("destination file name is not Unicode")?;
    unique_path(parent, &format!(".{name}.{tag}"))
}

fn unique_path(parent: &Path, prefix: &str) -> Result<PathBuf> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock precedes the Unix epoch")?
        .as_nanos();
    for attempt in 0..100u32 {
        let candidate = parent.join(format!(
            "{prefix}-{}-{timestamp}-{attempt}",
            std::process::id()
        ));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    bail!("could not allocate a unique staging path")
}

fn create_directory_checked(path: &Path) -> Result<()> {
    reject_linked_ancestors(path)?;
    fs::create_dir_all(path).with_context(|| format!("could not create {}", display_path(path)))?;
    reject_linked_ancestors(path)?;
    require_directory_if_present(path, "directory")
}

fn require_directory_if_present(path: &Path, label: &str) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("could not inspect {label} {}", display_path(path)))?;
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
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("could not inspect {label} {}", display_path(path)))?;
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
    let mut existing = Some(path);
    while let Some(candidate) = existing {
        match fs::symlink_metadata(candidate) {
            Ok(metadata) => {
                if metadata_is_link(&metadata) {
                    bail!(
                        "linked or reparse-point paths are not allowed: {}",
                        display_path(candidate)
                    );
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("could not inspect {}", display_path(candidate)))
            }
        }
        existing = candidate.parent();
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

fn read_utf8_limited(path: &Path, limit: u64) -> Result<String> {
    let bytes = read_binary_limited(path, limit)?;
    String::from_utf8(bytes).with_context(|| format!("{} is not UTF-8", display_path(path)))
}

fn read_binary_limited(path: &Path, limit: u64) -> Result<Vec<u8>> {
    require_regular_file(path, "source file")?;
    let metadata = fs::metadata(path)?;
    if metadata.len() > limit {
        bail!("{} exceeds the {limit} byte limit", display_path(path));
    }
    let file =
        File::open(path).with_context(|| format!("could not open {}", display_path(path)))?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        bail!("{} grew beyond the {limit} byte limit", display_path(path));
    }
    Ok(bytes)
}

fn same_existing_file(left: &Path, right: &Path) -> Result<bool> {
    if !right.exists() {
        return Ok(false);
    }
    Ok(fs::canonicalize(left)? == fs::canonicalize(right)?)
}

fn safe_file_name(value: &std::ffi::OsStr) -> bool {
    let path = Path::new(value);
    matches!(
        path.components().collect::<Vec<_>>().as_slice(),
        [Component::Normal(_)]
    )
}

fn numeric_file_stem(path: &Path) -> Option<u32> {
    path.file_stem()?
        .to_str()?
        .parse()
        .ok()
        .filter(|value| *value != 0)
}

fn extension_is(path: &Path, expected: &str) -> bool {
    path.extension()
        .and_then(|value| value.to_str())
        .is_some_and(|value| value.eq_ignore_ascii_case(expected))
}

fn strip_lua_comment(line: &str) -> String {
    let mut quote = None;
    let mut escaped = false;
    let mut chars = line.char_indices().peekable();
    while let Some((index, value)) = chars.next() {
        if escaped {
            escaped = false;
            continue;
        }
        if value == '\\' && quote.is_some() {
            escaped = true;
            continue;
        }
        if matches!(value, '\'' | '"') {
            quote = if quote == Some(value) {
                None
            } else if quote.is_none() {
                Some(value)
            } else {
                quote
            };
            continue;
        }
        if value == '-' && quote.is_none() && chars.peek().is_some_and(|(_, next)| *next == '-') {
            return line[..index].to_string();
        }
    }
    line.to_string()
}

fn call_arguments<'a>(line: &'a str, function: &str) -> Option<&'a str> {
    let bytes = line.as_bytes();
    let function_bytes = function.as_bytes();
    let mut offset = 0;
    while offset + function_bytes.len() <= bytes.len() {
        let relative = line[offset..].find(function)?;
        let start = offset + relative;
        let before_ok = start == 0 || !is_identifier_byte(bytes[start - 1]);
        let after = start + function_bytes.len();
        let after_ok = after == bytes.len() || !is_identifier_byte(bytes[after]);
        if before_ok && after_ok {
            let rest = &line[after..];
            let open = rest.find('(')?;
            let arguments = &rest[open + 1..];
            let close = arguments.find(')')?;
            return Some(&arguments[..close]);
        }
        offset = after;
    }
    None
}

fn is_identifier_byte(value: u8) -> bool {
    value.is_ascii_alphanumeric() || value == b'_'
}

fn first_u64_argument(arguments: &str) -> Option<u64> {
    let value = arguments.split(',').next()?.trim();
    let value = value.trim_matches(|character| matches!(character, '\'' | '"'));
    value.parse().ok()
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
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
    lexical_normalize(&path)
}

fn lexical_normalize(path: &Path) -> Result<PathBuf> {
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

fn display_path(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn fixture() -> (tempfile::TempDir, PathBuf) {
        let temp = tempdir().unwrap();
        let steam = temp.path().join("Steam");
        fs::create_dir_all(&steam).unwrap();
        (temp, steam)
    }

    #[test]
    fn resolves_configured_lua_directory_and_default() {
        let (_temp, steam) = fixture();
        let default = resolve_lua_layout(&steam).unwrap();
        assert_eq!(PathBuf::from(default.lua_root), steam.join("config/lua"));
        fs::write(
            steam.join("opensteamtool.toml"),
            "[lua]\npaths = [\"extra/lua\", \"config/lua\"]\n",
        )
        .unwrap();
        fs::create_dir_all(steam.join("config/lua")).unwrap();
        let configured = resolve_lua_layout(&steam).unwrap();
        assert_eq!(PathBuf::from(configured.lua_root), steam.join("config/lua"));
        assert_eq!(configured.source, "opensteamtool.toml");
    }

    #[test]
    fn invalid_config_falls_back_with_warning() {
        let (_temp, steam) = fixture();
        fs::write(steam.join("opensteamtool.toml"), "[lua\npaths = [").unwrap();
        let layout = resolve_lua_layout(&steam).unwrap();
        assert_eq!(PathBuf::from(layout.lua_root), steam.join("config/lua"));
        assert!(!layout.warnings.is_empty());
    }

    #[test]
    fn parses_only_live_declarative_calls() {
        let parsed = parse_lua_manifest(
            r#"
                -- addappid(1)
                addappid(42)
                addappid(100, 1, "KEY--NOT-A-COMMENT") -- trailing
                setManifestid(200, "123", 0)
                prefix_addappid(999)
            "#,
        );
        assert_eq!(parsed.app_ids, vec![42, 100]);
        assert_eq!(parsed.primary_app_id, Some(42));
        assert_eq!(parsed.pinned_manifest_count, 1);
        assert_eq!(parsed.pinned_depot_ids, vec![200]);
    }

    #[test]
    fn creates_replaces_and_backs_up_lua() {
        let (_temp, steam) = fixture();
        let first = create_lua_entry(&steam, 42, None, false).unwrap();
        assert!(!first.replaced);
        assert!(fs::read_to_string(&first.destination)
            .unwrap()
            .contains("addappid(42)"));
        assert!(create_lua_entry(&steam, 42, None, false).is_err());
        let second = create_lua_entry(
            &steam,
            42,
            Some("addappid(42)\nsetManifestid(200, \"3\", 0)"),
            true,
        )
        .unwrap();
        assert!(second.replaced);
        assert!(second
            .backup_path
            .as_ref()
            .is_some_and(|path| Path::new(path).is_file()));
        assert!(fs::read_to_string(second.destination)
            .unwrap()
            .contains("setManifestid"));
    }

    #[test]
    fn rejects_custom_lua_for_another_app() {
        let (_temp, steam) = fixture();
        assert!(create_lua_entry(&steam, 42, Some("addappid(7)"), false).is_err());
    }

    #[test]
    fn scans_enabled_disabled_and_conflicting_entries() {
        let (_temp, steam) = fixture();
        let root = steam.join("config/lua");
        fs::create_dir_all(root.join("Disable")).unwrap();
        fs::write(root.join("42.lua"), "addappid(42)\n").unwrap();
        fs::write(
            root.join("Disable/42.lua"),
            "addappid(42)\nsetManifestid(200, \"3\", 0)\n",
        )
        .unwrap();
        fs::write(root.join("Disable/7.lua"), "addappid(7)\n").unwrap();
        let snapshot = scan_lua_library(&steam).unwrap();
        assert_eq!(snapshot.entries.len(), 2);
        let conflict = snapshot
            .entries
            .iter()
            .find(|entry| entry.app_id == 42)
            .unwrap();
        assert!(conflict.enabled);
        assert!(conflict.conflict);
        assert_eq!(conflict.pinned_manifest_count, 1);
        let disabled = snapshot
            .entries
            .iter()
            .find(|entry| entry.app_id == 7)
            .unwrap();
        assert!(!disabled.enabled);
        assert!(!disabled.conflict);
    }

    #[test]
    fn imports_supported_files_to_expected_roots() {
        let (temp, steam) = fixture();
        let input = temp.path().join("input/nested");
        fs::create_dir_all(&input).unwrap();
        fs::write(input.join("42.lua"), "addappid(42)\n").unwrap();
        fs::write(input.join("42.bin"), b"bin").unwrap();
        fs::write(input.join("42.manifest"), b"manifest").unwrap();
        fs::write(input.join("ignored.txt"), b"ignored").unwrap();
        let summary = import_library_files(&LibraryImportRequest {
            steam_root: steam.clone(),
            sources: vec![temp.path().join("input")],
            overwrite: false,
        })
        .unwrap();
        assert_eq!(
            (summary.lua_count, summary.bin_count, summary.manifest_count),
            (1, 1, 1)
        );
        assert!(steam.join("config/lua/42.lua").is_file());
        assert!(steam.join("appcache/stats/42.bin").is_file());
        assert!(steam.join("depotcache/42.manifest").is_file());
        assert_eq!(summary.skipped.len(), 1);
        assert!(summary.failed.is_empty());
    }

    #[test]
    fn enables_disables_and_archives_entries() {
        let (_temp, steam) = fixture();
        create_lua_entry(&steam, 42, None, false).unwrap();
        let disabled = set_lua_entry_enabled(&steam, 42, false, false).unwrap();
        assert_eq!(disabled.action, "disable");
        assert!(steam.join("config/lua/Disable/42.lua").is_file());
        let enabled = set_lua_entry_enabled(&steam, 42, true, false).unwrap();
        assert_eq!(enabled.action, "enable");
        assert!(steam.join("config/lua/42.lua").is_file());
        let deleted = delete_lua_entry(&steam, 42).unwrap();
        assert_eq!(deleted.backup_paths.len(), 1);
        assert!(!steam.join("config/lua/42.lua").exists());
        assert!(Path::new(&deleted.backup_paths[0]).is_file());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_linked_lua_directories() {
        use std::os::unix::fs::symlink;
        let (temp, steam) = fixture();
        let outside = temp.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::create_dir_all(steam.join("config")).unwrap();
        symlink(&outside, steam.join("config/lua")).unwrap();
        assert!(create_lua_entry(&steam, 42, None, false).is_err());
    }
}
