use super::{manifest_error, JitError};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Component, Path, PathBuf};

pub(super) struct Root {
    path: PathBuf,
    pub(super) resolved: PathBuf,
}

impl Root {
    pub(super) fn new(path: &Path) -> Result<Self, JitError> {
        let path = absolute(path)?;
        let resolved = fs::canonicalize(&path).map_err(JitError::Io)?;
        if !resolved.is_dir() {
            return Err(manifest_error("artifact root must be a directory"));
        }
        Ok(Self { path, resolved })
    }

    pub(super) fn contains(&self, path: &Path) -> bool {
        absolute(path).is_ok_and(|path| self.contains_absolute(&path))
    }

    fn contains_absolute(&self, path: &Path) -> bool {
        !path.components().any(|c| c == Component::ParentDir)
            && (path.starts_with(&self.path) || path.starts_with(&self.resolved))
    }
}

pub(super) fn read(path: &Path, limit: usize, root: Option<&Root>) -> Result<Vec<u8>, JitError> {
    let path = absolute(path)?;
    if root.is_some_and(|root| !root.contains_absolute(&path)) {
        return Err(manifest_error("object path is outside the artifact root"));
    }
    let file = open_regular(&path, root)?;
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(JitError::Io)?;
    if bytes.len() > limit {
        return Err(JitError::ObjectTooLarge { limit });
    }
    Ok(bytes)
}

#[cfg(not(windows))]
fn absolute(path: &Path) -> Result<PathBuf, JitError> {
    std::path::absolute(path).map_err(JitError::Io)
}

#[cfg(windows)]
fn absolute(path: &Path) -> Result<PathBuf, JitError> {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Prefix;

    let path = std::path::absolute(path).map_err(JitError::Io)?;
    let mut components = path.components();
    let Some(Component::Prefix(prefix)) = components.next() else {
        return Err(manifest_error("expected an absolute filesystem path"));
    };
    // A verbatim disk/share pathname avoids DOS device aliases. Reject device
    // namespaces, pipe shares and alternate streams for artifact reads.
    let mut result = OsString::from(r"\\?\");
    match prefix.kind() {
        Prefix::Disk(drive) | Prefix::VerbatimDisk(drive) => {
            result.push(format!("{}:", char::from(drive)));
        }
        Prefix::UNC(server, share) | Prefix::VerbatimUNC(server, share)
            if !share.eq_ignore_ascii_case("pipe") && !share.eq_ignore_ascii_case("mailslot") =>
        {
            result.push(r"UNC\");
            result.push(server);
            result.push(r"\");
            result.push(share);
        }
        _ => return Err(manifest_error("expected a disk or filesystem share path")),
    }
    if components.clone().any(|part| {
        matches!(part, Component::Normal(name) if name.encode_wide().any(|c| c == u16::from(b':')))
    }) {
        return Err(manifest_error("alternate file streams are unsupported"));
    }
    let mut result = PathBuf::from(result);
    result.extend(components);
    Ok(result)
}

#[cfg(any(windows, target_os = "linux"))]
fn check_resolved(path: &Path, root: Option<&Root>) -> Result<(), JitError> {
    if root.is_some_and(|root| !path.starts_with(&root.resolved)) {
        return Err(manifest_error("object resolves outside the artifact root"));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn open_regular(path: &Path, root: Option<&Root>) -> Result<File, JitError> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;

    // Inspect the inode without opening a data stream. Reopen that exact
    // inode only after checking its type and resolved location.
    let file = File::options()
        .read(true)
        .custom_flags(libc::O_PATH)
        .open(path)
        .map_err(JitError::Io)?;
    if !file.metadata().map_err(JitError::Io)?.is_file() {
        return Err(manifest_error("artifact must be a regular file"));
    }
    let fd_path = PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()));
    if root.is_some() {
        check_resolved(&fs::read_link(&fd_path).map_err(JitError::Io)?, root)?;
    }
    File::options()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(fd_path)
        .map_err(JitError::Io)
}

#[cfg(windows)]
fn open_regular(path: &Path, root: Option<&Root>) -> Result<File, JitError> {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    use windows_sys::Win32::Foundation::{GENERIC_READ, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileType, GetFinalPathNameByHandleW, ReOpenFile, FILE_ATTRIBUTE_REPARSE_POINT,
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
        FILE_SHARE_READ, FILE_TYPE_DISK,
    };

    let file = File::options()
        .access_mode(FILE_READ_ATTRIBUTES)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .map_err(JitError::Io)?;
    let handle = file.as_raw_handle();
    let metadata = file.metadata().map_err(JitError::Io)?;
    if unsafe { GetFileType(handle) } != FILE_TYPE_DISK
        || !metadata.is_file()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    {
        return Err(manifest_error("artifact must be a regular file"));
    }
    if root.is_some() {
        let mut buffer = vec![0u16; 32768];
        let written = unsafe {
            GetFinalPathNameByHandleW(handle, buffer.as_mut_ptr(), buffer.len() as u32, 0)
        } as usize;
        if written == 0 || written >= buffer.len() {
            return Err(manifest_error("cannot resolve the opened artifact path"));
        }
        check_resolved(Path::new(&OsString::from_wide(&buffer[..written])), root)?;
    }
    // ReOpenFile preserves the file identity even if its pathname changes.
    let handle = unsafe { ReOpenFile(handle, GENERIC_READ, FILE_SHARE_READ, 0) };
    if handle == INVALID_HANDLE_VALUE {
        return Err(JitError::Io(std::io::Error::last_os_error()));
    }
    Ok(unsafe { File::from_raw_handle(handle) })
}

#[cfg(not(any(windows, target_os = "linux")))]
fn open_regular(_path: &Path, _root: Option<&Root>) -> Result<File, JitError> {
    Err(JitError::UnsupportedTarget)
}
