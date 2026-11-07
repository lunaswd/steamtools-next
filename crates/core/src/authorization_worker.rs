//! User-confirmed authorization operations through a per-application worker.
//!
//! Only the current signed-in account and an application it owns are accepted.
//! Ownership and encrypted tickets are obtained from Steam, never fabricated,
//! decrypted or modified. Private bytes stay in the worker and the explicitly
//! selected local ticket file; IPC returns only metadata and typed errors.
//!
//! Interface layout reference: Fluent-Steam-Lua TicketWorker (zlib License).
use crate::authorization::{
    export_authorization_ticket, import_authorization_file_guarded, AuthorizationExportReceipt,
    AuthorizationImportContext, AuthorizationImportReceipt, AuthorizationTicket, TicketOffsets,
};
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
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

pub const AUTHORIZATION_WORKER_ARGUMENT: &str = "--authorization-worker";
const STEAM_ID_BASE: u64 = 76_561_197_960_265_728;
const MAX_IPC_BYTES: u64 = 1024 * 1024;
const MAX_OWNERSHIP_BYTES: usize = 2048;
const MAX_ENCRYPTED_BYTES: usize = 4096;
const WORKER_TIMEOUT: Duration = Duration::from_secs(45);
const ENCRYPTED_TIMEOUT: Duration = Duration::from_secs(15);
static WORKER_LOCK: Mutex<()> = Mutex::new(());
static SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone)]
pub struct AuthorizationExtractRequest {
    pub steam_root: PathBuf,
    pub app_id: u32,
    pub expected_steam_id: String,
    pub destination: PathBuf,
    pub overwrite: bool,
}

#[derive(Clone)]
pub struct AuthorizationImportRequest {
    pub steam_root: PathBuf,
    pub app_id: u32,
    pub expected_steam_id: String,
    pub source: PathBuf,
    pub expected_sha256: String,
    pub overwrite: bool,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthorizationSessionSummary {
    pub steam_root: String,
    pub app_id: u32,
    pub steam_id: String,
    pub connected: bool,
    pub subscribed: bool,
}

impl std::fmt::Debug for AuthorizationSessionSummary {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AuthorizationSessionSummary")
            .field("connected", &self.connected)
            .field("subscribed", &self.subscribed)
            .field("identity", &"<REDACTED>")
            .finish()
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WorkerRequest {
    steam_root: PathBuf,
    app_id: u32,
    expected_steam_id: Option<String>,
    operation: WorkerOperation,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind", deny_unknown_fields)]
enum WorkerOperation {
    Session,
    Extract {
        destination: PathBuf,
        overwrite: bool,
    },
    Import {
        source: PathBuf,
        expected_sha256: String,
        overwrite: bool,
    },
}

#[derive(Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WorkerResponse {
    session: Option<AuthorizationSessionSummary>,
    exported: Option<AuthorizationExportReceipt>,
    imported: Option<AuthorizationImportReceipt>,
    error: Option<String>,
    error_code: Option<String>,
}

/// Inspect the live account/subscription only. No ticket API is called.
pub fn authorization_session(
    executable: &Path,
    cache: &Path,
    steam_root: &Path,
    app_id: u32,
) -> Result<AuthorizationSessionSummary> {
    let response = invoke_worker(
        executable,
        cache,
        WorkerRequest {
            steam_root: steam_root.to_path_buf(),
            app_id,
            expected_steam_id: None,
            operation: WorkerOperation::Session,
        },
    )?;
    response
        .session
        .context("Authorization worker returned no current-session metadata")
}

pub fn extract_authorization_ticket(
    executable: &Path,
    cache: &Path,
    request: &AuthorizationExtractRequest,
) -> Result<AuthorizationExportReceipt> {
    let response = invoke_worker(
        executable,
        cache,
        WorkerRequest {
            steam_root: request.steam_root.clone(),
            app_id: request.app_id,
            expected_steam_id: Some(request.expected_steam_id.clone()),
            operation: WorkerOperation::Extract {
                destination: request.destination.clone(),
                overwrite: request.overwrite,
            },
        },
    )?;
    response
        .exported
        .context("Authorization worker returned no export receipt")
}

/// Revalidate the current native account and its subscription before a registry
/// write. Frontend JSON cannot create the opaque import authorization context.
pub fn import_current_account_authorization_file(
    executable: &Path,
    cache: &Path,
    request: &AuthorizationImportRequest,
) -> Result<AuthorizationImportReceipt> {
    let response = invoke_worker(
        executable,
        cache,
        WorkerRequest {
            steam_root: request.steam_root.clone(),
            app_id: request.app_id,
            expected_steam_id: Some(request.expected_steam_id.clone()),
            operation: WorkerOperation::Import {
                source: request.source.clone(),
                expected_sha256: request.expected_sha256.clone(),
                overwrite: request.overwrite,
            },
        },
    )?;
    response
        .imported
        .context("Authorization worker returned no import receipt")
}

fn valid_identity(id: u64) -> bool {
    id.checked_sub(STEAM_ID_BASE)
        .is_some_and(|value| value > 0 && value <= u64::from(u32::MAX))
}

fn validate_request(request: &WorkerRequest) -> Result<Option<u64>> {
    ensure!(request.app_id > 0, "Authorization AppID must be positive");
    if let Some(raw) = &request.expected_steam_id {
        let id = raw
            .parse::<u64>()
            .context("Authorization account identifier is invalid")?;
        ensure!(
            valid_identity(id),
            "Authorization account identifier is invalid"
        );
        Ok(Some(id))
    } else {
        ensure!(
            matches!(request.operation, WorkerOperation::Session),
            "Confirm the current Steam account before ticket operations"
        );
        Ok(None)
    }
}

struct IpcDirectory(PathBuf);
impl Drop for IpcDirectory {
    fn drop(&mut self) {
        for name in ["request.json", "response.json"] {
            let _ = fs::remove_file(self.0.join(name));
        }
        let _ = fs::remove_dir(&self.0);
    }
}

fn bounded_regular(path: &Path, limit: u64) -> Result<Vec<u8>> {
    crate::kernel_manage::reject_link_ancestors(path)?;
    let metadata =
        fs::symlink_metadata(path).context("Authorization worker file is unavailable")?;
    ensure!(
        metadata.is_file() && metadata.len() <= limit,
        "Authorization worker file is not a bounded regular file"
    );
    let mut result = Vec::new();
    File::open(path)?.take(limit + 1).read_to_end(&mut result)?;
    ensure!(
        result.len() as u64 <= limit,
        "Authorization worker data exceeded its limit"
    );
    Ok(result)
}

fn invoke_worker(
    executable: &Path,
    cache: &Path,
    request: WorkerRequest,
) -> Result<WorkerResponse> {
    validate_request(&request)?;
    crate::steam_native::ensure_session_running(&request.steam_root)?;
    let _lock = WORKER_LOCK
        .lock()
        .map_err(|_| anyhow::anyhow!("Authorization worker lock is poisoned"))?;
    crate::steam_native::ensure_session_running(&request.steam_root)?;
    let executable =
        crate::kernel_manage::absolute_local_path(executable, "Authorization worker executable")?;
    ensure!(
        fs::canonicalize(&executable)? == fs::canonicalize(std::env::current_exe()?)?,
        "Authorization worker must be the current application executable"
    );
    let cache = crate::kernel_manage::absolute_local_path(cache, "Authorization worker cache")?;
    fs::create_dir_all(&cache)?;
    crate::kernel_manage::reject_link_ancestors(&cache)?;
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let files = IpcDirectory(cache.join(format!(
        "authorization-{}-{stamp}-{}",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    )));
    fs::create_dir(&files.0)?;
    let input = files.0.join("request.json");
    let output = files.0.join("response.json");
    let mut input_file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&input)?;
    input_file.write_all(&serde_json::to_vec(&request)?)?;
    input_file.sync_all()?;
    drop(input_file);
    crate::steam_native::ensure_session_running(&request.steam_root)?;
    let mut command = Command::new(&executable);
    command
        .arg(AUTHORIZATION_WORKER_ARGUMENT)
        .arg(&input)
        .arg(&output)
        .env("SteamAppId", request.app_id.to_string())
        .env("SteamGameId", request.app_id.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(parent) = executable.parent() {
        command.current_dir(parent);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    let mut child = command
        .spawn()
        .context("Could not start authorization worker")?;
    let deadline = Instant::now() + WORKER_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                ensure!(status.success(), "Authorization worker exited unexpectedly");
                break;
            }
            Ok(None) => {}
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error).context("Could not wait for authorization worker");
            }
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("Authorization worker timed out; no ticket operation was confirmed");
        }
        thread::sleep(Duration::from_millis(25));
    }
    let response: WorkerResponse =
        serde_json::from_slice(&bounded_regular(&output, MAX_IPC_BYTES)?)
            .context("Authorization worker returned invalid metadata")?;
    if let Some(error) = response.error.as_ref() {
        return Err(match response.error_code.as_deref() {
            Some(code) => crate::achievements::steam_session_error_from_code(code, error.clone()),
            None => anyhow::anyhow!(error.clone()),
        });
    }
    Ok(response)
}

/// Dispatch before the Tauri window is created. Native libraries are loaded
/// only in this private, per-application worker after fresh process preflight.
pub fn run_authorization_worker(input: &Path, output: &Path) -> Result<()> {
    let request: WorkerRequest = serde_json::from_slice(&bounded_regular(input, MAX_IPC_BYTES)?)
        .context("Authorization worker request is invalid")?;
    let output =
        crate::kernel_manage::absolute_local_path(output, "Authorization worker response")?;
    ensure!(
        input.parent().map(fs::canonicalize).transpose()?
            == output.parent().map(fs::canonicalize).transpose()?,
        "Authorization response must stay in its private IPC directory"
    );
    let response = (|| -> Result<WorkerResponse> {
        validate_request(&request)?;
        crate::steam_native::ensure_session_running(&request.steam_root)?;
        execute_native(&request)
    })()
    .unwrap_or_else(|error| WorkerResponse {
        error_code: crate::achievements::steam_session_error_code(&error).map(str::to_owned),
        error: Some(error.to_string()),
        ..Default::default()
    });
    let mut result_file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(output)?;
    result_file.write_all(&serde_json::to_vec(&response)?)?;
    result_file.sync_all()?;
    Ok(())
}

struct SensitiveBytes(Vec<u8>);
impl SensitiveBytes {
    fn into_vec(mut self) -> Vec<u8> {
        std::mem::take(&mut self.0)
    }
}
impl Drop for SensitiveBytes {
    fn drop(&mut self) {
        for byte in &mut self.0 {
            // Keep erasure observable to the optimizer; no private ticket bytes
            // are retained in a dead temporary allocation after a failed step.
            unsafe {
                std::ptr::write_volatile(byte, 0);
            }
        }
    }
}
struct OwnershipTicket {
    bytes: SensitiveBytes,
    offsets: TicketOffsets,
}

trait LiveAuthorization {
    fn identity(&mut self) -> Result<u64>;
    fn subscribed(&mut self, app_id: u32) -> Result<bool>;
    fn ownership_ticket(&mut self, app_id: u32) -> Result<OwnershipTicket>;
    fn encrypted_ticket(&mut self) -> Result<SensitiveBytes>;
}

fn changed_account() -> anyhow::Error {
    crate::achievements::steam_session_error_from_code(
        "steam_session_identity_changed",
        "Current Steam account changed; reconnect and confirm the target account".into(),
    )
}

fn ensure_identity(live: &mut dyn LiveAuthorization, expected: u64) -> Result<()> {
    if live.identity()? != expected {
        return Err(changed_account());
    }
    Ok(())
}

fn require_subscription(live: &mut dyn LiveAuthorization, app_id: u32) -> Result<()> {
    ensure!(
        live.subscribed(app_id)?,
        "The current Steam account does not own the selected application"
    );
    Ok(())
}

fn execute_with(
    request: &WorkerRequest,
    live: &mut dyn LiveAuthorization,
) -> Result<WorkerResponse> {
    let expected = validate_request(request)?;
    let initial = live.identity()?;
    ensure!(
        valid_identity(initial),
        "The current Steam account identity is invalid"
    );
    if expected.is_some_and(|id| id != initial) {
        return Err(changed_account());
    }
    let subscribed = live.subscribed(request.app_id)?;
    if matches!(request.operation, WorkerOperation::Session) {
        ensure_identity(live, initial)?;
        return Ok(WorkerResponse {
            session: Some(AuthorizationSessionSummary {
                steam_root: request.steam_root.to_string_lossy().into_owned(),
                app_id: request.app_id,
                steam_id: initial.to_string(),
                connected: true,
                subscribed,
            }),
            ..Default::default()
        });
    }
    ensure!(
        subscribed,
        "The current Steam account does not own the selected application"
    );
    match &request.operation {
        WorkerOperation::Session => unreachable!(),
        WorkerOperation::Extract {
            destination,
            overwrite,
        } => {
            let ownership = live.ownership_ticket(request.app_id)?;
            ensure_identity(live, initial)?;
            require_subscription(live, request.app_id)?;
            let encrypted = live.encrypted_ticket()?;
            ensure_identity(live, initial)?;
            require_subscription(live, request.app_id)?;
            let ticket = AuthorizationTicket::from_native(
                request.app_id,
                initial,
                ownership.bytes.into_vec(),
                encrypted.into_vec(),
                ownership.offsets,
            )?;
            // Final fresh identity/license checks precede any private file write.
            ensure_identity(live, initial)?;
            require_subscription(live, request.app_id)?;
            let exported = export_authorization_ticket(&ticket, destination, *overwrite)?;
            Ok(WorkerResponse {
                exported: Some(exported),
                ..Default::default()
            })
        }
        WorkerOperation::Import {
            source,
            expected_sha256,
            overwrite,
        } => {
            ensure_identity(live, initial)?;
            require_subscription(live, request.app_id)?;
            let context =
                AuthorizationImportContext::from_verified_session(request.app_id, initial, true)?;
            let imported = import_authorization_file_guarded(
                source,
                expected_sha256,
                &context,
                *overwrite,
                || {
                    ensure_identity(live, initial)?;
                    require_subscription(live, request.app_id)
                },
            )?;
            Ok(WorkerResponse {
                imported: Some(imported),
                ..Default::default()
            })
        }
    }
}

#[cfg(all(windows, target_pointer_width = "64"))]
fn execute_native(request: &WorkerRequest) -> Result<WorkerResponse> {
    crate::steam_native::ensure_session_running(&request.steam_root)?;
    let app_context = request.app_id.to_string();
    ensure!(
        std::env::var("SteamAppId").as_deref() == Ok(app_context.as_str())
            && std::env::var("SteamGameId").as_deref() == Ok(app_context.as_str()),
        "Authorization worker application context is inconsistent"
    );
    let mut session = native::Session::connect(&request.steam_root)?;
    execute_with(request, &mut session)
}
#[cfg(not(all(windows, target_pointer_width = "64")))]
fn execute_native(_request: &WorkerRequest) -> Result<WorkerResponse> {
    bail!("Authorization operations require the Windows x64 desktop application")
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
    unsafe fn method<T: Copy>(object: Object, slot: usize) -> Result<T> {
        ensure!(!object.is_null(), "Required Steam interface is unavailable");
        let table = *(object as *const *const Object);
        ensure!(!table.is_null(), "Steam interface table is unavailable");
        let address = *table.add(slot);
        ensure!(
            !address.is_null(),
            "Required Steam interface method is unavailable"
        );
        ensure!(
            std::mem::size_of::<T>() == std::mem::size_of::<Object>(),
            "Steam interface method type has an invalid size"
        );
        Ok(std::mem::transmute_copy(&address))
    }
    pub(super) struct Session {
        _module: Module,
        root: PathBuf,
        client: Object,
        identity_user: Object,
        ticket_user: Object,
        apps: Object,
        utils: Object,
        app_ticket: Object,
        pipe: i32,
        user: i32,
        get_callback: GetCallback,
        free_callback: FreeCallback,
    }
    impl Drop for Session {
        fn drop(&mut self) {
            unsafe {
                if self.user != 0 {
                    if let Ok(call) =
                        method::<unsafe extern "system" fn(Object, i32, i32)>(self.client, 4)
                    {
                        call(self.client, self.pipe, self.user);
                    }
                }
                if self.pipe != 0 {
                    if let Ok(call) =
                        method::<unsafe extern "system" fn(Object, i32) -> bool>(self.client, 1)
                    {
                        call(self.client, self.pipe);
                    }
                }
            }
        }
    }
    impl Session {
        pub(super) fn connect(root: &Path) -> Result<Self> {
            unsafe {
                let path = crate::steam_native::prepare_client_library(root)?;
                let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
                let handle = crate::steam_native::with_suppressed_loader_errors(|| {
                    let handle = LoadLibraryExW(wide.as_ptr(), ptr::null_mut(), 8);
                    ensure!(
                        !handle.is_null(),
                        "Steam native client library could not be loaded"
                    );
                    Ok(handle)
                })?;
                let module = Module(handle);
                let create_ptr = GetProcAddress(handle, c"CreateInterface".as_ptr());
                let callback_ptr = GetProcAddress(handle, c"Steam_BGetCallback".as_ptr());
                let free_ptr = GetProcAddress(handle, c"Steam_FreeLastCallback".as_ptr());
                ensure!(
                    !create_ptr.is_null() && !callback_ptr.is_null() && !free_ptr.is_null(),
                    "Steam IPC exports are unavailable"
                );
                let create: unsafe extern "C" fn(*const c_char, *mut i32) -> Object =
                    std::mem::transmute(create_ptr);
                let client = create(c"SteamClient023".as_ptr(), ptr::null_mut());
                ensure!(!client.is_null(), "SteamClient023 is unavailable");
                let mut session = Self {
                    _module: module,
                    root: root.to_path_buf(),
                    client,
                    identity_user: ptr::null_mut(),
                    ticket_user: ptr::null_mut(),
                    apps: ptr::null_mut(),
                    utils: ptr::null_mut(),
                    app_ticket: ptr::null_mut(),
                    pipe: 0,
                    user: 0,
                    get_callback: std::mem::transmute::<Object, GetCallback>(callback_ptr),
                    free_callback: std::mem::transmute::<Object, FreeCallback>(free_ptr),
                };
                let create_pipe: unsafe extern "system" fn(Object) -> i32 = method(client, 0)?;
                session.pipe = create_pipe(client);
                ensure!(session.pipe != 0, "Steam IPC pipe is unavailable");
                let connect: unsafe extern "system" fn(Object, i32) -> i32 = method(client, 2)?;
                session.user = connect(client, session.pipe);
                ensure!(
                    session.user != 0,
                    "Sign in to the existing Steam account before authorization operations"
                );
                let user: unsafe extern "system" fn(Object, i32, i32, *const c_char) -> Object =
                    method(client, 5)?;
                session.identity_user =
                    user(client, session.user, session.pipe, c"SteamUser012".as_ptr());
                session.ticket_user =
                    user(client, session.user, session.pipe, c"SteamUser023".as_ptr());
                let apps: unsafe extern "system" fn(Object, i32, i32, *const c_char) -> Object =
                    method(client, 15)?;
                session.apps = apps(
                    client,
                    session.user,
                    session.pipe,
                    c"STEAMAPPS_INTERFACE_VERSION008".as_ptr(),
                );
                let utils: unsafe extern "system" fn(Object, i32, *const c_char) -> Object =
                    method(client, 9)?;
                session.utils = utils(client, session.pipe, c"SteamUtils010".as_ptr());
                let generic: unsafe extern "system" fn(Object, i32, i32, *const c_char) -> Object =
                    method(client, 12)?;
                session.app_ticket = generic(
                    client,
                    session.user,
                    session.pipe,
                    c"STEAMAPPTICKET_INTERFACE_VERSION001".as_ptr(),
                );
                ensure!(
                    !session.identity_user.is_null() && !session.apps.is_null(),
                    "Required Steam authorization interfaces are unavailable"
                );
                let logged: unsafe extern "system" fn(Object) -> bool =
                    method(session.identity_user, 1)?;
                let get_id: unsafe extern "system" fn(Object, *mut u64) =
                    method(session.identity_user, 2)?;
                wait_for_steam_user_ready(
                    400,
                    || {
                        let mut id = 0;
                        get_id(session.identity_user, &mut id);
                        Ok(SteamUserReadinessProbe {
                            logged_on: logged(session.identity_user),
                            steam_id: id,
                        })
                    },
                    || session.pump(),
                    || thread::sleep(Duration::from_millis(20)),
                )?;
                crate::steam_native::ensure_session_running(root)?;
                Ok(session)
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
            bail!("Steam callback inspection limit exceeded")
        }
    }
    impl LiveAuthorization for Session {
        fn identity(&mut self) -> Result<u64> {
            unsafe {
                crate::steam_native::ensure_session_running(&self.root)?;
                let logged: unsafe extern "system" fn(Object) -> bool =
                    method(self.identity_user, 1)?;
                if !logged(self.identity_user) {
                    return Err(crate::achievements::steam_session_error_from_code(
                        "steam_session_signed_out",
                        "Sign in to Steam before authorization operations".into(),
                    ));
                }
                let get: unsafe extern "system" fn(Object, *mut u64) =
                    method(self.identity_user, 2)?;
                let mut id = 0;
                get(self.identity_user, &mut id);
                ensure!(
                    valid_identity(id),
                    "Current Steam account identity is unavailable"
                );
                Ok(id)
            }
        }
        fn subscribed(&mut self, app_id: u32) -> Result<bool> {
            unsafe {
                let call: unsafe extern "system" fn(Object, u32) -> bool = method(self.apps, 6)?;
                Ok(call(self.apps, app_id))
            }
        }
        fn ownership_ticket(&mut self, app_id: u32) -> Result<OwnershipTicket> {
            unsafe {
                let current = self.identity()?;
                require_subscription(self, app_id)?;
                type ReadTicket = unsafe extern "system" fn(
                    Object,
                    u32,
                    *mut u8,
                    u32,
                    *mut u32,
                    *mut u32,
                    *mut u32,
                    *mut u32,
                ) -> u32;
                let read: ReadTicket = method(self.app_ticket, 0)?;
                let mut buffer = SensitiveBytes(vec![0; MAX_OWNERSHIP_BYTES]);
                let (mut app_offset, mut account_offset, mut signature_offset, mut signature_size) =
                    (0, 0, 0, 0);
                let written = read(
                    self.app_ticket,
                    app_id,
                    buffer.0.as_mut_ptr(),
                    MAX_OWNERSHIP_BYTES as u32,
                    &mut app_offset,
                    &mut account_offset,
                    &mut signature_offset,
                    &mut signature_size,
                ) as usize;
                ensure!(
                    (20..=MAX_OWNERSHIP_BYTES).contains(&written),
                    "Steam returned no bounded ownership ticket"
                );
                let app_start = app_offset as usize;
                let account_start = account_offset as usize;
                ensure!(
                    app_start.checked_add(4).is_some_and(|end| end <= written)
                        && account_start
                            .checked_add(8)
                            .is_some_and(|end| end <= written),
                    "Steam ownership identity offsets are invalid"
                );
                ensure!(
                    signature_size > 0
                        && (signature_offset as usize)
                            .checked_add(signature_size as usize)
                            .is_some_and(|end| end <= written),
                    "Steam ownership signature range is invalid"
                );
                let actual_app = u32::from_le_bytes(buffer.0[app_start..app_start + 4].try_into()?);
                let actual_account =
                    u64::from_le_bytes(buffer.0[account_start..account_start + 8].try_into()?);
                ensure!(
                    actual_app == app_id,
                    "Steam ownership ticket belongs to another application"
                );
                if actual_account != current {
                    return Err(changed_account());
                }
                buffer.0.truncate(written);
                Ok(OwnershipTicket {
                    bytes: buffer,
                    offsets: TicketOffsets {
                        app_id_offset: app_offset,
                        steam_id_offset: account_offset,
                    },
                })
            }
        }
        fn encrypted_ticket(&mut self) -> Result<SensitiveBytes> {
            unsafe {
                let current = self.identity()?;
                let request: unsafe extern "system" fn(Object, *const u8, i32) -> u64 =
                    method(self.ticket_user, 21)?;
                let handle = request(self.ticket_user, ptr::null(), 0);
                ensure!(
                    handle != 0,
                    "Steam encrypted-ticket request was not accepted"
                );
                let completed: unsafe extern "system" fn(Object, u64, *mut bool) -> bool =
                    method(self.utils, 11)?;
                let deadline = Instant::now() + ENCRYPTED_TIMEOUT;
                let mut failed = false;
                loop {
                    if self.identity()? != current {
                        return Err(changed_account());
                    }
                    if completed(self.utils, handle, &mut failed) {
                        break;
                    }
                    ensure!(
                        Instant::now() < deadline,
                        "Steam encrypted-ticket request timed out"
                    );
                    thread::sleep(Duration::from_millis(50));
                }
                ensure!(!failed, "Steam encrypted-ticket request had an I/O failure");
                let result: unsafe extern "system" fn(
                    Object,
                    u64,
                    *mut u8,
                    i32,
                    i32,
                    *mut bool,
                ) -> bool = method(self.utils, 13)?;
                let mut code = [0u8; 4];
                failed = false;
                ensure!(
                    result(self.utils, handle, code.as_mut_ptr(), 4, 154, &mut failed) && !failed,
                    "Steam encrypted-ticket completion was unavailable"
                );
                match i32::from_le_bytes(code) {
                    1 => {}
                    3 => {
                        return Err(crate::achievements::steam_session_error_from_code(
                            "steam_session_offline",
                            "Steam could not provide an encrypted ticket while offline".into(),
                        ))
                    }
                    29 => bail!("Steam encrypted-ticket request is already pending"),
                    25 => {
                        bail!("Steam encrypted-ticket requests are limited to one per 60 seconds")
                    }
                    _ => bail!("Steam did not approve the encrypted-ticket request"),
                }
                let get: unsafe extern "system" fn(Object, *mut u8, i32, *mut u32) -> bool =
                    method(self.ticket_user, 22)?;
                let mut buffer = SensitiveBytes(vec![0; MAX_ENCRYPTED_BYTES]);
                let mut written = 0;
                ensure!(
                    get(
                        self.ticket_user,
                        buffer.0.as_mut_ptr(),
                        MAX_ENCRYPTED_BYTES as i32,
                        &mut written
                    ) && written > 0
                        && written as usize <= MAX_ENCRYPTED_BYTES,
                    "Steam returned no bounded encrypted ticket"
                );
                buffer.0.truncate(written as usize);
                if self.identity()? != current {
                    return Err(changed_account());
                }
                Ok(buffer)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::Cell, collections::VecDeque};
    const ID: u64 = STEAM_ID_BASE + 1;
    struct FakeSession {
        ids: VecDeque<u64>,
        own: bool,
        ownership_calls: Cell<usize>,
        encrypted_calls: Cell<usize>,
    }
    impl FakeSession {
        fn new(own: bool) -> Self {
            Self {
                ids: VecDeque::from([ID]),
                own,
                ownership_calls: Cell::new(0),
                encrypted_calls: Cell::new(0),
            }
        }
    }
    impl LiveAuthorization for FakeSession {
        fn identity(&mut self) -> Result<u64> {
            let id = *self.ids.front().unwrap_or(&ID);
            if self.ids.len() > 1 {
                self.ids.pop_front();
            }
            Ok(id)
        }
        fn subscribed(&mut self, _app_id: u32) -> Result<bool> {
            Ok(self.own)
        }
        fn ownership_ticket(&mut self, app_id: u32) -> Result<OwnershipTicket> {
            self.ownership_calls.set(self.ownership_calls.get() + 1);
            let mut bytes = vec![0; 36];
            bytes[12..20].copy_from_slice(&ID.to_le_bytes());
            bytes[24..28].copy_from_slice(&app_id.to_le_bytes());
            Ok(OwnershipTicket {
                bytes: SensitiveBytes(bytes),
                offsets: TicketOffsets {
                    app_id_offset: 24,
                    steam_id_offset: 12,
                },
            })
        }
        fn encrypted_ticket(&mut self) -> Result<SensitiveBytes> {
            self.encrypted_calls.set(self.encrypted_calls.get() + 1);
            Ok(SensitiveBytes(vec![1; 16]))
        }
    }
    fn extract_request(root: &Path) -> WorkerRequest {
        WorkerRequest {
            steam_root: root.to_path_buf(),
            app_id: 42,
            expected_steam_id: Some(ID.to_string()),
            operation: WorkerOperation::Extract {
                destination: root.join("tickets.txt"),
                overwrite: false,
            },
        }
    }

    #[test]
    fn session_probe_never_requests_private_tickets() {
        let temp = tempfile::tempdir().unwrap();
        let mut live = FakeSession::new(false);
        let request = WorkerRequest {
            steam_root: temp.path().into(),
            app_id: 42,
            expected_steam_id: None,
            operation: WorkerOperation::Session,
        };
        let result = execute_with(&request, &mut live).unwrap();
        let info = result.session.unwrap();
        assert!(info.connected);
        assert!(!info.subscribed);
        assert_eq!(info.steam_id, ID.to_string());
        assert_eq!(live.ownership_calls.get(), 0);
        assert_eq!(live.encrypted_calls.get(), 0);
        assert!(!temp.path().join("tickets.txt").exists());
    }
    #[test]
    fn unowned_application_never_requests_or_writes_a_ticket() {
        let temp = tempfile::tempdir().unwrap();
        let mut live = FakeSession::new(false);
        assert!(execute_with(&extract_request(temp.path()), &mut live).is_err());
        assert_eq!(live.ownership_calls.get(), 0);
        assert_eq!(live.encrypted_calls.get(), 0);
        assert!(!temp.path().join("tickets.txt").exists());
    }
    #[test]
    fn confirmation_account_mismatch_precedes_ticket_apis() {
        let temp = tempfile::tempdir().unwrap();
        let mut live = FakeSession::new(true);
        let mut request = extract_request(temp.path());
        request.expected_steam_id = Some((ID + 1).to_string());
        let error = execute_with(&request, &mut live)
            .err()
            .expect("Changed account must fail");
        assert_eq!(
            crate::achievements::steam_session_error_code(&error),
            Some("steam_session_identity_changed")
        );
        assert_eq!(live.ownership_calls.get(), 0);
        assert!(!temp.path().join("tickets.txt").exists());
    }
    #[test]
    fn account_change_during_ticket_collection_prevents_private_file_write() {
        let temp = tempfile::tempdir().unwrap();
        let mut live = FakeSession::new(true);
        live.ids = VecDeque::from([ID, ID + 1]);
        assert!(execute_with(&extract_request(temp.path()), &mut live).is_err());
        assert_eq!(live.encrypted_calls.get(), 0);
        assert!(!temp.path().join("tickets.txt").exists());
    }
    #[test]
    fn native_offsets_are_preserved_and_only_metadata_crosses_ipc() {
        let temp = tempfile::tempdir().unwrap();
        let mut live = FakeSession::new(true);
        let response = execute_with(&extract_request(temp.path()), &mut live).unwrap();
        let encoded = serde_json::to_string(&response).unwrap();
        assert!(!encoded.contains("appticket("));
        assert!(!encoded.contains("eticket("));
        let decoded: WorkerResponse = serde_json::from_str(&encoded).unwrap();
        let receipt = decoded.exported.unwrap();
        assert_eq!(receipt.summary.app_id_offset, 24);
        assert_eq!(receipt.summary.steam_id_offset, 12);
        assert_eq!(receipt.summary.steam_id, ID.to_string());
        assert!(!receipt.runtime_authorization_verified);
        assert!(temp.path().join("tickets.txt").is_file());
    }
    #[test]
    fn malformed_or_missing_confirmation_is_rejected_without_worker_creation() {
        let temp = tempfile::tempdir().unwrap();
        let mut request = extract_request(temp.path());
        request.expected_steam_id = None;
        assert!(validate_request(&request).is_err());
        request.expected_steam_id = Some("1".into());
        assert!(validate_request(&request).is_err());
    }
    #[test]
    #[cfg(windows)]
    fn stopped_steam_never_spawns_worker_or_creates_ipc_cache() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("steam.exe"), b"fixture executable").unwrap();
        fs::write(
            temp.path().join("steamclient64.dll"),
            b"invalid DLL fixture",
        )
        .unwrap();
        let cache = temp.path().join("ipc");
        let error =
            authorization_session(&std::env::current_exe().unwrap(), &cache, temp.path(), 42)
                .unwrap_err();
        assert_eq!(
            crate::achievements::steam_session_error_code(&error),
            Some("steam_session_offline")
        );
        assert!(!cache.exists());
    }
    #[test]
    fn import_refuses_unowned_or_changed_account_before_registry_access() {
        let temp = tempfile::tempdir().unwrap();
        let request = WorkerRequest {
            steam_root: temp.path().into(),
            app_id: 42,
            expected_steam_id: Some(ID.to_string()),
            operation: WorkerOperation::Import {
                source: temp.path().join("missing.txt"),
                expected_sha256: "a".repeat(64),
                overwrite: false,
            },
        };
        let mut live = FakeSession::new(false);
        assert!(execute_with(&request, &mut live).is_err());
        assert_eq!(live.ownership_calls.get(), 0);
        assert_eq!(live.encrypted_calls.get(), 0);
        let mut changed = FakeSession::new(true);
        changed.ids = VecDeque::from([ID + 1]);
        assert!(execute_with(&request, &mut changed).is_err());
    }
}
