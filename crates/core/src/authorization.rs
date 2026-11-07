//! Private local Steam ownership/encrypted-ticket file management.
//!
//! Format/identity checks are not signature validation and never prove that
//! Steam, a publisher, or a game accepts a ticket. The caller's live account and
//! subscription context must come from the native worker, not from frontend JSON.
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_TICKET_BYTES: usize = 64 * 1024;
const STEAM_ID_BASE: u64 = 76_561_197_960_265_728;
const APP_VALUE: &str = "AppTicket";
const ENCRYPTED_VALUE: &str = "ETicket";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TicketOffsets {
    pub app_id_offset: u32,
    pub steam_id_offset: u32,
}

/// Opaque private bytes: intentionally no Serialize, Clone, or raw Debug.
pub struct AuthorizationTicket {
    app_id: u32,
    steam_id: u64,
    app_ticket: Vec<u8>,
    e_ticket: Vec<u8>,
    offsets: TicketOffsets,
}
impl std::fmt::Debug for AuthorizationTicket {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AuthorizationTicket")
            .field("app_ticket_bytes", &self.app_ticket.len())
            .field("e_ticket_bytes", &self.e_ticket.len())
            .field("content", &"<REDACTED>")
            .finish()
    }
}
impl Drop for AuthorizationTicket {
    fn drop(&mut self) {
        self.app_ticket.fill(0);
        self.e_ticket.fill(0);
    }
}
impl AuthorizationTicket {
    pub fn from_native(
        app_id: u32,
        steam_id: u64,
        app_ticket: Vec<u8>,
        e_ticket: Vec<u8>,
        offsets: TicketOffsets,
    ) -> Result<Self> {
        let ticket = Self {
            app_id,
            steam_id,
            app_ticket,
            e_ticket,
            offsets,
        };
        ticket.validate()?;
        Ok(ticket)
    }
    fn validate(&self) -> Result<()> {
        ensure!(self.app_id > 0, "Authorization AppID must be positive");
        ensure!(
            valid_steam_id(self.steam_id),
            "Authorization account identifier is invalid"
        );
        ensure!(
            (20..=MAX_TICKET_BYTES).contains(&self.app_ticket.len()),
            "Ownership ticket size is invalid"
        );
        ensure!(
            !self.e_ticket.is_empty() && self.e_ticket.len() <= MAX_TICKET_BYTES,
            "Encrypted ticket size is invalid"
        );
        let app_start = self.offsets.app_id_offset as usize;
        let account_start = self.offsets.steam_id_offset as usize;
        let app_end = app_start
            .checked_add(4)
            .context("Authorization AppID offset overflow")?;
        let account_end = account_start
            .checked_add(8)
            .context("Authorization account offset overflow")?;
        ensure!(
            app_end <= self.app_ticket.len() && account_end <= self.app_ticket.len(),
            "Authorization metadata offsets are outside the ticket"
        );
        ensure!(
            app_end <= account_start || account_end <= app_start,
            "Authorization identity fields overlap"
        );
        let embedded_app = u32::from_le_bytes(self.app_ticket[app_start..app_end].try_into()?);
        let embedded_account =
            u64::from_le_bytes(self.app_ticket[account_start..account_end].try_into()?);
        ensure!(
            embedded_app == self.app_id,
            "Authorization header and ownership-ticket AppID differ"
        );
        ensure!(
            embedded_account == self.steam_id,
            "Authorization header and ownership-ticket account differ"
        );
        Ok(())
    }
    fn encoded(&self) -> Vec<u8> {
        let mut text = format!(
            "# steamtools-next-ticket-v1\n# appid-offset:{}\n# steamid-offset:{}\nappid:{}\nappticket({} bytes):",
            self.offsets.app_id_offset, self.offsets.steam_id_offset, self.app_id, self.app_ticket.len());
        append_hex(&mut text, &self.app_ticket);
        text.push_str(&format!("\neticket({} bytes):", self.e_ticket.len()));
        append_hex(&mut text, &self.e_ticket);
        text.push_str("\n# Private authorization material. Do not upload or share.\n");
        text.into_bytes()
    }
    fn summary(&self, source_path: Option<String>, sha256: String) -> AuthorizationTicketSummary {
        AuthorizationTicketSummary {
            app_id: self.app_id,
            steam_id: self.steam_id.to_string(),
            app_ticket_bytes: self.app_ticket.len() as u64,
            e_ticket_bytes: self.e_ticket.len() as u64,
            sha256,
            source_path,
            app_id_offset: self.offsets.app_id_offset,
            steam_id_offset: self.offsets.steam_id_offset,
            validation_scope: "fileStructureAndDeclaredIdentity".into(),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthorizationTicketSummary {
    pub app_id: u32,
    pub steam_id: String,
    pub app_ticket_bytes: u64,
    pub e_ticket_bytes: u64,
    pub sha256: String,
    pub source_path: Option<String>,
    pub app_id_offset: u32,
    pub steam_id_offset: u32,
    pub validation_scope: String,
}
impl std::fmt::Debug for AuthorizationTicketSummary {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AuthorizationTicketSummary")
            .field("app_ticket_bytes", &self.app_ticket_bytes)
            .field("e_ticket_bytes", &self.e_ticket_bytes)
            .field("validation_scope", &self.validation_scope)
            .field("identity_and_location", &"<REDACTED>")
            .finish()
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthorizationExportReceipt {
    pub summary: AuthorizationTicketSummary,
    pub output_path: String,
    pub replaced: bool,
    pub runtime_authorization_verified: bool,
}
impl std::fmt::Debug for AuthorizationExportReceipt {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AuthorizationExportReceipt")
            .field("summary", &self.summary)
            .field("replaced", &self.replaced)
            .field(
                "runtime_authorization_verified",
                &self.runtime_authorization_verified,
            )
            .finish()
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthorizationImportReceipt {
    pub summary: AuthorizationTicketSummary,
    pub registry_path: String,
    pub replaced: bool,
    pub write_verified: bool,
    pub runtime_authorization_verified: bool,
}
impl std::fmt::Debug for AuthorizationImportReceipt {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AuthorizationImportReceipt")
            .field("summary", &self.summary)
            .field("replaced", &self.replaced)
            .field("write_verified", &self.write_verified)
            .field(
                "runtime_authorization_verified",
                &self.runtime_authorization_verified,
            )
            .finish()
    }
}

/// Opaque context constructed only by this crate's live native session worker.
pub struct AuthorizationImportContext {
    app_id: u32,
    steam_id: u64,
}
impl AuthorizationImportContext {
    pub(crate) fn from_verified_session(
        app_id: u32,
        steam_id: u64,
        subscribed: bool,
    ) -> Result<Self> {
        ensure!(
            app_id > 0 && valid_steam_id(steam_id),
            "Verified authorization context is invalid"
        );
        ensure!(
            subscribed,
            "The current Steam account does not own the selected application"
        );
        Ok(Self { app_id, steam_id })
    }
}

fn valid_steam_id(value: u64) -> bool {
    value
        .checked_sub(STEAM_ID_BASE)
        .is_some_and(|account| account > 0 && account <= u64::from(u32::MAX))
}
fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn valid_hash(hash: &str) -> bool {
    hash.len() == 64
        && hash
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}
fn append_hex(text: &mut String, bytes: &[u8]) {
    const HEX: &[u8] = b"0123456789abcdef";
    for byte in bytes {
        text.push(HEX[(byte >> 4) as usize] as char);
        text.push(HEX[(byte & 15) as usize] as char);
    }
}
fn decode_hex(text: &str) -> Result<Vec<u8>> {
    ensure!(
        !text.is_empty() && text.len().is_multiple_of(2) && text.len() <= MAX_TICKET_BYTES * 2,
        "Authorization hex length is invalid"
    );
    ensure!(
        text.bytes().all(|b| b.is_ascii_hexdigit()),
        "Authorization ticket contains invalid hex"
    );
    let value = |b: u8| -> u8 {
        if b.is_ascii_digit() {
            b - b'0'
        } else {
            b.to_ascii_lowercase() - b'a' + 10
        }
    };
    Ok(text
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| value(pair[0]) * 16 + value(pair[1]))
        .collect())
}
fn numeric(text: &str) -> Result<u32> {
    ensure!(
        !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()),
        "Authorization numeric metadata is invalid"
    );
    text.parse()
        .context("Authorization numeric metadata is out of range")
}

/// Parse the bounded plaintext legacy format and the explicit-offset v1 format.
/// Unknown comments are ignored; identity offsets never use heuristic scanning.
pub fn parse_authorization_ticket(bytes: &[u8]) -> Result<AuthorizationTicket> {
    ensure!(
        !bytes.is_empty() && bytes.len() as u64 <= MAX_FILE_BYTES,
        "Authorization file exceeds its size limit or is empty"
    );
    let text = std::str::from_utf8(bytes)
        .context("Authorization file must be UTF-8 text")?
        .trim_start_matches('\u{feff}');
    let mut app_id = None;
    let mut app_ticket = None;
    let mut e_ticket = None;
    let mut version = false;
    let mut app_offset = None;
    let mut account_offset = None;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(comment) = line.strip_prefix('#') {
            let comment = comment.trim();
            if comment.eq_ignore_ascii_case("steamtools-next-ticket-v1") {
                ensure!(!version, "Authorization format marker is duplicated");
                version = true;
            } else if comment
                .to_ascii_lowercase()
                .starts_with("steamtools-next-ticket-")
            {
                bail!("Authorization format version is unsupported");
            } else if let Some((key, value)) = comment.split_once(':') {
                match key.trim().to_ascii_lowercase().as_str() {
                    "appid-offset" => {
                        ensure!(
                            app_offset.is_none(),
                            "Authorization AppID offset is duplicated"
                        );
                        app_offset = Some(numeric(value.trim())?);
                    }
                    "steamid-offset" => {
                        ensure!(
                            account_offset.is_none(),
                            "Authorization account offset is duplicated"
                        );
                        account_offset = Some(numeric(value.trim())?);
                    }
                    _ => {}
                }
            }
            continue;
        }
        let (label, value) = line
            .split_once(':')
            .context("Authorization file has an unsupported line")?;
        let label = label.trim().to_ascii_lowercase();
        let value = value.trim();
        if label == "appid" {
            ensure!(app_id.is_none(), "Authorization AppID is duplicated");
            app_id = Some(numeric(value)?);
            continue;
        }
        let open = label
            .find('(')
            .context("Authorization ticket byte count is missing")?;
        let close = label
            .rfind(')')
            .context("Authorization ticket byte count is missing")?;
        ensure!(
            close == label.len() - 1 && open < close,
            "Authorization ticket label is invalid"
        );
        let name = label[..open].trim();
        let declared = label[open + 1..close].trim();
        let declared = declared
            .strip_suffix("bytes")
            .context("Authorization declared size must use bytes")?
            .trim();
        let declared = numeric(declared)? as usize;
        let data = decode_hex(value)?;
        ensure!(
            data.len() == declared,
            "Authorization declared and decoded byte counts differ"
        );
        match name {
            "appticket" => {
                ensure!(app_ticket.is_none(), "Ownership ticket is duplicated");
                app_ticket = Some(data);
            }
            "eticket" => {
                ensure!(e_ticket.is_none(), "Encrypted ticket is duplicated");
                e_ticket = Some(data);
            }
            _ => bail!("Authorization file contains an unsupported ticket field"),
        }
    }
    ensure!(
        version || (app_offset.is_none() && account_offset.is_none()),
        "Explicit authorization offsets require the version marker"
    );
    let offsets = if version {
        TicketOffsets {
            app_id_offset: app_offset.context("Authorization AppID offset is missing")?,
            steam_id_offset: account_offset.context("Authorization account offset is missing")?,
        }
    } else {
        TicketOffsets {
            app_id_offset: 16,
            steam_id_offset: 8,
        }
    };
    let app_id = app_id.context("Authorization AppID is missing")?;
    let app_ticket = app_ticket.context("Ownership ticket is missing")?;
    let e_ticket = e_ticket.context("Encrypted ticket is missing")?;
    let start = offsets.steam_id_offset as usize;
    let end = start
        .checked_add(8)
        .context("Authorization account offset overflow")?;
    ensure!(
        end <= app_ticket.len(),
        "Authorization account offset is outside the ticket"
    );
    let steam_id = u64::from_le_bytes(app_ticket[start..end].try_into()?);
    AuthorizationTicket::from_native(app_id, steam_id, app_ticket, e_ticket, offsets)
}

fn ticket_path(path: &Path) -> Result<PathBuf> {
    let path = crate::kernel_manage::absolute_local_path(path, "authorization file")?;
    ensure!(
        path.extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("txt")),
        "Authorization file must have a .txt extension"
    );
    ensure!(
        path.file_name().is_some(),
        "Authorization file name is missing"
    );
    Ok(path)
}
fn read_file(path: &Path) -> Result<Vec<u8>> {
    crate::kernel_manage::reject_link_ancestors(path)?;
    let meta = fs::symlink_metadata(path).context("Authorization file is unavailable")?;
    ensure!(
        meta.is_file() && meta.len() <= MAX_FILE_BYTES,
        "Authorization source must be a bounded regular file"
    );
    let mut bytes = Vec::new();
    File::open(path)?
        .take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_FILE_BYTES,
        "Authorization source exceeds its size limit"
    );
    Ok(bytes)
}
fn load_ticket(path: &Path) -> Result<(AuthorizationTicket, AuthorizationTicketSummary)> {
    let path = ticket_path(path)?;
    let mut bytes = read_file(&path)?;
    let result = parse_authorization_ticket(&bytes).map(|ticket| {
        let summary = ticket.summary(Some(path.to_string_lossy().into_owned()), digest(&bytes));
        (ticket, summary)
    });
    bytes.fill(0);
    result
}
pub fn inspect_authorization_file(path: &Path) -> Result<AuthorizationTicketSummary> {
    load_ticket(path).map(|(_, summary)| summary)
}

/// Export only to an explicitly selected local text file. Existing files are
/// replaced only when overwrite is true and their captured hash still matches.
pub fn export_authorization_ticket(
    ticket: &AuthorizationTicket,
    destination: &Path,
    overwrite: bool,
) -> Result<AuthorizationExportReceipt> {
    ticket.validate()?;
    let destination = ticket_path(destination)?;
    let previous = match fs::symlink_metadata(&destination) {
        Ok(_) => {
            ensure!(
                overwrite,
                "Authorization destination exists; confirm replacement"
            );
            Some(digest(&read_file(&destination)?))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error).context("Could not inspect authorization destination"),
    };
    let parent = destination
        .parent()
        .context("Authorization destination has no parent")?;
    fs::create_dir_all(parent)?;
    crate::kernel_manage::reject_link_ancestors(parent)?;
    let temporary = parent.join(format!(
        ".stn-ticket-{}-{}.tmp",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    ));
    let mut bytes = ticket.encoded();
    let sha256 = digest(&bytes);
    let outcome = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        match &previous {
            Some(hash) => ensure!(
                digest(&read_file(&destination)?) == *hash,
                "Authorization destination changed before replacement"
            ),
            None => ensure!(
                !destination.exists(),
                "Authorization destination appeared before commit"
            ),
        }
        crate::kernel_manage::reject_link_ancestors(&destination)?;
        replace_file(&temporary, &destination, previous.is_some())?;
        ensure!(
            digest(&read_file(&destination)?) == sha256,
            "Authorization export readback failed"
        );
        Ok(())
    })();
    bytes.fill(0);
    if outcome.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    outcome?;
    Ok(AuthorizationExportReceipt {
        summary: ticket.summary(Some(destination.to_string_lossy().into_owned()), sha256),
        output_path: destination.to_string_lossy().into_owned(),
        replaced: previous.is_some(),
        runtime_authorization_verified: false,
    })
}
#[cfg(windows)]
fn replace_file(from: &Path, to: &Path, replace: bool) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };
    let source: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
    let target: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
    let flags = MOVEFILE_WRITE_THROUGH
        | if replace {
            MOVEFILE_REPLACE_EXISTING
        } else {
            0
        };
    if unsafe { MoveFileExW(source.as_ptr(), target.as_ptr(), flags) } == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}
#[cfg(not(windows))]
fn replace_file(from: &Path, to: &Path, replace: bool) -> Result<()> {
    if !replace {
        fs::hard_link(from, to)?;
        fs::remove_file(from)?;
    } else {
        fs::rename(from, to)?;
    }
    Ok(())
}

struct PrivateSnapshot(Option<Vec<u8>>);
impl Drop for PrivateSnapshot {
    fn drop(&mut self) {
        if let Some(bytes) = &mut self.0 {
            bytes.fill(0)
        }
    }
}
trait TicketStore {
    fn read(&mut self, name: &str) -> Result<Option<Vec<u8>>>;
    fn write(&mut self, name: &str, bytes: &[u8]) -> Result<()>;
    fn delete(&mut self, name: &str) -> Result<()>;
    fn check_account(&mut self, steam_id: u64) -> Result<()>;
}
#[cfg(test)]
fn import_with_store(
    ticket: &AuthorizationTicket,
    context: &AuthorizationImportContext,
    overwrite: bool,
    store: &mut impl TicketStore,
) -> Result<bool> {
    import_with_store_guarded(ticket, context, overwrite, store, &mut || Ok(()))
}
fn import_with_store_guarded(
    ticket: &AuthorizationTicket,
    context: &AuthorizationImportContext,
    overwrite: bool,
    store: &mut impl TicketStore,
    verify_current: &mut impl FnMut() -> Result<()>,
) -> Result<bool> {
    ticket.validate()?;
    ensure!(
        ticket.app_id == context.app_id,
        "Authorization file does not match the selected application"
    );
    ensure!(
        ticket.steam_id == context.steam_id,
        "Authorization file does not belong to the current Steam account"
    );
    verify_current()?;
    store.check_account(context.steam_id)?;
    let previous = [
        PrivateSnapshot(store.read(APP_VALUE)?),
        PrivateSnapshot(store.read(ENCRYPTED_VALUE)?),
    ];
    let names = [APP_VALUE, ENCRYPTED_VALUE];
    let new = [ticket.app_ticket.as_slice(), ticket.e_ticket.as_slice()];
    let replaced = previous.iter().any(|value| value.0.is_some());
    ensure!(
        overwrite || !replaced,
        "Stored authorization exists; confirm replacement"
    );
    let outcome = (|| -> Result<()> {
        for (index, name) in names.iter().enumerate() {
            ensure!(
                store.read(name)? == previous[index].0,
                "Stored authorization changed before import"
            );
        }
        verify_current()?;
        for (index, name) in names.iter().enumerate() {
            store.write(name, new[index])?;
        }
        for (index, name) in names.iter().enumerate() {
            ensure!(
                store.read(name)?.as_deref() == Some(new[index]),
                "Stored authorization failed readback verification"
            );
        }
        store.check_account(context.steam_id)?;
        verify_current()?;
        Ok(())
    })();
    if let Err(error) = outcome {
        let mut conflict = false;
        for (index, name) in names.iter().enumerate() {
            let current = match store.read(name) {
                Ok(current) => current,
                Err(_) => {
                    conflict = true;
                    continue;
                }
            };
            if current == previous[index].0 {
                continue;
            }
            if current.as_deref() != Some(new[index]) {
                conflict = true;
                continue;
            }
            let restored = match &previous[index].0 {
                Some(bytes) => store.write(name, bytes),
                None => store.delete(name),
            };
            if restored.is_err()
                || !store
                    .read(name)
                    .is_ok_and(|value| value == previous[index].0)
            {
                conflict = true;
            }
        }
        if conflict {
            bail!("Authorization import failed; external changes were preserved and automatic restoration needs review");
        }
        return Err(error).context("Authorization import failed; previous values were restored");
    }
    Ok(replaced)
}

struct AuthorizationLock(File);
impl AuthorizationLock {
    fn acquire(app_id: u32) -> Result<Self> {
        let root = crate::settings::app_data_dir()?.join("locks");
        crate::kernel_manage::reject_link_ancestors(&root)?;
        fs::create_dir_all(&root)?;
        let path = root.join(format!("authorization-{app_id}.lock"));
        crate::kernel_manage::reject_link_ancestors(&path)?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;
        file.try_lock()
            .context("Another authorization operation is in progress")?;
        Ok(Self(file))
    }
}
impl Drop for AuthorizationLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

/// The opaque context is made by a verified native session. No context fields,
/// raw bytes, passwords or replacement SteamID are accepted from frontend JSON.
pub fn import_authorization_file(
    source: &Path,
    expected_sha256: &str,
    context: &AuthorizationImportContext,
    overwrite: bool,
) -> Result<AuthorizationImportReceipt> {
    import_authorization_file_guarded(source, expected_sha256, context, overwrite, || Ok(()))
}

/// The native worker supplies a live identity/license callback. It is checked
/// after source inspection and snapshots, immediately before writes and after
/// readback. A late account change triggers the same compensating rollback.
pub(crate) fn import_authorization_file_guarded(
    source: &Path,
    expected_sha256: &str,
    context: &AuthorizationImportContext,
    overwrite: bool,
    mut verify_current: impl FnMut() -> Result<()>,
) -> Result<AuthorizationImportReceipt> {
    ensure!(
        valid_hash(expected_sha256),
        "Expected authorization source hash is invalid"
    );
    let (ticket, summary) = load_ticket(source)?;
    ensure!(
        summary.sha256 == expected_sha256,
        "Authorization file changed after preview"
    );
    let _lock = AuthorizationLock::acquire(context.app_id)?;
    #[cfg(windows)]
    let replaced = import_with_store_guarded(
        &ticket,
        context,
        overwrite,
        &mut RegistryTicketStore::new(context.app_id),
        &mut verify_current,
    )?;
    #[cfg(not(windows))]
    let replaced = {
        let _ = (&ticket, context, overwrite);
        bail!("Authorization registry import requires the Windows desktop build")
    };
    Ok(AuthorizationImportReceipt {
        summary,
        registry_path: format!(r"HKCU\Software\Valve\Steam\Apps\{}", context.app_id),
        replaced,
        write_verified: true,
        runtime_authorization_verified: false,
    })
}

#[cfg(windows)]
struct RegistryTicketStore {
    app_id: u32,
}
#[cfg(windows)]
impl RegistryTicketStore {
    fn new(app_id: u32) -> Self {
        Self { app_id }
    }
    fn subkey(&self) -> String {
        format!(r"Software\Valve\Steam\Apps\{}", self.app_id)
    }
    fn open(&self) -> Result<Option<winreg::RegKey>> {
        use winreg::{
            enums::{HKEY_CURRENT_USER, KEY_READ, KEY_WOW64_64KEY, KEY_WRITE},
            RegKey,
        };
        match RegKey::predef(HKEY_CURRENT_USER)
            .open_subkey_with_flags(self.subkey(), KEY_READ | KEY_WRITE | KEY_WOW64_64KEY)
        {
            Ok(key) => Ok(Some(key)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).context("Authorization registry key is inaccessible"),
        }
    }
}
#[cfg(windows)]
impl TicketStore for RegistryTicketStore {
    fn read(&mut self, name: &str) -> Result<Option<Vec<u8>>> {
        ensure!(
            [APP_VALUE, ENCRYPTED_VALUE].contains(&name),
            "Unsupported authorization registry value"
        );
        let Some(key) = self.open()? else {
            return Ok(None);
        };
        match key.get_raw_value(name) {
            Ok(value) => {
                ensure!(
                    value.vtype == winreg::enums::REG_BINARY,
                    "Existing authorization registry value is not binary; original value preserved"
                );
                ensure!(
                    value.bytes.len() <= MAX_TICKET_BYTES,
                    "Existing authorization registry value exceeds its size limit"
                );
                Ok(Some(value.bytes))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).context("Could not inspect authorization registry value"),
        }
    }
    fn write(&mut self, name: &str, bytes: &[u8]) -> Result<()> {
        use winreg::{
            enums::{HKEY_CURRENT_USER, KEY_READ, KEY_WOW64_64KEY, KEY_WRITE, REG_BINARY},
            RegKey, RegValue,
        };
        ensure!(
            [APP_VALUE, ENCRYPTED_VALUE].contains(&name) && bytes.len() <= MAX_TICKET_BYTES,
            "Unsupported authorization registry write"
        );
        let (key, _) = RegKey::predef(HKEY_CURRENT_USER)
            .create_subkey_with_flags(self.subkey(), KEY_READ | KEY_WRITE | KEY_WOW64_64KEY)?;
        key.set_raw_value(
            name,
            &RegValue {
                vtype: REG_BINARY,
                bytes: bytes.to_vec(),
            },
        )
        .context("Could not write authorization registry value")
    }
    fn delete(&mut self, name: &str) -> Result<()> {
        ensure!(
            [APP_VALUE, ENCRYPTED_VALUE].contains(&name),
            "Unsupported authorization registry deletion"
        );
        if let Some(key) = self.open()? {
            match key.delete_value(name) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error)
                        .context("Could not restore missing authorization registry value")
                }
            }
        }
        Ok(())
    }
    fn check_account(&mut self, steam_id: u64) -> Result<()> {
        let Some(key) = self.open()? else {
            return Ok(());
        };
        match key.get_raw_value("SteamID") {
            Ok(value) => {
                ensure!(
                    value.vtype == winreg::enums::REG_SZ,
                    "Stored authorization account has an unsupported registry type"
                );
                ensure!(
                    value.bytes.len() <= 128,
                    "Stored authorization account value exceeds its size limit"
                );
                let text: String = key
                    .get_value("SteamID")
                    .context("Stored authorization account is invalid")?;
                let stored = text
                    .parse::<u64>()
                    .context("Stored authorization account is invalid")?;
                ensure!(
                    stored == steam_id,
                    "Stored authorization belongs to another Steam account; no values were changed"
                );
                Ok(())
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).context("Could not inspect authorization account binding"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    fn fixture() -> AuthorizationTicket {
        let mut app = vec![0x31; 32];
        app[8..16].copy_from_slice(&(STEAM_ID_BASE + 1).to_le_bytes());
        app[16..20].copy_from_slice(&42u32.to_le_bytes());
        AuthorizationTicket::from_native(
            42,
            STEAM_ID_BASE + 1,
            app,
            vec![0x11, 0x22, 0x33],
            TicketOffsets {
                app_id_offset: 16,
                steam_id_offset: 8,
            },
        )
        .unwrap()
    }
    fn context() -> AuthorizationImportContext {
        AuthorizationImportContext::from_verified_session(42, STEAM_ID_BASE + 1, true).unwrap()
    }
    #[derive(Default)]
    struct MockStore {
        values: BTreeMap<String, Vec<u8>>,
        fail_second: bool,
        external: bool,
        fail_read: bool,
        other_account: bool,
        writes: usize,
    }
    impl TicketStore for MockStore {
        fn read(&mut self, name: &str) -> Result<Option<Vec<u8>>> {
            ensure!(!self.fail_read, "fixture unsupported registry type");
            Ok(self.values.get(name).cloned())
        }
        fn write(&mut self, name: &str, bytes: &[u8]) -> Result<()> {
            self.writes += 1;
            if name == ENCRYPTED_VALUE && self.fail_second {
                self.fail_second = false;
                if self.external {
                    self.values.insert(APP_VALUE.into(), vec![0x99]);
                }
                bail!("fixture write failure")
            }
            self.values.insert(name.into(), bytes.to_vec());
            Ok(())
        }
        fn delete(&mut self, name: &str) -> Result<()> {
            self.values.remove(name);
            Ok(())
        }
        fn check_account(&mut self, _steam_id: u64) -> Result<()> {
            ensure!(!self.other_account, "fixture account binding mismatch");
            Ok(())
        }
    }
    #[test]
    fn explicit_and_legacy_formats_are_bounded_and_identity_checked() {
        let ticket = fixture();
        let encoded = ticket.encoded();
        let parsed = parse_authorization_ticket(&encoded).unwrap();
        assert_eq!(parsed.app_id, 42);
        assert_eq!(parsed.steam_id, STEAM_ID_BASE + 1);
        let legacy = std::str::from_utf8(&encoded)
            .unwrap()
            .lines()
            .filter(|line| !line.starts_with('#'))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(parse_authorization_ticket(legacy.as_bytes()).is_ok());
        assert!(!format!("{ticket:?}").contains("313131"));
    }
    #[test]
    fn parser_rejects_duplicate_null_bad_hex_lengths_and_identity() {
        let bytes = fixture().encoded();
        let text = String::from_utf8(bytes).unwrap();
        for bad in [
            format!("{text}\nappid:42"),
            text.replace("appid:42", "appid:43"),
            text.replace("eticket(3 bytes):112233", "eticket:null"),
            text.replace("eticket(3 bytes):112233", "eticket(4 bytes):112233"),
            text.replace("eticket(3 bytes):112233", "eticket(3 bytes):1122xz"),
            text.replace("eticket(3 bytes):112233", "eticket(3 bytes):11223"),
        ] {
            assert!(parse_authorization_ticket(bad.as_bytes()).is_err())
        }
        assert!(parse_authorization_ticket(&vec![b' '; MAX_FILE_BYTES as usize + 1]).is_err());
    }
    #[test]
    fn explicit_offsets_need_version_both_fields_and_do_not_scan_for_appid() {
        let text = String::from_utf8(fixture().encoded()).unwrap();
        assert!(parse_authorization_ticket(
            text.replace("# steamtools-next-ticket-v1\n", "").as_bytes()
        )
        .is_err());
        assert!(
            parse_authorization_ticket(text.replace("# steamid-offset:8\n", "").as_bytes())
                .is_err()
        );
        assert!(parse_authorization_ticket(
            text.replace("# appid-offset:16", "# appid-offset:28")
                .as_bytes()
        )
        .is_err());
        assert!(parse_authorization_ticket(
            text.replace("steamtools-next-ticket-v1", "steamtools-next-ticket-v2")
                .as_bytes()
        )
        .is_err());
    }
    #[test]
    fn native_offsets_are_verified_not_assumed_canonical() {
        let mut app = vec![0; 40];
        app[0..4].copy_from_slice(&42u32.to_le_bytes());
        app[24..32].copy_from_slice(&(STEAM_ID_BASE + 1).to_le_bytes());
        let ticket = AuthorizationTicket::from_native(
            42,
            STEAM_ID_BASE + 1,
            app,
            vec![1],
            TicketOffsets {
                app_id_offset: 0,
                steam_id_offset: 24,
            },
        )
        .unwrap();
        assert!(parse_authorization_ticket(&ticket.encoded()).is_ok());
    }
    #[test]
    fn export_preview_has_only_metadata_and_preserves_existing_destination() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("tickets.txt");
        let receipt = export_authorization_ticket(&fixture(), &path, false).unwrap();
        assert!(!receipt.runtime_authorization_verified);
        let summary = inspect_authorization_file(&path).unwrap();
        assert_eq!(summary.sha256, receipt.summary.sha256);
        let json = serde_json::to_string(&summary).unwrap();
        assert!(!json.contains("31313131"));
        let original = fs::read(&path).unwrap();
        assert!(export_authorization_ticket(&fixture(), &path, false).is_err());
        assert_eq!(fs::read(&path).unwrap(), original);
        assert!(
            export_authorization_ticket(&fixture(), &path, true)
                .unwrap()
                .replaced
        );
        assert!(
            export_authorization_ticket(&fixture(), &temp.path().join("file.lua"), false).is_err()
        );
    }
    #[test]
    fn import_rejects_other_account_or_app_without_writes() {
        let mut store = MockStore::default();
        let other =
            AuthorizationImportContext::from_verified_session(42, STEAM_ID_BASE + 2, true).unwrap();
        assert!(import_with_store(&fixture(), &other, true, &mut store).is_err());
        assert_eq!(store.writes, 0);
        assert!(
            AuthorizationImportContext::from_verified_session(42, STEAM_ID_BASE + 1, false)
                .is_err()
        );
        let other =
            AuthorizationImportContext::from_verified_session(43, STEAM_ID_BASE + 1, true).unwrap();
        assert!(import_with_store(&fixture(), &other, true, &mut store).is_err());
        assert_eq!(store.writes, 0);
    }
    #[test]
    fn successful_import_verifies_both_binary_values_and_requires_overwrite() {
        let mut store = MockStore::default();
        assert!(!import_with_store(&fixture(), &context(), false, &mut store).unwrap());
        assert_eq!(store.values[APP_VALUE], fixture().app_ticket);
        assert_eq!(store.values[ENCRYPTED_VALUE], fixture().e_ticket);
        let writes = store.writes;
        assert!(import_with_store(&fixture(), &context(), false, &mut store).is_err());
        assert_eq!(store.writes, writes);
        assert!(import_with_store(&fixture(), &context(), true, &mut store).unwrap());
    }
    #[test]
    fn partial_write_failure_restores_original_values_and_does_not_persist_raw_backup() {
        let mut store = MockStore {
            values: BTreeMap::from([
                (APP_VALUE.into(), vec![0xa1]),
                (ENCRYPTED_VALUE.into(), vec![0xb1]),
            ]),
            fail_second: true,
            ..Default::default()
        };
        let old = store.values.clone();
        assert!(import_with_store(&fixture(), &context(), true, &mut store).is_err());
        assert_eq!(store.values, old);
    }
    #[test]
    fn rollback_preserves_external_changes_instead_of_overwriting_them() {
        let mut store = MockStore {
            fail_second: true,
            external: true,
            ..Default::default()
        };
        let error = import_with_store(&fixture(), &context(), true, &mut store).unwrap_err();
        assert!(error.to_string().contains("external changes"));
        assert_eq!(store.values[APP_VALUE], vec![0x99]);
        assert!(!store.values.contains_key(ENCRYPTED_VALUE));
    }
    #[test]
    fn preview_hash_prevents_import_of_a_file_changed_after_confirmation() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("tickets.txt");
        export_authorization_ticket(&fixture(), &path, false).unwrap();
        let old = inspect_authorization_file(&path).unwrap().sha256;
        let mut bytes = fs::read(&path).unwrap();
        bytes.extend_from_slice(b"\n# changed\n");
        fs::write(&path, bytes).unwrap();
        let error = import_authorization_file(&path, &old, &context(), true).unwrap_err();
        assert!(error.to_string().contains("changed after preview"));
    }
    #[test]
    fn unsupported_old_registry_types_and_other_account_binding_precede_writes() {
        for mut store in [
            MockStore {
                fail_read: true,
                ..Default::default()
            },
            MockStore {
                other_account: true,
                ..Default::default()
            },
        ] {
            assert!(import_with_store(&fixture(), &context(), true, &mut store).is_err());
            assert_eq!(store.writes, 0);
        }
    }
    #[test]
    fn debug_output_redacts_ticket_identity_locations_and_contents() {
        let ticket = fixture();
        let summary = ticket.summary(Some("PRIVATE_FIXTURE_PATH".into()), "b".repeat(64));
        let export = AuthorizationExportReceipt {
            summary: summary.clone(),
            output_path: "PRIVATE_FIXTURE_PATH".into(),
            replaced: false,
            runtime_authorization_verified: false,
        };
        let import = AuthorizationImportReceipt {
            summary,
            registry_path: "PRIVATE_FIXTURE_PATH".into(),
            replaced: false,
            write_verified: true,
            runtime_authorization_verified: false,
        };
        let debug = format!("{ticket:?} {export:?} {import:?}");
        assert!(!debug.contains("PRIVATE_FIXTURE_PATH"));
        assert!(!debug.contains(&(STEAM_ID_BASE + 1).to_string()));
        assert!(!debug.contains(&"b".repeat(64)));
        assert!(!debug.contains("31313131"));
    }
    #[test]
    fn live_account_change_after_snapshots_prevents_all_registry_writes() {
        let mut store = MockStore::default();
        let mut calls = 0;
        let result =
            import_with_store_guarded(&fixture(), &context(), true, &mut store, &mut || {
                calls += 1;
                ensure!(calls < 2, "fixture live account changed");
                Ok(())
            });
        assert!(result.is_err());
        assert_eq!(store.writes, 0);
        assert!(store.values.is_empty());
    }
    #[test]
    fn live_account_change_after_readback_rolls_back_both_ticket_values() {
        let mut store = MockStore {
            values: BTreeMap::from([
                (APP_VALUE.into(), vec![0xa1]),
                (ENCRYPTED_VALUE.into(), vec![0xb1]),
            ]),
            ..Default::default()
        };
        let old = store.values.clone();
        let mut calls = 0;
        let result =
            import_with_store_guarded(&fixture(), &context(), true, &mut store, &mut || {
                calls += 1;
                ensure!(calls < 3, "fixture live account changed");
                Ok(())
            });
        assert!(result.is_err());
        assert_eq!(store.values, old);
        assert_eq!(calls, 3);
    }
}
