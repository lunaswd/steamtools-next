//! Shared preflight for live Steam IPC clients. Inspection never loads a DLL,
//! reads accounts, scans a game library, or modifies Steam.
use anyhow::{bail, Context, Result};
use std::{
    fs::{self, File},
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

const MAX_CLIENT_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Debug, Clone)]
pub(crate) struct SteamClientValidationError {
    detail: String,
}
impl SteamClientValidationError {
    fn new(detail: impl Into<String>) -> Self {
        Self {
            detail: detail.into(),
        }
    }
    pub(crate) fn code(&self) -> &'static str {
        "steam_client_invalid"
    }
    pub(crate) fn from_code(code: &str, detail: String) -> Option<Self> {
        (code == "steam_client_invalid").then(|| {
            Self::new(
                detail
                    .strip_prefix("steam_client_invalid: ")
                    .unwrap_or(&detail)
                    .to_owned(),
            )
        })
    }
}
impl std::fmt::Display for SteamClientValidationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code(), self.detail)
    }
}
impl std::error::Error for SteamClientValidationError {}

fn invalid(detail: impl Into<String>) -> anyhow::Error {
    SteamClientValidationError::new(detail).into()
}

fn client_executable(root: &Path) -> Result<PathBuf> {
    let root = crate::kernel_manage::absolute_local_path(root, "Steam directory")?;
    if !fs::symlink_metadata(&root)
        .context("Steam directory is unavailable")?
        .is_dir()
    {
        return Err(invalid("Select the real Steam installation directory"));
    }
    let executable = root.join("steam.exe");
    crate::kernel_manage::reject_link_ancestors(&executable)?;
    if !fs::symlink_metadata(&executable)
        .context("Steam executable is unavailable")?
        .is_file()
    {
        return Err(invalid("Steam executable is not a regular file"));
    }
    fs::canonicalize(executable).context("Could not resolve the Steam executable")
}

fn ensure_running_with(root: &Path, inspect: impl FnOnce(&Path) -> Result<bool>) -> Result<()> {
    let executable = client_executable(root)?;
    if !inspect(&executable)? {
        return Err(crate::achievements::SteamSessionReadinessError::offline(
            "Steam is not running. Start Steam, complete sign-in, and refresh the current session.",
        )
        .into());
    }
    Ok(())
}

/// Fast fail-closed check of the selected installation's main process only.
pub(crate) fn ensure_session_running(root: &Path) -> Result<()> {
    ensure_running_with(root, matching_steam_process)
}

/// Validate only bounded on-disk PE metadata; never ask the Windows loader to
/// inspect malformed DLLs (which otherwise opens a native Bad Image dialog).
pub(crate) fn validate_steam_client_library(path: &Path) -> Result<()> {
    let check = || -> Result<()> {
        let path = crate::kernel_manage::absolute_local_path(path, "Steam client DLL")?;
        let metadata = fs::symlink_metadata(&path)?;
        if !metadata.is_file() || metadata.len() < 64 || metadata.len() > MAX_CLIENT_BYTES {
            bail!("Steam client DLL has an invalid file type or size");
        }
        let mut file = File::open(path)?;
        let mut dos = [0u8; 64];
        file.read_exact(&mut dos)?;
        if &dos[..2] != b"MZ" {
            bail!("Steam client DLL is not a Windows PE image");
        }
        let offset = u32::from_le_bytes(dos[60..64].try_into()?) as u64;
        if !(64..=16 * 1024 * 1024).contains(&offset) || offset + 24 > metadata.len() {
            bail!("Steam client DLL has an invalid PE offset");
        }
        file.seek(SeekFrom::Start(offset))?;
        let mut coff = [0u8; 24];
        file.read_exact(&mut coff)?;
        let machine = u16::from_le_bytes([coff[4], coff[5]]);
        let sections = u16::from_le_bytes([coff[6], coff[7]]) as u64;
        let optional_size = u16::from_le_bytes([coff[20], coff[21]]) as u64;
        let characteristics = u16::from_le_bytes([coff[22], coff[23]]);
        if &coff[..4] != b"PE\0\0"
            || machine != 0x8664
            || characteristics & 0x2002 != 0x2002
            || sections == 0
            || sections > 96
            || optional_size < 112
            || offset + 24 + optional_size + sections * 40 > metadata.len()
        {
            bail!("Steam client requires a complete Windows x64 DLL header and section table");
        }
        let mut magic = [0u8; 2];
        file.read_exact(&mut magic)?;
        if u16::from_le_bytes(magic) != 0x20b {
            bail!("Steam client DLL is not PE32+");
        }
        Ok(())
    };
    check().map_err(|_| invalid("The selected steamclient64.dll is missing or invalid. Select a valid Steam installation; no library was loaded."))
}

fn prepare_client_library_with(
    root: &Path,
    mut inspect: impl FnMut(&Path) -> Result<bool>,
) -> Result<PathBuf> {
    ensure_running_with(root, &mut inspect)?;
    let root = fs::canonicalize(root).context("Could not resolve Steam directory")?;
    let path = [
        root.join("steamclient64.dll"),
        root.join("bin/steamclient64.dll"),
    ]
    .into_iter()
    .find(|path| fs::symlink_metadata(path).is_ok())
    .ok_or_else(|| invalid("Steam client DLL was not found in the selected installation"))?;
    validate_steam_client_library(&path)?;
    // A client may have exited during validation. Check again immediately before
    // returning the load candidate; a late disconnect still has a bounded IPC check.
    ensure_running_with(&root, inspect)?;
    Ok(path)
}

pub(crate) fn prepare_client_library(root: &Path) -> Result<PathBuf> {
    prepare_client_library_with(root, matching_steam_process)
}

#[cfg(windows)]
fn path_identity(path: &Path) -> String {
    fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .trim_start_matches(r"\\?\")
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_ascii_lowercase()
}

#[cfg(windows)]
fn matching_steam_process(executable: &Path) -> Result<bool> {
    use windows_sys::Win32::{
        Foundation::{
            CloseHandle, GetLastError, ERROR_INVALID_PARAMETER, ERROR_NO_MORE_FILES, HANDLE,
            INVALID_HANDLE_VALUE,
        },
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
    struct Handle(HANDLE);
    impl Drop for Handle {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(
                crate::achievements::SteamSessionReadinessError::unavailable(
                    "Steam running state could not be inspected",
                )
                .into(),
            );
        }
        let _snapshot = Handle(snapshot);
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut next = Process32FirstW(snapshot, &mut entry);
        let expected = path_identity(executable);
        let mut unknown = false;
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
                    // A PID that exited during enumeration is not an active client.
                    if GetLastError() != ERROR_INVALID_PARAMETER {
                        unknown = true;
                    }
                } else {
                    let _process = Handle(process);
                    let mut buffer = vec![0u16; 32768];
                    let mut length = buffer.len() as u32;
                    if QueryFullProcessImageNameW(process, 0, buffer.as_mut_ptr(), &mut length) == 0
                    {
                        unknown = true;
                    } else {
                        let actual =
                            PathBuf::from(String::from_utf16_lossy(&buffer[..length as usize]));
                        if path_identity(&actual) == expected {
                            return Ok(true);
                        }
                    }
                }
            }
            next = Process32NextW(snapshot, &mut entry);
        }
        if GetLastError() != ERROR_NO_MORE_FILES || unknown {
            return Err(crate::achievements::SteamSessionReadinessError::unavailable("A Steam process exists but its selected installation could not be verified; refresh the client state").into());
        }
        Ok(false)
    }
}

#[cfg(not(windows))]
fn matching_steam_process(_executable: &Path) -> Result<bool> {
    Err(
        crate::achievements::SteamSessionReadinessError::unavailable(
            "Live Steam sessions require the Windows desktop build",
        )
        .into(),
    )
}

#[cfg(windows)]
mod error_mode {
    use anyhow::{Context, Result};
    use std::{marker::PhantomData, rc::Rc};
    #[link(name = "kernel32")]
    extern "system" {
        fn GetThreadErrorMode() -> u32;
        fn SetThreadErrorMode(mode: u32, previous: *mut u32) -> i32;
    }
    const QUIET_LOADER: u32 = 0x0001 | 0x8000; // SEM_FAILCRITICALERRORS | SEM_NOOPENFILEERRORBOX
    pub(super) struct Guard {
        previous: u32,
        _thread_bound: PhantomData<Rc<()>>,
    }
    impl Guard {
        pub(super) fn enter() -> Result<Self> {
            let mut previous = 0;
            // Thread-local and restored on all return/unwind paths. Never alters
            // another worker's error mode or suppresses application-level errors.
            if unsafe { SetThreadErrorMode(GetThreadErrorMode() | QUIET_LOADER, &mut previous) }
                == 0
            {
                return Err(std::io::Error::last_os_error())
                    .context("Could not suppress native loader error dialogs");
            }
            Ok(Self {
                previous,
                _thread_bound: PhantomData,
            })
        }
    }
    impl Drop for Guard {
        fn drop(&mut self) {
            unsafe {
                SetThreadErrorMode(self.previous, std::ptr::null_mut());
            }
        }
    }
    #[cfg(test)]
    pub(super) fn current() -> u32 {
        unsafe { GetThreadErrorMode() }
    }
}

pub(crate) fn with_suppressed_loader_errors<T>(operation: impl FnOnce() -> Result<T>) -> Result<T> {
    #[cfg(windows)]
    let _mode = error_mode::Guard::enter()?;
    operation()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("steam.exe"), b"fixture process name only").unwrap();
        dir
    }
    fn pe() -> Vec<u8> {
        let mut bytes = vec![0u8; 512];
        bytes[..2].copy_from_slice(b"MZ");
        bytes[60..64].copy_from_slice(&128u32.to_le_bytes());
        bytes[128..132].copy_from_slice(b"PE\0\0");
        bytes[132..134].copy_from_slice(&0x8664u16.to_le_bytes());
        bytes[134..136].copy_from_slice(&1u16.to_le_bytes());
        bytes[148..150].copy_from_slice(&112u16.to_le_bytes());
        bytes[150..152].copy_from_slice(&0x2002u16.to_le_bytes());
        bytes[152..154].copy_from_slice(&0x20bu16.to_le_bytes());
        bytes
    }
    #[test]
    fn offline_check_stops_before_inspecting_a_poisoned_dll() {
        let fixture = fixture();
        fs::write(fixture.path().join("steamclient64.dll"), b"invalid DLL").unwrap();
        let error = prepare_client_library_with(fixture.path(), |_| Ok(false)).unwrap_err();
        assert_eq!(
            crate::steam_session_error_code(&error),
            Some("steam_session_offline")
        );
    }
    #[test]
    fn invalid_library_is_rejected_without_calling_the_loader() {
        let fixture = fixture();
        fs::write(fixture.path().join("steamclient64.dll"), b"invalid DLL").unwrap();
        let loader_calls = Cell::new(0);
        let result = prepare_client_library_with(fixture.path(), |_| Ok(true)).map(|_| {
            loader_calls.set(loader_calls.get() + 1);
        });
        assert_eq!(
            crate::steam_session_error_code(&result.unwrap_err()),
            Some("steam_client_invalid")
        );
        assert_eq!(loader_calls.get(), 0);
    }
    #[test]
    fn only_complete_x64_pe32plus_dll_headers_are_accepted() {
        let fixture = fixture();
        let file = fixture.path().join("steamclient64.dll");
        fs::write(&file, pe()).unwrap();
        validate_steam_client_library(&file).unwrap();
        for change in 0..5 {
            let mut bytes = pe();
            match change {
                0 => bytes[132..134].copy_from_slice(&0x14cu16.to_le_bytes()),
                1 => bytes[150..152].copy_from_slice(&2u16.to_le_bytes()),
                2 => bytes[152..154].copy_from_slice(&0x10bu16.to_le_bytes()),
                3 => bytes[148..150].copy_from_slice(&4096u16.to_le_bytes()),
                _ => bytes.truncate(160),
            }
            fs::write(&file, bytes).unwrap();
            assert!(validate_steam_client_library(&file).is_err());
        }
    }
    #[test]
    fn client_exiting_during_validation_stops_before_loading() {
        let fixture = fixture();
        fs::write(fixture.path().join("steamclient64.dll"), pe()).unwrap();
        let count = Cell::new(0);
        let error = prepare_client_library_with(fixture.path(), |_| {
            count.set(count.get() + 1);
            Ok(count.get() == 1)
        })
        .unwrap_err();
        assert_eq!(
            crate::steam_session_error_code(&error),
            Some("steam_session_offline")
        );
        assert_eq!(count.get(), 2);
    }
    #[test]
    fn unverified_process_state_is_not_assumed_running() {
        let fixture = fixture();
        let error = ensure_running_with(fixture.path(), |_| {
            Err(
                crate::achievements::SteamSessionReadinessError::unavailable(
                    "fixture process access denied",
                )
                .into(),
            )
        })
        .unwrap_err();
        assert_eq!(
            crate::steam_session_error_code(&error),
            Some("steam_session_startup_timeout")
        );
    }
    #[cfg(windows)]
    #[test]
    fn loader_error_mode_is_thread_scoped_and_restored_after_failure_and_unwind() {
        let before = error_mode::current();
        let error = with_suppressed_loader_errors::<()>(|| {
            assert_eq!(error_mode::current() & 0x8001, 0x8001);
            bail!("fixture loader failure")
        })
        .unwrap_err();
        assert_eq!(error.to_string(), "fixture loader failure");
        assert_eq!(error_mode::current(), before);
        let result = std::panic::catch_unwind(|| {
            let _ = with_suppressed_loader_errors::<()>(|| {
                panic!("fixture unwind");
            });
        });
        assert!(result.is_err());
        assert_eq!(error_mode::current(), before);
    }
}
