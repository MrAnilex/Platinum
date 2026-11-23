//! Tests of the NSIS reader (nsis.rs): synthetic headers and installers, and GG 123's own
//! installer when the workspace has it.

use super::*;

/// A UTF-16 string table: the empty string, then each string, NUL-terminated. Codes are given as
/// units 1 to 4 followed by their parameter unit. Returns the table and each string's offset.
fn table(strings: &[&[u16]]) -> (Vec<u8>, Vec<i32>) {
    let mut units: Vec<u16> = vec![0];
    let mut offsets = Vec::new();
    for s in strings {
        offsets.push(units.len() as i32);
        units.extend_from_slice(s);
        units.push(0);
    }
    (units.iter().flat_map(|u| u.to_le_bytes()).collect(), offsets)
}

fn text(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

/// `CODE_SHORT`, as NSIS stores variable numbers.
fn code_short(n: u16) -> u16 {
    (n & 0x7F) | ((n & 0x3F80) << 1) | 0x8080
}

fn instdir_then(rest: &str) -> Vec<u16> {
    let mut units = vec![3, code_short(21)];
    units.extend(text(rest));
    units
}

/// A header with the given entries (opcode and parameters) and string table.
fn header(entries: &[(i32, [i32; 6])], strings: &[u8]) -> Vec<u8> {
    let mut h = Vec::new();
    let entries_at = 4 + 8 * BLOCKS;
    let strings_at = entries_at + ENTRY_SIZE * entries.len();
    h.extend_from_slice(&0u32.to_le_bytes());
    for block in 0..BLOCKS {
        let (offset, count) = match block {
            ENTRIES => (entries_at, entries.len()),
            STRINGS => (strings_at, strings.len()),
            _ => (strings_at + strings.len(), 0),
        };
        h.extend_from_slice(&(offset as u32).to_le_bytes());
        h.extend_from_slice(&(count as u32).to_le_bytes());
    }
    for (opcode, parameters) in entries {
        h.extend_from_slice(&opcode.to_le_bytes());
        for p in parameters {
            h.extend_from_slice(&p.to_le_bytes());
        }
    }
    h.extend_from_slice(strings);
    h
}

#[test]
fn strings_decode_variables_and_codes() {
    let mut skip = text("a");
    skip.extend([4, u16::from(b'$')]);
    skip.extend(text("b"));
    let (strings, at) = table(&[&instdir_then(r"\apps\sonar"), &skip, &[3, code_short(32)], &[1, code_short(5)]]);
    assert_eq!(string(&strings, at[0]).unwrap(), r"$INSTDIR\apps\sonar");
    assert_eq!(string(&strings, at[1]).unwrap(), "a$b");
    assert_eq!(string(&strings, at[2]).unwrap(), "$_0_");
    assert_eq!(string(&strings, at[3]).unwrap(), "$(LSTR_5)");
    assert_eq!(string(&strings, -3).unwrap(), "$(LSTR_2)");
    assert!(string(&strings, 10_000).is_err());
    assert_eq!(decode_short(code_short(155)), 155);
    assert_eq!((variable(3), variable(13), variable(21), variable(31), variable(155)), ("$3".into(), "$R3".into(), "$INSTDIR".into(), "$_OUTDIR".into(), "$_123_".into()));
}

#[test]
fn paths_follow_the_last_output_folder() {
    assert_eq!(install_path(r"$INSTDIR\apps\sonar\driver\vad", "a.inf"), r"apps\sonar\driver\vad\a.inf");
    assert_eq!(install_path(r"$INSTDIR\x\", r"$INSTDIR\shared\tool.exe"), r"shared\tool.exe");
    assert_eq!(install_path(r"$_123_", "Sonar.AgsSetup.exe"), r"$_123_\Sonar.AgsSetup.exe");
    assert_eq!(install_path("", r"C:\abs\f.txt"), r"C:\abs\f.txt");
    assert_eq!(install_path(r"$INSTDIR", r".\a\\b.txt"), r"a\b.txt");
    assert_eq!(install_path(r"$INSTDIRX", "f"), r"$INSTDIRX\f");
}

#[test]
fn the_header_lists_files_in_script_order() {
    let (strings, at) = table(&[&instdir_then(r"\apps\x"), &text("a.txt"), &instdir_then(r"\b.txt"), &text("c.txt")]);
    let h = header(
        &[
            (EW_EXTRACTFILE, [0, at[3], 40, 0, 0, 0]),
            (EW_CREATEDIR, [at[0], 1, 0, 0, 0, 0]),
            (EW_CREATEDIR, [at[2], 0, 0, 0, 0, 0]),
            (EW_EXTRACTFILE, [0, at[1], 0, 0, 0, 0]),
            (2, [0; 6]),
            (EW_EXTRACTFILE, [0, at[2], 9, 0, 0, 0]),
        ],
        &strings,
    );
    let listed = items(&h).unwrap();
    let paths: Vec<(&str, u32)> = listed.iter().map(|i| (i.path.as_str(), i.offset)).collect();
    // A CreateDirectory that is not a SetOutPath changes nothing.
    assert_eq!(paths, [("c.txt", 40), (r"apps\x\a.txt", 0), ("b.txt", 9)]);
    assert!(items(&h[..20]).is_err());
    let mut ansi = h.clone();
    let strings_at = 4 + 8 * BLOCKS + ENTRY_SIZE * 6;
    ansi[strings_at + 1] = b'x';
    assert!(items(&ansi).unwrap_err().contains("Unicode"));
}

#[test]
fn a_path_extracted_twice_keeps_its_last_block() {
    let item = |path: &str, offset| Item { path: path.into(), offset };
    let blocks = blocks_to_keep(vec![item("a", 0), item("b", 9), item("a", 20), item("c", 9), item("skip", 30)], &|p| p != "skip");
    assert_eq!(blocks.into_iter().collect::<Vec<_>>(), [(9, vec!["b".to_string(), "c".to_string()]), (20, vec!["a".to_string()])]);
}

#[test]
fn methods_other_than_solid_lzma_are_refused() {
    let lzma = [0x5D, 0, 0, 0, 1, 0, 0x3F, 0x91];
    assert_eq!(lzma_start(&lzma, 1000), Ok(0));
    let mut flagged = vec![0];
    flagged.extend_from_slice(&lzma);
    assert_eq!(lzma_start(&flagged, 1000), Ok(1));
    flagged[0] = 1;
    assert!(lzma_start(&flagged, 1000).unwrap_err().contains("x86"));
    assert!(lzma_start(&1000u32.to_le_bytes(), 1000).unwrap_err().contains("not compressed"));
    assert!(lzma_start(&[0x10, 0x20, 0, 0x80, 0x5D, 0, 0, 0, 1, 0], 1000).unwrap_err().contains("non-solid"));
    assert!(lzma_start(&[0x78, 0xDA, 1, 2, 3, 4, 5], 1000).is_err());
}

/// A whole synthetic installer: a stub, the first header, then the solid LZMA stream of the
/// header and two files, compressed by lzma-rs's own encoder.
#[test]
fn a_synthetic_installer_extracts() {
    let (strings, at) = table(&[&instdir_then(r"\apps\sonar\driver\vad"), &text("a.inf"), &instdir_then(r"\other.txt")]);
    let h = header(&[(EW_CREATEDIR, [at[0], 1, 0, 0, 0, 0]), (EW_EXTRACTFILE, [0, at[1], 0, 0, 0, 0]), (EW_EXTRACTFILE, [0, at[2], 9, 0, 0, 0])], &strings);
    let mut stream = Vec::new();
    stream.extend_from_slice(&(h.len() as u32).to_le_bytes());
    stream.extend_from_slice(&h);
    for data in [&b"hello"[..], &b"abc"[..]] {
        stream.extend_from_slice(&(data.len() as u32).to_le_bytes());
        stream.extend_from_slice(data);
    }
    let mut compressed = Vec::new();
    let options = lzma_rs::compress::Options { unpacked_size: lzma_rs::compress::UnpackedSize::SkipWritingToHeader };
    lzma_rs::lzma_compress_with_options(&mut std::io::Cursor::new(&stream), &mut compressed, &options).unwrap();
    let mut file = b"MZ".to_vec();
    file.resize(1024, 0);
    file.extend_from_slice(&NO_CRC.to_le_bytes());
    file.extend_from_slice(&SIGNATURE.to_le_bytes());
    file.extend_from_slice(MAGIC);
    file.extend_from_slice(&(h.len() as u32).to_le_bytes());
    file.extend_from_slice(&((FIRST_HEADER + compressed.len()) as u32).to_le_bytes());
    file.extend_from_slice(&compressed);
    let base = std::env::temp_dir().join(format!("platinum-nsis-synthetic-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    let installer = base.join("setup.exe");
    std::fs::write(&installer, &file).unwrap();
    assert_eq!(find_first_header(&mut std::fs::File::open(&installer).unwrap()).unwrap().map(|(at, _)| at), Some(1024));
    assert_eq!(list(&installer).unwrap().len(), 2);
    let mut reads = 0;
    let out = base.join("out");
    let written = extract(&installer, &|p| p.starts_with("apps"), &out, &mut |_, _| reads += 1).unwrap();
    assert_eq!(written, [Extracted { path: r"apps\sonar\driver\vad\a.inf".into(), size: 5 }]);
    assert_eq!(std::fs::read(out.join("apps/sonar/driver/vad/a.inf")).unwrap(), b"hello");
    assert!(!out.join("other.txt").exists() && reads > 0);
    let both = extract(&installer, &|_| true, &base.join("all"), &mut |_, _| {}).unwrap();
    assert_eq!(both.len(), 2);
    assert_eq!(std::fs::read(base.join("all/other.txt")).unwrap(), b"abc");
    assert!(extract(&installer, &|_| false, &base.join("none"), &mut |_, _| {}).unwrap_err().contains("no file"));
    // Truncated: the first header promises more than the file has.
    std::fs::write(&installer, &file[..file.len() - 10]).unwrap();
    assert!(extract(&installer, &|_| true, &base.join("cut"), &mut |_, _| {}).unwrap_err().contains("truncated"));
    std::fs::write(&installer, b"MZ not an installer").unwrap();
    assert!(extract(&installer, &|_| true, &base.join("cut"), &mut |_, _| {}).unwrap_err().contains("not an NSIS installer"));
    let _ = std::fs::remove_dir_all(&base);
}

/// GG 123's own installer, when the workspace has it: the files the setup takes come out
/// byte-identical to 7-Zip's extraction (`extracted/`), when that is there too. About ten seconds
/// (the solid stream is unpacked up to the device tool, 850 MB in).
#[test]
fn ggs_installer_extracts_as_7zip_does() {
    let Some(installer) = crate::tests::local_gg_installer() else {
        eprintln!("GG's installer is not in the workspace: skipped");
        return;
    };
    let base = std::env::var_os("PLATINUM_NSIS_TEST_OUT").map(std::path::PathBuf::from).unwrap_or_else(std::env::temp_dir);
    let out = base.join(format!("platinum-nsis-gg-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&out);
    let started = std::time::Instant::now();
    let written = extract(&installer, &crate::plan::wanted, &out, &mut |_, _| {}).unwrap();
    eprintln!("{} files in {:.1} s", written.len(), started.elapsed().as_secs_f64());
    assert!(crate::plan::missing_driver_files(&out).is_empty());
    assert_eq!(written.iter().filter(|f| f.path.contains("db-migrations")).count(), 106);
    assert_eq!(written.len(), 121);
    let reference = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../extracted");
    if reference.is_dir() {
        for file in &written {
            let relative = file.path.replace('\\', "/");
            let ours = std::fs::read(out.join(&relative)).unwrap();
            let theirs = std::fs::read(reference.join(&relative)).unwrap_or_else(|e| panic!("{relative}: {e}"));
            assert!(ours == theirs, "{relative} differs from 7-Zip's");
        }
        eprintln!("all {} files identical to 7-Zip's extraction", written.len());
    }
    // The migrations taken from it rebuild GG's preset library without a fault.
    let library = platinum::library::Library::from_migrations(&out.join(platinum::sonarfiles::MIGRATIONS)).unwrap();
    assert!(library.skipped.is_empty(), "{:?}", library.skipped);
    assert!(library.configs.len() > 400, "{} presets", library.configs.len());
    let _ = std::fs::remove_dir_all(&out);
}


/// A header with pages (window procedure and second parameter each), a string table, and
/// language tables (language id and string offsets each).
fn header_with_pages(pages: &[(i32, i32)], strings: &[u8], tables: &[(u16, Vec<i32>)]) -> Vec<u8> {
    let table_size = tables.first().map_or(0, |(_, offsets)| 10 + 4 * offsets.len());
    let pages_at = 4 + 8 * BLOCKS + 4 * 9;
    let strings_at = pages_at + PAGE_SIZE * pages.len();
    let tables_at = strings_at + strings.len();
    let mut h = vec![0u8; pages_at];
    let mut put = |at: usize, value: u32| h[at..at + 4].copy_from_slice(&value.to_le_bytes());
    put(4 + 8 * PAGES, pages_at as u32);
    put(8 + 8 * PAGES, pages.len() as u32);
    put(4 + 8 * STRINGS, strings_at as u32);
    put(4 + 8 * LANGUAGE_TABLES, tables_at as u32);
    put(8 + 8 * LANGUAGE_TABLES, tables.len() as u32);
    put(LANGUAGE_TABLE_SIZE_AT, table_size as u32);
    for (procedure, parameter) in pages {
        let mut page = [0i32; 16];
        page[1] = *procedure;
        page[12] = *parameter;
        h.extend(page.iter().flat_map(|v| v.to_le_bytes()));
    }
    h.extend_from_slice(strings);
    for (language, offsets) in tables {
        h.extend_from_slice(&language.to_le_bytes());
        h.extend_from_slice(&[0; 8]);
        h.extend(offsets.iter().flat_map(|v| v.to_le_bytes()));
    }
    h
}

#[test]
fn the_licence_page_text_is_read() {
    let mut plain = vec![0x11];
    plain.extend(text("LICENSE\r\nTerms."));
    let mut rtf = vec![0x12];
    // The euro sign as RTF writes it: a backslash, "u", then 8364 (written in two pieces here).
    let euro = format!("\\{}{}?", 'u', 8364);
    rtf.extend(text(&format!(r"{{\rtf1\ansi{{\fonttbl{{\f0 Arial;}}}}{{\*\generator x;}}\f0 First\par Caf\'e9 {euro} done\}}}}")));
    let (strings, at) = table(&[&plain, &rtf, &text("other")]);
    // The licence is the first page with window procedure 0, after a custom page (6).
    assert_eq!(license_text(&header_with_pages(&[(6, at[2]), (PAGE_LICENSE, at[0]), (3, 0)], &strings, &[])).unwrap(), "LICENSE\r\nTerms.");
    assert_eq!(license_text(&header_with_pages(&[(PAGE_LICENSE, at[1])], &strings, &[])).unwrap(), "First\r\nCafé € done}");
    // A licence that differs by language: language string 1, read in the English table.
    let tables = [(1036u16, vec![at[2], at[2]]), (ENGLISH, vec![at[2], at[0]])];
    assert_eq!(license_text(&header_with_pages(&[(PAGE_LICENSE, -2)], &strings, &tables)).unwrap(), "LICENSE\r\nTerms.");
    assert!(license_text(&header_with_pages(&[(3, at[0])], &strings, &[])).unwrap_err().contains("no licence page"));
    assert!(license_text(&header_with_pages(&[(PAGE_LICENSE, 0)], &strings, &[])).unwrap_err().contains("no text"));
    assert!(license_text(&header_with_pages(&[(PAGE_LICENSE, -2)], &strings, &[])).is_err());
}

/// GG 123's licence, read from its installer, is the text 7-Zip shows as `[LICENSE].txt`
/// (UTF-16LE), when the workspace has both.
#[test]
fn ggs_licence_is_read_from_its_installer() {
    let Some(installer) = crate::tests::local_gg_installer() else {
        eprintln!("GG's installer is not in the workspace: skipped");
        return;
    };
    let text = license(&installer).unwrap();
    assert!(text.starts_with("END USER LICENSE AGREEMENT\r\n"), "{}", &text[..60.min(text.len())]);
    assert!(text.len() > 8000);
    let reference = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../extracted/[LICENSE].txt");
    if let Ok(bytes) = std::fs::read(reference) {
        let units: Vec<u16> = bytes.chunks_exact(2).map(|b| u16::from_le_bytes([b[0], b[1]])).collect();
        assert_eq!(text, String::from_utf16_lossy(&units));
        eprintln!("licence identical to 7-Zip's [LICENSE].txt ({} characters)", text.encode_utf16().count());
    }
}
