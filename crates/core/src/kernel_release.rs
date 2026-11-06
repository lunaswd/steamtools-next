//! Verified remote kernel releases. No workspace or bundled artifact fallback.
//!
//! The provider's latest-download route supplies a small manifest. Its tag pins
//! the ZIP to the same release even if a new release appears during download.
//! SHA-256 detects corruption; authenticity relies on HTTPS and repository
//! ownership, not on a checksum downloaded alongside the package.

use anyhow::{bail, Context, Result};
use reqwest::{blocking::Client, redirect::Policy, Url};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub const DEFAULT_KERNEL_RELEASE_SOURCE: &str = "https://github.com/lunaswd/steamtools-next-kernel";
const MANIFEST_NAME: &str = "kernel-release.json";
const CACHE_RECORD: &str = ".remote-release.json";
const MAX_MANIFEST_BYTES: u64 = 512 * 1024;
const MAX_ARCHIVE_BYTES: u64 = 128 * 1024 * 1024;
const MAX_UNPACKED_BYTES: u64 = 256 * 1024 * 1024;
const MAX_FILES: usize = 64;
const REQUIRED_FILES: &[&str] = &[
    "OpenSteamTool.dll",
    "xinput1_4.dll",
    "dwmapi.dll",
    "version.dll",
    "opensteamtool.example.toml",
    "LICENSE",
    "BUILD_INFO.json",
];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct KernelReleaseArtifact {
    pub source_url: String,
    pub release_tag: String,
    pub version: String,
    pub source_commit: String,
    pub asset_name: String,
    pub download_url: String,
    pub archive_sha256: String,
    pub artifact_sha256: String,
    pub artifact_root: String,
    pub cache_hit: bool,
    #[serde(skip)]
    manifest_sha256: String,
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
    sha256: String,
    size_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ReleaseFile {
    name: String,
    sha256: String,
    size_bytes: u64,
}

#[derive(Debug, Clone)]
struct ReleaseSource {
    repository: String,
}

impl ReleaseSource {
    fn parse(raw: &str) -> Result<Self> {
        let mut url = Url::parse(raw.trim()).context("Kernel source must be a repository URL")?;
        if url.scheme() != "https"
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || url.port().is_some()
        {
            bail!("Kernel source must be an HTTPS repository URL without credentials or query");
        }
        if url.host_str() != Some("github.com") {
            bail!("Kernel source must be a github.com repository");
        }
        let path = url.path().trim_matches('/').trim_end_matches(".git");
        let segments: Vec<_> = path.split('/').collect();
        if segments.len() != 2 || segments.iter().any(|part| !safe_component(part)) {
            bail!("Kernel source has an invalid repository path");
        }
        let path = format!("/{}", segments.join("/"));
        url.set_path(&path);
        Ok(Self {
            repository: url.as_str().trim_end_matches('/').into(),
        })
    }

    fn manifest_url(&self) -> String {
        format!(
            "{}/releases/latest/download/{MANIFEST_NAME}",
            self.repository
        )
    }

    fn asset_url(&self, manifest: &ReleaseManifest) -> String {
        format!(
            "{}/releases/download/{}/{}",
            self.repository, manifest.release_tag, manifest.asset.name
        )
    }
}

/// Validate the persisted repository setting, without making a network request.
pub fn validate_kernel_release_source(source: &str) -> Result<()> {
    ReleaseSource::parse(source).map(|_| ())
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KernelUpdateInfo {
    pub installed: bool,
    pub current_version: Option<String>,
    pub current_sha256: Option<String>,
    pub latest_version: String,
    pub latest_sha256: String,
    pub release_tag: String,
    pub update_available: bool,
}

/// Fixed-source metadata check only: no package download, Steam writes, account
/// data, or game-library information is sent to the release provider.
pub fn check_kernel_update(steam_root: Option<&Path>) -> Result<KernelUpdateInfo> {
    let source = ReleaseSource::parse(DEFAULT_KERNEL_RELEASE_SOURCE)?;
    let bytes = download_manifest(&release_client()?, &source.manifest_url())?;
    let manifest: ReleaseManifest =
        serde_json::from_slice(&bytes).context("Kernel release manifest is not valid JSON")?;
    validate_manifest(&manifest)?;
    let state = steam_root
        .map(crate::kernel::detect_kernel_installation)
        .transpose()?;
    let installed = state
        .as_ref()
        .is_some_and(|state| state.status != "notInstalled");
    let current_version = state.as_ref().and_then(|state| state.version.clone());
    let current_sha256 = state
        .as_ref()
        .and_then(|state| state.installed_sha256.clone());
    let latest_sha256 = manifest
        .files
        .iter()
        .find(|file| file.name == "OpenSteamTool.dll")
        .context("Kernel release lacks its primary DLL identity")?
        .sha256
        .clone();
    let update_available = installed
        && kernel_update_needed(
            current_version.as_deref(),
            current_sha256.as_deref(),
            &manifest.version,
            &latest_sha256,
        );
    Ok(KernelUpdateInfo {
        installed,
        current_version,
        current_sha256,
        latest_version: manifest.version,
        latest_sha256,
        release_tag: manifest.release_tag,
        update_available,
    })
}

fn kernel_update_needed(
    current_version: Option<&str>,
    current_hash: Option<&str>,
    latest_version: &str,
    latest_hash: &str,
) -> bool {
    match current_version {
        Some(current) => {
            if crate::tool_update::is_newer_release_version(latest_version, current)
                .unwrap_or(false)
            {
                return true;
            }
            if crate::tool_update::is_newer_release_version(current, latest_version)
                .unwrap_or(false)
            {
                return false;
            }
            current_hash != Some(latest_hash)
        }
        None => current_hash != Some(latest_hash),
    }
}

/// Acquire a release into an application-owned, content-addressed cache.
/// Always fetch current release metadata first; a network failure never silently
/// selects an old cached release or a DLL beside the executable/workspace.
pub fn acquire_kernel_release(source: &str, cache_root: &Path) -> Result<KernelReleaseArtifact> {
    let source = ReleaseSource::parse(source)?;
    let client = release_client()?;
    let bytes = download_manifest(&client, &source.manifest_url())?;
    let manifest: ReleaseManifest =
        serde_json::from_slice(&bytes).context("Kernel release manifest is not valid JSON")?;
    validate_manifest(&manifest)?;
    acquire_manifest(&source, &client, cache_root, &manifest)
}

/// Plan against the exact manifest captured by remote acquisition, not just
/// whatever currently resides in the cache directory.
pub fn plan_kernel_release_install(
    request: &crate::kernel_manage::KernelInstallRequest,
    release: &KernelReleaseArtifact,
) -> Result<crate::kernel_manage::KernelOperationPlan> {
    let (_lock, manifest) = lock_verified_release(request, release)?;
    let plan = crate::kernel_manage::plan_kernel_install(request)?;
    verify_release_plan(&plan, release, &manifest)?;
    Ok(plan)
}

/// Hold the release's OS lock through verification, planning and the existing
/// transactional executor. The executor stages the very same verified plan and
/// rejects source changes against its recorded hashes; it does not replan.
pub fn install_kernel_release(
    request: &crate::kernel_manage::KernelInstallRequest,
    release: &KernelReleaseArtifact,
) -> Result<crate::kernel_manage::KernelOperationReceipt> {
    let (_lock, manifest) = lock_verified_release(request, release)?;
    let plan = crate::kernel_manage::plan_kernel_install(request)?;
    verify_release_plan(&plan, release, &manifest)?;
    if request.dry_run {
        Ok(crate::kernel_manage::dry_run_receipt(plan))
    } else {
        crate::kernel_manage::execute_install_plan(plan)
    }
}

fn lock_verified_release(
    request: &crate::kernel_manage::KernelInstallRequest,
    release: &KernelReleaseArtifact,
) -> Result<(CacheLock, ReleaseManifest)> {
    let root = crate::kernel_manage::absolute_local_path(
        Path::new(&release.artifact_root),
        "remote kernel artifact",
    )?;
    if fs::canonicalize(&request.artifact_root)? != fs::canonicalize(&root)? {
        bail!("Kernel request artifact directory does not match its remote release");
    }
    let source = ReleaseSource::parse(&release.source_url)?;
    let source_hash = sha256(source.repository.as_bytes());
    let key = format!("{}-{}", &source_hash[..16], release.archive_sha256);
    if root.file_name().and_then(|name| name.to_str()) != Some(key.as_str()) {
        bail!("Kernel cache directory does not match its remote release identity");
    }
    let parent = root
        .parent()
        .context("Kernel cache has no parent directory")?;
    let lock = CacheLock::acquire(&parent.join(format!(".{key}.lock")))?;
    let bytes = read_regular_limited(&root.join(CACHE_RECORD), MAX_MANIFEST_BYTES)?;
    let manifest: ReleaseManifest = serde_json::from_slice(&bytes)?;
    validate_manifest(&manifest)?;
    if sha256(&serde_json::to_vec(&manifest)?) != release.manifest_sha256
        || manifest.version != release.version
        || manifest.release_tag != release.release_tag
        || manifest.source_commit != release.source_commit
        || manifest.asset.sha256 != release.archive_sha256
    {
        bail!("Kernel cached manifest changed after remote acquisition");
    }
    validate_cache(&root, &manifest)?;
    Ok((lock, manifest))
}

fn verify_release_plan(
    plan: &crate::kernel_manage::KernelOperationPlan,
    release: &KernelReleaseArtifact,
    manifest: &ReleaseManifest,
) -> Result<()> {
    if plan.artifact_version.as_deref() != Some(release.version.as_str())
        || plan.artifact_sha256.as_deref() != Some(release.artifact_sha256.as_str())
    {
        bail!("Kernel install plan version or DLL identity does not match its remote release");
    }
    let root = fs::canonicalize(&release.artifact_root)?;
    for action in &plan.actions {
        let Some(source) = &action.source else {
            continue;
        };
        let source = fs::canonicalize(source)?;
        if source.parent() != Some(root.as_path()) {
            bail!("Kernel install source escaped the verified remote cache");
        }
        let name = source
            .file_name()
            .and_then(|name| name.to_str())
            .context("Kernel source name is invalid")?;
        let expected = manifest
            .files
            .iter()
            .find(|file| file.name == name)
            .context("Kernel install source is absent from the remote release manifest")?;
        if action.sha256.as_deref() != Some(expected.sha256.as_str())
            || action.size_bytes != Some(expected.size_bytes)
        {
            bail!("Kernel install plan source changed after remote verification");
        }
    }
    Ok(())
}

fn validate_release_dll(path: &Path) -> Result<()> {
    use std::io::{Seek, SeekFrom};
    crate::kernel_manage::validate_pe_x64(path)?;
    let mut file = File::open(path)?;
    let length = file.metadata()?.len();
    file.seek(SeekFrom::Start(60))?;
    let mut offset = [0; 4];
    file.read_exact(&mut offset)?;
    let offset = u32::from_le_bytes(offset) as u64;
    file.seek(SeekFrom::Start(offset))?;
    let mut coff = [0; 24];
    file.read_exact(&mut coff)
        .context("Kernel DLL has a truncated COFF header")?;
    let sections = u16::from_le_bytes([coff[6], coff[7]]) as u64;
    let optional_size = u16::from_le_bytes([coff[20], coff[21]]) as u64;
    let characteristics = u16::from_le_bytes([coff[22], coff[23]]);
    if characteristics & 0x2000 == 0
        || sections == 0
        || sections > 96
        || optional_size < 112
        || offset + 24 + optional_size + sections * 40 > length
    {
        bail!("Kernel artifact requires a complete PE32+ DLL header and section table");
    }
    let mut magic = [0; 2];
    file.read_exact(&mut magic)?;
    if u16::from_le_bytes(magic) != 0x20b {
        bail!("Kernel DLL is not a PE32+ image");
    }
    Ok(())
}

fn release_client() -> Result<Client> {
    Client::builder()
        .user_agent(concat!("SteamToolsNext/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(120))
        .redirect(Policy::custom(move |attempt| {
            if attempt.previous().len() >= 8 {
                return attempt.error("Kernel download exceeded the redirect limit");
            }
            if trusted_download_url(attempt.url()) {
                attempt.follow()
            } else {
                attempt.error("Kernel download redirected outside the release provider")
            }
        }))
        .build()
        .context("Could not initialize kernel release downloader")
}

fn trusted_download_url(url: &Url) -> bool {
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

fn download_manifest(client: &Client, url: &str) -> Result<Vec<u8>> {
    let mut response = client
        .get(url)
        .send()
        .context("Could not download kernel release manifest")?
        .error_for_status()
        .context("Kernel release manifest is unavailable; check the published release")?;
    if response
        .content_length()
        .is_some_and(|length| length > MAX_MANIFEST_BYTES)
    {
        bail!("Kernel release manifest exceeds the size limit");
    }
    let mut bytes = Vec::new();
    response
        .by_ref()
        .take(MAX_MANIFEST_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_MANIFEST_BYTES {
        bail!("Kernel release manifest exceeds the size limit");
    }
    Ok(bytes)
}

fn validate_manifest(manifest: &ReleaseManifest) -> Result<()> {
    if manifest.schema_version != 1 || manifest.platform != "windows-x64" {
        bail!("Kernel release manifest requires schemaVersion 1 and windows-x64");
    }
    let versions: Vec<_> = manifest.version.split('.').collect();
    if !(2..=4).contains(&versions.len())
        || versions
            .iter()
            .any(|part| part.is_empty() || part.len() > 5 || part.parse::<u16>().is_err())
        || manifest.release_tag != format!("v{}", manifest.version)
        || manifest.source_commit.len() != 40
        || !manifest
            .source_commit
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        bail!("Kernel release version, tag or source commit is invalid");
    }
    if manifest.asset.name
        != format!(
            "steamtools-next-kernel-{}-windows-x64.zip",
            manifest.version
        )
        || !valid_sha256(&manifest.asset.sha256)
        || manifest.asset.size_bytes == 0
        || manifest.asset.size_bytes > MAX_ARCHIVE_BYTES
        || manifest.files.is_empty()
        || manifest.files.len() > MAX_FILES
    {
        bail!("Kernel release ZIP identity or file count is invalid");
    }
    let mut seen = BTreeSet::new();
    let mut total = 0u64;
    for file in &manifest.files {
        if !safe_file_name(&file.name)
            || !seen.insert(file.name.to_ascii_lowercase())
            || !valid_sha256(&file.sha256)
        {
            bail!("Kernel release manifest contains unsafe, duplicate or invalid file entries");
        }
        total = total
            .checked_add(file.size_bytes)
            .context("Kernel file size overflow")?;
        if total > MAX_UNPACKED_BYTES {
            bail!("Kernel release exceeds the unpacked-size limit");
        }
    }
    for name in REQUIRED_FILES {
        if !manifest
            .files
            .iter()
            .any(|file| file.name == *name && file.size_bytes > 0)
        {
            bail!("Kernel release manifest is missing {name}");
        }
    }
    Ok(())
}

fn safe_component(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name != "."
        && name != ".."
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

fn safe_file_name(name: &str) -> bool {
    let base = name.split('.').next().unwrap_or("").to_ascii_uppercase();
    safe_component(name)
        && !name.ends_with('.')
        && name != CACHE_RECORD
        && name != MANIFEST_NAME
        && !matches!(
            base.as_str(),
            "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
        )
        && !(base.len() == 4
            && (base.starts_with("COM") || base.starts_with("LPT"))
            && base.as_bytes()[3].is_ascii_digit())
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn acquire_manifest(
    source: &ReleaseSource,
    client: &Client,
    cache_root: &Path,
    manifest: &ReleaseManifest,
) -> Result<KernelReleaseArtifact> {
    let root = crate::kernel_manage::absolute_local_path(cache_root, "kernel release cache")?;
    fs::create_dir_all(&root).context("Could not create kernel release cache")?;
    crate::kernel_manage::reject_link_ancestors(&root)?;
    let source_hash = sha256(source.repository.as_bytes());
    let key = format!("{}-{}", &source_hash[..16], manifest.asset.sha256);
    let target = root.join(&key);
    let _lock = CacheLock::acquire(&root.join(format!(".{key}.lock")))?;
    crate::kernel_manage::reject_link_ancestors(&target)?;
    if target.exists() {
        if validate_cache(&target, manifest).is_ok() {
            return Ok(artifact_result(source, manifest, &target, true));
        }
        // Preserve a damaged cache for inspection. Never remove user-modified DLLs.
        fs::rename(
            &target,
            root.join(format!(".invalid-{key}-{}", unique_stamp()?)),
        )
        .context("Could not quarantine invalid kernel release cache")?;
    }
    let stage = Stage::create(&root)?;
    let archive_path = stage.path.join(".download.zip");
    download_archive(
        client,
        &source.asset_url(manifest),
        &archive_path,
        &manifest.asset,
    )?;
    extract_archive(&archive_path, &stage.path, manifest)?;
    fs::remove_file(&archive_path)?;
    let record = serde_json::to_vec_pretty(manifest)?;
    write_new(&stage.path.join(CACHE_RECORD), &record)?;
    validate_cache(&stage.path, manifest)?;
    crate::kernel_manage::reject_link_ancestors(&target)?;
    fs::rename(&stage.path, &target)
        .context("Could not atomically complete kernel release cache")?;
    Ok(artifact_result(source, manifest, &target, false))
}

fn artifact_result(
    source: &ReleaseSource,
    manifest: &ReleaseManifest,
    target: &Path,
    cache_hit: bool,
) -> KernelReleaseArtifact {
    KernelReleaseArtifact {
        source_url: source.repository.clone(),
        release_tag: manifest.release_tag.clone(),
        version: manifest.version.clone(),
        source_commit: manifest.source_commit.clone(),
        asset_name: manifest.asset.name.clone(),
        download_url: source.asset_url(manifest),
        archive_sha256: manifest.asset.sha256.clone(),
        artifact_sha256: manifest
            .files
            .iter()
            .find(|file| file.name == "OpenSteamTool.dll")
            .unwrap()
            .sha256
            .clone(),
        artifact_root: target.to_string_lossy().into_owned(),
        cache_hit,
        manifest_sha256: sha256(
            &serde_json::to_vec(manifest).expect("serializable release manifest"),
        ),
    }
}

fn download_archive(client: &Client, url: &str, path: &Path, asset: &ReleaseAsset) -> Result<()> {
    let mut response = client
        .get(url)
        .send()
        .context("Could not download kernel release ZIP")?
        .error_for_status()
        .context("Kernel release ZIP is unavailable")?;
    if response
        .content_length()
        .is_some_and(|size| size != asset.size_bytes || size > MAX_ARCHIVE_BYTES)
    {
        bail!("Kernel release ZIP Content-Length does not match its manifest");
    }
    let mut file = OpenOptions::new().create_new(true).write(true).open(path)?;
    let mut hasher = Sha256::new();
    let mut count = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let size = response
            .read(&mut buffer)
            .context("Kernel release ZIP download was interrupted")?;
        if size == 0 {
            break;
        }
        count += size as u64;
        if count > asset.size_bytes || count > MAX_ARCHIVE_BYTES {
            bail!("Kernel release ZIP exceeds its declared size");
        }
        hasher.update(&buffer[..size]);
        file.write_all(&buffer[..size])?;
    }
    file.sync_all()?;
    if count != asset.size_bytes || format!("{:x}", hasher.finalize()) != asset.sha256 {
        bail!("Kernel release ZIP failed its size or SHA-256 verification");
    }
    Ok(())
}

fn extract_archive(path: &Path, stage: &Path, manifest: &ReleaseManifest) -> Result<()> {
    let mut archive =
        zip::ZipArchive::new(File::open(path)?).context("Kernel release ZIP is invalid")?;
    if archive.len() != manifest.files.len() || archive.len() > MAX_FILES {
        bail!("Kernel release ZIP file count does not match its manifest");
    }
    let mut seen = BTreeSet::new();
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .context("Could not inspect kernel ZIP entry")?;
        let name = entry.name().to_string();
        let mode = entry.unix_mode().map(|mode| mode & 0o170000);
        if !safe_file_name(&name)
            || entry.is_dir()
            || mode.is_some_and(|kind| kind != 0 && kind != 0o100000)
            || !seen.insert(name.to_ascii_lowercase())
        {
            bail!("Kernel release ZIP contains unsafe paths, links, directories or duplicates");
        }
        let expected = manifest
            .files
            .iter()
            .find(|file| file.name == name)
            .context("Kernel release ZIP contains a file absent from its manifest")?;
        if entry.size() != expected.size_bytes {
            bail!("Kernel release ZIP entry size does not match its manifest");
        }
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(stage.join(&name))?;
        let mut hasher = Sha256::new();
        let mut count = 0u64;
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let size = entry
                .read(&mut buffer)
                .context("Kernel release ZIP entry is damaged")?;
            if size == 0 {
                break;
            }
            count += size as u64;
            if count > expected.size_bytes {
                bail!("Kernel ZIP entry exceeded its declared size");
            }
            hasher.update(&buffer[..size]);
            file.write_all(&buffer[..size])?;
        }
        file.sync_all()?;
        if count != expected.size_bytes || format!("{:x}", hasher.finalize()) != expected.sha256 {
            bail!("Kernel release ZIP entry failed SHA-256 verification");
        }
    }
    Ok(())
}

fn validate_cache(root: &Path, manifest: &ReleaseManifest) -> Result<()> {
    crate::kernel_manage::reject_link_ancestors(root)?;
    if !fs::symlink_metadata(root)?.is_dir() {
        bail!("Kernel release cache is not a directory");
    }
    let record = read_regular_limited(&root.join(CACHE_RECORD), MAX_MANIFEST_BYTES)?;
    let saved: ReleaseManifest =
        serde_json::from_slice(&record).context("Kernel cache record is invalid")?;
    if saved != *manifest {
        bail!("Kernel cache record does not match the current release manifest");
    }
    let entries = fs::read_dir(root)?.collect::<std::io::Result<Vec<_>>>()?;
    if entries.len() != manifest.files.len() + 1 {
        bail!("Kernel cache contains unexpected or missing files");
    }
    for expected in &manifest.files {
        let path = root.join(&expected.name);
        crate::kernel_manage::reject_link_ancestors(&path)?;
        let metadata = fs::symlink_metadata(&path)?;
        if !metadata.is_file() || metadata.len() != expected.size_bytes {
            bail!("Kernel cache file does not match its declared size");
        }
        let mut file = File::open(&path)?;
        let mut hasher = Sha256::new();
        let mut buffer = [0u8; 64 * 1024];
        let mut count = 0u64;
        loop {
            let size = file.read(&mut buffer)?;
            if size == 0 {
                break;
            }
            count += size as u64;
            if count > expected.size_bytes {
                bail!("Kernel cache file changed during verification");
            }
            hasher.update(&buffer[..size]);
        }
        if count != expected.size_bytes || format!("{:x}", hasher.finalize()) != expected.sha256 {
            bail!("Kernel cache file failed SHA-256 verification");
        }
        if expected.name.ends_with(".dll") || expected.name.ends_with(".dll.disabled") {
            validate_release_dll(&path)?;
        }
    }
    let build = read_regular_limited(&root.join("BUILD_INFO.json"), MAX_MANIFEST_BYTES)?;
    let build: serde_json::Value =
        serde_json::from_slice(&build).context("Kernel BUILD_INFO is not valid JSON")?;
    let primary = manifest
        .files
        .iter()
        .find(|file| file.name == "OpenSteamTool.dll")
        .unwrap();
    let primary_build = build
        .get("files")
        .and_then(serde_json::Value::as_array)
        .and_then(|files| {
            files.iter().find(|file| {
                file.get("name").and_then(serde_json::Value::as_str) == Some("OpenSteamTool.dll")
            })
        });
    if build.get("version").and_then(serde_json::Value::as_str) != Some(manifest.version.as_str())
        || build
            .get("sourceCommit")
            .and_then(serde_json::Value::as_str)
            != Some(manifest.source_commit.as_str())
        || build.get("platform").and_then(serde_json::Value::as_str) != Some("windows-x64")
        || primary_build
            .and_then(|file| file.get("sha256"))
            .and_then(serde_json::Value::as_str)
            != Some(primary.sha256.as_str())
    {
        bail!("Kernel BUILD_INFO does not match its release version, commit or DLL hash");
    }
    let config =
        read_regular_limited(&root.join("opensteamtool.example.toml"), MAX_MANIFEST_BYTES)?;
    let config = std::str::from_utf8(&config).context("Kernel example config is not UTF-8")?;
    let _: toml::Value = config
        .parse()
        .context("Kernel example config is not valid TOML")?;
    Ok(())
}

fn read_regular_limited(path: &Path, limit: u64) -> Result<Vec<u8>> {
    crate::kernel_manage::reject_link_ancestors(path)?;
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.len() > limit {
        bail!("Kernel metadata is not a bounded regular file");
    }
    let mut result = Vec::new();
    File::open(path)?.take(limit + 1).read_to_end(&mut result)?;
    if result.len() as u64 > limit {
        bail!("Kernel metadata exceeds the size limit");
    }
    Ok(result)
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new().create_new(true).write(true).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn unique_stamp() -> Result<String> {
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    Ok(format!("{}-{stamp}", std::process::id()))
}

struct CacheLock {
    _file: File,
}
impl CacheLock {
    fn acquire(path: &Path) -> Result<Self> {
        crate::kernel_manage::reject_link_ancestors(path)?;
        // Retain a stable inode. OS locks release on all forms of process exit,
        // including forced termination, so stale lock files are reusable.
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;
        if !file.metadata()?.is_file() {
            bail!("Kernel cache lock is not a regular file");
        }
        file.try_lock()
            .context("Another kernel release operation is already in progress")?;
        Ok(Self { _file: file })
    }
}

struct Stage {
    path: PathBuf,
}
impl Stage {
    fn create(root: &Path) -> Result<Self> {
        for attempt in 0..20 {
            let path = root.join(format!(".staging-{}-{attempt}", unique_stamp()?));
            match fs::create_dir(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(error).context("Could not create kernel release staging directory")
                }
            }
        }
        bail!("Could not allocate kernel release staging directory")
    }
}
impl Drop for Stage {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use zip::{write::SimpleFileOptions, ZipWriter};

    fn pe() -> Vec<u8> {
        let mut bytes = vec![0u8; 512];
        bytes[..2].copy_from_slice(b"MZ");
        bytes[60..64].copy_from_slice(&128u32.to_le_bytes());
        bytes[128..132].copy_from_slice(b"PE\0\0");
        bytes[132..134].copy_from_slice(&0x8664u16.to_le_bytes());
        bytes[134..136].copy_from_slice(&1u16.to_le_bytes());
        bytes[148..150].copy_from_slice(&240u16.to_le_bytes());
        bytes[150..152].copy_from_slice(&0x2000u16.to_le_bytes());
        bytes[152..154].copy_from_slice(&0x020bu16.to_le_bytes());
        bytes
    }
    fn fixture() -> (ReleaseManifest, Vec<(String, Vec<u8>)>) {
        let files: Vec<_> = REQUIRED_FILES
            .iter()
            .map(|name| {
                (
                    (*name).to_string(),
                    if name.ends_with(".dll") {
                        pe()
                    } else if *name == "LICENSE" {
                        b"GPL-3.0".to_vec()
                    } else if *name == "BUILD_INFO.json" {
                        serde_json::to_vec(&serde_json::json!({"version":"1.5.4.1","sourceCommit":"a".repeat(40),"platform":"windows-x64","files":[{"name":"OpenSteamTool.dll","sha256":sha256(&pe())}]})).unwrap()
                    } else {
                        b"[lua]\npaths=[]\n".to_vec()
                    },
                )
            })
            .collect();
        let manifest = ReleaseManifest {
            schema_version: 1,
            version: "1.5.4.1".into(),
            release_tag: "v1.5.4.1".into(),
            source_commit: "a".repeat(40),
            platform: "windows-x64".into(),
            asset: ReleaseAsset {
                name: "steamtools-next-kernel-1.5.4.1-windows-x64.zip".into(),
                sha256: "b".repeat(64),
                size_bytes: 1,
            },
            files: files
                .iter()
                .map(|(name, data)| ReleaseFile {
                    name: name.clone(),
                    sha256: sha256(data),
                    size_bytes: data.len() as u64,
                })
                .collect(),
        };
        (manifest, files)
    }
    fn zip_bytes(files: &[(String, Vec<u8>)]) -> Vec<u8> {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        for (name, data) in files {
            writer
                .start_file(name, SimpleFileOptions::default())
                .unwrap();
            writer.write_all(data).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }
    fn complete_cache(root: &Path, manifest: &ReleaseManifest, files: &[(String, Vec<u8>)]) {
        for (name, bytes) in files {
            fs::write(root.join(name), bytes).unwrap();
        }
        fs::write(
            root.join(CACHE_RECORD),
            serde_json::to_vec(manifest).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn repository_urls_are_normalized_and_download_routes_are_pinned() {
        let github =
            ReleaseSource::parse("https://github.com/lunaswd/steamtools-next-kernel.git/").unwrap();
        assert_eq!(github.repository, DEFAULT_KERNEL_RELEASE_SOURCE);
        assert_eq!(
            github.manifest_url(),
            format!("{DEFAULT_KERNEL_RELEASE_SOURCE}/releases/latest/download/kernel-release.json")
        );
        let (manifest, _) = fixture();
        assert_eq!(github.asset_url(&manifest), format!("{DEFAULT_KERNEL_RELEASE_SOURCE}/releases/download/v1.5.4.1/steamtools-next-kernel-1.5.4.1-windows-x64.zip"));
        assert!(ReleaseSource::parse("https://gitlab.com/group/subgroup/kernel").is_err());
    }

    #[test]
    fn repository_and_redirect_boundaries_reject_untrusted_inputs() {
        for bad in [
            "http://github.com/o/r",
            "https://user:secret@github.com/o/r",
            "https://github.com/o/r?token=secret",
            "https://github.com/o/r#foo",
            "https://github.com/o/r/extra",
            "https://example.com/o/r",
            "https://gitlab.com/o/r",
            "https://github.com/o/%2e%2e",
            "C:\\local\\kernel",
        ] {
            assert!(ReleaseSource::parse(bad).is_err(), "{bad}");
        }
        for bad in [
            "http://github.com/a",
            "https://127.0.0.1/a",
            "https://github.com.evil.test/a",
            "https://gitlab.com/o/r",
            "https://release-assets.githubusercontent.com:444/a",
        ] {
            assert!(!trusted_download_url(&Url::parse(bad).unwrap()));
        }
    }

    #[test]
    fn manifest_rejects_wrong_platform_hashes_missing_files_and_ambiguous_names() {
        let (good, _) = fixture();
        validate_manifest(&good).unwrap();
        let mut bad = good.clone();
        bad.platform = "windows-x86".into();
        assert!(validate_manifest(&bad).is_err());
        let mut bad = good.clone();
        bad.release_tag = "../../latest".into();
        assert!(validate_manifest(&bad).is_err());
        let mut bad = good.clone();
        bad.asset.sha256 = "x".repeat(64);
        assert!(validate_manifest(&bad).is_err());
        let mut bad = good.clone();
        bad.files.pop();
        assert!(validate_manifest(&bad).is_err());
        for name in [
            "../evil.dll",
            "folder/evil.dll",
            "folder\\evil.dll",
            "C:evil.dll",
            "CON.txt",
            "lpt1.dll",
            "file.",
            "OpenSteamTool.dll",
        ] {
            let mut bad = good.clone();
            bad.files.push(ReleaseFile {
                name: name.into(),
                sha256: "b".repeat(64),
                size_bytes: 1,
            });
            assert!(validate_manifest(&bad).is_err(), "{name}");
        }
    }

    #[test]
    fn extraction_and_cache_verification_detect_tampering() {
        let temp = tempfile::tempdir().unwrap();
        let (manifest, files) = fixture();
        let archive = temp.path().join(".download.zip");
        fs::write(&archive, zip_bytes(&files)).unwrap();
        extract_archive(&archive, temp.path(), &manifest).unwrap();
        fs::remove_file(&archive).unwrap();
        fs::write(
            temp.path().join(CACHE_RECORD),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        validate_cache(temp.path(), &manifest).unwrap();
        fs::write(temp.path().join("OpenSteamTool.dll"), vec![1; 512]).unwrap();
        assert!(validate_cache(temp.path(), &manifest).is_err());
    }

    #[test]
    fn cache_rejects_extra_files_corrupt_record_invalid_pe_and_config() {
        let temp = tempfile::tempdir().unwrap();
        let (mut manifest, mut files) = fixture();
        complete_cache(temp.path(), &manifest, &files);
        validate_cache(temp.path(), &manifest).unwrap();
        fs::write(temp.path().join("extra.dll"), pe()).unwrap();
        assert!(validate_cache(temp.path(), &manifest).is_err());
        fs::remove_file(temp.path().join("extra.dll")).unwrap();
        fs::write(temp.path().join(CACHE_RECORD), b"{}").unwrap();
        assert!(validate_cache(temp.path(), &manifest).is_err());
        files[0].1 = vec![0; 512];
        manifest.files[0].sha256 = sha256(&files[0].1);
        complete_cache(temp.path(), &manifest, &files);
        assert!(validate_cache(temp.path(), &manifest).is_err());
        files[0].1 = pe();
        manifest.files[0].sha256 = sha256(&files[0].1);
        let index = files
            .iter()
            .position(|(name, _)| name == "opensteamtool.example.toml")
            .unwrap();
        files[index].1 = b"[broken".to_vec();
        manifest.files[index].size_bytes = files[index].1.len() as u64;
        manifest.files[index].sha256 = sha256(&files[index].1);
        complete_cache(temp.path(), &manifest, &files);
        assert!(validate_cache(temp.path(), &manifest).is_err());
    }

    #[test]
    fn zip_rejects_traversal_unknown_files_duplicates_and_corrupt_file_hash() {
        let (manifest, files) = fixture();
        for name in ["../evil.dll", "evil.dll", "OPENSTEAMTOOL.DLL"] {
            let temp = tempfile::tempdir().unwrap();
            let mut altered = files.clone();
            altered[0].0 = name.into();
            let path = temp.path().join(".download.zip");
            fs::write(&path, zip_bytes(&altered)).unwrap();
            assert!(extract_archive(&path, temp.path(), &manifest).is_err());
        }
        let temp = tempfile::tempdir().unwrap();
        let mut altered = files.clone();
        altered[0].1[0] ^= 1;
        let path = temp.path().join(".download.zip");
        fs::write(&path, zip_bytes(&altered)).unwrap();
        assert!(extract_archive(&path, temp.path(), &manifest).is_err());
    }

    #[test]
    fn invalid_source_creates_no_cache_and_cache_locks_clean_up() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("cache");
        assert!(acquire_kernel_release("https://evil.invalid/a/b", &path).is_err());
        assert!(!path.exists());
        let lock_path = temp.path().join("lock");
        let guard = CacheLock::acquire(&lock_path).unwrap();
        assert!(CacheLock::acquire(&lock_path).is_err());
        drop(guard);
        assert!(lock_path.exists());
        drop(CacheLock::acquire(&lock_path).unwrap());
        let path = {
            let stage = Stage::create(temp.path()).unwrap();
            stage.path.clone()
        };
        assert!(!path.exists());
    }

    #[test]
    fn archive_download_is_bounded_and_hash_checked_with_mock_http() {
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/asset.zip", listener.local_addr().unwrap());
        let thread = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buffer = [0; 1024];
            let _ = stream.read(&mut buffer);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\ndata")
                .unwrap();
        });
        let temp = tempfile::tempdir().unwrap();
        let client = Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();
        let asset = ReleaseAsset {
            name: "test.zip".into(),
            size_bytes: 4,
            sha256: "b".repeat(64),
        };
        let result = download_archive(&client, &url, &temp.path().join("archive"), &asset);
        thread.join().unwrap();
        assert!(result.unwrap_err().to_string().contains("SHA-256"));
    }

    #[test]
    fn verified_cache_is_reused_without_downloading_zip() {
        let temp = tempfile::tempdir().unwrap();
        let (manifest, files) = fixture();
        let source = ReleaseSource::parse(DEFAULT_KERNEL_RELEASE_SOURCE).unwrap();
        let source_hash = sha256(source.repository.as_bytes());
        let target = temp
            .path()
            .join(format!("{}-{}", &source_hash[..16], manifest.asset.sha256));
        fs::create_dir(&target).unwrap();
        complete_cache(&target, &manifest, &files);
        let result =
            acquire_manifest(&source, &release_client().unwrap(), temp.path(), &manifest).unwrap();
        assert!(result.cache_hit);
        assert_eq!(result.artifact_sha256, sha256(&pe()));
    }

    fn verified_fixture(
        temp: &Path,
    ) -> (
        crate::kernel_manage::KernelInstallRequest,
        KernelReleaseArtifact,
        ReleaseManifest,
    ) {
        let (manifest, files) = fixture();
        let source = ReleaseSource::parse(DEFAULT_KERNEL_RELEASE_SOURCE).unwrap();
        let cache = temp.join("cache");
        fs::create_dir(&cache).unwrap();
        let source_hash = sha256(source.repository.as_bytes());
        let target = cache.join(format!("{}-{}", &source_hash[..16], manifest.asset.sha256));
        fs::create_dir(&target).unwrap();
        complete_cache(&target, &manifest, &files);
        let release = artifact_result(&source, &manifest, &target, false);
        let steam = temp.join("steam");
        fs::create_dir(&steam).unwrap();
        for name in ["steam.exe", "steamclient64.dll", "steamui.dll"] {
            fs::write(steam.join(name), pe()).unwrap();
        }
        let request = crate::kernel_manage::KernelInstallRequest {
            steam_root: steam,
            artifact_root: target,
            injection_method: crate::kernel_manage::InjectionMethod::Dwmapi,
            install_example_config: true,
            backup_root: None,
            dry_run: false,
        };
        (request, release, manifest)
    }

    #[test]
    fn remote_install_rejects_modified_but_still_valid_dll_and_rewritten_manifest() {
        let temp = tempfile::tempdir().unwrap();
        let (request, release, mut manifest) = verified_fixture(temp.path());
        plan_kernel_release_install(&request, &release).unwrap();
        let mut modified = pe();
        modified[500] = 7; // still a valid x64 DLL
        fs::write(request.artifact_root.join("OpenSteamTool.dll"), &modified).unwrap();
        validate_release_dll(&request.artifact_root.join("OpenSteamTool.dll")).unwrap();
        assert!(install_kernel_release(&request, &release).is_err());
        assert!(!request.steam_root.join("OpenSteamTool.dll").exists());
        // Updating a local manifest to bless the changed DLL does not change the
        // manifest identity captured when the release came from the provider.
        manifest.files[0].sha256 = sha256(&modified);
        fs::write(
            request.artifact_root.join(CACHE_RECORD),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        assert!(install_kernel_release(&request, &release)
            .unwrap_err()
            .to_string()
            .contains("manifest changed"));
    }

    #[test]
    fn remote_install_is_transactional_and_repeat_install_preserves_hash_evidence() {
        let temp = tempfile::tempdir().unwrap();
        let (request, release, _) = verified_fixture(temp.path());
        let first = install_kernel_release(&request, &release).unwrap();
        assert!(first.changed);
        assert_eq!(first.resulting_state.hash_match, Some(true));
        assert_eq!(
            fs::read(request.steam_root.join("OpenSteamTool.dll")).unwrap(),
            pe()
        );
        let second = install_kernel_release(&request, &release).unwrap();
        assert!(!second.changed);
        assert_eq!(second.resulting_state.hash_match, Some(true));
        assert_eq!(
            second.resulting_state.artifact_version.as_deref(),
            Some("1.5.4.1")
        );
    }

    #[test]
    fn release_plan_rejects_a_source_hash_changed_between_verification_and_planning() {
        let temp = tempfile::tempdir().unwrap();
        let (request, release, manifest) = verified_fixture(temp.path());
        let mut plan = plan_kernel_release_install(&request, &release).unwrap();
        let action = plan
            .actions
            .iter_mut()
            .find(|action| {
                action
                    .source
                    .as_ref()
                    .is_some_and(|source| source.ends_with("opensteamtool.example.toml"))
            })
            .unwrap();
        action.sha256 = Some("f".repeat(64));
        assert!(verify_release_plan(&plan, &release, &manifest).is_err());
    }

    #[test]
    fn strict_remote_pe_validation_rejects_exe_pe32_and_truncated_headers() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("kernel.dll");
        fs::write(&path, pe()).unwrap();
        validate_release_dll(&path).unwrap();
        let mut exe = pe();
        exe[150..152].copy_from_slice(&0x0002u16.to_le_bytes());
        fs::write(&path, exe).unwrap();
        assert!(validate_release_dll(&path).is_err());
        let mut pe32 = pe();
        pe32[152..154].copy_from_slice(&0x010bu16.to_le_bytes());
        fs::write(&path, pe32).unwrap();
        assert!(validate_release_dll(&path).is_err());
        fs::write(&path, &pe()[..200]).unwrap();
        assert!(validate_release_dll(&path).is_err());
    }

    #[test]
    fn zip_links_and_case_folded_duplicate_names_are_rejected() {
        let (manifest, files) = fixture();
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(".download.zip");
        let mut duplicate = files.clone();
        duplicate[1].0 = "opensteamtool.dll".into();
        fs::write(&path, zip_bytes(&duplicate)).unwrap();
        assert!(extract_archive(&path, temp.path(), &manifest).is_err());
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(".download.zip");
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        writer
            .add_symlink(
                "OpenSteamTool.dll",
                "xinput1_4.dll",
                SimpleFileOptions::default(),
            )
            .unwrap();
        for (name, bytes) in files.iter().skip(1) {
            writer
                .start_file(name, SimpleFileOptions::default())
                .unwrap();
            writer.write_all(bytes).unwrap();
        }
        fs::write(&path, writer.finish().unwrap().into_inner()).unwrap();
        assert!(extract_archive(&path, temp.path(), &manifest).is_err());
    }

    #[test]
    fn os_lock_process_exit_helper() {
        if let Some(path) = std::env::var_os("STN_TEST_KERNEL_LOCK_EXIT") {
            let _guard = CacheLock::acquire(Path::new(&path)).unwrap();
            // Deliberately bypass Rust Drop handlers to model a force-closed app.
            std::process::exit(0);
        }
    }

    #[test]
    fn os_cache_lock_recovers_after_process_exit_without_drop() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("stale.lock");
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "kernel_release::tests::os_lock_process_exit_helper",
                "--nocapture",
            ])
            .env("STN_TEST_KERNEL_LOCK_EXIT", &path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(path.exists());
        CacheLock::acquire(&path).unwrap();
    }
}
