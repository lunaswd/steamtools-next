//! Bounded archive import for OpenSteamTool library payloads.
//!
//! Archive contents are extracted into an isolated temporary directory, with
//! paths flattened to their file names, and then committed through the
//! transactional library importer.  Only `.lua`, `.bin` and `.manifest`
//! regular files are accepted.  The first entry with a given flattened name
//! wins; later duplicates are reported and skipped.

use crate::library::{import_library_files, ImportIssue, ImportSummary, LibraryImportRequest};
use anyhow::{bail, Context, Result};
use flate2::read::GzDecoder;
use serde::{Deserialize, Serialize};
use std::ffi::OsStr;
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_ARCHIVE_FILES: usize = 10_000;
const MAX_ARCHIVE_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_ARCHIVES: usize = 128;
const COPY_BUFFER_BYTES: usize = 64 * 1024;
const REPARSE_POINT_ATTRIBUTE: u32 = 0x0400;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ArchiveFormat {
    Zip,
    Tar,
    TarGz,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ArchiveImportRequest {
    pub steam_root: PathBuf,
    pub archives: Vec<PathBuf>,
    #[serde(default)]
    pub overwrite: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ArchiveImportIssue {
    pub archive: String,
    pub entry: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ArchiveResult {
    pub archive: String,
    pub format: Option<ArchiveFormat>,
    pub extracted_files: usize,
    pub extracted_bytes: u64,
    pub success: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ArchiveImportSummary {
    pub archives: Vec<ArchiveResult>,
    pub extracted_files: usize,
    pub extracted_bytes: u64,
    pub skipped: Vec<ArchiveImportIssue>,
    pub failed: Vec<ArchiveImportIssue>,
    pub import: ImportSummary,
}

#[derive(Default)]
struct ExtractionBudget {
    files: usize,
    bytes: u64,
    seen_files: usize,
    seen_bytes: u64,
}

struct TemporaryWorkspace {
    path: PathBuf,
}

impl TemporaryWorkspace {
    fn create() -> Result<Self> {
        let root = std::env::temp_dir().join("SteamToolsNext");
        fs::create_dir_all(&root).context("could not create the archive staging root")?;
        reject_linked_ancestors(&root)?;
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("system clock precedes the Unix epoch")?
            .as_nanos();
        for attempt in 0..100u32 {
            let path = root.join(format!(
                "archive-import-{}-{stamp}-{attempt}",
                std::process::id()
            ));
            match fs::create_dir(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error).context("could not create archive staging area"),
            }
        }
        bail!("could not allocate a unique archive staging area")
    }
}

impl Drop for TemporaryWorkspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Import one or more ZIP/TAR/TAR.GZ archives through the shared library
/// transaction layer. Unsupported or invalid archives are reported per-file;
/// a valid archive can still be committed when another archive fails.
pub fn import_library_archives(request: &ArchiveImportRequest) -> Result<ArchiveImportSummary> {
    if request.archives.is_empty() {
        bail!("at least one archive is required");
    }
    if request.archives.len() > MAX_ARCHIVES {
        bail!("archive request exceeds the {MAX_ARCHIVES} file limit");
    }
    let workspace = TemporaryWorkspace::create()?;
    let mut budget = ExtractionBudget::default();
    let mut archives = Vec::new();
    let mut skipped = Vec::new();
    let mut failed = Vec::new();

    for archive in &request.archives {
        let archive_display = display_path(archive);
        let format = detect_archive_format(archive);
        let detected = format.as_ref().ok().copied();
        let before_files = budget.files;
        let before_bytes = budget.bytes;
        let before_seen_files = budget.seen_files;
        let before_seen_bytes = budget.seen_bytes;
        let before_names = staged_file_names(&workspace.path)?;
        let result = match format {
            Ok(ArchiveFormat::Zip) => {
                extract_zip(archive, &workspace.path, &mut budget, &mut skipped)
            }
            Ok(ArchiveFormat::Tar) => open_archive(archive).and_then(|file| {
                extract_tar(archive, file, &workspace.path, &mut budget, &mut skipped)
            }),
            Ok(ArchiveFormat::TarGz) => open_archive(archive).and_then(|file| {
                extract_tar(
                    archive,
                    GzDecoder::new(file),
                    &workspace.path,
                    &mut budget,
                    &mut skipped,
                )
            }),
            Err(error) => Err(error),
        };
        let (success, error) = match result {
            Ok(()) => (true, None),
            Err(error) => {
                rollback_staged_files(&workspace.path, &before_names)
                    .context("could not roll back a failed archive extraction")?;
                budget.files = before_files;
                budget.bytes = before_bytes;
                budget.seen_files = before_seen_files;
                budget.seen_bytes = before_seen_bytes;
                failed.push(ArchiveImportIssue {
                    archive: archive_display.clone(),
                    entry: String::new(),
                    reason: format!("{error:#}"),
                });
                (false, Some(format!("{error:#}")))
            }
        };
        archives.push(ArchiveResult {
            archive: archive_display,
            format: detected,
            extracted_files: budget.files.saturating_sub(before_files),
            extracted_bytes: budget.bytes.saturating_sub(before_bytes),
            success,
            error,
        });
    }

    let import = if budget.files == 0 {
        empty_import_summary()
    } else {
        import_library_files(&LibraryImportRequest {
            steam_root: request.steam_root.clone(),
            sources: vec![workspace.path.clone()],
            overwrite: request.overwrite,
        })?
    };
    Ok(ArchiveImportSummary {
        archives,
        extracted_files: budget.files,
        extracted_bytes: budget.bytes,
        skipped,
        failed,
        import,
    })
}

fn extract_zip(
    archive_path: &Path,
    staging: &Path,
    budget: &mut ExtractionBudget,
    skipped: &mut Vec<ArchiveImportIssue>,
) -> Result<()> {
    let file = open_archive(archive_path)?;
    let mut archive = zip::ZipArchive::new(file)
        .with_context(|| format!("invalid ZIP archive: {}", display_path(archive_path)))?;
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .with_context(|| format!("could not inspect ZIP entry {index}"))?;
        let raw_name = entry.name().to_string();
        if entry.is_dir() {
            continue;
        }
        observe_entry(budget, entry.size())?;
        if entry
            .unix_mode()
            .is_some_and(|mode| mode & 0o170000 == 0o120000)
        {
            skipped.push(issue(
                archive_path,
                &raw_name,
                "linked entries are not imported",
            ));
            continue;
        }
        let Some(file_name) = accepted_entry_name(&raw_name) else {
            skipped.push(issue(
                archive_path,
                &raw_name,
                "unsafe path or unsupported extension",
            ));
            continue;
        };
        stage_entry(
            archive_path,
            &raw_name,
            &file_name,
            entry.size(),
            &mut entry,
            staging,
            budget,
            skipped,
        )?;
    }
    Ok(())
}

fn extract_tar<R: Read>(
    archive_path: &Path,
    reader: R,
    staging: &Path,
    budget: &mut ExtractionBudget,
    skipped: &mut Vec<ArchiveImportIssue>,
) -> Result<()> {
    let mut archive = tar::Archive::new(reader);
    for entry in archive
        .entries()
        .with_context(|| format!("invalid TAR archive: {}", display_path(archive_path)))?
    {
        let mut entry = entry.context("could not inspect TAR entry")?;
        let raw_path = entry.path().context("TAR entry path is invalid")?;
        let raw_name = raw_path.to_string_lossy().into_owned();
        if !entry.header().entry_type().is_dir() {
            observe_entry(
                budget,
                entry.header().size().context("TAR entry size is invalid")?,
            )?;
        }
        if !entry.header().entry_type().is_file() {
            if !entry.header().entry_type().is_dir() {
                skipped.push(issue(
                    archive_path,
                    &raw_name,
                    "non-regular TAR entries are not imported",
                ));
            }
            continue;
        }
        let Some(file_name) = accepted_path_name(&raw_path) else {
            skipped.push(issue(
                archive_path,
                &raw_name,
                "unsafe path or unsupported extension",
            ));
            continue;
        };
        let declared_size = entry.header().size().context("TAR entry size is invalid")?;
        stage_entry(
            archive_path,
            &raw_name,
            &file_name,
            declared_size,
            &mut entry,
            staging,
            budget,
            skipped,
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn stage_entry<R: Read>(
    archive_path: &Path,
    raw_name: &str,
    file_name: &OsStr,
    declared_size: u64,
    reader: &mut R,
    staging: &Path,
    budget: &mut ExtractionBudget,
    skipped: &mut Vec<ArchiveImportIssue>,
) -> Result<()> {
    let destination = staging.join(file_name);
    let mut output = match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&destination)
    {
        Ok(output) => output,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            skipped.push(issue(
                archive_path,
                raw_name,
                "duplicate flattened file name; the first entry was kept",
            ));
            return Ok(());
        }
        Err(error) => return Err(error).context("could not create extracted staging file"),
    };
    let copied = match copy_entry_limited(reader, &mut output, declared_size, budget.bytes) {
        Ok(copied) => copied,
        Err(error) => {
            drop(output);
            let _ = fs::remove_file(&destination);
            return Err(error).with_context(|| format!("could not extract {raw_name}"));
        }
    };
    output
        .sync_all()
        .context("could not flush extracted staging file")?;
    budget.files += 1;
    budget.bytes += copied;
    Ok(())
}

fn observe_entry(budget: &mut ExtractionBudget, declared_size: u64) -> Result<()> {
    budget.seen_files = budget
        .seen_files
        .checked_add(1)
        .context("archive entry count overflow")?;
    if budget.seen_files > MAX_ARCHIVE_FILES {
        bail!("archive import exceeds the {MAX_ARCHIVE_FILES} file limit");
    }
    budget.seen_bytes = budget
        .seen_bytes
        .checked_add(declared_size)
        .context("archive declared size overflow")?;
    if budget.seen_bytes > MAX_ARCHIVE_BYTES {
        bail!("archive import exceeds the {MAX_ARCHIVE_BYTES} byte limit");
    }
    Ok(())
}

fn copy_entry_limited<R: Read, W: Write>(
    reader: &mut R,
    writer: &mut W,
    declared_size: u64,
    committed_bytes: u64,
) -> Result<u64> {
    let mut copied = 0u64;
    let mut buffer = [0u8; COPY_BUFFER_BYTES];
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        copied = copied
            .checked_add(count as u64)
            .context("archive entry size overflow")?;
        if committed_bytes.saturating_add(copied) > MAX_ARCHIVE_BYTES {
            bail!("archive import exceeds the {MAX_ARCHIVE_BYTES} byte limit");
        }
        writer.write_all(&buffer[..count])?;
    }
    if copied != declared_size {
        bail!("archive entry size mismatch: declared {declared_size}, extracted {copied}");
    }
    Ok(copied)
}

fn detect_archive_format(path: &Path) -> Result<ArchiveFormat> {
    let name = path
        .file_name()
        .and_then(OsStr::to_str)
        .context("archive file name is not Unicode")?
        .to_ascii_lowercase();
    if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
        Ok(ArchiveFormat::TarGz)
    } else if name.ends_with(".tar") {
        Ok(ArchiveFormat::Tar)
    } else if name.ends_with(".zip") {
        Ok(ArchiveFormat::Zip)
    } else {
        bail!("unsupported archive format; use ZIP, TAR or TAR.GZ")
    }
}

fn accepted_entry_name(raw_name: &str) -> Option<std::ffi::OsString> {
    if raw_name.contains('\0') || raw_name.contains(':') {
        return None;
    }
    let normalized = raw_name.replace('\\', "/");
    accepted_path_name(Path::new(&normalized))
}

fn accepted_path_name(path: &Path) -> Option<std::ffi::OsString> {
    let mut has_normal = false;
    for component in path.components() {
        match component {
            Component::Normal(_) => has_normal = true,
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    if !has_normal {
        return None;
    }
    let file_name = path.file_name()?;
    let extension = Path::new(file_name)
        .extension()
        .and_then(OsStr::to_str)?
        .to_ascii_lowercase();
    if !matches!(extension.as_str(), "lua" | "bin" | "manifest") {
        return None;
    }
    Some(file_name.to_os_string())
}

fn open_archive(path: &Path) -> Result<File> {
    require_regular_file(path, "archive")?;
    reject_linked_ancestors(path)?;
    if fs::metadata(path)?.len() > MAX_ARCHIVE_BYTES {
        bail!("archive file exceeds the {MAX_ARCHIVE_BYTES} byte limit");
    }
    File::open(path).with_context(|| format!("could not open archive {}", display_path(path)))
}

fn staged_file_names(path: &Path) -> Result<std::collections::BTreeSet<std::ffi::OsString>> {
    let mut names = std::collections::BTreeSet::new();
    for entry in fs::read_dir(path).context("could not inspect archive staging area")? {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            names.insert(entry.file_name());
        }
    }
    Ok(names)
}

fn rollback_staged_files(
    path: &Path,
    keep: &std::collections::BTreeSet<std::ffi::OsString>,
) -> Result<()> {
    for entry in fs::read_dir(path).context("could not inspect archive staging area")? {
        let entry = entry?;
        if entry.file_type()?.is_file() && !keep.contains(&entry.file_name()) {
            fs::remove_file(entry.path())
                .context("could not remove partial archive staging file")?;
        }
    }
    Ok(())
}

fn require_regular_file(path: &Path, label: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("could not inspect {label} {}", display_path(path)))?;
    if metadata_is_link(&metadata) || !metadata.is_file() {
        bail!(
            "{label} must be a regular non-linked file: {}",
            display_path(path)
        );
    }
    Ok(())
}

fn reject_linked_ancestors(path: &Path) -> Result<()> {
    let mut current = Some(path);
    while let Some(candidate) = current {
        match fs::symlink_metadata(candidate) {
            Ok(metadata) if metadata_is_link(&metadata) => {
                bail!(
                    "linked or reparse-point paths are not allowed: {}",
                    display_path(candidate)
                )
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("could not inspect archive path ancestry"),
        }
        current = candidate.parent();
    }
    Ok(())
}

fn metadata_is_link(metadata: &Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & REPARSE_POINT_ATTRIBUTE != 0
    }
    #[cfg(not(windows))]
    {
        let _ = REPARSE_POINT_ATTRIBUTE;
        false
    }
}

fn issue(archive: &Path, entry: &str, reason: &str) -> ArchiveImportIssue {
    ArchiveImportIssue {
        archive: display_path(archive),
        entry: entry.to_string(),
        reason: reason.to_string(),
    }
}

fn empty_import_summary() -> ImportSummary {
    ImportSummary {
        lua_count: 0,
        bin_count: 0,
        manifest_count: 0,
        imported: Vec::new(),
        skipped: Vec::<ImportIssue>::new(),
        failed: Vec::<ImportIssue>::new(),
    }
}

fn display_path(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{write::GzEncoder, Compression};
    use std::io::Cursor;
    use tempfile::tempdir;
    use zip::write::SimpleFileOptions;

    fn fixture() -> (tempfile::TempDir, PathBuf) {
        let temp = tempdir().unwrap();
        let steam = temp.path().join("Steam");
        fs::create_dir_all(&steam).unwrap();
        (temp, steam)
    }

    fn write_zip(path: &Path) {
        let file = File::create(path).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        let options = SimpleFileOptions::default();
        writer.start_file("nested/42.lua", options).unwrap();
        writer.write_all(b"addappid(42)\n").unwrap();
        writer.start_file("nested/42.bin", options).unwrap();
        writer.write_all(b"stats").unwrap();
        writer.start_file("../escape.manifest", options).unwrap();
        writer.write_all(b"unsafe").unwrap();
        writer.start_file("ignored.txt", options).unwrap();
        writer.write_all(b"ignored").unwrap();
        writer.finish().unwrap();
    }

    fn tar_bytes() -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut bytes);
            let mut header = tar::Header::new_gnu();
            header.set_size(8);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, "a/7.lua", Cursor::new(b"addappid"))
                .unwrap();
            let mut duplicate = tar::Header::new_gnu();
            duplicate.set_size(7);
            duplicate.set_mode(0o644);
            duplicate.set_cksum();
            builder
                .append_data(&mut duplicate, "b/7.lua", Cursor::new(b"ignored"))
                .unwrap();
            builder.finish().unwrap();
        }
        bytes
    }

    #[test]
    fn imports_zip_and_rejects_traversal() {
        let (temp, steam) = fixture();
        let archive = temp.path().join("input.zip");
        write_zip(&archive);
        let summary = import_library_archives(&ArchiveImportRequest {
            steam_root: steam.clone(),
            archives: vec![archive],
            overwrite: false,
        })
        .unwrap();
        assert_eq!(summary.extracted_files, 2);
        assert_eq!((summary.import.lua_count, summary.import.bin_count), (1, 1));
        assert!(steam.join("config/lua/42.lua").is_file());
        assert!(steam.join("appcache/stats/42.bin").is_file());
        assert_eq!(summary.skipped.len(), 2);
        assert!(!temp.path().join("escape.manifest").exists());
    }

    #[test]
    fn imports_tar_and_keeps_first_flattened_name() {
        let (temp, steam) = fixture();
        let archive = temp.path().join("input.tar");
        fs::write(&archive, tar_bytes()).unwrap();
        let summary = import_library_archives(&ArchiveImportRequest {
            steam_root: steam.clone(),
            archives: vec![archive],
            overwrite: false,
        })
        .unwrap();
        assert_eq!(summary.extracted_files, 1);
        assert_eq!(summary.import.lua_count, 1);
        assert_eq!(summary.skipped.len(), 1);
        assert_eq!(
            fs::read(steam.join("config/lua/7.lua")).unwrap(),
            b"addappid"
        );
    }

    #[test]
    fn imports_tar_gz_and_reports_invalid_archive_without_leaking_staging() {
        let (temp, steam) = fixture();
        let archive = temp.path().join("input.tar.gz");
        let mut encoder = GzEncoder::new(File::create(&archive).unwrap(), Compression::default());
        encoder.write_all(&tar_bytes()).unwrap();
        encoder.finish().unwrap();
        let broken = temp.path().join("broken.zip");
        fs::write(&broken, b"not a zip").unwrap();
        let summary = import_library_archives(&ArchiveImportRequest {
            steam_root: steam,
            archives: vec![archive, broken],
            overwrite: false,
        })
        .unwrap();
        assert_eq!(summary.archives.len(), 2);
        assert!(summary.archives[0].success);
        assert!(!summary.archives[1].success);
        assert_eq!(summary.failed.len(), 1);
    }

    #[test]
    fn detects_supported_formats_case_insensitively() {
        assert_eq!(
            detect_archive_format(Path::new("a.ZIP")).unwrap(),
            ArchiveFormat::Zip
        );
        assert_eq!(
            detect_archive_format(Path::new("a.tar")).unwrap(),
            ArchiveFormat::Tar
        );
        assert_eq!(
            detect_archive_format(Path::new("a.TGZ")).unwrap(),
            ArchiveFormat::TarGz
        );
        assert!(detect_archive_format(Path::new("a.rar")).is_err());
    }
}
