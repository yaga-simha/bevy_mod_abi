use super::*;

fn build_synthetic_elf64(has_pt_tls: bool, dt_flags: u64, dt_flags_1: u64) -> Vec<u8> {
    let phoff = 64usize;
    let phnum = if has_pt_tls { 2usize } else { 1usize };
    let dyn_off = phoff + phnum * 56;
    let dyn_entries = 3usize;
    let mut buf = vec![0u8; dyn_off + dyn_entries * 16];

    buf[0..4].copy_from_slice(b"\x7fELF");
    buf[4] = 2; // ELFCLASS64
    buf[5] = 1; // ELFDATA2LSB
    buf[6] = 1; // EV_CURRENT
    buf[32..40].copy_from_slice(&(phoff as u64).to_le_bytes());
    buf[54..56].copy_from_slice(&56u16.to_le_bytes());
    buf[56..58].copy_from_slice(&(phnum as u16).to_le_bytes());

    // Phdr 0: PT_DYNAMIC
    let p0 = phoff;
    buf[p0..p0 + 4].copy_from_slice(&2u32.to_le_bytes());
    buf[p0 + 8..p0 + 16].copy_from_slice(&(dyn_off as u64).to_le_bytes());
    buf[p0 + 32..p0 + 40].copy_from_slice(&((dyn_entries * 16) as u64).to_le_bytes());

    if has_pt_tls {
        let p1 = phoff + 56;
        buf[p1..p1 + 4].copy_from_slice(&7u32.to_le_bytes());
    }

    // Dynamic entries: DT_FLAGS, DT_FLAGS_1, DT_NULL
    buf[dyn_off..dyn_off + 8].copy_from_slice(&30u64.to_le_bytes());
    buf[dyn_off + 8..dyn_off + 16].copy_from_slice(&dt_flags.to_le_bytes());
    buf[dyn_off + 16..dyn_off + 24].copy_from_slice(&0x6fff_fffbu64.to_le_bytes());
    buf[dyn_off + 24..dyn_off + 32].copy_from_slice(&dt_flags_1.to_le_bytes());
    buf
}

fn build_synthetic_elf32(has_pt_tls: bool, dt_flags: u32, dt_flags_1: u32) -> Vec<u8> {
    let phoff = 52usize;
    let phnum = if has_pt_tls { 2usize } else { 1usize };
    let dyn_off = phoff + phnum * 32;
    let mut buf = vec![0u8; dyn_off + 3 * 8];

    buf[0..4].copy_from_slice(b"\x7fELF");
    buf[4] = 1; // ELFCLASS32
    buf[5] = 1; // ELFDATA2LSB
    buf[28..32].copy_from_slice(&(phoff as u32).to_le_bytes());
    buf[42..44].copy_from_slice(&32u16.to_le_bytes());
    buf[44..46].copy_from_slice(&(phnum as u16).to_le_bytes());

    let p0 = phoff;
    buf[p0..p0 + 4].copy_from_slice(&2u32.to_le_bytes());
    buf[p0 + 4..p0 + 8].copy_from_slice(&(dyn_off as u32).to_le_bytes());
    buf[p0 + 16..p0 + 20].copy_from_slice(&24u32.to_le_bytes());

    if has_pt_tls {
        let p1 = phoff + 32;
        buf[p1..p1 + 4].copy_from_slice(&7u32.to_le_bytes());
    }

    buf[dyn_off..dyn_off + 4].copy_from_slice(&30u32.to_le_bytes());
    buf[dyn_off + 4..dyn_off + 8].copy_from_slice(&dt_flags.to_le_bytes());
    buf[dyn_off + 8..dyn_off + 12].copy_from_slice(&0x6fff_fffbu32.to_le_bytes());
    buf[dyn_off + 12..dyn_off + 16].copy_from_slice(&dt_flags_1.to_le_bytes());
    buf
}

fn build_synthetic_macho64(hdr_flags: u32, sect_name: &str, sect_flags: u32) -> Vec<u8> {
    let seg_cmd_size = 72usize + 80usize;
    let mut buf = vec![0u8; 32 + seg_cmd_size];

    buf[0..4].copy_from_slice(&0xfeed_facfu32.to_le_bytes());
    buf[16..20].copy_from_slice(&1u32.to_le_bytes()); // ncmds = 1
    buf[20..24].copy_from_slice(&(seg_cmd_size as u32).to_le_bytes());
    buf[24..28].copy_from_slice(&hdr_flags.to_le_bytes());

    let lc = 32usize;
    buf[lc..lc + 4].copy_from_slice(&0x19u32.to_le_bytes()); // LC_SEGMENT_64
    buf[lc + 4..lc + 8].copy_from_slice(&(seg_cmd_size as u32).to_le_bytes());
    buf[lc + 64..lc + 68].copy_from_slice(&1u32.to_le_bytes()); // nsects = 1

    let sec = lc + 72;
    let name_bytes = sect_name.as_bytes();
    let copy_len = name_bytes.len().min(16);
    buf[sec..sec + copy_len].copy_from_slice(&name_bytes[..copy_len]);
    buf[sec + 64..sec + 68].copy_from_slice(&sect_flags.to_le_bytes());
    buf
}

fn build_synthetic_pe_with_rsds(original_pdb_slot: &str) -> Vec<u8> {
    let lfanew = 0x80usize;
    let opt_size = 240usize; // PE32+
    let sections_off = lfanew + 24 + opt_size;
    let rdata_raw_ptr = 0x400usize;
    let debug_dir_rva = 0x2000u32;
    let cv_rva = 0x2020u32;
    let cv_raw_ptr = rdata_raw_ptr + 0x20;
    let cv_size = 24 + original_pdb_slot.len() + 1;

    let mut buf = vec![0u8; cv_raw_ptr + cv_size + 32];
    buf[0..2].copy_from_slice(b"MZ");
    buf[0x3c..0x40].copy_from_slice(&(lfanew as u32).to_le_bytes());
    buf[lfanew..lfanew + 4].copy_from_slice(b"PE\0\0");

    let coff = lfanew + 4;
    buf[coff + 2..coff + 4].copy_from_slice(&1u16.to_le_bytes()); // 1 section
    buf[coff + 16..coff + 18].copy_from_slice(&(opt_size as u16).to_le_bytes());

    let opt = coff + 20;
    buf[opt..opt + 2].copy_from_slice(&0x20bu16.to_le_bytes()); // PE32+
    buf[opt + 64..opt + 68].copy_from_slice(&0x1234_5678u32.to_le_bytes()); // non-zero checksum
    buf[opt + 108..opt + 112].copy_from_slice(&16u32.to_le_bytes()); // NumberOfRvaAndSizes
    // IMAGE_DIRECTORY_ENTRY_DEBUG (index 6)
    buf[opt + 112 + 6 * 8..opt + 112 + 6 * 8 + 4].copy_from_slice(&debug_dir_rva.to_le_bytes());
    buf[opt + 112 + 6 * 8 + 4..opt + 112 + 6 * 8 + 8].copy_from_slice(&28u32.to_le_bytes());

    // Section 0 (.rdata): VA=0x2000, VirtSize=0x1000, RawSize=0x400, RawPtr=0x400
    buf[sections_off..sections_off + 6].copy_from_slice(b".rdata");
    buf[sections_off + 8..sections_off + 12].copy_from_slice(&0x1000u32.to_le_bytes());
    buf[sections_off + 12..sections_off + 16].copy_from_slice(&0x2000u32.to_le_bytes());
    buf[sections_off + 16..sections_off + 20].copy_from_slice(&0x400u32.to_le_bytes());
    buf[sections_off + 20..sections_off + 24]
        .copy_from_slice(&(rdata_raw_ptr as u32).to_le_bytes());

    // Debug directory entry at rdata_raw_ptr
    buf[rdata_raw_ptr + 12..rdata_raw_ptr + 16].copy_from_slice(&2u32.to_le_bytes()); // CODEVIEW
    buf[rdata_raw_ptr + 16..rdata_raw_ptr + 20].copy_from_slice(&(cv_size as u32).to_le_bytes());
    buf[rdata_raw_ptr + 20..rdata_raw_ptr + 24].copy_from_slice(&cv_rva.to_le_bytes());
    buf[rdata_raw_ptr + 24..rdata_raw_ptr + 28].copy_from_slice(&(cv_raw_ptr as u32).to_le_bytes());

    // RSDS CodeView record
    buf[cv_raw_ptr..cv_raw_ptr + 4].copy_from_slice(b"RSDS");
    buf[cv_raw_ptr + 24..cv_raw_ptr + 24 + original_pdb_slot.len()]
        .copy_from_slice(original_pdb_slot.as_bytes());

    buf
}

#[test]
fn elf64_and_elf32_residency_detects_nodelete_static_tls_and_pt_tls() {
    let clean = build_synthetic_elf64(false, 0, 0);
    let clean_info = inspect_library_residency(&clean);
    assert_eq!(clean_info.format, BinaryFormat::Elf64);
    assert!(!clean_info.will_pin_resident());

    let pinned64 = build_synthetic_elf64(true, 0x10, 0x08);
    let info64 = inspect_library_residency(&pinned64);
    assert_eq!(info64.format, BinaryFormat::Elf64);
    assert!(info64.nodelete);
    assert!(info64.static_tls);
    assert!(info64.has_tls);
    assert!(info64.will_pin_resident());

    let pinned32 = build_synthetic_elf32(false, 0, 0x08);
    let info32 = inspect_library_residency(&pinned32);
    assert_eq!(info32.format, BinaryFormat::Elf32);
    assert!(info32.nodelete);
    assert!(!info32.has_tls);
    assert!(info32.will_pin_resident());
}

#[test]
fn macho_residency_detects_thread_vars_tlv_and_reexport_flags() {
    let clean = build_synthetic_macho64(0x0010_0000, "__text", 0);
    let clean_info = inspect_library_residency(&clean);
    assert_eq!(clean_info.format, BinaryFormat::MachO64);
    assert!(clean_info.has_no_reexported_dylibs);
    assert!(!clean_info.has_tls);
    assert!(!clean_info.will_pin_resident());

    let tlv_sect = build_synthetic_macho64(0, "__thread_vars", 0x13);
    let tlv_info = inspect_library_residency(&tlv_sect);
    assert_eq!(tlv_info.format, BinaryFormat::MachO64);
    assert!(tlv_info.has_tls);
    assert!(tlv_info.will_pin_resident());

    let tlv_hdr = build_synthetic_macho64(0x0080_0000, "__data", 0);
    let hdr_info = inspect_library_residency(&tlv_hdr);
    assert!(hdr_info.has_tls);
    assert!(hdr_info.will_pin_resident());

    // Truncated/unknown data never panics.
    assert_eq!(
        inspect_library_residency(&[1, 2, 3]),
        LibraryResidencyInfo::default()
    );
}

#[test]
fn shadow_copy_stages_and_cleans_up_pdb_sidecar_and_patches_rsds() {
    let orig_pdb_str = "C:\\very\\long\\build\\output\\target\\debug\\sample_mod.pdb";
    let pe_bytes = build_synthetic_pe_with_rsds(orig_pdb_str);
    if cfg!(miri) {
        let mut pe = pe_bytes;
        assert!(patch_pe_codeview_pdb_path(
            &mut pe,
            Path::new("shadow_sample_mod.pdb")
        ));
        assert_eq!(&pe[0x80 + 24 + 64..0x80 + 24 + 68], &[0, 0, 0, 0]);
        return;
    }

    let dir = std::env::temp_dir().join(format!(
        "bam_pdb_shadow_test_{}_{}",
        std::process::id(),
        NEXT_SHADOW_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let src_dir = dir.join("src");
    let shd_dir = dir.join("shadow");
    std::fs::create_dir_all(&src_dir).unwrap();

    let dll_path = src_dir.join("sample_mod.dll");
    let pdb_path = src_dir.join("sample_mod.pdb");

    std::fs::write(&dll_path, &pe_bytes).unwrap();
    std::fs::write(&pdb_path, b"FAKE_PDB_DATA").unwrap();

    let guard = stage_shadow_copy(&dll_path, Some(&shd_dir)).unwrap();
    let shadow_dll = guard.path().to_path_buf();
    let shadow_pdb = guard
        .pdb_path()
        .expect("expected shadow .pdb sidecar to be staged")
        .to_path_buf();

    assert!(shadow_dll.is_file());
    assert!(shadow_pdb.is_file());
    assert_eq!(
        shadow_pdb.file_stem(),
        shadow_dll.file_stem(),
        "shadow .pdb must share unique per-generation stem with shadow .dll"
    );
    assert_eq!(std::fs::read(&shadow_pdb).unwrap(), b"FAKE_PDB_DATA");

    let patched_dll = std::fs::read(&shadow_dll).unwrap();
    let expected_pdb_name = shadow_pdb.file_name().unwrap().to_str().unwrap();
    assert!(
        patched_dll
            .windows(expected_pdb_name.len())
            .any(|w| w == expected_pdb_name.as_bytes()),
        "RSDS CodeView PDB path should be rewritten to shadow .pdb"
    );
    // CheckSum cleared to 0 after patching.
    assert_eq!(&patched_dll[0x80 + 24 + 64..0x80 + 24 + 68], &[0, 0, 0, 0]);

    drop(guard);
    assert!(!shadow_dll.exists());
    assert!(!shadow_pdb.exists());
    let _ = std::fs::remove_dir_all(&dir);
}
