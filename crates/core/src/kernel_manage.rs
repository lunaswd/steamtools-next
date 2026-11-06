//! Transactional OpenSteamTool deployment and lifecycle management.
//!
//! The OpenSteamTool DLL has no stable management ABI.  SteamTools Next manages
//! it as an on-disk installation: all writes are planned first, Steam must be
//! stopped, existing files are backed up, replacements are staged beside their
//! destination and every failed transaction is rolled back.

use crate::kernel::{
    detect_kernel_installation, detect_kernel_installation_with_artifact, KernelInstallation,
};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, File, Metadata, OpenOptions},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

const MAX_ARTIFACT_BYTES: u64 = 512 * 1024 * 1024;
const LOCK_NAME: &str = ".steamtools-next-kernel.lock";
const RECEIPT_NAME: &str = "receipt.json";
const MANAGED_FILES: &[&str] = &[
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum InjectionMethod {
    Dwmapi,
    Version,
}

impl InjectionMethod {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Dwmapi => "dwmapi",
            Self::Version => "version",
        }
    }

    fn dll_name(self) -> &'static str {
        match self {
            Self::Dwmapi => "dwmapi.dll",
            Self::Version => "version.dll",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "dwmapi" => Ok(Self::Dwmapi),
            "version" => Ok(Self::Version),
            _ => bail!("Kernel injection method must be dwmapi or version"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum KernelOperation {
    Install,
    SwitchInjection,
    Enable,
    Disable,
    Uninstall,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum KernelFileActionKind {
    Keep,
    Create,
    Replace,
    Remove,
    Move,
    Restore,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KernelInstallRequest {
    pub steam_root: PathBuf,
    pub artifact_root: PathBuf,
    pub injection_method: InjectionMethod,
    #[serde(default)]
    pub install_example_config: bool,
    #[serde(default)]
    pub backup_root: Option<PathBuf>,
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KernelSwitchRequest {
    pub steam_root: PathBuf,
    pub injection_method: InjectionMethod,
    #[serde(default)]
    pub backup_root: Option<PathBuf>,
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KernelToggleRequest {
    pub steam_root: PathBuf,
    pub enabled: bool,
    #[serde(default)]
    pub backup_root: Option<PathBuf>,
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KernelUninstallRequest {
    pub steam_root: PathBuf,
    #[serde(default)]
    pub install_receipt: Option<PathBuf>,
    #[serde(default)]
    pub backup_root: Option<PathBuf>,
    /// Required when uninstalling an installation not created by this module.
    #[serde(default)]
    pub force: bool,
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KernelFileAction {
    pub kind: KernelFileActionKind,
    pub source: Option<String>,
    pub target: String,
    pub backup: Option<String>,
    pub size_bytes: Option<u64>,
    pub sha256: Option<String>,
    /// Identity of an existing target/source observed during preflight.
    pub previous_sha256: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KernelOperationPlan {
    pub operation_id: String,
    pub operation: KernelOperation,
    pub steam_root: String,
    pub backup_dir: String,
    pub previous_state: KernelInstallation,
    pub expected_status: String,
    pub expected_injection_method: String,
    pub steam_running: bool,
    pub already_satisfied: bool,
    pub blockers: Vec<String>,
    pub actions: Vec<KernelFileAction>,
    #[serde(default)]
    pub artifact_root: Option<String>,
    #[serde(default)]
    pub artifact_version: Option<String>,
    #[serde(default)]
    pub artifact_sha256: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ManagedKernelFileAction {
    Created,
    Replaced,
    Removed,
    Moved,
    Restored,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagedKernelFile {
    /// Path relative to the Steam root.
    pub target: String,
    pub action: ManagedKernelFileAction,
    pub installed_sha256: Option<String>,
    pub previous_sha256: Option<String>,
    pub previous_backup: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KernelOperationReceipt {
    pub schema_version: u32,
    pub operation_id: String,
    pub operation: KernelOperation,
    pub dry_run: bool,
    pub changed: bool,
    pub steam_root: String,
    pub backup_dir: String,
    pub receipt_path: Option<String>,
    pub previous_state: KernelInstallation,
    pub resulting_state: KernelInstallation,
    pub records: Vec<ManagedKernelFile>,
    pub plan: KernelOperationPlan,
}

#[derive(Debug, Clone)]
struct Artifact {
    source: PathBuf,
    target: String,
    size_bytes: u64,
    sha256: String,
}

#[derive(Debug)]
struct StagedFile {
    target: PathBuf,
    temporary: PathBuf,
}

#[derive(Debug)]
struct OperationGuard {
    path: PathBuf,
}

impl OperationGuard {
    fn acquire(steam_root: &Path, operation_id: &str) -> Result<Self> {
        let path = steam_root.join(LOCK_NAME);
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)
            .context("Another kernel file operation is already in progress")?;
        writeln!(file, "{operation_id}")?;
        file.sync_all()?;
        Ok(Self { path })
    }
}

impl Drop for OperationGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Produce a deterministic preflight plan without changing the Steam directory.
/// Fast, fail-closed target preflight before remote downloads. The installer
/// repeats these checks under its transaction lock immediately before writes.
pub fn ensure_kernel_target_stopped(path: &Path) -> Result<()> {
    let root = validate_steam_root(path)?;
    if steam_process_running(&root.join("steam.exe"))? {
        bail!("steam_running");
    }
    Ok(())
}

pub fn plan_kernel_install(request: &KernelInstallRequest) -> Result<KernelOperationPlan> {
    let steam_root = validate_steam_root(&request.steam_root)?;
    let artifact_root = validate_directory(&request.artifact_root, "kernel artifact")?;
    let operation_id = operation_id("install");
    let backup_dir =
        operation_backup_dir(&steam_root, request.backup_root.as_deref(), &operation_id)?;
    let previous_state = detect_kernel_installation(&steam_root)?;
    let running = steam_process_running(&steam_root.join("steam.exe"))?;
    let artifacts = install_artifacts(
        &artifact_root,
        request.injection_method,
        request.install_example_config,
    )?;
    let artifact_identity = artifact_identity(&artifact_root);

    let mut desired = BTreeMap::new();
    for artifact in artifacts {
        desired.insert(artifact.target.clone(), artifact);
    }

    let mut actions = Vec::new();
    for relative in MANAGED_FILES {
        if desired.contains_key(*relative) {
            continue;
        }
        let target = safe_target(&steam_root, relative)?;
        if regular_file(&target)?.is_some() {
            let (size, sha) = file_identity(&target)?;
            actions.push(KernelFileAction {
                kind: KernelFileActionKind::Remove,
                source: None,
                target: display_path(&target),
                backup: Some(display_path(&safe_target(&backup_dir, relative)?)),
                size_bytes: Some(size),
                sha256: Some(sha),
                previous_sha256: None,
            });
        }
    }

    for (_, artifact) in desired {
        let target = safe_target(&steam_root, &artifact.target)?;
        let existing = regular_file(&target)?;
        let (kind, backup, previous_sha256) = match existing {
            Some(_) => {
                let (_, current_hash) = file_identity(&target)?;
                if current_hash == artifact.sha256 {
                    (KernelFileActionKind::Keep, None, Some(current_hash))
                } else {
                    (
                        KernelFileActionKind::Replace,
                        Some(display_path(&safe_target(&backup_dir, &artifact.target)?)),
                        Some(current_hash),
                    )
                }
            }
            None => (KernelFileActionKind::Create, None, None),
        };
        actions.push(KernelFileAction {
            kind,
            source: Some(display_path(&artifact.source)),
            target: display_path(&target),
            backup,
            size_bytes: Some(artifact.size_bytes),
            sha256: Some(artifact.sha256),
            previous_sha256,
        });
    }

    if request.install_example_config {
        add_example_config_action(&artifact_root, &steam_root, &backup_dir, &mut actions)?;
    }
    actions.sort_by(|left, right| left.target.cmp(&right.target));
    let already_satisfied = actions
        .iter()
        .all(|action| action.kind == KernelFileActionKind::Keep);
    Ok(KernelOperationPlan {
        operation_id,
        operation: KernelOperation::Install,
        steam_root: display_path(&steam_root),
        backup_dir: display_path(&backup_dir),
        previous_state,
        expected_status: "enabled".into(),
        expected_injection_method: request.injection_method.as_str().into(),
        steam_running: running,
        already_satisfied,
        blockers: if running {
            vec!["steam_running".into()]
        } else {
            Vec::new()
        },
        actions,
        artifact_root: Some(display_path(&artifact_root)),
        artifact_version: artifact_identity.0,
        artifact_sha256: artifact_identity.1,
    })
}

/// Install or update the complete OpenSteamTool DLL set transactionally.
pub fn install_kernel(request: &KernelInstallRequest) -> Result<KernelOperationReceipt> {
    let plan = plan_kernel_install(request)?;
    if request.dry_run {
        return Ok(dry_run_receipt(plan));
    }
    execute_install_plan(plan)
}

/// Plan an injection-entry switch while preserving the enabled/disabled state.
pub fn plan_injection_switch(request: &KernelSwitchRequest) -> Result<KernelOperationPlan> {
    let steam_root = validate_steam_root(&request.steam_root)?;
    let previous_state = detect_kernel_installation(&steam_root)?;
    if !matches!(previous_state.status.as_str(), "enabled" | "disabled") {
        bail!("Injection switching requires a complete enabled or disabled kernel installation");
    }
    let current = InjectionMethod::parse(&previous_state.injection_method)?;
    let operation_id = operation_id("switch");
    let backup_dir =
        operation_backup_dir(&steam_root, request.backup_root.as_deref(), &operation_id)?;
    let running = steam_process_running(&steam_root.join("steam.exe"))?;
    let mut actions = Vec::new();
    if current != request.injection_method {
        let selected_suffix = if previous_state.status == "enabled" {
            ""
        } else {
            ".off"
        };
        let current_source = format!("{}{}", current.dll_name(), selected_suffix);
        let desired_source = format!("{}.disabled", request.injection_method.dll_name());
        let current_target = format!("{}.disabled", current.dll_name());
        let desired_target = format!("{}{}", request.injection_method.dll_name(), selected_suffix);
        for (source, target) in [
            (current_source.as_str(), current_target.as_str()),
            (desired_source.as_str(), desired_target.as_str()),
        ] {
            let source_path = safe_target(&steam_root, source)?;
            let target_path = safe_target(&steam_root, target)?;
            regular_file(&source_path)?
                .with_context(|| format!("Required kernel file is missing: {source}"))?;
            let (size, sha) = file_identity(&source_path)?;
            actions.push(KernelFileAction {
                kind: KernelFileActionKind::Move,
                source: Some(display_path(&source_path)),
                target: display_path(&target_path),
                backup: Some(display_path(&safe_target(&backup_dir, source)?)),
                size_bytes: Some(size),
                sha256: Some(sha),
                previous_sha256: None,
            });
        }
    }
    Ok(KernelOperationPlan {
        operation_id,
        operation: KernelOperation::SwitchInjection,
        steam_root: display_path(&steam_root),
        backup_dir: display_path(&backup_dir),
        previous_state: previous_state.clone(),
        expected_status: previous_state.status.clone(),
        expected_injection_method: request.injection_method.as_str().into(),
        steam_running: running,
        already_satisfied: actions.is_empty(),
        blockers: if running {
            vec!["steam_running".into()]
        } else {
            Vec::new()
        },
        actions,
        artifact_root: None,
        artifact_version: None,
        artifact_sha256: None,
    })
}

pub fn switch_injection_method(request: &KernelSwitchRequest) -> Result<KernelOperationReceipt> {
    let plan = plan_injection_switch(request)?;
    if request.dry_run {
        return Ok(dry_run_receipt(plan));
    }
    execute_move_plan(plan)
}

/// Plan enabling or disabling the installed body and selected injection entry.
pub fn plan_kernel_toggle(request: &KernelToggleRequest) -> Result<KernelOperationPlan> {
    let steam_root = validate_steam_root(&request.steam_root)?;
    let previous_state = detect_kernel_installation(&steam_root)?;
    let operation = if request.enabled {
        KernelOperation::Enable
    } else {
        KernelOperation::Disable
    };
    let operation_id = operation_id(if request.enabled { "enable" } else { "disable" });
    let backup_dir =
        operation_backup_dir(&steam_root, request.backup_root.as_deref(), &operation_id)?;
    let running = steam_process_running(&steam_root.join("steam.exe"))?;
    let expected_status = if request.enabled {
        "enabled"
    } else {
        "disabled"
    };
    let mut actions = Vec::new();
    if previous_state.status != expected_status {
        let valid_from = if request.enabled {
            "disabled"
        } else {
            "enabled"
        };
        if previous_state.status != valid_from {
            bail!("Kernel enable/disable requires a complete enabled or disabled installation");
        }
        let injection = InjectionMethod::parse(&previous_state.injection_method)?;
        let pairs = if request.enabled {
            vec![
                (
                    "OpenSteamTool.dll.off".to_owned(),
                    "OpenSteamTool.dll".to_owned(),
                ),
                ("xinput1_4.dll.off".to_owned(), "xinput1_4.dll".to_owned()),
                (
                    format!("{}.off", injection.dll_name()),
                    injection.dll_name().to_owned(),
                ),
            ]
        } else {
            vec![
                (
                    "OpenSteamTool.dll".to_owned(),
                    "OpenSteamTool.dll.off".to_owned(),
                ),
                ("xinput1_4.dll".to_owned(), "xinput1_4.dll.off".to_owned()),
                (
                    injection.dll_name().to_owned(),
                    format!("{}.off", injection.dll_name()),
                ),
            ]
        };
        for (source, target) in pairs {
            let source_path = safe_target(&steam_root, &source)?;
            let target_path = safe_target(&steam_root, &target)?;
            regular_file(&source_path)?
                .with_context(|| format!("Required kernel file is missing: {source}"))?;
            if regular_file(&target_path)?.is_some() {
                bail!(
                    "Kernel target already exists: {}",
                    display_path(&target_path)
                );
            }
            let (size, sha) = file_identity(&source_path)?;
            actions.push(KernelFileAction {
                kind: KernelFileActionKind::Move,
                source: Some(display_path(&source_path)),
                target: display_path(&target_path),
                backup: Some(display_path(&safe_target(&backup_dir, &source)?)),
                size_bytes: Some(size),
                sha256: Some(sha),
                previous_sha256: None,
            });
        }
    }
    Ok(KernelOperationPlan {
        operation_id,
        operation,
        steam_root: display_path(&steam_root),
        backup_dir: display_path(&backup_dir),
        previous_state: previous_state.clone(),
        expected_status: expected_status.into(),
        expected_injection_method: previous_state.injection_method.clone(),
        steam_running: running,
        already_satisfied: actions.is_empty(),
        blockers: if running {
            vec!["steam_running".into()]
        } else {
            Vec::new()
        },
        actions,
        artifact_root: None,
        artifact_version: None,
        artifact_sha256: None,
    })
}

pub fn set_kernel_enabled(request: &KernelToggleRequest) -> Result<KernelOperationReceipt> {
    let plan = plan_kernel_toggle(request)?;
    if request.dry_run {
        return Ok(dry_run_receipt(plan));
    }
    execute_move_plan(plan)
}

/// Plan an uninstall.  A prior install receipt is preferred; `force` is
/// required to remove an externally installed kernel without one.
pub fn plan_kernel_uninstall(request: &KernelUninstallRequest) -> Result<KernelOperationPlan> {
    let steam_root = validate_steam_root(&request.steam_root)?;
    let previous_state = detect_kernel_installation(&steam_root)?;
    let operation_id = operation_id("uninstall");
    let backup_dir =
        operation_backup_dir(&steam_root, request.backup_root.as_deref(), &operation_id)?;
    let running = steam_process_running(&steam_root.join("steam.exe"))?;
    let mut blockers = if running {
        vec!["steam_running".into()]
    } else {
        Vec::new()
    };
    let mut actions = Vec::new();
    let mut expected_status = "notInstalled".to_owned();
    let mut expected_injection_method = "none".to_owned();

    if let Some(path) = request.install_receipt.as_deref() {
        let receipt = read_receipt(path)?;
        if path_identity(Path::new(&receipt.steam_root)) != path_identity(&steam_root) {
            bail!("The install receipt belongs to a different Steam directory");
        }
        if receipt.operation != KernelOperation::Install || receipt.dry_run {
            bail!("A completed install receipt is required for receipt-based uninstall");
        }
        expected_status = receipt.previous_state.status.clone();
        expected_injection_method = receipt.previous_state.injection_method.clone();
        for record in &receipt.records {
            let target = safe_target(&steam_root, &record.target)?;
            match record.action {
                ManagedKernelFileAction::Created | ManagedKernelFileAction::Replaced => {
                    if regular_file(&target)?.is_some() {
                        let (size, hash) = file_identity(&target)?;
                        if record.installed_sha256.as_deref() != Some(hash.as_str())
                            && !request.force
                        {
                            blockers.push(format!("modified_file:{}", record.target));
                        }
                        actions.push(KernelFileAction {
                            kind: KernelFileActionKind::Remove,
                            source: None,
                            target: display_path(&target),
                            backup: Some(display_path(&safe_target(&backup_dir, &record.target)?)),
                            size_bytes: Some(size),
                            sha256: Some(hash),
                            previous_sha256: None,
                        });
                    }
                    if record.action == ManagedKernelFileAction::Replaced {
                        let backup = record
                            .previous_backup
                            .as_deref()
                            .context("Install receipt is missing a replacement backup")?;
                        let source = validate_regular_file(Path::new(backup), "install backup")?;
                        let (size, hash) = file_identity(&source)?;
                        if record.previous_sha256.as_deref() != Some(hash.as_str()) {
                            blockers.push(format!("backup_changed:{}", record.target));
                        }
                        actions.push(KernelFileAction {
                            kind: KernelFileActionKind::Restore,
                            source: Some(display_path(&source)),
                            target: display_path(&target),
                            backup: None,
                            size_bytes: Some(size),
                            sha256: Some(hash),
                            previous_sha256: None,
                        });
                    }
                }
                ManagedKernelFileAction::Removed => {
                    if regular_file(&target)?.is_some() && !request.force {
                        blockers.push(format!("target_reappeared:{}", record.target));
                    }
                    let backup = record
                        .previous_backup
                        .as_deref()
                        .context("Install receipt is missing a removal backup")?;
                    let source = validate_regular_file(Path::new(backup), "install backup")?;
                    let (size, hash) = file_identity(&source)?;
                    if record.previous_sha256.as_deref() != Some(hash.as_str()) {
                        blockers.push(format!("backup_changed:{}", record.target));
                    }
                    actions.push(KernelFileAction {
                        kind: KernelFileActionKind::Restore,
                        source: Some(display_path(&source)),
                        target: display_path(&target),
                        backup: None,
                        size_bytes: Some(size),
                        sha256: Some(hash),
                        previous_sha256: None,
                    });
                }
                ManagedKernelFileAction::Moved | ManagedKernelFileAction::Restored => {}
            }
        }
    } else {
        let present = existing_managed_files(&steam_root)?;
        if !present.is_empty() && !request.force {
            blockers.push("receipt_or_force_required".into());
        }
        for (relative, target, size, sha) in present {
            actions.push(KernelFileAction {
                kind: KernelFileActionKind::Remove,
                source: None,
                target: display_path(&target),
                backup: Some(display_path(&safe_target(&backup_dir, &relative)?)),
                size_bytes: Some(size),
                sha256: Some(sha),
                previous_sha256: None,
            });
        }
    }
    actions.sort_by(|left, right| {
        action_priority(left.kind)
            .cmp(&action_priority(right.kind))
            .then(left.target.cmp(&right.target))
    });
    Ok(KernelOperationPlan {
        operation_id,
        operation: KernelOperation::Uninstall,
        steam_root: display_path(&steam_root),
        backup_dir: display_path(&backup_dir),
        previous_state,
        expected_status,
        expected_injection_method,
        steam_running: running,
        already_satisfied: actions.is_empty(),
        blockers,
        actions,
        artifact_root: None,
        artifact_version: None,
        artifact_sha256: None,
    })
}

pub fn uninstall_kernel(request: &KernelUninstallRequest) -> Result<KernelOperationReceipt> {
    let plan = plan_kernel_uninstall(request)?;
    if request.dry_run {
        return Ok(dry_run_receipt(plan));
    }
    execute_uninstall_plan(plan)
}

fn install_artifacts(
    artifact_root: &Path,
    method: InjectionMethod,
    _install_example_config: bool,
) -> Result<Vec<Artifact>> {
    let body = artifact(
        artifact_root,
        &["OpenSteamTool.dll"],
        "OpenSteamTool.dll",
        true,
    )?;
    let xinput = artifact(artifact_root, &["xinput1_4.dll"], "xinput1_4.dll", true)?;
    let dwmapi_target = if method == InjectionMethod::Dwmapi {
        "dwmapi.dll"
    } else {
        "dwmapi.dll.disabled"
    };
    let version_target = if method == InjectionMethod::Version {
        "version.dll"
    } else {
        "version.dll.disabled"
    };
    let dwmapi = artifact(
        artifact_root,
        &["dwmapi.dll", "dwmapi.dll.disabled"],
        dwmapi_target,
        true,
    )?;
    let version = artifact(
        artifact_root,
        &["version.dll", "version.dll.disabled"],
        version_target,
        true,
    )?;
    Ok(vec![body, xinput, dwmapi, version])
}

fn artifact(root: &Path, names: &[&str], target: &str, pe_x64: bool) -> Result<Artifact> {
    let source = first_regular_named_file(root, names)?
        .with_context(|| format!("Kernel artifact is missing: {}", names.join(" or ")))?;
    validate_regular_file(&source, "kernel artifact")?;
    if pe_x64 {
        validate_pe_x64(&source)?;
    }
    let (size_bytes, sha256) = file_identity(&source)?;
    Ok(Artifact {
        source,
        target: target.into(),
        size_bytes,
        sha256,
    })
}

fn add_example_config_action(
    artifact_root: &Path,
    steam_root: &Path,
    _backup_dir: &Path,
    actions: &mut Vec<KernelFileAction>,
) -> Result<()> {
    let target = steam_root.join("opensteamtool.toml");
    if regular_file(&target)?.is_some() {
        actions.push(KernelFileAction {
            kind: KernelFileActionKind::Keep,
            source: None,
            target: display_path(&target),
            backup: None,
            size_bytes: None,
            sha256: None,
            previous_sha256: None,
        });
        return Ok(());
    }
    let source = first_regular_named_file(
        artifact_root,
        &["opensteamtool.toml", "opensteamtool.example.toml"],
    )?;
    let Some(source) = source else {
        return Ok(());
    };
    let (size, hash) = file_identity(&source)?;
    actions.push(KernelFileAction {
        kind: KernelFileActionKind::Create,
        source: Some(display_path(&source)),
        target: display_path(&target),
        backup: None,
        size_bytes: Some(size),
        sha256: Some(hash),
        previous_sha256: None,
    });
    Ok(())
}

fn first_regular_named_file(root: &Path, names: &[&str]) -> Result<Option<PathBuf>> {
    for name in names {
        let path = root.join(name);
        if regular_file(&path)?.is_some() {
            return Ok(Some(path));
        }
    }
    Ok(None)
}

pub(crate) fn execute_install_plan(plan: KernelOperationPlan) -> Result<KernelOperationReceipt> {
    ensure_executable_plan(&plan)?;
    let steam_root = PathBuf::from(&plan.steam_root);
    if steam_process_running(&steam_root.join("steam.exe"))? {
        bail!("Steam started after preflight; close Steam before installing the kernel");
    }
    let _guard = OperationGuard::acquire(&steam_root, &plan.operation_id)?;
    let backup_dir = PathBuf::from(&plan.backup_dir);
    let mut staged = Vec::new();
    let mut backups = Vec::new();
    let mut committed = Vec::new();
    let mut records = Vec::new();

    if plan.already_satisfied {
        let resulting_state = detect_kernel_installation_with_artifact(
            &steam_root,
            plan.artifact_root.as_deref().map(Path::new),
        )?;
        return Ok(receipt_without_file(
            &plan,
            false,
            resulting_state,
            Vec::new(),
        ));
    }
    fs::create_dir_all(&backup_dir)?;
    reject_link_ancestors(&backup_dir)?;

    let outcome: Result<()> = (|| {
        for (index, action) in plan.actions.iter().enumerate() {
            if !matches!(
                action.kind,
                KernelFileActionKind::Create | KernelFileActionKind::Replace
            ) {
                continue;
            }
            let source = action
                .source
                .as_deref()
                .context("Install action has no source")?;
            let source = validate_regular_file(Path::new(source), "kernel artifact")?;
            if action.target.to_ascii_lowercase().ends_with(".dll")
                || action.target.to_ascii_lowercase().contains(".dll.")
            {
                validate_pe_x64(&source)?;
            }
            let (_, current_hash) = file_identity(&source)?;
            if action.sha256.as_deref() != Some(current_hash.as_str()) {
                bail!("Kernel artifact changed after preflight: {source:?}");
            }
            let target = PathBuf::from(&action.target);
            let parent = target.parent().context("Kernel target has no parent")?;
            fs::create_dir_all(parent)?;
            reject_link_ancestors(parent)?;
            let temporary = parent.join(format!(".stn-stage-{}-{index}.tmp", plan.operation_id));
            copy_verified(&source, &temporary, action.sha256.as_deref())?;
            staged.push(StagedFile { target, temporary });
        }

        for action in &plan.actions {
            if !matches!(
                action.kind,
                KernelFileActionKind::Replace | KernelFileActionKind::Remove
            ) {
                continue;
            }
            let target = PathBuf::from(&action.target);
            let backup = action
                .backup
                .as_deref()
                .context("Destructive install action has no backup")?;
            let backup = PathBuf::from(backup);
            let (size, current_hash) = file_identity(&target)?;
            if action.kind == KernelFileActionKind::Replace
                && action.previous_sha256.as_deref() != Some(current_hash.as_str())
            {
                bail!("Kernel target changed after preflight: {}", action.target);
            }
            if action.kind == KernelFileActionKind::Remove
                && action.sha256.as_deref() != Some(current_hash.as_str())
            {
                bail!("Kernel target changed after preflight: {}", action.target);
            }
            fs::create_dir_all(backup.parent().context("Backup has no parent")?)?;
            copy_verified(&target, &backup, Some(&current_hash)).with_context(|| {
                format!("Could not back up kernel file {}", display_path(&target))
            })?;
            fs::remove_file(&target).with_context(|| {
                format!("Could not replace kernel file {}", display_path(&target))
            })?;
            backups.push((target.clone(), backup.clone()));
            if action.kind == KernelFileActionKind::Remove {
                records.push(ManagedKernelFile {
                    target: relative_string(&steam_root, &target)?,
                    action: ManagedKernelFileAction::Removed,
                    installed_sha256: None,
                    previous_sha256: Some(current_hash),
                    previous_backup: Some(display_path(&backup)),
                });
            } else {
                let _ = size;
            }
        }

        for item in &staged {
            fs::rename(&item.temporary, &item.target).with_context(|| {
                format!(
                    "Could not commit kernel file {}",
                    display_path(&item.target)
                )
            })?;
            committed.push(item.target.clone());
            let action = plan
                .actions
                .iter()
                .find(|action| {
                    path_identity(Path::new(&action.target)) == path_identity(&item.target)
                })
                .context("Committed target is absent from its plan")?;
            let (_, installed_hash) = file_identity(&item.target)?;
            let previous = backups
                .iter()
                .find(|(target, _)| path_identity(target) == path_identity(&item.target));
            let (managed_action, previous_sha256, previous_backup) =
                if let Some((_, backup)) = previous {
                    let (_, previous_hash) = file_identity(backup)?;
                    (
                        ManagedKernelFileAction::Replaced,
                        Some(previous_hash),
                        Some(display_path(backup)),
                    )
                } else {
                    (ManagedKernelFileAction::Created, None, None)
                };
            let relative = relative_string(&steam_root, &item.target)?;
            if relative.eq_ignore_ascii_case("opensteamtool.toml") {
                // Configuration is intentionally user-owned, even when seeded.
                continue;
            }
            records.push(ManagedKernelFile {
                target: relative,
                action: managed_action,
                installed_sha256: Some(installed_hash),
                previous_sha256,
                previous_backup,
            });
            let _ = action;
        }
        Ok(())
    })();

    if let Err(error) = outcome {
        rollback_files(&committed, &backups, &staged);
        return Err(error);
    }

    let resulting_state = match validate_result(&plan, &steam_root) {
        Ok(state) => state,
        Err(error) => {
            rollback_files(&committed, &backups, &staged);
            return Err(error);
        }
    };
    let mut receipt = receipt_without_file(&plan, true, resulting_state, records);
    let receipt_path = backup_dir.join(RECEIPT_NAME);
    receipt.receipt_path = Some(display_path(&receipt_path));
    if let Err(error) = write_receipt(&receipt_path, &receipt) {
        rollback_files(&committed, &backups, &staged);
        return Err(
            error.context("Could not persist the install receipt; installation rolled back")
        );
    }
    Ok(receipt)
}

fn execute_move_plan(plan: KernelOperationPlan) -> Result<KernelOperationReceipt> {
    ensure_executable_plan(&plan)?;
    let steam_root = PathBuf::from(&plan.steam_root);
    if steam_process_running(&steam_root.join("steam.exe"))? {
        bail!("Steam started after preflight; close Steam before changing the kernel");
    }
    let _guard = OperationGuard::acquire(&steam_root, &plan.operation_id)?;
    if plan.already_satisfied {
        let resulting_state = detect_kernel_installation(&steam_root)?;
        return Ok(receipt_without_file(
            &plan,
            false,
            resulting_state,
            Vec::new(),
        ));
    }
    let backup_dir = PathBuf::from(&plan.backup_dir);
    fs::create_dir_all(&backup_dir)?;
    let mut originals = Vec::new();
    let mut temporaries = Vec::new();
    let mut targets = Vec::new();
    let mut records = Vec::new();

    let outcome: Result<()> = (|| {
        for (index, action) in plan.actions.iter().enumerate() {
            if action.kind != KernelFileActionKind::Move {
                continue;
            }
            let source = PathBuf::from(action.source.as_deref().context("Move source is missing")?);
            let target = PathBuf::from(&action.target);
            let (_, source_hash) = file_identity(&source)?;
            if action.sha256.as_deref() != Some(source_hash.as_str()) {
                bail!(
                    "Kernel file changed after preflight: {}",
                    display_path(&source)
                );
            }
            let backup = PathBuf::from(action.backup.as_deref().context("Move backup is missing")?);
            copy_verified(&source, &backup, Some(&source_hash))?;
            let temporary = steam_root.join(format!(".stn-move-{}-{index}.tmp", plan.operation_id));
            if regular_file(&temporary)?.is_some() {
                bail!("Unexpected transaction staging file already exists");
            }
            fs::rename(&source, &temporary)?;
            originals.push((source.clone(), backup.clone()));
            temporaries.push((source, temporary, target));
        }
        for (source, temporary, target) in &temporaries {
            if regular_file(target)?.is_some() {
                bail!(
                    "Kernel move target already exists: {}",
                    display_path(target)
                );
            }
            fs::rename(temporary, target)?;
            targets.push(target.clone());
            let (_, hash) = file_identity(target)?;
            records.push(ManagedKernelFile {
                target: relative_string(&steam_root, target)?,
                action: ManagedKernelFileAction::Moved,
                installed_sha256: Some(hash),
                previous_sha256: None,
                previous_backup: Some(display_path(
                    &originals
                        .iter()
                        .find(|(candidate, _)| path_identity(candidate) == path_identity(source))
                        .context("Move backup lookup failed")?
                        .1,
                )),
            });
        }
        Ok(())
    })();
    if let Err(error) = outcome {
        restore_move_sources(&originals, &temporaries, &targets);
        return Err(error);
    }
    let resulting_state = match validate_result(&plan, &steam_root) {
        Ok(state) => state,
        Err(error) => {
            restore_move_sources(&originals, &temporaries, &targets);
            return Err(error);
        }
    };
    let mut receipt = receipt_without_file(&plan, true, resulting_state, records);
    let receipt_path = backup_dir.join(RECEIPT_NAME);
    receipt.receipt_path = Some(display_path(&receipt_path));
    if let Err(error) = write_receipt(&receipt_path, &receipt) {
        restore_move_sources(&originals, &temporaries, &targets);
        return Err(
            error.context("Could not persist the kernel operation receipt; operation rolled back")
        );
    }
    Ok(receipt)
}

fn execute_uninstall_plan(plan: KernelOperationPlan) -> Result<KernelOperationReceipt> {
    ensure_executable_plan(&plan)?;
    let steam_root = PathBuf::from(&plan.steam_root);
    if steam_process_running(&steam_root.join("steam.exe"))? {
        bail!("Steam started after preflight; close Steam before uninstalling the kernel");
    }
    let _guard = OperationGuard::acquire(&steam_root, &plan.operation_id)?;
    if plan.already_satisfied {
        let resulting_state = detect_kernel_installation(&steam_root)?;
        return Ok(receipt_without_file(
            &plan,
            false,
            resulting_state,
            Vec::new(),
        ));
    }
    let backup_dir = PathBuf::from(&plan.backup_dir);
    fs::create_dir_all(&backup_dir)?;
    let mut removals = Vec::new();
    let mut restores = Vec::new();
    let mut records = Vec::new();

    let outcome: Result<()> = (|| {
        for action in &plan.actions {
            if action.kind != KernelFileActionKind::Remove {
                continue;
            }
            let target = PathBuf::from(&action.target);
            if regular_file(&target)?.is_none() {
                continue;
            }
            let (_, current_hash) = file_identity(&target)?;
            if action.sha256.as_deref() != Some(current_hash.as_str()) {
                bail!("Kernel target changed after preflight: {}", action.target);
            }
            let backup = PathBuf::from(
                action
                    .backup
                    .as_deref()
                    .context("Removal backup is missing")?,
            );
            fs::create_dir_all(backup.parent().context("Removal backup has no parent")?)?;
            copy_verified(&target, &backup, Some(&current_hash))?;
            fs::remove_file(&target)?;
            removals.push((target.clone(), backup.clone()));
            records.push(ManagedKernelFile {
                target: relative_string(&steam_root, &target)?,
                action: ManagedKernelFileAction::Removed,
                installed_sha256: None,
                previous_sha256: Some(current_hash),
                previous_backup: Some(display_path(&backup)),
            });
        }
        for (index, action) in plan.actions.iter().enumerate() {
            if action.kind != KernelFileActionKind::Restore {
                continue;
            }
            let source = validate_regular_file(
                Path::new(
                    action
                        .source
                        .as_deref()
                        .context("Restore source is missing")?,
                ),
                "install backup",
            )?;
            let (_, source_hash) = file_identity(&source)?;
            if action.sha256.as_deref() != Some(source_hash.as_str()) {
                bail!("Install backup changed after preflight");
            }
            let target = PathBuf::from(&action.target);
            if regular_file(&target)?.is_some() {
                bail!("Restore target is occupied: {}", action.target);
            }
            fs::create_dir_all(target.parent().context("Restore target has no parent")?)?;
            let temporary = target
                .parent()
                .unwrap()
                .join(format!(".stn-restore-{}-{index}.tmp", plan.operation_id));
            copy_verified(&source, &temporary, Some(&source_hash))?;
            fs::rename(&temporary, &target)?;
            restores.push(target.clone());
            records.push(ManagedKernelFile {
                target: relative_string(&steam_root, &target)?,
                action: ManagedKernelFileAction::Restored,
                installed_sha256: Some(source_hash),
                previous_sha256: None,
                previous_backup: None,
            });
        }
        Ok(())
    })();
    if let Err(error) = outcome {
        rollback_uninstall(&removals, &restores);
        return Err(error);
    }
    let resulting_state = match validate_result(&plan, &steam_root) {
        Ok(state) => state,
        Err(error) => {
            rollback_uninstall(&removals, &restores);
            return Err(error);
        }
    };
    let mut receipt = receipt_without_file(&plan, true, resulting_state, records);
    let receipt_path = backup_dir.join(RECEIPT_NAME);
    receipt.receipt_path = Some(display_path(&receipt_path));
    if let Err(error) = write_receipt(&receipt_path, &receipt) {
        rollback_uninstall(&removals, &restores);
        return Err(error.context("Could not persist the uninstall receipt; uninstall rolled back"));
    }
    Ok(receipt)
}

fn ensure_executable_plan(plan: &KernelOperationPlan) -> Result<()> {
    if !plan.blockers.is_empty() {
        bail!("Kernel operation is blocked: {}", plan.blockers.join(", "));
    }
    Ok(())
}

pub(crate) fn dry_run_receipt(plan: KernelOperationPlan) -> KernelOperationReceipt {
    receipt_without_file(&plan, false, plan.previous_state.clone(), Vec::new()).with_dry_run()
}

impl KernelOperationReceipt {
    fn with_dry_run(mut self) -> Self {
        self.dry_run = true;
        self
    }
}

fn receipt_without_file(
    plan: &KernelOperationPlan,
    changed: bool,
    resulting_state: KernelInstallation,
    records: Vec<ManagedKernelFile>,
) -> KernelOperationReceipt {
    KernelOperationReceipt {
        schema_version: 1,
        operation_id: plan.operation_id.clone(),
        operation: plan.operation,
        dry_run: false,
        changed,
        steam_root: plan.steam_root.clone(),
        backup_dir: plan.backup_dir.clone(),
        receipt_path: None,
        previous_state: plan.previous_state.clone(),
        resulting_state,
        records,
        plan: plan.clone(),
    }
}

fn validate_result(plan: &KernelOperationPlan, steam_root: &Path) -> Result<KernelInstallation> {
    let state = detect_kernel_installation_with_artifact(
        steam_root,
        plan.artifact_root.as_deref().map(Path::new),
    )?;
    if state.status != plan.expected_status
        || state.injection_method != plan.expected_injection_method
    {
        bail!(
            "Kernel operation produced {} / {}, expected {} / {}",
            state.status,
            state.injection_method,
            plan.expected_status,
            plan.expected_injection_method
        );
    }
    Ok(state)
}

fn artifact_identity(root: &Path) -> (Option<String>, Option<String>) {
    let build_info = root.join("BUILD_INFO.json");
    let Ok(bytes) = fs::read(build_info) else {
        return (None, None);
    };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return (None, None);
    };
    let version = value
        .get("version")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    let sha = value
        .get("files")
        .and_then(serde_json::Value::as_array)
        .and_then(|files| {
            files.iter().find_map(|file| {
                let name = file.get("name").and_then(serde_json::Value::as_str)?;
                (name.eq_ignore_ascii_case("OpenSteamTool.dll"))
                    .then(|| file.get("sha256").and_then(serde_json::Value::as_str))
                    .flatten()
                    .map(str::to_owned)
            })
        });
    (version, sha)
}

fn copy_verified(source: &Path, destination: &Path, expected_hash: Option<&str>) -> Result<()> {
    validate_regular_file(source, "source file")?;
    reject_link_ancestors(destination)?;
    if regular_file(destination)?.is_some() {
        bail!("Transaction staging destination already exists");
    }
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
        reject_link_ancestors(parent)?;
    }
    let result: Result<()> = (|| {
        fs::copy(source, destination)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(destination)?;
        file.sync_all()?;
        let (_, hash) = file_identity(destination)?;
        if expected_hash.is_some_and(|expected| expected != hash) {
            bail!("Copied file failed SHA-256 verification");
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(destination);
    }
    result
}

fn rollback_files(committed: &[PathBuf], backups: &[(PathBuf, PathBuf)], staged: &[StagedFile]) {
    for target in committed.iter().rev() {
        let _ = fs::remove_file(target);
    }
    for (target, backup) in backups.iter().rev() {
        if backup.exists() {
            if target.exists() {
                let _ = fs::remove_file(target);
            }
            if fs::copy(backup, target).is_ok() {
                let _ = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(target)
                    .and_then(|file| file.sync_all());
            }
        }
    }
    for item in staged {
        let _ = fs::remove_file(&item.temporary);
    }
}

fn restore_move_sources(
    originals: &[(PathBuf, PathBuf)],
    temporaries: &[(PathBuf, PathBuf, PathBuf)],
    targets: &[PathBuf],
) {
    for target in targets.iter().rev() {
        let _ = fs::remove_file(target);
    }
    for (_, temporary, _) in temporaries {
        let _ = fs::remove_file(temporary);
    }
    for (source, backup) in originals {
        if source.exists() {
            let _ = fs::remove_file(source);
        }
        let _ = fs::copy(backup, source);
    }
}

fn rollback_uninstall(removals: &[(PathBuf, PathBuf)], restores: &[PathBuf]) {
    for target in restores.iter().rev() {
        let _ = fs::remove_file(target);
    }
    for (target, backup) in removals.iter().rev() {
        if target.exists() {
            let _ = fs::remove_file(target);
        }
        if backup.exists() && fs::copy(backup, target).is_ok() {
            let _ = OpenOptions::new()
                .read(true)
                .write(true)
                .open(target)
                .and_then(|file| file.sync_all());
        }
    }
}

fn write_receipt(path: &Path, receipt: &KernelOperationReceipt) -> Result<()> {
    let parent = path.parent().context("Receipt path has no parent")?;
    fs::create_dir_all(parent)?;
    reject_link_ancestors(parent)?;
    let temporary = parent.join(format!(".{RECEIPT_NAME}.tmp"));
    let bytes = serde_json::to_vec_pretty(receipt)?;
    let result: Result<()> = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn read_receipt(path: &Path) -> Result<KernelOperationReceipt> {
    let path = validate_regular_file(path, "kernel install receipt")?;
    let meta = fs::metadata(&path)?;
    if meta.len() > 4 * 1024 * 1024 {
        bail!("Kernel install receipt is unexpectedly large");
    }
    let receipt: KernelOperationReceipt = serde_json::from_slice(&fs::read(path)?)?;
    if receipt.schema_version != 1 {
        bail!("Unsupported kernel install receipt schema");
    }
    Ok(receipt)
}

fn existing_managed_files(root: &Path) -> Result<Vec<(String, PathBuf, u64, String)>> {
    let mut files = Vec::new();
    for relative in MANAGED_FILES {
        let path = safe_target(root, relative)?;
        if regular_file(&path)?.is_some() {
            let (size, sha) = file_identity(&path)?;
            files.push(((*relative).into(), path, size, sha));
        }
    }
    Ok(files)
}

fn action_priority(kind: KernelFileActionKind) -> u8 {
    match kind {
        KernelFileActionKind::Remove => 0,
        KernelFileActionKind::Restore => 1,
        _ => 2,
    }
}

fn operation_id(prefix: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{prefix}-{nanos}-{}", std::process::id())
}

fn operation_backup_dir(
    steam_root: &Path,
    requested: Option<&Path>,
    operation_id: &str,
) -> Result<PathBuf> {
    let base = match requested {
        Some(path) => absolute_local_path(path, "backup directory")?,
        None => steam_root.join(".steamtools-next").join("backups"),
    };
    reject_link_ancestors(&base)?;
    Ok(base.join(operation_id))
}

fn validate_steam_root(path: &Path) -> Result<PathBuf> {
    let root = validate_directory(path, "Steam")?;
    for required in ["steam.exe", "steamclient64.dll", "steamui.dll"] {
        validate_regular_file(&root.join(required), "Steam component")
            .with_context(|| format!("Steam directory is missing {required}"))?;
    }
    Ok(root)
}

fn validate_directory(path: &Path, label: &str) -> Result<PathBuf> {
    let path = absolute_local_path(path, label)?;
    match fs::symlink_metadata(&path) {
        Ok(meta) if is_link(&meta) => bail!("{label} path is a link or reparse point"),
        Ok(meta) if meta.is_dir() => Ok(path),
        Ok(_) => bail!("{label} path is not a directory"),
        Err(error) => Err(error).with_context(|| format!("Could not inspect {label} directory")),
    }
}

fn validate_regular_file(path: &Path, label: &str) -> Result<PathBuf> {
    let path = absolute_local_path(path, label)?;
    match regular_file(&path)? {
        Some(meta) if meta.len() <= MAX_ARTIFACT_BYTES => Ok(path),
        Some(_) => bail!("{label} exceeds the file-size limit"),
        None => bail!("{label} is missing or not a regular file"),
    }
}

pub(crate) fn absolute_local_path(path: &Path, label: &str) -> Result<PathBuf> {
    if path.as_os_str().is_empty() {
        bail!("{label} path is required");
    }
    if forbidden_namespace(path) {
        bail!("{label} must be on a local filesystem");
    }
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        bail!("{label} path must not contain parent-directory traversal");
    }
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    reject_link_ancestors(&path)?;
    Ok(path)
}

fn safe_target(root: &Path, relative: &str) -> Result<PathBuf> {
    let relative_path = Path::new(relative);
    if relative_path.is_absolute()
        || relative_path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        bail!("Managed kernel path is not a safe relative path: {relative}");
    }
    let target = root.join(relative_path);
    reject_link_ancestors(&target)?;
    Ok(target)
}

fn relative_string(root: &Path, path: &Path) -> Result<String> {
    let relative = path
        .strip_prefix(root)
        .context("Managed kernel target escaped the Steam root")?;
    if relative
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        bail!("Managed kernel target is not a safe relative path");
    }
    Ok(relative.to_string_lossy().replace('\\', "/"))
}

fn forbidden_namespace(path: &Path) -> bool {
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
        if meta.file_attributes() & 0x400 != 0 {
            return true;
        }
    }
    false
}

pub(crate) fn reject_link_ancestors(path: &Path) -> Result<()> {
    for ancestor in path.ancestors() {
        match fs::symlink_metadata(ancestor) {
            Ok(meta) if is_link(&meta) => {
                bail!("Path contains a symbolic link, junction or reparse point")
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("Could not inspect path metadata"),
        }
    }
    Ok(())
}

fn regular_file(path: &Path) -> Result<Option<Metadata>> {
    reject_link_ancestors(path)?;
    match fs::symlink_metadata(path) {
        Ok(meta) if is_link(&meta) => {
            bail!("Managed file is a symbolic link, junction or reparse point")
        }
        Ok(meta) if meta.is_file() => Ok(Some(meta)),
        Ok(_) => Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).context("Could not inspect file metadata"),
    }
}

pub(crate) fn validate_pe_x64(path: &Path) -> Result<()> {
    let meta = regular_file(path)?.context("PE artifact is not a regular file")?;
    if meta.len() < 136 || meta.len() > MAX_ARTIFACT_BYTES {
        bail!("PE artifact has an invalid size");
    }
    let mut file = File::open(path)?;
    let mut dos = [0u8; 64];
    file.read_exact(&mut dos)?;
    if &dos[..2] != b"MZ" {
        bail!("Kernel artifact is not a PE image");
    }
    let offset = u32::from_le_bytes(dos[60..64].try_into().unwrap()) as u64;
    if offset > meta.len().saturating_sub(6) || offset > 16 * 1024 * 1024 {
        bail!("Kernel artifact has an invalid PE header offset");
    }
    use std::io::{Seek, SeekFrom};
    file.seek(SeekFrom::Start(offset))?;
    let mut header = [0u8; 6];
    file.read_exact(&mut header)?;
    if &header[..4] != b"PE\0\0" {
        bail!("Kernel artifact has an invalid PE signature");
    }
    let machine = u16::from_le_bytes([header[4], header[5]]);
    if machine != 0x8664 {
        bail!("Kernel artifact is not an x64 PE image");
    }
    Ok(())
}

fn file_identity(path: &Path) -> Result<(u64, String)> {
    let meta = regular_file(path)?.context("File is missing or not regular")?;
    if meta.len() > MAX_ARTIFACT_BYTES {
        bail!("File exceeds the kernel-management size limit");
    }
    let mut file = File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok((meta.len(), format!("{:x}", digest.finalize())))
}

fn display_path(path: &Path) -> String {
    let value = path.to_string_lossy();
    value.strip_prefix(r"\\?\").unwrap_or(&value).to_owned()
}

fn path_identity(path: &Path) -> String {
    let path = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    path.to_string_lossy()
        .trim_start_matches(r"\\?\")
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_ascii_lowercase()
}

#[cfg(windows)]
fn steam_process_running(executable: &Path) -> Result<bool> {
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
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            bail!("Steam running state could not be inspected");
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
                .position(|value| *value == 0)
                .unwrap_or(entry.szExeFile.len());
            if String::from_utf16_lossy(&entry.szExeFile[..length])
                .eq_ignore_ascii_case("steam.exe")
            {
                let process =
                    OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, entry.th32ProcessID);
                if process.is_null() {
                    // Conservatively refuse to write when a Steam process cannot be identified.
                    running = true;
                    break;
                }
                let mut buffer = vec![0u16; 32768];
                let mut length = buffer.len() as u32;
                let queried =
                    QueryFullProcessImageNameW(process, 0, buffer.as_mut_ptr(), &mut length);
                CloseHandle(process);
                if queried == 0 {
                    running = true;
                    break;
                }
                let process_path =
                    PathBuf::from(String::from_utf16_lossy(&buffer[..length as usize]));
                if path_identity(&process_path) == path_identity(executable) {
                    running = true;
                    break;
                }
            }
            next = Process32NextW(snapshot, &mut entry);
        }
        CloseHandle(snapshot);
        Ok(running)
    }
}

#[cfg(not(windows))]
fn steam_process_running(_executable: &Path) -> Result<bool> {
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pe(machine: u16, marker: u8) -> Vec<u8> {
        let mut bytes = vec![marker; 256];
        bytes[..2].copy_from_slice(b"MZ");
        bytes[60..64].copy_from_slice(&128_u32.to_le_bytes());
        bytes[128..132].copy_from_slice(b"PE\0\0");
        bytes[132..134].copy_from_slice(&machine.to_le_bytes());
        bytes
    }

    fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let steam = temp.path().join("Steam");
        let artifacts = temp.path().join("artifacts");
        fs::create_dir_all(&steam).unwrap();
        fs::create_dir_all(&artifacts).unwrap();
        for name in ["steam.exe", "steamclient64.dll", "steamui.dll"] {
            fs::write(steam.join(name), b"fixture").unwrap();
        }
        for (index, name) in [
            "OpenSteamTool.dll",
            "xinput1_4.dll",
            "dwmapi.dll",
            "version.dll.disabled",
        ]
        .into_iter()
        .enumerate()
        {
            fs::write(artifacts.join(name), pe(0x8664, index as u8 + 1)).unwrap();
        }
        (temp, steam, artifacts)
    }

    fn install_request(steam: &Path, artifacts: &Path) -> KernelInstallRequest {
        KernelInstallRequest {
            steam_root: steam.to_path_buf(),
            artifact_root: artifacts.to_path_buf(),
            injection_method: InjectionMethod::Dwmapi,
            install_example_config: false,
            backup_root: None,
            dry_run: false,
        }
    }

    #[test]
    fn install_plan_and_dry_run_do_not_write() {
        let (_temp, steam, artifacts) = fixture();
        let mut request = install_request(&steam, &artifacts);
        let plan = plan_kernel_install(&request).unwrap();
        assert_eq!(plan.expected_status, "enabled");
        assert_eq!(plan.expected_injection_method, "dwmapi");
        assert_eq!(
            plan.actions
                .iter()
                .filter(|action| action.kind == KernelFileActionKind::Create)
                .count(),
            4
        );
        request.dry_run = true;
        let receipt = install_kernel(&request).unwrap();
        assert!(receipt.dry_run);
        assert!(!receipt.changed);
        assert!(!steam.join("OpenSteamTool.dll").exists());
    }

    #[test]
    fn installs_idempotently_switches_and_toggles() {
        let (_temp, steam, artifacts) = fixture();
        let request = install_request(&steam, &artifacts);
        let receipt = install_kernel(&request).unwrap();
        assert!(receipt.changed);
        assert_eq!(receipt.resulting_state.status, "enabled");
        assert_eq!(receipt.resulting_state.injection_method, "dwmapi");
        assert!(steam.join("version.dll.disabled").is_file());

        let second = install_kernel(&request).unwrap();
        assert!(!second.changed);

        let switched = switch_injection_method(&KernelSwitchRequest {
            steam_root: steam.clone(),
            injection_method: InjectionMethod::Version,
            backup_root: None,
            dry_run: false,
        })
        .unwrap();
        assert_eq!(switched.resulting_state.injection_method, "version");
        assert!(steam.join("version.dll").is_file());
        assert!(steam.join("dwmapi.dll.disabled").is_file());

        let disabled = set_kernel_enabled(&KernelToggleRequest {
            steam_root: steam.clone(),
            enabled: false,
            backup_root: None,
            dry_run: false,
        })
        .unwrap();
        assert_eq!(disabled.resulting_state.status, "disabled");
        assert!(steam.join("OpenSteamTool.dll.off").is_file());
        let enabled = set_kernel_enabled(&KernelToggleRequest {
            steam_root: steam.clone(),
            enabled: true,
            backup_root: None,
            dry_run: false,
        })
        .unwrap();
        assert_eq!(enabled.resulting_state.status, "enabled");
        assert_eq!(enabled.resulting_state.injection_method, "version");
    }

    #[test]
    fn receipt_uninstall_restores_replaced_files() {
        let (_temp, steam, artifacts) = fixture();
        let old = pe(0x8664, 0x7f);
        fs::write(steam.join("OpenSteamTool.dll"), &old).unwrap();
        fs::write(steam.join("xinput1_4.dll"), pe(0x8664, 0x7e)).unwrap();
        fs::write(steam.join("dwmapi.dll"), pe(0x8664, 0x7d)).unwrap();
        let receipt = install_kernel(&install_request(&steam, &artifacts)).unwrap();
        let receipt_path = PathBuf::from(receipt.receipt_path.as_ref().unwrap());
        let removed = uninstall_kernel(&KernelUninstallRequest {
            steam_root: steam.clone(),
            install_receipt: Some(receipt_path),
            backup_root: None,
            force: false,
            dry_run: false,
        })
        .unwrap();
        assert!(removed.changed);
        assert_eq!(fs::read(steam.join("OpenSteamTool.dll")).unwrap(), old);
        assert_eq!(
            removed.resulting_state.status,
            receipt.previous_state.status
        );
    }

    #[test]
    fn force_uninstall_preserves_configuration_and_lua() {
        let (_temp, steam, artifacts) = fixture();
        install_kernel(&install_request(&steam, &artifacts)).unwrap();
        fs::write(steam.join("opensteamtool.toml"), b"[lua]\n").unwrap();
        fs::create_dir_all(steam.join("config/lua")).unwrap();
        fs::write(steam.join("config/lua/10.lua"), b"addappid(10)\n").unwrap();
        let receipt = uninstall_kernel(&KernelUninstallRequest {
            steam_root: steam.clone(),
            install_receipt: None,
            backup_root: None,
            force: true,
            dry_run: false,
        })
        .unwrap();
        assert_eq!(receipt.resulting_state.status, "notInstalled");
        assert!(steam.join("opensteamtool.toml").is_file());
        assert!(steam.join("config/lua/10.lua").is_file());
    }

    #[test]
    fn rejects_x86_artifact() {
        let (_temp, steam, artifacts) = fixture();
        fs::write(artifacts.join("OpenSteamTool.dll"), pe(0x014c, 1)).unwrap();
        let error = plan_kernel_install(&install_request(&steam, &artifacts)).unwrap_err();
        assert!(format!("{error:#}").contains("not an x64"));
    }

    #[test]
    fn external_uninstall_requires_force_or_receipt() {
        let (_temp, steam, artifacts) = fixture();
        install_kernel(&install_request(&steam, &artifacts)).unwrap();
        let plan = plan_kernel_uninstall(&KernelUninstallRequest {
            steam_root: steam,
            install_receipt: None,
            backup_root: None,
            force: false,
            dry_run: true,
        })
        .unwrap();
        assert!(plan
            .blockers
            .iter()
            .any(|item| item == "receipt_or_force_required"));
    }
}
