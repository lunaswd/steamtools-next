//! Fixed-source, verified Windows desktop updates with an out-of-process helper.
//!
//! The running executable is never overwritten in-process. A verified copy of
//! it acknowledges the prepared plan, waits for the exact parent process to
//! exit, stages only the six published application files, and rolls back on
//! replacement or relaunch failure. No Steam or account data enters a release.

use anyhow::{bail, ensure, Context, Result};
use reqwest::{blocking::Client, redirect::Policy, Url};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::{Child, Command},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub const DEFAULT_TOOL_RELEASE_SOURCE: &str = "https://github.com/lunaswd/SteamTools-Next";
pub const TOOL_UPDATE_HELPER_ARGUMENT: &str = "--apply-update-helper";
const MANIFEST_NAME: &str = "tool-release.json";
const PLAN_NAME: &str = "plan.json";
const HELPER_NAME: &str = "SteamTools-Next-update-helper.exe";
const READY_NAME: &str = "helper-ready.txt";
const LAST_RESULT_NAME: &str = "last-update-result.json";
const MAX_METADATA: u64 = 512 * 1024;
const MAX_ARCHIVE: u64 = 128 * 1024 * 1024;
const MAX_UNPACKED: u64 = 256 * 1024 * 1024;
const MAX_PLAN_AGE_SECS: u64 = 3600;
const APP_EXE: &str = "SteamTools Next.exe";
const FILE_NAMES: &[&str] = &[
    APP_EXE,
    "LICENSE.txt",
    "\u{4f7f}\u{7528}\u{8bf4}\u{660e}.txt",
    "THIRD-PARTY-NOTICES.txt",
    "licenses.txt",
    "SHA256SUMS.txt",
];

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ToolUpdateInfo {
    pub current_version: String,
    pub latest_version: String,
    pub release_tag: String,
    pub source_commit: String,
    pub update_available: bool,
    pub size_bytes: u64,
    pub download_url: String,
    pub release_url: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreparedToolUpdate {
    pub version: String,
    pub release_tag: String,
    pub plan_path: PathBuf,
    pub helper_path: PathBuf,
    pub destination_path: PathBuf,
    pub parent_pid: u32,
    #[serde(skip)]
    plan_id: String,
    #[serde(skip)]
    helper_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ReleaseManifest {
    schema_version: u32,
    version: String,
    release_tag: String,
    source_commit: String,
    platform: String,
    asset: ReleaseAsset,
    files: Vec<ReleaseFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ReleaseAsset {
    name: String,
    size_bytes: u64,
    sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ReleaseFile {
    name: String,
    size_bytes: u64,
    sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ParentIdentity {
    pid: u32,
    creation_time: u64,
    executable: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct UpdatePlan {
    schema_version: u32,
    plan_id: String,
    created_at: u64,
    parent: ParentIdentity,
    previous_executable_sha256: String,
    helper_sha256: String,
    stage_root: PathBuf,
    target_executable: PathBuf,
    manifest_sha256: String,
    release: ReleaseManifest,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ToolUpdateResult {
    pub success: bool,
    pub version: String,
    pub restarted_process_id: Option<u32>,
    pub rolled_back: bool,
    pub error: Option<String>,
}

/// Numeric release comparison supports the kernel's four-part version too.
/// No guessed ordering for prereleases, suffixes or malformed version strings.
pub fn is_newer_release_version(latest: &str, current: &str) -> Result<bool> {
    let mut latest = numeric_version(latest)?;
    let mut current = numeric_version(current)?;
    latest.resize(4, 0);
    current.resize(4, 0);
    Ok(latest > current)
}

fn numeric_version(version: &str) -> Result<Vec<u16>> {
    let parts: Vec<_> = version.split('.').collect();
    ensure!(
        (2..=4).contains(&parts.len()),
        "Release version has an invalid component count"
    );
    parts
        .iter()
        .map(|part| {
            ensure!(
                !part.is_empty() && part.len() <= 5 && part.bytes().all(|b| b.is_ascii_digit()),
                "Release version is not numeric"
            );
            part.parse::<u16>()
                .context("Release version component is out of range")
        })
        .collect()
}

/// Query a small manifest only. This never modifies the app, Steam or settings.
pub fn check_tool_update(current_version: &str) -> Result<ToolUpdateInfo> {
    let manifest = fetch_manifest(&client()?)?;
    update_info(&manifest, current_version)
}

fn update_info(manifest: &ReleaseManifest, current_version: &str) -> Result<ToolUpdateInfo> {
    validate_manifest(manifest)?;
    Ok(ToolUpdateInfo {
        current_version: current_version.into(),
        latest_version: manifest.version.clone(),
        release_tag: manifest.release_tag.clone(),
        source_commit: manifest.source_commit.clone(),
        update_available: is_newer_release_version(&manifest.version, current_version)?,
        size_bytes: manifest.asset.size_bytes,
        download_url: asset_url(manifest),
        release_url: format!(
            "{DEFAULT_TOOL_RELEASE_SOURCE}/releases/tag/{}",
            manifest.release_tag
        ),
    })
}

/// Prepare a verified package and helper. The caller must still explicitly
/// launch the helper and wait for its acknowledgement before exiting the app.
pub fn prepare_tool_update(
    cache_root: &Path,
    current_executable: &Path,
    parent_pid: u32,
    current_version: &str,
) -> Result<PreparedToolUpdate> {
    #[cfg(not(windows))]
    {
        let _ = (cache_root, current_executable, parent_pid, current_version);
        bail!("Automatic desktop replacement is currently supported on Windows only");
    }
    #[cfg(windows)]
    {
        let expected_cache = owned_cache_root()?;
        let cache = local_path(cache_root, "tool update cache")?;
        ensure!(
            same_path(&cache, &expected_cache),
            "Tool update cache must be the application's dedicated update directory"
        );
        let executable = regular_file(current_executable)?;
        ensure!(
            same_path(&executable, &std::env::current_exe()?),
            "Update target must be the current executable"
        );
        ensure!(
            parent_pid == std::process::id(),
            "Update parent must be this application process"
        );
        validate_target_name(&executable)?;
        validate_executable(&executable)?;
        let parent =
            process_identity(parent_pid)?.context("Current application process is unavailable")?;
        ensure!(
            same_path(&parent.executable, &executable),
            "Update parent executable identity mismatch"
        );
        let transport = client()?;
        let manifest = fetch_manifest(&transport)?;
        ensure!(
            is_newer_release_version(&manifest.version, current_version)?,
            "No newer tool release is available"
        );
        fs::create_dir_all(&cache)?;
        reject_links(&cache)?;
        let id = unique_id()?;
        let stage = cache.join(format!("update-{id}"));
        fs::create_dir(&stage)?;
        let outcome = prepare_into_stage(&transport, &stage, &executable, parent, manifest, id);
        // Preserve failed preparation for diagnosis; no application files have
        // been touched. Later application-owned cleanup may remove old stages.
        outcome
    }
}

fn prepare_into_stage(
    transport: &Client,
    stage: &Path,
    executable: &Path,
    parent: ParentIdentity,
    manifest: ReleaseManifest,
    id: String,
) -> Result<PreparedToolUpdate> {
    let package = stage.join("package");
    fs::create_dir(&package)?;
    let archive = stage.join("download.zip");
    download_archive(transport, &asset_url(&manifest), &archive, &manifest.asset)?;
    extract_archive(&archive, &package, &manifest)?;
    validate_package(&package, &manifest)?;
    validate_packaged_version(&package, &manifest)?;
    let helper = stage.join(HELPER_NAME);
    let previous_hash = identity(executable)?.1;
    copy_verified(executable, &helper, &previous_hash)?;
    let plan = UpdatePlan {
        schema_version: 1,
        plan_id: id.clone(),
        created_at: now_secs()?,
        parent,
        previous_executable_sha256: previous_hash.clone(),
        helper_sha256: previous_hash.clone(),
        stage_root: stage.to_path_buf(),
        target_executable: executable.to_path_buf(),
        manifest_sha256: hash(&serde_json::to_vec(&manifest)?),
        release: manifest,
    };
    let plan_path = stage.join(PLAN_NAME);
    write_new(&plan_path, &serde_json::to_vec_pretty(&plan)?)?;
    Ok(PreparedToolUpdate {
        version: plan.release.version,
        release_tag: plan.release.release_tag,
        plan_path,
        helper_path: helper,
        destination_path: executable.to_path_buf(),
        parent_pid: plan.parent.pid,
        plan_id: id,
        helper_sha256: previous_hash,
    })
}

/// A successful spawn alone is insufficient. The existing app stays alive if
/// the helper exits or never acknowledges the exact prepared plan.
pub fn launch_tool_update_helper(prepared: &PreparedToolUpdate) -> Result<u32> {
    let plan = load_owned_plan(&prepared.plan_path)?;
    ensure!(
        plan.plan_id == prepared.plan_id && plan.parent.pid == prepared.parent_pid,
        "Prepared update identity changed"
    );
    ensure!(
        plan.helper_sha256 == prepared.helper_sha256,
        "Prepared helper identity changed"
    );
    let helper = regular_file(&prepared.helper_path)?;
    ensure!(
        same_path(&helper, &plan.stage_root.join(HELPER_NAME)),
        "Prepared helper escaped its update stage"
    );
    ensure!(
        identity(&helper)?.1 == prepared.helper_sha256,
        "Update helper failed SHA-256 verification"
    );
    let ready = plan.stage_root.join(READY_NAME);
    ensure!(
        !ready.exists(),
        "Prepared update has already been acknowledged; prepare a fresh update"
    );
    let mut command = Command::new(&helper);
    command
        .arg(TOOL_UPDATE_HELPER_ARGUMENT)
        .arg(&prepared.plan_path)
        .current_dir(&plan.stage_root);
    hide_child(&mut command);
    let mut child = command
        .spawn()
        .context("Could not start the isolated update helper; current app remains open")?;
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        if ready.exists() {
            let bytes = bounded_file(&ready, 256)?;
            ensure!(
                bytes == plan.plan_id.as_bytes(),
                "Update helper acknowledged another plan"
            );
            return Ok(child.id());
        }
        if let Some(status) = child.try_wait()? {
            bail!(
                "Update helper exited before acknowledgement ({status}); current app remains open"
            );
        }
        if Instant::now() >= deadline {
            // This is the child that we just spawned from our verified copy,
            // never the user app or Steam. Keep the original app running.
            let _ = child.kill();
            let _ = child.wait();
            bail!("Update helper did not acknowledge; current app remains open");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Dispatch before creating a Tauri window. This only accepts a recently
/// prepared plan in the current user's dedicated application update directory.
pub fn run_tool_update_helper(plan_path: &Path) -> Result<ToolUpdateResult> {
    let plan = load_owned_plan(plan_path)?;
    let helper = regular_file(&std::env::current_exe()?)?;
    ensure!(
        same_path(&helper, &plan.stage_root.join(HELPER_NAME)),
        "This executable is not the prepared update helper"
    );
    ensure!(
        identity(&helper)?.1 == plan.helper_sha256,
        "Running helper identity does not match the prepared application"
    );
    let actual = process_identity(plan.parent.pid)?
        .context("Update parent exited before helper acknowledgement")?;
    ensure!(
        actual == plan.parent,
        "Update parent PID or creation-time identity changed"
    );
    ensure!(
        identity(&plan.target_executable)?.1 == plan.previous_executable_sha256,
        "Application changed before helper acknowledgement"
    );
    validate_package(&plan.stage_root.join("package"), &plan.release)?;
    validate_packaged_version(&plan.stage_root.join("package"), &plan.release)?;
    write_new(&plan.stage_root.join(READY_NAME), plan.plan_id.as_bytes())?;
    let mut runtime = NativeRuntime;
    let result = execute_plan(&plan, &mut runtime, Duration::from_secs(60));
    if let Ok(report) = &result {
        let _ = write_new(
            &plan.stage_root.join("update-result.json"),
            &serde_json::to_vec_pretty(report)?,
        );
        let _ = write_last_result(&plan.stage_root, report);
    } else if let Err(error) = &result {
        let report = ToolUpdateResult {
            success: false,
            version: plan.release.version.clone(),
            restarted_process_id: None,
            rolled_back: false,
            error: Some(error.to_string()),
        };
        let _ = write_new(
            &plan.stage_root.join("update-result.json"),
            &serde_json::to_vec_pretty(&report)?,
        );
        let _ = write_last_result(&plan.stage_root, &report);
    }
    result
}

/// Fixed application-owned status, never a directory scan or caller-supplied
/// path. Technical paths stay in the isolated stage and are not returned here.
pub fn read_last_tool_update_result() -> Result<Option<ToolUpdateResult>> {
    read_result_at(&owned_cache_root()?, false)
}

/// Consume a one-time restart notification while retaining the stage's full
/// diagnostic result and backups. The writer and consumer share an OS lock.
pub fn consume_last_tool_update_result() -> Result<Option<ToolUpdateResult>> {
    read_result_at(&owned_cache_root()?, true)
}

fn public_result(report: &ToolUpdateResult) -> ToolUpdateResult {
    let mut public = report.clone();
    public.error = if report.success {
        None
    } else if report.rolled_back {
        Some("Tool update failed; the previous version was restored and restarted.".into())
    } else {
        Some("Tool update did not complete; review its status before retrying.".into())
    };
    public
}

fn write_last_result(stage: &Path, report: &ToolUpdateResult) -> Result<()> {
    let cache = stage.parent().context("Tool update stage has no cache")?;
    ensure!(
        same_path(cache, &owned_cache_root()?),
        "Tool result is outside the application cache"
    );
    write_result_at(cache, report)
}

fn write_result_at(cache: &Path, report: &ToolUpdateResult) -> Result<()> {
    reject_links(cache)?;
    ensure!(
        fs::symlink_metadata(cache)?.is_dir(),
        "Tool result cache is not a directory"
    );
    let _lock = UpdateLock::acquire(&cache.join(".last-result.lock"))?;
    let path = cache.join(LAST_RESULT_NAME);
    if path.exists() {
        regular_file(&path)?;
    }
    let temporary = cache.join(format!(".last-result-{}.tmp", unique_id()?));
    write_new(
        &temporary,
        &serde_json::to_vec_pretty(&public_result(report))?,
    )?;
    let result = replace_file(&temporary, &path);
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn read_result_at(cache: &Path, consume: bool) -> Result<Option<ToolUpdateResult>> {
    reject_links(cache)?;
    if !cache.exists() {
        return Ok(None);
    }
    let _lock = UpdateLock::acquire(&cache.join(".last-result.lock"))?;
    let path = cache.join(LAST_RESULT_NAME);
    if !path.exists() {
        return Ok(None);
    }
    let report: ToolUpdateResult = serde_json::from_slice(&bounded_file(&path, MAX_METADATA)?)
        .context("Last tool update result is invalid")?;
    ensure!(
        numeric_version(&report.version)?.len() == 3,
        "Last update version is invalid"
    );
    let report = public_result(&report);
    if consume {
        fs::remove_file(&path)?;
    }
    Ok(Some(report))
}

fn load_owned_plan(path: &Path) -> Result<UpdatePlan> {
    let path = regular_file(path)?;
    ensure!(
        path.file_name().and_then(|name| name.to_str()) == Some(PLAN_NAME),
        "Update plan has an invalid file name"
    );
    let cache = owned_cache_root()?;
    let stage = path.parent().context("Update plan has no stage")?;
    ensure!(
        same_path(stage.parent().context("Update stage has no cache")?, &cache),
        "Update plan is outside the application-owned cache"
    );
    let name = stage
        .file_name()
        .and_then(|name| name.to_str())
        .context("Invalid update stage name")?;
    ensure!(
        name.starts_with("update-") && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'),
        "Invalid update stage name"
    );
    let plan: UpdatePlan = serde_json::from_slice(&bounded_file(&path, MAX_METADATA)?)
        .context("Update plan JSON is invalid")?;
    validate_plan(&plan, stage)?;
    Ok(plan)
}

fn validate_plan(plan: &UpdatePlan, stage: &Path) -> Result<()> {
    ensure!(
        plan.schema_version == 1 && plan.parent.pid > 0,
        "Invalid update plan schema or parent"
    );
    ensure!(
        !plan.plan_id.is_empty()
            && plan.plan_id.len() <= 128
            && plan
                .plan_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-'),
        "Invalid update plan ID"
    );
    let now = now_secs()?;
    ensure!(
        plan.created_at <= now && now - plan.created_at <= MAX_PLAN_AGE_SECS,
        "Update plan is stale or from the future"
    );
    ensure!(
        same_path(&plan.stage_root, stage),
        "Update stage identity changed"
    );
    ensure!(
        same_path(&plan.parent.executable, &plan.target_executable),
        "Update target is not the original application"
    );
    let target = regular_file(&plan.target_executable)?;
    validate_target_name(&target)?;
    validate_executable(&target)?;
    ensure!(
        valid_hash(&plan.previous_executable_sha256)
            && valid_hash(&plan.helper_sha256)
            && plan.previous_executable_sha256 == plan.helper_sha256,
        "Invalid original application hash"
    );
    validate_manifest(&plan.release)?;
    ensure!(
        hash(&serde_json::to_vec(&plan.release)?) == plan.manifest_sha256,
        "Prepared release manifest changed"
    );
    reject_links(stage)?;
    reject_links(&stage.join("package"))?;
    Ok(())
}

trait UpdateRuntime {
    fn process(&mut self, pid: u32) -> Result<Option<ParentIdentity>>;
    fn sleep(&mut self, duration: Duration);
    fn launch(&mut self, executable: &Path) -> Result<u32>;
}

struct NativeRuntime;
impl UpdateRuntime for NativeRuntime {
    fn process(&mut self, pid: u32) -> Result<Option<ParentIdentity>> {
        process_identity(pid)
    }
    fn sleep(&mut self, duration: Duration) {
        std::thread::sleep(duration);
    }
    fn launch(&mut self, executable: &Path) -> Result<u32> {
        let target = regular_file(executable)?;
        validate_executable(&target)?;
        let mut command = Command::new(&target);
        command.current_dir(
            target
                .parent()
                .context("Application has no parent directory")?,
        );
        // Relaunch the GUI normally; only the helper gets CREATE_NO_WINDOW.
        let mut child = command
            .spawn()
            .context("Could not restart the updated application")?;
        check_immediate_exit(&mut child)?;
        Ok(child.id())
    }
}

fn check_immediate_exit(child: &mut Child) -> Result<()> {
    let deadline = Instant::now() + Duration::from_millis(800);
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait()? {
            // A successful exit code is not a running desktop application.
            // Any immediate exit triggers restoration instead of a false
            // "restarted" result with no window or live process.
            bail!("Updated application exited immediately with {status}");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

fn execute_plan(
    plan: &UpdatePlan,
    runtime: &mut dyn UpdateRuntime,
    wait_limit: Duration,
) -> Result<ToolUpdateResult> {
    let mut waited = Duration::ZERO;
    let interval = Duration::from_millis(200);
    while let Some(actual) = runtime.process(plan.parent.pid)? {
        ensure!(
            actual == plan.parent,
            "Update parent PID was reused by another process"
        );
        ensure!(
            waited < wait_limit,
            "Original application did not exit; no update files were changed"
        );
        runtime.sleep(interval);
        waited += interval;
    }
    let target_parent = plan
        .target_executable
        .parent()
        .context("Application target has no parent")?;
    reject_links(target_parent)?;
    let _guard = UpdateLock::acquire(&target_parent.join(".steamtools-next-update.lock"))?;
    ensure!(
        identity(&plan.target_executable)?.1 == plan.previous_executable_sha256,
        "Original application changed after preparation"
    );
    if let Err(error) = validate_package(&plan.stage_root.join("package"), &plan.release) {
        let restarted = runtime.launch(&plan.target_executable)
            .context("Original files were preserved, but the previous application needs to be started manually")?;
        return Ok(ToolUpdateResult {
            success: false,
            version: plan.release.version.clone(),
            restarted_process_id: Some(restarted),
            rolled_back: false,
            error: Some(error.to_string()),
        });
    }
    let mut transaction = match Replacement::prepare(plan) {
        Ok(transaction) => transaction,
        Err(error) => {
            let restarted = runtime.launch(&plan.target_executable)
                .context("Original files were preserved, but the previous application needs to be started manually")?;
            return Ok(ToolUpdateResult {
                success: false,
                version: plan.release.version.clone(),
                restarted_process_id: Some(restarted),
                rolled_back: false,
                error: Some(error.to_string()),
            });
        }
    };
    let outcome = transaction
        .commit()
        .and_then(|_| runtime.launch(&plan.target_executable));
    match outcome {
        Ok(pid) => Ok(ToolUpdateResult {
            success: true,
            version: plan.release.version.clone(),
            restarted_process_id: Some(pid),
            rolled_back: false,
            error: None,
        }),
        Err(error) => {
            transaction
                .rollback()
                .context("Update failed and restoration requires attention; backups preserved")?;
            let restarted = runtime.launch(&plan.target_executable)
                .context("Original files restored, but the previous application needs to be started manually")?;
            Ok(ToolUpdateResult {
                success: false,
                version: plan.release.version.clone(),
                restarted_process_id: Some(restarted),
                rolled_back: true,
                error: Some(error.to_string()),
            })
        }
    }
}

struct ReplacementEntry {
    target: PathBuf,
    staged: PathBuf,
    backup: Option<PathBuf>,
    previous_hash: Option<String>,
    expected_new_hash: String,
    committed: bool,
}
struct Replacement {
    entries: Vec<ReplacementEntry>,
}
impl Replacement {
    fn prepare(plan: &UpdatePlan) -> Result<Self> {
        let parent = plan
            .target_executable
            .parent()
            .context("Application target has no parent")?;
        let package = plan.stage_root.join("package");
        let backups = plan.stage_root.join("backup");
        fs::create_dir(&backups)?;
        let mut transaction = Self {
            entries: Vec::new(),
        };
        // Replace the executable last; companion failures restore old content.
        let mut files = plan.release.files.clone();
        files.sort_by_key(|file| file.name == APP_EXE);
        for (index, file) in files.iter().enumerate() {
            let target = if file.name == APP_EXE {
                plan.target_executable.clone()
            } else {
                parent.join(&file.name)
            };
            reject_links(&target)?;
            let previous = match fs::symlink_metadata(&target) {
                Ok(metadata) => {
                    ensure!(
                        metadata.is_file(),
                        "Existing application companion is not a regular file"
                    );
                    Some(identity(&target)?.1)
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => return Err(error.into()),
            };
            let backup = if let Some(hash) = &previous {
                let path = backups.join(&file.name);
                copy_verified(&target, &path, hash)?;
                Some(path)
            } else {
                None
            };
            let staged = parent.join(format!(".stn-update-{}-{index}.tmp", plan.plan_id));
            copy_verified(&package.join(&file.name), &staged, &file.sha256)?;
            transaction.entries.push(ReplacementEntry {
                target,
                staged,
                backup,
                previous_hash: previous,
                expected_new_hash: file.sha256.clone(),
                committed: false,
            });
        }
        Ok(transaction)
    }
    fn commit(&mut self) -> Result<()> {
        for entry in &mut self.entries {
            reject_links(&entry.target)?;
            ensure!(
                identity(&entry.staged)?.1 == entry.expected_new_hash,
                "Staged update changed before replacement"
            );
            match &entry.previous_hash {
                Some(expected) => ensure!(
                    identity(&entry.target)?.1 == *expected,
                    "Application file changed during replacement"
                ),
                None => ensure!(
                    !entry.target.exists(),
                    "Application companion appeared during replacement"
                ),
            }
            replace_file(&entry.staged, &entry.target)?;
            entry.committed = true;
            ensure!(
                identity(&entry.target)?.1 == entry.expected_new_hash,
                "Replaced file does not match the verified release"
            );
        }
        Ok(())
    }
    fn rollback(&mut self) -> Result<()> {
        for entry in self
            .entries
            .iter_mut()
            .rev()
            .filter(|entry| entry.committed)
        {
            reject_links(&entry.target)?;
            ensure!(
                identity(&entry.target)?.1 == entry.expected_new_hash,
                "Application file changed externally during rollback; modified file and backups preserved"
            );
            if let Some(backup) = &entry.backup {
                let rollback = entry.staged.with_extension("rollback");
                copy_verified(
                    backup,
                    &rollback,
                    entry
                        .previous_hash
                        .as_deref()
                        .context("Backup has no hash")?,
                )?;
                replace_file(&rollback, &entry.target)?;
            } else if entry.target.exists() {
                // This exact known application file was created by us.
                ensure!(
                    fs::symlink_metadata(&entry.target)?.is_file(),
                    "Rollback target is no longer a regular file"
                );
                fs::remove_file(&entry.target)?;
            }
            entry.committed = false;
        }
        Ok(())
    }
}
impl Drop for Replacement {
    fn drop(&mut self) {
        for entry in &self.entries {
            if entry.staged.exists() {
                let _ = fs::remove_file(&entry.staged);
            }
        }
    }
}

fn client() -> Result<Client> {
    Client::builder()
        .user_agent(concat!("SteamToolsNext/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(120))
        .redirect(Policy::custom(|attempt| {
            if attempt.previous().len() >= 8 {
                return attempt.error("Tool download exceeded redirect limit");
            }
            if trusted_url(attempt.url()) {
                attempt.follow()
            } else {
                attempt.error("Tool download redirected outside GitHub release hosting")
            }
        }))
        .build()
        .context("Could not initialize tool update downloader")
}
fn trusted_url(url: &Url) -> bool {
    url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && url.port().is_none()
        && matches!(
            url.host_str(),
            Some(
                "github.com"
                    | "release-assets.githubusercontent.com"
                    | "objects.githubusercontent.com"
                    | "github-releases.githubusercontent.com"
            )
        )
}
fn asset_url(manifest: &ReleaseManifest) -> String {
    format!(
        "{DEFAULT_TOOL_RELEASE_SOURCE}/releases/download/{}/{}",
        manifest.release_tag, manifest.asset.name
    )
}
fn fetch_manifest(transport: &Client) -> Result<ReleaseManifest> {
    let url = format!("{DEFAULT_TOOL_RELEASE_SOURCE}/releases/latest/download/{MANIFEST_NAME}");
    let mut response = transport
        .get(url)
        .send()
        .context("Could not check tool updates")?
        .error_for_status()
        .context("Tool release manifest is unavailable")?;
    ensure!(
        response
            .content_length()
            .is_none_or(|size| size <= MAX_METADATA),
        "Tool update manifest is oversized"
    );
    let mut bytes = Vec::new();
    response
        .by_ref()
        .take(MAX_METADATA + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_METADATA,
        "Tool update manifest is oversized"
    );
    let manifest =
        serde_json::from_slice(&bytes).context("Tool update manifest JSON is invalid")?;
    validate_manifest(&manifest)?;
    Ok(manifest)
}
fn validate_manifest(manifest: &ReleaseManifest) -> Result<()> {
    ensure!(
        manifest.schema_version == 1 && manifest.platform == "windows-x64",
        "Tool release requires schema 1 and windows-x64"
    );
    ensure!(
        numeric_version(&manifest.version)?.len() == 3
            && manifest.release_tag == format!("v{}", manifest.version),
        "Tool release version or tag is invalid"
    );
    ensure!(
        manifest.source_commit.len() == 40
            && manifest
                .source_commit
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
        "Tool source commit is invalid"
    );
    ensure!(
        manifest.asset.name == format!("steamtools-next-{}-windows-x64.zip", manifest.version)
            && valid_hash(&manifest.asset.sha256)
            && manifest.asset.size_bytes > 0
            && manifest.asset.size_bytes <= MAX_ARCHIVE,
        "Tool ZIP identity is invalid"
    );
    ensure!(
        manifest.files.len() == FILE_NAMES.len(),
        "Tool release must contain exactly the six application files"
    );
    let mut seen = BTreeSet::new();
    let mut total = 0u64;
    for file in &manifest.files {
        ensure!(
            FILE_NAMES.contains(&file.name.as_str())
                && seen.insert(file.name.to_ascii_lowercase())
                && valid_hash(&file.sha256)
                && file.size_bytes > 0,
            "Tool manifest contains an unexpected, duplicate or invalid file"
        );
        total = total
            .checked_add(file.size_bytes)
            .context("Tool release size overflow")?;
        ensure!(
            total <= MAX_UNPACKED,
            "Tool release unpacked content is oversized"
        );
    }
    Ok(())
}
fn download_archive(
    transport: &Client,
    url: &str,
    path: &Path,
    asset: &ReleaseAsset,
) -> Result<()> {
    let mut response = transport
        .get(url)
        .send()
        .context("Could not download tool release")?
        .error_for_status()
        .context("Tool release ZIP is unavailable")?;
    ensure!(
        response
            .content_length()
            .is_none_or(|size| size == asset.size_bytes),
        "Tool ZIP size does not match manifest"
    );
    let mut file = OpenOptions::new().create_new(true).write(true).open(path)?;
    let mut hasher = Sha256::new();
    let mut count = 0u64;
    let mut buffer = [0; 64 * 1024];
    loop {
        let size = response.read(&mut buffer)?;
        if size == 0 {
            break;
        }
        count += size as u64;
        ensure!(
            count <= asset.size_bytes && count <= MAX_ARCHIVE,
            "Tool ZIP exceeds declared size"
        );
        hasher.update(&buffer[..size]);
        file.write_all(&buffer[..size])?;
    }
    file.sync_all()?;
    ensure!(
        count == asset.size_bytes && format!("{:x}", hasher.finalize()) == asset.sha256,
        "Tool ZIP failed size or SHA-256 verification"
    );
    Ok(())
}
fn extract_archive(archive_path: &Path, package: &Path, manifest: &ReleaseManifest) -> Result<()> {
    let mut archive =
        zip::ZipArchive::new(File::open(archive_path)?).context("Tool ZIP is invalid")?;
    ensure!(
        archive.len() == FILE_NAMES.len(),
        "Tool ZIP must contain six flat files"
    );
    let mut seen = BTreeSet::new();
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        let name = entry.name().to_owned();
        let kind = entry.unix_mode().map(|mode| mode & 0o170000);
        ensure!(
            FILE_NAMES.contains(&name.as_str())
                && !entry.is_dir()
                && kind.is_none_or(|kind| kind == 0 || kind == 0o100000)
                && seen.insert(name.to_ascii_lowercase()),
            "Tool ZIP contains paths, links, duplicates or unexpected files"
        );
        let expected = manifest
            .files
            .iter()
            .find(|file| file.name == name)
            .context("Tool ZIP file is absent from manifest")?;
        ensure!(
            entry.size() == expected.size_bytes,
            "Tool ZIP entry size mismatch"
        );
        let mut output = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(package.join(name))?;
        let mut hasher = Sha256::new();
        let mut count = 0u64;
        let mut buffer = [0; 64 * 1024];
        loop {
            let size = entry.read(&mut buffer)?;
            if size == 0 {
                break;
            }
            count += size as u64;
            ensure!(count <= expected.size_bytes, "Tool ZIP entry exceeded size");
            hasher.update(&buffer[..size]);
            output.write_all(&buffer[..size])?;
        }
        output.sync_all()?;
        ensure!(
            count == expected.size_bytes && format!("{:x}", hasher.finalize()) == expected.sha256,
            "Tool ZIP entry failed SHA-256 verification"
        );
    }
    Ok(())
}
fn validate_package(package: &Path, manifest: &ReleaseManifest) -> Result<()> {
    reject_links(package)?;
    ensure!(
        fs::read_dir(package)?.count() == FILE_NAMES.len(),
        "Tool package has missing or extra files"
    );
    for file in &manifest.files {
        let path = regular_file(&package.join(&file.name))?;
        let (size, sha) = identity(&path)?;
        ensure!(
            size == file.size_bytes && sha == file.sha256,
            "Prepared tool package changed after verification"
        );
    }
    validate_executable(&package.join(APP_EXE))?;
    Ok(())
}

fn validate_packaged_version(package: &Path, manifest: &ReleaseManifest) -> Result<()> {
    #[cfg(windows)]
    {
        let version = crate::steam::file_version(&package.join(APP_EXE))
            .context("Tool release executable has no Windows version resource")?;
        let mut actual = numeric_version(&version)?;
        let mut expected = numeric_version(&manifest.version)?;
        actual.resize(4, 0);
        expected.resize(4, 0);
        ensure!(
            actual == expected,
            "Tool executable version does not match its release manifest"
        );
    }
    #[cfg(not(windows))]
    let _ = (package, manifest);
    Ok(())
}
fn validate_executable(path: &Path) -> Result<()> {
    let path = regular_file(path)?;
    let mut file = File::open(path)?;
    let length = file.metadata()?.len();
    ensure!(length >= 256, "Tool executable is truncated");
    let mut dos = [0u8; 64];
    file.read_exact(&mut dos)?;
    ensure!(&dos[..2] == b"MZ", "Tool executable is not a PE image");
    let offset = u32::from_le_bytes(dos[60..64].try_into()?) as u64;
    ensure!(offset <= length - 24, "Tool PE header is truncated");
    file.seek(SeekFrom::Start(offset))?;
    let mut header = [0; 24];
    file.read_exact(&mut header)?;
    let sections = u16::from_le_bytes([header[6], header[7]]) as u64;
    let optional = u16::from_le_bytes([header[20], header[21]]) as u64;
    let flags = u16::from_le_bytes([header[22], header[23]]);
    ensure!(
        &header[..4] == b"PE\0\0"
            && u16::from_le_bytes([header[4], header[5]]) == 0x8664
            && flags & 0x2000 == 0
            && flags & 0x0002 != 0
            && (1..=96).contains(&sections)
            && optional >= 112
            && offset + 24 + optional + sections * 40 <= length,
        "Tool release requires a complete x64 executable, not a DLL"
    );
    let mut magic = [0; 2];
    file.read_exact(&mut magic)?;
    ensure!(
        u16::from_le_bytes(magic) == 0x20b,
        "Tool executable is not PE32+"
    );
    Ok(())
}
fn validate_target_name(path: &Path) -> Result<()> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("Application executable has an invalid name")?;
    ensure!(
        name.eq_ignore_ascii_case(APP_EXE) || name.eq_ignore_ascii_case("steam-tools-next.exe"),
        "Only the SteamTools Next application may be replaced"
    );
    Ok(())
}
fn owned_cache_root() -> Result<PathBuf> {
    local_path(
        &crate::settings::app_data_dir()?
            .join("cache")
            .join("tool-updates-v1"),
        "tool update cache",
    )
}
fn local_path(path: &Path, label: &str) -> Result<PathBuf> {
    crate::kernel_manage::absolute_local_path(path, label)
}
fn reject_links(path: &Path) -> Result<()> {
    crate::kernel_manage::reject_link_ancestors(path)
}
fn regular_file(path: &Path) -> Result<PathBuf> {
    let path = local_path(path, "update file")?;
    reject_links(&path)?;
    ensure!(
        fs::symlink_metadata(&path)?.is_file(),
        "Update file is not a regular file"
    );
    Ok(path)
}
fn bounded_file(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let path = regular_file(path)?;
    ensure!(
        fs::metadata(&path)?.len() <= limit,
        "Update metadata is oversized"
    );
    let mut bytes = Vec::new();
    File::open(path)?.take(limit + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= limit,
        "Update metadata changed while reading"
    );
    Ok(bytes)
}
fn identity(path: &Path) -> Result<(u64, String)> {
    let path = regular_file(path)?;
    let size = fs::metadata(&path)?.len();
    ensure!(size <= MAX_UNPACKED, "Update file is oversized");
    let mut file = File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    let mut count = 0;
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        count += read as u64;
        ensure!(count <= size, "Update file changed while hashing");
        digest.update(&buffer[..read]);
    }
    ensure!(count == size, "Update file size changed while hashing");
    Ok((size, format!("{:x}", digest.finalize())))
}
fn copy_verified(from: &Path, to: &Path, expected: &str) -> Result<()> {
    let from = regular_file(from)?;
    reject_links(to)?;
    ensure!(!to.exists(), "Update staging file already exists");
    let size = fs::metadata(&from)?.len();
    ensure!(size <= MAX_UNPACKED, "Update source file is oversized");
    let result = (|| -> Result<()> {
        let mut input = File::open(from)?;
        let mut output = OpenOptions::new().create_new(true).write(true).open(to)?;
        let mut digest = Sha256::new();
        let mut count = 0u64;
        let mut bytes = [0u8; 64 * 1024];
        loop {
            let read = input.read(&mut bytes)?;
            if read == 0 {
                break;
            }
            count += read as u64;
            ensure!(count <= size, "Update source grew during copy");
            output.write_all(&bytes[..read])?;
            digest.update(&bytes[..read]);
        }
        output.sync_all()?;
        ensure!(
            count == size && format!("{:x}", digest.finalize()) == expected,
            "Update copy failed size or SHA-256 verification"
        );
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(to);
    }
    result
}
fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    reject_links(path)?;
    let mut file = OpenOptions::new().create_new(true).write(true).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}
fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn valid_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}
fn same_path(left: &Path, right: &Path) -> bool {
    let identity = |path: &Path| {
        fs::canonicalize(path)
            .unwrap_or_else(|_| path.to_path_buf())
            .to_string_lossy()
            .trim_start_matches(r"\\?\")
            .replace('/', "\\")
            .trim_end_matches('\\')
            .to_ascii_lowercase()
    };
    identity(left) == identity(right)
}
fn now_secs() -> Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}
fn unique_id() -> Result<String> {
    Ok(format!(
        "{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    ))
}

struct UpdateLock(File);
impl UpdateLock {
    fn acquire(path: &Path) -> Result<Self> {
        reject_links(path)?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;
        file.try_lock()
            .context("Another tool update is in progress")?;
        Ok(Self(file))
    }
}
impl Drop for UpdateLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

#[cfg(windows)]
fn hide_child(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    command.creation_flags(0x0800_0000);
}
#[cfg(not(windows))]
fn hide_child(_command: &mut Command) {}

#[cfg(windows)]
fn replace_file(from: &Path, to: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };
    reject_links(from)?;
    reject_links(to)?;
    let from: Vec<_> = from.as_os_str().encode_wide().chain(Some(0)).collect();
    let to: Vec<_> = to.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: both UTF-16 buffers are terminated and valid for this synchronous call.
    ensure!(
        unsafe {
            MoveFileExW(
                from.as_ptr(),
                to.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        } != 0,
        "Could not replace application file: {}",
        std::io::Error::last_os_error()
    );
    Ok(())
}
#[cfg(not(windows))]
fn replace_file(from: &Path, to: &Path) -> Result<()> {
    reject_links(from)?;
    reject_links(to)?;
    fs::rename(from, to)?;
    Ok(())
}

#[cfg(windows)]
fn process_identity(pid: u32) -> Result<Option<ParentIdentity>> {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, FILETIME},
        System::Threading::{
            GetProcessTimes, OpenProcess, QueryFullProcessImageNameW,
            PROCESS_QUERY_LIMITED_INFORMATION,
        },
    };
    // SAFETY: handle is closed on every path; output buffers are correctly sized.
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(87) {
                return Ok(None);
            }
            return Err(error).context("Could not identify the update parent process");
        }
        let result = (|| -> Result<Option<ParentIdentity>> {
            let mut creation = FILETIME::default();
            let mut exit = FILETIME::default();
            let mut kernel = FILETIME::default();
            let mut user = FILETIME::default();
            ensure!(
                GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) != 0,
                "Could not read update parent creation time"
            );
            if exit.dwHighDateTime != 0 || exit.dwLowDateTime != 0 {
                return Ok(None);
            }
            let mut buffer = vec![0u16; 32768];
            let mut length = buffer.len() as u32;
            ensure!(
                QueryFullProcessImageNameW(handle, 0, buffer.as_mut_ptr(), &mut length) != 0,
                "Could not identify update parent image"
            );
            Ok(Some(ParentIdentity {
                pid,
                creation_time: ((creation.dwHighDateTime as u64) << 32)
                    | creation.dwLowDateTime as u64,
                executable: local_path(
                    &PathBuf::from(String::from_utf16_lossy(&buffer[..length as usize])),
                    "update parent image",
                )?,
            }))
        })();
        CloseHandle(handle);
        result
    }
}
#[cfg(not(windows))]
fn process_identity(_pid: u32) -> Result<Option<ParentIdentity>> {
    bail!("Windows process identity is required for automatic updates")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::VecDeque, io::Cursor};
    use zip::{write::SimpleFileOptions, ZipWriter};
    type ReleaseFixture = (ReleaseManifest, Vec<(String, Vec<u8>)>, Vec<u8>);

    fn pe(marker: u8) -> Vec<u8> {
        let mut bytes = vec![marker; 512];
        bytes[..64].fill(0);
        bytes[..2].copy_from_slice(b"MZ");
        bytes[60..64].copy_from_slice(&64u32.to_le_bytes());
        bytes[64..88].fill(0);
        bytes[64..68].copy_from_slice(b"PE\0\0");
        bytes[68..70].copy_from_slice(&0x8664u16.to_le_bytes());
        bytes[70..72].copy_from_slice(&1u16.to_le_bytes());
        bytes[84..86].copy_from_slice(&112u16.to_le_bytes());
        bytes[86..88].copy_from_slice(&2u16.to_le_bytes());
        bytes[88..90].copy_from_slice(&0x20bu16.to_le_bytes());
        bytes
    }

    fn zip_bytes(files: &[(String, Vec<u8>)]) -> Vec<u8> {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        for (name, bytes) in files {
            writer
                .start_file(name, SimpleFileOptions::default())
                .unwrap();
            writer.write_all(bytes).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    fn release_fixture() -> ReleaseFixture {
        let files: Vec<_> = FILE_NAMES
            .iter()
            .map(|name| {
                (
                    name.to_string(),
                    if *name == APP_EXE {
                        pe(2)
                    } else {
                        format!("new published companion: {name}").into_bytes()
                    },
                )
            })
            .collect();
        let zip = zip_bytes(&files);
        let manifest = ReleaseManifest {
            schema_version: 1,
            version: "0.3.1".into(),
            release_tag: "v0.3.1".into(),
            source_commit: "a".repeat(40),
            platform: "windows-x64".into(),
            asset: ReleaseAsset {
                name: "steamtools-next-0.3.1-windows-x64.zip".into(),
                size_bytes: zip.len() as u64,
                sha256: hash(&zip),
            },
            files: files
                .iter()
                .map(|(name, bytes)| ReleaseFile {
                    name: name.clone(),
                    size_bytes: bytes.len() as u64,
                    sha256: hash(bytes),
                })
                .collect(),
        };
        (manifest, files, zip)
    }

    struct Fixture {
        _temporary: tempfile::TempDir,
        plan: UpdatePlan,
        old_files: Vec<(PathBuf, Vec<u8>)>,
    }
    fn deployment_fixture() -> Fixture {
        let temporary = tempfile::tempdir().unwrap();
        let destination = temporary.path().join("app");
        let stage = temporary.path().join("update-fixture");
        let package = stage.join("package");
        fs::create_dir(&destination).unwrap();
        fs::create_dir(&stage).unwrap();
        fs::create_dir(&package).unwrap();
        let (release, files, _) = release_fixture();
        for (name, bytes) in &files {
            fs::write(package.join(name), bytes).unwrap();
        }
        let mut old_files = Vec::new();
        for name in FILE_NAMES {
            let bytes = if *name == APP_EXE {
                pe(1)
            } else {
                format!("old companion: {name}").into_bytes()
            };
            let path = destination.join(name);
            fs::write(&path, &bytes).unwrap();
            old_files.push((path, bytes));
        }
        let target = destination.join(APP_EXE);
        let parent = ParentIdentity {
            pid: 100,
            creation_time: 42,
            executable: target.clone(),
        };
        let old_hash = hash(&pe(1));
        let plan = UpdatePlan {
            schema_version: 1,
            plan_id: "fixture-1".into(),
            created_at: now_secs().unwrap(),
            parent,
            previous_executable_sha256: old_hash.clone(),
            helper_sha256: old_hash,
            stage_root: stage,
            target_executable: target,
            manifest_sha256: hash(&serde_json::to_vec(&release).unwrap()),
            release,
        };
        Fixture {
            _temporary: temporary,
            plan,
            old_files,
        }
    }

    #[derive(Default)]
    struct MockRuntime {
        processes: VecDeque<Option<ParentIdentity>>,
        failed_launches: usize,
        launches: Vec<PathBuf>,
        sleeps: usize,
    }
    impl UpdateRuntime for MockRuntime {
        fn process(&mut self, _pid: u32) -> Result<Option<ParentIdentity>> {
            Ok(self.processes.pop_front().flatten())
        }
        fn sleep(&mut self, _duration: Duration) {
            self.sleeps += 1;
        }
        fn launch(&mut self, path: &Path) -> Result<u32> {
            self.launches.push(path.to_path_buf());
            if self.failed_launches > 0 {
                self.failed_launches -= 1;
                bail!("fixture relaunch failure");
            }
            Ok(9000)
        }
    }
    fn assert_old_files(fixture: &Fixture) {
        for (path, bytes) in &fixture.old_files {
            assert_eq!(fs::read(path).unwrap(), *bytes);
        }
    }

    #[test]
    fn numeric_release_order_is_not_lexicographic_or_a_tag_guess() {
        assert!(is_newer_release_version("0.3.10", "0.3.9").unwrap());
        assert!(!is_newer_release_version("0.3.1", "0.3.1.0").unwrap());
        assert!(!is_newer_release_version("0.2.90", "0.3.0").unwrap());
        assert!(is_newer_release_version("1.5.4.2", "1.5.4.1").unwrap());
        for bad in [
            "v0.3.1",
            "0.3.1-beta",
            "0.3.1+build.1",
            "0.3.65536",
            "0..1",
            "0.3.-1",
        ] {
            assert!(is_newer_release_version(bad, "0.3.0").is_err(), "{bad}");
        }
    }

    #[test]
    fn manifest_is_exact_flat_gui_package_with_fixed_github_routes() {
        let (good, _, _) = release_fixture();
        validate_manifest(&good).unwrap();
        assert_eq!(asset_url(&good), format!("{DEFAULT_TOOL_RELEASE_SOURCE}/releases/download/v0.3.1/steamtools-next-0.3.1-windows-x64.zip"));
        let info = update_info(&good, "0.3.0").unwrap();
        assert!(info.update_available);
        assert_eq!(info.latest_version, "0.3.1");
        let mut bad = good.clone();
        bad.release_tag = "latest".into();
        assert!(validate_manifest(&bad).is_err());
        let mut bad = good.clone();
        bad.platform = "windows-x86".into();
        assert!(validate_manifest(&bad).is_err());
        let mut bad = good.clone();
        bad.asset.name = "anything.exe".into();
        assert!(validate_manifest(&bad).is_err());
        let mut bad = good.clone();
        bad.files[1].name = "loginusers.vdf".into();
        assert!(validate_manifest(&bad).is_err());
        let mut bad = good.clone();
        bad.files[1].name = APP_EXE.into();
        assert!(validate_manifest(&bad).is_err());
        let mut bad = good.clone();
        bad.files[1].sha256 = "x".repeat(64);
        assert!(validate_manifest(&bad).is_err());
        let mut bad = good;
        bad.files[1].size_bytes = MAX_UNPACKED + 1;
        assert!(validate_manifest(&bad).is_err());
    }

    #[test]
    fn zip_and_verified_package_detect_extra_paths_tampering_and_wrong_pe() {
        let temporary = tempfile::tempdir().unwrap();
        let (manifest, files, bytes) = release_fixture();
        let archive = temporary.path().join("package.zip");
        let package = temporary.path().join("package");
        fs::write(&archive, &bytes).unwrap();
        fs::create_dir(&package).unwrap();
        extract_archive(&archive, &package, &manifest).unwrap();
        validate_package(&package, &manifest).unwrap();
        fs::write(package.join("personal-data.txt"), b"extra").unwrap();
        assert!(validate_package(&package, &manifest).is_err());
        fs::remove_file(package.join("personal-data.txt")).unwrap();
        fs::write(package.join(APP_EXE), pe(3)).unwrap();
        assert!(validate_package(&package, &manifest).is_err());
        let mut malicious = files;
        malicious[0].0 = "../SteamTools Next.exe".into();
        fs::write(&archive, zip_bytes(&malicious)).unwrap();
        let another = temporary.path().join("another");
        fs::create_dir(&another).unwrap();
        assert!(extract_archive(&archive, &another, &manifest).is_err());
        assert!(!temporary.path().join(APP_EXE).exists());
        let fake = temporary.path().join("fake.exe");
        let mut bytes = pe(1);
        bytes[86..88].copy_from_slice(&0x2002u16.to_le_bytes());
        fs::write(&fake, bytes).unwrap();
        assert!(validate_executable(&fake).is_err());
        let mut bytes = pe(1);
        bytes[68..70].copy_from_slice(&0x14cu16.to_le_bytes());
        fs::write(&fake, bytes).unwrap();
        assert!(validate_executable(&fake).is_err());
    }

    #[test]
    fn update_waits_for_exact_parent_and_restarts_new_executable_without_touching_unrelated_data() {
        let fixture = deployment_fixture();
        let unrelated = fixture
            .plan
            .target_executable
            .parent()
            .unwrap()
            .join("unrelated-fixture.txt");
        fs::write(&unrelated, b"preserve").unwrap();
        let mut runtime = MockRuntime {
            processes: VecDeque::from([Some(fixture.plan.parent.clone()), None]),
            ..Default::default()
        };
        let result = execute_plan(&fixture.plan, &mut runtime, Duration::from_secs(1)).unwrap();
        assert!(result.success && !result.rolled_back);
        assert_eq!(runtime.sleeps, 1);
        assert_eq!(
            runtime.launches,
            vec![fixture.plan.target_executable.clone()]
        );
        for file in &fixture.plan.release.files {
            assert_eq!(
                identity(
                    &fixture
                        .plan
                        .target_executable
                        .parent()
                        .unwrap()
                        .join(&file.name)
                )
                .unwrap()
                .1,
                file.sha256
            );
        }
        assert_eq!(fs::read(unrelated).unwrap(), b"preserve");
    }

    #[test]
    fn parent_pid_reuse_or_timeout_never_replaces_application_files() {
        let fixture = deployment_fixture();
        let mut other = fixture.plan.parent.clone();
        other.creation_time += 1;
        let mut runtime = MockRuntime {
            processes: VecDeque::from([Some(other)]),
            ..Default::default()
        };
        assert!(execute_plan(&fixture.plan, &mut runtime, Duration::from_secs(1)).is_err());
        assert_old_files(&fixture);
        assert!(runtime.launches.is_empty());
        let mut runtime = MockRuntime {
            processes: VecDeque::from([Some(fixture.plan.parent.clone())]),
            ..Default::default()
        };
        assert!(execute_plan(&fixture.plan, &mut runtime, Duration::ZERO).is_err());
        assert_old_files(&fixture);
        assert!(runtime.launches.is_empty());
    }

    #[test]
    fn relaunch_failure_restores_every_original_file_and_restarts_old_app() {
        let fixture = deployment_fixture();
        let mut runtime = MockRuntime {
            failed_launches: 1,
            ..Default::default()
        };
        let result = execute_plan(&fixture.plan, &mut runtime, Duration::ZERO).unwrap();
        assert!(!result.success && result.rolled_back);
        assert_eq!(runtime.launches.len(), 2);
        assert_old_files(&fixture);
    }

    #[test]
    fn successful_immediate_exit_child() {
        // Invoked only by the parent regression below. Normal full-suite runs
        // simply return; the isolated child terminates with success before a
        // desktop window could stay alive.
        if std::env::var_os("STN_UPDATE_EXIT_ZERO_FIXTURE").as_deref()
            == Some(std::ffi::OsStr::new("1"))
        {
            std::process::exit(0);
        }
    }

    fn spawn_exit_zero_fixture() -> Child {
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tool_update::tests::successful_immediate_exit_child",
                "--nocapture",
            ])
            .env("STN_UPDATE_EXIT_ZERO_FIXTURE", "1")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap()
    }

    #[test]
    fn exit_zero_before_window_stays_alive_is_a_launch_failure() {
        let mut child = spawn_exit_zero_fixture();
        // Ensure this test checks a real exit-0 child, not a spawn failure or
        // a nonzero exit code caused by another assertion.
        assert!(child.wait().unwrap().success());
        assert!(check_immediate_exit(&mut child)
            .unwrap_err()
            .to_string()
            .contains("exited immediately"));
    }

    #[test]
    fn immediate_success_exit_rolls_back_and_restarts_the_previous_app() {
        struct ImmediateExitRuntime {
            launches: usize,
        }
        impl UpdateRuntime for ImmediateExitRuntime {
            fn process(&mut self, _pid: u32) -> Result<Option<ParentIdentity>> {
                Ok(None)
            }
            fn sleep(&mut self, _duration: Duration) {}
            fn launch(&mut self, _executable: &Path) -> Result<u32> {
                self.launches += 1;
                if self.launches == 1 {
                    let mut child = spawn_exit_zero_fixture();
                    assert!(child.wait().unwrap().success());
                    check_immediate_exit(&mut child)?;
                }
                Ok(9000)
            }
        }
        let fixture = deployment_fixture();
        let mut runtime = ImmediateExitRuntime { launches: 0 };
        let result = execute_plan(&fixture.plan, &mut runtime, Duration::ZERO).unwrap();
        assert!(!result.success && result.rolled_back);
        assert_eq!(runtime.launches, 2);
        assert_old_files(&fixture);
    }

    #[test]
    fn external_executable_rewrite_is_preserved_and_never_relaunched() {
        struct ExternalRewriteRuntime {
            launches: usize,
        }
        impl UpdateRuntime for ExternalRewriteRuntime {
            fn process(&mut self, _pid: u32) -> Result<Option<ParentIdentity>> {
                Ok(None)
            }
            fn sleep(&mut self, _duration: Duration) {}
            fn launch(&mut self, executable: &Path) -> Result<u32> {
                self.launches += 1;
                // Model an external replacement during the failed first
                // launch. No fixture executable is actually run.
                fs::write(executable, pe(9))?;
                bail!("fixture external executable replacement");
            }
        }
        let fixture = deployment_fixture();
        let mut runtime = ExternalRewriteRuntime { launches: 0 };
        let error = execute_plan(&fixture.plan, &mut runtime, Duration::ZERO).unwrap_err();
        assert!(format!("{error:#}").contains("changed externally during rollback"));
        assert_eq!(runtime.launches, 1);
        assert_eq!(fs::read(&fixture.plan.target_executable).unwrap(), pe(9));
        assert_eq!(
            fs::read(fixture.plan.stage_root.join("backup").join(APP_EXE)).unwrap(),
            pe(1)
        );
    }

    #[test]
    fn rollback_does_not_delete_a_created_companion_rewritten_externally() {
        let fixture = deployment_fixture();
        let target = fixture
            .plan
            .target_executable
            .parent()
            .unwrap()
            .join("licenses.txt");
        fs::remove_file(&target).unwrap();
        let mut replacement = Replacement::prepare(&fixture.plan).unwrap();
        replacement.commit().unwrap();
        fs::write(&target, b"external replacement must be preserved").unwrap();
        assert!(replacement
            .rollback()
            .unwrap_err()
            .to_string()
            .contains("changed externally during rollback"));
        assert_eq!(
            fs::read(&target).unwrap(),
            b"external replacement must be preserved"
        );
        assert_eq!(
            fs::read(fixture.plan.stage_root.join("backup").join(APP_EXE)).unwrap(),
            pe(1)
        );
    }

    #[test]
    fn failed_new_companions_are_removed_on_rollback_and_existing_unrelated_files_remain() {
        let fixture = deployment_fixture();
        let new_companion = fixture
            .plan
            .target_executable
            .parent()
            .unwrap()
            .join("licenses.txt");
        fs::remove_file(&new_companion).unwrap();
        let unrelated = fixture
            .plan
            .target_executable
            .parent()
            .unwrap()
            .join("keep.dat");
        fs::write(&unrelated, b"keep").unwrap();
        let mut runtime = MockRuntime {
            failed_launches: 1,
            ..Default::default()
        };
        let result = execute_plan(&fixture.plan, &mut runtime, Duration::ZERO).unwrap();
        assert!(result.rolled_back);
        assert!(!new_companion.exists());
        assert_eq!(fs::read(&unrelated).unwrap(), b"keep");
        assert_eq!(fs::read(&fixture.plan.target_executable).unwrap(), pe(1));
    }

    #[test]
    fn changed_source_or_existing_target_aborts_before_adopting_modified_hashes() {
        let fixture = deployment_fixture();
        fs::write(fixture.plan.stage_root.join("package").join(APP_EXE), pe(9)).unwrap();
        let mut runtime = MockRuntime::default();
        let result = execute_plan(&fixture.plan, &mut runtime, Duration::ZERO).unwrap();
        assert!(!result.success);
        assert_old_files(&fixture);
        assert_eq!(runtime.launches.len(), 1);
        let fixture = deployment_fixture();
        fs::write(&fixture.plan.target_executable, pe(8)).unwrap();
        let mut runtime = MockRuntime::default();
        assert!(execute_plan(&fixture.plan, &mut runtime, Duration::ZERO).is_err());
        assert!(runtime.launches.is_empty());
        assert_eq!(fs::read(&fixture.plan.target_executable).unwrap(), pe(8));
    }

    #[test]
    fn staging_failure_preserves_old_executable_and_restarts_it() {
        let fixture = deployment_fixture();
        let conflict = fixture
            .plan
            .target_executable
            .parent()
            .unwrap()
            .join("LICENSE.txt");
        fs::remove_file(&conflict).unwrap();
        fs::create_dir(&conflict).unwrap();
        let mut runtime = MockRuntime::default();
        let result = execute_plan(&fixture.plan, &mut runtime, Duration::ZERO).unwrap();
        assert!(!result.success && !result.rolled_back);
        assert_eq!(runtime.launches.len(), 1);
        assert!(conflict.is_dir());
        assert_eq!(fs::read(&fixture.plan.target_executable).unwrap(), pe(1));
    }

    #[test]
    fn staged_file_tampering_is_caught_before_atomic_replace() {
        let fixture = deployment_fixture();
        let mut replacement = Replacement::prepare(&fixture.plan).unwrap();
        let first = &replacement.entries[0];
        fs::write(&first.staged, b"tampered").unwrap();
        assert!(replacement.commit().is_err());
        replacement.rollback().unwrap();
        assert_old_files(&fixture);
    }

    #[test]
    fn plans_reject_stale_dates_foreign_targets_and_changed_manifest() {
        let fixture = deployment_fixture();
        validate_plan(&fixture.plan, &fixture.plan.stage_root).unwrap();
        let mut bad = fixture.plan.clone();
        bad.created_at = 0;
        assert!(validate_plan(&bad, &bad.stage_root).is_err());
        let mut bad = fixture.plan.clone();
        bad.parent.executable = bad.stage_root.join("other.exe");
        assert!(validate_plan(&bad, &bad.stage_root).is_err());
        let mut bad = fixture.plan.clone();
        bad.manifest_sha256 = "0".repeat(64);
        assert!(validate_plan(&bad, &bad.stage_root).is_err());
        assert!(load_owned_plan(&fixture.plan.stage_root.join(PLAN_NAME)).is_err());
    }

    #[test]
    fn failed_copy_cleans_the_exact_temporary_file_and_redirects_are_https_github_only() {
        let fixture = deployment_fixture();
        let to = fixture.plan.stage_root.join("temp.exe");
        assert!(copy_verified(&fixture.plan.target_executable, &to, &"0".repeat(64)).is_err());
        assert!(!to.exists());
        for bad in [
            "http://github.com/a",
            "https://evil.invalid/a",
            "https://github.com:444/a",
            "https://user:pass@github.com/a",
        ] {
            assert!(!trusted_url(&Url::parse(bad).unwrap()));
        }
        assert!(trusted_url(
            &Url::parse("https://release-assets.githubusercontent.com/asset?signature=fixture")
                .unwrap()
        ));
    }

    #[test]
    fn archive_http_payload_must_match_published_size_and_hash() {
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/release.zip", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; 2048];
            let _ = stream.read(&mut request);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\ndata")
                .unwrap();
        });
        let temporary = tempfile::tempdir().unwrap();
        let asset = ReleaseAsset {
            name: "fixture.zip".into(),
            size_bytes: 4,
            sha256: "0".repeat(64),
        };
        let error = download_archive(
            &Client::builder()
                .timeout(Duration::from_secs(2))
                .build()
                .unwrap(),
            &url,
            &temporary.path().join("download"),
            &asset,
        )
        .unwrap_err();
        server.join().unwrap();
        assert!(error.to_string().contains("SHA-256"));
    }

    #[test]
    fn restart_status_is_fixed_path_bounded_consumable_and_has_no_local_paths() {
        let temporary = tempfile::tempdir().unwrap();
        let report = ToolUpdateResult {
            success: false,
            version: "0.3.1".into(),
            restarted_process_id: Some(9000),
            rolled_back: true,
            error: Some("private technical fixture path C:\\private\\data".into()),
        };
        write_result_at(temporary.path(), &report).unwrap();
        let bytes = fs::read(temporary.path().join(LAST_RESULT_NAME)).unwrap();
        assert!(!String::from_utf8(bytes).unwrap().contains("private"));
        let first = read_result_at(temporary.path(), false).unwrap().unwrap();
        assert!(first.rolled_back && first.error.unwrap().contains("restored"));
        assert!(read_result_at(temporary.path(), true).unwrap().is_some());
        assert!(read_result_at(temporary.path(), true).unwrap().is_none());
        let mut success = report;
        success.success = true;
        success.rolled_back = false;
        write_result_at(temporary.path(), &success).unwrap();
        assert!(read_result_at(temporary.path(), false)
            .unwrap()
            .unwrap()
            .error
            .is_none());
    }

    #[cfg(windows)]
    #[test]
    fn tool_without_embedded_windows_version_is_not_publishable() {
        let fixture = deployment_fixture();
        assert!(validate_packaged_version(
            &fixture.plan.stage_root.join("package"),
            &fixture.plan.release
        )
        .is_err());
    }
}
