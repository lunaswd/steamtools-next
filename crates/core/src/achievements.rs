//! Per-game achievement management through an isolated Steam client worker.
//!
//! Interface layout evidence: Fluent-Steam-Lua Services/SamApi, an altered
//! adaptation of Rick's SAM API (zlib license; see docs/THIRD-PARTY-NOTICES.md).
//! No Steam files, achievement IDs, or current states are invented or patched.
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const MAX_SCHEMA_BYTES: usize = 8 * 1024 * 1024;
const MAX_ACHIEVEMENTS: usize = 10_000;
static WORKER_COUNTER: AtomicU64 = AtomicU64::new(0);
static WORKER_LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SteamSessionReadinessState {
    Offline,
    SignedOut,
    IdentityChanged,
    StartupTimeout,
}

#[derive(Clone, Debug)]
pub struct SteamSessionReadinessError {
    state: SteamSessionReadinessState,
    detail: String,
}
impl SteamSessionReadinessError {
    pub fn code(&self) -> &'static str {
        match self.state {
            SteamSessionReadinessState::Offline => "steam_session_offline",
            SteamSessionReadinessState::SignedOut => "steam_session_signed_out",
            SteamSessionReadinessState::IdentityChanged => "steam_session_identity_changed",
            SteamSessionReadinessState::StartupTimeout => "steam_session_startup_timeout",
        }
    }
    pub(crate) fn unavailable(detail: impl Into<String>) -> Self {
        Self {
            state: SteamSessionReadinessState::StartupTimeout,
            detail: detail.into(),
        }
    }
    fn from_code(code: &str, detail: String) -> Option<Self> {
        let detail = detail
            .strip_prefix(&format!("{code}: "))
            .unwrap_or(&detail)
            .to_owned();
        Some(Self {
            state: match code {
                "steam_session_offline" => SteamSessionReadinessState::Offline,
                "steam_session_signed_out" => SteamSessionReadinessState::SignedOut,
                "steam_session_identity_changed" => SteamSessionReadinessState::IdentityChanged,
                "steam_session_startup_timeout" => SteamSessionReadinessState::StartupTimeout,
                _ => return None,
            },
            detail,
        })
    }
}
impl std::fmt::Display for SteamSessionReadinessError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code(), self.detail)
    }
}
impl std::error::Error for SteamSessionReadinessError {}
pub fn steam_session_error_code(error: &anyhow::Error) -> Option<&'static str> {
    error
        .downcast_ref::<SteamSessionReadinessError>()
        .map(SteamSessionReadinessError::code)
}

#[derive(Clone, Copy)]
pub(crate) struct SteamUserReadinessProbe {
    pub(crate) logged_on: bool,
    pub(crate) steam_id: u64,
}

/// Startup readiness seam; credentials stay in native memory and are never logged.
pub(crate) fn wait_for_steam_user_ready<P, C, W>(
    maximum_probes: usize,
    mut probe: P,
    mut pump: C,
    mut wait: W,
) -> std::result::Result<u64, SteamSessionReadinessError>
where
    P: FnMut() -> Result<SteamUserReadinessProbe>,
    C: FnMut() -> Result<()>,
    W: FnMut(),
{
    let mut initial_identity = 0;
    let mut consecutive_ready = 0;
    let mut last = None;
    for attempt in 0..maximum_probes.min(400) {
        pump().map_err(|error| SteamSessionReadinessError::unavailable(error.to_string()))?;
        let state =
            probe().map_err(|error| SteamSessionReadinessError::unavailable(error.to_string()))?;
        if state.steam_id != 0 {
            if initial_identity != 0 && initial_identity != state.steam_id {
                return Err(SteamSessionReadinessError { state: SteamSessionReadinessState::IdentityChanged, detail: "Steam user identity changed during startup; refresh before changing achievements".into() });
            }
            initial_identity = state.steam_id;
        }
        if state.logged_on && state.steam_id != 0 {
            consecutive_ready += 1;
            if consecutive_ready >= 2 {
                return Ok(state.steam_id);
            }
        } else {
            consecutive_ready = 0;
        }
        last = Some(state);
        if attempt + 1 < maximum_probes.min(400) {
            wait();
        }
    }
    let state = match last {
        Some(state) if !state.logged_on && state.steam_id != 0 => {
            SteamSessionReadinessState::Offline
        }
        Some(state) if !state.logged_on && state.steam_id == 0 => {
            SteamSessionReadinessState::SignedOut
        }
        _ => SteamSessionReadinessState::StartupTimeout,
    };
    Err(SteamSessionReadinessError { state, detail: "Steam session did not become ready within the bounded startup wait; check the Steam client's login/network state, then refresh".into() })
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AchievementEntry {
    pub id: String,
    pub display_name: String,
    pub description: String,
    pub icon: String,
    pub icon_gray: String,
    pub unlocked: Option<bool>,
    pub unlock_time: Option<u64>,
    pub hidden: bool,
    pub protected: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AchievementCatalog {
    pub app_id: u32,
    pub requested_language: String,
    pub resolved_language: String,
    pub source: String,
    pub connected: bool,
    pub writable: bool,
    pub achievements: Vec<AchievementEntry>,
    pub error: Option<String>,
    #[serde(default)]
    pub session_error_code: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AchievementChange {
    pub id: String,
    pub unlock: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AchievementChangeResult {
    pub id: String,
    pub unlock: bool,
    pub success: bool,
    /// Fresh local Steam client readback matches, not proof of cloud persistence.
    pub local_verified: bool,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AchievementApplyResult {
    pub app_id: u32,
    pub confirmed: bool,
    pub local_verified: bool,
    pub server_acknowledged: bool,
    pub save_warning: Option<String>,
    pub results: Vec<AchievementChangeResult>,
    pub catalog: AchievementCatalog,
}

#[cfg(any(test, all(windows, target_pointer_width = "64")))]
type AchievementReadback = (String, std::result::Result<(bool, u64), String>);

/// Deterministic seam between setter/store callbacks and fresh Steam readback.
#[cfg(any(test, all(windows, target_pointer_width = "64")))]
fn reconcile_achievement_apply(
    catalog: &mut AchievementCatalog,
    mut results: Vec<AchievementChangeResult>,
    save_attempted: bool,
    stored: std::result::Result<(), String>,
    refresh: std::result::Result<(), String>,
    readbacks: Vec<AchievementReadback>,
) -> AchievementApplyResult {
    let refreshed = refresh.is_ok();
    let acknowledged = stored.is_ok();
    let confirmed = acknowledged && refreshed;
    let save_warning = stored.err();
    // StoreStats acknowledgement and fresh client state are independent evidence.
    // A callback 1102 rejection is one batch-level warning, not one failure per ID.
    catalog.error = refresh.err();
    if !refreshed {
        catalog.writable = false;
        for entry in &mut catalog.achievements {
            entry.unlocked = None;
            entry.unlock_time = None;
        }
    }
    for result in &mut results {
        result.success = false;
        result.local_verified = false;
        if !refreshed {
            result.error.get_or_insert_with(|| {
                "Steam state refresh failed; achievement change is unverified".into()
            });
            continue;
        }
        match readbacks
            .iter()
            .find(|(id, _)| id == &result.id)
            .map(|(_, readback)| readback)
        {
            Some(Ok((unlocked, timestamp))) => {
                if let Some(entry) = catalog
                    .achievements
                    .iter_mut()
                    .find(|entry| entry.id == result.id)
                {
                    entry.unlocked = Some(*unlocked);
                    entry.unlock_time = if *unlocked && *timestamp > 0 {
                        Some(*timestamp)
                    } else {
                        None
                    };
                }
                result.local_verified = result.error.is_none() && *unlocked == result.unlock;
                result.success = acknowledged && result.local_verified;
                if !result.local_verified && result.error.is_none() {
                    result.error = Some("Steam readback does not match the requested state".into());
                }
            }
            Some(Err(error)) => {
                result.error.get_or_insert_with(|| error.clone());
                if let Some(entry) = catalog
                    .achievements
                    .iter_mut()
                    .find(|entry| entry.id == result.id)
                {
                    entry.unlocked = None;
                    entry.unlock_time = None;
                }
            }
            None => {
                result
                    .error
                    .get_or_insert_with(|| "Steam achievement readback is unavailable".into());
                if let Some(entry) = catalog
                    .achievements
                    .iter_mut()
                    .find(|entry| entry.id == result.id)
                {
                    entry.unlocked = None;
                    entry.unlock_time = None;
                }
            }
        }
    }
    AchievementApplyResult {
        app_id: catalog.app_id,
        confirmed: confirmed && !results.is_empty() && results.iter().all(|result| result.success),
        local_verified: !results.is_empty() && results.iter().all(|result| result.local_verified),
        server_acknowledged: save_attempted && acknowledged,
        save_warning,
        results,
        catalog: catalog.clone(),
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WorkerRequest {
    steam_path: PathBuf,
    app_id: u32,
    locale: String,
    changes: Option<Vec<AchievementChange>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkerResponse {
    catalog: Option<AchievementCatalog>,
    applied: Option<AchievementApplyResult>,
    error: Option<String>,
    #[serde(default)]
    error_code: Option<String>,
}

fn worker_response_error(
    code: Option<String>,
    detail: Option<String>,
    fallback: &str,
) -> anyhow::Error {
    let detail = detail.unwrap_or_else(|| fallback.into());
    if let Some(error) = code
        .as_deref()
        .and_then(|code| SteamSessionReadinessError::from_code(code, detail.clone()))
    {
        error.into()
    } else {
        anyhow::anyhow!(detail)
    }
}

/// Map application UI locales to Steam schema keys, not the Steam client's UI.
pub fn achievement_language(locale: &str) -> &'static str {
    match crate::settings::resolve_locale(locale).as_str() {
        "zh-CN" => "schinese",
        "zh-TW" => "tchinese",
        "ja" => "japanese",
        "ko" => "koreana",
        "de" => "german",
        "fr" => "french",
        "es" => "spanish",
        "pt-BR" => "brazilian",
        "ru" => "russian",
        _ => "english",
    }
}

fn validate_request(app_id: u32, changes: Option<&[AchievementChange]>) -> Result<()> {
    if app_id == 0 {
        bail!("A nonzero AppID is required");
    }
    if let Some(changes) = changes {
        if changes.is_empty() || changes.len() > MAX_ACHIEVEMENTS {
            bail!("Select between 1 and {MAX_ACHIEVEMENTS} achievements");
        }
        let mut ids = BTreeSet::new();
        for change in changes {
            if change.id.is_empty() || change.id.len() > 256 || change.id.contains('\0') {
                bail!("Invalid achievement API ID");
            }
            if !ids.insert(&change.id) {
                bail!("Duplicate achievement API ID: {}", change.id);
            }
        }
    }
    Ok(())
}

#[cfg(any(test, all(windows, target_pointer_width = "64")))]
fn validate_catalog_changes(
    catalog: &AchievementCatalog,
    changes: &[AchievementChange],
) -> Result<()> {
    validate_request(catalog.app_id, Some(changes))?;
    if !catalog.connected || !catalog.writable {
        bail!("Steam achievement states are not ready for changes");
    }
    for change in changes {
        let entry = catalog
            .achievements
            .iter()
            .find(|entry| entry.id == change.id)
            .context("Selected achievement does not exist in this game's schema")?;
        if entry.protected {
            bail!("Achievement {} is server-managed and protected", change.id);
        }
        if entry.unlocked.is_none() {
            bail!(
                "Achievement {} state is unknown; refresh before changing it",
                change.id
            );
        }
    }
    Ok(())
}

fn steam_root(path: Option<&Path>) -> Result<PathBuf> {
    let root = if let Some(path) = path {
        path.to_path_buf()
    } else {
        PathBuf::from(
            crate::steam::scan_steam(None)?
                .steam
                .context("Steam installation was not found")?
                .path,
        )
    };
    if !root.join("steam.exe").is_file() {
        bail!("Steam installation is missing steam.exe");
    }
    root.canonicalize()
        .context("Steam directory is unavailable")
}

pub fn list_achievements(
    executable: &Path,
    cache_dir: &Path,
    steam_path: Option<&Path>,
    app_id: u32,
    locale: &str,
) -> Result<AchievementCatalog> {
    validate_request(app_id, None)?;
    let request = WorkerRequest {
        steam_path: steam_root(steam_path)?,
        app_id,
        locale: locale.to_owned(),
        changes: None,
    };
    let response = invoke_worker(executable, cache_dir, &request)?;
    response.catalog.ok_or_else(|| {
        worker_response_error(
            response.error_code,
            response.error,
            "Achievement worker returned no catalog",
        )
    })
}

pub fn apply_achievement_changes(
    executable: &Path,
    cache_dir: &Path,
    steam_path: Option<&Path>,
    app_id: u32,
    locale: &str,
    changes: Vec<AchievementChange>,
) -> Result<AchievementApplyResult> {
    validate_request(app_id, Some(&changes))?;
    let request = WorkerRequest {
        steam_path: steam_root(steam_path)?,
        app_id,
        locale: locale.to_owned(),
        changes: Some(changes),
    };
    let response = invoke_worker(executable, cache_dir, &request)?;
    response.applied.ok_or_else(|| {
        worker_response_error(
            response.error_code,
            response.error,
            "Achievement worker returned no apply result",
        )
    })
}

struct WorkerFiles {
    directory: PathBuf,
}
impl Drop for WorkerFiles {
    fn drop(&mut self) {
        // Only this invocation's three known IPC files are removed; no recursion.
        for name in ["request.json", "response.json"] {
            let _ = fs::remove_file(self.directory.join(name));
        }
        let _ = fs::remove_dir(&self.directory);
    }
}

fn invoke_worker(
    executable: &Path,
    cache_dir: &Path,
    request: &WorkerRequest,
) -> Result<WorkerResponse> {
    let _lock = WORKER_LOCK
        .lock()
        .map_err(|_| anyhow::anyhow!("Achievement worker lock is poisoned"))?;
    fs::create_dir_all(cache_dir)?;
    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let directory = cache_dir.join(format!(
        "ach-{}-{timestamp}-{}",
        std::process::id(),
        WORKER_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&directory)?;
    let files = WorkerFiles { directory };
    let input = files.directory.join("request.json");
    let output = files.directory.join("response.json");
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&input)?;
    file.write_all(&serde_json::to_vec(request)?)?;
    file.sync_all()?;
    drop(file);
    let mut command = Command::new(executable);
    command
        .arg("--achievement-worker")
        .arg(&input)
        .arg(&output)
        .env("SteamAppId", request.app_id.to_string())
        .env("SteamGameId", request.app_id.to_string())
        .env("SteamLanguage", achievement_language(&request.locale))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    let mut child = command
        .spawn()
        .context("Could not start the achievement worker")?;
    let deadline = Instant::now() + Duration::from_secs(55);
    loop {
        if let Some(status) = child.try_wait()? {
            if !status.success() {
                bail!("Steam achievement worker exited with {status}; no success was confirmed");
            }
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("Steam achievement worker timed out; any write is unconfirmed. Refresh the game before retrying");
        }
        thread::sleep(Duration::from_millis(25));
    }
    let mut bytes = Vec::new();
    fs::File::open(output)?
        .take((MAX_SCHEMA_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_SCHEMA_BYTES {
        bail!("Achievement worker response exceeds inspection limit");
    }
    serde_json::from_slice(&bytes).context("Invalid achievement worker response")
}

/// Called before constructing the Tauri application. It never launches a window.
pub fn run_achievement_worker(input: &Path, output: &Path) -> Result<()> {
    let mut bytes = Vec::new();
    fs::File::open(input)?
        .take((MAX_SCHEMA_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_SCHEMA_BYTES {
        bail!("Worker request exceeds inspection limit");
    }
    let request: WorkerRequest = serde_json::from_slice(&bytes)?;
    validate_request(request.app_id, request.changes.as_deref())?;
    let result = execute_worker(&request);
    let response = match result {
        Ok(response) => response,
        Err(error) => WorkerResponse {
            catalog: None,
            applied: None,
            error_code: steam_session_error_code(&error).map(str::to_owned),
            error: Some(error.to_string()),
        },
    };
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(output)?;
    file.write_all(&serde_json::to_vec(&response)?)?;
    file.sync_all()?;
    Ok(())
}

#[derive(Clone, Debug)]
enum Kv {
    Object(Vec<(String, Kv)>),
    Text(String),
}
impl Kv {
    fn get(&self, key: &str) -> Option<&Kv> {
        self.entries()?
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(key))
            .map(|(_, value)| value)
    }
    fn entries(&self) -> Option<&[(String, Kv)]> {
        match self {
            Self::Object(items) => Some(items),
            _ => None,
        }
    }
    fn text(&self) -> Option<&str> {
        match self {
            Self::Text(text) => Some(text),
            _ => None,
        }
    }
    fn string(&self, key: &str) -> &str {
        self.get(key).and_then(Kv::text).unwrap_or("")
    }
}

struct KvReader<'a> {
    bytes: &'a [u8],
    offset: usize,
    nodes: usize,
}
impl<'a> KvReader<'a> {
    fn take(&mut self, count: usize) -> Result<&'a [u8]> {
        let end = self
            .offset
            .checked_add(count)
            .context("Schema size overflow")?;
        let result = self
            .bytes
            .get(self.offset..end)
            .context("Truncated binary KeyValues schema")?;
        self.offset = end;
        Ok(result)
    }
    fn string(&mut self) -> Result<String> {
        let length = self.bytes[self.offset..]
            .iter()
            .position(|byte| *byte == 0)
            .context("Unterminated schema string")?;
        if length > 64 * 1024 {
            bail!("Schema string exceeds limit");
        }
        let value =
            String::from_utf8(self.take(length)?.to_vec()).context("Schema string is not UTF-8")?;
        self.take(1)?;
        Ok(value)
    }
    fn object(&mut self, depth: usize) -> Result<Kv> {
        if depth > 64 {
            bail!("Schema nesting exceeds 64 levels");
        }
        let mut entries = Vec::new();
        loop {
            let kind = self.take(1)?[0];
            if kind == 8 {
                break;
            }
            self.nodes += 1;
            if self.nodes > 250_000 {
                bail!("Schema node count exceeds limit");
            }
            let key = self.string()?;
            let value = match kind {
                0 => self.object(depth + 1)?,
                1 => Kv::Text(self.string()?),
                2 => Kv::Text(i32::from_le_bytes(self.take(4)?.try_into()?).to_string()),
                3 => Kv::Text(f32::from_le_bytes(self.take(4)?.try_into()?).to_string()),
                4 | 6 => Kv::Text(u32::from_le_bytes(self.take(4)?.try_into()?).to_string()),
                5 => {
                    let length = u16::from_le_bytes(self.take(2)?.try_into()?) as usize;
                    let words: Vec<u16> = self
                        .take(length * 2)?
                        .chunks_exact(2)
                        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                        .collect();
                    Kv::Text(
                        String::from_utf16(&words)?
                            .trim_end_matches('\0')
                            .to_owned(),
                    )
                }
                7 => Kv::Text(u64::from_le_bytes(self.take(8)?.try_into()?).to_string()),
                _ => bail!("Unsupported binary schema value type {kind}"),
            };
            entries.push((key, value));
        }
        Ok(Kv::Object(entries))
    }
}

fn localized(node: Option<&Kv>, language: &str, default: &str) -> (String, String) {
    if let Some(node) = node {
        for key in [language, "english"] {
            if let Some(value) = node
                .get(key)
                .and_then(Kv::text)
                .filter(|value| !value.is_empty())
            {
                return (value.into(), key.into());
            }
        }
        if let Some(value) = node.text().filter(|value| !value.is_empty()) {
            return (value.into(), "native".into());
        }
        if let Some((key, value)) = node.entries().and_then(|entries| {
            entries
                .iter()
                .filter(|(key, _)| !key.eq_ignore_ascii_case("token"))
                .find_map(|(key, value)| {
                    value
                        .text()
                        .filter(|value| !value.is_empty())
                        .map(|value| (key, value))
                })
        }) {
            return (value.into(), key.to_owned());
        }
    }
    (default.into(), "api-id".into())
}

fn icon_url(app_id: u32, value: &str) -> String {
    if value.is_empty() {
        return String::new();
    }
    // Only Steam image hashes/file names from the local schema are accepted.
    if value.len() <= 128
        && !value.starts_with('.')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        format!(
            "https://shared.akamai.steamstatic.com/community_assets/images/apps/{app_id}/{value}"
        )
    } else {
        String::new()
    }
}

pub fn read_achievement_schema(
    path: &Path,
    app_id: u32,
    locale: &str,
) -> Result<AchievementCatalog> {
    validate_request(app_id, None)?;
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take((MAX_SCHEMA_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    parse_schema(&bytes, app_id, locale)
}

fn parse_schema(bytes: &[u8], app_id: u32, locale: &str) -> Result<AchievementCatalog> {
    if bytes.len() > MAX_SCHEMA_BYTES {
        bail!("Achievement schema exceeds 8 MiB limit");
    }
    let mut reader = KvReader {
        bytes,
        offset: 0,
        nodes: 0,
    };
    let root = reader.object(0)?;
    if reader.offset != bytes.len() {
        bail!("Unexpected trailing bytes in achievement schema");
    }
    let stats = root
        .get(&app_id.to_string())
        .and_then(|app| app.get("stats"))
        .context("Achievement schema AppID does not match selected game")?;
    let language = achievement_language(locale);
    let mut resolved = BTreeSet::new();
    let mut ids = BTreeSet::new();
    let mut achievements = Vec::new();
    for (_, stat) in stats.entries().context("Schema stats is not an object")? {
        let kind = stat.string("type").to_ascii_lowercase();
        let number = stat
            .string("type_int")
            .parse::<i32>()
            .ok()
            .or_else(|| kind.parse().ok());
        if !matches!(kind.as_str(), "achievements" | "groupachievements")
            && !matches!(number, Some(4 | 5))
        {
            continue;
        }
        for (key, bits) in stat.entries().unwrap_or(&[]) {
            if !key.eq_ignore_ascii_case("bits") {
                continue;
            }
            for (_, bit) in bits.entries().unwrap_or(&[]) {
                let id = bit.string("name");
                if id.is_empty() || id.len() > 256 || !ids.insert(id.to_owned()) {
                    continue;
                }
                let display = bit.get("display");
                let (display_name, name_language) =
                    localized(display.and_then(|node| node.get("name")), language, id);
                resolved.insert(name_language);
                let (description, _) =
                    localized(display.and_then(|node| node.get("desc")), language, "");
                achievements.push(AchievementEntry {
                    id: id.into(),
                    display_name,
                    description,
                    icon: icon_url(
                        app_id,
                        display.map(|node| node.string("icon")).unwrap_or(""),
                    ),
                    icon_gray: icon_url(
                        app_id,
                        display.map(|node| node.string("icon_gray")).unwrap_or(""),
                    ),
                    unlocked: None,
                    unlock_time: None,
                    hidden: display
                        .map(|node| matches!(node.string("hidden"), "1" | "true"))
                        .unwrap_or(false),
                    protected: bit.string("permission").parse::<u32>().unwrap_or(0) & 3 != 0,
                });
                if achievements.len() > MAX_ACHIEVEMENTS {
                    bail!("Schema has too many achievements");
                }
            }
        }
    }
    let resolved_language = if resolved.is_empty() {
        language.into()
    } else if resolved.len() == 1 {
        resolved.into_iter().next().unwrap()
    } else {
        "mixed".into()
    };
    Ok(AchievementCatalog {
        app_id,
        requested_language: language.into(),
        resolved_language,
        source: "steam-schema".into(),
        connected: false,
        writable: false,
        achievements,
        error: None,
        session_error_code: None,
    })
}

fn execute_worker(request: &WorkerRequest) -> Result<WorkerResponse> {
    let schema_path = request
        .steam_path
        .join("appcache")
        .join("stats")
        .join(format!("UserGameStatsSchema_{}.bin", request.app_id));
    let schema = read_achievement_schema(&schema_path, request.app_id, &request.locale);
    #[cfg(all(windows, target_pointer_width = "64"))]
    {
        match native::SteamClient::connect(&request.steam_path, request.app_id) {
            Ok(mut client) => {
                if let Err(error) = client.request_stats() {
                    if request.changes.is_some() {
                        return Err(error);
                    }
                    let mut catalog = match schema {
                        Ok(catalog) => catalog,
                        Err(_) => return Err(error),
                    };
                    catalog.error = Some(error.to_string());
                    catalog.session_error_code =
                        steam_session_error_code(&error).map(str::to_owned);
                    return Ok(WorkerResponse {
                        catalog: Some(catalog),
                        applied: None,
                        error: None,
                        error_code: None,
                    });
                }
                // RequestUserStats can populate a previously missing schema.
                let schema = schema.or_else(|_| {
                    read_achievement_schema(&schema_path, request.app_id, &request.locale)
                });
                let mut catalog = client.catalog(schema.ok(), &request.locale)?;
                if let Some(changes) = &request.changes {
                    let applied = client.apply(&mut catalog, changes)?;
                    return Ok(WorkerResponse {
                        catalog: None,
                        applied: Some(applied),
                        error: None,
                        error_code: None,
                    });
                }
                Ok(WorkerResponse {
                    catalog: Some(catalog),
                    applied: None,
                    error: None,
                    error_code: None,
                })
            }
            Err(error) => {
                if request.changes.is_some() {
                    return Err(error);
                }
                let mut catalog = match schema {
                    Ok(catalog) => catalog,
                    Err(_) => return Err(error),
                };
                catalog.error = Some(error.to_string());
                catalog.session_error_code = steam_session_error_code(&error).map(str::to_owned);
                Ok(WorkerResponse {
                    catalog: Some(catalog),
                    applied: None,
                    error: None,
                    error_code: None,
                })
            }
        }
    }
    #[cfg(not(all(windows, target_pointer_width = "64")))]
    {
        if request.changes.is_some() {
            bail!("Live achievement changes require the Windows x64 desktop build");
        }
        let mut catalog = schema?;
        catalog.error = Some("Live Steam states require the Windows x64 desktop build".into());
        Ok(WorkerResponse {
            catalog: Some(catalog),
            applied: None,
            error: None,
            error_code: None,
        })
    }
}

#[cfg(any(test, all(windows, target_pointer_width = "64")))]
fn matching_stats_callback(
    id: i32,
    message_user: i32,
    payload: &[u8],
    app_id: u32,
    steam_id: u64,
    user_handle: i32,
) -> Option<i32> {
    if !matches!(id, 1101 | 1102) || message_user != user_handle || payload.len() < 12 {
        return None;
    }
    let game = u64::from_le_bytes(payload[0..8].try_into().ok()?);
    if game != u64::from(app_id) {
        return None;
    }
    if id == 1101 {
        let received_user = u64::from_le_bytes(payload.get(12..20)?.try_into().ok()?);
        if received_user != steam_id {
            return None;
        }
    }
    Some(i32::from_le_bytes(payload[8..12].try_into().ok()?))
}

#[cfg(all(windows, target_pointer_width = "64"))]
mod native {
    use super::*;
    use std::{
        ffi::{c_char, c_void, CStr, CString},
        os::windows::ffi::OsStrExt,
        ptr,
    };

    #[link(name = "kernel32")]
    extern "system" {
        fn LoadLibraryExW(name: *const u16, file: *mut c_void, flags: u32) -> *mut c_void;
        fn GetProcAddress(module: *mut c_void, name: *const c_char) -> *mut c_void;
        fn FreeLibrary(module: *mut c_void) -> i32;
    }
    type Object = *mut c_void;
    type CreateInterface = unsafe extern "C" fn(*const c_char, *mut i32) -> Object;
    type GetCallback = unsafe extern "C" fn(i32, *mut CallbackMessage, *mut i32) -> bool;
    type FreeCallback = unsafe extern "C" fn(i32) -> bool;
    #[repr(C)]
    struct CallbackMessage {
        user: i32,
        id: i32,
        data: *const u8,
        size: i32,
    }

    struct Module(Object);
    impl Drop for Module {
        fn drop(&mut self) {
            unsafe {
                FreeLibrary(self.0);
            }
        }
    }
    unsafe fn export<T: Copy>(module: Object, name: &[u8]) -> Result<T> {
        let address = GetProcAddress(module, name.as_ptr().cast());
        if address.is_null() {
            bail!("Required Steam client export is unavailable");
        }
        Ok(std::mem::transmute_copy(&address))
    }
    unsafe fn method<T: Copy>(object: Object, slot: usize) -> T {
        let table = *(object as *const *const Object);
        std::mem::transmute_copy(&*table.add(slot))
    }
    unsafe fn interface(
        client: Object,
        slot: usize,
        user: i32,
        pipe: i32,
        version: &[u8],
    ) -> Result<Object> {
        let call: unsafe extern "system" fn(Object, i32, i32, *const c_char) -> Object =
            method(client, slot);
        let result = call(client, user, pipe, version.as_ptr().cast());
        if result.is_null() {
            bail!("Requested Steam client interface is unavailable");
        }
        Ok(result)
    }
    unsafe fn text(pointer: *const c_char) -> String {
        if pointer.is_null() {
            String::new()
        } else {
            CStr::from_ptr(pointer).to_string_lossy().into_owned()
        }
    }

    pub struct SteamClient {
        // Module stays alive until all interface objects and pipe are released.
        _module: Module,
        client: Object,
        stats: Object,
        pipe: i32,
        user: i32,
        steam_id: u64,
        app_id: u32,
        get_callback: GetCallback,
        free_callback: FreeCallback,
    }
    impl Drop for SteamClient {
        fn drop(&mut self) {
            unsafe {
                if self.user != 0 {
                    let call: unsafe extern "system" fn(Object, i32, i32) = method(self.client, 4);
                    call(self.client, self.pipe, self.user);
                }
                if self.pipe != 0 {
                    let call: unsafe extern "system" fn(Object, i32) -> bool =
                        method(self.client, 1);
                    call(self.client, self.pipe);
                }
            }
        }
    }
    impl SteamClient {
        pub fn connect(root: &Path, app_id: u32) -> Result<Self> {
            unsafe {
                // Only this isolated worker process receives the per-game environment.
                std::env::set_var("SteamAppId", app_id.to_string());
                std::env::set_var("SteamGameId", app_id.to_string());
                let path = [
                    root.join("steamclient64.dll"),
                    root.join("bin").join("steamclient64.dll"),
                ]
                .into_iter()
                .find(|path| path.is_file())
                .context("steamclient64.dll was not found")?;
                let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
                let handle = LoadLibraryExW(wide.as_ptr(), ptr::null_mut(), 8); // absolute DLL, altered search path
                if handle.is_null() {
                    bail!(
                        "Steam client DLL load failed: {}",
                        std::io::Error::last_os_error()
                    );
                }
                let module = Module(handle);
                let create: CreateInterface = export(handle, b"CreateInterface\0")?;
                let get_callback = export(handle, b"Steam_BGetCallback\0")?;
                let free_callback = export(handle, b"Steam_FreeLastCallback\0")?;
                let client = create(c"SteamClient018".as_ptr(), ptr::null_mut());
                if client.is_null() {
                    bail!("SteamClient018 is unavailable");
                }
                let mut result = Self {
                    _module: module,
                    client,
                    stats: ptr::null_mut(),
                    pipe: 0,
                    user: 0,
                    steam_id: 0,
                    app_id,
                    get_callback,
                    free_callback,
                };
                let create_pipe: unsafe extern "system" fn(Object) -> i32 = method(client, 0);
                result.pipe = create_pipe(client);
                if result.pipe == 0 {
                    return Err(SteamSessionReadinessError::unavailable(
                        "Steam is not running or could not create an IPC pipe",
                    )
                    .into());
                }
                let connect_user: unsafe extern "system" fn(Object, i32) -> i32 = method(client, 2);
                result.user = connect_user(client, result.pipe);
                if result.user == 0 {
                    return Err(SteamSessionReadinessError::unavailable("The current Steam global-user IPC connection is unavailable; finish Steam sign-in and refresh").into());
                }
                let get_utils: unsafe extern "system" fn(Object, i32, *const c_char) -> Object =
                    method(client, 9);
                let utils = get_utils(client, result.pipe, c"SteamUtils005".as_ptr());
                if utils.is_null() {
                    bail!("SteamUtils005 is unavailable");
                }
                let get_app: unsafe extern "system" fn(Object) -> u32 = method(utils, 9);
                if get_app(utils) != app_id {
                    bail!("Steam worker AppID context does not match the selected game");
                }
                let user = interface(client, 5, result.user, result.pipe, b"SteamUser012\0")?;
                let logged_on: unsafe extern "system" fn(Object) -> bool = method(user, 1);
                // SteamUser012 returns CSteamID by an out-struct argument on Windows.
                let get_id: unsafe extern "system" fn(Object, *mut u64) = method(user, 2);
                result.steam_id = wait_for_steam_user_ready(
                    400,
                    || {
                        let mut steam_id = 0;
                        get_id(user, &mut steam_id);
                        Ok(SteamUserReadinessProbe {
                            logged_on: logged_on(user),
                            steam_id,
                        })
                    },
                    || result.drain().map(|_| ()),
                    || thread::sleep(Duration::from_millis(20)),
                )?;
                result.stats = interface(
                    client,
                    13,
                    result.user,
                    result.pipe,
                    b"STEAMUSERSTATS_INTERFACE_VERSION013\0",
                )?;
                Ok(result)
            }
        }

        fn drain(&self) -> Result<Vec<(i32, u64, i32)>> {
            unsafe {
                let mut events = Vec::new();
                for _ in 0..10_000 {
                    let mut callback = CallbackMessage {
                        user: 0,
                        id: 0,
                        data: ptr::null(),
                        size: 0,
                    };
                    let mut call = 0;
                    if !(self.get_callback)(self.pipe, &mut callback, &mut call) {
                        return Ok(events);
                    }
                    if matches!(callback.id, 1101 | 1102)
                        && (12..=128).contains(&callback.size)
                        && !callback.data.is_null()
                    {
                        let payload =
                            std::slice::from_raw_parts(callback.data, callback.size as usize);
                        if let Some(result) = matching_stats_callback(
                            callback.id,
                            callback.user,
                            payload,
                            self.app_id,
                            self.steam_id,
                            self.user,
                        ) {
                            events.push((callback.id, self.app_id as u64, result));
                        }
                    }
                    (self.free_callback)(self.pipe);
                }
                bail!("Steam callback queue exceeded inspection limit")
            }
        }

        fn wait(&self, id: i32) -> Result<()> {
            let deadline = Instant::now() + Duration::from_secs(15);
            loop {
                for (event, _, result) in self.drain()? {
                    if event == id {
                        if result == 1 {
                            return Ok(());
                        }
                        bail!("Steam callback {id} rejected the request (EResult={result})");
                    }
                }
                if Instant::now() >= deadline {
                    bail!("Steam callback {id} timed out; request was not confirmed");
                }
                thread::sleep(Duration::from_millis(20));
            }
        }

        pub fn request_stats(&self) -> Result<()> {
            unsafe {
                self.drain()?;
                let request: unsafe extern "system" fn(Object, u64) -> u64 = method(self.stats, 15);
                if request(self.stats, self.steam_id) == 0 {
                    bail!("Steam rejected RequestUserStats for this game");
                }
                self.wait(1101)
            }
        }

        fn ids(&self) -> Result<Vec<String>> {
            unsafe {
                let count: unsafe extern "system" fn(Object) -> u32 = method(self.stats, 13);
                let name: unsafe extern "system" fn(Object, u32) -> *const c_char =
                    method(self.stats, 14);
                let count = count(self.stats);
                if count as usize > MAX_ACHIEVEMENTS {
                    bail!("Steam reported too many achievements");
                }
                let mut ids = Vec::new();
                for index in 0..count {
                    let id = text(name(self.stats, index));
                    if id.is_empty() || id.len() > 256 {
                        bail!("Steam returned an invalid achievement API ID");
                    }
                    ids.push(id);
                }
                Ok(ids)
            }
        }

        fn state(&self, id: &str) -> Result<(bool, u64)> {
            unsafe {
                let id = CString::new(id)?;
                let get: unsafe extern "system" fn(
                    Object,
                    *const c_char,
                    *mut bool,
                    *mut u32,
                ) -> bool = method(self.stats, 8);
                let mut unlocked = false;
                let mut timestamp = 0;
                if !get(self.stats, id.as_ptr(), &mut unlocked, &mut timestamp) {
                    bail!("Steam did not return achievement state");
                }
                Ok((unlocked, timestamp as u64))
            }
        }

        pub fn catalog(
            &self,
            schema: Option<AchievementCatalog>,
            locale: &str,
        ) -> Result<AchievementCatalog> {
            unsafe {
                let ids = self.ids()?;
                let mut catalog = schema.unwrap_or_else(|| AchievementCatalog {
                    app_id: self.app_id,
                    requested_language: achievement_language(locale).into(),
                    resolved_language: "steam-client".into(),
                    source: "steam-client".into(),
                    connected: false,
                    writable: false,
                    achievements: Vec::new(),
                    error: None,
                    session_error_code: None,
                });
                let attribute: unsafe extern "system" fn(
                    Object,
                    *const c_char,
                    *const c_char,
                ) -> *const c_char = method(self.stats, 11);
                for id in ids {
                    if !catalog.achievements.iter().any(|entry| entry.id == id) {
                        let native_id = CString::new(id.as_str())?;
                        let name =
                            text(attribute(self.stats, native_id.as_ptr(), c"name".as_ptr()));
                        let description =
                            text(attribute(self.stats, native_id.as_ptr(), c"desc".as_ptr()));
                        let hidden = text(attribute(
                            self.stats,
                            native_id.as_ptr(),
                            c"hidden".as_ptr(),
                        )) == "1";
                        catalog.achievements.push(AchievementEntry {
                            display_name: if name.is_empty() { id.clone() } else { name },
                            id,
                            description,
                            icon: String::new(),
                            icon_gray: String::new(),
                            unlocked: None,
                            unlock_time: None,
                            hidden,
                            protected: false,
                        });
                    }
                }
                catalog.connected = true;
                catalog.writable = true;
                catalog.session_error_code = None;
                for entry in &mut catalog.achievements {
                    match self.state(&entry.id) {
                        Ok((unlocked, timestamp)) => {
                            entry.unlocked = Some(unlocked);
                            entry.unlock_time = if unlocked && timestamp > 0 {
                                Some(timestamp)
                            } else {
                                None
                            };
                        }
                        Err(_) => {
                            entry.unlocked = None;
                            entry.unlock_time = None;
                        }
                    }
                }
                Ok(catalog)
            }
        }

        pub fn apply(
            &mut self,
            catalog: &mut AchievementCatalog,
            changes: &[AchievementChange],
        ) -> Result<AchievementApplyResult> {
            // Validate the complete batch before changing even one in-memory state.
            validate_catalog_changes(catalog, changes)?;
            let mut results: Vec<AchievementChangeResult> = changes
                .iter()
                .map(|change| AchievementChangeResult {
                    id: change.id.clone(),
                    unlock: change.unlock,
                    success: false,
                    local_verified: false,
                    error: None,
                })
                .collect();
            let mut accepted = false;
            unsafe {
                for result in &mut results {
                    let entry = catalog
                        .achievements
                        .iter()
                        .find(|entry| entry.id == result.id)
                        .unwrap();
                    if entry.unlocked == Some(result.unlock) {
                        continue;
                    }
                    let id = CString::new(result.id.as_str())?;
                    let set: unsafe extern "system" fn(Object, *const c_char) -> bool =
                        method(self.stats, if result.unlock { 6 } else { 7 });
                    if set(self.stats, id.as_ptr()) {
                        accepted = true;
                    } else {
                        result.error = Some("Steam rejected the achievement change".into());
                    }
                }
            }
            let stored = if accepted {
                self.drain()?;
                let store: unsafe extern "system" fn(Object) -> bool =
                    unsafe { method(self.stats, 9) };
                if unsafe { store(self.stats) } {
                    self.wait(1102)
                } else {
                    Err(anyhow::anyhow!("Steam rejected StoreStats"))
                }
            } else {
                Ok(())
            };
            let refresh = self.request_stats().map_err(|error| error.to_string());
            let readbacks = if refresh.is_ok() {
                results
                    .iter()
                    .map(|result| {
                        (
                            result.id.clone(),
                            self.state(&result.id).map_err(|error| error.to_string()),
                        )
                    })
                    .collect()
            } else {
                Vec::new()
            };
            Ok(reconcile_achievement_apply(
                catalog,
                results,
                accepted,
                stored.map_err(|error| error.to_string()),
                refresh,
                readbacks,
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn temporary_logged_on_false_is_retried_before_valid_ipc_session_is_rejected() {
        let attempts = std::cell::Cell::new(0);
        let states = [false, true, true];
        let result = wait_for_steam_user_ready(
            4,
            || {
                let index = attempts.get();
                attempts.set(index + 1);
                Ok(SteamUserReadinessProbe {
                    logged_on: states[index.min(2)],
                    steam_id: 99,
                })
            },
            || Ok(()),
            || {},
        );
        assert!(result.is_ok(), "A valid global Steam IPC session must not be declared offline on its first transient LoggedOn=false probe: {result:?}");
        assert_eq!(result.unwrap(), 99);
        assert!(attempts.get() >= 3);
    }
    #[test]
    fn persistent_offline_or_signed_out_is_rejected_only_after_readiness_budget() {
        for (steam_id, code) in [
            (99, "steam_session_offline"),
            (0, "steam_session_signed_out"),
        ] {
            let probes = std::cell::Cell::new(0);
            let pumps = std::cell::Cell::new(0);
            let waits = std::cell::Cell::new(0);
            let error = wait_for_steam_user_ready(
                4,
                || {
                    probes.set(probes.get() + 1);
                    Ok(SteamUserReadinessProbe {
                        logged_on: false,
                        steam_id,
                    })
                },
                || {
                    pumps.set(pumps.get() + 1);
                    Ok(())
                },
                || waits.set(waits.get() + 1),
            )
            .unwrap_err();
            assert_eq!(probes.get(), 4);
            assert_eq!(pumps.get(), 4);
            assert_eq!(waits.get(), 3);
            assert_eq!(error.code(), code);
        }
    }
    #[test]
    fn signed_in_flag_without_identity_never_becomes_write_ready() {
        let error = wait_for_steam_user_ready(
            4,
            || {
                Ok(SteamUserReadinessProbe {
                    logged_on: true,
                    steam_id: 0,
                })
            },
            || Ok(()),
            || {},
        )
        .unwrap_err();
        assert_eq!(error.code(), "steam_session_startup_timeout");
    }
    #[test]
    fn user_identity_change_during_startup_aborts_instead_of_adopting_new_account() {
        let probes = std::cell::Cell::new(0);
        let error = wait_for_steam_user_ready(
            4,
            || {
                probes.set(probes.get() + 1);
                Ok(SteamUserReadinessProbe {
                    logged_on: probes.get() > 1,
                    steam_id: if probes.get() == 1 { 99 } else { 100 },
                })
            },
            || Ok(()),
            || {},
        )
        .unwrap_err();
        assert_eq!(error.code(), "steam_session_identity_changed");
        assert_eq!(probes.get(), 2);
    }
    #[test]
    fn readiness_requires_two_consecutive_stable_ready_probes_and_pumps_ipc() {
        let flags = [true, false, true, true];
        let probes = std::cell::Cell::new(0);
        let pumps = std::cell::Cell::new(0);
        let id = wait_for_steam_user_ready(
            4,
            || {
                let index = probes.get();
                probes.set(index + 1);
                Ok(SteamUserReadinessProbe {
                    logged_on: flags[index],
                    steam_id: 99,
                })
            },
            || {
                pumps.set(pumps.get() + 1);
                Ok(())
            },
            || {},
        )
        .unwrap();
        assert_eq!(id, 99);
        assert_eq!(probes.get(), 4);
        assert_eq!(pumps.get(), 4);
        let error = wait_for_steam_user_ready(
            4,
            || {
                Ok(SteamUserReadinessProbe {
                    logged_on: true,
                    steam_id: 99,
                })
            },
            || Err(anyhow::anyhow!("IPC unavailable")),
            || {},
        )
        .unwrap_err();
        assert_eq!(error.code(), "steam_session_startup_timeout");
    }
    #[test]
    fn structured_session_error_code_survives_worker_json_roundtrip_without_text_guessing() {
        for code in [
            "steam_session_offline",
            "steam_session_signed_out",
            "steam_session_identity_changed",
            "steam_session_startup_timeout",
        ] {
            let error =
                SteamSessionReadinessError::from_code(code, "Fixture reason".into()).unwrap();
            let response = WorkerResponse {
                catalog: None,
                applied: None,
                error_code: Some(error.code().into()),
                error: Some(error.to_string()),
            };
            let decoded: WorkerResponse =
                serde_json::from_slice(&serde_json::to_vec(&response).unwrap()).unwrap();
            let restored = worker_response_error(decoded.error_code, decoded.error, "missing");
            assert_eq!(steam_session_error_code(&restored), Some(code));
            assert_eq!(restored.to_string(), format!("{code}: Fixture reason"));
        }
        assert!(steam_session_error_code(&anyhow::anyhow!(
            "Steam rejected the achievement change"
        ))
        .is_none());
        let mut catalog = parse_schema(&schema(), 42, "en").unwrap();
        catalog.session_error_code = Some("steam_session_offline".into());
        let json = serde_json::to_value(catalog).unwrap();
        assert_eq!(json["sessionErrorCode"], "steam_session_offline");
    }
    fn object(key: &str, children: Vec<u8>) -> Vec<u8> {
        let mut bytes = vec![0];
        bytes.extend(key.as_bytes());
        bytes.push(0);
        bytes.extend(children);
        bytes.push(8);
        bytes
    }
    fn string(key: &str, value: &str) -> Vec<u8> {
        let mut bytes = vec![1];
        bytes.extend(key.as_bytes());
        bytes.push(0);
        bytes.extend(value.as_bytes());
        bytes.push(0);
        bytes
    }
    fn schema() -> Vec<u8> {
        let display = object(
            "display",
            [
                object(
                    "name",
                    [
                        string("english", "First step"),
                        string("schinese", "第一步"),
                        string("japanese", "最初の一歩"),
                    ]
                    .concat(),
                ),
                object(
                    "desc",
                    [
                        string("english", "Start a game"),
                        string("schinese", "开始游戏"),
                    ]
                    .concat(),
                ),
                string("hidden", "1"),
                string("icon", "abcd.jpg"),
            ]
            .concat(),
        );
        let bit = object(
            "0",
            [string("name", "FIRST"), string("permission", "1"), display].concat(),
        );
        let mut bytes = object(
            "42",
            object(
                "stats",
                object("1", [string("type_int", "4"), object("bits", bit)].concat()),
            ),
        );
        bytes.push(8);
        bytes
    }
    #[test]
    fn ui_locale_maps_all_ten_languages() {
        for (locale, expected) in [
            ("zh-CN", "schinese"),
            ("zh-TW", "tchinese"),
            ("en", "english"),
            ("ja", "japanese"),
            ("ko", "koreana"),
            ("de", "german"),
            ("fr", "french"),
            ("es", "spanish"),
            ("pt-BR", "brazilian"),
            ("ru", "russian"),
        ] {
            assert_eq!(achievement_language(locale), expected);
        }
    }
    #[test]
    fn binary_schema_uses_ui_language_and_preserves_unknown_state() {
        let catalog = parse_schema(&schema(), 42, "zh-CN").unwrap();
        assert_eq!(catalog.achievements[0].display_name, "第一步");
        assert_eq!(catalog.achievements[0].description, "开始游戏");
        assert!(catalog.achievements[0].protected && catalog.achievements[0].hidden);
        assert_eq!(catalog.achievements[0].unlocked, None);
        assert!(!catalog.connected && !catalog.writable);
        assert_eq!(catalog.resolved_language, "schinese");
    }
    #[test]
    fn unsupported_game_language_falls_back_to_english_without_changing_id() {
        let catalog = parse_schema(&schema(), 42, "de").unwrap();
        assert_eq!(catalog.achievements[0].display_name, "First step");
        assert_eq!(catalog.achievements[0].id, "FIRST");
        assert_eq!(catalog.requested_language, "german");
        assert_eq!(catalog.resolved_language, "english");
    }
    #[test]
    fn schema_rejects_truncation_wrong_appid_and_trailing_bytes() {
        let bytes = schema();
        assert!(parse_schema(&bytes[..bytes.len() - 1], 42, "en").is_err());
        assert!(parse_schema(&bytes, 43, "en").is_err());
        assert!(parse_schema(&[bytes, vec![0]].concat(), 42, "en").is_err());
    }
    #[test]
    fn changes_reject_empty_duplicate_and_nul_ids() {
        assert!(validate_request(0, None).is_err());
        assert!(validate_request(42, Some(&[])).is_err());
        let change = AchievementChange {
            id: "FIRST".into(),
            unlock: true,
        };
        assert!(validate_request(42, Some(&[change.clone(), change])).is_err());
        assert!(validate_request(
            42,
            Some(&[AchievementChange {
                id: "a\0b".into(),
                unlock: true
            }])
        )
        .is_err());
    }
    #[test]
    fn icon_schema_cannot_inject_url_or_path() {
        assert!(icon_url(42, "https://example.invalid/a").is_empty());
        assert!(icon_url(42, "../outside").is_empty());
        assert!(icon_url(42, "..").is_empty());
        assert!(icon_url(42, ".").is_empty());
        assert!(icon_url(42, "%2e%2e%2foutside").is_empty());
        assert!(icon_url(42, "\\outside").is_empty());
        assert!(icon_url(42, &"a".repeat(129)).is_empty());
        assert_eq!(
            icon_url(42, "abcdef.jpg"),
            "https://shared.akamai.steamstatic.com/community_assets/images/apps/42/abcdef.jpg"
        );
    }
    #[test]
    fn achievement_schema_emits_current_square_icon_asset_path() {
        let catalog = parse_schema(&schema(), 42, "zh-CN").unwrap();
        assert_eq!(
            catalog.achievements[0].icon,
            "https://shared.akamai.steamstatic.com/community_assets/images/apps/42/abcd.jpg"
        );
        assert!(!catalog.achievements[0]
            .icon
            .contains("steamcommunity/public"));
    }
    #[test]
    fn stats_callback_requires_complete_matching_game_user_and_handle() {
        let bytes = [
            42_u64.to_le_bytes().to_vec(),
            1_i32.to_le_bytes().to_vec(),
            99_u64.to_le_bytes().to_vec(),
        ]
        .concat();
        assert_eq!(matching_stats_callback(1101, 7, &bytes, 42, 99, 7), Some(1));
        assert_eq!(
            matching_stats_callback(1101, 7, &bytes[..12], 42, 99, 7),
            None
        );
        assert_eq!(matching_stats_callback(1101, 7, &bytes, 43, 99, 7), None);
        assert_eq!(matching_stats_callback(1101, 7, &bytes, 42, 100, 7), None);
        assert_eq!(matching_stats_callback(1101, 8, &bytes, 42, 99, 7), None);
        assert_eq!(
            matching_stats_callback(1102, 7, &bytes[..12], 42, 99, 7),
            Some(1)
        );
        let wrong_game = [
            (42_u64 + (1 << 32)).to_le_bytes().to_vec(),
            bytes[8..].to_vec(),
        ]
        .concat();
        assert_eq!(
            matching_stats_callback(1101, 7, &wrong_game, 42, 99, 7),
            None
        );
    }
    #[test]
    fn entire_change_batch_validates_known_game_state_and_protection_before_write() {
        let mut catalog = parse_schema(&schema(), 42, "en").unwrap();
        let change = AchievementChange {
            id: "FIRST".into(),
            unlock: true,
        };
        assert!(validate_catalog_changes(&catalog, std::slice::from_ref(&change)).is_err());
        catalog.connected = true;
        catalog.writable = true;
        catalog.achievements[0].unlocked = Some(false);
        assert!(validate_catalog_changes(&catalog, std::slice::from_ref(&change)).is_err());
        catalog.achievements[0].protected = false;
        assert!(validate_catalog_changes(&catalog, std::slice::from_ref(&change)).is_ok());
        let other_game = AchievementChange {
            id: "OTHER_GAME_ID".into(),
            unlock: true,
        };
        assert!(validate_catalog_changes(&catalog, &[change, other_game]).is_err());
        assert_eq!(catalog.achievements[0].unlocked, Some(false));
    }
    #[test]
    fn localization_token_is_not_presented_as_a_language_or_title() {
        let node = Kv::Object(vec![("token".into(), Kv::Text("#internal_token".into()))]);
        assert_eq!(
            localized(Some(&node), "schinese", "FIRST"),
            ("FIRST".into(), "api-id".into())
        );
    }

    #[test]
    fn save_callback_failure_with_matching_fresh_readback_is_local_warning_not_row_failure() {
        let mut catalog = parse_schema(&schema(), 42, "en").unwrap();
        catalog.connected = true;
        catalog.writable = true;
        catalog.achievements[0].protected = false;
        catalog.achievements[0].unlocked = Some(false);
        let row = AchievementChangeResult {
            id: "FIRST".into(),
            unlock: true,
            success: false,
            local_verified: false,
            error: None,
        };
        let applied = reconcile_achievement_apply(
            &mut catalog,
            vec![row],
            true,
            Err("Steam callback 1102 rejected the request (EResult=2)".into()),
            Ok(()),
            vec![("FIRST".into(), Ok((true, 100)))],
        );
        assert!(!applied.confirmed && !applied.server_acknowledged);
        assert!(
            applied.local_verified,
            "Fresh readback matches; global save failure must not erase local verification"
        );
        assert!(applied.results[0].local_verified);
        assert!(
            applied.results[0].error.is_none(),
            "A batch save warning must not be duplicated as a per-achievement failure"
        );
        assert!(
            !applied.results[0].success,
            "Cloud acknowledgement remains unconfirmed"
        );
        assert!(applied.save_warning.unwrap().contains("EResult=2"));
        assert!(applied.catalog.error.is_none());
        assert_eq!(applied.catalog.achievements[0].unlocked, Some(true));
    }

    fn reconcile_fixture(
        unlock: bool,
        count: usize,
    ) -> (
        AchievementCatalog,
        Vec<AchievementChangeResult>,
        Vec<AchievementReadback>,
    ) {
        let mut catalog = parse_schema(&schema(), 42, "en").unwrap();
        catalog.connected = true;
        catalog.writable = true;
        let template = catalog.achievements[0].clone();
        catalog.achievements = (0..count)
            .map(|index| {
                let mut entry = template.clone();
                entry.id = format!("FIRST_{index}");
                entry.protected = false;
                entry.unlocked = Some(!unlock);
                entry
            })
            .collect();
        let results = catalog
            .achievements
            .iter()
            .map(|entry| AchievementChangeResult {
                id: entry.id.clone(),
                unlock,
                success: false,
                local_verified: false,
                error: None,
            })
            .collect();
        let readbacks = catalog
            .achievements
            .iter()
            .map(|entry| (entry.id.clone(), Ok((unlock, if unlock { 100 } else { 0 }))))
            .collect();
        (catalog, results, readbacks)
    }

    #[test]
    fn bulk_29_unlock_and_relock_store_failure_has_one_warning_and_29_locally_verified_rows() {
        for unlock in [true, false] {
            let (mut catalog, results, readbacks) = reconcile_fixture(unlock, 29);
            let applied = reconcile_achievement_apply(
                &mut catalog,
                results,
                true,
                Err("Steam callback 1102 rejected the request (EResult=2)".into()),
                Ok(()),
                readbacks,
            );
            assert_eq!(applied.results.len(), 29);
            assert!(applied.local_verified && !applied.confirmed && !applied.server_acknowledged);
            assert!(applied
                .results
                .iter()
                .all(|row| row.local_verified && !row.success && row.error.is_none()));
            assert!(applied
                .catalog
                .achievements
                .iter()
                .all(|entry| entry.unlocked == Some(unlock)));
            assert!(applied.save_warning.is_some() && applied.catalog.error.is_none());
        }
    }

    #[test]
    fn store_acknowledgement_and_fresh_readback_match_are_strict_success() {
        let (mut catalog, results, readbacks) = reconcile_fixture(true, 2);
        let applied =
            reconcile_achievement_apply(&mut catalog, results, true, Ok(()), Ok(()), readbacks);
        assert!(applied.confirmed && applied.local_verified && applied.server_acknowledged);
        assert!(applied
            .results
            .iter()
            .all(|row| row.success && row.local_verified && row.error.is_none()));
        assert!(applied.save_warning.is_none() && applied.catalog.error.is_none());
    }

    #[test]
    fn partial_readback_mismatch_reports_only_the_actual_failed_id() {
        let (mut catalog, results, mut readbacks) = reconcile_fixture(true, 2);
        readbacks[1].1 = Ok((false, 0));
        let applied = reconcile_achievement_apply(
            &mut catalog,
            results,
            true,
            Err("Steam callback 1102 rejected the request (EResult=2)".into()),
            Ok(()),
            readbacks,
        );
        assert!(!applied.confirmed && !applied.local_verified);
        assert!(applied.results[0].local_verified && applied.results[0].error.is_none());
        assert!(
            !applied.results[1].local_verified
                && applied.results[1]
                    .error
                    .as_ref()
                    .unwrap()
                    .contains("readback does not match")
        );
        assert_eq!(applied.catalog.achievements[1].unlocked, Some(false));
        assert!(applied.catalog.error.is_none());
    }

    #[test]
    fn failed_fresh_stats_or_individual_readback_never_reuse_stale_state_as_success() {
        let (mut catalog, results, readbacks) = reconcile_fixture(true, 2);
        let applied = reconcile_achievement_apply(
            &mut catalog,
            results,
            true,
            Ok(()),
            Err("Steam callback 1101 timed out".into()),
            readbacks,
        );
        assert!(!applied.confirmed && !applied.local_verified && !applied.catalog.writable);
        assert!(applied
            .catalog
            .achievements
            .iter()
            .all(|entry| entry.unlocked.is_none()));
        assert!(applied
            .results
            .iter()
            .all(|row| !row.success && !row.local_verified));
        assert!(applied.catalog.error.unwrap().contains("1101"));

        let (mut catalog, results, mut readbacks) = reconcile_fixture(true, 2);
        readbacks[1].1 = Err("Steam did not return achievement state".into());
        let applied =
            reconcile_achievement_apply(&mut catalog, results, true, Ok(()), Ok(()), readbacks);
        assert!(applied.results[0].local_verified && applied.results[0].success);
        assert!(!applied.results[1].local_verified && !applied.results[1].success);
        assert_eq!(applied.catalog.achievements[1].unlocked, None);
    }

    #[test]
    fn rejected_setter_is_not_claimed_as_our_success_even_when_state_now_matches() {
        let (mut catalog, mut results, readbacks) = reconcile_fixture(true, 1);
        results[0].error = Some("Steam rejected the achievement change".into());
        let applied =
            reconcile_achievement_apply(&mut catalog, results, false, Ok(()), Ok(()), readbacks);
        assert!(!applied.confirmed && !applied.local_verified && !applied.server_acknowledged);
        assert!(applied.results[0]
            .error
            .as_ref()
            .unwrap()
            .contains("rejected"));
        assert_eq!(applied.catalog.achievements[0].unlocked, Some(true));
    }

    #[test]
    fn no_op_matching_state_requires_no_store_but_does_not_invent_server_acknowledgement() {
        let (mut catalog, results, readbacks) = reconcile_fixture(true, 1);
        catalog.achievements[0].unlocked = Some(true);
        let applied =
            reconcile_achievement_apply(&mut catalog, results, false, Ok(()), Ok(()), readbacks);
        assert!(applied.confirmed && applied.local_verified && !applied.server_acknowledged);
        assert!(applied.save_warning.is_none());
    }

    #[test]
    fn stored_callback_reads_result_at_offset_8_with_or_without_tail_padding() {
        let payload = [42_u64.to_le_bytes().to_vec(), 2_i32.to_le_bytes().to_vec()].concat();
        assert_eq!(
            matching_stats_callback(1102, 7, &payload, 42, 99, 7),
            Some(2)
        );
        assert_eq!(
            matching_stats_callback(
                1102,
                7,
                &[payload.clone(), vec![0xff; 4]].concat(),
                42,
                99,
                7
            ),
            Some(2)
        );
        assert_eq!(
            matching_stats_callback(1102, 7, &payload[..11], 42, 99, 7),
            None
        );
    }
}
