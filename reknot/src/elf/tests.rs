use super::format::{self, Object};
use super::*;
use object::elf;

fn put16(bytes: &mut [u8], at: usize, value: u16) {
    bytes[at..at + 2].copy_from_slice(&value.to_le_bytes());
}
fn put32(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}
fn put64(bytes: &mut [u8], at: usize, value: u64) {
    bytes[at..at + 8].copy_from_slice(&value.to_le_bytes());
}

struct Fixture {
    bytes: Vec<u8>,
    headers: usize,
    symbols: usize,
    rela: usize,
    unwind: usize,
}
fn fixture() -> Fixture {
    let mut bytes = vec![0; 64];
    bytes[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
    put16(&mut bytes, 16, elf::ET_REL);
    put16(&mut bytes, 18, elf::EM_X86_64);
    put32(&mut bytes, 20, elf::EV_CURRENT as u32);
    put16(&mut bytes, 52, 64);
    put16(&mut bytes, 58, 64);
    put16(&mut bytes, 60, 7);
    put16(&mut bytes, 62, 6);
    let text = bytes.len();
    bytes.extend_from_slice(&[0; 8]);
    let unwind = bytes.len();
    bytes.extend_from_slice(&[0; 40]);
    put32(&mut bytes, unwind, 12);
    bytes[unwind + 8..unwind + 13].copy_from_slice(&[1, 0, 1, 0x78, 16]);
    put32(&mut bytes, unwind + 16, 20);
    put32(&mut bytes, unwind + 20, 20);
    put64(&mut bytes, unwind + 32, 1);
    let rela = bytes.len();
    bytes.extend_from_slice(&[0; 24]);
    put64(&mut bytes, rela + 8, (1u64 << 32) | elf::R_X86_64_64 as u64);
    let symbols = bytes.len();
    bytes.extend_from_slice(&[0; 48]);
    put32(&mut bytes, symbols + 24, 1);
    bytes[symbols + 28] = (elf::STB_GLOBAL << 4) | elf::STT_FUNC;
    put16(&mut bytes, symbols + 30, 1);
    put64(&mut bytes, symbols + 40, 8);
    let strings = bytes.len();
    bytes.extend_from_slice(b"\0hot_handler_api\0");
    let names = bytes.len();
    bytes.extend_from_slice(b"\0.text\0.eh_frame\0.rela.text\0.symtab\0.strtab\0.shstrtab\0");
    let names_len = bytes.len() - names;
    while !bytes.len().is_multiple_of(8) {
        bytes.push(0);
    }
    let headers = bytes.len();
    bytes.resize(headers + 7 * 64, 0);
    put64(&mut bytes, 40, headers as u64);
    for (index, name, typ, flags, offset, len, alignment, link, info, entry_size) in [
        (
            1,
            1,
            elf::SHT_PROGBITS,
            elf::SHF_ALLOC | elf::SHF_EXECINSTR,
            text,
            8,
            8,
            0,
            0,
            0,
        ),
        (
            2,
            7,
            elf::SHT_PROGBITS,
            elf::SHF_ALLOC,
            unwind,
            40,
            8,
            0,
            0,
            0,
        ),
        (3, 17, elf::SHT_RELA, 0, rela, 24, 8, 4, 1, 24),
        (4, 27, elf::SHT_SYMTAB, 0, symbols, 48, 8, 5, 1, 24),
        (5, 35, elf::SHT_STRTAB, 0, strings, 17, 1, 0, 0, 0),
        (6, 43, elf::SHT_STRTAB, 0, names, names_len, 1, 0, 0, 0),
    ] {
        let at = headers + index * 64;
        put32(&mut bytes, at, name);
        put32(&mut bytes, at + 4, typ);
        put64(&mut bytes, at + 8, flags as u64);
        put64(&mut bytes, at + 24, offset as u64);
        put64(&mut bytes, at + 32, len as u64);
        put32(&mut bytes, at + 40, link);
        put32(&mut bytes, at + 44, info);
        put64(&mut bytes, at + 48, alignment);
        put64(&mut bytes, at + 56, entry_size);
    }
    Fixture {
        bytes,
        headers,
        symbols,
        rela,
        unwind,
    }
}
fn reject(bytes: &[u8], needle: &str) {
    let result = Object::parse(bytes, 1 << 20);
    assert!(result.is_err(), "accepted invalid ELF: {needle}");
    assert!(
        result.err().unwrap().to_string().contains(needle),
        "missing diagnostic {needle}"
    );
}

#[test]
fn elf_parser_accepts_relocatable_and_rejects_every_truncation() {
    let f = fixture();
    let object = Object::parse(&f.bytes, 1 << 20).unwrap();
    assert_eq!(object.unwind_entries, 1);
    assert_eq!(object.sections[1].relocations.len(), 1);
    for end in 0..f.bytes.len() {
        assert!(
            Object::parse(&f.bytes[..end], 1 << 20).is_err(),
            "accepted truncation {end}"
        );
    }
}
#[test]
fn elf_rejects_wrong_target_tls_startup_and_writable_code() {
    for (at, value, needle) in [
        (16, elf::ET_DYN, "relocatable"),
        (18, elf::EM_AARCH64, "relocatable"),
    ] {
        let mut f = fixture();
        put16(&mut f.bytes, at, value);
        reject(&f.bytes, needle);
    }
    for (flags, needle) in [
        (elf::SHF_ALLOC | elf::SHF_TLS, "TLS"),
        (
            elf::SHF_ALLOC | elf::SHF_WRITE | elf::SHF_EXECINSTR,
            "writable",
        ),
        (
            elf::SHF_ALLOC | elf::SHF_COMPRESSED,
            "unsupported allocated",
        ),
    ] {
        let mut f = fixture();
        put64(&mut f.bytes, f.headers + 64 + 8, flags as u64);
        reject(&f.bytes, needle);
    }
    let mut f = fixture();
    put32(&mut f.bytes, f.headers + 64 + 4, elf::SHT_INIT_ARRAY);
    reject(&f.bytes, "startup");
    let mut f = fixture();
    f.bytes[f.symbols + 28] = (elf::STB_GLOBAL << 4) | elf::STT_TLS;
    reject(&f.bytes, "TLS");
}
#[test]
fn elf_rejects_relocation_bounds_kinds_and_symbol_indices() {
    let mut f = fixture();
    put64(&mut f.bytes, f.rela, 1);
    reject(&f.bytes, "relocation outside");
    let mut f = fixture();
    put64(&mut f.bytes, f.rela + 8, (99u64 << 32) | 1);
    reject(&f.bytes, "symbol table");
    let mut f = fixture();
    put64(
        &mut f.bytes,
        f.rela + 8,
        (1u64 << 32) | elf::R_X86_64_TLSGD as u64,
    );
    reject(&f.bytes, "unsupported ELF relocation");
    let mut f = fixture();
    put16(&mut f.bytes, f.symbols + 30, elf::SHN_COMMON);
    reject(&f.bytes, "common");
}
#[test]
fn elf_bounds_bss_alignment_tables_and_unwind() {
    let mut f = fixture();
    put32(&mut f.bytes, f.headers + 64 + 4, elf::SHT_NOBITS);
    put64(
        &mut f.bytes,
        f.headers + 64 + 8,
        (elf::SHF_ALLOC | elf::SHF_WRITE) as u64,
    );
    put64(&mut f.bytes, f.headers + 64 + 32, 2 << 20);
    reject(&f.bytes, "BSS");
    let mut f = fixture();
    put64(&mut f.bytes, f.headers + 64 + 48, 3);
    reject(&f.bytes, "alignment");
    let mut f = fixture();
    put16(&mut f.bytes, 60, 8193);
    reject(&f.bytes, "quota");
    let mut f = fixture();
    put64(&mut f.bytes, f.headers + 4 * 64 + 56, 1);
    reject(&f.bytes, "symbol table");
    let mut f = fixture();
    put32(&mut f.bytes, f.unwind + 20, 19);
    reject(&f.bytes, "CIE reference");
    let mut f = fixture();
    put32(&mut f.bytes, f.unwind + 16, 100);
    reject(&f.bytes, "truncated .eh_frame");
}
#[test]
fn elf_rela_math_uses_explicit_signed_addend_and_checks_overflow() {
    let mut word = [0xff; 4];
    for kind in [
        elf::R_X86_64_PC32,
        elf::R_X86_64_PLT32,
        elf::R_X86_64_GOTPCREL,
        elf::R_X86_64_GOTPCRELX,
        elf::R_X86_64_REX_GOTPCRELX,
    ] {
        format::relocate(&mut word, kind, 0x1000, -4, 0x2000).unwrap();
        assert_eq!(i32::from_le_bytes(word), -4100);
    }
    assert!(format::relocate(&mut word, elf::R_X86_64_PC32, usize::MAX, 0, 0).is_err());
    assert!(format::relocate(&mut word, elf::R_X86_64_32, 0, -1, 0).is_err());
    format::relocate(&mut word, elf::R_X86_64_32S, 0, -1, 0).unwrap();
    assert_eq!(i32::from_le_bytes(word), -1);
    let mut wide = [0xff; 8];
    format::relocate(&mut wide, elf::R_X86_64_64, 0x1000, -4, 0).unwrap();
    assert_eq!(u64::from_le_bytes(wide), 0xffc);
    format::relocate(&mut wide, elf::R_X86_64_PC64, 0x1000, -4, 0x2000).unwrap();
    assert_eq!(i64::from_le_bytes(wide), -4100);
}
#[test]
fn elf_invalid_limits_precede_library_loading() {
    let limits = ElfLimits {
        max_image_bytes: 0,
        ..Default::default()
    };
    assert!(matches!(
        unsafe { ElfEngine::open_layered("absent", "absent", limits) },
        Err(JitError::InvalidLimits)
    ));
}

#[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))]
#[path = "linux_tests.rs"]
mod linux;
