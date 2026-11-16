//! Windows allocation/protection, explicit DLL resolution and SEH registration.
use super::format::{aligned, error, Definition, Object, Protection};
use super::{CoffEngine, JitError, JitLoadMetrics};
use crate::LoadedHandler;
use object::{endian::LittleEndian as LE, pe};
use std::collections::HashMap;
use std::ffi::CString;
use std::io::Read;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::ptr::NonNull;
use std::time::Instant;
use windows_sys::Win32::System::Diagnostics::Debug::{
    FlushInstructionCache, RtlAddFunctionTable, RtlDeleteFunctionTable,
    IMAGE_RUNTIME_FUNCTION_ENTRY, IMAGE_RUNTIME_FUNCTION_ENTRY_0,
};
use windows_sys::Win32::System::LibraryLoader::{
    GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, LOAD_LIBRARY_SEARCH_SYSTEM32,
};
use windows_sys::Win32::System::Memory::*;
use windows_sys::Win32::System::SystemInformation::{GetSystemInfo, SYSTEM_INFO};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

#[path = "identity_windows.rs"]
mod identity_windows;
pub(super) use identity_windows::open_verified_foundation;
#[path = "retention_windows.rs"]
pub(super) mod retention;

// The MSVC CRT defines this actual table in std_type_info_static.obj. Referencing
// it pulls the real definition into the host; no fabricated RTTI/personality.
unsafe extern "C" {
    #[link_name = "??_7type_info@@6B@"]
    static TYPE_INFO_VTABLE: u8;
}

fn os_error(operation: &str) -> JitError {
    error(format!("{operation}: {}", std::io::Error::last_os_error()))
}
#[derive(Clone)]
pub(super) struct Runtime {
    // LoadLibrary reference counts are intentionally never decremented, including
    // partially successful opens. Handles are not arbitrary process-wide scopes.
    modules: Vec<usize>,
    cache: HashMap<Vec<u8>, usize>,
}
impl Runtime {
    pub unsafe fn open(std: &Path, support: &Path) -> Result<Self, JitError> {
        let paths = [std, support].map(|path| {
            let path = std::fs::canonicalize(path).map_err(JitError::Io)?;
            let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
            if wide.contains(&0) {
                return Err(error("NUL in resident DLL path"));
            }
            wide.push(0);
            Ok(wide)
        });
        // Validate both paths before either initializer runs.
        let [std, support] = paths;
        let paths = [std?, support?];
        let mut modules = Vec::new();
        for path in paths {
            let module = LoadLibraryExW(
                path.as_ptr(),
                std::ptr::null_mut(),
                LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32,
            );
            if module.is_null() {
                return Err(os_error("loading resident DLL"));
            }
            modules.push(module as usize);
        }
        for name in [
            "vcruntime140.dll",
            "ucrtbase.dll",
            "kernel32.dll",
            "ntdll.dll",
            "userenv.dll",
            "ws2_32.dll",
            "bcryptprimitives.dll",
            "api-ms-win-core-synch-l1-2-0.dll",
        ] {
            let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
            let module = LoadLibraryExW(
                wide.as_ptr(),
                std::ptr::null_mut(),
                LOAD_LIBRARY_SEARCH_SYSTEM32,
            );
            if module.is_null() {
                return Err(os_error("loading fixed Windows runtime DLL"));
            }
            modules.push(module as usize);
        }
        Ok(Self {
            modules,
            cache: HashMap::new(),
        })
    }

    fn find(&mut self, name: &[u8]) -> Result<usize, JitError> {
        if let Some(address) = self.cache.get(name) {
            return Ok(*address);
        }
        let address = if name == b"??_7type_info@@6B@" {
            &raw const TYPE_INFO_VTABLE as usize
        } else {
            let terminated = CString::new(name).map_err(|_| error("NUL in import"))?;
            self.modules
                .iter()
                .find_map(|module| unsafe {
                    GetProcAddress(*module as _, terminated.as_ptr().cast()).map(|p| p as usize)
                })
                .ok_or_else(|| {
                    error(format!(
                        "unresolved import: {}",
                        String::from_utf8_lossy(name)
                    ))
                })?
        };
        self.cache.insert(name.to_vec(), address);
        Ok(address)
    }
}

fn executable(address: usize) -> bool {
    let mut info = MEMORY_BASIC_INFORMATION::default();
    unsafe {
        VirtualQuery(address as _, &mut info, std::mem::size_of_val(&info)) != 0
            && info.State == MEM_COMMIT
            && info.Protect
                & (PAGE_EXECUTE
                    | PAGE_EXECUTE_READ
                    | PAGE_EXECUTE_READWRITE
                    | PAGE_EXECUTE_WRITECOPY)
                != 0
    }
}

#[derive(Clone, Copy)]
enum Binding {
    Section { index: usize, value: u32 },
    Absolute(usize),
    ImageBase,
    External { address: usize, import: bool },
}
fn resolve(
    index: usize,
    object: &Object<'_>,
    runtime: &mut Runtime,
    bindings: &mut [Option<Binding>],
    depth: usize,
) -> Result<Binding, JitError> {
    if depth > 64 {
        return Err(error("cyclic/overlong symbol resolution"));
    }
    if let Some(binding) = bindings[index] {
        return Ok(binding);
    }
    let symbol = object.symbols[index]
        .as_ref()
        .ok_or_else(|| error("auxiliary symbol reference"))?;
    let binding = match symbol.definition {
        Definition::Section(index) => {
            if object.sections[index].protection == Protection::Discard {
                return Err(error("runtime reference to discarded/debug section"));
            }
            Binding::Section {
                index,
                value: symbol.value,
            }
        }
        Definition::Absolute => Binding::Absolute(symbol.value as usize),
        Definition::Alias(target) => resolve(target, object, runtime, bindings, depth + 1)?,
        Definition::External => {
            if let Some(target) = object.exports.get(symbol.name) {
                resolve(*target, object, runtime, bindings, depth + 1)?
            } else if symbol.name == b"__ImageBase" {
                Binding::ImageBase
            } else {
                let plain = symbol.name.strip_prefix(b"__imp_");
                Binding::External {
                    address: runtime.find(plain.unwrap_or(symbol.name))?,
                    import: plain.is_some(),
                }
            }
        }
        Definition::Ignored => return Err(error("runtime reference to debug symbol")),
    };
    bindings[index] = Some(binding);
    Ok(binding)
}

struct Layout {
    sections: Vec<usize>,
    slots: Vec<Option<usize>>,
    stubs: Vec<Option<usize>>,
    // Each protection group occupies whole pages; no page is W+X.
    groups: Vec<(usize, usize, u32)>,
    size: usize,
    linked_bytes: usize,
}
impl Layout {
    fn new(
        object: &Object<'_>,
        bindings: &[Option<Binding>],
        limit: usize,
    ) -> Result<Self, JitError> {
        let mut info = SYSTEM_INFO::default();
        unsafe {
            GetSystemInfo(&mut info);
        }
        let page = info.dwPageSize as usize;
        if !page.is_power_of_two() {
            return Err(error("unsupported OS page size"));
        }
        let mut result = Self {
            sections: vec![0; object.sections.len()],
            slots: vec![None; bindings.len()],
            stubs: vec![None; bindings.len()],
            groups: Vec::new(),
            size: 0,
            linked_bytes: 0,
        };
        for (protection, flags) in [
            (Protection::Execute, PAGE_EXECUTE_READ),
            (Protection::Read, PAGE_READONLY),
            (Protection::Write, PAGE_READWRITE),
        ] {
            let begin = result.size;
            for (i, section) in object
                .sections
                .iter()
                .enumerate()
                .filter(|(_, s)| s.protection == protection)
            {
                result.size = aligned(result.size, section.alignment)?;
                result.sections[i] = result.size;
                // A zero-sized section still needs an in-image address for symbols.
                result.size = result
                    .size
                    .checked_add(section.size.max(1))
                    .ok_or_else(|| error("image layout overflow"))?;
                result.linked_bytes += section.size;
            }
            for (i, binding) in bindings.iter().enumerate() {
                if let Some(Binding::External { address, import }) = binding {
                    if protection == Protection::Read && *import {
                        result.size = aligned(result.size, 8)?;
                        result.slots[i] = Some(result.size);
                        result.size += 8;
                        result.linked_bytes += 8;
                    } else if protection == Protection::Execute && !import && executable(*address) {
                        result.size = aligned(result.size, 16)?;
                        result.stubs[i] = Some(result.size);
                        result.size += 14;
                        result.linked_bytes += 14;
                    }
                }
            }
            result.size = aligned(result.size, page)?;
            if result.size > limit {
                return Err(error("COFF image budget exceeded (layout/page padding)"));
            }
            if result.size > begin {
                result.groups.push((begin, result.size - begin, flags));
            }
        }
        if result.size == 0 {
            return Err(error("empty executable image"));
        }
        Ok(result)
    }
}

struct Image {
    base: NonNull<u8>,
    size: usize,
    unwind: Vec<IMAGE_RUNTIME_FUNCTION_ENTRY>,
    registered: bool,
    released: bool,
    #[cfg(test)]
    fail_release: u8,
}
impl Image {
    fn new(size: usize) -> Result<Self, JitError> {
        let base = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                size,
                MEM_RESERVE | MEM_COMMIT,
                PAGE_READWRITE,
            )
        };
        Ok(Self {
            base: NonNull::new(base.cast()).ok_or_else(|| os_error("VirtualAlloc"))?,
            size,
            unwind: Vec::new(),
            registered: false,
            released: false,
            #[cfg(test)]
            fail_release: 0,
        })
    }
    fn address(&self) -> usize {
        self.base.as_ptr() as usize
    }
    fn register(&mut self) -> Result<(), JitError> {
        if !unsafe {
            RtlAddFunctionTable(
                self.unwind.as_ptr(),
                self.unwind.len() as u32,
                self.address() as u64,
            )
        } {
            return Err(os_error("RtlAddFunctionTable"));
        }
        self.registered = true;
        Ok(())
    }
    fn release(&mut self) -> Result<(), (super::CoffRetentionReason, String)> {
        use super::CoffRetentionReason::*;
        if self.released {
            return Ok(());
        }
        unsafe {
            if self.registered {
                #[cfg(test)]
                if self.fail_release == 1 {
                    return Err((
                        UnwindRemovalFailed,
                        "injected unwind removal failure".into(),
                    ));
                }
                if !RtlDeleteFunctionTable(self.unwind.as_ptr()) {
                    return Err((
                        UnwindRemovalFailed,
                        os_error("RtlDeleteFunctionTable").to_string(),
                    ));
                }
                self.registered = false;
            }
            #[cfg(test)]
            if self.fail_release == 2 {
                return Err((
                    MemoryReleaseFailed,
                    "injected memory release failure".into(),
                ));
            }
            if VirtualFree(self.base.as_ptr().cast(), 0, MEM_RELEASE) == 0 {
                return Err((MemoryReleaseFailed, os_error("VirtualFree").to_string()));
            }
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
        if let Err((reason, _message)) = self.release() {
            // Unexecuted rollback can fail too. Preserve both the table and its
            // allocation and keep charging retention, without retry from Drop.
            let retained = Self {
                base: self.base,
                size: self.size,
                unwind: std::mem::take(&mut self.unwind),
                registered: self.registered,
                released: false,
                #[cfg(test)]
                fail_release: self.fail_release,
            };
            self.released = true;
            retention::retain_failed_rollback(retained, reason);
        }
    }
}

fn address(binding: Binding, symbol: usize, layout: &Layout, base: usize) -> usize {
    match binding {
        Binding::Section { index, value } => base + layout.sections[index] + value as usize,
        Binding::Absolute(value) => value,
        Binding::ImageBase => base,
        Binding::External { address, import } => {
            if import {
                base + layout.slots[symbol].unwrap()
            } else {
                address
            }
        }
    }
}
fn narrow<T: TryFrom<i128>>(value: i128) -> Result<T, JitError> {
    T::try_from(value).map_err(|_| error("COFF relocation overflow/out-of-range target"))
}

fn relocate(
    object: &Object<'_>,
    bindings: &[Option<Binding>],
    layout: &Layout,
    image: &mut Image,
) -> Result<(), JitError> {
    let base = image.address();
    // This exclusive view is dropped before page protections change or any code
    // executes. All write ranges were checked against the owning section.
    let bytes = unsafe { std::slice::from_raw_parts_mut(image.base.as_ptr(), image.size) };
    for (i, section) in object
        .sections
        .iter()
        .enumerate()
        .filter(|(_, s)| s.protection != Protection::Discard)
    {
        let offset = layout.sections[i];
        bytes[offset..offset + section.data.len()].copy_from_slice(section.data);
    }
    for (i, binding) in bindings.iter().enumerate() {
        if let Some(Binding::External { address, .. }) = binding {
            if let Some(slot) = layout.slots[i] {
                bytes[slot..slot + 8].copy_from_slice(&(*address as u64).to_le_bytes());
            }
            if let Some(stub) = layout.stubs[i] {
                bytes[stub..stub + 6].copy_from_slice(&[0xff, 0x25, 0, 0, 0, 0]);
                bytes[stub + 6..stub + 14].copy_from_slice(&(*address as u64).to_le_bytes());
            }
        }
    }
    for (i, section) in object
        .sections
        .iter()
        .enumerate()
        .filter(|(_, s)| s.protection != Protection::Discard)
    {
        for reloc in section.relocations {
            let kind = reloc.typ.get(LE);
            if kind == pe::IMAGE_REL_AMD64_ABSOLUTE {
                continue;
            }
            let symbol = reloc.symbol_table_index.get(LE) as usize;
            let binding = bindings[symbol].ok_or_else(|| error("missing relocation binding"))?;
            let at = layout.sections[i] + reloc.virtual_address.get(LE) as usize;
            let mut target = address(binding, symbol, layout, base) as i128;
            // PC-relative calls and SEH handler RVAs need an image-local code
            // address. Never turn an external data address into a jump stub.
            if kind == pe::IMAGE_REL_AMD64_ADDR32NB
                || (pe::IMAGE_REL_AMD64_REL32..=pe::IMAGE_REL_AMD64_REL32_5).contains(&kind)
            {
                if let Some(stub) = layout.stubs[symbol] {
                    target = (base + stub) as i128;
                }
            }
            match kind {
                pe::IMAGE_REL_AMD64_ADDR64 => {
                    let addend = i64::from_le_bytes(bytes[at..at + 8].try_into().unwrap()) as i128;
                    bytes[at..at + 8]
                        .copy_from_slice(&narrow::<u64>(target + addend)?.to_le_bytes());
                }
                pe::IMAGE_REL_AMD64_SECTION => {
                    let Binding::Section { index, .. } = binding else {
                        return Err(error("SECTION relocation requires local section"));
                    };
                    let addend = u16::from_le_bytes(bytes[at..at + 2].try_into().unwrap()) as i128;
                    bytes[at..at + 2]
                        .copy_from_slice(&narrow::<u16>(index as i128 + 1 + addend)?.to_le_bytes());
                }
                pe::IMAGE_REL_AMD64_REL32..=pe::IMAGE_REL_AMD64_REL32_5 => {
                    let addend = i32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as i128;
                    let value = target + addend
                        - (base + at + 4) as i128
                        - (kind - pe::IMAGE_REL_AMD64_REL32) as i128;
                    bytes[at..at + 4].copy_from_slice(&narrow::<i32>(value)?.to_le_bytes());
                }
                _ => {
                    let addend = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as i128;
                    let value = match kind {
                        pe::IMAGE_REL_AMD64_ADDR32 => target + addend,
                        pe::IMAGE_REL_AMD64_ADDR32NB => target + addend - base as i128,
                        pe::IMAGE_REL_AMD64_SECREL => {
                            let Binding::Section { value, .. } = binding else {
                                return Err(error("SECREL relocation requires local section"));
                            };
                            value as i128 + addend
                        }
                        _ => return Err(error("unsupported relocation")),
                    };
                    bytes[at..at + 4].copy_from_slice(&narrow::<u32>(value)?.to_le_bytes());
                }
            }
        }
    }
    Ok(())
}

fn inside(object: &Object<'_>, layout: &Layout, rva: usize, size: usize, execute: bool) -> bool {
    object.sections.iter().enumerate().any(|(i, s)| {
        s.protection != Protection::Discard
            && (!execute || s.protection == Protection::Execute)
            && rva >= layout.sections[i]
            && rva
                .checked_add(size)
                .is_some_and(|end| end <= layout.sections[i] + s.size)
    }) || (execute
        && size <= 14
        && layout
            .stubs
            .iter()
            .flatten()
            .any(|s| rva >= *s && rva + size <= *s + 14))
}

fn unwind(object: &Object<'_>, layout: &Layout, image: &mut Image) -> Result<(), JitError> {
    let bytes = unsafe { std::slice::from_raw_parts(image.base.as_ptr(), image.size) };
    let u32_at = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
    for (i, section) in object
        .sections
        .iter()
        .enumerate()
        .filter(|(_, s)| s.name == b".pdata" || s.name.starts_with(b".pdata$"))
    {
        if section.protection != Protection::Read || section.size % 12 != 0 {
            return Err(error("invalid .pdata size/protection"));
        }
        for at in (layout.sections[i]..layout.sections[i] + section.size).step_by(12) {
            let begin = u32_at(at);
            let end = u32_at(at + 4);
            let info = u32_at(at + 8);
            if begin >= end
                || !inside(object, layout, begin as usize, (end - begin) as usize, true)
                || info % 4 != 0
                || !inside(object, layout, info as usize, 4, false)
            {
                return Err(error("invalid RUNTIME_FUNCTION range/unwind RVA"));
            }
            let info_at = info as usize;
            let version = bytes[info_at] & 7;
            let flags = bytes[info_at] >> 3;
            let codes_size = aligned(4 + bytes[info_at + 2] as usize * 2, 4)?;
            let extra = if flags & 4 != 0 {
                12
            } else if flags & 3 != 0 {
                4
            } else {
                0
            };
            if version != 1
                || flags > 7
                || (flags & 4 != 0 && flags & 3 != 0)
                || !inside(object, layout, info_at, codes_size + extra, false)
            {
                return Err(error("unsupported/truncated x64 UNWIND_INFO"));
            }
            if flags & 3 != 0
                && !inside(
                    object,
                    layout,
                    u32_at(info_at + codes_size) as usize,
                    1,
                    true,
                )
            {
                return Err(error("unwind handler is not image-local code"));
            }
            // Chained unwind records need recursive validation; reject until that
            // profile is implemented rather than handing unchecked chains to OS.
            if flags & 4 != 0 {
                return Err(error("chained UNWIND_INFO is not supported"));
            }
            image.unwind.push(IMAGE_RUNTIME_FUNCTION_ENTRY {
                BeginAddress: begin,
                EndAddress: end,
                Anonymous: IMAGE_RUNTIME_FUNCTION_ENTRY_0 { UnwindData: info },
            });
        }
    }
    if image.unwind.is_empty() {
        return Err(error("logic object has no .pdata unwind table"));
    }
    image
        .unwind
        .sort_unstable_by_key(|entry| entry.BeginAddress);
    if image
        .unwind
        .windows(2)
        .any(|pair| pair[0].EndAddress > pair[1].BeginAddress)
    {
        return Err(error("overlapping RUNTIME_FUNCTION entries"));
    }
    Ok(())
}

struct Linked {
    image: Image,
    entry: usize,
    #[cfg(test)]
    symbols: HashMap<Vec<u8>, usize>,
}
struct LinkPlan {
    bindings: Vec<Option<Binding>>,
    entry_binding: Binding,
    entry_index: usize,
    layout: Layout,
}
fn plan_link(
    runtime: &mut Runtime,
    object: &Object<'_>,
    limit: usize,
) -> Result<LinkPlan, JitError> {
    let mut bindings = vec![None; object.symbols.len()];
    for section in object
        .sections
        .iter()
        .filter(|s| s.protection != Protection::Discard)
    {
        for reloc in section.relocations {
            if reloc.typ.get(LE) != pe::IMAGE_REL_AMD64_ABSOLUTE {
                resolve(
                    reloc.symbol_table_index.get(LE) as usize,
                    object,
                    runtime,
                    &mut bindings,
                    0,
                )?;
            }
        }
    }
    let entry_index = *object
        .exports
        .get(b"hot_handler_api".as_slice())
        .ok_or_else(|| error("missing hot_handler_api"))?;
    let entry_binding = resolve(entry_index, object, runtime, &mut bindings, 0)?;
    let Binding::Section { index, value } = entry_binding else {
        return Err(error("entry is not local code"));
    };
    if object.sections[index].protection != Protection::Execute
        || value as usize >= object.sections[index].size
    {
        return Err(error("entry outside executable section"));
    }
    let layout = Layout::new(object, &bindings, limit)?;
    Ok(LinkPlan {
        bindings,
        entry_binding,
        entry_index,
        layout,
    })
}

#[cfg(feature = "jit-dev")]
pub(super) fn required_image_bytes(
    engine: &mut CoffEngine,
    bytes: &[u8],
) -> Result<usize, JitError> {
    let object = Object::parse(bytes, engine.limits.max_image_bytes)?;
    Ok(
        plan_link(&mut engine.runtime, &object, engine.limits.max_image_bytes)?
            .layout
            .size,
    )
}

fn link(
    runtime: &mut Runtime,
    object: &Object<'_>,
    limit: usize,
    metrics: &mut JitLoadMetrics,
    register: impl FnOnce(&mut Image) -> Result<(), JitError>,
) -> Result<Linked, JitError> {
    let LinkPlan {
        bindings,
        entry_binding,
        entry_index,
        layout,
    } = plan_link(runtime, object, limit)?;
    let mut image = Image::new(layout.size)?;
    relocate(object, &bindings, &layout, &mut image)?;
    unwind(object, &layout, &mut image)?;
    for &(begin, size, flags) in &layout.groups {
        let mut previous = 0;
        if unsafe {
            VirtualProtect(
                image.base.as_ptr().add(begin).cast(),
                size,
                flags,
                &mut previous,
            )
        } == 0
        {
            return Err(os_error("VirtualProtect"));
        }
    }
    if unsafe { FlushInstructionCache(GetCurrentProcess(), image.base.as_ptr().cast(), image.size) }
        == 0
    {
        return Err(os_error("FlushInstructionCache"));
    }
    let before = Instant::now();
    register(&mut image)?;
    metrics.unwind_registration_ms = before.elapsed().as_secs_f64() * 1000.0;
    metrics.unwind_entries = image.unwind.len() as u64;
    metrics.linked_bytes = layout.linked_bytes as u64;
    let entry = address(entry_binding, entry_index, &layout, image.address());
    #[cfg(test)]
    let symbols = object
        .exports
        .iter()
        .filter_map(|(name, index)| {
            let symbol = object.symbols[*index].as_ref()?;
            let Definition::Section(section) = symbol.definition else {
                return None;
            };
            (object.sections[section].protection != Protection::Discard).then(|| {
                (
                    name.to_vec(),
                    image.address() + layout.sections[section] + symbol.value as usize,
                )
            })
        })
        .collect();
    Ok(Linked {
        image,
        entry,
        #[cfg(test)]
        symbols,
    })
}

pub(super) unsafe fn load(engine: &mut CoffEngine, path: &Path) -> Result<LoadedHandler, JitError> {
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
        super::CoffPolicy::KeepLoaded,
        None,
        None,
    )
}

/// Caller has already consumed an attempt and, for the checked API, verified
/// identity. Never reopen a pathname after verifying these exact bytes.
pub(super) unsafe fn load_bytes(
    engine: &mut CoffEngine,
    bytes: &[u8],
    start: Instant,
    metrics: &mut JitLoadMetrics,
    policy: super::CoffPolicy,
    foundation: Option<&str>,
    verified_hash: Option<&str>,
) -> Result<LoadedHandler, JitError> {
    retention::check_budget(engine.limits, 1)?;
    if bytes.len() > engine.limits.load.max_object_bytes {
        return Err(JitError::ObjectTooLarge {
            limit: engine.limits.load.max_object_bytes,
        });
    }
    let before = Instant::now();
    let object = Object::parse(bytes, engine.limits.max_image_bytes)?;
    metrics.copy_ms = before.elapsed().as_secs_f64() * 1000.0;
    let before = Instant::now();
    let linked = link(
        &mut engine.runtime,
        &object,
        engine.limits.max_image_bytes,
        metrics,
        Image::register,
    )?;
    metrics.materialize_ms = before.elapsed().as_secs_f64() * 1000.0;
    let entry = linked.entry;
    #[cfg(test)]
    {
        engine.last_symbols = linked.symbols;
    }
    // Publication point: before the first callback, irreversibly retain code,
    // unwind table and their registration. Even an ABI rejection can have run
    // object code, so it must not trigger pre-publication rollback.
    let (owner, tracking) = retention::publish(
        linked.image,
        engine.limits,
        policy,
        foundation,
        verified_hash
            .map(str::to_owned)
            .unwrap_or_else(|| super::artifact::digest(bytes)),
    )?;
    engine.last_image = Some(tracking.id);
    let entry: reknot_abi::services::HandlerApiFn = std::mem::transmute(entry);
    let handler = LoadedHandler::from_entry(entry, owner).map_err(|e| {
        tracking.pin(super::CoffRetentionReason::EntryRejected);
        JitError::Handler(e)
    })?;
    metrics.total_ms = start.elapsed().as_secs_f64() * 1000.0;
    engine.last_metrics = Some(*metrics);
    Ok(handler)
}

#[cfg(test)]
mod tests {
    use super::super::format::tests::{minimal, put16, put32};
    use super::*;
    use windows_sys::Win32::System::Diagnostics::Debug::RtlLookupFunctionEntry;

    // Avoid an unrelated test reusing an address between VirtualFree and the
    // assertion that queries its state/registration.
    static ALLOCATION_TEST: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn runtime() -> Runtime {
        Runtime {
            modules: Vec::new(),
            cache: HashMap::new(),
        }
    }
    fn lookup(address: usize) -> bool {
        let mut base = 0;
        !unsafe { RtlLookupFunctionEntry(address as u64, &mut base, std::ptr::null_mut()) }
            .is_null()
    }
    fn state(address: usize) -> MEMORY_BASIC_INFORMATION {
        let mut info = MEMORY_BASIC_INFORMATION::default();
        assert_ne!(
            unsafe { VirtualQuery(address as _, &mut info, std::mem::size_of_val(&info)) },
            0
        );
        info
    }
    #[test]
    fn regular_and_bigobj_permissions_unwind_and_rollback() {
        let _serial = ALLOCATION_TEST.lock().unwrap();
        for big in [false, true] {
            let f = minimal(big);
            let object = Object::parse(&f.bytes, 65536).unwrap();
            let linked = link(
                &mut runtime(),
                &object,
                65536,
                &mut JitLoadMetrics::default(),
                Image::register,
            )
            .unwrap();
            assert_eq!(state(linked.entry).Protect, PAGE_EXECUTE_READ);
            assert_eq!(state(linked.image.address() + 4096).Protect, PAGE_READONLY);
            assert!(lookup(linked.entry));
            let address = linked.entry;
            drop(linked);
            assert!(
                !lookup(address),
                "registration must be removed before freeing image"
            );
            assert_eq!(state(address).State, MEM_FREE);
        }
    }
    #[test]
    fn registration_failure_and_post_registration_failure_do_not_publish() {
        let _serial = ALLOCATION_TEST.lock().unwrap();
        let f = minimal(false);
        let object = Object::parse(&f.bytes, 65536).unwrap();
        for register_first in [false, true] {
            let mut address = 0;
            let result = link(
                &mut runtime(),
                &object,
                65536,
                &mut JitLoadMetrics::default(),
                |image| {
                    address = image.address();
                    assert_eq!(state(address).Protect, PAGE_EXECUTE_READ);
                    if register_first {
                        image.register()?;
                        assert!(lookup(address));
                    }
                    Err(error("injected registration boundary failure"))
                },
            );
            assert!(result.is_err());
            assert_ne!(address, 0);
            assert!(!lookup(address));
            assert_eq!(state(address).State, MEM_FREE);
        }
    }
    #[test]
    fn missing_import_budget_overflow_and_bad_unwind_fail_before_registration() {
        let _serial = ALLOCATION_TEST.lock().unwrap();
        let f = minimal(false);
        let object = Object::parse(&f.bytes, 65536).unwrap();
        let never = |_: &mut Image| -> Result<(), JitError> {
            panic!("invalid input reached registration")
        };
        assert!(link(
            &mut runtime(),
            &object,
            4096,
            &mut JitLoadMetrics::default(),
            never
        )
        .is_err());
        let mut f = minimal(false);
        put32(&mut f.bytes, f.header + 120 + 5, u32::MAX); // RVA overflows after adding text offset below
        put32(&mut f.bytes, f.symbols + 18 + 8, 1);
        let object = Object::parse(&f.bytes, 65536).unwrap();
        assert!(
            matches!(link(&mut runtime(), &object, 65536, &mut JitLoadMetrics::default(), never), Err(JitError::Link(e)) if e.contains("overflow"))
        );
        let mut f = minimal(false);
        f.bytes[f.header + 120 + 1] = 0; // bad unwind version
        let object = Object::parse(&f.bytes, 65536).unwrap();
        assert!(link(
            &mut runtime(),
            &object,
            65536,
            &mut JitLoadMetrics::default(),
            never
        )
        .is_err());
        let mut f = minimal(false);
        put16(&mut f.bytes, f.symbols + 18 + 12, 0);
        f.bytes[f.symbols + 18 + 16] = pe::IMAGE_SYM_CLASS_EXTERNAL;
        let object = Object::parse(&f.bytes, 65536).unwrap();
        assert!(
            matches!(link(&mut runtime(), &object, 65536, &mut JitLoadMetrics::default(), never), Err(JitError::Link(e)) if e.contains("unresolved import"))
        );
    }
    #[test]
    fn external_code_gets_stubs_but_data_does_not() {
        let f = minimal(false);
        let object = Object::parse(&f.bytes, 65536).unwrap();
        let bindings = [
            Some(Binding::External {
                address: GetCurrentProcess as *const () as usize,
                import: false,
            }),
            Some(Binding::External {
                address: &raw const TYPE_INFO_VTABLE as usize,
                import: false,
            }),
            Some(Binding::External {
                address: &raw const TYPE_INFO_VTABLE as usize,
                import: true,
            }),
        ];
        let layout = Layout::new(&object, &bindings, 65536).unwrap();
        assert!(layout.stubs[0].is_some());
        assert!(layout.stubs[1].is_none());
        assert!(layout.slots[2].is_some());
    }

    #[test]
    fn managed_reclamation_seals_ownership_and_confirms_os_release() {
        use super::super::*;
        let _serial = ALLOCATION_TEST.lock().unwrap();
        for _ in 0..128 {
            let f = minimal(false);
            let object = Object::parse(&f.bytes, 65536).unwrap();
            let linked = link(
                &mut runtime(),
                &object,
                65536,
                &mut JitLoadMetrics::default(),
                Image::register,
            )
            .unwrap();
            let address = linked.entry;
            let (owner, tracking) = super::retention::publish(
                linked.image,
                CoffLimits::default(),
                CoffPolicy::Managed,
                None,
                "test".into(),
            )
            .unwrap();
            owner.activate();
            let lease = owner.lease();
            let guard = owner.enter();
            drop(owner);
            assert!(collect_retired_coff(CoffReclaimOptions::default())
                .released
                .is_empty());
            drop(lease);
            assert!(collect_retired_coff(CoffReclaimOptions::default())
                .released
                .is_empty());
            assert!(lookup(address));
            drop(guard);
            assert_eq!(
                collect_retired_coff(CoffReclaimOptions::default()).released,
                [tracking.id]
            );
            assert!(!lookup(address));
            assert_eq!(state(address).State, MEM_FREE);
            assert!(collect_retired_coff(CoffReclaimOptions::default())
                .released
                .is_empty());
        }
        assert_eq!(coff_retention_snapshot().retained_bytes, 0);
    }
    #[test]
    fn managed_failures_pin_without_retry_or_budget_refund() {
        use super::super::*;
        let _serial = ALLOCATION_TEST.lock().unwrap();
        for failure in [1, 2] {
            let f = minimal(false);
            let object = Object::parse(&f.bytes, 65536).unwrap();
            let mut linked = link(
                &mut runtime(),
                &object,
                65536,
                &mut JitLoadMetrics::default(),
                Image::register,
            )
            .unwrap();
            linked.image.fail_release = failure;
            let address = linked.entry;
            let (owner, tracking) = super::retention::publish(
                linked.image,
                CoffLimits::default(),
                CoffPolicy::Managed,
                None,
                "failure".into(),
            )
            .unwrap();
            drop(owner);
            let before = coff_retention_snapshot().retained_bytes;
            let report = collect_retired_coff(CoffReclaimOptions::default());
            assert_eq!(report.errors.len(), 1);
            assert_eq!(report.errors[0].0, tracking.id);
            assert_eq!(coff_retention_snapshot().retained_bytes, before);
            assert_eq!(lookup(address), failure == 1);
            assert_eq!(state(address).State, MEM_COMMIT);
            assert!(collect_retired_coff(CoffReclaimOptions::default())
                .errors
                .is_empty());
        }
    }
    #[test]
    fn owner_thread_exit_keeps_code_and_function_table() {
        use super::super::*;
        let _serial = ALLOCATION_TEST.lock().unwrap();
        let address = std::thread::spawn(|| {
            let f = minimal(false);
            let object = Object::parse(&f.bytes, 65536).unwrap();
            let linked = link(
                &mut runtime(),
                &object,
                65536,
                &mut JitLoadMetrics::default(),
                Image::register,
            )
            .unwrap();
            let address = linked.entry;
            let (owner, _) = super::retention::publish(
                linked.image,
                CoffLimits::default(),
                CoffPolicy::Managed,
                None,
                "exit".into(),
            )
            .unwrap();
            drop(owner);
            address
        })
        .join()
        .unwrap();
        assert!(lookup(address));
        assert_eq!(state(address).State, MEM_COMMIT);
    }
    #[test]
    fn unexecuted_rollback_failure_is_retained_and_charged() {
        use super::super::*;
        let _serial = ALLOCATION_TEST.lock().unwrap();
        for failure in [1, 2] {
            let f = minimal(false);
            let object = Object::parse(&f.bytes, 65536).unwrap();
            let mut linked = link(
                &mut runtime(),
                &object,
                65536,
                &mut JitLoadMetrics::default(),
                Image::register,
            )
            .unwrap();
            linked.image.fail_release = failure;
            let address = linked.entry;
            let size = linked.image.size;
            let before = coff_retention_snapshot().retained_bytes;
            drop(linked);
            let snapshot = coff_retention_snapshot();
            assert_eq!(snapshot.retained_bytes, before + size);
            assert!(snapshot
                .images
                .last()
                .unwrap()
                .reasons
                .iter()
                .any(|r| matches!(
                    r,
                    CoffRetentionReason::UnwindRemovalFailed
                        | CoffRetentionReason::MemoryReleaseFailed
                )));
            assert!(collect_retired_coff(CoffReclaimOptions::default())
                .released
                .is_empty());
            assert_eq!(lookup(address), failure == 1);
            assert_eq!(state(address).State, MEM_COMMIT);
        }
    }
    #[test]
    fn relocation_arithmetic_including_signed_addends_and_rel32_biases() {
        let _serial = ALLOCATION_TEST.lock().unwrap();
        for kind in [
            pe::IMAGE_REL_AMD64_ADDR64,
            pe::IMAGE_REL_AMD64_ADDR32,
            pe::IMAGE_REL_AMD64_ADDR32NB,
            pe::IMAGE_REL_AMD64_REL32,
            pe::IMAGE_REL_AMD64_REL32_1,
            pe::IMAGE_REL_AMD64_REL32_2,
            pe::IMAGE_REL_AMD64_REL32_3,
            pe::IMAGE_REL_AMD64_REL32_4,
            pe::IMAGE_REL_AMD64_REL32_5,
            pe::IMAGE_REL_AMD64_SECTION,
            pe::IMAGE_REL_AMD64_SECREL,
        ] {
            let mut f = minimal(false);
            put16(&mut f.bytes, f.header + 80 + 32, 1);
            put16(&mut f.bytes, f.relocs + 8, kind);
            let data = f.header + 120 + 5;
            f.bytes[data..data + 12].fill(0);
            if kind == pe::IMAGE_REL_AMD64_ADDR64 {
                f.bytes[data..data + 8].copy_from_slice(&(-1i64).to_le_bytes());
            } else {
                put32(&mut f.bytes, data, 3);
            }
            let object = Object::parse(&f.bytes, 65536).unwrap();
            let binding = if kind == pe::IMAGE_REL_AMD64_ADDR32 {
                Binding::Absolute(100)
            } else {
                Binding::Section { index: 0, value: 1 }
            };
            let bindings = [None, Some(binding), None];
            let layout = Layout::new(&object, &bindings, 65536).unwrap();
            let mut image = Image::new(layout.size).unwrap();
            relocate(&object, &bindings, &layout, &mut image).unwrap();
            let at = layout.sections[2];
            let bytes = unsafe { std::slice::from_raw_parts(image.base.as_ptr().add(at), 8) };
            if kind == pe::IMAGE_REL_AMD64_ADDR64 {
                assert_eq!(
                    u64::from_le_bytes(bytes.try_into().unwrap()),
                    image.address() as u64
                );
            } else if kind == pe::IMAGE_REL_AMD64_SECTION {
                assert_eq!(u16::from_le_bytes(bytes[..2].try_into().unwrap()), 4);
            } else if (pe::IMAGE_REL_AMD64_REL32..=pe::IMAGE_REL_AMD64_REL32_5).contains(&kind) {
                assert_eq!(
                    i32::from_le_bytes(bytes[..4].try_into().unwrap()),
                    -(at as i32) - (kind - pe::IMAGE_REL_AMD64_REL32) as i32
                );
            } else {
                assert_eq!(
                    u32::from_le_bytes(bytes[..4].try_into().unwrap()),
                    if kind == pe::IMAGE_REL_AMD64_ADDR32 {
                        103
                    } else {
                        4
                    }
                );
            }
        }
    }
}
