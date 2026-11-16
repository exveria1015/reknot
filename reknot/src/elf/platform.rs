//! Linux mmap/mprotect, explicit dlopen scopes and libgcc frame registration.
use super::format::{
    aligned, error, relocate, relocation_width, uses_got, Definition, Object, Protection,
};
use super::{CoffPolicy, ElfEngine, JitError, JitLoadMetrics};
use crate::jit::CoffRetentionReason;
use crate::LoadedHandler;
use libloading::os::unix::{Library, RTLD_LOCAL, RTLD_NOW};
use object::elf;
use std::collections::HashMap;
use std::ffi::{c_void, CString};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::time::Instant;

#[path = "identity.rs"]
mod identity;
#[path = "../jit_retention.rs"]
pub(crate) mod retention;
pub(in crate::jit) use identity::open_verified_foundation;

#[derive(Clone)]
pub(in crate::jit) struct Runtime {
    modules: Vec<&'static Library>,
    cache: HashMap<Vec<u8>, Option<usize>>,
    register_frame: unsafe extern "C" fn(*const c_void),
    deregister_frame_info: unsafe extern "C" fn(*const c_void) -> *mut c_void,
}
impl Runtime {
    pub unsafe fn open(paths: Option<(&Path, &Path)>) -> Result<Self, JitError> {
        // Validate both names before either initializer runs. OS dependency
        // resolution remains part of the caller's trusted loader configuration.
        let paths = paths
            .map(|(std, support)| {
                let paths = [std, support].map(|path| {
                    let path = std::fs::canonicalize(path).map_err(JitError::Io)?;
                    use std::os::unix::ffi::OsStrExt;
                    CString::new(path.as_os_str().as_bytes())
                        .map_err(|_| error("NUL in resident ELF path"))?;
                    Ok::<_, JitError>(path)
                });
                let [std, support] = paths;
                Ok::<_, JitError>([std?, support?])
            })
            .transpose()?;
        Self::open_paths(paths.as_ref().map_or(&[], |paths| paths.as_slice()))
    }
    // Verified callers supply immutable memfd paths; do not canonicalize these
    // into the non-filesystem /memfd:... names shown by /proc/self/fd symlinks.
    unsafe fn open_paths(paths: &[PathBuf]) -> Result<Self, JitError> {
        let mut modules = Vec::new();
        for path in paths {
            let library =
                Library::open(Some(path), RTLD_NOW | RTLD_LOCAL).map_err(JitError::Bridge)?;
            let handle = library.into_raw();
            // Keep the loaded library resident even if identity confirmation fails.
            let library = Library::from_raw(handle);
            modules.push(&*Box::leak(Box::new(library)));
            identity::check_loaded_path(handle, path)?;
        }
        // Prefer the selected support's exports over its dependency std.
        modules.reverse();
        for name in ["libc.so.6", "libm.so.6", "libgcc_s.so.1"] {
            let library =
                Library::open(Some(name), RTLD_NOW | RTLD_LOCAL).map_err(JitError::Bridge)?;
            modules.push(&*Box::leak(Box::new(library)));
        }
        let register_frame = *modules
            .last()
            .unwrap()
            .get::<unsafe extern "C" fn(*const c_void)>(b"__register_frame\0")
            .map_err(JitError::Bridge)?;
        let deregister_frame_info = *modules
            .last()
            .unwrap()
            .get::<unsafe extern "C" fn(*const c_void) -> *mut c_void>(b"__deregister_frame_info\0")
            .map_err(JitError::Bridge)?;
        Ok(Self {
            modules,
            cache: HashMap::new(),
            register_frame,
            deregister_frame_info,
        })
    }

    fn find(&mut self, name: &[u8], weak: bool) -> Result<usize, JitError> {
        let address = if let Some(cached) = self.cache.get(name) {
            *cached
        } else {
            let terminated = CString::new(name).map_err(|_| error("NUL in ELF import"))?;
            let address = self.modules.iter().find_map(|library| unsafe {
                library
                    .get::<*const c_void>(terminated.as_bytes_with_nul())
                    .ok()
                    .map(|symbol| *symbol as usize)
                    .filter(|address| *address != 0)
            });
            self.cache.insert(name.to_vec(), address);
            address
        };
        address
            .or(if weak { Some(0) } else { None })
            .ok_or_else(|| {
                error(format!(
                    "unresolved ELF import: {}",
                    String::from_utf8_lossy(name)
                ))
            })
    }
}

struct Layout {
    sections: Vec<usize>,
    got: Vec<Option<usize>>,
    stubs: Vec<Option<usize>>,
    groups: Vec<(usize, usize, i32)>,
    size: usize,
    linked_bytes: usize,
    alignment: usize,
}
impl Layout {
    fn new(object: &Object<'_>, limit: usize) -> Result<Self, JitError> {
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        if page <= 0 || !(page as usize).is_power_of_two() {
            return Err(error("invalid Linux page size"));
        }
        let page = page as usize;
        let mut result = Self {
            sections: vec![0; object.sections.len()],
            got: vec![None; object.symbols.len()],
            stubs: vec![None; object.symbols.len()],
            groups: Vec::new(),
            size: 0,
            linked_bytes: 0,
            alignment: page,
        };
        let mut got_needed = vec![false; object.symbols.len()];
        let mut stub_needed = vec![false; object.symbols.len()];
        for section in &object.sections {
            for relocation in &section.relocations {
                if uses_got(relocation.kind) {
                    got_needed[relocation.symbol] = true;
                }
                if relocation.kind == elf::R_X86_64_PLT32
                    && matches!(
                        object.symbols[relocation.symbol].definition,
                        Definition::External
                    )
                {
                    stub_needed[relocation.symbol] = true;
                }
            }
        }
        for (protection, flags) in [
            (Protection::Execute, libc::PROT_READ | libc::PROT_EXEC),
            (Protection::Read, libc::PROT_READ),
            (Protection::Write, libc::PROT_READ | libc::PROT_WRITE),
        ] {
            let begin = result.size;
            for (index, section) in object
                .sections
                .iter()
                .enumerate()
                .filter(|(_, s)| s.protection == protection)
            {
                // libgcc requires an aligned, zero-terminated .eh_frame stream.
                let alignment = if index == object.unwind {
                    section.alignment.max(8)
                } else {
                    section.alignment
                };
                result.alignment = result.alignment.max(alignment);
                let size = section
                    .size
                    .checked_add(if index == object.unwind { 4 } else { 0 })
                    .ok_or_else(|| error("ELF image overflow"))?;
                result.sections[index] = result.reserve(size, alignment, limit)?;
            }
            for index in 0..object.symbols.len() {
                if protection == Protection::Read && got_needed[index] {
                    result.got[index] = Some(result.reserve(8, 8, limit)?);
                }
                if protection == Protection::Execute && stub_needed[index] {
                    // jmp qword ptr [rip+0]; 64-bit absolute destination.
                    result.stubs[index] = Some(result.reserve(14, 8, limit)?);
                }
            }
            result.size = aligned(result.size, page)?;
            if result.size > limit {
                return Err(error("ELF image budget exceeded (page padding)"));
            }
            if result.size != begin {
                result.groups.push((begin, result.size - begin, flags));
            }
        }
        if result.size == 0 {
            return Err(error("empty ELF image"));
        }
        Ok(result)
    }
    fn reserve(&mut self, size: usize, alignment: usize, limit: usize) -> Result<usize, JitError> {
        let at = aligned(self.size, alignment)?;
        self.size = at
            .checked_add(size)
            .ok_or_else(|| error("ELF image overflow"))?;
        if self.size > limit {
            return Err(error("ELF image budget exceeded (BSS/GOT/stubs/alignment)"));
        }
        self.linked_bytes += size;
        Ok(at)
    }
}

struct Image {
    base: NonNull<u8>,
    size: usize,
    unwind: usize,
    deregister: Option<unsafe extern "C" fn(*const c_void) -> *mut c_void>,
    released: bool,
    #[cfg(test)]
    fail_release: u8,
}
impl Image {
    fn new(size: usize, alignment: usize) -> Result<Self, JitError> {
        // Overallocate and trim to support alignments larger than a page without
        // MAP_FIXED or replacing any existing address range.
        let allocation = size
            .checked_add(alignment)
            .ok_or_else(|| error("ELF allocation overflow"))?;
        let raw = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                allocation,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if raw == libc::MAP_FAILED {
            return Err(os_error("mmap"));
        }
        let base = aligned(raw as usize, alignment)?;
        let prefix = base - raw as usize;
        let suffix = allocation - prefix - size;
        // Both fragments are page multiples on Linux. A failure leaks that
        // fragment, but never permits reuse of the image's live mapping.
        if prefix != 0 {
            unsafe {
                libc::munmap(raw, prefix);
            }
        }
        if suffix != 0 {
            unsafe {
                libc::munmap((base + size) as *mut c_void, suffix);
            }
        }
        Ok(Self {
            base: NonNull::new(base as *mut u8).ok_or_else(|| error("null ELF allocation"))?,
            size,
            unwind: 0,
            deregister: None,
            released: false,
            #[cfg(test)]
            fail_release: 0,
        })
    }
    fn release(&mut self) -> Result<(), (CoffRetentionReason, String)> {
        use CoffRetentionReason::*;
        if self.released {
            return Ok(());
        }
        if let Some(deregister) = self.deregister {
            #[cfg(test)]
            if self.fail_release == 1 {
                return Err((
                    UnwindRemovalFailed,
                    "injected unwind removal failure".into(),
                ));
            }
            // __register_frame allocated this libgcc object with malloc.
            // This is the checked equivalent of __deregister_frame's free(...).
            let object = unsafe { deregister(self.unwind as *const c_void) };
            if object.is_null() {
                return Err((
                    UnwindRemovalFailed,
                    "libgcc did not confirm frame removal".into(),
                ));
            }
            unsafe {
                libc::free(object);
            }
            self.deregister = None;
        }
        #[cfg(test)]
        if self.fail_release == 2 {
            return Err((
                MemoryReleaseFailed,
                "injected memory release failure".into(),
            ));
        }
        if unsafe { libc::munmap(self.base.as_ptr().cast(), self.size) } != 0 {
            return Err((MemoryReleaseFailed, os_error("munmap").to_string()));
        }
        self.released = true;
        Ok(())
    }
}
impl Drop for Image {
    fn drop(&mut self) {
        if self.released {
            return;
        }
        if let Err((reason, _)) = self.release() {
            let retained = Self {
                base: self.base,
                size: self.size,
                unwind: self.unwind,
                deregister: self.deregister,
                released: false,
                #[cfg(test)]
                fail_release: self.fail_release,
            };
            self.released = true;
            retention::retain_failed_rollback(retained, reason);
        }
    }
}
fn os_error(operation: &str) -> JitError {
    error(format!("{operation}: {}", std::io::Error::last_os_error()))
}

fn link(
    runtime: &mut Runtime,
    object: &Object<'_>,
    limit: usize,
    metrics: &mut JitLoadMetrics,
) -> Result<(Image, usize), JitError> {
    let layout = Layout::new(object, limit)?;
    let mut image = Image::new(layout.size, layout.alignment)?;
    let base = image.base.as_ptr() as usize;
    let bytes = unsafe { std::slice::from_raw_parts_mut(image.base.as_ptr(), image.size) };
    let mut addresses = vec![None; object.symbols.len()];
    let mut resolve = |index: usize| -> Result<usize, JitError> {
        if let Some(address) = addresses[index] {
            return Ok(address);
        }
        let symbol = &object.symbols[index];
        let address = match symbol.definition {
            Definition::Absolute => symbol.value,
            Definition::External => runtime.find(symbol.name, symbol.weak)?,
            Definition::Section(section) => {
                if object.sections[section].protection == Protection::Discard {
                    return Err(error("ELF runtime reference to discarded section"));
                }
                base + layout.sections[section] + symbol.value
            }
        };
        addresses[index] = Some(address);
        Ok(address)
    };
    let entry = resolve(object.entry)?;
    for (index, section) in object
        .sections
        .iter()
        .enumerate()
        .filter(|(_, s)| s.protection != Protection::Discard)
    {
        let at = layout.sections[index];
        bytes[at..at + section.data.len()].copy_from_slice(section.data);
        for relocation in &section.relocations {
            let mut target = resolve(relocation.symbol)?;
            if let Some(got) = layout.got[relocation.symbol] {
                bytes[got..got + 8].copy_from_slice(&(target as u64).to_le_bytes());
                if uses_got(relocation.kind) {
                    target = base + got;
                }
            }
            if relocation.kind == elf::R_X86_64_PLT32 {
                if let Some(stub) = layout.stubs[relocation.symbol] {
                    bytes[stub..stub + 6].copy_from_slice(&[0xff, 0x25, 0, 0, 0, 0]);
                    bytes[stub + 6..stub + 14].copy_from_slice(&(target as u64).to_le_bytes());
                    target = base + stub;
                }
            }
            let place = at + relocation.offset;
            relocate(
                &mut bytes[place..place + relocation_width(relocation.kind)?],
                relocation.kind,
                target,
                relocation.addend,
                base + place,
            )?;
        }
    }
    let unwind = layout.sections[object.unwind];
    super::format::validate_eh_frame(&bytes[unwind..unwind + object.sections[object.unwind].size])?;
    for &(begin, size, flags) in &layout.groups {
        if unsafe { libc::mprotect(image.base.as_ptr().add(begin).cast(), size, flags) } != 0 {
            return Err(os_error("mprotect"));
        }
    }
    // x86-64 has coherent instruction/data caches. No executable page was ever
    // writable after publication. All fallible preparation has completed.
    metrics.linked_bytes = layout.linked_bytes as u64;
    metrics.unwind_entries = object.unwind_entries as u64;
    let start = Instant::now();
    unsafe {
        (runtime.register_frame)((base + unwind) as *const c_void);
    }
    image.unwind = base + unwind;
    image.deregister = Some(runtime.deregister_frame_info);
    metrics.unwind_registration_ms = start.elapsed().as_secs_f64() * 1000.0;
    Ok((image, entry))
}

pub(in crate::jit) unsafe fn load(
    engine: &mut ElfEngine,
    path: &Path,
) -> Result<LoadedHandler, JitError> {
    let start = Instant::now();
    let file = std::fs::File::open(path).map_err(JitError::Io)?;
    let mut bytes = Vec::new();
    file.take(engine.limits.load.max_object_bytes as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(JitError::Io)?;
    if bytes.len() > engine.limits.load.max_object_bytes {
        return Err(JitError::ObjectTooLarge {
            limit: engine.limits.load.max_object_bytes,
        });
    }
    let mut metrics = JitLoadMetrics {
        read_ms: start.elapsed().as_secs_f64() * 1000.0,
        ..Default::default()
    };
    load_bytes(
        engine,
        &bytes,
        start,
        &mut metrics,
        CoffPolicy::KeepLoaded,
        None,
        None,
    )
}

#[cfg(feature = "jit-dev")]
pub(in crate::jit) fn required_image_bytes(
    engine: &mut ElfEngine,
    bytes: &[u8],
) -> Result<usize, JitError> {
    let object = Object::parse(bytes, engine.limits.max_image_bytes)?;
    Ok(Layout::new(&object, engine.limits.max_image_bytes)?.size)
}

pub(in crate::jit) unsafe fn load_bytes(
    engine: &mut ElfEngine,
    bytes: &[u8],
    start: Instant,
    metrics: &mut JitLoadMetrics,
    policy: CoffPolicy,
    foundation: Option<&str>,
    verified_hash: Option<&str>,
) -> Result<LoadedHandler, JitError> {
    retention::check_budget(engine.limits.into(), 1)?;
    if bytes.len() > engine.limits.load.max_object_bytes {
        return Err(JitError::ObjectTooLarge {
            limit: engine.limits.load.max_object_bytes,
        });
    }
    let inspection = Instant::now();
    let object = Object::parse(bytes, engine.limits.max_image_bytes)?;
    metrics.copy_ms = inspection.elapsed().as_secs_f64() * 1000.0;
    let materialize = Instant::now();
    let remaining = if policy == CoffPolicy::KeepLoaded {
        engine
            .limits
            .max_session_bytes
            .saturating_sub(engine.retained_bytes)
    } else {
        engine.limits.max_image_bytes
    };
    let required = Layout::new(&object, engine.limits.max_image_bytes.min(remaining))?.size;
    retention::check_budget(engine.limits.into(), required)?;
    let (image, entry) = link(
        &mut engine.runtime,
        &object,
        engine.limits.max_image_bytes.min(remaining),
        metrics,
    )?;
    let size = image.size;
    let (owner, tracking) = retention::publish(
        image,
        engine.limits.into(),
        policy,
        foundation,
        verified_hash
            .map(str::to_owned)
            .unwrap_or_else(|| crate::jit::artifact::digest(bytes)),
    )?;
    engine.retained_bytes = engine.retained_bytes.saturating_add(size);
    engine.last_image = Some(tracking.id);
    metrics.materialize_ms = materialize.elapsed().as_secs_f64() * 1000.0;
    let get_api: reknot_abi::services::HandlerApiFn = std::mem::transmute(entry);
    let handler = LoadedHandler::from_entry(get_api, owner).map_err(|e| {
        tracking.pin(CoffRetentionReason::EntryRejected);
        JitError::Handler(e)
    });
    metrics.total_ms = start.elapsed().as_secs_f64() * 1000.0;
    engine.last_metrics = Some(*metrics);
    handler
}
