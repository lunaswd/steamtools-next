//! Steam-owned account selection and graceful client lifecycle management.
//!
//! Passwords, refresh tokens, SSFN files and account authentication are never
//! accepted by this service. Login completion remains in Steam's native UI.
use anyhow::{bail, Context, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};
#[cfg(windows)]
use std::process::{Command, Stdio};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const MAX_LOGIN_USERS_BYTES: u64 = 1024 * 1024;
const STEAM_ID_BASE: u64 = 76_561_197_960_265_728;
const SHUTDOWN_ATTEMPTS: usize = 180;
const POLL_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SteamSavedAccount {
    pub steam_id: String,
    pub account_name: String,
    pub persona_name: String,
    pub is_current: bool,
    pub last_login_at: Option<u64>,
    pub can_auto_login: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SteamAccountState {
    pub steam_root: String,
    pub running: bool,
    pub current_steam_id: Option<String>,
    pub current_account_name: Option<String>,
    pub accounts: Vec<SteamSavedAccount>,
    /// Local ActiveProcess metadata is a client identity, not server login proof.
    pub identity_source: String,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SteamLifecycleReceipt {
    pub operation: String,
    pub steam_root: String,
    pub previous_running: bool,
    pub launch_requested: bool,
    pub process_id: Option<u32>,
    pub requires_native_login: bool,
    pub selected_steam_id: Option<String>,
    /// Contains only changed login preferences; no credential-bearing VDF copy.
    pub backup_path: Option<String>,
    pub warnings: Vec<String>,
}

/// Read only approved, non-credential fields from Steam's saved-account list.
pub fn get_steam_account_state(steam_root: &Path) -> Result<SteamAccountState> {
    account_state(steam_root, &NativeSteamRuntime)
}

/// Ask the current client to shut down, wait for it to exit, then start normally.
pub fn restart_steam(steam_root: &Path) -> Result<SteamLifecycleReceipt> {
    lifecycle(steam_root, Lifecycle::Restart, &NativeSteamRuntime)
}

/// Disable automatic account selection, then open Steam's own login/chooser UI.
pub fn relogin_steam(steam_root: &Path) -> Result<SteamLifecycleReceipt> {
    lifecycle(steam_root, Lifecycle::Relogin, &NativeSteamRuntime)
}

/// Select a previously saved Steam account, leaving authentication to Steam.
pub fn switch_saved_steam_account(
    steam_root: &Path,
    steam_id: &str,
) -> Result<SteamLifecycleReceipt> {
    if individual_account_id(steam_id).is_none() {
        bail!("A valid saved Steam account ID is required");
    }
    lifecycle(
        steam_root,
        Lifecycle::Switch(steam_id.to_owned()),
        &NativeSteamRuntime,
    )
}

enum Lifecycle {
    Restart,
    Relogin,
    Switch(String),
}

#[derive(Clone, Debug)]
struct LoginSelection {
    account_name: Option<String>,
}

trait SteamRuntime {
    /// Fail closed for an unidentified Steam process or another installation.
    fn running_pids(&self, executable: &Path) -> Result<Vec<u32>>;
    fn active_account_id(&self, root: &Path, pids: &[u32]) -> Result<Option<u32>>;
    fn login_selection(&self, root: &Path) -> Result<LoginSelection>;
    fn set_login_selection(&self, root: &Path, value: Option<&str>) -> Result<()>;
    fn request_shutdown(&self, executable: &Path, root: &Path) -> Result<()>;
    fn launch(&self, executable: &Path, root: &Path) -> Result<u32>;
    fn pause(&self) {
        std::thread::sleep(POLL_INTERVAL);
    }
}

struct NativeSteamRuntime;

fn account_state(root: &Path, runtime: &impl SteamRuntime) -> Result<SteamAccountState> {
    let root = validate_root(root)?;
    let pids = runtime.running_pids(&root.join("steam.exe"))?;
    let mut warnings = Vec::new();
    let document = read_login_document(&root)?;
    let mut accounts = document
        .as_ref()
        .map(LoginDocument::accounts)
        .transpose()?
        .unwrap_or_default();
    let active_id = if pids.is_empty() {
        None
    } else {
        match runtime.active_account_id(&root, &pids) {
            Ok(value) => value,
            Err(_) => {
                warnings.push("Current Steam client identity could not be verified from local process metadata.".into());
                None
            }
        }
    };
    let current = active_id.map(|id| (STEAM_ID_BASE + u64::from(id)).to_string());
    for account in &mut accounts {
        account.is_current = current.as_deref() == Some(account.steam_id.as_str());
    }
    let current_name = accounts
        .iter()
        .find(|account| account.is_current)
        .map(|account| account.account_name.clone());
    accounts.sort_by(|a, b| {
        b.is_current
            .cmp(&a.is_current)
            .then_with(|| b.last_login_at.cmp(&a.last_login_at))
            .then_with(|| {
                a.account_name
                    .to_ascii_lowercase()
                    .cmp(&b.account_name.to_ascii_lowercase())
            })
    });
    Ok(SteamAccountState {
        steam_root: display_path(&root),
        running: !pids.is_empty(),
        current_steam_id: current,
        current_account_name: current_name,
        accounts,
        identity_source: if active_id.is_some() {
            "steamActiveProcess"
        } else {
            "unknown"
        }
        .into(),
        warnings,
    })
}

fn lifecycle(
    root: &Path,
    operation: Lifecycle,
    runtime: &impl SteamRuntime,
) -> Result<SteamLifecycleReceipt> {
    let root = validate_root(root)?;
    let executable = root.join("steam.exe");
    validate_steam_executable(&executable)?;
    // Shares the installer's exclusive sentinel, so Steam cannot be restarted
    // while the kernel is being replaced by this application.
    let _guard = SteamOperationGuard::acquire(&root)?;
    let previous_pids = runtime.running_pids(&executable)?;
    let previous_running = !previous_pids.is_empty();
    if !matches!(operation, Lifecycle::Restart) {
        // Reject malformed account/configuration and unmatched registry roots
        // before disrupting an already running Steam session.
        let before = read_login_document(&root)?;
        validate_target_account(before.as_ref(), &operation)?;
        runtime.login_selection(&root)?;
    }
    if previous_running {
        runtime.request_shutdown(&executable, &root)?;
        let mut stopped = false;
        for _ in 0..SHUTDOWN_ATTEMPTS {
            if runtime.running_pids(&executable)?.is_empty() {
                stopped = true;
                break;
            }
            runtime.pause();
        }
        if !stopped {
            bail!("Steam did not finish its graceful shutdown within 45 seconds; no login preferences were changed and no second client was launched");
        }
    }

    let mut change = if matches!(operation, Lifecycle::Restart) {
        None
    } else {
        Some(prepare_login_change(&root, &operation, runtime)?)
    };
    let process_id = match runtime.launch(&executable, &root) {
        Ok(id) => id,
        Err(error) => {
            if let Some(change) = &mut change {
                change
                    .rollback(&root, runtime)
                    .context("Steam launch failed; login preference rollback also failed")?;
            }
            return Err(error)
                .context("Could not start Steam; changed login preferences were restored");
        }
    };
    let name = match &operation {
        Lifecycle::Restart => "restart",
        Lifecycle::Relogin => "relogin",
        Lifecycle::Switch(_) => "switchAccount",
    };
    Ok(SteamLifecycleReceipt {
        operation: name.into(),
        steam_root: display_path(&root),
        previous_running,
        launch_requested: true,
        process_id: Some(process_id),
        requires_native_login: !matches!(operation, Lifecycle::Restart),
        selected_steam_id: match operation { Lifecycle::Switch(id) => Some(id), _ => None },
        backup_path: change.as_ref().map(|change| display_path(&change.backup)),
        warnings: vec!["Steam launch was requested; account authentication and login completion remain in the Steam client.".into()],
    })
}

fn validate_target_account(document: Option<&LoginDocument>, operation: &Lifecycle) -> Result<()> {
    if let Lifecycle::Switch(id) = operation {
        let accounts = document
            .context("Steam has no saved accounts")?
            .accounts()?;
        if !accounts.iter().any(|account| account.steam_id == *id) {
            bail!("The selected account is not in Steam's saved-account list");
        }
    } else if let Some(document) = document {
        document.accounts()?;
    }
    Ok(())
}

struct LoginChange {
    original: Option<Vec<u8>>,
    changed_sha256: Option<String>,
    registry: LoginSelection,
    backup: PathBuf,
}

impl LoginChange {
    fn rollback(&mut self, root: &Path, runtime: &impl SteamRuntime) -> Result<()> {
        let registry_result =
            runtime.set_login_selection(root, self.registry.account_name.as_deref());
        let document_result = if let (Some(original), Some(expected)) =
            (&self.original, &self.changed_sha256)
        {
            let path = root.join("config/loginusers.vdf");
            let current = read_regular(&path, MAX_LOGIN_USERS_BYTES)?;
            if sha256(&current) != *expected {
                bail!("Steam login preferences changed externally; they were preserved instead of overwritten during rollback");
            }
            atomic_replace(&path, original)
        } else {
            Ok(())
        };
        registry_result?;
        document_result
    }
}

fn prepare_login_change(
    root: &Path,
    operation: &Lifecycle,
    runtime: &impl SteamRuntime,
) -> Result<LoginChange> {
    let document = read_login_document(root)?;
    validate_target_account(document.as_ref(), operation)?;
    let registry = runtime.login_selection(root)?;
    let selected = if let Lifecycle::Switch(id) = operation {
        Some(
            document
                .as_ref()
                .context("Saved account list is missing")?
                .accounts()?
                .into_iter()
                .find(|account| account.steam_id == *id)
                .context("Saved account was removed during shutdown")?,
        )
    } else {
        None
    };
    let changed = document
        .as_ref()
        .map(|doc| {
            doc.changed_preferences(selected.as_ref().map(|account| account.steam_id.as_str()))
        })
        .transpose()?;
    let stamp = unique_stamp()?;
    let backup_dir = root
        .join(".steamtools-next/backups")
        .join(format!("accounts-{stamp}"));
    crate::kernel_manage::reject_link_ancestors(&backup_dir)?;
    fs::create_dir_all(&backup_dir)?;
    crate::kernel_manage::reject_link_ancestors(&backup_dir)?;
    let backup = backup_dir.join("login-preferences.json");
    let preferences = document
        .as_ref()
        .map(LoginDocument::preference_backup)
        .transpose()?
        .unwrap_or_default();
    let record = serde_json::json!({
        "schemaVersion": 1,
        "kind": "steamLoginPreferences",
        "createdAt": stamp,
        "autoLoginUser": registry.account_name,
        "loginUsersSha256": document.as_ref().map(|doc| sha256(&doc.original)),
        "preferences": preferences,
        "note": "Only login preferences are backed up. Passwords, tokens and unknown source fields are not included."
    });
    write_new(&backup, &serde_json::to_vec_pretty(&record)?)?;
    let mut change = LoginChange {
        original: document.as_ref().map(|doc| doc.original.clone()),
        changed_sha256: changed.as_ref().map(|bytes| sha256(bytes)),
        registry,
        backup,
    };
    if let Some(bytes) = &changed {
        let path = root.join("config/loginusers.vdf");
        // Steam is stopped; nevertheless reject a concurrent external rewrite.
        let original = &document.as_ref().unwrap().original;
        if read_regular(&path, MAX_LOGIN_USERS_BYTES)? != *original {
            bail!("Steam login preferences changed before the requested account selection");
        }
        atomic_replace(&path, bytes)?;
    }
    let target_name = selected
        .as_ref()
        .map(|account| account.account_name.as_str())
        .unwrap_or("");
    if let Err(error) = runtime.set_login_selection(root, Some(target_name)) {
        change
            .rollback(root, runtime)
            .context("Could not restore login preferences after registry update failed")?;
        return Err(error).context("Could not select the requested Steam login screen");
    }
    Ok(change)
}

#[derive(Clone, Copy)]
enum Encoding {
    Utf8,
    Utf8Bom,
    Utf16Le,
    Utf16Be,
}

struct LoginDocument {
    original: Vec<u8>,
    text: String,
    encoding: Encoding,
    accounts: Vec<AccountBlock>,
}

struct AccountBlock {
    steam_id: String,
    close: usize,
    fields: Vec<(String, std::ops::Range<usize>)>,
}

struct Token {
    kind: TokenKind,
    start: usize,
    end: usize,
}
enum TokenKind {
    Text,
    Open,
    Close,
}

impl LoginDocument {
    fn parse(original: Vec<u8>) -> Result<Self> {
        let (text, encoding) = decode_vdf(&original)?;
        // Syntax validation is bounded and never reports values in errors.
        crate::vdf::parse(&text).context("Steam saved-account configuration is invalid")?;
        let tokens = token_ranges(&text)?;
        let mut index = 0;
        let mut accounts = Vec::new();
        while index < tokens.len() {
            let key = token_text(&text, &tokens[index])?;
            index += 1;
            if index == tokens.len() {
                bail!("Steam account configuration is truncated");
            }
            if key.eq_ignore_ascii_case("users") && matches!(tokens[index].kind, TokenKind::Open) {
                index += 1;
                while index < tokens.len() && !matches!(tokens[index].kind, TokenKind::Close) {
                    let id = token_text(&text, &tokens[index])?;
                    index += 1;
                    if index == tokens.len() || !matches!(tokens[index].kind, TokenKind::Open) {
                        bail!("Steam account entry must be an object");
                    }
                    index += 1;
                    let mut fields = Vec::new();
                    while index < tokens.len() && !matches!(tokens[index].kind, TokenKind::Close) {
                        let name = token_text(&text, &tokens[index])?;
                        index += 1;
                        if index == tokens.len() {
                            bail!("Steam account configuration is truncated");
                        }
                        if matches!(tokens[index].kind, TokenKind::Text) {
                            if approved_field(&name) {
                                if fields.iter().any(
                                    |(key, _): &(String, std::ops::Range<usize>)| {
                                        key.eq_ignore_ascii_case(&name)
                                    },
                                ) {
                                    bail!("Steam account configuration contains duplicate approved fields");
                                }
                                fields.push((name, tokens[index].start..tokens[index].end));
                            }
                            index += 1;
                        } else {
                            skip_object(&tokens, &mut index)?;
                        }
                    }
                    if index == tokens.len() {
                        bail!("Steam account configuration is truncated");
                    }
                    let close = tokens[index].start;
                    index += 1;
                    if individual_account_id(&id).is_some() {
                        if accounts
                            .iter()
                            .any(|entry: &AccountBlock| entry.steam_id == id)
                        {
                            bail!("Steam saved-account IDs are duplicated");
                        }
                        accounts.push(AccountBlock {
                            steam_id: id,
                            close,
                            fields,
                        });
                    }
                    if accounts.len() > 256 {
                        bail!("Steam saved-account count exceeds the limit");
                    }
                }
                if index == tokens.len() {
                    bail!("Steam users configuration is truncated");
                }
                index += 1;
            } else if matches!(tokens[index].kind, TokenKind::Open) {
                skip_object(&tokens, &mut index)?;
            } else {
                index += 1;
            }
        }
        Ok(Self {
            original,
            text,
            encoding,
            accounts,
        })
    }

    fn field(&self, account: &AccountBlock, name: &str) -> Result<Option<String>> {
        account
            .fields
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, range)| decode_token(&self.text[range.clone()]))
            .transpose()
    }

    fn accounts(&self) -> Result<Vec<SteamSavedAccount>> {
        self.accounts
            .iter()
            .filter_map(|account| {
                let value = (|| -> Result<Option<SteamSavedAccount>> {
                    let name = self.field(account, "AccountName")?.unwrap_or_default();
                    if name.is_empty() {
                        return Ok(None);
                    }
                    let name = display_name(name)?;
                    let persona = self
                        .field(account, "PersonaName")?
                        .filter(|value| !value.is_empty())
                        .unwrap_or_else(|| name.clone());
                    for key in ["AllowAutoLogin", "MostRecent"] {
                        if self
                            .field(account, key)?
                            .is_some_and(|value| value != "0" && value != "1")
                        {
                            bail!("Steam saved-account login preference is not a boolean");
                        }
                    }
                    Ok(Some(SteamSavedAccount {
                        steam_id: account.steam_id.clone(),
                        account_name: name,
                        persona_name: display_name(persona)?,
                        is_current: false,
                        last_login_at: self
                            .field(account, "Timestamp")?
                            .and_then(|value| value.parse::<u64>().ok())
                            .filter(|value| *value > 0),
                        can_auto_login: self.field(account, "AllowAutoLogin")?.as_deref()
                            == Some("1"),
                    }))
                })();
                match value {
                    Ok(Some(value)) => Some(Ok(value)),
                    Ok(None) => None,
                    Err(error) => Some(Err(error)),
                }
            })
            .collect()
    }

    fn preference_backup(&self) -> Result<Vec<serde_json::Value>> {
        self.accounts
            .iter()
            .map(|account| {
                Ok(serde_json::json!({
                    "steamId": account.steam_id,
                    "allowAutoLogin": self.field(account, "AllowAutoLogin")?,
                    "mostRecent": self.field(account, "MostRecent")?
                }))
            })
            .collect()
    }

    fn changed_preferences(&self, selected: Option<&str>) -> Result<Vec<u8>> {
        let newline = if self.text.contains("\r\n") {
            "\r\n"
        } else {
            "\n"
        };
        let mut replacements = Vec::new();
        for account in &self.accounts {
            let mut additions = String::new();
            let changes: &[(&str, &str)] = if let Some(selected) = selected {
                // A cached account's existing automatic-login preference is
                // never upgraded. Steam asks for authentication when needed.
                if account.steam_id == selected {
                    &[("MostRecent", "1")]
                } else {
                    &[("MostRecent", "0")]
                }
            } else {
                &[("AllowAutoLogin", "0"), ("MostRecent", "0")]
            };
            for (key, value) in changes {
                if let Some((_, range)) = account
                    .fields
                    .iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case(key))
                {
                    let quoted = self.text[range.clone()].starts_with('"');
                    replacements.push((
                        range.clone(),
                        if quoted {
                            format!("\"{value}\"")
                        } else {
                            (*value).into()
                        },
                    ));
                } else {
                    additions.push_str(&format!("\t\t\"{key}\"\t\t\"{value}\"{newline}"));
                }
            }
            if !additions.is_empty() {
                replacements.push((account.close..account.close, additions));
            }
        }
        replacements.sort_by_key(|replacement| std::cmp::Reverse(replacement.0.start));
        let mut text = self.text.clone();
        for (range, value) in replacements {
            text.replace_range(range, &value);
        }
        crate::vdf::parse(&text).context("Changed Steam login preferences are invalid")?;
        Ok(encode_vdf(&text, self.encoding))
    }
}

fn approved_field(name: &str) -> bool {
    [
        "AccountName",
        "PersonaName",
        "Timestamp",
        "AllowAutoLogin",
        "MostRecent",
    ]
    .iter()
    .any(|allowed| name.eq_ignore_ascii_case(allowed))
}

fn display_name(value: String) -> Result<String> {
    if value.chars().count() > 256 || value.chars().any(char::is_control) {
        bail!("Steam saved-account display metadata is invalid");
    }
    Ok(value)
}

fn individual_account_id(value: &str) -> Option<u32> {
    let id = value.parse::<u64>().ok()?;
    let account = id.checked_sub(STEAM_ID_BASE)?;
    (account > 0 && account <= u64::from(u32::MAX)).then_some(account as u32)
}

fn decode_vdf(bytes: &[u8]) -> Result<(String, Encoding)> {
    if bytes.starts_with(&[0xff, 0xfe]) || bytes.starts_with(&[0xfe, 0xff]) {
        if !(bytes.len() - 2).is_multiple_of(2) {
            bail!("Steam login metadata has truncated UTF-16");
        }
        let little = bytes[0] == 0xff;
        let words: Vec<_> = bytes[2..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| {
                if little {
                    u16::from_le_bytes([pair[0], pair[1]])
                } else {
                    u16::from_be_bytes([pair[0], pair[1]])
                }
            })
            .collect();
        Ok((
            String::from_utf16(&words).context("Steam login metadata has invalid UTF-16")?,
            if little {
                Encoding::Utf16Le
            } else {
                Encoding::Utf16Be
            },
        ))
    } else {
        let bom = bytes.starts_with(&[0xef, 0xbb, 0xbf]);
        Ok((
            String::from_utf8(bytes[if bom { 3 } else { 0 }..].to_vec())
                .context("Steam login metadata has invalid UTF-8")?,
            if bom {
                Encoding::Utf8Bom
            } else {
                Encoding::Utf8
            },
        ))
    }
}

fn encode_vdf(text: &str, encoding: Encoding) -> Vec<u8> {
    match encoding {
        Encoding::Utf8 => text.as_bytes().to_vec(),
        Encoding::Utf8Bom => [vec![0xef, 0xbb, 0xbf], text.as_bytes().to_vec()].concat(),
        Encoding::Utf16Le | Encoding::Utf16Be => {
            let little = matches!(encoding, Encoding::Utf16Le);
            let mut bytes = if little {
                vec![0xff, 0xfe]
            } else {
                vec![0xfe, 0xff]
            };
            for word in text.encode_utf16() {
                bytes.extend(if little {
                    word.to_le_bytes()
                } else {
                    word.to_be_bytes()
                });
            }
            bytes
        }
    }
}

fn token_ranges(text: &str) -> Result<Vec<Token>> {
    let bytes = text.as_bytes();
    let mut index = 0;
    let mut tokens = Vec::new();
    while index < bytes.len() {
        if bytes[index].is_ascii_whitespace() {
            index += 1;
            continue;
        }
        if bytes[index..].starts_with(b"//") {
            while index < bytes.len() && bytes[index] != b'\n' {
                index += 1;
            }
            continue;
        }
        let start = index;
        let kind = match bytes[index] {
            b'{' => {
                index += 1;
                TokenKind::Open
            }
            b'}' => {
                index += 1;
                TokenKind::Close
            }
            b'"' => {
                index += 1;
                loop {
                    if index == bytes.len() {
                        bail!("Steam login metadata contains an unfinished string");
                    }
                    match bytes[index] {
                        b'\\' => index += 2,
                        b'"' => {
                            index += 1;
                            break;
                        }
                        _ => index += 1,
                    }
                    if index > bytes.len() {
                        bail!("Steam login metadata contains an unfinished escape");
                    }
                }
                TokenKind::Text
            }
            _ => {
                while index < bytes.len()
                    && !bytes[index].is_ascii_whitespace()
                    && !matches!(bytes[index], b'{' | b'}')
                {
                    index += 1;
                }
                TokenKind::Text
            }
        };
        tokens.push(Token {
            kind,
            start,
            end: index,
        });
    }
    Ok(tokens)
}

fn token_text(text: &str, token: &Token) -> Result<String> {
    if !matches!(token.kind, TokenKind::Text) {
        bail!("Steam account metadata has an unexpected object token");
    }
    decode_token(&text[token.start..token.end])
}

fn decode_token(text: &str) -> Result<String> {
    if !text.starts_with('"') {
        return Ok(text.into());
    }
    let root = crate::vdf::parse(&format!("\"field\" {text}"))?;
    Ok(root
        .string("field")
        .context("Steam account field is invalid")?
        .to_owned())
}

fn skip_object(tokens: &[Token], index: &mut usize) -> Result<()> {
    if !matches!(tokens[*index].kind, TokenKind::Open) {
        bail!("Steam account metadata contains an unexpected object");
    }
    let mut depth = 0;
    while *index < tokens.len() {
        match tokens[*index].kind {
            TokenKind::Open => depth += 1,
            TokenKind::Close => depth -= 1,
            TokenKind::Text => {}
        }
        *index += 1;
        if depth == 0 {
            return Ok(());
        }
    }
    bail!("Steam account metadata contains an unfinished object")
}

fn read_login_document(root: &Path) -> Result<Option<LoginDocument>> {
    let path = root.join("config/loginusers.vdf");
    crate::kernel_manage::reject_link_ancestors(&path)?;
    match fs::symlink_metadata(&path) {
        Ok(_) => LoginDocument::parse(read_regular(&path, MAX_LOGIN_USERS_BYTES)?).map(Some),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).context("Could not inspect Steam saved-account metadata"),
    }
}

fn validate_root(path: &Path) -> Result<PathBuf> {
    let root = crate::kernel_manage::absolute_local_path(path, "Steam account directory")?;
    crate::kernel_manage::reject_link_ancestors(&root)?;
    if !fs::symlink_metadata(&root)?.is_dir() {
        bail!("Steam account directory is not a directory");
    }
    for name in ["steam.exe", "steamclient64.dll", "steamui.dll"] {
        require_regular(&root.join(name))?;
    }
    fs::canonicalize(&root).context("Could not resolve Steam account directory")
}

fn require_regular(path: &Path) -> Result<fs::Metadata> {
    crate::kernel_manage::reject_link_ancestors(path)?;
    let meta = fs::symlink_metadata(path).context("Steam component is missing or inaccessible")?;
    if !meta.is_file() {
        bail!("Steam component is not a regular file");
    }
    Ok(meta)
}

fn read_regular(path: &Path, limit: u64) -> Result<Vec<u8>> {
    if require_regular(path)?.len() > limit {
        bail!("Steam account metadata exceeds its size limit");
    }
    let mut bytes = Vec::new();
    File::open(path)?.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        bail!("Steam account metadata exceeds its size limit");
    }
    Ok(bytes)
}

fn validate_steam_executable(path: &Path) -> Result<()> {
    let mut file = File::open(path)?;
    let length = file.metadata()?.len();
    let mut head = [0u8; 64];
    file.read_exact(&mut head)
        .context("Steam executable has a truncated PE header")?;
    if &head[..2] != b"MZ" {
        bail!("Steam executable is not a Windows PE executable");
    }
    let offset = u32::from_le_bytes(head[60..64].try_into().unwrap()) as u64;
    if offset + 26 > length {
        bail!("Steam executable has an invalid PE offset");
    }
    file.seek(SeekFrom::Start(offset))?;
    let mut header = [0u8; 26];
    file.read_exact(&mut header)?;
    let machine = u16::from_le_bytes([header[4], header[5]]);
    let flags = u16::from_le_bytes([header[22], header[23]]);
    let magic = u16::from_le_bytes([header[24], header[25]]);
    if &header[..4] != b"PE\0\0"
        || ![0x14c, 0x8664].contains(&machine)
        || flags & 2 == 0
        || flags & 0x2000 != 0
        || ![0x10b, 0x20b].contains(&magic)
    {
        bail!("Steam executable is not a supported PE executable image");
    }
    Ok(())
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    crate::kernel_manage::reject_link_ancestors(path)?;
    let mut file = OpenOptions::new().create_new(true).write(true).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn atomic_replace(path: &Path, bytes: &[u8]) -> Result<()> {
    require_regular(path)?;
    let parent = path.parent().context("Steam account file has no parent")?;
    let temp = parent.join(format!(".stn-account-{}.tmp", unique_stamp()?));
    let result = (|| {
        write_new(&temp, bytes)?;
        replace_file(&temp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

#[cfg(windows)]
fn replace_file(from: &Path, to: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };
    let from: Vec<_> = from.as_os_str().encode_wide().chain(Some(0)).collect();
    let to: Vec<_> = to.as_os_str().encode_wide().chain(Some(0)).collect();
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
fn replace_file(from: &Path, to: &Path) -> Result<()> {
    fs::rename(from, to)?;
    Ok(())
}

struct SteamOperationGuard {
    path: PathBuf,
    marker: Vec<u8>,
    _file: File,
}
impl SteamOperationGuard {
    fn acquire(root: &Path) -> Result<Self> {
        let path = root.join(".steamtools-next-kernel.lock");
        let marker = format!("accounts-{}", unique_stamp()?).into_bytes();
        crate::kernel_manage::reject_link_ancestors(&path)?;
        for _ in 0..3 {
            match OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut file) => {
                    file.try_lock()
                        .context("Another Steam account operation is already in progress")?;
                    if let Err(error) = file.write_all(&marker).and_then(|_| file.sync_all()) {
                        let _ = fs::remove_file(&path);
                        return Err(error).context("Could not write the Steam operation guard");
                    }
                    return Ok(Self {
                        path,
                        marker,
                        _file: file,
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    let prior = read_regular(&path, 1024)
                        .context("Another Steam or kernel operation is already in progress")?;
                    // Recover only this service's crashed guards. Existing
                    // kernel installer guards use their own marker and remain
                    // untouched. The OS lock is released automatically on exit.
                    if !prior.starts_with(b"accounts-") {
                        bail!("Another Steam or kernel operation is already in progress");
                    }
                    let mut file = OpenOptions::new().read(true).write(true).open(&path)?;
                    file.try_lock()
                        .context("Another Steam account operation is already in progress")?;
                    if read_guard_marker(&mut file)? != prior {
                        bail!("Steam operation guard changed during recovery");
                    }
                    fs::remove_file(&path)
                        .context("Could not recover a completed Steam account operation guard")?;
                    drop(file);
                }
                Err(error) => {
                    return Err(error).context("Could not reserve the Steam operation guard")
                }
            }
        }
        bail!("Steam operation guard recovery was interrupted")
    }
}
impl Drop for SteamOperationGuard {
    fn drop(&mut self) {
        // Windows byte-range locks also block reads through other handles.
        // Inspect the marker using the same locked handle before unlinking it.
        if read_guard_marker(&mut self._file).ok().as_deref() == Some(self.marker.as_slice()) {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn read_guard_marker(file: &mut File) -> Result<Vec<u8>> {
    if file.metadata()?.len() > 1024 {
        bail!("Steam operation guard is invalid");
    }
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    Read::by_ref(file).take(1025).read_to_end(&mut bytes)?;
    if bytes.len() > 1024 {
        bail!("Steam operation guard is invalid");
    }
    Ok(bytes)
}

fn unique_stamp() -> Result<String> {
    Ok(format!(
        "{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    ))
}
fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn display_path(path: &Path) -> String {
    path.to_string_lossy().trim_start_matches(r"\\?\").into()
}
#[cfg(windows)]
fn path_identity(path: &Path) -> String {
    display_path(&fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()))
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_ascii_lowercase()
}

#[cfg(windows)]
fn registry_key(root: &Path, writable: bool) -> Result<winreg::RegKey> {
    use winreg::{
        enums::{HKEY_CURRENT_USER, KEY_READ, KEY_WRITE},
        RegKey,
    };
    let key = RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey_with_flags(
            "Software\\Valve\\Steam",
            if writable {
                KEY_READ | KEY_WRITE
            } else {
                KEY_READ
            },
        )
        .context("Steam's current-user registry configuration is unavailable")?;
    let configured: String = key
        .get_value("SteamPath")
        .or_else(|_| key.get_value("InstallPath"))
        .context("Steam registry installation path is unavailable")?;
    if path_identity(Path::new(&configured)) != path_identity(root) {
        bail!("Selected Steam directory does not match the current user's registered installation");
    }
    Ok(key)
}

#[cfg(windows)]
impl SteamRuntime for NativeSteamRuntime {
    fn running_pids(&self, executable: &Path) -> Result<Vec<u32>> {
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
        // Query executable names/paths only, never process memory or command lines.
        unsafe {
            let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
            if snapshot == INVALID_HANDLE_VALUE {
                bail!("Steam process state could not be inspected");
            }
            let result = (|| {
                let mut entry = PROCESSENTRY32W {
                    dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
                    ..Default::default()
                };
                let mut next = Process32FirstW(snapshot, &mut entry);
                let mut pids = Vec::new();
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
                            bail!("An existing Steam process could not be identified; no client was stopped or launched");
                        }
                        let mut buffer = vec![0u16; 32768];
                        let mut length = buffer.len() as u32;
                        let queried = QueryFullProcessImageNameW(
                            process,
                            0,
                            buffer.as_mut_ptr(),
                            &mut length,
                        );
                        CloseHandle(process);
                        if queried == 0 {
                            bail!("An existing Steam process path could not be verified");
                        }
                        let path =
                            PathBuf::from(String::from_utf16_lossy(&buffer[..length as usize]));
                        if path_identity(&path) != path_identity(executable) {
                            bail!("A Steam client from another installation is running; select that installation before managing the client");
                        }
                        pids.push(entry.th32ProcessID);
                    }
                    next = Process32NextW(snapshot, &mut entry);
                }
                Ok(pids)
            })();
            CloseHandle(snapshot);
            result
        }
    }
    fn active_account_id(&self, root: &Path, pids: &[u32]) -> Result<Option<u32>> {
        let active = registry_key(root, false)?.open_subkey("ActiveProcess")?;
        let pid: u32 = active.get_value("pid")?;
        let account: u32 = active.get_value("ActiveUser")?;
        Ok((pids.contains(&pid) && account > 0).then_some(account))
    }
    fn login_selection(&self, root: &Path) -> Result<LoginSelection> {
        use winreg::{enums::REG_SZ, types::FromRegValue};
        // Operations validate write access before requesting client shutdown.
        let key = registry_key(root, true)?;
        let value = match key.get_raw_value("AutoLoginUser") {
            Ok(raw) => {
                if raw.vtype != REG_SZ || raw.bytes.len() > 1026 {
                    bail!("Steam auto-login selection has an unsupported registry format");
                }
                let value = String::from_reg_value(&raw)
                    .context("Steam auto-login selection is not a valid string")?;
                if value.chars().count() > 256 || value.chars().any(char::is_control) {
                    bail!("Steam auto-login account metadata is invalid");
                }
                Some(value)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(error).context("Steam auto-login selection is not a readable string")
            }
        };
        Ok(LoginSelection {
            account_name: value,
        })
    }
    fn set_login_selection(&self, root: &Path, value: Option<&str>) -> Result<()> {
        let key = registry_key(root, true)?;
        match value {
            Some(value) => key.set_value("AutoLoginUser", &value)?,
            None => match key.delete_value("AutoLoginUser") {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            },
        }
        Ok(())
    }
    fn request_shutdown(&self, executable: &Path, root: &Path) -> Result<()> {
        use std::os::windows::process::CommandExt;
        Command::new(executable)
            .arg("-shutdown")
            .current_dir(root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(0x08000000)
            .spawn()
            .context("Could not request Steam's graceful shutdown")?;
        Ok(())
    }
    fn launch(&self, executable: &Path, root: &Path) -> Result<u32> {
        Ok(Command::new(executable)
            .current_dir(root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("Could not open the Steam client")?
            .id())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;

    const FIRST: &str = "76561197960265729";
    const SECOND: &str = "76561197960265730";
    const SOURCE: &str = "\"users\"\r\n{\r\n\t\"76561197960265729\"\r\n\t{\r\n\t\t\"AccountName\" \"alpha_user\"\r\n\t\t\"PersonaName\" \"示例 A\"\r\n\t\t\"AllowAutoLogin\" \"1\"\r\n\t\t\"MostRecent\" \"1\"\r\n\t\t\"Timestamp\" \"123\"\r\n\t\t\"UnknownAuthField\" \"PRIVATE_FIXTURE_AUTH_VALUE\"\r\n\t\t\"NestedUnknown\" { \"RefreshToken\" \"PRIVATE_FIXTURE_REFRESH_VALUE\" }\r\n\t}\r\n\t\"76561197960265730\"\r\n\t{\r\n\t\t\"AccountName\" \"beta_user\"\r\n\t\t\"PersonaName\" \"Example B\"\r\n\t\t\"AllowAutoLogin\" \"0\"\r\n\t\t\"MostRecent\" \"0\"\r\n\t}\r\n}\r\n// untouched comment with braces { }\r\n";

    struct MockRuntime {
        pids: RefCell<VecDeque<Vec<u32>>>,
        fallback_pids: Vec<u32>,
        active_id: Option<u32>,
        selected: RefCell<Option<String>>,
        events: RefCell<Vec<String>>,
        launch_fails: Cell<bool>,
        selection_failures: Cell<usize>,
        external_change_on_launch: Cell<bool>,
    }

    impl MockRuntime {
        fn stopped() -> Self {
            Self {
                pids: RefCell::new(VecDeque::new()),
                fallback_pids: vec![],
                active_id: None,
                selected: RefCell::new(Some("alpha_user".into())),
                events: RefCell::new(vec![]),
                launch_fails: Cell::new(false),
                selection_failures: Cell::new(0),
                external_change_on_launch: Cell::new(false),
            }
        }
        fn shutting_down() -> Self {
            let mut runtime = Self::stopped();
            runtime.pids = RefCell::new([vec![11], vec![11], vec![]].into_iter().collect());
            runtime
        }
    }

    impl SteamRuntime for MockRuntime {
        fn running_pids(&self, _executable: &Path) -> Result<Vec<u32>> {
            Ok(self
                .pids
                .borrow_mut()
                .pop_front()
                .unwrap_or_else(|| self.fallback_pids.clone()))
        }
        fn active_account_id(&self, _root: &Path, _pids: &[u32]) -> Result<Option<u32>> {
            Ok(self.active_id)
        }
        fn login_selection(&self, _root: &Path) -> Result<LoginSelection> {
            Ok(LoginSelection {
                account_name: self.selected.borrow().clone(),
            })
        }
        fn set_login_selection(&self, _root: &Path, value: Option<&str>) -> Result<()> {
            self.events.borrow_mut().push("select".into());
            if self.selection_failures.get() > 0 {
                self.selection_failures
                    .set(self.selection_failures.get() - 1);
                bail!("mock registry write failure");
            }
            *self.selected.borrow_mut() = value.map(str::to_owned);
            Ok(())
        }
        fn request_shutdown(&self, executable: &Path, root: &Path) -> Result<()> {
            assert_eq!(executable, root.join("steam.exe"));
            self.events.borrow_mut().push("shutdown".into());
            Ok(())
        }
        fn launch(&self, executable: &Path, root: &Path) -> Result<u32> {
            assert_eq!(executable, root.join("steam.exe"));
            self.events.borrow_mut().push("launch".into());
            if self.external_change_on_launch.get() {
                fs::write(root.join("config/loginusers.vdf"), b"external preferences")?;
            }
            if self.launch_fails.get() {
                bail!("mock Steam launch failure");
            }
            Ok(22)
        }
        fn pause(&self) {
            self.events.borrow_mut().push("wait".into());
        }
    }

    fn executable() -> Vec<u8> {
        let mut bytes = vec![0u8; 256];
        bytes[..2].copy_from_slice(b"MZ");
        bytes[60..64].copy_from_slice(&128u32.to_le_bytes());
        bytes[128..132].copy_from_slice(b"PE\0\0");
        bytes[132..134].copy_from_slice(&0x14cu16.to_le_bytes());
        bytes[150..152].copy_from_slice(&2u16.to_le_bytes());
        bytes[152..154].copy_from_slice(&0x10bu16.to_le_bytes());
        bytes
    }

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("config")).unwrap();
        fs::write(dir.path().join("steam.exe"), executable()).unwrap();
        for name in ["steamclient64.dll", "steamui.dll"] {
            fs::write(dir.path().join(name), b"fixture").unwrap();
        }
        fs::write(dir.path().join("config/loginusers.vdf"), SOURCE).unwrap();
        dir
    }

    #[test]
    fn summary_exports_only_approved_account_metadata_and_current_client_identity() {
        let dir = fixture();
        let mut runtime = MockRuntime::stopped();
        runtime.fallback_pids = vec![11];
        runtime.active_id = Some(2);
        let state = account_state(dir.path(), &runtime).unwrap();
        assert!(state.running);
        assert_eq!(state.current_steam_id.as_deref(), Some(SECOND));
        assert_eq!(state.current_account_name.as_deref(), Some("beta_user"));
        assert_eq!(state.accounts[0].steam_id, SECOND);
        assert!(state.accounts[0].is_current);
        let json = serde_json::to_string(&state).unwrap();
        for private in [
            "PRIVATE_FIXTURE_AUTH_VALUE",
            "PRIVATE_FIXTURE_REFRESH_VALUE",
            "RefreshToken",
            "UnknownAuthField",
            "MostRecent",
        ] {
            assert!(!json.contains(private));
        }
        assert_eq!(
            fs::read(dir.path().join("config/loginusers.vdf")).unwrap(),
            SOURCE.as_bytes()
        );
    }

    #[test]
    fn a_saved_most_recent_account_is_not_claimed_current_when_steam_is_stopped() {
        let dir = fixture();
        let mut runtime = MockRuntime::stopped();
        runtime.active_id = Some(1);
        let state = account_state(dir.path(), &runtime).unwrap();
        assert!(!state.running);
        assert!(state.current_steam_id.is_none());
        assert!(state.current_account_name.is_none());
        assert!(state.accounts.iter().all(|account| !account.is_current));
    }

    #[test]
    fn relogin_patches_only_preferences_and_preserves_encoding_unknown_fields_and_comments() {
        for encoding in [
            Encoding::Utf8,
            Encoding::Utf8Bom,
            Encoding::Utf16Le,
            Encoding::Utf16Be,
        ] {
            let source = encode_vdf(SOURCE, encoding);
            let document = LoginDocument::parse(source).unwrap();
            let changed = document.changed_preferences(None).unwrap();
            let (text, _) = decode_vdf(&changed).unwrap();
            let expected = SOURCE
                .replace("\"AllowAutoLogin\" \"1\"", "\"AllowAutoLogin\" \"0\"")
                .replace("\"MostRecent\" \"1\"", "\"MostRecent\" \"0\"");
            assert_eq!(text, expected);
            assert_eq!(changed, encode_vdf(&expected, encoding));
            let backup = serde_json::to_string(&document.preference_backup().unwrap()).unwrap();
            assert!(!backup.contains("PRIVATE_FIXTURE"));
            assert!(!backup.contains("UnknownAuthField"));
        }
    }

    #[test]
    fn missing_preference_fields_are_added_without_removing_existing_metadata() {
        let source = format!(
            "\"users\" {{ \"{FIRST}\" {{ \"AccountName\" \"alpha_user\" \"Unknown\" \"kept\" }} }}"
        );
        let doc = LoginDocument::parse(source.into_bytes()).unwrap();
        let bytes = doc.changed_preferences(None).unwrap();
        let value = crate::vdf::parse(std::str::from_utf8(&bytes).unwrap()).unwrap();
        let account = value.get("users").unwrap().get(FIRST).unwrap();
        assert_eq!(account.string("Unknown"), Some("kept"));
        assert_eq!(account.string("AllowAutoLogin"), Some("0"));
        assert_eq!(account.string("MostRecent"), Some("0"));
    }

    #[test]
    fn restart_is_graceful_waits_for_exit_and_never_changes_account_preferences() {
        let dir = fixture();
        let runtime = MockRuntime::shutting_down();
        let result = lifecycle(dir.path(), Lifecycle::Restart, &runtime).unwrap();
        assert!(result.previous_running && result.launch_requested);
        assert!(!result.requires_native_login);
        assert!(result.backup_path.is_none());
        assert_eq!(*runtime.events.borrow(), vec!["shutdown", "wait", "launch"]);
        assert_eq!(runtime.selected.borrow().as_deref(), Some("alpha_user"));
        assert_eq!(
            fs::read(dir.path().join("config/loginusers.vdf")).unwrap(),
            SOURCE.as_bytes()
        );
        assert!(!dir.path().join(".steamtools-next-kernel.lock").exists());
    }

    #[test]
    fn shutdown_timeout_does_not_change_accounts_or_launch_a_second_client() {
        let dir = fixture();
        let mut runtime = MockRuntime::stopped();
        runtime.fallback_pids = vec![11];
        let error = lifecycle(dir.path(), Lifecycle::Relogin, &runtime).unwrap_err();
        assert!(error.to_string().contains("45 seconds"));
        assert!(!runtime
            .events
            .borrow()
            .iter()
            .any(|event| event == "select" || event == "launch"));
        assert_eq!(
            fs::read(dir.path().join("config/loginusers.vdf")).unwrap(),
            SOURCE.as_bytes()
        );
        assert!(!dir.path().join(".steamtools-next").exists());
    }

    #[test]
    fn relogin_disables_automatic_selection_and_opens_native_steam_without_credentials() {
        let dir = fixture();
        let runtime = MockRuntime::shutting_down();
        let result = lifecycle(dir.path(), Lifecycle::Relogin, &runtime).unwrap();
        assert!(result.launch_requested && result.requires_native_login);
        assert_eq!(result.process_id, Some(22));
        assert_eq!(runtime.selected.borrow().as_deref(), Some(""));
        assert_eq!(
            *runtime.events.borrow(),
            vec!["shutdown", "wait", "select", "launch"]
        );
        let doc = read_login_document(dir.path()).unwrap().unwrap();
        assert!(doc
            .accounts()
            .unwrap()
            .iter()
            .all(|account| !account.can_auto_login));
        let backup = fs::read_to_string(result.backup_path.unwrap()).unwrap();
        assert!(!backup.contains("PRIVATE_FIXTURE"));
        assert!(!backup.contains("RefreshToken"));
        assert!(doc.text.contains("PRIVATE_FIXTURE_AUTH_VALUE"));
    }

    #[test]
    fn saved_account_switch_keeps_native_auto_login_choice_and_authentication_unchanged() {
        let dir = fixture();
        let runtime = MockRuntime::stopped();
        let result = lifecycle(dir.path(), Lifecycle::Switch(SECOND.into()), &runtime).unwrap();
        assert_eq!(result.selected_steam_id.as_deref(), Some(SECOND));
        assert!(result.requires_native_login);
        assert_eq!(runtime.selected.borrow().as_deref(), Some("beta_user"));
        let doc = read_login_document(dir.path()).unwrap().unwrap();
        let account = doc
            .accounts
            .iter()
            .find(|account| account.steam_id == SECOND)
            .unwrap();
        assert_eq!(
            doc.field(account, "MostRecent").unwrap().as_deref(),
            Some("1")
        );
        assert_eq!(
            doc.field(account, "AllowAutoLogin").unwrap().as_deref(),
            Some("0")
        );
        assert!(doc.text.contains("PRIVATE_FIXTURE_AUTH_VALUE"));
    }

    #[test]
    fn unavailable_selected_account_is_rejected_before_shutdown_or_writes() {
        let dir = fixture();
        let runtime = MockRuntime::shutting_down();
        assert!(lifecycle(
            dir.path(),
            Lifecycle::Switch("76561197960265899".into()),
            &runtime
        )
        .is_err());
        assert!(runtime.events.borrow().is_empty());
        assert_eq!(
            fs::read(dir.path().join("config/loginusers.vdf")).unwrap(),
            SOURCE.as_bytes()
        );
    }

    #[test]
    fn launch_failure_restores_exact_vdf_bytes_and_missing_registry_selection() {
        let dir = fixture();
        let runtime = MockRuntime::stopped();
        *runtime.selected.borrow_mut() = None;
        runtime.launch_fails.set(true);
        let error = lifecycle(dir.path(), Lifecycle::Relogin, &runtime).unwrap_err();
        assert!(error.to_string().contains("restored"));
        assert!(runtime.selected.borrow().is_none());
        assert_eq!(
            fs::read(dir.path().join("config/loginusers.vdf")).unwrap(),
            SOURCE.as_bytes()
        );
        assert!(!dir.path().join(".steamtools-next-kernel.lock").exists());
    }

    #[test]
    fn registry_write_failure_restores_preferences_before_reporting_error() {
        let dir = fixture();
        let runtime = MockRuntime::stopped();
        runtime.selection_failures.set(1);
        assert!(lifecycle(dir.path(), Lifecycle::Relogin, &runtime).is_err());
        assert!(!runtime
            .events
            .borrow()
            .iter()
            .any(|event| event == "launch"));
        assert_eq!(
            fs::read(dir.path().join("config/loginusers.vdf")).unwrap(),
            SOURCE.as_bytes()
        );
        assert_eq!(runtime.selected.borrow().as_deref(), Some("alpha_user"));
    }

    #[test]
    fn rollback_preserves_an_external_rewrite_instead_of_overwriting_it() {
        let dir = fixture();
        let runtime = MockRuntime::stopped();
        runtime.launch_fails.set(true);
        runtime.external_change_on_launch.set(true);
        let error = lifecycle(dir.path(), Lifecycle::Relogin, &runtime).unwrap_err();
        assert!(format!("{error:#}").contains("changed externally"));
        assert_eq!(
            fs::read(dir.path().join("config/loginusers.vdf")).unwrap(),
            b"external preferences"
        );
        assert_eq!(runtime.selected.borrow().as_deref(), Some("alpha_user"));
    }

    #[test]
    fn malformed_metadata_and_invalid_executable_never_reach_shutdown() {
        let dir = fixture();
        let runtime = MockRuntime::shutting_down();
        fs::write(
            dir.path().join("config/loginusers.vdf"),
            b"\"users\" { \"PRIVATE_FIXTURE\"",
        )
        .unwrap();
        let error = lifecycle(dir.path(), Lifecycle::Relogin, &runtime).unwrap_err();
        assert!(!format!("{error:#}").contains("PRIVATE_FIXTURE"));
        assert!(runtime.events.borrow().is_empty());
        fs::write(dir.path().join("steam.exe"), b"not an executable").unwrap();
        assert!(lifecycle(dir.path(), Lifecycle::Restart, &runtime).is_err());
        assert!(runtime.events.borrow().is_empty());
    }

    #[test]
    fn duplicate_ids_duplicate_fields_and_invalid_boolean_values_are_rejected() {
        let duplicate = format!("\"users\" {{ \"{FIRST}\" {{ }} \"{FIRST}\" {{ }} }}");
        assert!(LoginDocument::parse(duplicate.into_bytes()).is_err());
        let duplicate = format!(
            "\"users\" {{ \"{FIRST}\" {{ \"AccountName\" \"a\" \"accountname\" \"b\" }} }}"
        );
        assert!(LoginDocument::parse(duplicate.into_bytes()).is_err());
        let invalid = SOURCE.replace(
            "\"AllowAutoLogin\" \"1\"",
            "\"AllowAutoLogin\" \"INVALID_PRIVATE_VALUE\"",
        );
        let error = LoginDocument::parse(invalid.into_bytes())
            .unwrap()
            .accounts()
            .unwrap_err();
        assert!(!format!("{error:#}").contains("INVALID_PRIVATE_VALUE"));
    }

    #[test]
    fn operation_guard_blocks_live_operations_and_recovers_only_crashed_account_guards() {
        let dir = fixture();
        let first = SteamOperationGuard::acquire(dir.path()).unwrap();
        assert!(SteamOperationGuard::acquire(dir.path()).is_err());
        drop(first);
        let path = dir.path().join(".steamtools-next-kernel.lock");
        fs::write(&path, b"accounts-finished-fixture").unwrap();
        let recovered = SteamOperationGuard::acquire(dir.path()).unwrap();
        drop(recovered);
        assert!(!path.exists());
        fs::write(&path, b"install-existing-kernel-operation").unwrap();
        assert!(SteamOperationGuard::acquire(dir.path()).is_err());
        assert_eq!(
            fs::read(path).unwrap(),
            b"install-existing-kernel-operation"
        );
    }

    #[test]
    fn missing_saved_accounts_still_allow_steam_owned_native_login() {
        let dir = fixture();
        fs::remove_file(dir.path().join("config/loginusers.vdf")).unwrap();
        let runtime = MockRuntime::stopped();
        let result = lifecycle(dir.path(), Lifecycle::Relogin, &runtime).unwrap();
        assert!(result.requires_native_login && result.launch_requested);
        assert_eq!(runtime.selected.borrow().as_deref(), Some(""));
        assert!(!dir.path().join("config/loginusers.vdf").exists());
    }
}

#[cfg(not(windows))]
impl SteamRuntime for NativeSteamRuntime {
    fn running_pids(&self, _executable: &Path) -> Result<Vec<u32>> {
        bail!("Steam account lifecycle management currently requires Windows")
    }
    fn active_account_id(&self, _root: &Path, _pids: &[u32]) -> Result<Option<u32>> {
        Ok(None)
    }
    fn login_selection(&self, _root: &Path) -> Result<LoginSelection> {
        bail!("Steam account selection currently requires Windows")
    }
    fn set_login_selection(&self, _root: &Path, _value: Option<&str>) -> Result<()> {
        bail!("Steam account selection currently requires Windows")
    }
    fn request_shutdown(&self, _executable: &Path, _root: &Path) -> Result<()> {
        bail!("Steam account lifecycle management currently requires Windows")
    }
    fn launch(&self, _executable: &Path, _root: &Path) -> Result<u32> {
        bail!("Steam account lifecycle management currently requires Windows")
    }
}
