//! Windows resources without a resource compiler (no `windres`, no `rc`, no crates).
//!
//! Turns an `.ico` file into a COFF object whose `.rsrc` section holds the icon (one `RT_ICON`
//! per image of the file, plus the `RT_GROUP_ICON` that lists them), writes it to `OUT_DIR`, and
//! asks Cargo to hand it to the linker. Explorer, the taskbar, Alt+Tab, Task Manager and shortcuts
//! without an icon of their own then show this icon for the executable.
//!
//! The object has the layout GNU `windres -O coff` produces: a single `.rsrc` section (directory
//! tree, data entries, then the images), whose data entries hold section offsets fixed up by
//! `ADDR32NB` relocations against the section symbol. rust-lld (GNU or MSVC flavour), GNU ld and
//! MSVC `link.exe` all copy such a section into the image's resource directory.
//!
//! Used from a build script: `#[path = "…/winres.rs"] mod winres;` then
//! `winres::embed_icon("app.ico", None)`.

#![allow(dead_code)]

use std::{env, fs, path::PathBuf};

pub const RT_ICON: u16 = 3;
pub const RT_GROUP_ICON: u16 = 14;
/// English (United States), the language GNU windres gives resources by default. Windows finds a
/// resource whatever its language when the program has only one, so the choice does not matter.
pub const LANG_EN_US: u16 = 0x0409;

/// One resource: type, numeric name, language and raw bytes.
pub struct Resource {
    pub kind: u16,
    pub id: u16,
    pub lang: u16,
    pub data: Vec<u8>,
}

/// Embeds the icon file `ico` (absolute, or relative to the package's directory) as icon group 1.
/// `bin` names the binary that gets it (`None`: every binary of the package). Does nothing when
/// the target is not Windows. Panics, failing the build, if the file is missing or malformed.
pub fn embed_icon(ico: &str, bin: Option<&str>) {
    let path = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR")).join(ico);
    println!("cargo:rerun-if-changed={}", path.display());
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let bytes = fs::read(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    let resources = icon_resources(&bytes, 1, 1).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let arch = env::var("CARGO_CFG_TARGET_ARCH").expect("CARGO_CFG_TARGET_ARCH");
    let object = coff_object(&resources, &arch).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("icon");
    let out = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR")).join(format!("{stem}.rsrc.o"));
    fs::write(&out, object).unwrap_or_else(|e| panic!("cannot write {}: {e}", out.display()));
    // An object named on the command line is always linked in (unlike a member of a library).
    // One argument per line, so a path with spaces stays one argument.
    match bin {
        Some(bin) => println!("cargo:rustc-link-arg-bin={bin}={}", out.display()),
        None => println!("cargo:rustc-link-arg-bins={}", out.display()),
    }
}

/// The resources of an icon file: its images as `RT_ICON` `first_icon_id`, `first_icon_id + 1`…
/// in file order, and the `RT_GROUP_ICON` `group_id` that lists them. PNG and BMP images are both
/// stored as they are in the file, which is what `RT_ICON` expects.
pub fn icon_resources(ico: &[u8], group_id: u16, first_icon_id: u16) -> Result<Vec<Resource>, String> {
    let u16_at = |at: usize| u16::from_le_bytes([ico[at], ico[at + 1]]);
    let u32_at = |at: usize| u32::from_le_bytes([ico[at], ico[at + 1], ico[at + 2], ico[at + 3]]);
    if ico.len() < 6 || u16_at(0) != 0 || u16_at(2) != 1 {
        return Err("not an icon file (ICONDIR header)".into());
    }
    let count = u16_at(4) as usize;
    if count == 0 || ico.len() < 6 + 16 * count {
        return Err("icon file without images, or truncated directory".into());
    }
    if first_icon_id as usize + count > 0x10000 {
        return Err("too many images".into());
    }
    // GRPICONDIR, then one 14-byte GRPICONDIRENTRY per image: the ICONDIRENTRY with the file
    // offset replaced by the RT_ICON id.
    let mut group = Vec::with_capacity(6 + 14 * count);
    group.extend_from_slice(&0u16.to_le_bytes());
    group.extend_from_slice(&1u16.to_le_bytes());
    group.extend_from_slice(&(count as u16).to_le_bytes());
    let mut resources = Vec::with_capacity(count + 1);
    for i in 0..count {
        let e = 6 + 16 * i;
        let size = u32_at(e + 8) as usize;
        let offset = u32_at(e + 12) as usize;
        let data = offset.checked_add(size).and_then(|end| ico.get(offset..end));
        let data = data.ok_or_else(|| format!("image {i} lies outside the file"))?;
        let png = data.starts_with(b"\x89PNG\r\n\x1a\n");
        // Some writers leave planes and bit count at 0 for PNG images; Windows uses the bit count
        // to pick the best image, so give those the 32 bits they have.
        let planes = match u16_at(e + 4) { 0 => 1, p => p };
        let bits = match u16_at(e + 6) { 0 if png => 32, b => b };
        let id = first_icon_id + i as u16;
        group.extend_from_slice(&ico[e..e + 3]); // width, height (0 = 256), colour count
        group.push(0);
        group.extend_from_slice(&planes.to_le_bytes());
        group.extend_from_slice(&bits.to_le_bytes());
        group.extend_from_slice(&(size as u32).to_le_bytes());
        group.extend_from_slice(&id.to_le_bytes());
        resources.push(Resource { kind: RT_ICON, id, lang: LANG_EN_US, data: data.to_vec() });
    }
    resources.push(Resource { kind: RT_GROUP_ICON, id: group_id, lang: LANG_EN_US, data: group });
    Ok(resources)
}

/// A COFF object holding `resources` in its `.rsrc` section, for the Rust target architecture
/// `arch` (`CARGO_CFG_TARGET_ARCH`).
pub fn coff_object(resources: &[Resource], arch: &str) -> Result<Vec<u8>, String> {
    // Machine, and the relocation type that stores a 32-bit RVA (address without image base).
    let (machine, addr32nb): (u16, u16) = match arch {
        "x86_64" => (0x8664, 0x0003), // IMAGE_FILE_MACHINE_AMD64, IMAGE_REL_AMD64_ADDR32NB
        "x86" => (0x014c, 0x0007),    // IMAGE_FILE_MACHINE_I386, IMAGE_REL_I386_DIR32NB
        "aarch64" => (0xaa64, 0x0002), // IMAGE_FILE_MACHINE_ARM64, IMAGE_REL_ARM64_ADDR32NB
        other => return Err(format!("no COFF machine type known for {other}")),
    };
    let (section, fixups) = rsrc_section(resources)?;
    if fixups.len() > 0xffff {
        return Err("too many resources for one section".into());
    }
    const FILE_HEADER: usize = 20;
    const SECTION_HEADER: usize = 40;
    let raw = FILE_HEADER + SECTION_HEADER;
    let relocations = raw + section.len();
    let symbols = relocations + 10 * fixups.len();

    let mut o = Vec::with_capacity(symbols + 2 * 18 + 4);
    // IMAGE_FILE_HEADER
    o.extend_from_slice(&machine.to_le_bytes());
    o.extend_from_slice(&1u16.to_le_bytes()); // NumberOfSections
    o.extend_from_slice(&0u32.to_le_bytes()); // TimeDateStamp: 0 keeps builds reproducible
    o.extend_from_slice(&(symbols as u32).to_le_bytes()); // PointerToSymbolTable
    o.extend_from_slice(&2u32.to_le_bytes()); // NumberOfSymbols (section symbol + its aux record)
    o.extend_from_slice(&0u16.to_le_bytes()); // SizeOfOptionalHeader
    o.extend_from_slice(&0u16.to_le_bytes()); // Characteristics
    // IMAGE_SECTION_HEADER
    o.extend_from_slice(b".rsrc\0\0\0");
    o.extend_from_slice(&0u32.to_le_bytes()); // VirtualSize
    o.extend_from_slice(&0u32.to_le_bytes()); // VirtualAddress
    o.extend_from_slice(&(section.len() as u32).to_le_bytes()); // SizeOfRawData
    o.extend_from_slice(&(raw as u32).to_le_bytes()); // PointerToRawData
    o.extend_from_slice(&(relocations as u32).to_le_bytes()); // PointerToRelocations
    o.extend_from_slice(&0u32.to_le_bytes()); // PointerToLinenumbers
    o.extend_from_slice(&(fixups.len() as u16).to_le_bytes()); // NumberOfRelocations
    o.extend_from_slice(&0u16.to_le_bytes()); // NumberOfLinenumbers
    // IMAGE_SCN_CNT_INITIALIZED_DATA | IMAGE_SCN_ALIGN_8BYTES | IMAGE_SCN_MEM_READ
    o.extend_from_slice(&0x4040_0040u32.to_le_bytes());
    o.extend_from_slice(&section);
    // IMAGE_RELOCATION: where, which symbol (0: the section symbol), type. The 32 bits at `where`
    // already hold the offset of the data in the section; the linker adds the section's RVA.
    for at in &fixups {
        o.extend_from_slice(&at.to_le_bytes());
        o.extend_from_slice(&0u32.to_le_bytes());
        o.extend_from_slice(&addr32nb.to_le_bytes());
    }
    // Symbol 0: `.rsrc`, static, section 1, followed by its section-definition aux record.
    o.extend_from_slice(b".rsrc\0\0\0");
    o.extend_from_slice(&0u32.to_le_bytes()); // Value
    o.extend_from_slice(&1i16.to_le_bytes()); // SectionNumber
    o.extend_from_slice(&0u16.to_le_bytes()); // Type
    o.push(3); // StorageClass: IMAGE_SYM_CLASS_STATIC
    o.push(1); // NumberOfAuxSymbols
    o.extend_from_slice(&(section.len() as u32).to_le_bytes()); // Length
    o.extend_from_slice(&(fixups.len() as u16).to_le_bytes()); // NumberOfRelocations
    o.extend_from_slice(&0u16.to_le_bytes()); // NumberOfLinenumbers
    o.extend_from_slice(&0u32.to_le_bytes()); // CheckSum
    o.extend_from_slice(&0u16.to_le_bytes()); // Number
    o.push(0); // Selection
    o.extend_from_slice(&[0; 3]);
    o.extend_from_slice(&4u32.to_le_bytes()); // empty string table: just its size
    Ok(o)
}

/// The bytes of a `.rsrc` section (type, name and language directories, the data entries, then
/// the data, each aligned on 8 bytes), and the section offsets of the data entries' RVA fields.
fn rsrc_section(resources: &[Resource]) -> Result<(Vec<u8>, Vec<u32>), String> {
    let mut sorted: Vec<&Resource> = resources.iter().collect();
    // Windows looks entries up by binary search: ids in increasing order at every level.
    sorted.sort_by_key(|r| (r.kind, r.id, r.lang));
    if sorted.windows(2).any(|w| (w[0].kind, w[0].id, w[0].lang) == (w[1].kind, w[1].id, w[1].lang)) {
        return Err("two resources with the same type, id and language".into());
    }
    // types -> names -> indices in `sorted` (one per language)
    let mut types: Vec<(u16, Vec<(u16, Vec<usize>)>)> = Vec::new();
    for (i, r) in sorted.iter().enumerate() {
        if types.last().map(|t| t.0) != Some(r.kind) {
            types.push((r.kind, Vec::new()));
        }
        let names = &mut types.last_mut().unwrap().1;
        if names.last().map(|n| n.0) != Some(r.id) {
            names.push((r.id, Vec::new()));
        }
        names.last_mut().unwrap().1.push(i);
    }
    let dir = |entries: usize| 16 + 8 * entries;
    let align8 = |n: usize| (n + 7) & !7;
    let names_at = dir(types.len());
    let langs_at = names_at + types.iter().map(|t| dir(t.1.len())).sum::<usize>();
    let entries_at = langs_at + types.iter().flat_map(|t| &t.1).map(|n| dir(n.1.len())).sum::<usize>();
    let data_at = align8(entries_at + 16 * sorted.len());
    let mut offsets = Vec::with_capacity(sorted.len());
    let mut end = data_at;
    for r in &sorted {
        offsets.push(end);
        end = align8(end + r.data.len());
    }
    if end > u32::MAX as usize >> 1 {
        return Err("resources too large".into());
    }

    let mut s = Vec::with_capacity(end);
    let put32 = |s: &mut Vec<u8>, v: usize| s.extend_from_slice(&(v as u32).to_le_bytes());
    // IMAGE_RESOURCE_DIRECTORY: characteristics, time stamp, version, then counts of named and
    // numbered entries (only numbered ones here).
    let put_dir = |s: &mut Vec<u8>, entries: usize| {
        s.extend_from_slice(&[0; 12]);
        s.extend_from_slice(&0u16.to_le_bytes());
        s.extend_from_slice(&(entries as u16).to_le_bytes());
    };
    // IMAGE_RESOURCE_DIRECTORY_ENTRY: id, then offset of a subdirectory (high bit set) or of a
    // data entry, from the start of the section.
    const SUBDIR: usize = 0x8000_0000;
    let mut next = names_at;
    put_dir(&mut s, types.len());
    for (kind, names) in &types {
        put32(&mut s, *kind as usize);
        put32(&mut s, SUBDIR | next);
        next += dir(names.len());
    }
    for (_, names) in &types {
        put_dir(&mut s, names.len());
        for (id, langs) in names {
            put32(&mut s, *id as usize);
            put32(&mut s, SUBDIR | next);
            next += dir(langs.len());
        }
    }
    for (_, names) in &types {
        for (_, langs) in names {
            put_dir(&mut s, langs.len());
            for &i in langs {
                put32(&mut s, sorted[i].lang as usize);
                put32(&mut s, entries_at + 16 * i);
            }
        }
    }
    debug_assert_eq!(s.len(), entries_at);
    // IMAGE_RESOURCE_DATA_ENTRY: RVA of the data (relocated), size, code page, reserved.
    let mut fixups = Vec::with_capacity(sorted.len());
    for (i, r) in sorted.iter().enumerate() {
        fixups.push(s.len() as u32);
        put32(&mut s, offsets[i]);
        put32(&mut s, r.data.len());
        put32(&mut s, 0);
        put32(&mut s, 0);
    }
    for (i, r) in sorted.iter().enumerate() {
        s.resize(offsets[i], 0);
        s.extend_from_slice(&r.data);
    }
    s.resize(end, 0);
    Ok((s, fixups))
}
