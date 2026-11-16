//! Bounded COFF inspection without executing or allocating executable memory.
#![cfg_attr(
    not(all(windows, target_arch = "x86_64", target_env = "msvc")),
    allow(dead_code)
)]
use super::JitError;
use object::endian::LittleEndian as LE;
use object::pe;
use object::read::coff::{CoffFile, CoffHeader, ImageSymbol};
use std::collections::HashMap;

pub(super) fn error(message: impl Into<String>) -> JitError {
    JitError::Link(message.into())
}
fn parsed<T>(result: object::read::Result<T>) -> Result<T, JitError> {
    result.map_err(|e| error(e.to_string()))
}
pub(super) fn aligned(value: usize, alignment: usize) -> Result<usize, JitError> {
    value
        .checked_add(alignment - 1)
        .map(|v| v & !(alignment - 1))
        .ok_or_else(|| error("COFF layout overflow"))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Protection {
    Execute,
    Read,
    Write,
    Discard,
}
pub(super) struct Section<'a> {
    pub name: &'a [u8],
    pub data: &'a [u8],
    pub size: usize,
    pub alignment: usize,
    pub protection: Protection,
    pub relocations: &'a [pe::ImageRelocation],
    comdat: bool,
    selection: Option<u8>,
    associate: Option<usize>,
}
#[derive(Clone, Copy, Debug)]
pub(super) enum Definition {
    Section(usize),
    Absolute,
    External,
    Alias(usize),
    Ignored,
}
pub(super) struct Symbol<'a> {
    pub name: &'a [u8],
    pub value: u32,
    pub definition: Definition,
}
pub(super) struct Object<'a> {
    pub sections: Vec<Section<'a>>,
    /// Auxiliary records are None and must never be relocation targets.
    pub symbols: Vec<Option<Symbol<'a>>>,
    pub exports: HashMap<&'a [u8], usize>,
}
impl<'a> Object<'a> {
    pub fn parse(bytes: &'a [u8], max_image_bytes: usize) -> Result<Self, JitError> {
        if bytes.starts_with(&[0, 0, 255, 255]) {
            let file = parsed(CoffFile::<_, pe::AnonObjectHeaderBigobj>::parse(bytes))?;
            let header = file.coff_header();
            if header.flags.get(LE) != 0
                || header.meta_data_size.get(LE) != 0
                || header.meta_data_offset.get(LE) != 0
            {
                return Err(error("bigobj metadata is not supported"));
            }
            Self::from_file(&file, bytes, max_image_bytes)
        } else {
            let file = parsed(CoffFile::<_, pe::ImageFileHeader>::parse(bytes))?;
            if file.coff_header().size_of_optional_header.get(LE) != 0 {
                return Err(error("optional headers/PE images are not logic COFF"));
            }
            Self::from_file(&file, bytes, max_image_bytes)
        }
    }

    fn from_file<C: CoffHeader>(
        file: &CoffFile<'a, &'a [u8], C>,
        bytes: &'a [u8],
        max_image_bytes: usize,
    ) -> Result<Self, JitError> {
        let header = file.coff_header();
        if header.machine() != pe::IMAGE_FILE_MACHINE_AMD64
            || header.characteristics() & (pe::IMAGE_FILE_EXECUTABLE_IMAGE | pe::IMAGE_FILE_DLL)
                != 0
        {
            return Err(error("expected relocatable x64 COFF"));
        }
        let symbol_count = header.number_of_symbols() as usize;
        if header.number_of_sections() == 0
            || header.number_of_sections() > 8192
            || symbol_count == 0
            || symbol_count > 1_000_000
        {
            return Err(error("COFF section/symbol quota exceeded or empty object"));
        }
        // object validates referenced strings lazily; validate the entire declared
        // table too, including objects with no long names.
        let record_size = if C::is_type_bigobj() { 20 } else { 18 };
        let strings_start = (header.pointer_to_symbol_table() as usize)
            .checked_add(symbol_count * record_size)
            .ok_or_else(|| error("symbol table overflow"))?;
        let length = bytes
            .get(strings_start..strings_start + 4)
            .ok_or_else(|| error("missing COFF string table"))?;
        let length = u32::from_le_bytes(length.try_into().unwrap()) as usize;
        if header.pointer_to_symbol_table() == 0
            || length < 4
            || strings_start
                .checked_add(length)
                .is_none_or(|end| end > bytes.len())
        {
            return Err(error("invalid COFF string table bounds"));
        }
        let table = file.coff_symbol_table();
        let mut sections = Vec::new();
        let mut total_size = 0usize;
        let mut relocations = 0usize;
        for raw in file.coff_section_table().iter() {
            let name = parsed(raw.name(table.strings()))?;
            if name.starts_with(b".tls") {
                return Err(error("native TLS section in logic"));
            }
            if [
                b".CRT".as_slice(),
                b".ctors",
                b".dtors",
                b".init_array",
                b".fini_array",
            ]
            .iter()
            .any(|p| name.starts_with(p))
            {
                return Err(error("startup/destructor section in logic"));
            }
            let flags = raw.characteristics.get(LE);
            let protection =
                if name.starts_with(b".debug") || name == b".llvm_addrsig" || name == b".drectve" {
                    Protection::Discard
                } else if flags & (pe::IMAGE_SCN_LNK_REMOVE | pe::IMAGE_SCN_MEM_DISCARDABLE) != 0 {
                    return Err(error("unsupported discardable runtime section"));
                } else if flags & pe::IMAGE_SCN_MEM_EXECUTE != 0 {
                    if flags & pe::IMAGE_SCN_MEM_WRITE != 0 {
                        return Err(error("writable executable section"));
                    }
                    Protection::Execute
                } else if flags & pe::IMAGE_SCN_MEM_WRITE != 0 {
                    Protection::Write
                } else {
                    Protection::Read
                };
            if flags & pe::IMAGE_SCN_ALIGN_MASK == pe::IMAGE_SCN_ALIGN_MASK
                || raw.virtual_address.get(LE) != 0
                || raw.virtual_size.get(LE) != 0
            {
                return Err(error("unsupported COFF alignment/virtual section layout"));
            }
            if flags & pe::IMAGE_SCN_LNK_NRELOC_OVFL != 0
                && raw.number_of_relocations.get(LE) != u16::MAX
            {
                return Err(error("invalid extended relocation count"));
            }
            let data = raw
                .coff_data(bytes)
                .map_err(|_| error("COFF section data outside file"))?;
            // Directives may annotate exports only; never load libraries or run
            // initializers requested by an object. Unknown directives fail closed.
            if name == b".drectve" {
                let text =
                    std::str::from_utf8(data).map_err(|_| error("non-UTF8 linker directive"))?;
                if text
                    .split_ascii_whitespace()
                    .any(|word| !word.starts_with("/EXPORT:") && !word.starts_with("-export:"))
                {
                    return Err(error("unsupported COFF linker directive"));
                }
            }
            let size = raw.size_of_raw_data.get(LE) as usize;
            if protection != Protection::Discard {
                total_size = total_size
                    .checked_add(size)
                    .ok_or_else(|| error("section size overflow"))?;
                if total_size > max_image_bytes {
                    return Err(error("COFF image budget exceeded (including BSS)"));
                }
            }
            let relocs = parsed(raw.coff_relocations(bytes))?;
            relocations += relocs.len();
            if relocations > 1_000_000 {
                return Err(error("COFF relocation quota exceeded"));
            }
            sections.push(Section {
                name,
                data,
                size,
                alignment: raw.coff_alignment() as usize,
                protection,
                relocations: relocs,
                comdat: flags & pe::IMAGE_SCN_LNK_COMDAT != 0,
                selection: None,
                associate: None,
            });
        }
        let mut symbols: Vec<Option<Symbol<'a>>> = (0..symbol_count).map(|_| None).collect();
        let mut exports = HashMap::new();
        for (index, raw) in table.iter() {
            if index.0 + 1 + raw.number_of_aux_symbols() as usize > symbol_count {
                return Err(error("truncated auxiliary symbol records"));
            }
            let name = parsed(raw.name(table.strings()))?;
            reject_tls(name)?;
            let section_number = raw.section_number();
            let definition = match section_number {
                n if n > 0 => {
                    if !matches!(
                        raw.storage_class(),
                        pe::IMAGE_SYM_CLASS_STATIC
                            | pe::IMAGE_SYM_CLASS_EXTERNAL
                            | pe::IMAGE_SYM_CLASS_LABEL
                    ) {
                        return Err(error("unsupported defined symbol storage class"));
                    }
                    let index = n as usize - 1;
                    let section = sections
                        .get_mut(index)
                        .ok_or_else(|| error("symbol section outside table"))?;
                    if raw.value() as usize > section.size {
                        return Err(error("symbol value outside section"));
                    }
                    if raw.has_aux_section()
                        && (raw.number_of_aux_symbols() != 1 || raw.value() != 0)
                    {
                        return Err(error("unsupported section auxiliary record"));
                    }
                    Definition::Section(index)
                }
                pe::IMAGE_SYM_ABSOLUTE => Definition::Absolute,
                pe::IMAGE_SYM_DEBUG => Definition::Ignored,
                pe::IMAGE_SYM_UNDEFINED if raw.has_aux_weak_external() => {
                    let aux = parsed(table.aux_weak_external(index))?;
                    if raw.number_of_aux_symbols() != 1
                        || aux.weak_search_type.get(LE) != pe::IMAGE_WEAK_EXTERN_SEARCH_ALIAS
                    {
                        return Err(error("only weak alias symbols are supported"));
                    }
                    Definition::Alias(aux.weak_default_sym_index.get(LE) as usize)
                }
                pe::IMAGE_SYM_UNDEFINED
                    if raw.value() == 0 && raw.storage_class() == pe::IMAGE_SYM_CLASS_EXTERNAL =>
                {
                    Definition::External
                }
                _ => return Err(error("unsupported symbol class/common storage")),
            };
            if raw.has_aux_section() && section_number > 0 {
                let aux = parsed(table.aux_section(index))?;
                let section = &mut sections[section_number as usize - 1];
                if section.selection.replace(aux.selection).is_some() {
                    return Err(error("duplicate section auxiliary record"));
                }
                if section.comdat {
                    match aux.selection {
                        pe::IMAGE_COMDAT_SELECT_NODUPLICATES | pe::IMAGE_COMDAT_SELECT_ANY => {}
                        pe::IMAGE_COMDAT_SELECT_ASSOCIATIVE => {
                            let parent = aux.number.get(LE) as usize
                                | ((aux.high_number.get(LE) as usize) << 16);
                            if parent == 0 || parent > header.number_of_sections() as usize {
                                return Err(error("COMDAT association outside table"));
                            }
                            section.associate = Some(parent - 1);
                        }
                        _ => return Err(error("unsupported COMDAT selection")),
                    }
                } else if aux.selection != 0 {
                    return Err(error("COMDAT selection on ordinary section"));
                }
            }
            if raw.storage_class() == pe::IMAGE_SYM_CLASS_EXTERNAL
                && matches!(definition, Definition::Section(_) | Definition::Absolute)
                && exports.insert(name, index.0).is_some()
            {
                // One object only: duplicate definitions requiring COMDAT winner
                // selection/folding are deliberately outside this first profile.
                return Err(error("duplicate external definition in logic object"));
            }
            symbols[index.0] = Some(Symbol {
                name,
                value: raw.value(),
                definition,
            });
        }
        for (i, section) in sections.iter().enumerate() {
            if section.comdat && section.selection.is_none() {
                return Err(error("COMDAT without section definition"));
            }
            let mut at = section.associate;
            let mut visited = 0;
            while let Some(parent) = at {
                visited += 1;
                if parent == i || visited > sections.len() || !sections[parent].comdat {
                    return Err(error("invalid/cyclic COMDAT association"));
                }
                at = sections[parent].associate;
            }
            for reloc in section.relocations {
                let kind = reloc.typ.get(LE);
                let width = relocation_width(kind)?;
                let offset = reloc.virtual_address.get(LE) as usize;
                if offset
                    .checked_add(width)
                    .is_none_or(|end| end > section.size)
                {
                    return Err(error("relocation outside section"));
                }
                if kind != pe::IMAGE_REL_AMD64_ABSOLUTE
                    && symbols
                        .get(reloc.symbol_table_index.get(LE) as usize)
                        .and_then(Option::as_ref)
                        .is_none()
                {
                    return Err(error("relocation references auxiliary/missing symbol"));
                }
            }
        }
        for symbol in symbols.iter().flatten() {
            if matches!(symbol.definition, Definition::Alias(_))
                && exports.contains_key(symbol.name)
            {
                return Err(error("weak alias collides with a strong definition"));
            }
            let mut definition = symbol.definition;
            let mut depth = 0;
            while let Definition::Alias(target) = definition {
                depth += 1;
                if depth > 64 {
                    return Err(error("cyclic or overlong weak alias chain"));
                }
                definition = symbols
                    .get(target)
                    .and_then(Option::as_ref)
                    .ok_or_else(|| error("weak alias references missing/auxiliary symbol"))?
                    .definition;
            }
        }
        Ok(Self {
            sections,
            symbols,
            exports,
        })
    }
}

fn reject_tls(name: &[u8]) -> Result<(), JitError> {
    let name = name.strip_prefix(b"__imp_").unwrap_or(name);
    if name.starts_with(b"__emutls_") {
        return Err(error("emulated TLS is forbidden in layered logic"));
    }
    if name == b"_tls_index"
        || name == b"_tls_used"
        || [b"__dyn_tls".as_slice(), b"__tls_guard", b"__tls_get_addr"]
            .iter()
            .any(|p| name.starts_with(p))
    {
        return Err(error("native TLS symbol in logic"));
    }
    Ok(())
}
pub(super) fn relocation_width(kind: u16) -> Result<usize, JitError> {
    match kind {
        pe::IMAGE_REL_AMD64_ABSOLUTE => Ok(0),
        pe::IMAGE_REL_AMD64_ADDR64 => Ok(8),
        pe::IMAGE_REL_AMD64_SECTION => Ok(2),
        pe::IMAGE_REL_AMD64_ADDR32
        | pe::IMAGE_REL_AMD64_ADDR32NB
        | pe::IMAGE_REL_AMD64_REL32..=pe::IMAGE_REL_AMD64_REL32_5
        | pe::IMAGE_REL_AMD64_SECREL => Ok(4),
        _ => Err(error(format!("unsupported AMD64 relocation {kind:#x}"))),
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    pub struct Fixture {
        pub bytes: Vec<u8>,
        pub header: usize,
        pub symbols: usize,
        pub relocs: usize,
    }
    pub fn put16(bytes: &mut [u8], at: usize, value: u16) {
        bytes[at..at + 2].copy_from_slice(&value.to_le_bytes());
    }
    pub fn put32(bytes: &mut [u8], at: usize, value: u32) {
        bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
    }
    pub fn minimal(big: bool) -> Fixture {
        let header = if big { 56 } else { 20 };
        let record = if big { 20 } else { 18 };
        let data = header + 120;
        let relocs = data + 17;
        let symbols = relocs + 30;
        let strings = symbols + 3 * record;
        let mut bytes = vec![0; strings + 20];
        if big {
            put16(&mut bytes, 2, 0xffff);
            put16(&mut bytes, 4, 2);
            put16(&mut bytes, 6, pe::IMAGE_FILE_MACHINE_AMD64);
            bytes[12..28].copy_from_slice(&pe::ANON_OBJECT_HEADER_BIGOBJ_CLASS_ID.0);
            put32(&mut bytes, 44, 3);
            put32(&mut bytes, 48, symbols as u32);
            put32(&mut bytes, 52, 3);
        } else {
            put16(&mut bytes, 0, pe::IMAGE_FILE_MACHINE_AMD64);
            put16(&mut bytes, 2, 3);
            put32(&mut bytes, 8, symbols as u32);
            put32(&mut bytes, 12, 3);
        }
        for (i, (name, size, offset, flags)) in [
            (b".text\0\0\0", 1, data, 0x6050_0020),
            (b".xdata\0\0", 4, data + 1, 0x4030_0040),
            (b".pdata\0\0", 12, data + 5, 0x4030_0040),
        ]
        .into_iter()
        .enumerate()
        {
            let at = header + i * 40;
            bytes[at..at + 8].copy_from_slice(name);
            put32(&mut bytes, at + 16, size);
            put32(&mut bytes, at + 20, offset as u32);
            put32(&mut bytes, at + 36, flags);
        }
        put32(&mut bytes, header + 80 + 24, relocs as u32);
        put16(&mut bytes, header + 80 + 32, 3);
        bytes[data] = 0xc3; // ret; never called by these loader tests
        bytes[data + 1] = 1; // version 1, no unwind codes
        put32(&mut bytes, data + 9, 1); // EndAddress addend
        for (i, symbol) in [1, 1, 2].into_iter().enumerate() {
            put32(&mut bytes, relocs + i * 10, i as u32 * 4);
            put32(&mut bytes, relocs + i * 10 + 4, symbol);
            put16(
                &mut bytes,
                relocs + i * 10 + 8,
                pe::IMAGE_REL_AMD64_ADDR32NB,
            );
        }
        for i in 0..3 {
            let at = symbols + i * record;
            if i == 0 {
                put32(&mut bytes, at + 4, 4);
            } else {
                bytes[at..at + 8].copy_from_slice(if i == 1 {
                    b".text\0\0\0"
                } else {
                    b".xdata\0\0"
                });
            }
            if big {
                put32(&mut bytes, at + 12, if i == 2 { 2 } else { 1 });
            } else {
                put16(&mut bytes, at + 12, if i == 2 { 2 } else { 1 });
            }
            bytes[at + record - 2] = if i == 0 {
                pe::IMAGE_SYM_CLASS_EXTERNAL
            } else {
                pe::IMAGE_SYM_CLASS_STATIC
            };
        }
        put32(&mut bytes, strings, 20);
        bytes[strings + 4..strings + 20].copy_from_slice(b"hot_handler_api\0");
        Fixture {
            bytes,
            header,
            symbols,
            relocs,
        }
    }
    fn failure(bytes: &[u8], needle: &str) {
        let result = Object::parse(bytes, 65536);
        assert!(
            matches!(result, Err(JitError::Link(ref e)) if e.contains(needle)),
            "expected {needle}"
        );
    }
    #[test]
    fn regular_bigobj_and_all_truncations() {
        for big in [false, true] {
            let f = minimal(big);
            assert_eq!(Object::parse(&f.bytes, 65536).unwrap().sections.len(), 3);
            for end in 0..f.bytes.len() {
                assert!(
                    Object::parse(&f.bytes[..end], 65536).is_err(),
                    "accepted truncated object at {end}"
                );
            }
        }
    }
    #[test]
    fn quotas_bss_and_table_bounds() {
        let mut f = minimal(false);
        put32(&mut f.bytes, f.header + 40 + 36, 0xc030_0080);
        put32(&mut f.bytes, f.header + 40 + 16, u32::MAX);
        failure(&f.bytes, "budget");
        let mut f = minimal(false);
        put32(&mut f.bytes, 12, 1_000_001);
        assert!(Object::parse(&f.bytes, 65536).is_err());
        let mut f = minimal(false);
        f.bytes[f.symbols + 2 * 18 + 17] = 10;
        failure(&f.bytes, "auxiliary");
        let mut f = minimal(false);
        put32(&mut f.bytes, f.symbols + 3 * 18, u32::MAX);
        failure(&f.bytes, "string table");
    }
    #[test]
    fn relocations_must_fit_and_target_primary_symbols() {
        let mut f = minimal(false);
        put32(&mut f.bytes, f.relocs, 10);
        failure(&f.bytes, "outside section");
        let mut f = minimal(false);
        put32(&mut f.bytes, f.relocs + 4, 999);
        failure(&f.bytes, "missing symbol");
        let mut f = minimal(false);
        put16(&mut f.bytes, f.relocs + 8, 0xffff);
        failure(&f.bytes, "unsupported AMD64 relocation");
        let mut f = minimal(false);
        f.bytes[f.symbols + 17] = 1;
        failure(&f.bytes, "auxiliary/missing");
    }
    #[test]
    fn tls_startup_writable_code_and_metadata_are_rejected() {
        for name in [b".tls\0\0\0\0", b".CRT$XCU"] {
            let mut f = minimal(false);
            f.bytes[f.header..f.header + 8].copy_from_slice(name);
            assert!(Object::parse(&f.bytes, 65536).is_err());
        }
        let mut f = minimal(false);
        put32(&mut f.bytes, f.header + 36, 0xe050_0020);
        failure(&f.bytes, "writable executable");
        let mut f = minimal(true);
        put32(&mut f.bytes, 32, 1);
        failure(&f.bytes, "metadata");
    }
    #[test]
    fn weak_aliases_and_comdat_cycles_are_checked() {
        let mut f = minimal(false);
        let at = f.symbols + 18;
        put16(&mut f.bytes, at + 12, 0);
        f.bytes[at + 16] = pe::IMAGE_SYM_CLASS_WEAK_EXTERNAL;
        f.bytes[at + 17] = 1;
        f.bytes[at + 18..at + 36].fill(0);
        put32(&mut f.bytes, at + 18, 0); // alias to entry
        put32(&mut f.bytes, at + 22, pe::IMAGE_WEAK_EXTERN_SEARCH_ALIAS);
        put32(&mut f.bytes, f.relocs + 24, 1);
        assert!(Object::parse(&f.bytes, 65536).is_ok());
        let alias_name = f.bytes[at..at + 8].to_vec();
        f.bytes[at..at + 8].fill(0);
        put32(&mut f.bytes, at + 4, 4);
        failure(&f.bytes, "collides with a strong definition");
        f.bytes[at..at + 8].copy_from_slice(&alias_name);
        put32(&mut f.bytes, at + 18, 1);
        failure(&f.bytes, "cyclic");
        put32(&mut f.bytes, at + 22, 1);
        failure(&f.bytes, "only weak alias");
        let mut f = minimal(false);
        let at = f.symbols + 18;
        f.bytes[at + 17] = 1;
        f.bytes[at + 18..at + 36].fill(0);
        f.bytes[at + 18 + 14] = pe::IMAGE_COMDAT_SELECT_ASSOCIATIVE;
        put16(&mut f.bytes, at + 18 + 12, 1);
        put32(&mut f.bytes, f.header + 36, 0x6050_1020);
        put32(&mut f.bytes, f.relocs + 24, 0);
        failure(&f.bytes, "cyclic COMDAT");
    }
}
