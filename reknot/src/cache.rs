//! Cooperative cleanup of SDK-owned local cache sessions; never unloads code.
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
mod permissions;
mod session;
pub(crate) use session::directory as session_directory;
const MANAGED: &str = "managed-v1";
const MAGIC: &str = "reknot-cache-v1";

#[derive(Clone, Debug)]
pub struct CacheOptions {
    /// Dedicated trusted local root; never a build/source directory. The SDK
    /// writes to root/managed-v1 only. Default uses REKNOT_CACHE_ROOT when set,
    /// otherwise temp/reknot.
    pub root: PathBuf,
    pub apply: bool,
    pub max_sessions: usize,
    /// Bounds all directory entries examined, including unknown/legacy entries.
    pub max_entries: usize,
}
impl Default for CacheOptions {
    fn default() -> Self {
        Self {
            root: std::env::var_os("REKNOT_CACHE_ROOT")
                .map(PathBuf::from)
                .unwrap_or_else(|| std::env::temp_dir().join("reknot")),
            apply: false,
            max_sessions: 1024,
            max_entries: 65536,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CacheStatus {
    InUse,
    Eligible,
    Trash,
    Legacy,
    Unknown,
    Deleted,
    PartialFailure,
}
#[derive(Clone, Debug)]
pub struct CacheEntry {
    pub path: PathBuf,
    pub status: CacheStatus,
    pub files: u64,
    pub logical_bytes: u64,
    pub deleted_files: u64,
    pub deleted_bytes: u64,
    pub reason: String,
    pub partial: bool,
}
#[derive(Clone, Debug, Default)]
pub struct CacheReport {
    pub entries: Vec<CacheEntry>,
    pub partial: bool,
    pub applied: bool,
    pub files: u64,
    pub logical_bytes: u64,
    pub deleted_files: u64,
    pub deleted_bytes: u64,
}
fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
/// No following links, including Windows junctions/mount-point reparse points.
fn metadata(path: &Path) -> io::Result<fs::Metadata> {
    let m = fs::symlink_metadata(path)?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if m.file_attributes() & 0x400 != 0 {
            return Err(invalid("reparse point refused"));
        }
    }
    if m.file_type().is_symlink() {
        return Err(invalid("symlink refused"));
    }
    Ok(m)
}
fn checked_path(path: &Path) -> io::Result<()> {
    if !path.is_absolute()
        || path
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        return Err(invalid("cache paths must be absolute without . or .."));
    }
    #[cfg(windows)]
    {
        use std::path::Prefix;
        if path.components().any(|c| matches!(c, Component::Prefix(p) if matches!(p.kind(), Prefix::UNC(..) | Prefix::VerbatimUNC(..)))) { return Err(invalid("network cache refused")); }
    }
    Ok(())
}
fn checked_directory(path: &Path) -> io::Result<()> {
    checked_path(path)?;
    for ancestor in path.ancestors() {
        if !metadata(ancestor)?.is_dir() {
            return Err(invalid("expected directory"));
        }
    }
    Ok(())
}
fn new_directory(path: &Path) -> io::Result<()> {
    #[allow(unused_mut)]
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)?;
    permissions::harden(path)
}
fn ensure_directory(path: &Path) -> io::Result<()> {
    if !path.is_absolute()
        || path
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        return Err(invalid("invalid cache root"));
    }
    if !path.try_exists()? {
        if let Some(parent) = path.parent() {
            ensure_directory(parent)?;
        }
        match new_directory(path) {
            Ok(()) => (),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => (),
            Err(e) => return Err(e),
        }
    }
    checked_directory(path)
}
pub(crate) fn new_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}
fn open_control(path: &Path, create: bool) -> io::Result<File> {
    if create {
        match new_file(path) {
            Ok(f) => return Ok(f),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => (),
            Err(e) => return Err(e),
        }
    }
    if !metadata(path)?.is_file() {
        return Err(invalid("expected regular control file"));
    }
    permissions::private(path)?;
    OpenOptions::new().read(true).write(true).open(path)
}
fn lock(file: &File) -> io::Result<()> {
    file.try_lock().map_err(|e| match e {
        fs::TryLockError::WouldBlock => {
            io::Error::new(io::ErrorKind::WouldBlock, "cache namespace/session busy")
        }
        fs::TryLockError::Error(e) => e,
    })
}
fn valid_id(id: &str) -> bool {
    id.len() == 59
        && id.starts_with('s')
        && id.as_bytes()[9] == b'-'
        && id.as_bytes()[42] == b'-'
        && id.bytes().enumerate().all(|(i, c)| {
            i == 0 || i == 9 || i == 42 || c.is_ascii_hexdigit() && !c.is_ascii_uppercase()
        })
}
fn valid_copy(name: &str) -> bool {
    name.split_once('.').is_some_and(|(id, ext)| {
        id.len() == 16
            && id
                .bytes()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
            && matches!(ext, "dll" | "so" | "dylib" | "bin")
    })
}
fn manifest(path: &Path, id: &str) -> io::Result<()> {
    if !valid_id(id) {
        return Err(invalid("invalid session ID"));
    }
    let target = path.join("session.meta");
    if !metadata(&target)?.is_file() {
        return Err(invalid("invalid session metadata file"));
    }
    permissions::private(&target)?;
    let mut content = String::new();
    File::open(&target)?
        .take(513)
        .read_to_string(&mut content)?;
    if content.len() > 512 {
        return Err(invalid("oversized session metadata"));
    }
    let lines: Vec<_> = content.lines().collect();
    if lines.len() != 4
        || lines[0] != MAGIC
        || lines[1] != format!("id={id}")
        || lines[2]
            .strip_prefix("pid=")
            .and_then(|s| s.parse::<u32>().ok())
            .is_none()
        || lines[3]
            .strip_prefix("created=")
            .and_then(|s| s.parse::<u128>().ok())
            .is_none()
    {
        return Err(invalid("unknown or corrupt session metadata"));
    }
    Ok(())
}
struct Budget {
    remaining: usize,
    partial: bool,
}
impl Budget {
    fn take(&mut self) -> io::Result<()> {
        if self.remaining == 0 {
            self.partial = true;
            Err(invalid("scan entry limit reached"))
        } else {
            self.remaining -= 1;
            Ok(())
        }
    }
}
fn entry(path: PathBuf) -> CacheEntry {
    CacheEntry {
        path,
        status: CacheStatus::Unknown,
        files: 0,
        logical_bytes: 0,
        deleted_files: 0,
        deleted_bytes: 0,
        reason: String::new(),
        partial: false,
    }
}
struct Contents {
    files: Vec<PathBuf>,
    copies: bool,
    lease: bool,
}
fn contents(
    path: &Path,
    id: &str,
    trash: bool,
    budget: &mut Budget,
    report: &mut CacheEntry,
) -> io::Result<Contents> {
    checked_directory(path)?;
    permissions::private(path)?;
    // Empty trash directories can remain after deleting the final metadata.
    let mut top = Vec::new();
    for item in fs::read_dir(path)? {
        budget.take()?;
        top.push(item?);
    }
    if trash && top.is_empty() {
        return Ok(Contents {
            files: Vec::new(),
            copies: false,
            lease: false,
        });
    }
    manifest(path, id)?;
    let mut result = Contents {
        files: Vec::new(),
        copies: false,
        lease: false,
    };
    for item in top {
        let name = item.file_name();
        let child = item.path();
        let m = metadata(&child)?;
        match name.to_str() {
            Some("session.meta") if m.is_file() => (),
            Some("lease.lock") if m.is_file() && m.len() == 0 => {
                permissions::private(&child)?;
                result.lease = true;
            }
            Some("copies") if m.is_dir() => {
                permissions::private(&child)?;
                result.copies = true;
                for file in fs::read_dir(&child)? {
                    budget.take()?;
                    let file = file?;
                    let file_path = file.path();
                    let m = metadata(&file_path)?;
                    if !m.is_file() || !file.file_name().to_str().is_some_and(valid_copy) {
                        return Err(invalid("unknown copy entry"));
                    }
                    report.files = report.files.saturating_add(1);
                    report.logical_bytes = report.logical_bytes.saturating_add(m.len());
                    result.files.push(file_path);
                }
            }
            _ => return Err(invalid("unknown session entry")),
        }
    }
    if !trash && (!result.copies || !result.lease) {
        return Err(invalid("incomplete session"));
    }
    result.files.sort();
    Ok(result)
}
fn sweep(path: &Path, payload: Contents, report: &mut CacheEntry) -> io::Result<()> {
    // Explicit flat allowlist. Never recurse and never interpret paths from metadata.
    for file in payload.files {
        checked_directory(file.parent().unwrap())?;
        let m = metadata(&file)?;
        if !m.is_file() {
            return Err(invalid("copy changed type"));
        }
        fs::remove_file(&file)?;
        report.deleted_files += 1;
        report.deleted_bytes = report.deleted_bytes.saturating_add(m.len());
    }
    if payload.copies {
        fs::remove_dir(path.join("copies"))?;
    }
    if payload.lease {
        fs::remove_file(path.join("lease.lock"))?;
    }
    let meta = path.join("session.meta");
    if meta.try_exists()? {
        if !metadata(&meta)?.is_file() {
            return Err(invalid("metadata changed type"));
        }
        fs::remove_file(meta)?;
    }
    fs::remove_dir(path)
}
fn examine(
    path: PathBuf,
    trash: bool,
    apply: bool,
    managed: &Path,
    budget: &mut Budget,
) -> CacheEntry {
    let mut report = entry(path.clone());
    let operation = (|| {
        let id = path
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| invalid("non-UTF8 session ID"))?;
        if !valid_id(id) {
            return Err(invalid("invalid session ID"));
        }
        checked_directory(&path)?;
        if session::is_current(&path) {
            report.status = CacheStatus::InUse;
            report.reason = "current SDK session".into();
            if let Err(e) = contents(&path, id, false, budget, &mut report) {
                report.partial = true;
                report.reason.push_str(&format!("; size inspection: {e}"));
            }
            return Ok(());
        }
        let lease = if trash {
            None
        } else {
            manifest(&path, id)?;
            let f = open_control(&path.join("lease.lock"), false)?;
            match lock(&f) {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    report.status = CacheStatus::InUse;
                    report.reason = "session lease held".into();
                    // Best-effort bounded size inspection; never grants deletion.
                    if let Err(e) = contents(&path, id, false, budget, &mut report) {
                        report.partial = true;
                        report.reason.push_str(&format!("; size inspection: {e}"));
                    }
                    return Ok(());
                }
                Err(e) => return Err(e),
                Ok(()) => Some(f),
            }
        };
        let payload = contents(&path, id, trash, budget, &mut report)?;
        report.status = if trash {
            CacheStatus::Trash
        } else {
            CacheStatus::Eligible
        };
        if !apply {
            return Ok(());
        }
        let target = managed.join("trash").join(id);
        if !trash {
            if target.try_exists()? {
                return Err(invalid("trash destination already exists"));
            }
            drop(lease); // namespace exclusion prevents competing claims/reuse
            fs::rename(&path, &target)?;
            report.path = target.clone();
        }
        sweep(&target, payload_remap(payload, &path, &target), &mut report)?;
        report.status = CacheStatus::Deleted;
        Ok(())
    })();
    if let Err(e) = operation {
        report.partial = true;
        report.reason = e.to_string();
        report.status = if apply && report.path.starts_with(managed.join("trash")) {
            CacheStatus::PartialFailure
        } else {
            CacheStatus::Unknown
        };
    }
    report
}
fn payload_remap(mut payload: Contents, old: &Path, new: &Path) -> Contents {
    for file in &mut payload.files {
        *file = new.join(file.strip_prefix(old).expect("validated direct child"));
    }
    payload
}
/// Lists owned cache copies. Does not create files or delete anything.
pub fn inspect_cache(options: &CacheOptions) -> io::Result<CacheReport> {
    run(options, false)
}
/// Dry-run by default; apply always rescans under namespace exclusion.
pub fn clean_cache(options: &CacheOptions) -> io::Result<CacheReport> {
    run(options, options.apply)
}
fn run(options: &CacheOptions, apply: bool) -> io::Result<CacheReport> {
    checked_path(&options.root)?;
    if options.max_sessions == 0 || options.max_entries == 0 {
        return Err(invalid("scan limits must be positive"));
    }
    let mut report = CacheReport {
        applied: apply,
        ..CacheReport::default()
    };
    if !options.root.try_exists()? {
        return Ok(report);
    }
    checked_directory(&options.root)?;
    permissions::private(&options.root)?;
    let managed = options.root.join(MANAGED);
    let mut budget = Budget {
        remaining: options.max_entries,
        partial: false,
    };
    if managed.try_exists()? {
        checked_directory(&managed)?;
        permissions::private(&managed)?;
        let namespace = open_control(&managed.join("namespace.lock"), false)?;
        lock(&namespace)?;
        for name in ["trash", "sessions"] {
            let parent = managed.join(name);
            checked_directory(&parent)?;
            permissions::private(&parent)?;
            for item in fs::read_dir(&parent)? {
                if report.entries.len() >= options.max_sessions || budget.take().is_err() {
                    report.partial = true;
                    break;
                }
                report.entries.push(examine(
                    item?.path(),
                    name == "trash",
                    apply,
                    &managed,
                    &mut budget,
                ));
            }
        }
    }
    // Legacy PID directories are only listed, with a bounded flat size scan.
    for item in fs::read_dir(&options.root)? {
        if budget.take().is_err() {
            break;
        }
        let item = item?;
        if item
            .file_name()
            .to_str()
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|c| c.is_ascii_digit()))
        {
            if report.entries.len() >= options.max_sessions {
                report.partial = true;
                break;
            }
            let mut r = entry(item.path());
            r.status = CacheStatus::Legacy;
            r.reason = "legacy layout has no session lease; never deleted".into();
            let result = (|| {
                checked_directory(&r.path)?;
                for file in fs::read_dir(&r.path)? {
                    budget.take()?;
                    let file = file?;
                    let m = metadata(&file.path())?;
                    if m.is_file() {
                        r.files += 1;
                        r.logical_bytes = r.logical_bytes.saturating_add(m.len());
                    }
                }
                Ok::<_, io::Error>(())
            })();
            if let Err(e) = result {
                r.partial = true;
                r.reason = e.to_string();
            }
            report.entries.push(r);
        }
    }
    report.partial |= budget.partial;
    for r in &report.entries {
        report.partial |= r.partial;
        report.files = report.files.saturating_add(r.files);
        report.logical_bytes = report.logical_bytes.saturating_add(r.logical_bytes);
        report.deleted_files = report.deleted_files.saturating_add(r.deleted_files);
        report.deleted_bytes = report.deleted_bytes.saturating_add(r.deleted_bytes);
    }
    Ok(report)
}
