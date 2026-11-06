use crate::{settings::system_locale, vdf};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    env, fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SteamSnapshot {
    pub captured_at: u64,
    pub locale: String,
    pub steam: Option<SteamInstallation>,
    pub libraries: Vec<SteamLibrary>,
    pub apps: Vec<SteamApp>,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SteamInstallation {
    pub path: String,
    pub executable: String,
    pub version: Option<String>,
    pub client_build: Option<String>,
    pub running: bool,
    pub detection_source: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SteamLibrary {
    pub path: String,
    pub exists: bool,
    pub app_count: usize,
    pub size_bytes: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SteamApp {
    pub appid: u32,
    pub name: String,
    pub install_dir: String,
    pub manifest_path: String,
    pub library_path: String,
    pub size_bytes: u64,
    pub state_flags: u64,
    pub last_updated: u64,
    #[serde(default)]
    pub last_played: u64,
}

/// Inspect only install metadata. Size values are Steam's manifest estimates, not a disk crawl.
pub fn scan_steam(custom_path: Option<&Path>) -> Result<SteamSnapshot> {
    let mut snapshot = SteamSnapshot {
        captured_at: SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
        locale: system_locale(),
        steam: None,
        libraries: vec![],
        apps: vec![],
        warnings: vec![],
    };
    let detected = detect_steam(custom_path, &mut snapshot.warnings)?;
    let Some((root, source)) = detected else {
        snapshot
            .warnings
            .push("Steam was not detected. Select the Steam installation folder manually.".into());
        return Ok(snapshot);
    };
    let executable = steam_executable(&root);
    let version = file_version(&executable);
    if cfg!(windows) && version.is_none() {
        snapshot
            .warnings
            .push("Steam executable has no readable Windows file version resource.".into());
    }
    let client_build = client_build(&root, &mut snapshot.warnings);
    let running = steam_running(&executable, &mut snapshot.warnings);
    snapshot.steam = Some(SteamInstallation {
        path: display_path(&root),
        executable: display_path(&executable),
        version,
        client_build,
        running,
        detection_source: source,
    });

    let roots = library_roots(&root, &mut snapshot.warnings);
    let mut seen_apps = HashSet::new();
    for library in roots {
        let steamapps = library.join("steamapps");
        let exists = steamapps.is_dir();
        let mut summary = SteamLibrary {
            path: display_path(&library),
            exists,
            app_count: 0,
            size_bytes: 0,
        };
        if !exists {
            snapshot.warnings.push(format!(
                "Library is unavailable: {}",
                display_path(&library)
            ));
            snapshot.libraries.push(summary);
            continue;
        }
        let entries = match fs::read_dir(&steamapps) {
            Ok(entries) => entries,
            Err(error) => {
                snapshot.warnings.push(format!(
                    "Could not list library {}: {}",
                    display_path(&library),
                    error
                ));
                snapshot.libraries.push(summary);
                continue;
            }
        };
        let mut paths = Vec::new();
        for entry in entries {
            match entry {
                Ok(entry) => {
                    let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
                    if name.starts_with("appmanifest_") && name.ends_with(".acf") {
                        paths.push(entry.path());
                    }
                }
                Err(error) => snapshot
                    .warnings
                    .push(format!("Could not inspect a library entry: {error}")),
            }
        }
        paths.sort();
        for path in paths {
            match read_app_manifest(&path, &library) {
                Ok(app) => {
                    if !seen_apps.insert(app.appid) {
                        snapshot.warnings.push(format!(
                            "Duplicate AppID {} was skipped in {}",
                            app.appid,
                            display_path(&library)
                        ));
                        continue;
                    }
                    summary.app_count += 1;
                    summary.size_bytes = summary.size_bytes.saturating_add(app.size_bytes);
                    snapshot.apps.push(app);
                }
                Err(error) => snapshot
                    .warnings
                    .push(format!("Skipped {}: {error:#}", display_path(&path))),
            }
        }
        snapshot.libraries.push(summary);
    }
    snapshot.apps.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then(a.appid.cmp(&b.appid))
    });
    Ok(snapshot)
}

fn detect_steam(
    custom: Option<&Path>,
    warnings: &mut Vec<String>,
) -> Result<Option<(PathBuf, String)>> {
    if let Some(path) = custom {
        if path.as_os_str().is_empty() {
            bail!("Steam path must not be empty");
        }
        if is_network_path(path) {
            bail!("Choose a local Steam installation rather than a network path");
        }
        let path = if path.is_file() {
            path.parent().context("Steam executable has no parent")?
        } else {
            path
        };
        let path = path
            .canonicalize()
            .context("Custom Steam path does not exist or is inaccessible")?;
        if !path.is_dir() || !is_steam_root(&path) {
            bail!("Custom path has neither a Steam executable nor a steamapps directory");
        }
        return Ok(Some((path, "custom".into())));
    }
    let mut candidates: Vec<(PathBuf, String)> = vec![];
    if let Some(path) = env::var_os("STEAM_PATH") {
        candidates.push((PathBuf::from(path), "STEAM_PATH".into()));
    }
    #[cfg(windows)]
    {
        use winreg::{
            enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, KEY_WOW64_32KEY},
            RegKey,
        };
        for (hive, key, value, flags) in [
            (
                HKEY_CURRENT_USER,
                "Software\\Valve\\Steam",
                "SteamPath",
                KEY_READ,
            ),
            (
                HKEY_LOCAL_MACHINE,
                "Software\\Valve\\Steam",
                "InstallPath",
                KEY_READ | KEY_WOW64_32KEY,
            ),
        ] {
            if let Ok(key_handle) = RegKey::predef(hive).open_subkey_with_flags(key, flags) {
                if let Ok(value) = key_handle.get_value::<String, _>(value) {
                    candidates.push((PathBuf::from(value), "registry".into()));
                }
            }
        }
        for variable in ["ProgramFiles(x86)", "ProgramFiles"] {
            if let Some(root) = env::var_os(variable) {
                candidates.push((PathBuf::from(root).join("Steam"), "default-path".into()));
            }
        }
    }
    #[cfg(not(windows))]
    if let Some(home) = env::var_os("HOME") {
        let home = PathBuf::from(home);
        for suffix in [
            ".steam/steam",
            ".local/share/Steam",
            "Library/Application Support/Steam",
        ] {
            candidates.push((home.join(suffix), "default-path".into()));
        }
    }
    for (path, source) in candidates {
        if is_network_path(&path) {
            warnings.push("Skipped a network Steam installation candidate.".into());
            continue;
        }
        if is_steam_root(&path) {
            match path.canonicalize() {
                Ok(path) => return Ok(Some((path, source))),
                Err(error) => {
                    warnings.push(format!("Could not resolve detected Steam folder: {error}"))
                }
            }
        }
    }
    Ok(None)
}

fn is_steam_root(path: &Path) -> bool {
    path.is_dir()
        && (path.join("steam.exe").is_file()
            || path.join("steam.sh").is_file()
            || path.join("steamapps").is_dir())
}

fn steam_executable(root: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        root.join("steam.exe")
    }
    #[cfg(not(windows))]
    {
        if root.join("steam.sh").is_file() {
            root.join("steam.sh")
        } else {
            root.join("steam.exe")
        }
    }
}

fn client_build(root: &Path, warnings: &mut Vec<String>) -> Option<String> {
    for name in [
        "steam_client_win64.manifest",
        "steam_client_win32.manifest",
        "steam_client_ubuntu12.manifest",
        "steam_client_osx.manifest",
    ] {
        let path = root.join("package").join(name);
        if !path.exists() {
            continue;
        }
        let result = ensure_regular_file(&path).and_then(|()| vdf::read(&path));
        match result {
            Ok(value) => {
                let object = value
                    .entries()
                    .and_then(|entries| entries.first())
                    .map(|(_, value)| value)
                    .unwrap_or(&value);
                if let Some(version) = object.string("version").or_else(|| value.string("version"))
                {
                    if !version.is_empty()
                        && version.len() <= 32
                        && version.bytes().all(|b| b.is_ascii_digit())
                    {
                        return Some(version.into());
                    }
                }
                warnings.push(format!(
                    "Client manifest {name} has no valid numeric build version."
                ));
            }
            Err(error) => warnings.push(format!(
                "Could not inspect client manifest {name}: {error:#}"
            )),
        }
    }
    None
}

fn library_roots(root: &Path, warnings: &mut Vec<String>) -> Vec<PathBuf> {
    let mut result = vec![root.to_path_buf()];
    let mut seen = HashSet::from([path_identity(root)]);
    let modern = root.join("steamapps").join("libraryfolders.vdf");
    let legacy = root.join("config").join("libraryfolders.vdf");
    let path = if modern.exists() { modern } else { legacy };
    if !path.exists() {
        return result;
    }
    let value = match ensure_regular_file(&path).and_then(|()| vdf::read(&path)) {
        Ok(value) => value,
        Err(error) => {
            warnings.push(format!("Could not parse libraryfolders.vdf: {error:#}"));
            return result;
        }
    };
    let Some(entries) = value.get("libraryfolders").and_then(vdf::Value::entries) else {
        warnings.push(
            "libraryfolders.vdf has no libraryfolders object; scanned the primary library only."
                .into(),
        );
        return result;
    };
    for (key, entry) in entries {
        if key.parse::<u32>().is_err() {
            continue;
        }
        let Some(text) = entry.string("path").or_else(|| entry.text()) else {
            warnings.push(format!("Library entry {key} has no path."));
            continue;
        };
        let path = PathBuf::from(text);
        if !path.is_absolute() || is_network_path(&path) || text.contains('\0') {
            warnings.push(format!(
                "Library entry {key} is not an absolute local path; skipped."
            ));
            continue;
        }
        let path = path.canonicalize().unwrap_or(path);
        if seen.insert(path_identity(&path)) {
            result.push(path);
        }
    }
    result
}

/// Parse a single known app manifest. All fields are untrusted metadata.
pub fn read_app_manifest(path: &Path, library: &Path) -> Result<SteamApp> {
    ensure_regular_file(path)?;
    let expected_id = path
        .file_name()
        .and_then(|n| n.to_str())
        .map(str::to_ascii_lowercase)
        .and_then(|name| {
            name.strip_prefix("appmanifest_")?
                .strip_suffix(".acf")?
                .parse::<u32>()
                .ok()
        })
        .context("Invalid app manifest filename")?;
    let value = vdf::read(path)?;
    let state = value
        .get("AppState")
        .context("Manifest has no AppState object")?;
    let appid = state
        .string("appid")
        .and_then(|value| value.parse::<u32>().ok())
        .context("Manifest AppID is missing or invalid")?;
    if appid == 0 || appid != expected_id {
        bail!("Manifest AppID does not match its filename");
    }
    let name = state
        .string("name")
        .filter(|s| !s.trim().is_empty())
        .context("Manifest game name is missing")?;
    if name.len() > 4096 {
        bail!("Manifest game name exceeds inspection limit");
    }
    let install = state
        .string("installdir")
        .context("Manifest install directory is missing")?;
    if !safe_install_subpath(install) {
        bail!("Manifest install directory is not a safe relative path");
    }
    let common = library.join("steamapps").join("common");
    let install_path = common.join(install.replace('\\', "/"));
    // Canonical checks stop reparse points/symlinks from escaping the selected library.
    if install_path.exists() {
        let actual = install_path
            .canonicalize()
            .context("Could not resolve game installation path")?;
        let boundary = common
            .canonicalize()
            .context("Could not resolve library common directory")?;
        if !actual.starts_with(&boundary) {
            bail!("Manifest install directory resolves outside its library");
        }
    }
    let number = |key: &str| -> Result<u64> {
        match state.string(key) {
            Some(text) => text
                .parse::<u64>()
                .with_context(|| format!("Manifest {key} is not an unsigned integer")),
            None => Ok(0),
        }
    };
    Ok(SteamApp {
        appid,
        name: name.chars().filter(|c| !c.is_control()).collect(),
        install_dir: display_path(&install_path),
        manifest_path: display_path(path),
        library_path: display_path(library),
        size_bytes: number("SizeOnDisk")?,
        state_flags: number("StateFlags")?,
        last_updated: number("LastUpdated")?,
        last_played: number("LastPlayed")?,
    })
}

fn safe_install_subpath(input: &str) -> bool {
    if input.is_empty()
        || input.len() > 4096
        || input.contains(':')
        || input.chars().any(char::is_control)
        || input.starts_with(['/', '\\'])
    {
        return false;
    }
    input
        .split(['/', '\\'])
        .all(|part| !part.is_empty() && part != "." && part != ".." && !part.ends_with([' ', '.']))
}

fn ensure_regular_file(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        bail!("Inspection accepts regular files only, not links");
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            bail!("Inspection does not follow file reparse points");
        }
    }
    Ok(())
}

fn is_network_path(path: &Path) -> bool {
    let text = path.to_string_lossy();
    (text.starts_with("\\\\") && !text.starts_with("\\\\?\\"))
        || text.to_ascii_lowercase().starts_with("\\\\?\\unc\\")
        || text.starts_with("//")
}

fn path_identity(path: &Path) -> String {
    let text = display_path(path).replace('\\', "/");
    if cfg!(windows) {
        text.to_lowercase()
    } else {
        text
    }
}

fn display_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    text.strip_prefix("\\\\?\\").unwrap_or(&text).to_owned()
}

/// Read version resources as bytes through Windows, without loading/executing the binary.
#[cfg(windows)]
pub fn file_version(path: &Path) -> Option<String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW, VS_FIXEDFILEINFO,
    };
    let path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut ignored = 0;
    // SAFETY: all pointers refer to live buffers, lengths are checked, no foreign memory is retained.
    unsafe {
        let size = GetFileVersionInfoSizeW(path.as_ptr(), &mut ignored);
        if size == 0 || size > 8 * 1024 * 1024 {
            return None;
        }
        let mut bytes = vec![0u8; size as usize];
        if GetFileVersionInfoW(path.as_ptr(), 0, size, bytes.as_mut_ptr().cast()) == 0 {
            return None;
        }
        let mut data = std::ptr::null_mut();
        let mut length = 0;
        let root = [b'\\' as u16, 0];
        if VerQueryValueW(bytes.as_ptr().cast(), root.as_ptr(), &mut data, &mut length) == 0
            || data.is_null()
            || length < std::mem::size_of::<VS_FIXEDFILEINFO>() as u32
        {
            return None;
        }
        let value = std::ptr::read_unaligned(data.cast::<VS_FIXEDFILEINFO>());
        if value.dwSignature != 0xfeef04bd {
            return None;
        }
        Some(format!(
            "{}.{}.{}.{}",
            value.dwFileVersionMS >> 16,
            value.dwFileVersionMS & 0xffff,
            value.dwFileVersionLS >> 16,
            value.dwFileVersionLS & 0xffff
        ))
    }
}

#[cfg(not(windows))]
pub fn file_version(_path: &Path) -> Option<String> {
    None
}

#[cfg(windows)]
fn steam_running(executable: &Path, warnings: &mut Vec<String>) -> bool {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, INVALID_HANDLE_VALUE},
        System::{
            Diagnostics::ToolHelp::{
                CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
                TH32CS_SNAPPROCESS,
            },
            Threading::{
                OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
            },
        },
    };
    // Inspect executable names/paths only; no process memory, command line, environment or accounts.
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            warnings.push("Steam running state could not be inspected.".into());
            return false;
        }
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut next = Process32FirstW(snapshot, &mut entry);
        let mut running = false;
        while next != 0 {
            let length = entry
                .szExeFile
                .iter()
                .position(|c| *c == 0)
                .unwrap_or(entry.szExeFile.len());
            if String::from_utf16_lossy(&entry.szExeFile[..length])
                .eq_ignore_ascii_case("steam.exe")
            {
                let process =
                    OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, entry.th32ProcessID);
                if !process.is_null() {
                    let mut buffer = vec![0u16; 32768];
                    let mut length = buffer.len() as u32;
                    if QueryFullProcessImageNameW(process, 0, buffer.as_mut_ptr(), &mut length) != 0
                    {
                        let path =
                            PathBuf::from(String::from_utf16_lossy(&buffer[..length as usize]));
                        running = path_identity(&path) == path_identity(executable);
                    }
                    CloseHandle(process);
                    if running {
                        break;
                    }
                } else {
                    warnings.push(
                        "A Steam process exists, but its installation path could not be verified."
                            .into(),
                    );
                }
            }
            next = Process32NextW(snapshot, &mut entry);
        }
        CloseHandle(snapshot);
        running
    }
}

#[cfg(not(windows))]
fn steam_running(_executable: &Path, warnings: &mut Vec<String>) -> bool {
    warnings
        .push("Steam running-state inspection is currently implemented for Windows only.".into());
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("steamapps")).unwrap();
        fs::write(dir.path().join("steam.exe"), b"").unwrap();
        dir
    }

    fn manifest(root: &Path, id: u32, name: &str, install: &str) -> PathBuf {
        let path = root.join("steamapps").join(format!("appmanifest_{id}.acf"));
        let install = install.replace('\\', "\\\\");
        fs::write(&path, format!("\"AppState\" {{ \"appid\" \"{id}\" \"name\" \"{name}\" \"installdir\" \"{install}\" \"SizeOnDisk\" \"4096\" \"StateFlags\" \"4\" \"LastUpdated\" \"123\" }}")).unwrap();
        path
    }

    #[test]
    fn scans_unicode_game_and_keeps_broken_file_warning() {
        let dir = fixture();
        manifest(dir.path(), 570, "你好 / 日本語 / 한국어", "ゲーム");
        fs::write(
            dir.path().join("steamapps/appmanifest_999.acf"),
            "AppState {",
        )
        .unwrap();
        let snapshot = scan_steam(Some(dir.path())).unwrap();
        assert_eq!(snapshot.apps.len(), 1);
        assert_eq!(snapshot.apps[0].appid, 570);
        assert_eq!(snapshot.libraries[0].size_bytes, 4096);
        assert!(snapshot
            .warnings
            .iter()
            .any(|warning| warning.contains("appmanifest_999")));
    }

    #[test]
    fn reads_last_played_without_confusing_last_update() {
        let dir = fixture();
        let path = manifest(dir.path(), 570, "Fixture", "game");
        let text = fs::read_to_string(&path).unwrap().replace(
            "\"LastUpdated\" \"123\"",
            "\"LastUpdated\" \"123\" \"LastPlayed\" \"456\"",
        );
        fs::write(&path, text).unwrap();
        let app = read_app_manifest(&path, dir.path()).unwrap();
        assert_eq!((app.last_updated, app.last_played), (123, 456));
        let old = manifest(dir.path(), 571, "Old", "old");
        assert_eq!(read_app_manifest(&old, dir.path()).unwrap().last_played, 0);
    }

    #[test]
    fn rejects_unsafe_install_paths_and_mismatched_appid() {
        let dir = fixture();
        for install in [
            "../outside",
            "a/../../outside",
            "..\\outside",
            "C:\\Games",
            "/root",
            "\\\\server\\share",
            "a//b",
            "folder.",
            "folder/.. ",
        ] {
            let path = manifest(dir.path(), 570, "Fixture", install);
            assert!(
                read_app_manifest(&path, dir.path()).is_err(),
                "accepted {install}"
            );
        }
        let path = manifest(dir.path(), 570, "Fixture", "game");
        fs::rename(&path, path.with_file_name("appmanifest_571.acf")).unwrap();
        assert!(
            read_app_manifest(&path.with_file_name("appmanifest_571.acf"), dir.path()).is_err()
        );
    }

    #[test]
    fn detects_two_libraries_deduplicates_paths_and_parses_build() {
        let primary = fixture();
        let secondary = fixture();
        manifest(primary.path(), 1, "One", "one");
        manifest(secondary.path(), 2, "Two", "two");
        let primary_text = primary.path().to_string_lossy().replace('\\', "\\\\");
        let secondary_text = secondary.path().to_string_lossy().replace('\\', "\\\\");
        fs::write(primary.path().join("steamapps/libraryfolders.vdf"), format!("libraryfolders {{ 0 {{ path \"{primary_text}\" }} 1 {{ path \"{secondary_text}\" }} 2 \"{secondary_text}\" }}")).unwrap();
        fs::create_dir(primary.path().join("package")).unwrap();
        fs::write(
            primary.path().join("package/steam_client_win64.manifest"),
            "steam_client_win64 { version 1792618285 }",
        )
        .unwrap();
        let snapshot = scan_steam(Some(primary.path())).unwrap();
        assert_eq!(snapshot.apps.len(), 2);
        assert_eq!(snapshot.libraries.len(), 2);
        assert_eq!(
            snapshot.steam.unwrap().client_build.as_deref(),
            Some("1792618285")
        );
    }

    #[test]
    fn explicit_invalid_path_is_an_error_not_empty_success() {
        let dir = tempfile::tempdir().unwrap();
        assert!(scan_steam(Some(dir.path())).is_err());
        assert!(scan_steam(Some(&dir.path().join("missing"))).is_err());
        assert!(scan_steam(Some(Path::new(""))).is_err());
    }

    #[test]
    fn malformed_numeric_fields_do_not_become_zero() {
        let dir = fixture();
        let path = manifest(dir.path(), 1, "Fixture", "folder");
        let text = fs::read_to_string(&path).unwrap().replace("4096", "-1");
        fs::write(&path, text).unwrap();
        assert!(read_app_manifest(&path, dir.path()).is_err());
    }

    #[test]
    fn serializes_contract_in_camel_case() {
        let dir = fixture();
        manifest(dir.path(), 1, "Game", "game");
        let value = serde_json::to_value(scan_steam(Some(dir.path())).unwrap()).unwrap();
        assert!(value.get("capturedAt").is_some());
        assert!(value["apps"][0].get("sizeBytes").is_some());
        assert!(value["apps"][0].get("size_bytes").is_none());
    }
}
