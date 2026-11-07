#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use steam_tools_core::settings::{
    app_data_dir, load_settings, save_settings, system_locale, Settings,
};
use steam_tools_core::{
    ArchiveImportSummary, CatalogClient, CatalogConfig, CatalogGame, CatalogRankings,
    ImportSummary, KernelComparison, KernelInstallRequest, KernelInstallation,
    KernelOperationReceipt, KernelReport, KernelUninstallRequest, KeyCacheUpdate,
    LuaGenerationOptions, LuaGenerationResult, LuaLibrarySnapshot, LuaMutationReceipt,
    ManagedWrite, ManifestBatchResult, ManifestFetchMode, ManifestLogCursor, RemoteImportResult,
    RemoteRepositoryClient, RemoteRepositoryConfig, ReqwestManifestTransport, SteamSnapshot,
};

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct CommandError {
    code: &'static str,
    message_key: &'static str,
    message_args: BTreeMap<String, String>,
    detail: String,
}

impl CommandError {
    fn steam_session(
        fallback_key: &'static str,
        code: Option<&'static str>,
        error: impl ToString,
    ) -> Self {
        let code = code.unwrap_or("operation_failed");
        let key = match code {
            "steam_session_offline" => "error.steamSessionOffline",
            "steam_session_signed_out" => "error.steamSessionSignedOut",
            "steam_session_identity_changed" => "error.steamSessionChanged",
            "steam_session_startup_timeout" => "error.steamSessionStarting",
            "steam_client_invalid" => "error.steamClientInvalid",
            _ => fallback_key,
        };
        let mut result = Self::operation(key, error);
        result.code = code;
        result
    }
    fn operation(message_key: &'static str, detail: impl ToString) -> Self {
        Self {
            code: "operation_failed",
            message_key,
            message_args: BTreeMap::new(),
            detail: detail.to_string(),
        }
    }

    fn invalid(message_key: &'static str, detail: impl ToString) -> Self {
        Self {
            code: "invalid_input",
            message_key,
            message_args: BTreeMap::new(),
            detail: detail.to_string(),
        }
    }
}

async fn background<T, F>(message_key: &'static str, task: F) -> Result<T, CommandError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, String> + Send + 'static,
{
    tauri::async_runtime::spawn_blocking(move || {
        task().map_err(|error| CommandError::operation(message_key, error))
    })
    .await
    .map_err(|error| CommandError::operation(message_key, error))?
}

/// Preserve stable Steam-session error codes instead of flattening them to text.
async fn background_session<T, F>(message_key: &'static str, task: F) -> Result<T, CommandError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, CommandError> + Send + 'static,
{
    tauri::async_runtime::spawn_blocking(task)
        .await
        .map_err(|error| CommandError::operation(message_key, error))?
}

fn require_path(value: &str, label: &str) -> Result<PathBuf, CommandError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(CommandError::invalid(
            "error.pathRequired",
            format!("{label} is required"),
        ));
    }
    if value.contains('\0') || value.len() > 32768 {
        return Err(CommandError::invalid(
            "error.pathRequired",
            format!("{label} is invalid"),
        ));
    }
    Ok(PathBuf::from(value))
}

fn catalog_client() -> Result<CatalogClient, String> {
    let cache_dir = app_data_dir()
        .map_err(|error| error.to_string())?
        .join("cache")
        .join("catalog-v1");
    CatalogClient::new(CatalogConfig {
        cache_dir,
        ..CatalogConfig::default()
    })
    .map_err(|error| error.to_string())
}

fn command_catalog_client() -> Result<CatalogClient, String> {
    catalog_client()
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct CatalogAddReceipt {
    generation: LuaGenerationResult,
    write: ManagedWrite,
    library: LuaLibrarySnapshot,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ManifestPollResponse {
    cursor: ManifestLogCursor,
    batch: ManifestBatchResult,
}

#[tauri::command]
async fn scan_steam_command(path: Option<String>) -> Result<SteamSnapshot, CommandError> {
    background("error.steamScanFailed", move || {
        steam_tools_core::scan_steam(
            path.as_deref()
                .filter(|path| !path.trim().is_empty())
                .map(Path::new),
        )
        .map_err(|error| error.to_string())
    })
    .await
}

#[tauri::command]
async fn steam_account_state_command(
    steam_path: Option<String>,
) -> Result<steam_tools_core::SteamAccountState, CommandError> {
    let root = require_path(steam_path.as_deref().unwrap_or(""), "Steam path")?;
    background("steamAccounts.operationFailed", move || {
        steam_tools_core::get_steam_account_state(&root).map_err(|error| error.to_string())
    })
    .await
}

#[tauri::command]
async fn restart_steam_command(
    steam_path: Option<String>,
) -> Result<steam_tools_core::SteamLifecycleReceipt, CommandError> {
    let root = require_path(steam_path.as_deref().unwrap_or(""), "Steam path")?;
    background("steamAccounts.operationFailed", move || {
        steam_tools_core::restart_steam(&root).map_err(|error| error.to_string())
    })
    .await
}

#[tauri::command]
async fn relogin_steam_command(
    steam_path: Option<String>,
) -> Result<steam_tools_core::SteamLifecycleReceipt, CommandError> {
    let root = require_path(steam_path.as_deref().unwrap_or(""), "Steam path")?;
    background("steamAccounts.operationFailed", move || {
        steam_tools_core::relogin_steam(&root).map_err(|error| error.to_string())
    })
    .await
}

#[tauri::command]
async fn switch_saved_steam_account_command(
    steam_path: Option<String>,
    steam_id: String,
) -> Result<steam_tools_core::SteamLifecycleReceipt, CommandError> {
    let root = require_path(steam_path.as_deref().unwrap_or(""), "Steam path")?;
    background("steamAccounts.operationFailed", move || {
        steam_tools_core::switch_saved_steam_account(&root, &steam_id)
            .map_err(|error| error.to_string())
    })
    .await
}

#[tauri::command]
async fn inspect_authorization_ticket_command(
    source_path: String,
) -> Result<steam_tools_core::AuthorizationTicketSummary, CommandError> {
    let source = require_path(&source_path, "Authorization file")?;
    background("authorization.invalidFile", move || {
        steam_tools_core::inspect_authorization_file(&source).map_err(|error| error.to_string())
    })
    .await
}

#[tauri::command]
async fn authorization_session_command(
    steam_path: String,
    app_id: u32,
) -> Result<steam_tools_core::AuthorizationSessionSummary, CommandError> {
    let root = require_path(&steam_path, "Steam path")?;
    background_session("authorization.failed", move || {
        let executable = std::env::current_exe()
            .map_err(|error| CommandError::operation("authorization.failed", error))?;
        let cache = app_data_dir()
            .map_err(|error| CommandError::operation("authorization.failed", error))?
            .join("cache")
            .join("authorization-workers-v1");
        steam_tools_core::authorization_session(&executable, &cache, &root, app_id).map_err(
            |error| {
                CommandError::steam_session(
                    "authorization.failed",
                    steam_tools_core::steam_session_error_code(&error),
                    error,
                )
            },
        )
    })
    .await
}

#[tauri::command]
async fn extract_authorization_ticket_command(
    steam_path: String,
    app_id: u32,
    expected_steam_id: String,
    destination: String,
    overwrite: bool,
) -> Result<steam_tools_core::AuthorizationExportReceipt, CommandError> {
    let root = require_path(&steam_path, "Steam path")?;
    let destination = require_path(&destination, "Private authorization destination")?;
    background_session("authorization.failed", move || {
        let executable = std::env::current_exe()
            .map_err(|error| CommandError::operation("authorization.failed", error))?;
        let cache = app_data_dir()
            .map_err(|error| CommandError::operation("authorization.failed", error))?
            .join("cache")
            .join("authorization-workers-v1");
        steam_tools_core::extract_authorization_ticket(
            &executable,
            &cache,
            &steam_tools_core::AuthorizationExtractRequest {
                steam_root: root,
                app_id,
                expected_steam_id,
                destination,
                overwrite,
            },
        )
        .map_err(|error| {
            CommandError::steam_session(
                "authorization.failed",
                steam_tools_core::steam_session_error_code(&error),
                error,
            )
        })
    })
    .await
}

#[tauri::command]
async fn import_authorization_ticket_command(
    steam_path: String,
    app_id: u32,
    expected_steam_id: String,
    source_path: String,
    expected_sha256: String,
    overwrite: bool,
) -> Result<steam_tools_core::AuthorizationImportReceipt, CommandError> {
    let root = require_path(&steam_path, "Steam path")?;
    let source = require_path(&source_path, "Private authorization source")?;
    background_session("authorization.failed", move || {
        let executable = std::env::current_exe()
            .map_err(|error| CommandError::operation("authorization.failed", error))?;
        let cache = app_data_dir()
            .map_err(|error| CommandError::operation("authorization.failed", error))?
            .join("cache")
            .join("authorization-workers-v1");
        steam_tools_core::import_current_account_authorization_file(
            &executable,
            &cache,
            &steam_tools_core::AuthorizationImportRequest {
                steam_root: root,
                app_id,
                expected_steam_id,
                source,
                expected_sha256,
                overwrite,
            },
        )
        .map_err(|error| {
            CommandError::steam_session(
                "authorization.failed",
                steam_tools_core::steam_session_error_code(&error),
                error,
            )
        })
    })
    .await
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct UpdateCheck {
    status: &'static str,
    current_version: Option<String>,
    latest_version: Option<String>,
    release_tag: Option<String>,
    current_sha256: Option<String>,
    latest_sha256: Option<String>,
    error: Option<CommandError>,
}

impl UpdateCheck {
    fn failed(error: impl ToString) -> Self {
        Self {
            status: "error",
            current_version: None,
            latest_version: None,
            release_tag: None,
            current_sha256: None,
            latest_sha256: None,
            error: Some(CommandError::operation("updates.failed", error)),
        }
    }
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct UpdatesState {
    tool: UpdateCheck,
    kernel: UpdateCheck,
    checked_at: u64,
}

#[tauri::command]
async fn check_updates_command(steam_path: Option<String>) -> Result<UpdatesState, CommandError> {
    background("updates.failed", move || {
        // Independent provider failures do not hide a successful other check.
        let tool_task =
            std::thread::spawn(|| steam_tools_core::check_tool_update(env!("CARGO_PKG_VERSION")));
        let kernel_result = steam_tools_core::check_kernel_update(
            steam_path
                .as_deref()
                .filter(|path| !path.trim().is_empty())
                .map(Path::new),
        );
        let mut tool = match tool_task.join() {
            Ok(Ok(info)) => UpdateCheck {
                status: if info.update_available {
                    "available"
                } else {
                    "current"
                },
                current_version: Some(info.current_version),
                latest_version: Some(info.latest_version),
                release_tag: Some(info.release_tag),
                current_sha256: None,
                latest_sha256: None,
                error: None,
            },
            Ok(Err(error)) => UpdateCheck::failed(error),
            Err(_) => UpdateCheck::failed("Tool update check worker did not finish"),
        };
        if let Ok(Some(result)) = steam_tools_core::consume_last_tool_update_result() {
            if !result.success {
                tool.error = Some(CommandError::operation(
                    "updates.failed",
                    result.error.unwrap_or_else(|| {
                        "The last update did not finish; the previous version was retained".into()
                    }),
                ));
            }
        }
        let kernel = match kernel_result {
            Ok(info) => UpdateCheck {
                status: if !info.installed {
                    "notInstalled"
                } else if info.update_available {
                    "available"
                } else {
                    "current"
                },
                current_version: info.current_version,
                latest_version: Some(info.latest_version),
                release_tag: Some(info.release_tag),
                current_sha256: info.current_sha256,
                latest_sha256: Some(info.latest_sha256),
                error: None,
            },
            Err(error) => UpdateCheck::failed(error),
        };
        let checked_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| error.to_string())?
            .as_secs();
        Ok(UpdatesState {
            tool,
            kernel,
            checked_at,
        })
    })
    .await
}

static TOOL_UPDATE_IN_PROGRESS: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[derive(serde::Serialize)]
struct ToolUpdateReceipt {
    prepared: bool,
    restarting: bool,
}

#[tauri::command]
async fn install_tool_update_command(
    app: tauri::AppHandle,
    expected_version: String,
) -> Result<ToolUpdateReceipt, CommandError> {
    if !steam_tools_core::is_newer_release_version(&expected_version, env!("CARGO_PKG_VERSION"))
        .map_err(|error| CommandError::invalid("updates.failed", error))?
    {
        return Err(CommandError::invalid(
            "updates.failed",
            "Requested tool version is not newer",
        ));
    }
    if TOOL_UPDATE_IN_PROGRESS
        .compare_exchange(
            false,
            true,
            std::sync::atomic::Ordering::SeqCst,
            std::sync::atomic::Ordering::SeqCst,
        )
        .is_err()
    {
        return Err(CommandError::operation(
            "updates.failed",
            "A tool update is already in progress",
        ));
    }
    let result = background("updates.failed", move || {
        let cache = app_data_dir()
            .map_err(|error| error.to_string())?
            .join("cache")
            .join("tool-updates-v1");
        let executable = std::env::current_exe().map_err(|error| error.to_string())?;
        let prepared = steam_tools_core::prepare_tool_update(
            &cache,
            &executable,
            std::process::id(),
            env!("CARGO_PKG_VERSION"),
        )
        .map_err(|error| error.to_string())?;
        if prepared.version != expected_version {
            return Err("Latest tool release changed; check updates again".into());
        }
        steam_tools_core::launch_tool_update_helper(&prepared)
            .map_err(|error| error.to_string())?;
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(250));
            app.exit(0);
        });
        Ok(ToolUpdateReceipt {
            prepared: true,
            restarting: true,
        })
    })
    .await;
    if result.is_err() {
        TOOL_UPDATE_IN_PROGRESS.store(false, std::sync::atomic::Ordering::SeqCst);
    }
    result
}

#[tauri::command]
async fn detect_kernel_installation_command(
    steam_path: String,
    artifact_path: Option<String>,
) -> Result<KernelInstallation, CommandError> {
    let steam_path = require_path(&steam_path, "Steam path")?;
    let artifact_path = artifact_path
        .as_deref()
        .filter(|path| !path.trim().is_empty())
        .map(|value| require_path(value, "kernel artifact"))
        .transpose()?;
    background("error.kernelDetectFailed", move || {
        steam_tools_core::detect_kernel_installation_with_artifact(
            &steam_path,
            artifact_path.as_deref(),
        )
        .map_err(|error| error.to_string())
    })
    .await
}

#[tauri::command]
async fn scan_lua_library_command(steam_path: String) -> Result<LuaLibrarySnapshot, CommandError> {
    let steam_path = require_path(&steam_path, "Steam path")?;
    background("error.libraryScanFailed", move || {
        steam_tools_core::scan_lua_library(&steam_path).map_err(|error| error.to_string())
    })
    .await
}

#[tauri::command]
async fn add_lua_entry_command(
    steam_path: String,
    app_id: u32,
    custom_lua: Option<String>,
    overwrite: bool,
) -> Result<ManagedWrite, CommandError> {
    let steam_path = require_path(&steam_path, "Steam path")?;
    background("error.libraryWriteFailed", move || {
        steam_tools_core::create_lua_entry(&steam_path, app_id, custom_lua.as_deref(), overwrite)
            .map_err(|error| error.to_string())
    })
    .await
}

#[tauri::command]
async fn import_library_files_command(
    steam_path: String,
    sources: Vec<String>,
    overwrite: bool,
) -> Result<ImportSummary, CommandError> {
    let steam_root = require_path(&steam_path, "Steam path")?;
    if sources.is_empty() {
        return Err(CommandError::invalid(
            "error.pathRequired",
            "At least one import source is required",
        ));
    }
    background("error.libraryImportFailed", move || {
        steam_tools_core::import_library_files(&steam_tools_core::LibraryImportRequest {
            steam_root,
            sources: sources.into_iter().map(PathBuf::from).collect(),
            overwrite,
        })
        .map_err(|error| error.to_string())
    })
    .await
}

#[tauri::command]
async fn import_library_archives_command(
    steam_path: String,
    archives: Vec<String>,
    overwrite: bool,
) -> Result<ArchiveImportSummary, CommandError> {
    let steam_root = require_path(&steam_path, "Steam path")?;
    if archives.is_empty() {
        return Err(CommandError::invalid(
            "error.pathRequired",
            "At least one archive is required",
        ));
    }
    background("error.libraryImportFailed", move || {
        steam_tools_core::import_library_archives(&steam_tools_core::ArchiveImportRequest {
            steam_root,
            archives: archives.into_iter().map(PathBuf::from).collect(),
            overwrite,
        })
        .map_err(|error| error.to_string())
    })
    .await
}

#[tauri::command]
async fn set_lua_entry_enabled_command(
    steam_path: String,
    app_id: u32,
    enabled: bool,
    overwrite: bool,
) -> Result<LuaMutationReceipt, CommandError> {
    let steam_path = require_path(&steam_path, "Steam path")?;
    background("error.libraryWriteFailed", move || {
        steam_tools_core::set_lua_entry_enabled(&steam_path, app_id, enabled, overwrite)
            .map_err(|error| error.to_string())
    })
    .await
}

#[tauri::command]
async fn delete_lua_entry_command(
    steam_path: String,
    app_id: u32,
) -> Result<LuaMutationReceipt, CommandError> {
    let steam_path = require_path(&steam_path, "Steam path")?;
    background("error.libraryWriteFailed", move || {
        steam_tools_core::delete_lua_entry(&steam_path, app_id).map_err(|error| error.to_string())
    })
    .await
}

#[tauri::command]
async fn search_catalog_command(
    query: String,
    locale: String,
) -> Result<Vec<CatalogGame>, CommandError> {
    if query.trim().is_empty() {
        return Err(CommandError::invalid(
            "error.catalogSearchFailed",
            "Search query is required",
        ));
    }
    background("error.catalogSearchFailed", move || {
        command_catalog_client()?
            .search(&query, &locale)
            .map_err(|error| error.to_string())
    })
    .await
}

#[tauri::command]
async fn catalog_rankings_command() -> Result<CatalogRankings, CommandError> {
    background("error.catalogRankingsFailed", move || {
        command_catalog_client()?
            .rankings()
            .map_err(|error| error.to_string())
    })
    .await
}

#[tauri::command]
async fn refresh_key_cache_command() -> Result<KeyCacheUpdate, CommandError> {
    background("error.keyCacheFailed", move || {
        command_catalog_client()?
            .update_key_cache()
            .map_err(|error| error.to_string())
    })
    .await
}

#[tauri::command]
async fn catalog_add_command(
    steam_path: String,
    app_id: u32,
    options: LuaGenerationOptions,
    overwrite: bool,
) -> Result<CatalogAddReceipt, CommandError> {
    let steam_path = require_path(&steam_path, "Steam path")?;
    background("error.catalogAddFailed", move || {
        let generation = catalog_client()?
            .generate_lua(app_id, &options)
            .map_err(|error| error.to_string())?;
        let write = steam_tools_core::create_lua_entry(
            &steam_path,
            app_id,
            Some(&generation.lua),
            overwrite,
        )
        .map_err(|error| error.to_string())?;
        let library =
            steam_tools_core::scan_lua_library(&steam_path).map_err(|error| error.to_string())?;
        Ok(CatalogAddReceipt {
            generation,
            write,
            library,
        })
    })
    .await
}

#[tauri::command]
async fn remote_library_import_command(
    steam_path: String,
    app_id: u32,
    overwrite: bool,
) -> Result<RemoteImportResult, CommandError> {
    let steam_root = require_path(&steam_path, "Steam path")?;
    background("error.catalogAddFailed", move || {
        RemoteRepositoryClient::new(RemoteRepositoryConfig::default())
            .and_then(|client| {
                client.import_package(&steam_tools_core::RemoteImportRequest {
                    steam_root,
                    app_id,
                    overwrite,
                })
            })
            .map_err(|error| error.to_string())
    })
    .await
}

#[tauri::command]
async fn install_remote_kernel_command(
    request: RemoteKernelInstallRequest,
) -> Result<RemoteKernelReceipt, CommandError> {
    background_session("error.kernelInstallFailed", move || {
        let (release, request) = prepare_remote_kernel_install(request)?;
        let receipt = steam_tools_core::install_kernel_release(&request, &release)
            .map_err(|error| CommandError::operation("error.kernelInstallFailed", error))?;
        Ok(RemoteKernelReceipt { release, receipt })
    })
    .await
}

/// The desktop boundary accepts no source or injection overrides; both are built in.
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RemoteKernelInstallRequest {
    steam_root: String,
    #[serde(default)]
    install_example_config: bool,
    #[serde(default)]
    backup_root: Option<PathBuf>,
    #[serde(default)]
    dry_run: bool,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct RemoteKernelReceipt {
    release: steam_tools_core::KernelReleaseArtifact,
    receipt: KernelOperationReceipt,
}

fn prepare_remote_kernel_install(
    request: RemoteKernelInstallRequest,
) -> Result<
    (
        steam_tools_core::KernelReleaseArtifact,
        KernelInstallRequest,
    ),
    CommandError,
> {
    let steam_root = require_path(&request.steam_root, "Steam path")?;
    // Reject a running target before starting a potentially lengthy download.
    steam_tools_core::ensure_kernel_target_stopped(&steam_root).map_err(|error| {
        let key = if error.to_string() == "steam_running" {
            "kernel.stopSteamFirst"
        } else {
            "error.kernelPlanFailed"
        };
        CommandError::operation(key, error)
    })?;
    let cache = app_data_dir()
        .map_err(|error| CommandError::operation("error.kernelDownloadFailed", error))?
        .join("cache")
        .join("kernel-releases-v1");
    let release = steam_tools_core::acquire_kernel_release(
        steam_tools_core::DEFAULT_KERNEL_RELEASE_SOURCE,
        &cache,
    )
    .map_err(|error| CommandError::operation("error.kernelDownloadFailed", error))?;
    let install = KernelInstallRequest {
        steam_root,
        artifact_root: PathBuf::from(&release.artifact_root),
        injection_method: steam_tools_core::InjectionMethod::Dwmapi,
        install_example_config: request.install_example_config,
        backup_root: request.backup_root,
        dry_run: request.dry_run,
    };
    Ok((release, install))
}

#[tauri::command]
async fn uninstall_kernel_command(
    request: KernelUninstallRequest,
) -> Result<KernelOperationReceipt, CommandError> {
    background("error.kernelUninstallFailed", move || {
        steam_tools_core::uninstall_kernel(&request).map_err(|error| error.to_string())
    })
    .await
}

#[tauri::command]
async fn poll_manifest_command(
    steam_path: String,
    mut cursor: ManifestLogCursor,
    mode: String,
    api_key: Option<String>,
    limit: usize,
    download: bool,
) -> Result<ManifestPollResponse, CommandError> {
    let steam_path = require_path(&steam_path, "Steam path")?;
    let fetch_mode = match mode.as_str() {
        "code" | "codeDirect" => ManifestFetchMode::CodeDirect,
        "manifestHub" => ManifestFetchMode::ManifestHub {
            api_key: api_key.unwrap_or_default(),
        },
        _ => {
            return Err(CommandError::invalid(
                "error.manifestPollFailed",
                "Manifest mode must be code or manifestHub",
            ))
        }
    };
    background("error.manifestPollFailed", move || {
        let batch = if download {
            let transport = ReqwestManifestTransport::new().map_err(|error| error.to_string())?;
            steam_tools_core::process_manifest_log_once(
                &steam_path,
                &mut cursor,
                &transport,
                &fetch_mode,
                limit,
            )
            .map_err(|error| error.to_string())?
        } else {
            let poll = steam_tools_core::poll_manifest_requests(&steam_path, &mut cursor)
                .map_err(|error| error.to_string())?;
            ManifestBatchResult {
                poll,
                downloaded: Vec::new(),
                failed: Vec::new(),
            }
        };
        Ok(ManifestPollResponse { cursor, batch })
    })
    .await
}

#[tauri::command]
fn load_settings_command() -> Result<Settings, CommandError> {
    load_settings().map_err(|error| CommandError::operation("error.settingsLoadFailed", error))
}

#[tauri::command]
fn save_settings_command(settings: Settings) -> Result<(), CommandError> {
    save_settings(&settings)
        .map_err(|error| CommandError::operation("error.settingsSaveFailed", error))
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct Environment {
    locale: String,
    version: &'static str,
    default_kernel_path: Option<String>,
    default_kernel_artifact_path: Option<String>,
    default_kernel_release_source: &'static str,
    desktop: bool,
}

#[tauri::command]
fn get_environment_command() -> Environment {
    Environment {
        locale: system_locale(),
        version: env!("CARGO_PKG_VERSION"),
        default_kernel_path: None,
        default_kernel_artifact_path: None,
        default_kernel_release_source: steam_tools_core::DEFAULT_KERNEL_RELEASE_SOURCE,
        desktop: true,
    }
}

#[tauri::command]
async fn export_report_command(
    snapshot: Option<SteamSnapshot>,
    kernel: Option<KernelReport>,
    comparison: Option<KernelComparison>,
    installed_kernel: Option<KernelReport>,
    kernel_installation: Option<KernelInstallation>,
) -> Result<String, CommandError> {
    if snapshot.is_none()
        && kernel.is_none()
        && comparison.is_none()
        && installed_kernel.is_none()
        && kernel_installation.is_none()
    {
        return Err(CommandError::invalid(
            "error.reportEmpty",
            "Run a scan or operation before exporting a report",
        ));
    }
    let root = app_data_dir()
        .map_err(|error| CommandError::operation("error.exportFailed", error))?
        .join("reports");
    background("error.exportFailed", move || {
        std::fs::create_dir_all(&root).map_err(|error| error.to_string())?;
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| error.to_string())?
            .as_millis();
        let report = steam_tools_core::build_report_with_installation_context(
            snapshot,
            kernel,
            comparison,
            installed_kernel,
            kernel_installation,
        );
        let bytes = serde_json::to_vec_pretty(&report).map_err(|error| error.to_string())?;
        let destination = root.join(format!(
            "stn-report-{timestamp}-{}.json",
            std::process::id()
        ));
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&destination)
            .map_err(|error| error.to_string())?;
        file.write_all(&bytes)
            .and_then(|_| file.sync_all())
            .map_err(|error| error.to_string())?;
        Ok(destination.to_string_lossy().into_owned())
    })
    .await
}

#[tauri::command]
async fn list_achievements_command(
    steam_path: Option<String>,
    app_id: u32,
    locale: String,
) -> Result<steam_tools_core::AchievementCatalog, CommandError> {
    background_session("error.achievementLoadFailed", move || {
        let key = "error.achievementLoadFailed";
        let executable =
            std::env::current_exe().map_err(|error| CommandError::operation(key, error))?;
        let cache = app_data_dir()
            .map_err(|error| CommandError::operation(key, error))?
            .join("cache")
            .join("achievement-workers-v1");
        steam_tools_core::list_achievements(
            &executable,
            &cache,
            steam_path
                .as_deref()
                .filter(|path| !path.trim().is_empty())
                .map(Path::new),
            app_id,
            &locale,
        )
        .map_err(|error| {
            let code =
                steam_tools_core::steam_session_error_code(&error).unwrap_or("operation_failed");
            let mut result = CommandError::operation(key, error);
            result.code = code;
            result
        })
    })
    .await
}

#[tauri::command]
async fn apply_achievement_changes_command(
    steam_path: Option<String>,
    app_id: u32,
    locale: String,
    changes: Vec<steam_tools_core::AchievementChange>,
) -> Result<steam_tools_core::AchievementApplyResult, CommandError> {
    background_session("error.achievementApplyFailed", move || {
        let key = "error.achievementApplyFailed";
        let executable =
            std::env::current_exe().map_err(|error| CommandError::operation(key, error))?;
        let cache = app_data_dir()
            .map_err(|error| CommandError::operation(key, error))?
            .join("cache")
            .join("achievement-workers-v1");
        steam_tools_core::apply_achievement_changes(
            &executable,
            &cache,
            steam_path
                .as_deref()
                .filter(|path| !path.trim().is_empty())
                .map(Path::new),
            app_id,
            &locale,
            changes,
        )
        .map_err(|error| {
            let code =
                steam_tools_core::steam_session_error_code(&error).unwrap_or("operation_failed");
            let mut result = CommandError::operation(key, error);
            result.code = code;
            result
        })
    })
    .await
}

#[tauri::command]
async fn load_game_cover_command(
    steam_path: Option<String>,
    app_id: u32,
    language: String,
    candidates: Vec<String>,
    cache_only: Option<bool>,
) -> Result<Option<steam_tools_core::GameCover>, CommandError> {
    background("error.coverLoadFailed", move || {
        let cache = app_data_dir()
            .map_err(|error| error.to_string())?
            .join("cache")
            .join("covers-v1");
        let steam_path = steam_path
            .as_deref()
            .filter(|path| !path.trim().is_empty())
            .map(Path::new);
        if cache_only.unwrap_or(false) {
            steam_tools_core::load_cached_game_cover(steam_path, &cache, app_id, &language)
                .map_err(|error| error.to_string())
        } else {
            steam_tools_core::load_game_cover(steam_path, &cache, app_id, &language, &candidates)
                .map_err(|error| error.to_string())
        }
    })
    .await
}

#[tauri::command]
async fn load_achievement_image_command(
    app_id: u32,
    image: String,
    fallback_image: Option<String>,
    cache_only: Option<bool>,
) -> Result<Option<steam_tools_core::GameCover>, CommandError> {
    background("error.coverLoadFailed", move || {
        let cache = app_data_dir()
            .map_err(|error| error.to_string())?
            .join("cache")
            .join("achievement-images-v1");
        if cache_only.unwrap_or(false) {
            steam_tools_core::load_cached_achievement_image(&cache, app_id, &image)
                .map_err(|error| error.to_string())
        } else {
            steam_tools_core::load_achievement_image(
                &cache,
                app_id,
                &image,
                fallback_image.as_deref(),
            )
            .map_err(|error| error.to_string())
        }
    })
    .await
}

#[tauri::command]
async fn current_steam_session_command(
    steam_path: Option<String>,
) -> Result<steam_tools_core::CurrentSteamSession, CommandError> {
    background_session("error.extractFailed", move || {
        let exe = std::env::current_exe()
            .map_err(|error| CommandError::operation("error.extractFailed", error))?;
        let cache = app_data_dir()
            .map_err(|error| CommandError::operation("error.extractFailed", error))?
            .join("cache")
            .join("extract-workers-v1");
        steam_tools_core::current_steam_session(
            &exe,
            &cache,
            steam_path
                .as_deref()
                .filter(|path| !path.trim().is_empty())
                .map(Path::new),
        )
        .map_err(|error| {
            CommandError::steam_session(
                "error.extractFailed",
                steam_tools_core::steam_session_error_code(&error),
                error,
            )
        })
    })
    .await
}
#[tauri::command]
async fn extract_current_session_command(
    steam_path: Option<String>,
    app_id: u32,
    locale: String,
    expected_account_fingerprint: Option<String>,
) -> Result<steam_tools_core::CurrentSessionExtraction, CommandError> {
    background_session("error.extractFailed", move || {
        let exe = std::env::current_exe()
            .map_err(|error| CommandError::operation("error.extractFailed", error))?;
        let cache = app_data_dir()
            .map_err(|error| CommandError::operation("error.extractFailed", error))?
            .join("cache")
            .join("extract-workers-v1");
        steam_tools_core::extract_current_session(
            &exe,
            &cache,
            steam_path
                .as_deref()
                .filter(|path| !path.trim().is_empty())
                .map(Path::new),
            app_id,
            &locale,
            expected_account_fingerprint.as_deref(),
        )
        .map_err(|error| {
            CommandError::steam_session(
                "error.extractFailed",
                steam_tools_core::steam_session_error_code(&error),
                error,
            )
        })
    })
    .await
}
#[tauri::command]
async fn export_current_session_command(
    steam_path: Option<String>,
    app_id: u32,
    locale: String,
    expected_account_fingerprint: Option<String>,
    destination_directory: String,
) -> Result<steam_tools_core::CurrentSessionExport, CommandError> {
    let destination = require_path(&destination_directory, "Extraction export directory")?;
    background_session("error.extractFailed", move || {
        let exe = std::env::current_exe()
            .map_err(|error| CommandError::operation("error.extractFailed", error))?;
        let cache = app_data_dir()
            .map_err(|error| CommandError::operation("error.extractFailed", error))?
            .join("cache")
            .join("extract-workers-v1");
        steam_tools_core::export_current_session(
            &exe,
            &cache,
            steam_path
                .as_deref()
                .filter(|path| !path.trim().is_empty())
                .map(Path::new),
            app_id,
            &locale,
            expected_account_fingerprint.as_deref(),
            &destination,
        )
        .map_err(|error| {
            CommandError::steam_session(
                "error.extractFailed",
                steam_tools_core::steam_session_error_code(&error),
                error,
            )
        })
    })
    .await
}

fn main() {
    let args: Vec<_> = std::env::args_os().collect();
    if args
        .get(1)
        .is_some_and(|arg| arg == steam_tools_core::AUTHORIZATION_WORKER_ARGUMENT)
    {
        if args.len() != 4 {
            std::process::exit(2);
        }
        let result =
            steam_tools_core::run_authorization_worker(Path::new(&args[2]), Path::new(&args[3]));
        std::process::exit(if result.is_ok() { 0 } else { 2 });
    }
    if args
        .get(1)
        .is_some_and(|arg| arg == steam_tools_core::TOOL_UPDATE_HELPER_ARGUMENT)
    {
        if args.len() != 3 {
            std::process::exit(2);
        }
        let result = steam_tools_core::run_tool_update_helper(Path::new(&args[2]));
        std::process::exit(if result.is_ok_and(|result| result.success) {
            0
        } else {
            2
        });
    }
    if args.get(1).is_some_and(|arg| arg == "--extract-worker") {
        if args.len() != 4 {
            std::process::exit(2);
        }
        let result = steam_tools_core::run_extract_worker(Path::new(&args[2]), Path::new(&args[3]));
        std::process::exit(if result.is_ok() { 0 } else { 2 });
    }
    if args.get(1).is_some_and(|arg| arg == "--achievement-worker") {
        if args.len() != 4 {
            std::process::exit(2);
        }
        let result =
            steam_tools_core::run_achievement_worker(Path::new(&args[2]), Path::new(&args[3]));
        std::process::exit(if result.is_ok() { 0 } else { 2 });
    }
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            scan_steam_command,
            steam_account_state_command,
            restart_steam_command,
            relogin_steam_command,
            switch_saved_steam_account_command,
            authorization_session_command,
            inspect_authorization_ticket_command,
            extract_authorization_ticket_command,
            import_authorization_ticket_command,
            check_updates_command,
            install_tool_update_command,
            current_steam_session_command,
            extract_current_session_command,
            export_current_session_command,
            detect_kernel_installation_command,
            scan_lua_library_command,
            add_lua_entry_command,
            import_library_files_command,
            import_library_archives_command,
            set_lua_entry_enabled_command,
            delete_lua_entry_command,
            search_catalog_command,
            list_achievements_command,
            apply_achievement_changes_command,
            load_game_cover_command,
            load_achievement_image_command,
            catalog_rankings_command,
            refresh_key_cache_command,
            catalog_add_command,
            remote_library_import_command,
            install_remote_kernel_command,
            uninstall_kernel_command,
            poll_manifest_command,
            load_settings_command,
            save_settings_command,
            get_environment_command,
            export_report_command
        ])
        .run(tauri::generate_context!())
        .expect("SteamTools Next could not start");
}

#[cfg(test)]
mod ipc_acl_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_error_separates_translation_metadata_from_technical_detail() {
        let error = CommandError::operation("error.steamScanFailed", "raw OS detail");
        let value = serde_json::to_value(error).unwrap();
        assert_eq!(value["code"], "operation_failed");
        assert_eq!(value["messageKey"], "error.steamScanFailed");
        assert_eq!(value["messageArgs"], serde_json::json!({}));
        assert_eq!(value["detail"], "raw OS detail");
    }

    #[test]
    fn invalid_input_has_stable_localizable_key() {
        let value =
            serde_json::to_value(CommandError::invalid("error.pathRequired", "Path is empty"))
                .unwrap();
        assert_eq!(value["code"], "invalid_input");
        assert_eq!(value["messageKey"], "error.pathRequired");
    }

    #[test]
    fn offline_extraction_error_keeps_native_code_and_user_facing_message() {
        let value = serde_json::to_value(CommandError::steam_session(
            "error.extractFailed",
            Some("steam_session_offline"),
            "Steam is not running",
        ))
        .unwrap();
        assert_eq!(value["code"], "steam_session_offline");
        assert_eq!(value["messageKey"], "error.steamSessionOffline");
        assert_eq!(value["detail"], "Steam is not running");
    }

    #[test]
    fn invalid_client_extraction_error_is_not_an_os_bad_image_dialog() {
        let value = serde_json::to_value(CommandError::steam_session(
            "error.extractFailed",
            Some("steam_client_invalid"),
            "Native DLL was rejected before loading",
        ))
        .unwrap();
        assert_eq!(value["code"], "steam_client_invalid");
        assert_eq!(value["messageKey"], "error.steamClientInvalid");
    }

    #[test]
    fn remote_install_boundary_rejects_legacy_local_artifacts() {
        let value = serde_json::json!({
            "steamRoot": "C:\\Steam",
            "artifactRoot": "C:\\local-kernel"
        });
        assert!(serde_json::from_value::<RemoteKernelInstallRequest>(value).is_err());
    }

    #[test]
    fn remote_install_boundary_has_no_artifact_path() {
        let value = serde_json::json!({
            "steamRoot": "C:\\Steam"
        });
        let request: RemoteKernelInstallRequest = serde_json::from_value(value).unwrap();
        assert!(!request.dry_run);
        assert!(!request.install_example_config);
        assert_eq!(get_environment_command().default_kernel_artifact_path, None);
        assert_eq!(get_environment_command().default_kernel_path, None);
    }

    #[test]
    fn remote_install_boundary_rejects_source_and_method_overrides() {
        for field in ["source", "injectionMethod"] {
            let mut value = serde_json::json!({"steamRoot": "C:\\Steam"});
            value[field] = serde_json::json!("override");
            assert!(serde_json::from_value::<RemoteKernelInstallRequest>(value).is_err());
        }
    }
}
