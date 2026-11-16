//! Process-resident file identity for the checked loader, separate from linking.
use super::super::artifact::{dll_name, identity_error, FileRecord, Foundation};
use super::{JitError, Runtime};
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_READ_ATTRIBUTES, FILE_SHARE_READ,
};
use windows_sys::Win32::System::LibraryLoader::{
    GetModuleFileNameW, GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
};

struct LockedDll {
    path: PathBuf,
    // Deny write/delete on the file AND directory components of its canonical
    // locator. Locking only the leaf would not prevent renaming a parent path.
    _file: File,
    _directories: Vec<File>,
}
impl LockedDll {
    fn open(record: &FileRecord) -> Result<Self, JitError> {
        let path = std::fs::canonicalize(&record.path).map_err(JitError::Io)?;
        let mut directories = Vec::new();
        let parents: Vec<_> = path.ancestors().skip(1).collect();
        for parent in parents.into_iter().rev() {
            let directory = OpenOptions::new()
                .access_mode(FILE_READ_ATTRIBUTES)
                .share_mode(FILE_SHARE_READ)
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
                .open(parent)
                .map_err(JitError::Io)?;
            let metadata = directory.metadata().map_err(JitError::Io)?;
            if !metadata.is_dir() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
            {
                return Err(identity_error(
                    "canonical DLL path contains a reparse/directory change",
                ));
            }
            directories.push(directory);
        }
        let mut file = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(&path)
            .map_err(JitError::Io)?;
        let metadata = file.metadata().map_err(JitError::Io)?;
        if !metadata.is_file()
            || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
            || metadata.len() != record.bytes
        {
            return Err(identity_error("DLL size/type differs from its manifest"));
        }
        let mut hash = Sha256::new();
        let mut buffer = [0u8; 65536];
        loop {
            let n = file.read(&mut buffer).map_err(JitError::Io)?;
            if n == 0 {
                break;
            }
            hash.update(&buffer[..n]);
        }
        if format!("{:x}", hash.finalize()) != record.sha256 {
            return Err(identity_error("DLL SHA-256 differs from its manifest"));
        }
        Ok(Self {
            path,
            _file: file,
            _directories: directories,
        })
    }
}
struct ResidentFoundation {
    id: String,
    files: [LockedDll; 2],
    // None after any partial initialization failure. Keep guards and loaded
    // libraries resident, and require restart rather than reusing half a setup.
    runtime: Option<Runtime>,
}

pub(in crate::jit) fn open_verified_foundation(
    foundation: &Foundation,
    id: &str,
) -> Result<Runtime, JitError> {
    static RESIDENT: OnceLock<Mutex<Option<ResidentFoundation>>> = OnceLock::new();
    // A DLL initializer may reenter. Do not deadlock holding a lock over it.
    let mut state = RESIDENT
        .get_or_init(|| Mutex::new(None))
        .try_lock()
        .map_err(|_| identity_error("foundation initialization is busy or poisoned"))?;
    if let Some(resident) = state.as_ref() {
        if resident.id != id {
            return Err(identity_error(
                "a different foundation is resident; restart required",
            ));
        }
        return resident.runtime.clone().ok_or_else(|| {
            identity_error("foundation initialization previously failed; restart required")
        });
    }
    let files = [
        LockedDll::open(&foundation.std_dll)?,
        LockedDll::open(&foundation.support_dll)?,
    ];
    // Never infer the identity of a pre-existing image merely by hashing the
    // current file at its reported pathname. It may have been replaced earlier.
    for record in [&foundation.std_dll, &foundation.support_dll] {
        let name: Vec<u16> = dll_name(record)?.encode_utf16().chain(Some(0)).collect();
        let mut handle = std::ptr::null_mut();
        if unsafe {
            GetModuleHandleExW(
                GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                name.as_ptr(),
                &mut handle,
            )
        } != 0
        {
            return Err(identity_error(
                "std/support already loaded outside the verified session; start a fresh host",
            ));
        }
    }
    *state = Some(ResidentFoundation {
        id: id.to_owned(),
        files,
        runtime: None,
    });
    let resident = state.as_mut().unwrap();
    // The TrustedLayeredArtifact contract covers native initialization and loader
    // cooperation. Identity/guards are checked here, not delegated to the caller.
    let runtime = unsafe { Runtime::open(&resident.files[0].path, &resident.files[1].path) }?;
    for (module, file) in runtime.modules.iter().zip(&resident.files) {
        let mut path = vec![0u16; 32768];
        let n = unsafe { GetModuleFileNameW(*module as _, path.as_mut_ptr(), path.len() as u32) }
            as usize;
        if n == 0 || n >= path.len() {
            return Err(identity_error("cannot confirm loaded DLL path"));
        }
        use std::os::windows::ffi::OsStringExt;
        let actual = PathBuf::from(std::ffi::OsString::from_wide(&path[..n]));
        if std::fs::canonicalize(actual).map_err(JitError::Io)? != file.path {
            return Err(identity_error(
                "Windows redirected the requested DLL; restart required",
            ));
        }
    }
    resident.runtime = Some(runtime.clone());
    Ok(runtime)
}
