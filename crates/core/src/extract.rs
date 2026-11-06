//! Current, already-logged-in Steam IPC session + selected local depot data.
//! No password/token login, credential extraction, third-party key service, or
//! speculative private-interface slots are used. Public SteamApps008 has no
//! GetDepotDecryptionKey: key material here is explicitly Steam's local config.
//! Altered native interface adaptation: Fluent/SamApi, Rick's zlib SAM API.
use crate::{
    steam::{SteamApp, SteamSnapshot},
    vdf,
};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
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
const MAX_DEPOTS: usize = 4096;
const MAX_ASSET_BYTES: usize = 128 * 1024 * 1024;
const MAX_IPC_BYTES: usize = 8 * 1024 * 1024;
static SEQUENCE: AtomicU64 = AtomicU64::new(0);
static EXTRACT_LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CurrentSessionGame {
    pub app_id: u32,
    pub name: String,
    pub installed: bool,
    pub subscribed: Option<bool>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtractionCapabilities {
    pub subscription_checks: bool,
    pub installed_depot_keys: bool,
    pub fresh_depot_key_requests: bool,
    pub local_manifests: bool,
    pub achievement_schema: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CurrentSteamSession {
    pub connected: bool,
    pub source: String,
    pub account_label: String,
    pub account_fingerprint: String,
    pub steam_path: String,
    pub games: Vec<CurrentSessionGame>,
    pub capabilities: ExtractionCapabilities,
    pub warnings: Vec<String>,
    pub error: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtractedDepot {
    pub depot_id: u32,
    pub manifest_id: Option<String>,
    pub key_available: bool,
    pub key_source: Option<String>,
    pub missing_reason: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtractionAsset {
    pub kind: String,
    pub filename: String,
    pub source_path: String,
    pub size_bytes: u64,
    pub sha256: String,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CurrentSessionExtraction {
    pub session: CurrentSteamSession,
    pub app_id: u32,
    pub app_name: String,
    pub subscribed: bool,
    pub scope: String,
    pub depots: Vec<ExtractedDepot>,
    pub keyed_depot_count: usize,
    pub lua: String,
    pub assets: Vec<ExtractionAsset>,
    pub complete: bool,
    pub warnings: Vec<String>,
}
impl std::fmt::Debug for CurrentSessionExtraction {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CurrentSessionExtraction")
            .field("app_id", &self.app_id)
            .field("keyed_depot_count", &self.keyed_depot_count)
            .field("depot_count", &self.depots.len())
            .field("lua", &"<REDACTED>")
            .field("asset_count", &self.assets.len())
            .field("complete", &self.complete)
            .finish()
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportedExtractionFile {
    pub name: String,
    pub kind: String,
    pub size_bytes: u64,
    pub sha256: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CurrentSessionExport {
    pub app_id: u32,
    pub destination_directory: String,
    pub files: Vec<ExportedExtractionFile>,
    pub keyed_depot_count: usize,
    pub warnings: Vec<String>,
}
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ExtractRequest {
    steam_path: PathBuf,
    app_id: Option<u32>,
    locale: String,
    expected_account_fingerprint: Option<String>,
    destination_directory: Option<PathBuf>,
}
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExtractResponse {
    session: Option<CurrentSteamSession>,
    extraction: Option<CurrentSessionExtraction>,
    exported: Option<CurrentSessionExport>,
    error: Option<String>,
}

fn root_path(path: Option<&Path>) -> Result<PathBuf> {
    let root = if let Some(path) = path {
        path.to_path_buf()
    } else {
        PathBuf::from(
            crate::scan_steam(None)?
                .steam
                .context("Steam was not found")?
                .path,
        )
    };
    let root = root
        .canonicalize()
        .context("Steam directory is unavailable")?;
    regular_file(&root.join("steam.exe"))?;
    Ok(root)
}
pub fn current_steam_session(
    exe: &Path,
    cache: &Path,
    steam: Option<&Path>,
) -> Result<CurrentSteamSession> {
    let response = invoke(
        exe,
        cache,
        ExtractRequest {
            steam_path: root_path(steam)?,
            app_id: None,
            locale: "en".into(),
            expected_account_fingerprint: None,
            destination_directory: None,
        },
    )?;
    response.session.context(
        response
            .error
            .unwrap_or_else(|| "Current Steam session worker returned no status".into()),
    )
}
pub fn extract_current_session(
    exe: &Path,
    cache: &Path,
    steam: Option<&Path>,
    app_id: u32,
    locale: &str,
    expected: Option<&str>,
) -> Result<CurrentSessionExtraction> {
    validate_app(app_id)?;
    let response = invoke(
        exe,
        cache,
        ExtractRequest {
            steam_path: root_path(steam)?,
            app_id: Some(app_id),
            locale: locale.into(),
            expected_account_fingerprint: expected.map(str::to_owned),
            destination_directory: None,
        },
    )?;
    response.extraction.context(
        response
            .error
            .unwrap_or_else(|| "Current Steam session worker returned no extraction".into()),
    )
}
pub fn export_current_session(
    exe: &Path,
    cache: &Path,
    steam: Option<&Path>,
    app_id: u32,
    locale: &str,
    expected: Option<&str>,
    destination: &Path,
) -> Result<CurrentSessionExport> {
    validate_app(app_id)?;
    let response = invoke(
        exe,
        cache,
        ExtractRequest {
            steam_path: root_path(steam)?,
            app_id: Some(app_id),
            locale: locale.into(),
            expected_account_fingerprint: expected.map(str::to_owned),
            destination_directory: Some(destination.to_path_buf()),
        },
    )?;
    response.exported.context(
        response
            .error
            .unwrap_or_else(|| "Current Steam session worker returned no export".into()),
    )
}
fn validate_app(app_id: u32) -> Result<()> {
    if app_id == 0 {
        bail!("A nonzero AppID is required");
    }
    Ok(())
}
fn fingerprint(identity: u64) -> String {
    format!("{:x}", Sha256::digest(identity.to_le_bytes()))[..16].into()
}
fn check_account(identity: u64, expected: &str) -> Result<()> {
    if identity == 0 || fingerprint(identity) != expected {
        bail!("The current Steam account changed or signed out; reconnect before extraction or export");
    }
    Ok(())
}
fn regular_file(path: &Path) -> Result<()> {
    let metadata =
        fs::symlink_metadata(path).context("Required local Steam file is unavailable")?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        bail!("Local resource must be a regular file, not a link");
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            bail!("Reparse-point resource is not accepted");
        }
    }
    Ok(())
}
fn bounded_read(path: &Path, maximum: usize) -> Result<Vec<u8>> {
    regular_file(path)?;
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take((maximum + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > maximum {
        bail!("Local resource exceeds its inspection limit");
    }
    Ok(bytes)
}
fn safe_resource(root: &Path, path: &Path) -> Result<()> {
    regular_file(path)?;
    if !path.canonicalize()?.starts_with(root.canonicalize()?) {
        bail!("Local resource resolves outside the selected Steam library");
    }
    Ok(())
}
struct IpcFiles(PathBuf);
impl Drop for IpcFiles {
    fn drop(&mut self) {
        for file in ["request.json", "response.json"] {
            let _ = fs::remove_file(self.0.join(file));
        }
        let _ = fs::remove_dir(&self.0);
    }
}
fn invoke(exe: &Path, cache: &Path, request: ExtractRequest) -> Result<ExtractResponse> {
    let _lock = EXTRACT_LOCK
        .lock()
        .map_err(|_| anyhow::anyhow!("Extraction worker lock is poisoned"))?;
    fs::create_dir_all(cache)?;
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let files = IpcFiles(cache.join(format!(
        "extract-{}-{stamp}-{}",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    )));
    fs::create_dir(&files.0)?;
    let input = files.0.join("request.json");
    let output = files.0.join("response.json");
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&input)?;
    file.write_all(&serde_json::to_vec(&request)?)?;
    file.sync_all()?;
    drop(file);
    let mut command = Command::new(exe);
    command
        .arg("--extract-worker")
        .arg(&input)
        .arg(&output)
        .env("SteamAppId", "0")
        .env("SteamGameId", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    let mut child = command
        .spawn()
        .context("Could not start current Steam session worker")?;
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    bail!("Current Steam session worker exited with {status}");
                }
                break;
            }
            Ok(None) => {}
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error.into());
            }
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("Current Steam session worker timed out");
        }
        thread::sleep(Duration::from_millis(25));
    }
    // This response may contain the explicitly requested Lua; never log it.
    serde_json::from_slice(&bounded_read(&output, MAX_IPC_BYTES)?)
        .context("Invalid extraction worker response")
}
pub fn run_extract_worker(input: &Path, output: &Path) -> Result<()> {
    let request: ExtractRequest = serde_json::from_slice(&bounded_read(input, MAX_IPC_BYTES)?)?;
    if let Some(app_id) = request.app_id {
        validate_app(app_id)?;
    }
    let response = execute(&request).unwrap_or_else(|error| ExtractResponse {
        session: None,
        extraction: None,
        exported: None,
        error: Some(error.to_string()),
    });
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(output)?;
    file.write_all(&serde_json::to_vec(&response)?)?;
    file.sync_all()?;
    Ok(())
}

trait LiveSession {
    fn identity(&self) -> Result<u64>;
    fn subscribed(&self, app_id: u32) -> Result<bool>;
    fn installed(&self, app_id: u32) -> Result<bool>;
    fn installed_depots(&self, app_id: u32) -> Result<Vec<u32>>;
}
fn disconnected(root: &Path, error: String) -> CurrentSteamSession {
    CurrentSteamSession {
        connected: false,
        source: "steamClient".into(),
        account_label: String::new(),
        account_fingerprint: String::new(),
        steam_path: root.to_string_lossy().into_owned(),
        games: Vec::new(),
        capabilities: ExtractionCapabilities {
            subscription_checks: false,
            installed_depot_keys: false,
            fresh_depot_key_requests: false,
            local_manifests: false,
            achievement_schema: false,
        },
        warnings: Vec::new(),
        error: Some(error),
    }
}
fn account_label(root: &Path, identity: u64) -> String {
    let path = root.join("config").join("loginusers.vdf");
    if safe_resource(root, &path).is_err() {
        return String::new();
    }
    vdf::read(&path)
        .ok()
        .and_then(|root| {
            root.get("users")?
                .get(&identity.to_string())?
                .string("PersonaName")
                .map(str::to_owned)
        })
        .filter(|name| name.len() <= 256)
        .unwrap_or_default()
}
fn inspect_session<L: LiveSession>(
    root: &Path,
    snapshot: &SteamSnapshot,
    live: &L,
) -> Result<CurrentSteamSession> {
    let id = live.identity()?;
    if id == 0 {
        bail!("Sign in to the Steam client before connecting extraction");
    }
    let expected = fingerprint(id);
    let mut games = Vec::new();
    for app in snapshot.apps.iter().take(5000) {
        games.push(CurrentSessionGame {
            app_id: app.appid,
            name: app.name.clone(),
            installed: live.installed(app.appid)?,
            subscribed: Some(live.subscribed(app.appid)?),
        });
    }
    check_account(live.identity()?, &expected)?;
    Ok(CurrentSteamSession { connected: true, source: "steamClient".into(), account_label: account_label(root, id), account_fingerprint: expected, steam_path: root.to_string_lossy().into_owned(), games,
        capabilities: ExtractionCapabilities { subscription_checks: true, installed_depot_keys: true, fresh_depot_key_requests: false, local_manifests: true, achievement_schema: true },
        warnings: vec!["Games listed here are locally known installed games, not the account's complete owned library".into(), "Public SteamApps008 has no fresh depot-key request API; keys are read only from selected local Steam config records".into()], error: None })
}
fn execute(request: &ExtractRequest) -> Result<ExtractResponse> {
    let root = root_path(Some(&request.steam_path))?;
    #[cfg(all(windows, target_pointer_width = "64"))]
    {
        let live = match native::Session::connect(&root) {
            Ok(live) => live,
            Err(error) if request.app_id.is_none() => {
                return Ok(ExtractResponse {
                    session: Some(disconnected(&root, error.to_string())),
                    extraction: None,
                    exported: None,
                    error: None,
                })
            }
            Err(error) => return Err(error),
        };
        let snapshot = crate::scan_steam(Some(&root))?;
        let session = inspect_session(&root, &snapshot, &live)?;
        if let Some(expected) = &request.expected_account_fingerprint {
            check_account(live.identity()?, expected)?;
        }
        if let Some(app_id) = request.app_id {
            let extraction =
                extract_local(&root, &snapshot, &live, session, app_id, &request.locale)?;
            if let Some(destination) = &request.destination_directory {
                let exported = export_bundle(&root, &snapshot, &live, &extraction, destination)?;
                return Ok(ExtractResponse {
                    session: None,
                    extraction: None,
                    exported: Some(exported),
                    error: None,
                });
            }
            return Ok(ExtractResponse {
                session: None,
                extraction: Some(extraction),
                exported: None,
                error: None,
            });
        }
        Ok(ExtractResponse {
            session: Some(session),
            extraction: None,
            exported: None,
            error: None,
        })
    }
    #[cfg(not(all(windows, target_pointer_width = "64")))]
    {
        Ok(ExtractResponse {
            session: Some(disconnected(
                &root,
                "Current Steam IPC extraction requires the Windows x64 desktop build".into(),
            )),
            extraction: None,
            exported: None,
            error: None,
        })
    }
}
fn installed_records(app: Option<&SteamApp>) -> Result<BTreeMap<u32, Option<String>>> {
    let mut result = BTreeMap::new();
    let Some(app) = app else {
        return Ok(result);
    };
    let path = Path::new(&app.manifest_path);
    safe_resource(Path::new(&app.library_path), path)?;
    let root = vdf::read(path)?;
    let state = root
        .get("AppState")
        .context("Installed manifest has no AppState")?;
    if state.string("appid").and_then(|id| id.parse::<u32>().ok()) != Some(app.appid) {
        bail!("Installed manifest AppID mismatch");
    }
    for (key, value) in state
        .get("InstalledDepots")
        .and_then(vdf::Value::entries)
        .unwrap_or(&[])
    {
        let Ok(id) = key.parse::<u32>() else { continue };
        if id == 0 {
            continue;
        }
        let manifest = value
            .string("manifest")
            .and_then(|id| id.parse::<u64>().ok())
            .filter(|id| *id != 0)
            .map(|id| id.to_string());
        result.insert(id, manifest);
        if result.len() > MAX_DEPOTS {
            bail!("Installed depot count exceeds limit");
        }
    }
    Ok(result)
}
fn selected_local_keys(root: &Path, selected: &BTreeSet<u32>) -> Result<BTreeMap<u32, String>> {
    let path = root.join("config").join("config.vdf");
    safe_resource(root, &path)?;
    let config = vdf::read(&path)?;
    let depots = config
        .get("InstallConfigStore")
        .and_then(|node| node.get("Software"))
        .and_then(|node| node.get("Valve"))
        .and_then(|node| node.get("Steam"))
        .and_then(|node| node.get("depots"));
    let mut keys = BTreeMap::new();
    for id in selected {
        let key = depots
            .and_then(|depots| depots.get(&id.to_string()))
            .and_then(|depot| depot.string("DecryptionKey"));
        if let Some(key) =
            key.filter(|key| key.len() == 64 && key.bytes().all(|byte| byte.is_ascii_hexdigit()))
        {
            keys.insert(*id, key.to_ascii_lowercase());
        }
    }
    Ok(keys)
}
fn asset(root: &Path, path: PathBuf, kind: &str, limit: usize) -> Result<ExtractionAsset> {
    safe_resource(root, &path)?;
    let bytes = bounded_read(&path, limit)?;
    if kind == "manifest" && !bytes.starts_with(&[0xd0, 0x17, 0xf6, 0x71]) {
        bail!("Local manifest has invalid magic");
    }
    Ok(ExtractionAsset {
        kind: kind.into(),
        filename: path
            .file_name()
            .context("Resource filename is missing")?
            .to_string_lossy()
            .into_owned(),
        source_path: path.to_string_lossy().into_owned(),
        size_bytes: bytes.len() as u64,
        sha256: format!("{:x}", Sha256::digest(&bytes)),
    })
}
fn render_lua(
    app_id: u32,
    name: &str,
    records: &BTreeMap<u32, Option<String>>,
    keys: &BTreeMap<u32, String>,
) -> String {
    let mut lua = format!("-- {}\n-- Source: current Steam IPC subscription + steamLocalConfig keys\n-- Scope: installed depot records; no fresh depot-key server request\naddappid({app_id})\n", name.replace(['\r', '\n'], " "));
    for (depot, manifest) in records {
        if let Some(key) = keys.get(depot) {
            lua.push_str(&format!("addappid({depot}, 1, \"{key}\")\n"));
            if let Some(manifest) = manifest {
                lua.push_str(&format!("setManifestid({depot}, \"{manifest}\")\n"));
            }
        }
    }
    lua
}
fn extract_local<L: LiveSession>(
    root: &Path,
    snapshot: &SteamSnapshot,
    live: &L,
    session: CurrentSteamSession,
    app_id: u32,
    locale: &str,
) -> Result<CurrentSessionExtraction> {
    validate_app(app_id)?;
    check_account(live.identity()?, &session.account_fingerprint)?;
    if !live.subscribed(app_id)? {
        bail!("The current Steam client does not report a subscription for the selected AppID");
    }
    let app = snapshot.apps.iter().find(|app| app.appid == app_id);
    let name = app
        .map(|app| app.name.clone())
        .unwrap_or_else(|| format!("AppID {app_id}"));
    let mut records = installed_records(app)?;
    for depot in live.installed_depots(app_id)? {
        if depot != 0 {
            records.entry(depot).or_insert(None);
        }
    }
    if records.len() > MAX_DEPOTS {
        bail!("Installed depot count exceeds limit");
    }
    let selected: BTreeSet<_> = records.keys().copied().collect();
    let mut warnings = vec!["Depot keys are selected local Steam config cache records; they are not freshly fetched private account keys".into()];
    if !live.installed(app_id)? {
        warnings.push("Steam does not currently report this game as installed; only existing local records can be exported".into());
    }
    let keys = selected_local_keys(root, &selected).unwrap_or_else(|_| {
        warnings.push("Steam local config depot keys are unavailable or invalid".into());
        BTreeMap::new()
    });
    let depots: Vec<_> = records
        .iter()
        .map(|(id, manifest)| ExtractedDepot {
            depot_id: *id,
            manifest_id: manifest.clone(),
            key_available: keys.contains_key(id),
            key_source: keys.contains_key(id).then(|| "steamLocalConfig".into()),
            missing_reason: (!keys.contains_key(id)).then(|| {
                "No valid 32-byte key exists in the selected Steam local depot record".into()
            }),
        })
        .collect();
    if records.is_empty() {
        warnings.push("No installed depot records were found for this game; a complete depot extraction is not claimed".into());
    }
    if keys.len() != records.len() {
        warnings.push(format!(
            "{} of {} installed depots have usable local keys",
            keys.len(),
            records.len()
        ));
    }
    let mut assets = Vec::new();
    let schema = root
        .join("appcache")
        .join("stats")
        .join(format!("UserGameStatsSchema_{app_id}.bin"));
    if schema.exists() {
        match crate::read_achievement_schema(&schema, app_id, locale)
            .and_then(|_| asset(root, schema, "achievementSchema", MAX_IPC_BYTES))
        {
            Ok(asset) => assets.push(asset),
            Err(_) => warnings
                .push("Existing achievement schema could not be validated for this AppID".into()),
        }
    }
    for (depot, manifest) in &records {
        if let Some(manifest) = manifest {
            let filename = format!("{depot}_{manifest}.manifest");
            let mut roots = vec![root.to_path_buf()];
            roots.extend(
                snapshot
                    .libraries
                    .iter()
                    .map(|library| PathBuf::from(&library.path)),
            );
            for library_root in roots {
                let path = library_root.join("depotcache").join(&filename);
                if path.is_file() {
                    match asset(&library_root, path, "manifest", MAX_ASSET_BYTES) {
                        Ok(asset) => assets.push(asset),
                        Err(_) => warnings.push(format!(
                            "Local manifest for depot {depot} could not be validated"
                        )),
                    };
                    break;
                }
            }
        }
    }
    check_account(live.identity()?, &session.account_fingerprint)?;
    if !live.subscribed(app_id)? {
        bail!("Steam subscription changed during extraction; reconnect before retrying");
    }
    let lua = render_lua(app_id, &name, &records, &keys);
    Ok(CurrentSessionExtraction {
        session,
        app_id,
        app_name: name,
        subscribed: true,
        scope: "installedDepots".into(),
        complete: !records.is_empty() && keys.len() == records.len(),
        keyed_depot_count: keys.len(),
        lua,
        depots,
        assets,
        warnings,
    })
}

fn reject_reparse_ancestors(path: &Path) -> Result<()> {
    for component in path.ancestors() {
        let metadata = fs::symlink_metadata(component)?;
        if metadata.file_type().is_symlink() {
            bail!("Export directory must not use symlinks");
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if metadata.file_attributes() & 0x400 != 0 {
                bail!("Export directory must not use reparse points");
            }
        }
    }
    Ok(())
}
struct NewBundle {
    directory: PathBuf,
    names: Vec<String>,
    committed: bool,
}
impl Drop for NewBundle {
    fn drop(&mut self) {
        if !self.committed {
            for name in &self.names {
                let _ = fs::remove_file(self.directory.join(name));
            }
            let _ = fs::remove_dir(&self.directory);
        }
    }
}
impl NewBundle {
    fn write(&mut self, name: &str, kind: &str, bytes: &[u8]) -> Result<ExportedExtractionFile> {
        if name.contains(['/', '\\', ':', '\0']) || name == "." || name == ".." {
            bail!("Invalid export filename");
        }
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(self.directory.join(name))?;
        self.names.push(name.into());
        file.write_all(bytes)?;
        file.sync_all()?;
        Ok(ExportedExtractionFile {
            name: name.into(),
            kind: kind.into(),
            size_bytes: bytes.len() as u64,
            sha256: format!("{:x}", Sha256::digest(bytes)),
        })
    }
}
fn export_bundle<L: LiveSession>(
    root: &Path,
    snapshot: &SteamSnapshot,
    live: &L,
    extraction: &CurrentSessionExtraction,
    destination: &Path,
) -> Result<CurrentSessionExport> {
    check_account(live.identity()?, &extraction.session.account_fingerprint)?;
    reject_reparse_ancestors(destination)?;
    let parent = destination
        .canonicalize()
        .context("Select an existing export directory")?;
    if !parent.is_dir() || parent.starts_with(root.canonicalize()?) {
        bail!("Choose an export directory outside the Steam installation");
    }
    for library in &snapshot.libraries {
        let steamapps = PathBuf::from(&library.path).join("steamapps");
        if steamapps.exists() && parent.starts_with(steamapps.canonicalize()?) {
            bail!("Choose an export directory outside Steam's game/resource directories");
        }
    }
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let directory = parent.join(format!(
        "SteamToolsNext-{}-{stamp}-{}",
        extraction.app_id,
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&directory)?;
    let mut bundle = NewBundle {
        directory,
        names: Vec::new(),
        committed: false,
    };
    let mut files = vec![bundle.write(
        &format!("{}.lua", extraction.app_id),
        "lua",
        extraction.lua.as_bytes(),
    )?];
    let mut total = extraction.lua.len();
    for resource in &extraction.assets {
        let bytes = bounded_read(Path::new(&resource.source_path), MAX_ASSET_BYTES)?;
        total += bytes.len();
        if total > 256 * 1024 * 1024 {
            bail!("Extraction bundle exceeds 256 MiB limit");
        }
        if format!("{:x}", Sha256::digest(&bytes)) != resource.sha256 {
            bail!("A local resource changed during export; refresh extraction");
        }
        files.push(bundle.write(&resource.filename, &resource.kind, &bytes)?);
    }
    check_account(live.identity()?, &extraction.session.account_fingerprint)?;
    if !live.subscribed(extraction.app_id)? {
        bail!("Steam subscription changed during export");
    }
    let mut receipt = CurrentSessionExport {
        app_id: extraction.app_id,
        destination_directory: bundle.directory.to_string_lossy().into_owned(),
        files,
        keyed_depot_count: extraction.keyed_depot_count,
        warnings: extraction.warnings.clone(),
    };
    // Only nonsecret file inventory goes into the summary, never the source config or Lua.
    let summary_file = bundle.write(
        "extraction-summary.json",
        "summary",
        &serde_json::to_vec_pretty(&receipt)?,
    )?;
    receipt.files.push(summary_file);
    check_account(live.identity()?, &extraction.session.account_fingerprint)?;
    bundle.committed = true;
    Ok(receipt)
}

#[cfg(all(windows, target_pointer_width = "64"))]
mod native {
    use super::*;
    use crate::achievements::{wait_for_steam_user_ready, SteamUserReadinessProbe};
    use std::{
        ffi::{c_char, c_void},
        os::windows::ffi::OsStrExt,
        ptr,
    };
    type Object = *mut c_void;
    #[repr(C)]
    struct CallbackMessage {
        user: i32,
        id: i32,
        data: *const u8,
        size: i32,
    }
    type GetCallback = unsafe extern "C" fn(i32, *mut CallbackMessage, *mut i32) -> bool;
    type FreeCallback = unsafe extern "C" fn(i32) -> bool;
    #[link(name = "kernel32")]
    extern "system" {
        fn LoadLibraryExW(name: *const u16, file: Object, flags: u32) -> Object;
        fn GetProcAddress(module: Object, name: *const c_char) -> Object;
        fn FreeLibrary(module: Object) -> i32;
    }
    struct Module(Object);
    impl Drop for Module {
        fn drop(&mut self) {
            unsafe {
                FreeLibrary(self.0);
            }
        }
    }
    unsafe fn method<T: Copy>(object: Object, slot: usize) -> T {
        let table = *(object as *const *const Object);
        std::mem::transmute_copy(&*table.add(slot))
    }
    pub(super) struct Session {
        _module: Module,
        client: Object,
        user_api: Object,
        apps: Object,
        pipe: i32,
        user: i32,
        get_callback: GetCallback,
        free_callback: FreeCallback,
    }
    impl Drop for Session {
        fn drop(&mut self) {
            unsafe {
                if self.user != 0 {
                    let release: unsafe extern "system" fn(Object, i32, i32) =
                        method(self.client, 4);
                    release(self.client, self.pipe, self.user);
                }
                if self.pipe != 0 {
                    let release: unsafe extern "system" fn(Object, i32) -> bool =
                        method(self.client, 1);
                    release(self.client, self.pipe);
                }
            }
        }
    }
    impl Session {
        pub(super) fn connect(root: &Path) -> Result<Self> {
            unsafe {
                let path = [
                    root.join("steamclient64.dll"),
                    root.join("bin").join("steamclient64.dll"),
                ]
                .into_iter()
                .find(|path| path.is_file())
                .context("steamclient64.dll was not found")?;
                safe_resource(root, &path)?;
                let wide: Vec<_> = path.as_os_str().encode_wide().chain(Some(0)).collect();
                let handle = LoadLibraryExW(wide.as_ptr(), ptr::null_mut(), 8);
                if handle.is_null() {
                    bail!(
                        "Steam client DLL load failed: {}",
                        std::io::Error::last_os_error()
                    );
                }
                let module = Module(handle);
                let get_callback_address = GetProcAddress(handle, c"Steam_BGetCallback".as_ptr());
                let free_callback_address =
                    GetProcAddress(handle, c"Steam_FreeLastCallback".as_ptr());
                if get_callback_address.is_null() || free_callback_address.is_null() {
                    bail!("Steam IPC callback exports are unavailable");
                }
                let get_callback: GetCallback = std::mem::transmute(get_callback_address);
                let free_callback: FreeCallback = std::mem::transmute(free_callback_address);
                let address = GetProcAddress(handle, c"CreateInterface".as_ptr());
                if address.is_null() {
                    bail!("Steam CreateInterface export is unavailable");
                }
                let create: unsafe extern "C" fn(*const c_char, *mut i32) -> Object =
                    std::mem::transmute(address);
                let client = create(c"SteamClient018".as_ptr(), ptr::null_mut());
                if client.is_null() {
                    bail!("SteamClient018 is unavailable");
                }
                let mut result = Self {
                    _module: module,
                    client,
                    user_api: ptr::null_mut(),
                    apps: ptr::null_mut(),
                    pipe: 0,
                    user: 0,
                    get_callback,
                    free_callback,
                };
                let create_pipe: unsafe extern "system" fn(Object) -> i32 = method(client, 0);
                result.pipe = create_pipe(client);
                if result.pipe == 0 {
                    bail!("Steam is not running or its IPC pipe is unavailable");
                }
                let connect: unsafe extern "system" fn(Object, i32) -> i32 = method(client, 2);
                result.user = connect(client, result.pipe);
                if result.user == 0 {
                    bail!("Sign in to the existing Steam client account first");
                }
                let get_user: unsafe extern "system" fn(Object, i32, i32, *const c_char) -> Object =
                    method(client, 5);
                result.user_api =
                    get_user(client, result.user, result.pipe, c"SteamUser012".as_ptr());
                let get_apps: unsafe extern "system" fn(Object, i32, i32, *const c_char) -> Object =
                    method(client, 15);
                result.apps = get_apps(
                    client,
                    result.user,
                    result.pipe,
                    c"STEAMAPPS_INTERFACE_VERSION008".as_ptr(),
                );
                if result.user_api.is_null() || result.apps.is_null() {
                    bail!("Steam current-session public interfaces are unavailable");
                }
                let logged: unsafe extern "system" fn(Object) -> bool = method(result.user_api, 1);
                let get_id: unsafe extern "system" fn(Object, *mut u64) =
                    method(result.user_api, 2);
                wait_for_steam_user_ready(
                    400,
                    || {
                        let mut steam_id = 0;
                        get_id(result.user_api, &mut steam_id);
                        Ok(SteamUserReadinessProbe {
                            logged_on: logged(result.user_api),
                            steam_id,
                        })
                    },
                    || result.pump(),
                    || thread::sleep(Duration::from_millis(20)),
                )?;
                Ok(result)
            }
        }
        fn pump(&self) -> Result<()> {
            unsafe {
                for _ in 0..10_000 {
                    let mut callback = CallbackMessage {
                        user: 0,
                        id: 0,
                        data: ptr::null(),
                        size: 0,
                    };
                    let mut call = 0;
                    if !(self.get_callback)(self.pipe, &mut callback, &mut call) {
                        return Ok(());
                    }
                    (self.free_callback)(self.pipe);
                }
            }
            bail!("Steam startup callback queue exceeded inspection limit")
        }
    }
    impl LiveSession for Session {
        fn identity(&self) -> Result<u64> {
            unsafe {
                let logged: unsafe extern "system" fn(Object) -> bool = method(self.user_api, 1);
                if !logged(self.user_api) {
                    bail!("The Steam client account is signed out or offline");
                }
                let get: unsafe extern "system" fn(Object, *mut u64) = method(self.user_api, 2);
                let mut id = 0;
                get(self.user_api, &mut id);
                if id == 0 {
                    bail!("Steam current user identity is unavailable");
                }
                Ok(id)
            }
        }
        fn subscribed(&self, app_id: u32) -> Result<bool> {
            unsafe {
                let call: unsafe extern "system" fn(Object, u32) -> bool = method(self.apps, 6);
                Ok(call(self.apps, app_id))
            }
        }
        fn installed(&self, app_id: u32) -> Result<bool> {
            unsafe {
                let call: unsafe extern "system" fn(Object, u32) -> bool = method(self.apps, 19);
                Ok(call(self.apps, app_id))
            }
        }
        fn installed_depots(&self, app_id: u32) -> Result<Vec<u32>> {
            unsafe {
                let call: unsafe extern "system" fn(Object, u32, *mut u32, u32) -> u32 =
                    method(self.apps, 17);
                let mut depots = vec![0; MAX_DEPOTS];
                let count =
                    call(self.apps, app_id, depots.as_mut_ptr(), MAX_DEPOTS as u32) as usize;
                if count > MAX_DEPOTS {
                    bail!("Steam installed depot count exceeds limit");
                }
                depots.truncate(count);
                Ok(depots)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    struct FakeSession {
        id: Cell<u64>,
        subscribed: bool,
        switch_on_call: Option<usize>,
        calls: Cell<usize>,
    }
    impl LiveSession for FakeSession {
        fn identity(&self) -> Result<u64> {
            let call = self.calls.get() + 1;
            self.calls.set(call);
            if self.switch_on_call == Some(call) {
                self.id.set(100);
            }
            Ok(self.id.get())
        }
        fn subscribed(&self, _: u32) -> Result<bool> {
            Ok(self.subscribed)
        }
        fn installed(&self, _: u32) -> Result<bool> {
            Ok(true)
        }
        fn installed_depots(&self, _: u32) -> Result<Vec<u32>> {
            Ok(vec![11, 12])
        }
    }
    fn fake(subscribed: bool) -> FakeSession {
        FakeSession {
            id: Cell::new(99),
            subscribed,
            switch_on_call: None,
            calls: Cell::new(0),
        }
    }
    fn fixture() -> (tempfile::TempDir, SteamSnapshot) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::create_dir_all(root.join("config")).unwrap();
        fs::create_dir_all(root.join("steamapps")).unwrap();
        fs::write(root.join("steam.exe"), b"fixture").unwrap();
        fs::write(root.join("config/config.vdf"), format!("\"InstallConfigStore\"{{\"Software\"{{\"Valve\"{{\"Steam\"{{\"depots\"{{\"11\"{{\"DecryptionKey\"\"{}\"}}\"12\"{{\"DecryptionKey\"\"invalid\"}}\"13\"{{\"DecryptionKey\"\"{}\"}}}}\"RefreshToken\"\"fixture-secret-must-not-serialize\"}}}}}}}}", "11".repeat(32), "22".repeat(32))).unwrap();
        let acf = root.join("steamapps/appmanifest_42.acf");
        fs::write(
            &acf,
            "\"AppState\"{\"appid\"\"42\"\"InstalledDepots\"{\"11\"{\"manifest\"\"123\"}}}",
        )
        .unwrap();
        let app = SteamApp {
            appid: 42,
            name: "Fixture".into(),
            install_dir: String::new(),
            manifest_path: acf.to_string_lossy().into_owned(),
            library_path: root.to_string_lossy().into_owned(),
            size_bytes: 0,
            state_flags: 4,
            last_updated: 0,
            last_played: 0,
        };
        let snapshot = SteamSnapshot {
            captured_at: 0,
            locale: "en".into(),
            steam: None,
            libraries: vec![],
            apps: vec![app],
            warnings: vec![],
        };
        (temp, snapshot)
    }
    #[test]
    fn only_selected_valid_local_keys_are_extracted_and_not_credentials() {
        let (temp, _) = fixture();
        let keys = selected_local_keys(temp.path(), &BTreeSet::from([11, 12])).unwrap();
        assert_eq!(keys.len(), 1);
        assert!(keys.contains_key(&11));
        assert!(!keys.contains_key(&13));
        assert!(!format!("{keys:?}").contains("fixture-secret"));
    }
    #[test]
    fn current_session_status_is_live_not_cached_login_file_presence() {
        let (temp, snapshot) = fixture();
        let session = inspect_session(temp.path(), &snapshot, &fake(true)).unwrap();
        assert!(session.connected);
        assert_eq!(session.games[0].subscribed, Some(true));
        assert!(!session.capabilities.fresh_depot_key_requests);
        assert_eq!(session.account_fingerprint, fingerprint(99));
    }
    #[test]
    fn extraction_reports_real_local_source_and_missing_keys_without_public_cache() {
        let (temp, snapshot) = fixture();
        let live = fake(true);
        let session = inspect_session(temp.path(), &snapshot, &live).unwrap();
        let result = extract_local(temp.path(), &snapshot, &live, session, 42, "en").unwrap();
        assert_eq!(result.keyed_depot_count, 1);
        assert!(!result.complete);
        assert_eq!(
            result.depots[0].key_source.as_deref(),
            Some("steamLocalConfig")
        );
        assert!(result.lua.contains("addappid(11, 1,"));
        assert!(!result.lua.contains("addappid(12, 1,"));
        assert!(!result.lua.contains("fixture-secret"));
    }
    #[test]
    fn missing_subscription_and_account_switch_abort_extraction() {
        let (temp, snapshot) = fixture();
        let denied = fake(false);
        let session = inspect_session(temp.path(), &snapshot, &denied).unwrap();
        assert!(extract_local(temp.path(), &snapshot, &denied, session, 42, "en").is_err());
        let live = fake(true);
        let session = inspect_session(temp.path(), &snapshot, &live).unwrap();
        live.id.set(100);
        assert!(extract_local(temp.path(), &snapshot, &live, session, 42, "en").is_err());
        assert!(check_account(99, &fingerprint(100)).is_err());
    }
    #[test]
    fn account_switch_during_extraction_is_detected_before_secret_result_commit() {
        let (temp, snapshot) = fixture();
        let live = FakeSession {
            switch_on_call: Some(4),
            ..fake(true)
        };
        let session = inspect_session(temp.path(), &snapshot, &live).unwrap();
        assert!(extract_local(temp.path(), &snapshot, &live, session, 42, "en").is_err());
    }
    #[test]
    fn explicit_bundle_export_is_separate_and_summary_has_no_raw_keys_or_lua() {
        let (temp, snapshot) = fixture();
        let destination = tempfile::tempdir().unwrap();
        let live = fake(true);
        let session = inspect_session(temp.path(), &snapshot, &live).unwrap();
        let result = extract_local(temp.path(), &snapshot, &live, session, 42, "en").unwrap();
        let receipt =
            export_bundle(temp.path(), &snapshot, &live, &result, destination.path()).unwrap();
        let summary = fs::read_to_string(
            Path::new(&receipt.destination_directory).join("extraction-summary.json"),
        )
        .unwrap();
        assert!(!summary.contains(&"11".repeat(32)));
        assert!(!summary.contains("fixture-secret"));
        assert!(!summary.contains("addappid("));
        assert!(Path::new(&receipt.destination_directory)
            .join("42.lua")
            .exists());
        assert!(!temp.path().join("42.lua").exists());
    }
    #[test]
    fn export_refuses_steam_directory_and_rolls_back_its_new_files_on_account_switch() {
        let (temp, snapshot) = fixture();
        let live = fake(true);
        let session = inspect_session(temp.path(), &snapshot, &live).unwrap();
        let result = extract_local(temp.path(), &snapshot, &live, session, 42, "en").unwrap();
        assert!(export_bundle(temp.path(), &snapshot, &live, &result, temp.path()).is_err());
        let destination = tempfile::tempdir().unwrap();
        let switching = FakeSession {
            switch_on_call: Some(2),
            ..fake(true)
        };
        assert!(export_bundle(
            temp.path(),
            &snapshot,
            &switching,
            &result,
            destination.path()
        )
        .is_err());
        assert_eq!(fs::read_dir(destination.path()).unwrap().count(), 0);
    }
    #[test]
    fn bundle_copies_only_selected_validated_assets_and_rejects_changed_file() {
        let (temp, snapshot) = fixture();
        let destination = tempfile::tempdir().unwrap();
        let live = fake(true);
        let session = inspect_session(temp.path(), &snapshot, &live).unwrap();
        let mut result = extract_local(temp.path(), &snapshot, &live, session, 42, "en").unwrap();
        let manifest = temp.path().join("11_123.manifest");
        fs::write(&manifest, [0xd0, 0x17, 0xf6, 0x71, 0, 1, 2, 3]).unwrap();
        result
            .assets
            .push(asset(temp.path(), manifest.clone(), "manifest", MAX_ASSET_BYTES).unwrap());
        let receipt =
            export_bundle(temp.path(), &snapshot, &live, &result, destination.path()).unwrap();
        assert_eq!(receipt.files.len(), 3);
        assert_eq!(
            fs::read(Path::new(&receipt.destination_directory).join("11_123.manifest")).unwrap(),
            fs::read(&manifest).unwrap()
        );
        let changed_destination = tempfile::tempdir().unwrap();
        fs::write(&manifest, [0xd0, 0x17, 0xf6, 0x71, 9]).unwrap();
        assert!(export_bundle(
            temp.path(),
            &snapshot,
            &live,
            &result,
            changed_destination.path()
        )
        .is_err());
        assert_eq!(fs::read_dir(changed_destination.path()).unwrap().count(), 0);
    }
    #[test]
    fn debug_output_redacts_lua_key_material() {
        let (temp, snapshot) = fixture();
        let live = fake(true);
        let session = inspect_session(temp.path(), &snapshot, &live).unwrap();
        let result = extract_local(temp.path(), &snapshot, &live, session, 42, "en").unwrap();
        let debug = format!("{result:?}");
        assert!(debug.contains("REDACTED"));
        assert!(!debug.contains(&"11".repeat(32)));
        assert!(!debug.contains("addappid("));
    }
}
