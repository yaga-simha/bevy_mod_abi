//! Adversarial security tests for `.bampack` verification/extraction and embedded `.bam_sig`
//! / `__bam_sig` binary signature sections across ELF, PE, and Mach-O images.

use bevy_abi_mod_trust::{
    Algorithm, BAMPACK_MAGIC, BAMPACK_VERSION, BamPackBuilder, BamPackEntry, BamPackExtractError,
    BamPackExtractLimits, DEFAULT_BAMPACK_NAMESPACE, DEFAULT_SIGNATURE_NAMESPACE,
    MAX_BAMPACK_ENTRIES, MAX_BAMPACK_PATH_DEPTH, MAX_BAMPACK_PATH_LEN, ModTrustPolicy, PrivateKey,
    SignatureVerificationError, VerifiedBamPack, extract_embedded_bam_sig,
    parse_bampack_entries_unverified, sign_mod_bytes_with_key, validate_bampack_entry_path,
};
use sha2::{Digest, Sha256};
use ssh_key::rand_core::OsRng;
use std::fs;
use tempfile::TempDir;

fn make_key() -> PrivateKey {
    PrivateKey::random(&mut OsRng, Algorithm::Ed25519).unwrap()
}

/// Crafts a raw wire `.bampack` bypassing `BamPackBuilder` validation so we can test verifier
/// defenses against maliciously signed or crafted wire archives.
fn craft_raw_signed_bampack(
    key: &PrivateKey,
    namespace: &str,
    entries: &[(&str, &[u8], Option<[u8; 32]>)],
    override_entry_count: Option<u32>,
) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(BAMPACK_MAGIC);
    body.extend_from_slice(&BAMPACK_VERSION.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    let count = override_entry_count.unwrap_or(entries.len() as u32);
    body.extend_from_slice(&count.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());

    for &(path, data, custom_sha) in entries {
        let pb = path.as_bytes();
        body.extend_from_slice(&(pb.len() as u16).to_le_bytes());
        body.extend_from_slice(pb);
        body.extend_from_slice(&(data.len() as u64).to_le_bytes());
        let sha: [u8; 32] = custom_sha.unwrap_or_else(|| Sha256::digest(data).into());
        body.extend_from_slice(&sha);
        body.extend_from_slice(data);
    }

    let pem = sign_mod_bytes_with_key(key, namespace, &body).unwrap();
    body.extend_from_slice(&(pem.len() as u32).to_le_bytes());
    body.extend_from_slice(pem.as_bytes());
    body
}

#[test]
fn bampack_rejects_case_duplicates_prefix_collisions_windows_ntfs_and_empty_archives() {
    let key = make_key();
    let policy = ModTrustPolicy::from_public_key_openssh(&key.public_key().to_openssh().unwrap())
        .unwrap()
        .with_namespace(DEFAULT_BAMPACK_NAMESPACE)
        .unwrap();

    // 1. Empty archive rejected by both builder and verifier.
    assert!(
        BamPackBuilder::new()
            .build_signed_with_key(&key, DEFAULT_BAMPACK_NAMESPACE)
            .is_err()
    );
    let empty_wire = craft_raw_signed_bampack(&key, DEFAULT_BAMPACK_NAMESPACE, &[], Some(0));
    assert!(matches!(
        policy.verify_bampack_bytes(&empty_wire),
        Err(SignatureVerificationError::MalformedSignature { .. })
    ));

    // 2. Case-insensitive duplicates rejected in builder and verifier.
    assert!(
        BamPackBuilder::new()
            .add_entry("mod.toml", b"a".to_vec())
            .unwrap()
            .add_entry("MOD.TOML", b"b".to_vec())
            .is_err()
    );
    let dup_case_wire = craft_raw_signed_bampack(
        &key,
        DEFAULT_BAMPACK_NAMESPACE,
        &[("mod.toml", b"a", None), ("MOD.TOML", b"b", None)],
        None,
    );
    assert!(matches!(
        policy.verify_bampack_bytes(&dup_case_wire),
        Err(SignatureVerificationError::MalformedSignature { .. })
    ));

    // 3. File-vs-directory prefix collisions (both orders + mixed case).
    assert!(
        BamPackBuilder::new()
            .add_entry("assets", b"file".to_vec())
            .unwrap()
            .add_entry("assets/a.png", b"png".to_vec())
            .is_err()
    );
    assert!(
        BamPackBuilder::new()
            .add_entry("Assets/a.png", b"png".to_vec())
            .unwrap()
            .add_entry("assets", b"file".to_vec())
            .is_err()
    );
    let prefix_wire = craft_raw_signed_bampack(
        &key,
        DEFAULT_BAMPACK_NAMESPACE,
        &[("assets/a.png", b"png", None), ("ASSETS", b"file", None)],
        None,
    );
    assert!(matches!(
        policy.verify_bampack_bytes(&prefix_wire),
        Err(SignatureVerificationError::MalformedSignature { .. })
    ));

    // 4. Exhaustive path sanitization attacks (zip-slip, NTFS ADS, Windows reserved, trailing dot/space, control).
    let bad_paths = [
        "",
        "../evil",
        "assets/../evil",
        "./mod.toml",
        "/etc/passwd",
        "assets/",
        "assets//file.bin",
        "assets\\evil.bin",
        "C:evil.dll",
        "assets/foo:evil:$DATA",
        "assets/C:foo",
        "bad\0name",
        "bad\nname",
        "bad\rname",
        "bad\x1fname",
        "bad\x7fname",
        "mod.toml.",
        "mod.toml ",
        "dir./file.txt",
        "dir /file.txt",
        "CON",
        "con.toml",
        "PRN",
        "AUX.bin",
        "NUL.dll",
        "assets/COM1.txt",
        "assets/com9",
        "assets/LPT1.bin",
        "assets/lpt9.so",
    ];
    for bad in bad_paths {
        assert!(
            validate_bampack_entry_path(bad).is_err(),
            "validate_bampack_entry_path must reject {bad:?}"
        );
        assert!(
            BamPackBuilder::new().add_entry(bad, b"x".to_vec()).is_err(),
            "BamPackBuilder must reject {bad:?}"
        );
        let wire =
            craft_raw_signed_bampack(&key, DEFAULT_BAMPACK_NAMESPACE, &[(bad, b"x", None)], None);
        assert!(
            policy.verify_bampack_bytes(&wire).is_err(),
            "verify_bampack_bytes must reject {bad:?}"
        );
    }

    // Excessive depth and length rejected.
    let too_deep = vec!["a"; MAX_BAMPACK_PATH_DEPTH + 1].join("/");
    assert!(validate_bampack_entry_path(&too_deep).is_err());
    let too_long = format!("a/{}", "x".repeat(MAX_BAMPACK_PATH_LEN));
    assert!(validate_bampack_entry_path(&too_long).is_err());
}

#[test]
fn bampack_enforces_archive_bomb_bounds_and_verifies_signature_before_entry_hashing() {
    let trusted_key = make_key();
    let attacker_key = make_key();
    let policy =
        ModTrustPolicy::from_public_key_openssh(&trusted_key.public_key().to_openssh().unwrap())
            .unwrap()
            .with_namespace(DEFAULT_BAMPACK_NAMESPACE)
            .unwrap();

    // 1. Entry count > MAX_BAMPACK_ENTRIES rejected immediately in header scan.
    let bomb_count = craft_raw_signed_bampack(
        &trusted_key,
        DEFAULT_BAMPACK_NAMESPACE,
        &[("a.bin", b"1", None)],
        Some((MAX_BAMPACK_ENTRIES as u32) + 1),
    );
    assert!(matches!(
        policy.verify_bampack_bytes(&bomb_count),
        Err(SignatureVerificationError::MalformedSignature { .. })
    ));

    // 2. Cumulative size exceeding policy `max_binary_bytes` rejected.
    let big_pack = BamPackBuilder::new()
        .add_entry("payload.bin", vec![0xAA; 2048])
        .unwrap()
        .build_signed_with_key(&trusted_key, DEFAULT_BAMPACK_NAMESPACE)
        .unwrap();
    let tight_policy = policy.clone().with_max_binary_bytes(1024);
    assert!(matches!(
        tight_policy.verify_bampack_bytes(&big_pack),
        Err(SignatureVerificationError::BinaryTooLarge { .. })
    ));
    assert!(matches!(
        parse_bampack_entries_unverified(&big_pack, 1024),
        Err(SignatureVerificationError::BinaryTooLarge { .. })
    ));

    // 3. Signature verified BEFORE per-entry SHA-256 hashing & allocation:
    //    Craft an archive with a deliberately bogus entry SHA-256 signed by an UNTRUSTED key.
    //    Because signature verification runs first, it MUST fail with `UntrustedPublicKey`
    //    rather than `MalformedSignature` (SHA-256 mismatch)!
    let untrusted_bad_sha = craft_raw_signed_bampack(
        &attacker_key,
        DEFAULT_BAMPACK_NAMESPACE,
        &[("payload.bin", b"hello-world", Some([0xFF; 32]))],
        None,
    );
    assert!(matches!(
        policy.verify_bampack_bytes(&untrusted_bad_sha),
        Err(SignatureVerificationError::UntrustedPublicKey { .. })
    ));

    // Same bogus entry SHA-256 signed by TRUSTED key DOES fail post-signature SHA-256 check.
    let trusted_bad_sha = craft_raw_signed_bampack(
        &trusted_key,
        DEFAULT_BAMPACK_NAMESPACE,
        &[("payload.bin", b"hello-world", Some([0xFF; 32]))],
        None,
    );
    assert!(matches!(
        policy.verify_bampack_bytes(&trusted_bad_sha),
        Err(SignatureVerificationError::MalformedSignature { .. })
    ));
}

#[test]
fn verified_bampack_extract_to_dir_succeeds_enforces_limits_and_rolls_back_on_error() {
    let key = make_key();
    let policy = ModTrustPolicy::from_public_key_openssh(&key.public_key().to_openssh().unwrap())
        .unwrap()
        .with_namespace(DEFAULT_BAMPACK_NAMESPACE)
        .unwrap();

    let pack_bytes = BamPackBuilder::new()
        .add_entry(
            "mod.toml",
            b"[mod]\nid = \"ok\"\nversion = \"1.0.0\"\n".to_vec(),
        )
        .unwrap()
        .add_entry("assets/sub/data.bin", vec![1, 2, 3, 4, 5, 6, 7, 8])
        .unwrap()
        .build_signed_with_key(&key, DEFAULT_BAMPACK_NAMESPACE)
        .unwrap();

    let verified = policy.verify_bampack_bytes(&pack_bytes).unwrap();

    // 1. Clean extraction into fresh directory.
    let tmp = TempDir::new().unwrap();
    let out_dir = tmp.path().join("extracted");
    let written = verified.extract_to_dir(&out_dir).unwrap();
    assert_eq!(written.len(), 2);
    assert_eq!(
        fs::read(out_dir.join("assets/sub/data.bin")).unwrap(),
        vec![1, 2, 3, 4, 5, 6, 7, 8]
    );

    // 2. Re-extracting into non-empty directory refuses to overwrite existing files!
    let err_exists = verified.extract_to_dir(&out_dir).unwrap_err();
    assert!(matches!(
        err_exists,
        BamPackExtractError::AlreadyExists { .. }
    ));

    // 3. Partial extraction rollback: pre-populate second file so first file is created then
    //    second fails -> first file MUST be rolled back cleanly!
    let rollback_dir = tmp.path().join("rollback");
    fs::create_dir_all(rollback_dir.join("assets/sub")).unwrap();
    fs::write(rollback_dir.join("assets/sub/data.bin"), b"keep-me").unwrap();
    let err_rb = verified.extract_to_dir(&rollback_dir).unwrap_err();
    assert!(matches!(err_rb, BamPackExtractError::AlreadyExists { .. }));
    assert!(
        !rollback_dir.join("mod.toml").exists(),
        "mod.toml created before failure must be rolled back"
    );
    assert_eq!(
        fs::read(rollback_dir.join("assets/sub/data.bin")).unwrap(),
        b"keep-me"
    );

    // 4. Tight extraction limits rejected atomically before touching disk.
    let limit_dir = tmp.path().join("limits");
    assert!(matches!(
        verified.extract_to_dir_with_limits(
            &limit_dir,
            BamPackExtractLimits {
                max_entries: 1,
                max_total_bytes: 1024,
            }
        ),
        Err(BamPackExtractError::TooManyEntries { count: 2, limit: 1 })
    ));
    assert!(matches!(
        verified.extract_to_dir_with_limits(
            &limit_dir,
            BamPackExtractLimits {
                max_entries: 10,
                max_total_bytes: 10,
            }
        ),
        Err(BamPackExtractError::TotalBytesExceeded { .. })
    ));
    assert!(!limit_dir.exists());

    // 5. Mutated/tampered VerifiedBamPack fails defense-in-depth checks before touching disk.
    let mut tampered = verified.clone();
    tampered.entries[0].data[0] ^= 0xff;
    assert!(matches!(
        tampered.extract_to_dir(&limit_dir),
        Err(BamPackExtractError::DigestMismatch { .. })
    ));

    let empty = VerifiedBamPack {
        entries: Vec::new(),
        signature: verified.signature.clone(),
    };
    assert!(matches!(
        empty.extract_to_dir(&limit_dir),
        Err(BamPackExtractError::EmptyArchive)
    ));

    let mut evil_path = verified;
    evil_path.entries.push(BamPackEntry {
        path: "../escape.txt".to_owned(),
        sha256: Sha256::digest(b"x").into(),
        data: b"x".to_vec(),
    });
    assert!(matches!(
        evil_path.extract_to_dir(&limit_dir),
        Err(BamPackExtractError::InvalidEntryPath { .. })
    ));
}

#[cfg(unix)]
#[test]
fn verified_bampack_extract_to_dir_rejects_symlinks_without_touching_outside_files() {
    use std::os::unix::fs::symlink;

    let key = make_key();
    let policy = ModTrustPolicy::from_public_key_openssh(&key.public_key().to_openssh().unwrap())
        .unwrap()
        .with_namespace(DEFAULT_BAMPACK_NAMESPACE)
        .unwrap();

    let pack_bytes = BamPackBuilder::new()
        .add_entry(
            "mod.toml",
            b"[mod]\nid = \"ok\"\nversion = \"1.0.0\"\n".to_vec(),
        )
        .unwrap()
        .add_entry("assets/payload.bin", b"new-payload".to_vec())
        .unwrap()
        .build_signed_with_key(&key, DEFAULT_BAMPACK_NAMESPACE)
        .unwrap();
    let verified = policy.verify_bampack_bytes(&pack_bytes).unwrap();

    let tmp = TempDir::new().unwrap();
    let outside = tmp.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    let victim = outside.join("victim.txt");
    fs::write(&victim, b"original-secret").unwrap();

    // 1. Symlink destination root rejected.
    let link_root = tmp.path().join("link_root");
    symlink(&outside, &link_root).unwrap();
    assert!(matches!(
        verified.extract_to_dir(&link_root),
        Err(BamPackExtractError::SymlinkRejected { .. })
    ));

    // 2. Pre-existing symlinked intermediate directory (`dest/assets -> outside`) rejected & rolled back!
    let dest_dir_link = tmp.path().join("dest_dir_link");
    fs::create_dir_all(&dest_dir_link).unwrap();
    symlink(&outside, dest_dir_link.join("assets")).unwrap();
    assert!(matches!(
        verified.extract_to_dir(&dest_dir_link),
        Err(BamPackExtractError::SymlinkRejected { .. })
    ));
    assert!(!outside.join("payload.bin").exists());
    assert!(!dest_dir_link.join("mod.toml").exists());

    // 3. Pre-existing symlinked target file (`dest/assets/payload.bin -> victim`) rejected!
    let dest_file_link = tmp.path().join("dest_file_link");
    fs::create_dir_all(dest_file_link.join("assets")).unwrap();
    symlink(&victim, dest_file_link.join("assets/payload.bin")).unwrap();
    assert!(matches!(
        verified.extract_to_dir(&dest_file_link),
        Err(BamPackExtractError::SymlinkRejected { .. })
    ));
    assert_eq!(fs::read(&victim).unwrap(), b"original-secret");
}

// ================================================================================================
// Embedded `.bam_sig` / `__bam_sig` Adversarial Security Tests
// ================================================================================================

fn build_pe64(
    virt_size: u32,
    raw_size: u32,
    bam_characteristics: u32,
    duplicate_bam_sig: bool,
    overlap_text: bool,
) -> (Vec<u8>, usize) {
    let lfanew = 0x80usize;
    let opt_size = 240usize;
    let sections_off = lfanew + 24 + opt_size;
    let num_sections: u16 = if duplicate_bam_sig { 3 } else { 2 };
    let text_off = 0x400usize;
    let sig_off = if overlap_text {
        text_off + 0x40
    } else {
        text_off + 0x100
    };
    let total_len =
        sig_off + (raw_size as usize).max(0x200) + if duplicate_bam_sig { 0x400 } else { 0 };
    let mut buf = vec![0u8; total_len];

    buf[0..2].copy_from_slice(b"MZ");
    buf[0x3c..0x40].copy_from_slice(&(lfanew as u32).to_le_bytes());
    buf[lfanew..lfanew + 4].copy_from_slice(b"PE\0\0");
    buf[lfanew + 6..lfanew + 8].copy_from_slice(&num_sections.to_le_bytes());
    buf[lfanew + 20..lfanew + 22].copy_from_slice(&(opt_size as u16).to_le_bytes());
    buf[lfanew + 24..lfanew + 26].copy_from_slice(&0x20bu16.to_le_bytes());

    // Section 0: .text
    buf[sections_off..sections_off + 5].copy_from_slice(b".text");
    buf[sections_off + 8..sections_off + 12].copy_from_slice(&0x100u32.to_le_bytes());
    buf[sections_off + 16..sections_off + 20].copy_from_slice(&0x100u32.to_le_bytes());
    buf[sections_off + 20..sections_off + 24].copy_from_slice(&(text_off as u32).to_le_bytes());
    buf[text_off..text_off + 16].fill(0xCC);

    // Section 1: .bam_sig
    let s1 = sections_off + 40;
    buf[s1..s1 + 8].copy_from_slice(b".bam_sig");
    buf[s1 + 8..s1 + 12].copy_from_slice(&virt_size.to_le_bytes());
    buf[s1 + 16..s1 + 20].copy_from_slice(&raw_size.to_le_bytes());
    buf[s1 + 20..s1 + 24].copy_from_slice(&(sig_off as u32).to_le_bytes());
    buf[s1 + 36..s1 + 40].copy_from_slice(&bam_characteristics.to_le_bytes());

    if duplicate_bam_sig {
        let s2 = sections_off + 80;
        let sig2_off = sig_off + (raw_size as usize);
        buf[s2..s2 + 8].copy_from_slice(b".bam_sig");
        buf[s2 + 8..s2 + 12].copy_from_slice(&0x200u32.to_le_bytes());
        buf[s2 + 16..s2 + 20].copy_from_slice(&0x200u32.to_le_bytes());
        buf[s2 + 20..s2 + 24].copy_from_slice(&(sig2_off as u32).to_le_bytes());
    }

    (buf, sig_off)
}

fn build_elf64(
    sig_slot_len: usize,
    bam_flags: u64,
    duplicate_bam_sig: bool,
    overlap_text: bool,
) -> (Vec<u8>, usize) {
    let code_off = 64usize;
    let code_len = 64usize;
    let strtab = b"\0.text\0.shstrtab\0.bam_sig\0";
    let sig_off = if overlap_text {
        code_off + 16
    } else {
        code_off + code_len
    };
    let sig2_off = sig_off + sig_slot_len;
    let strtab_off = if duplicate_bam_sig {
        sig2_off + sig_slot_len
    } else {
        sig_off + sig_slot_len
    };
    let shoff = strtab_off + strtab.len();
    let shnum = if duplicate_bam_sig { 5usize } else { 4usize };
    let mut buf = vec![0u8; shoff + shnum * 64];

    buf[0..4].copy_from_slice(b"\x7fELF");
    buf[4] = 2; // ELFCLASS64
    buf[5] = 1; // ELFDATA2LSB
    buf[40..48].copy_from_slice(&(shoff as u64).to_le_bytes());
    buf[58..60].copy_from_slice(&64u16.to_le_bytes());
    buf[60..62].copy_from_slice(&(shnum as u16).to_le_bytes());
    buf[62..64].copy_from_slice(&2u16.to_le_bytes());

    buf[code_off..code_off + code_len].fill(0x90);
    buf[strtab_off..strtab_off + strtab.len()].copy_from_slice(strtab);

    let write_shdr = |b: &mut [u8],
                      idx: usize,
                      name_off: u32,
                      sh_type: u32,
                      sh_flags: u64,
                      off: usize,
                      sz: usize| {
        let s = shoff + idx * 64;
        b[s..s + 4].copy_from_slice(&name_off.to_le_bytes());
        b[s + 4..s + 8].copy_from_slice(&sh_type.to_le_bytes());
        b[s + 8..s + 16].copy_from_slice(&sh_flags.to_le_bytes());
        b[s + 24..s + 32].copy_from_slice(&(off as u64).to_le_bytes());
        b[s + 32..s + 40].copy_from_slice(&(sz as u64).to_le_bytes());
    };
    write_shdr(&mut buf, 1, 1, 1, 0x6, code_off, code_len);
    write_shdr(&mut buf, 2, 7, 3, 0, strtab_off, strtab.len());
    write_shdr(&mut buf, 3, 17, 1, bam_flags, sig_off, sig_slot_len);
    if duplicate_bam_sig {
        write_shdr(&mut buf, 4, 17, 1, 0, sig2_off, sig_slot_len);
    }
    (buf, sig_off)
}

fn build_macho64(
    sig_slot_len: usize,
    seg_initprot: u32,
    sec_flags: u32,
    duplicate_bam_sig: bool,
    overlap_header: bool,
) -> (Vec<u8>, usize) {
    let nsects = if duplicate_bam_sig { 2usize } else { 1usize };
    let seg_cmd_size = 72 + nsects * 80;
    let hdr_and_cmds = 32 + seg_cmd_size;
    let sig_off = if overlap_header {
        16
    } else {
        hdr_and_cmds + 32
    };
    let sig2_off = sig_off + sig_slot_len;
    let total = if duplicate_bam_sig {
        sig2_off + sig_slot_len
    } else {
        hdr_and_cmds + 32 + sig_slot_len
    };
    let mut buf = vec![0u8; total];

    buf[0..4].copy_from_slice(&0xfeed_facfu32.to_le_bytes());
    buf[16..20].copy_from_slice(&1u32.to_le_bytes());
    buf[20..24].copy_from_slice(&(seg_cmd_size as u32).to_le_bytes());

    let lc = 32usize;
    buf[lc..lc + 4].copy_from_slice(&0x19u32.to_le_bytes()); // LC_SEGMENT_64
    buf[lc + 4..lc + 8].copy_from_slice(&(seg_cmd_size as u32).to_le_bytes());
    buf[lc + 60..lc + 64].copy_from_slice(&seg_initprot.to_le_bytes());
    buf[lc + 64..lc + 68].copy_from_slice(&(nsects as u32).to_le_bytes());

    let sec0 = lc + 72;
    buf[sec0..sec0 + 9].copy_from_slice(b"__bam_sig");
    buf[sec0 + 40..sec0 + 48].copy_from_slice(&(sig_slot_len as u64).to_le_bytes());
    buf[sec0 + 48..sec0 + 52].copy_from_slice(&(sig_off as u32).to_le_bytes());
    buf[sec0 + 64..sec0 + 68].copy_from_slice(&sec_flags.to_le_bytes());

    if duplicate_bam_sig {
        let sec1 = sec0 + 80;
        buf[sec1..sec1 + 9].copy_from_slice(b"__bam_sig");
        buf[sec1 + 40..sec1 + 48].copy_from_slice(&(sig_slot_len as u64).to_le_bytes());
        buf[sec1 + 48..sec1 + 52].copy_from_slice(&(sig2_off as u32).to_le_bytes());
    }

    (buf, sig_off)
}

#[test]
fn pe_bam_sig_blocks_virt_size_smuggling_interior_nul_trailing_pem_duplicates_and_overlap() {
    let key = make_key();
    let policy =
        ModTrustPolicy::from_public_key_openssh(&key.public_key().to_openssh().unwrap()).unwrap();

    // 1. PE `virt_size < raw_size`: valid when padding is strictly 0x00, REJECTED if ANY byte in
    //    `[sec_off + virt_size .. sec_off + raw_size]` is non-zero!
    let (mut pe_img, sig_off) = build_pe64(512, 1024, 0, false, false);
    let pem = sign_mod_bytes_with_key(&key, DEFAULT_SIGNATURE_NAMESPACE, &pe_img).unwrap();
    assert!(pem.len() <= 512);
    pe_img[sig_off..sig_off + pem.len()].copy_from_slice(pem.as_bytes());
    assert!(policy.verify_embedded_bam_sig(&pe_img).is_ok());

    // Smuggle 1 byte beyond `virt_size` inside `[sig_off + 512 .. sig_off + 1024]`:
    let mut smuggled_pe = pe_img.clone();
    smuggled_pe[sig_off + 512] = 0x42;
    assert!(matches!(
        policy.verify_embedded_bam_sig(&smuggled_pe),
        Err(SignatureVerificationError::MalformedSignature { .. })
    ));

    // Smuggle bytes after NUL padding INSIDE `[sig_off .. sig_off + virt_size]`:
    let mut interior_nul_pe = pe_img.clone();
    interior_nul_pe[sig_off + pem.len() + 4] = b'X';
    assert!(matches!(
        policy.verify_embedded_bam_sig(&interior_nul_pe),
        Err(SignatureVerificationError::MalformedSignature { .. })
    ));

    // Smuggle non-whitespace bytes IMMEDIATELY after `-----END SSH SIGNATURE-----` (no NUL gap):
    let mut trailing_pem_pe = pe_img;
    trailing_pem_pe[sig_off + pem.len()..sig_off + pem.len() + 4].copy_from_slice(b"EVIL");
    assert!(matches!(
        policy.verify_embedded_bam_sig(&trailing_pem_pe),
        Err(SignatureVerificationError::MalformedSignature { .. })
    ));

    // 2. Duplicate PE `.bam_sig` sections rejected!
    let (mut dup_pe, dup_sig_off) = build_pe64(1024, 1024, 0, true, false);
    dup_pe[dup_sig_off..dup_sig_off + pem.len()].copy_from_slice(pem.as_bytes());
    assert!(matches!(
        extract_embedded_bam_sig(&dup_pe),
        Err(SignatureVerificationError::MalformedSignature { .. })
    ));

    // 3. Executable PE `.bam_sig` section (`IMAGE_SCN_MEM_EXECUTE`) rejected!
    let (mut exec_pe, exec_off) = build_pe64(1024, 1024, 0x2000_0000, false, false);
    exec_pe[exec_off..exec_off + pem.len()].copy_from_slice(pem.as_bytes());
    assert!(matches!(
        extract_embedded_bam_sig(&exec_pe),
        Err(SignatureVerificationError::MalformedSignature { .. })
    ));

    // 4. Overlapping PE `.bam_sig` and `.text` rejected!
    let (mut ov_pe, ov_off) = build_pe64(1024, 1024, 0, false, true);
    ov_pe[ov_off..ov_off + pem.len()].copy_from_slice(pem.as_bytes());
    assert!(matches!(
        extract_embedded_bam_sig(&ov_pe),
        Err(SignatureVerificationError::MalformedSignature { .. })
    ));

    // 5. PE `num_sections > 256` rejected instead of silently truncating!
    let (mut many_sec_pe, _) = build_pe64(1024, 1024, 0, false, false);
    many_sec_pe[0x80 + 6..0x80 + 8].copy_from_slice(&300u16.to_le_bytes());
    assert!(matches!(
        extract_embedded_bam_sig(&many_sec_pe),
        Err(SignatureVerificationError::MalformedSignature { .. })
    ));
}

#[test]
fn elf_and_macho_bam_sig_reject_duplicates_executable_overlap_and_scan_overflow() {
    let key = make_key();
    let pem = sign_mod_bytes_with_key(&key, DEFAULT_SIGNATURE_NAMESPACE, b"dummy").unwrap();

    // ELF: duplicate `.bam_sig` rejected.
    let (mut dup_elf, dup_off) = build_elf64(1024, 0, true, false);
    dup_elf[dup_off..dup_off + pem.len()].copy_from_slice(pem.as_bytes());
    assert!(matches!(
        extract_embedded_bam_sig(&dup_elf),
        Err(SignatureVerificationError::MalformedSignature { .. })
    ));

    // ELF: executable `.bam_sig` (`SHF_EXECINSTR = 0x4`) rejected.
    let (mut exec_elf, exec_off) = build_elf64(1024, 0x4, false, false);
    exec_elf[exec_off..exec_off + pem.len()].copy_from_slice(pem.as_bytes());
    assert!(matches!(
        extract_embedded_bam_sig(&exec_elf),
        Err(SignatureVerificationError::MalformedSignature { .. })
    ));

    // ELF: `.bam_sig` overlapping `.text` rejected.
    let (mut ov_elf, ov_off) = build_elf64(1024, 0, false, true);
    ov_elf[ov_off..ov_off + pem.len()].copy_from_slice(pem.as_bytes());
    assert!(matches!(
        extract_embedded_bam_sig(&ov_elf),
        Err(SignatureVerificationError::MalformedSignature { .. })
    ));

    // Mach-O thin: duplicate `__bam_sig` rejected.
    let (mut dup_macho, m_off) = build_macho64(1024, 0, 0, true, false);
    dup_macho[m_off..m_off + pem.len()].copy_from_slice(pem.as_bytes());
    assert!(matches!(
        extract_embedded_bam_sig(&dup_macho),
        Err(SignatureVerificationError::MalformedSignature { .. })
    ));

    // Mach-O thin: executable segment (`VM_PROT_EXECUTE = 0x4`) rejected.
    let (mut exec_seg_macho, es_off) = build_macho64(1024, 0x4, 0, false, false);
    exec_seg_macho[es_off..es_off + pem.len()].copy_from_slice(pem.as_bytes());
    assert!(matches!(
        extract_embedded_bam_sig(&exec_seg_macho),
        Err(SignatureVerificationError::MalformedSignature { .. })
    ));

    // Mach-O thin: executable section (`S_ATTR_PURE_INSTRUCTIONS`) rejected.
    let (mut exec_sec_macho, ec_off) = build_macho64(1024, 0, 0x8000_0000, false, false);
    exec_sec_macho[ec_off..ec_off + pem.len()].copy_from_slice(pem.as_bytes());
    assert!(matches!(
        extract_embedded_bam_sig(&exec_sec_macho),
        Err(SignatureVerificationError::MalformedSignature { .. })
    ));

    // Mach-O thin: `__bam_sig` overlapping Mach-O header/load commands rejected.
    let (mut ov_macho, ov_m_off) = build_macho64(512, 0, 0, false, true);
    ov_macho[ov_m_off..ov_m_off + 16].copy_from_slice(b"-----BEGIN SSH S");
    assert!(matches!(
        extract_embedded_bam_sig(&ov_macho),
        Err(SignatureVerificationError::MalformedSignature { .. })
    ));

    // Mach-O thin: `ncmds > 4096` rejected instead of silently truncating!
    let (mut many_cmds, _) = build_macho64(1024, 0, 0, false, false);
    many_cmds[16..20].copy_from_slice(&5000u32.to_le_bytes());
    assert!(matches!(
        extract_embedded_bam_sig(&many_cmds),
        Err(SignatureVerificationError::MalformedSignature { .. })
    ));

    // Mach-O fat: duplicate `__bam_sig` across two fat slices rejected!
    let (mut slice_a, off_a) = build_macho64(1024, 0, 0, false, false);
    slice_a[off_a..off_a + pem.len()].copy_from_slice(pem.as_bytes());
    let slice_b = slice_a.clone();
    let fat_hdr_len = 8 + 2 * 20;
    let slice_a_off = 0x100usize;
    let slice_b_off = slice_a_off + slice_a.len();
    let mut fat = vec![0u8; slice_b_off + slice_b.len()];
    fat[0..4].copy_from_slice(&0xcafe_babeu32.to_be_bytes());
    fat[4..8].copy_from_slice(&2u32.to_be_bytes());
    assert!(fat_hdr_len <= slice_a_off);
    // Arch 0
    fat[8 + 8..8 + 12].copy_from_slice(&(slice_a_off as u32).to_be_bytes());
    fat[8 + 12..8 + 16].copy_from_slice(&(slice_a.len() as u32).to_be_bytes());
    // Arch 1
    fat[28 + 8..28 + 12].copy_from_slice(&(slice_b_off as u32).to_be_bytes());
    fat[28 + 12..28 + 16].copy_from_slice(&(slice_b.len() as u32).to_be_bytes());
    fat[slice_a_off..slice_a_off + slice_a.len()].copy_from_slice(&slice_a);
    fat[slice_b_off..slice_b_off + slice_b.len()].copy_from_slice(&slice_b);

    assert!(matches!(
        extract_embedded_bam_sig(&fat),
        Err(SignatureVerificationError::MalformedSignature { .. })
    ));
}
