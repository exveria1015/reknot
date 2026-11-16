//! Bounded inspection; no executable allocation or native callbacks here.
#![cfg_attr(
    not(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu")),
    allow(dead_code)
)]
use super::JitError;
use object::elf;
use object::endian::LittleEndian as LE;
use object::read::elf::{FileHeader, SectionHeader};
use std::collections::HashSet;

pub(super) fn error(message: impl Into<String>) -> JitError {
    JitError::Link(message.into())
}
fn parsed<T>(value: object::read::Result<T>) -> Result<T, JitError> {
    value.map_err(|e| error(e.to_string()))
}
fn size(value: u64) -> Result<usize, JitError> {
    usize::try_from(value).map_err(|_| error("ELF size exceeds address space"))
}
pub(super) fn aligned(value: usize, alignment: usize) -> Result<usize, JitError> {
    if !alignment.is_power_of_two() {
        return Err(error("invalid ELF alignment"));
    }
    value
        .checked_add(alignment - 1)
        .map(|n| n & !(alignment - 1))
        .ok_or_else(|| error("ELF layout overflow"))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Protection {
    Discard,
    Read,
    Write,
    Execute,
}
pub(super) struct Section<'a> {
    pub data: &'a [u8],
    pub size: usize,
    pub alignment: usize,
    pub protection: Protection,
    pub relocations: Vec<Relocation>,
}
#[derive(Clone, Copy, Debug)]
pub(super) enum Definition {
    Section(usize),
    Absolute,
    External,
}
pub(super) struct Symbol<'a> {
    pub name: &'a [u8],
    pub value: usize,
    pub definition: Definition,
    pub weak: bool,
}
#[derive(Clone, Copy)]
pub(super) struct Relocation {
    pub offset: usize,
    pub symbol: usize,
    pub kind: u32,
    pub addend: i64,
}
pub(super) struct Object<'a> {
    pub sections: Vec<Section<'a>>,
    pub symbols: Vec<Symbol<'a>>,
    pub entry: usize,
    pub unwind: usize,
    pub unwind_entries: usize,
}

impl<'a> Object<'a> {
    pub fn parse(bytes: &'a [u8], max_image_bytes: usize) -> Result<Self, JitError> {
        let header = parsed(elf::FileHeader64::<LE>::parse(bytes))?;
        if header.e_ident.class != elf::ELFCLASS64
            || header.e_ident.data != elf::ELFDATA2LSB
            || !matches!(
                header.e_ident.os_abi,
                elf::ELFOSABI_SYSV | elf::ELFOSABI_GNU
            )
            || header.e_ident.abi_version != 0
            || header.e_ident.version != elf::EV_CURRENT
            || header.e_version.get(LE) != elf::EV_CURRENT as u32
            || header.e_type.get(LE) != elf::ET_REL
            || header.e_machine.get(LE) != elf::EM_X86_64
            || header.e_flags.get(LE) != 0
            || header.e_ehsize.get(LE) != 64
            || header.e_shentsize.get(LE) != 64
            || header.e_phnum.get(LE) != 0
            || header.e_phoff.get(LE) != 0
            || header.e_entry.get(LE) != 0
        {
            return Err(error(
                "expected little-endian x86-64 ELF relocatable object",
            ));
        }
        let count = header.e_shnum.get(LE) as usize;
        if count == 0 || count > 8192 {
            return Err(error("ELF section quota/extended numbering is unsupported"));
        }
        let table = parsed(header.sections(LE, bytes))?;
        let mut sections = Vec::with_capacity(count);
        let mut symtab = None;
        let mut unwind = None;
        let mut total = 0usize;
        for (index, raw) in table.iter().enumerate() {
            let name = parsed(table.section_name(LE, raw))?;
            let flags = raw.sh_flags.get(LE);
            let typ = raw.sh_type.get(LE);
            let section_size = size(raw.sh_size.get(LE))?;
            let alignment = size(raw.sh_addralign.get(LE))?.max(1);
            if !alignment.is_power_of_two() || alignment > max_image_bytes {
                return Err(error("invalid/excessive ELF section alignment"));
            }
            if flags & elf::SHF_TLS as u64 != 0
                || name.starts_with(b".tdata")
                || name.starts_with(b".tbss")
            {
                return Err(error("compiler TLS in ELF logic"));
            }
            if matches!(
                typ,
                elf::SHT_INIT_ARRAY | elf::SHT_FINI_ARRAY | elf::SHT_PREINIT_ARRAY
            ) || [b".init".as_slice(), b".fini", b".ctors", b".dtors"]
                .iter()
                .any(|prefix| name.starts_with(prefix))
            {
                return Err(error("startup/destructor section in ELF logic"));
            }
            if matches!(
                typ,
                elf::SHT_REL
                    | elf::SHT_RELR
                    | elf::SHT_CREL
                    | elf::SHT_DYNSYM
                    | elf::SHT_DYNAMIC
                    | elf::SHT_SYMTAB_SHNDX
            ) {
                return Err(error("unsupported ELF section type/relocation format"));
            }
            let protection = if flags & elf::SHF_ALLOC as u64 == 0 {
                if name == b".note.GNU-stack" && flags & elf::SHF_EXECINSTR as u64 != 0 {
                    return Err(error("executable stack requested by ELF object"));
                }
                Protection::Discard
            } else {
                let allowed = (elf::SHF_ALLOC
                    | elf::SHF_WRITE
                    | elf::SHF_EXECINSTR
                    | elf::SHF_MERGE
                    | elf::SHF_STRINGS
                    | elf::SHF_GROUP
                    | elf::SHF_LINK_ORDER) as u64;
                if flags & !allowed != 0
                    || raw.sh_addr.get(LE) != 0
                    || !matches!(
                        typ,
                        elf::SHT_PROGBITS | elf::SHT_NOBITS | elf::SHT_X86_64_UNWIND
                    )
                {
                    return Err(error("unsupported allocated ELF section"));
                }
                total = total
                    .checked_add(section_size)
                    .ok_or_else(|| error("ELF size overflow"))?;
                if total > max_image_bytes {
                    return Err(error("ELF image budget exceeded (including BSS)"));
                }
                if flags & elf::SHF_EXECINSTR as u64 != 0 {
                    if flags & elf::SHF_WRITE as u64 != 0 || typ == elf::SHT_NOBITS {
                        return Err(error("writable/uninitialized executable ELF section"));
                    }
                    Protection::Execute
                } else if flags & elf::SHF_WRITE as u64 != 0 {
                    Protection::Write
                } else {
                    Protection::Read
                }
            };
            let data = parsed(raw.data(LE, bytes))?;
            if typ == elf::SHT_SYMTAB
                && (symtab.replace(index).is_some()
                    || raw.sh_entsize.get(LE) != 24
                    || section_size == 0
                    || section_size % 24 != 0
                    || section_size / 24 > 1_000_000)
            {
                return Err(error("invalid ELF symbol table/quota"));
            }
            if name == b".eh_frame"
                && (unwind.replace(index).is_some() || protection != Protection::Read)
            {
                return Err(error("invalid ELF .eh_frame section"));
            }
            sections.push(Section {
                data,
                size: section_size,
                alignment,
                protection,
                relocations: Vec::new(),
            });
        }
        if sections[0].protection != Protection::Discard || !sections[0].data.is_empty() {
            return Err(error("invalid ELF null section"));
        }
        let symtab = symtab.ok_or_else(|| error("missing ELF symbol table"))?;
        let raw_symbols = parsed(table.symbols(LE, bytes, elf::SHT_SYMTAB))?;
        let mut symbols = Vec::with_capacity(raw_symbols.len());
        let mut definitions = HashSet::new();
        let mut entry = None;
        for (index, raw) in raw_symbols.iter().enumerate() {
            let name = parsed(raw_symbols.symbol_name(LE, raw))?;
            if raw.st_type() == elf::STT_TLS
                || name == b"__tls_get_addr"
                || name.starts_with(b"__emutls_")
                || name.starts_with(b"__cxa_thread_atexit")
            {
                return Err(error("compiler TLS symbol in ELF logic"));
            }
            if !matches!(
                raw.st_type(),
                elf::STT_NOTYPE
                    | elf::STT_OBJECT
                    | elf::STT_FUNC
                    | elf::STT_SECTION
                    | elf::STT_FILE
            ) || !matches!(
                raw.st_bind(),
                elf::STB_LOCAL | elf::STB_GLOBAL | elf::STB_WEAK
            ) {
                return Err(error("unsupported ELF symbol type/binding"));
            }
            let value = size(raw.st_value.get(LE))?;
            let definition = match raw.st_shndx.get(LE) {
                elf::SHN_UNDEF => {
                    if value != 0
                        || (index != 0
                            && (raw.st_bind() == elf::STB_LOCAL
                                || raw.st_visibility() != elf::STV_DEFAULT))
                    {
                        return Err(error("unsupported undefined ELF symbol"));
                    }
                    Definition::External
                }
                elf::SHN_ABS => Definition::Absolute,
                n if n < elf::SHN_LORESERVE => {
                    let section = sections
                        .get(n as usize)
                        .ok_or_else(|| error("ELF symbol section outside table"))?;
                    if value
                        .checked_add(size(raw.st_size.get(LE))?)
                        .is_none_or(|end| end > section.size)
                    {
                        return Err(error("ELF symbol outside section"));
                    }
                    if raw.st_bind() != elf::STB_LOCAL && !definitions.insert(name) {
                        return Err(error(
                            "duplicate ELF definition requires unsupported symbol selection",
                        ));
                    }
                    if name == b"hot_handler_api"
                        && raw.st_bind() == elf::STB_GLOBAL
                        && raw.st_type() == elf::STT_FUNC
                        && (section.protection != Protection::Execute
                            || value >= section.size
                            || entry.replace(index).is_some())
                    {
                        return Err(error("ELF handler entry is not unique local code"));
                    }
                    Definition::Section(n as usize)
                }
                _ => return Err(error("common/extended ELF symbols are unsupported")),
            };
            symbols.push(Symbol {
                name,
                value,
                definition,
                weak: raw.st_bind() == elf::STB_WEAK,
            });
        }
        let mut relocation_count = 0usize;
        for raw in table.iter().filter(|s| s.sh_type.get(LE) == elf::SHT_RELA) {
            if raw.sh_entsize.get(LE) != 24
                || raw.sh_size.get(LE) % 24 != 0
                || raw.sh_link.get(LE) as usize != symtab
            {
                return Err(error("invalid ELF relocation table"));
            }
            relocation_count = relocation_count
                .checked_add(size(raw.sh_size.get(LE))? / 24)
                .ok_or_else(|| error("ELF relocation quota exceeded"))?;
            if relocation_count > 1_000_000 {
                return Err(error("ELF relocation quota exceeded"));
            }
            let target = sections
                .get_mut(raw.sh_info.get(LE) as usize)
                .ok_or_else(|| error("ELF relocation target outside table"))?;
            let (relocations, _) = parsed(raw.rela(LE, bytes))?.unwrap();
            // Debug records have no runtime effect. Their raw bounds were checked.
            if target.protection == Protection::Discard {
                continue;
            }
            for raw in relocations {
                let kind = raw.r_type(LE, false);
                let width = relocation_width(kind)?;
                if width == 0 {
                    continue;
                }
                let offset = size(raw.r_offset.get(LE))?;
                let symbol = raw.r_sym(LE, false) as usize;
                if offset
                    .checked_add(width)
                    .is_none_or(|end| end > target.size)
                    || symbol == 0
                    || symbol >= symbols.len()
                {
                    return Err(error("ELF relocation outside section/symbol table"));
                }
                target.relocations.push(Relocation {
                    offset,
                    symbol,
                    kind,
                    addend: raw.r_addend.get(LE),
                });
            }
            target.relocations.sort_unstable_by_key(|r| r.offset);
            if target
                .relocations
                .windows(2)
                .any(|r| r[0].offset + relocation_width(r[0].kind).unwrap() > r[1].offset)
            {
                return Err(error("overlapping ELF relocation writes"));
            }
        }
        let unwind = unwind.ok_or_else(|| error("ELF logic requires .eh_frame unwind tables"))?;
        let unwind_entries = validate_eh_frame(sections[unwind].data)?;
        Ok(Self {
            sections,
            symbols,
            entry: entry.ok_or_else(|| error("missing hot_handler_api"))?,
            unwind,
            unwind_entries,
        })
    }
}

pub(super) fn relocation_width(kind: u32) -> Result<usize, JitError> {
    match kind {
        elf::R_X86_64_NONE => Ok(0),
        elf::R_X86_64_64 | elf::R_X86_64_PC64 => Ok(8),
        elf::R_X86_64_PC32
        | elf::R_X86_64_PLT32
        | elf::R_X86_64_32
        | elf::R_X86_64_32S
        | elf::R_X86_64_GOTPCREL
        | elf::R_X86_64_GOTPCRELX
        | elf::R_X86_64_REX_GOTPCRELX => Ok(4),
        _ => Err(error(format!(
            "unsupported ELF relocation {kind} (including TLS/dynamic relocations)"
        ))),
    }
}
pub(super) fn uses_got(kind: u32) -> bool {
    matches!(
        kind,
        elf::R_X86_64_GOTPCREL | elf::R_X86_64_GOTPCRELX | elf::R_X86_64_REX_GOTPCRELX
    )
}

/// Bounds-check DWARF32 record envelopes and backward CIE references. This does
/// not certify arbitrary CFI instructions: unwind metadata shares native-code trust.
pub(super) fn validate_eh_frame(bytes: &[u8]) -> Result<usize, JitError> {
    let mut at = 0usize;
    let mut cies = HashSet::new();
    let mut entries = 0;
    while at < bytes.len() {
        let length = bytes
            .get(at..at + 4)
            .ok_or_else(|| error("truncated .eh_frame length"))?;
        let length = u32::from_le_bytes(length.try_into().unwrap()) as usize;
        if length == 0 {
            if bytes[at..].iter().any(|b| *b != 0) {
                return Err(error("data after .eh_frame terminator"));
            }
            break;
        }
        let end = at
            .checked_add(4)
            .and_then(|n| n.checked_add(length))
            .filter(|end| *end <= bytes.len())
            .ok_or_else(|| error("truncated .eh_frame record"))?;
        if length < 8 || length == u32::MAX as usize {
            return Err(error("unsupported .eh_frame record"));
        }
        let cie = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().unwrap()) as usize;
        if cie == 0 {
            if bytes[at + 8] != 1 {
                return Err(error("unsupported .eh_frame CIE version"));
            }
            cies.insert(at);
        } else {
            if !(at + 4)
                .checked_sub(cie)
                .is_some_and(|offset| cies.contains(&offset))
            {
                return Err(error("invalid .eh_frame CIE reference"));
            }
            entries += 1;
        }
        at = end;
    }
    if entries == 0 {
        return Err(error("ELF .eh_frame has no FDE records"));
    }
    Ok(entries)
}

/// psABI RELA: explicit signed addend, P is the address of the relocation field.
pub(super) fn relocate(
    bytes: &mut [u8],
    kind: u32,
    target: usize,
    addend: i64,
    place: usize,
) -> Result<(), JitError> {
    let absolute = target as i128 + addend as i128;
    let relative = absolute - place as i128;
    let overflow = || error("ELF relocation overflow (compile PIC with the small code model)");
    match kind {
        elf::R_X86_64_64 => bytes.copy_from_slice(
            &u64::try_from(absolute)
                .map_err(|_| overflow())?
                .to_le_bytes(),
        ),
        elf::R_X86_64_PC64 => bytes.copy_from_slice(
            &i64::try_from(relative)
                .map_err(|_| overflow())?
                .to_le_bytes(),
        ),
        elf::R_X86_64_32 => bytes.copy_from_slice(
            &u32::try_from(absolute)
                .map_err(|_| overflow())?
                .to_le_bytes(),
        ),
        elf::R_X86_64_32S => bytes.copy_from_slice(
            &i32::try_from(absolute)
                .map_err(|_| overflow())?
                .to_le_bytes(),
        ),
        elf::R_X86_64_PC32
        | elf::R_X86_64_PLT32
        | elf::R_X86_64_GOTPCREL
        | elf::R_X86_64_GOTPCRELX
        | elf::R_X86_64_REX_GOTPCRELX => bytes.copy_from_slice(
            &i32::try_from(relative)
                .map_err(|_| overflow())?
                .to_le_bytes(),
        ),
        _ => return Err(error("unsupported ELF relocation")),
    }
    Ok(())
}
