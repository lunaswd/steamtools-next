use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    env,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

pub const SUPPORTED_LOCALES: &[&str] = &[
    "zh-CN", "zh-TW", "en", "ja", "ko", "de", "fr", "es", "pt-BR", "ru",
];

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    pub language: String,
    pub theme: String,
    pub accent_color: String,
    pub compact_mode: bool,
    pub show_covers: bool,
    pub reduce_motion: bool,
    pub steam_path: Option<String>,
    pub kernel_path: Option<String>,
    pub kernel_artifact_path: Option<String>,
    pub kernel_release_source: String,
    pub injection_method: String,
    pub download_mode: String,
    pub include_dlc: bool,
    pub pin_manifest: bool,
    pub fetch_name_comments: bool,
    pub auto_refresh_key_cache: bool,
    pub show_ranking: bool,
    pub auto_refresh_library: bool,
    pub enable_logging: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            language: "auto".into(),
            theme: "dark".into(),
            accent_color: "steamBlue".into(),
            compact_mode: false,
            show_covers: true,
            reduce_motion: false,
            steam_path: None,
            kernel_path: None,
            kernel_artifact_path: None,
            kernel_release_source: crate::kernel_release::DEFAULT_KERNEL_RELEASE_SOURCE.into(),
            injection_method: "dwmapi".into(),
            download_mode: "depotKeys".into(),
            include_dlc: true,
            pin_manifest: false,
            fetch_name_comments: false,
            auto_refresh_key_cache: true,
            show_ranking: true,
            auto_refresh_library: true,
            enable_logging: true,
        }
    }
}

/// Resolve language/script/region aliases against the shipped language packs.
pub fn resolve_locale(input: &str) -> String {
    let normalized = input.trim().replace('_', "-").to_ascii_lowercase();
    let normalized = normalized.split(['.', '@']).next().unwrap_or("en");
    let parts: Vec<&str> = normalized.split('-').collect();
    let language = parts.first().copied().unwrap_or("en");
    match language {
        "zh" => {
            if parts.contains(&"hans") || parts.contains(&"chs") {
                "zh-CN"
            } else if parts.contains(&"hant")
                || parts.contains(&"cht")
                || parts.iter().any(|p| matches!(*p, "tw" | "hk" | "mo"))
            {
                "zh-TW"
            } else {
                "zh-CN"
            }
        }
        "ja" => "ja",
        "ko" => "ko",
        "de" => "de",
        "fr" => "fr",
        "es" => "es",
        "pt" => "pt-BR",
        "ru" => "ru",
        _ => "en",
    }
    .into()
}

pub fn system_locale() -> String {
    resolve_locale(&sys_locale::get_locale().unwrap_or_else(|| "en".into()))
}

/// Only this application's per-user data directory is written by the settings service.
pub fn app_data_dir() -> Result<PathBuf> {
    #[cfg(windows)]
    let root = env::var_os("LOCALAPPDATA")
        .or_else(|| env::var_os("APPDATA"))
        .map(PathBuf::from)
        .context("Windows per-user application data directory is unavailable")?;
    #[cfg(not(windows))]
    let root = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|p| PathBuf::from(p).join(".config")))
        .context("Per-user application data directory is unavailable")?;
    if !root.is_absolute() {
        bail!("Application data directory must be an absolute path");
    }
    Ok(root.join("SteamToolsNext"))
}

pub fn settings_path() -> Result<PathBuf> {
    Ok(app_data_dir()?.join("settings.json"))
}
pub fn load_settings() -> Result<Settings> {
    load_settings_at(&settings_path()?)
}
pub fn save_settings(settings: &Settings) -> Result<()> {
    save_settings_at(&settings_path()?, settings)
}

pub fn validate_settings(settings: &Settings) -> Result<()> {
    crate::kernel_release::validate_kernel_release_source(&settings.kernel_release_source)?;
    if settings.language != "auto" && !SUPPORTED_LOCALES.contains(&settings.language.as_str()) {
        bail!("Unsupported language setting; use auto or a shipped locale code");
    }
    if !["dark", "light", "system"].contains(&settings.theme.as_str()) {
        bail!("Theme must be dark, light or system");
    }
    if !["steamBlue", "mint", "violet"].contains(&settings.accent_color.as_str()) {
        bail!("Accent color must be steamBlue, mint or violet");
    }
    if !["dwmapi", "version"].contains(&settings.injection_method.as_str()) {
        bail!("Injection method must be dwmapi or version");
    }
    if !["depotKeys", "remote"].contains(&settings.download_mode.as_str()) {
        bail!("Download mode must be depotKeys or remote");
    }
    for path in [
        &settings.steam_path,
        &settings.kernel_path,
        &settings.kernel_artifact_path,
    ]
    .into_iter()
    .flatten()
    {
        if path.trim().is_empty() || path.contains('\0') || path.len() > 32768 {
            bail!("Configured path is empty, too long or contains NUL");
        }
    }
    Ok(())
}

fn load_settings_at(path: &Path) -> Result<Settings> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Settings::default())
        }
        Err(error) => return Err(error).context("Could not inspect application settings"),
    };
    if !metadata.is_file() || is_link(&metadata) {
        bail!("Settings path must be a regular file, not a link");
    }
    if metadata.len() > 65536 {
        bail!("Settings file exceeds 64 KiB; preserved without modification");
    }
    let mut settings: Settings =
        serde_json::from_slice(&fs::read(path).context("Could not read application settings")?)
            .context("Settings JSON is invalid; original file was preserved")?;
    normalize_kernel_preferences(&mut settings);
    validate_settings(&settings)?;
    Ok(settings)
}

fn save_settings_at(path: &Path, settings: &Settings) -> Result<()> {
    let mut settings = settings.clone();
    normalize_kernel_preferences(&mut settings);
    validate_settings(&settings)?;
    let parent = path.parent().context("Settings path has no parent")?;
    fs::create_dir_all(parent).context("Could not create application data directory")?;
    if let Ok(meta) = fs::symlink_metadata(path) {
        if !meta.is_file() || is_link(&meta) {
            bail!("Settings target must be a regular file, not a link");
        }
    }
    let bytes = serde_json::to_vec_pretty(&settings)?;
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let temp = parent.join(format!(".settings-{}-{stamp}.tmp", std::process::id()));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(&bytes)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        drop(file);
        atomic_replace(&temp, path).context("Could not atomically save settings")?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

/// Legacy source-workspace settings are accepted only for migration, never used
/// as installation inputs. Keep the original disk file until the user saves.
fn normalize_kernel_preferences(settings: &mut Settings) {
    settings.kernel_path = None;
    settings.kernel_artifact_path = None;
    settings.kernel_release_source = crate::kernel_release::DEFAULT_KERNEL_RELEASE_SOURCE.into();
    settings.injection_method = "dwmapi".into();
}

#[cfg(windows)]
fn atomic_replace(from: &Path, to: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };
    let from: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
    let to: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: null-terminated UTF-16 buffers live throughout the synchronous call.
    if unsafe {
        MoveFileExW(
            from.as_ptr(),
            to.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

#[cfg(not(windows))]
fn atomic_replace(from: &Path, to: &Path) -> Result<()> {
    fs::rename(from, to)?;
    Ok(())
}

fn is_link(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_type().is_symlink() || metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_regions_scripts_and_unknown_locales() {
        for (input, expected) in [
            ("zh_CN.UTF-8", "zh-CN"),
            ("zh-Hant", "zh-TW"),
            ("zh_HK", "zh-TW"),
            ("ZH-hans-HK", "zh-CN"),
            ("zh-MO", "zh-TW"),
            ("ja-JP", "ja"),
            ("ko_KR", "ko"),
            ("de-DE", "de"),
            ("fr-CA", "fr"),
            ("es-MX", "es"),
            ("pt-PT", "pt-BR"),
            ("ru-RU", "ru"),
            ("nl-NL", "en"),
            ("C", "en"),
            ("", "en"),
        ] {
            assert_eq!(resolve_locale(input), expected, "{input}");
        }
    }

    #[test]
    fn atomic_roundtrip_and_replace() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("settings.json");
        assert_eq!(load_settings_at(&path).unwrap(), Settings::default());
        let mut settings = Settings {
            language: "ja".into(),
            steam_path: Some("E:\\游戏\\Steam".into()),
            ..Default::default()
        };
        save_settings_at(&path, &settings).unwrap();
        assert_eq!(load_settings_at(&path).unwrap(), settings);
        settings.theme = "light".into();
        save_settings_at(&path, &settings).unwrap();
        assert_eq!(load_settings_at(&path).unwrap(), settings);
        assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 1);
    }

    #[test]
    fn corrupt_file_is_preserved_and_invalid_save_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("settings.json");
        fs::write(&path, b"{broken").unwrap();
        assert!(load_settings_at(&path).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"{broken");
        let settings = Settings {
            language: "unsupported".into(),
            ..Default::default()
        };
        assert!(save_settings_at(&path, &settings).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"{broken");
    }

    #[test]
    fn validates_themes_and_paths() {
        let mut settings = Settings {
            theme: "bright".into(),
            ..Default::default()
        };
        assert!(validate_settings(&settings).is_err());
        settings.theme = "system".into();
        settings.kernel_path = Some(String::new());
        assert!(validate_settings(&settings).is_err());
        settings.kernel_path = Some("path\0suffix".into());
        assert!(validate_settings(&settings).is_err());
        settings.kernel_path = None;
        settings.injection_method = "other".into();
        assert!(validate_settings(&settings).is_err());
        settings.injection_method = "dwmapi".into();
        settings.download_mode = "unknown".into();
        assert!(validate_settings(&settings).is_err());
    }

    #[test]
    fn legacy_gitlab_source_migrates_to_github_without_overwriting_settings() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("settings.json");
        let legacy = Settings {
            kernel_release_source: "https://gitlab.com/group/subgroup/kernel".into(),
            language: "ja".into(),
            steam_path: Some("E:\\Steam".into()),
            ..Default::default()
        };
        let original = serde_json::to_vec(&legacy).unwrap();
        fs::write(&path, &original).unwrap();
        let loaded = load_settings_at(&path).unwrap();
        assert_eq!(
            loaded.kernel_release_source,
            crate::kernel_release::DEFAULT_KERNEL_RELEASE_SOURCE
        );
        assert_eq!(loaded.language, "ja");
        assert_eq!(loaded.steam_path, legacy.steam_path);
        assert_eq!(fs::read(&path).unwrap(), original);
        save_settings_at(&path, &legacy).unwrap();
        assert_eq!(
            load_settings_at(&path).unwrap().kernel_release_source,
            crate::kernel_release::DEFAULT_KERNEL_RELEASE_SOURCE
        );
        assert!(!String::from_utf8(fs::read(&path).unwrap())
            .unwrap()
            .contains("gitlab.com"));
    }

    #[test]
    fn retired_workspace_and_source_preferences_are_ignored_on_load_and_save() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("settings.json");
        let legacy = Settings {
            kernel_path: Some(String::new()),
            kernel_artifact_path: Some("legacy directory".into()),
            kernel_release_source: "https://github.com/example/other-source".into(),
            injection_method: "version".into(),
            ..Default::default()
        };
        fs::write(&path, serde_json::to_vec(&legacy).unwrap()).unwrap();
        let migrated = load_settings_at(&path).unwrap();
        assert_eq!(migrated.kernel_path, None);
        assert_eq!(migrated.kernel_artifact_path, None);
        assert_eq!(migrated.injection_method, "dwmapi");
        assert_eq!(
            migrated.kernel_release_source,
            crate::kernel_release::DEFAULT_KERNEL_RELEASE_SOURCE
        );
        save_settings_at(&path, &legacy).unwrap();
        assert_eq!(load_settings_at(&path).unwrap(), migrated);
    }
}
