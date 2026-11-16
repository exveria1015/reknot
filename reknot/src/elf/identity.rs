//! Verified, immutable, process-resident Linux foundation snapshots.
use super::{JitError, Runtime};
use crate::jit::artifact::{digest, dll_name, identity_error, FileRecord, Foundation};
use object::{elf, read::elf::ElfFile64, LittleEndian as LE};
use std::ffi::{CStr, CString};
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

struct Snapshot {
    path: PathBuf,
    // Retain the descriptor for the process lifetime, including partial failure.
    _file: File,
}
impl Snapshot {
    fn open(record: &FileRecord, std_name: &str, support: bool) -> Result<Self, JitError> {
        let source = File::open(&record.path).map_err(JitError::Io)?;
        if !source.metadata().map_err(JitError::Io)?.is_file() {
            return Err(identity_error("foundation is not a regular file"));
        }
        let mut bytes = Vec::new();
        source
            .take(record.bytes + 1)
            .read_to_end(&mut bytes)
            .map_err(JitError::Io)?;
        if bytes.len() as u64 != record.bytes || digest(&bytes) != record.sha256 {
            return Err(identity_error(
                "foundation size/SHA-256 differs from its manifest",
            ));
        }
        let name = dll_name(record)?;
        validate_dynamic(&bytes, &name, std_name, support)?;
        let label = CString::new(format!("reknot-{name}"))
            .map_err(|_| identity_error("invalid library name"))?;
        let fd = unsafe {
            libc::memfd_create(label.as_ptr(), libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING)
        };
        if fd < 0 {
            return Err(JitError::Io(std::io::Error::last_os_error()));
        }
        let mut file = unsafe { File::from_raw_fd(fd) };
        file.write_all(&bytes).map_err(JitError::Io)?;
        let seals =
            libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL;
        if unsafe { libc::fcntl(fd, libc::F_ADD_SEALS, seals) } != 0
            || unsafe { libc::fcntl(fd, libc::F_GET_SEALS) } != seals
        {
            return Err(JitError::Io(std::io::Error::last_os_error()));
        }
        Ok(Self {
            path: PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd())),
            _file: file,
        })
    }
}

fn validate_dynamic(
    bytes: &[u8],
    name: &str,
    std_name: &str,
    support: bool,
) -> Result<(), JitError> {
    let file = ElfFile64::<LE>::parse(bytes).map_err(|e| identity_error(e.to_string()))?;
    let header = file.elf_header();
    if header.e_type.get(LE) != elf::ET_DYN || header.e_machine.get(LE) != elf::EM_X86_64 {
        return Err(identity_error(
            "foundation must be a Linux x86_64 shared library",
        ));
    }
    let table = file
        .elf_dynamic_table()
        .map_err(|e| identity_error(e.to_string()))?;
    let mut soname = None;
    let mut needs_std = false;
    for item in table.iter() {
        match item.tag {
            elf::DT_SONAME => {
                let value = table
                    .string(item)
                    .map_err(|e| identity_error(e.to_string()))?;
                if soname.replace(value).is_some() {
                    return Err(identity_error("duplicate SONAME"));
                }
            }
            elf::DT_NEEDED => {
                let value = table
                    .string(item)
                    .map_err(|e| identity_error(e.to_string()))?;
                if support && value == std_name.as_bytes() {
                    needs_std = true;
                } else if ![
                    b"libc.so.6".as_slice(),
                    b"libm.so.6",
                    b"libgcc_s.so.1",
                    b"libpthread.so.0",
                    b"libdl.so.2",
                    b"librt.so.1",
                    b"ld-linux-x86-64.so.2",
                ]
                .contains(&value)
                {
                    return Err(identity_error(format!(
                        "unverified ELF dependency: {}",
                        String::from_utf8_lossy(value)
                    )));
                }
            }
            elf::DT_AUDIT | elf::DT_DEPAUDIT | elf::DT_FILTER | elf::DT_AUXILIARY => {
                return Err(identity_error(
                    "ELF audit/filter dependencies are unsupported",
                ));
            }
            _ => {}
        }
    }
    if soname != Some(name.as_bytes()) || (support && !needs_std) {
        return Err(identity_error(
            "SONAME or support's std dependency differs from manifest",
        ));
    }
    Ok(())
}

struct ResidentFoundation {
    id: String,
    files: [Snapshot; 2],
    runtime: Option<Runtime>,
}

pub(in crate::jit) fn open_verified_foundation(
    foundation: &Foundation,
    id: &str,
) -> Result<Runtime, JitError> {
    static RESIDENT: OnceLock<Mutex<Option<ResidentFoundation>>> = OnceLock::new();
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
    let std_name = dll_name(&foundation.std_dll)?;
    let files = [
        Snapshot::open(&foundation.std_dll, &std_name, false)?,
        Snapshot::open(&foundation.support_dll, &std_name, true)?,
    ];
    // A pathname hash cannot prove the bytes of a module loaded earlier.
    for record in [&foundation.std_dll, &foundation.support_dll] {
        let name = CString::new(dll_name(record)?).map_err(|_| identity_error("invalid SONAME"))?;
        let handle = unsafe { libc::dlopen(name.as_ptr(), libc::RTLD_NOW | libc::RTLD_NOLOAD) };
        if !handle.is_null() {
            unsafe {
                libc::dlclose(handle);
            }
            return Err(identity_error(
                "std/support already loaded outside the verified session; start a fresh host",
            ));
        }
    }
    *state = Some(ResidentFoundation {
        id: id.into(),
        files,
        runtime: None,
    });
    let resident = state.as_mut().unwrap();
    let paths = resident.files.each_ref().map(|f| f.path.clone());
    let runtime = unsafe { Runtime::open_paths(&paths) }?;
    resident.runtime = Some(runtime.clone());
    Ok(runtime)
}

// The initial ABI fields of glibc's public link_map (link.h).
#[repr(C)]
struct LinkMap {
    address: usize,
    name: *const libc::c_char,
}

pub(super) unsafe fn check_loaded_path(
    handle: *mut libc::c_void,
    path: &std::path::Path,
) -> Result<(), JitError> {
    use std::os::unix::ffi::OsStrExt;
    let mut map: *mut LinkMap = std::ptr::null_mut();
    if libc::dlinfo(
        handle,
        libc::RTLD_DI_LINKMAP,
        (&mut map as *mut *mut LinkMap).cast(),
    ) != 0
        || map.is_null()
        || (*map).name.is_null()
        || CStr::from_ptr((*map).name).to_bytes() != path.as_os_str().as_bytes()
    {
        return Err(identity_error(
            "loader redirected the requested foundation; restart required",
        ));
    }
    Ok(())
}
