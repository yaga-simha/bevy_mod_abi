//! End-to-end integration tests for `cargo bam pack` (`.bampack`), `cargo bam watch`,
//! `cargo bam verify` (`.bampack` & loose binaries), and enhanced `cargo bam doctor`.

use bevy_abi_mod_trust::{
    CertificateValidityPolicy, DEFAULT_BAMPACK_NAMESPACE, DEFAULT_SIGNATURE_NAMESPACE,
    ModTrustPolicy, PrivateKey, PublicKey, issue_ed25519_certificate_openssh,
};
use cargo_bam::pack::{self, PackOptions};
use cargo_bam::signing;
use cargo_bam::watch::{WatchConfig, WatchRunner};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tempfile::TempDir;

fn cargo_bam_bin() -> &'static str {
    env!("CARGO_BIN_EXE_cargo-bam")
}

fn find_staged_library(dir: &Path) -> PathBuf {
    fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| {
            p.extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| matches!(e, "so" | "dylib" | "dll"))
        })
        .expect("staged shared library must exist")
}

fn assert_no_temp_files(dir: &Path) {
    for entry in fs::read_dir(dir).unwrap().flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        assert!(
            !name_str.contains(".bamtmp."),
            "leftover temporary staging file found: {}",
            entry.path().display()
        );
    }
}

#[test]
fn e2e_new_build_sign_pack_verify_and_deterministic_packing() {
    let tmp = TempDir::new().unwrap();

    // 1. `cargo bam new` (via real binary with Cargo subcommand `bam` prefix)
    let new_out = Command::new(cargo_bam_bin())
        .args([
            "bam",
            "new",
            "pack_demo",
            "--lang",
            "c99",
            "--dir",
            tmp.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        new_out.status.success(),
        "new failed: {}",
        String::from_utf8_lossy(&new_out.stderr)
    );
    let proj = tmp.path().join("pack_demo");

    // 2. Populate scoped `assets/` in unsorted creation order
    fs::create_dir_all(proj.join("assets/textures")).unwrap();
    fs::create_dir_all(proj.join("assets/config")).unwrap();
    fs::write(
        proj.join("assets/textures/hero.bin"),
        [0xDE, 0xAD, 0xBE, 0xEF],
    )
    .unwrap();
    fs::write(
        proj.join("assets/config/meta.json"),
        b"{\"mod\":\"pack_demo\",\"tier\":3}\n",
    )
    .unwrap();
    fs::write(proj.join("assets/a_first.txt"), b"hello assets\n").unwrap();

    // 3. `cargo bam keygen`
    let key_prefix = tmp.path().join("keys/dev_signer");
    let pub_path = tmp.path().join("keys/dev_signer.pub");
    let keygen_out = Command::new(cargo_bam_bin())
        .args([
            "keygen",
            "--out",
            key_prefix.to_str().unwrap(),
            "--comment",
            "pack-e2e",
        ])
        .output()
        .unwrap();
    assert!(
        keygen_out.status.success(),
        "keygen failed: {}",
        String::from_utf8_lossy(&keygen_out.stderr)
    );
    assert!(key_prefix.is_file());
    assert!(pub_path.is_file());

    // 4. `cargo bam build` + `sign` + `verify` on loose binary
    let dist = tmp.path().join("dist");
    let build_out = Command::new(cargo_bam_bin())
        .args([
            "build",
            "--dir",
            proj.to_str().unwrap(),
            "--release",
            "--out-dir",
            dist.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        build_out.status.success(),
        "build failed: {}",
        String::from_utf8_lossy(&build_out.stderr)
    );
    let staged_lib = find_staged_library(&dist.join("pack_demo"));
    let lib_file_name = staged_lib.file_name().unwrap().to_str().unwrap().to_owned();

    let sign_out = Command::new(cargo_bam_bin())
        .args([
            "sign",
            "--key",
            key_prefix.to_str().unwrap(),
            staged_lib.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        sign_out.status.success(),
        "sign failed: {}",
        String::from_utf8_lossy(&sign_out.stderr)
    );
    let verify_loose = Command::new(cargo_bam_bin())
        .args([
            "verify",
            "--pubkey",
            pub_path.to_str().unwrap(),
            staged_lib.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        verify_loose.status.success(),
        "verify loose failed: {}",
        String::from_utf8_lossy(&verify_loose.stderr)
    );

    // 5. `cargo bam pack` (signed) twice -> must be byte-for-byte identical!
    let pack1 = tmp.path().join("out/pack_demo_1.bampack");
    let pack2 = tmp.path().join("out/pack_demo_2.bampack");

    let pack_cmd1 = Command::new(cargo_bam_bin())
        .args([
            "pack",
            "--dir",
            proj.to_str().unwrap(),
            "--release",
            "--key",
            key_prefix.to_str().unwrap(),
            "--out",
            pack1.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        pack_cmd1.status.success(),
        "pack 1 failed: {}",
        String::from_utf8_lossy(&pack_cmd1.stderr)
    );

    let pack_cmd2 = Command::new(cargo_bam_bin())
        .args([
            "pack",
            "--dir",
            proj.to_str().unwrap(),
            "--release",
            "--skip-build",
            "--key",
            key_prefix.to_str().unwrap(),
            "--out",
            pack2.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        pack_cmd2.status.success(),
        "pack 2 failed: {}",
        String::from_utf8_lossy(&pack_cmd2.stderr)
    );

    let bytes1 = fs::read(&pack1).unwrap();
    let bytes2 = fs::read(&pack2).unwrap();
    assert_eq!(
        bytes1, bytes2,
        "two .bampack runs on identical inputs must be byte-for-byte deterministic"
    );

    // 6. Verify `.bampack` via `cargo bam verify` CLI AND directly via `bevy_abi_mod_trust`!
    let verify_pack = Command::new(cargo_bam_bin())
        .args([
            "verify",
            "--pubkey",
            pub_path.to_str().unwrap(),
            pack1.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        verify_pack.status.success(),
        "verify .bampack failed: {}",
        String::from_utf8_lossy(&verify_pack.stderr)
    );
    let verify_stdout = String::from_utf8_lossy(&verify_pack.stdout);
    assert!(
        verify_stdout.contains("Verified .bampack archive"),
        "unexpected verify stdout: {verify_stdout}"
    );

    let policy = ModTrustPolicy::strict()
        .with_namespace(DEFAULT_BAMPACK_NAMESPACE)
        .unwrap()
        .add_public_key_file(&pub_path)
        .unwrap();
    let verified = policy
        .verify_bampack_file(&pack1)
        .expect("bevy_abi_mod_trust must verify cargo-bam .bampack archive");

    let entry_paths: Vec<&str> = verified.entries.iter().map(|e| e.path.as_str()).collect();
    let mut expected_paths = vec![
        "assets/a_first.txt".to_owned(),
        "assets/config/meta.json".to_owned(),
        "assets/textures/hero.bin".to_owned(),
        lib_file_name,
        "mod.toml".to_owned(),
    ];
    expected_paths.sort();
    assert_eq!(entry_paths, expected_paths);

    let hero = verified
        .entries
        .iter()
        .find(|e| e.path == "assets/textures/hero.bin")
        .unwrap();
    assert_eq!(hero.data, vec![0xDE, 0xAD, 0xBE, 0xEF]);

    // 7. Tampering any archive byte must fail BOTH CLI and trust policy verification
    let tampered_pack = tmp.path().join("out/tampered.bampack");
    let mut tampered_bytes = bytes1;
    let mid = tampered_bytes.len() / 2;
    tampered_bytes[mid] ^= 0x55;
    fs::write(&tampered_pack, &tampered_bytes).unwrap();
    assert!(policy.verify_bampack_file(&tampered_pack).is_err());
    let verify_tampered = Command::new(cargo_bam_bin())
        .args([
            "verify",
            "--pubkey",
            pub_path.to_str().unwrap(),
            tampered_pack.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        !verify_tampered.status.success(),
        "tampered .bampack must fail verification"
    );

    // Passing `--sig` on a self-contained `.bampack` archive must be rejected clearly
    let verify_with_sig = Command::new(cargo_bam_bin())
        .args([
            "verify",
            "--pubkey",
            pub_path.to_str().unwrap(),
            "--sig",
            signing::default_sig_path(&staged_lib).to_str().unwrap(),
            pack1.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        !verify_with_sig.status.success(),
        "`--sig` must be rejected on .bampack archives"
    );

    // 8. Unsigned `.bampack` is deterministic and readable via `inspect_bampack_entries`,
    //    but strictly rejected by cryptographic verification
    let unsigned1 = tmp.path().join("out/unsigned_1.bampack");
    let unsigned2 = tmp.path().join("out/unsigned_2.bampack");
    for out_path in [&unsigned1, &unsigned2] {
        let out = Command::new(cargo_bam_bin())
            .args([
                "pack",
                "--dir",
                proj.to_str().unwrap(),
                "--release",
                "--skip-build",
                "--out",
                out_path.to_str().unwrap(),
            ])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "unsigned pack failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let ubytes1 = fs::read(&unsigned1).unwrap();
    let ubytes2 = fs::read(&unsigned2).unwrap();
    assert_eq!(ubytes1, ubytes2);
    let (inspected, has_sig) = pack::inspect_bampack_entries(&ubytes1).unwrap();
    assert!(!has_sig);
    assert_eq!(inspected.len(), 5);
    assert!(policy.verify_bampack_bytes(&ubytes1).is_err());

    // 9. Certificate-signed `.bampack` (`--key` + `--cert`) verified with `--ca` + `--principal`
    let ca_prefix = tmp.path().join("keys/ca_root");
    let ca_pub = tmp.path().join("keys/ca_root.pub");
    let ca_keygen = Command::new(cargo_bam_bin())
        .args(["keygen", "--out", ca_prefix.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(ca_keygen.status.success());

    let ca_priv = PrivateKey::read_openssh_file(&ca_prefix).unwrap();
    let signer_pub = PublicKey::read_openssh_file(&pub_path).unwrap();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let issued = issue_ed25519_certificate_openssh(
        &ca_priv,
        &signer_pub,
        42,
        "pack-cert-e2e",
        &["pack_demo".to_owned()],
        now.saturating_sub(60),
        now + 3600,
    )
    .unwrap();
    let cert_path = tmp.path().join("keys/dev_signer-cert.pub");
    fs::write(&cert_path, format!("{}\n", issued.certificate_openssh)).unwrap();

    let cert_pack = tmp.path().join("out/cert_signed.bampack");
    let cert_pack_out = Command::new(cargo_bam_bin())
        .args([
            "pack",
            "--dir",
            proj.to_str().unwrap(),
            "--release",
            "--skip-build",
            "--key",
            key_prefix.to_str().unwrap(),
            "--cert",
            cert_path.to_str().unwrap(),
            "--out",
            cert_pack.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        cert_pack_out.status.success(),
        "cert pack failed: {}",
        String::from_utf8_lossy(&cert_pack_out.stderr)
    );

    let verify_cert_ok = Command::new(cargo_bam_bin())
        .args([
            "verify",
            "--ca",
            ca_pub.to_str().unwrap(),
            "--principal",
            "pack_demo",
            cert_pack.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        verify_cert_ok.status.success(),
        "verify cert .bampack failed: {}",
        String::from_utf8_lossy(&verify_cert_ok.stderr)
    );

    let verify_cert_wrong_principal = Command::new(cargo_bam_bin())
        .args([
            "verify",
            "--ca",
            ca_pub.to_str().unwrap(),
            "--principal",
            "wrong_mod",
            cert_pack.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        !verify_cert_wrong_principal.status.success(),
        "mismatched principal must fail verification"
    );

    let ca_policy = ModTrustPolicy::strict()
        .with_namespace(DEFAULT_BAMPACK_NAMESPACE)
        .unwrap()
        .add_ca_file(&ca_pub)
        .unwrap()
        .with_validity_policy(CertificateValidityPolicy::FixedTimestamp(now))
        .with_required_principal("pack_demo");
    let cert_verified = ca_policy.verify_bampack_file(&cert_pack).unwrap();
    assert_eq!(cert_verified.signature.serial, Some(42));
}

#[test]
fn e2e_pack_custom_assets_dir_and_rust_cdylib() {
    let tmp = TempDir::new().unwrap();

    cargo_bam::run_cli([
        "cargo-bam",
        "new",
        "rust_pack_demo",
        "--lang",
        "rust",
        "--dir",
        tmp.path().to_str().unwrap(),
    ])
    .unwrap();
    let proj = tmp.path().join("rust_pack_demo");

    let custom_assets = tmp.path().join("external_assets");
    fs::create_dir_all(custom_assets.join("shaders")).unwrap();
    fs::write(custom_assets.join("shaders/glow.wesl"), b"// shader\n").unwrap();

    let key_prefix = tmp.path().join("keys/rust_signer");
    let keygen = signing::generate_keypair(&key_prefix, Some("rust-pack")).unwrap();

    let out1 = tmp.path().join("rust_1.bampack");
    let out2 = tmp.path().join("rust_2.bampack");

    let res1 = pack::pack_mod(&PackOptions {
        dir: Some(proj.clone()),
        out: Some(out1.clone()),
        key: Some(keygen.private_key.clone()),
        assets_dir: Some(custom_assets.clone()),
        ..PackOptions::default()
    })
    .unwrap();
    assert!(res1.signed);
    assert!(
        res1.entry_paths
            .contains(&"assets/shaders/glow.wesl".to_owned())
    );

    let res2 = pack::pack_mod(&PackOptions {
        dir: Some(proj),
        out: Some(out2.clone()),
        key: Some(keygen.private_key),
        assets_dir: Some(custom_assets),
        skip_build: true,
        ..PackOptions::default()
    })
    .unwrap();
    assert_eq!(res1.archive_sha256, res2.archive_sha256);
    assert_eq!(fs::read(&out1).unwrap(), fs::read(&out2).unwrap());

    let verified = pack::verify_bampack_file(
        &out1,
        &[keygen.public_key],
        &[],
        None,
        DEFAULT_BAMPACK_NAMESPACE,
    )
    .unwrap();
    assert_eq!(verified.entries.len(), 3);
}

#[test]
fn e2e_watch_programmatic_edit_rebuild_sign_and_atomic_stage() {
    let tmp = TempDir::new().unwrap();

    cargo_bam::run_cli([
        "cargo-bam",
        "new",
        "watch_prog_mod",
        "--lang",
        "c99",
        "--dir",
        tmp.path().to_str().unwrap(),
    ])
    .unwrap();
    let proj = tmp.path().join("watch_prog_mod");
    let stage_root = tmp.path().join("hot_reload_stage");

    let key_prefix = tmp.path().join("keys/watch_signer");
    let keygen = signing::generate_keypair(&key_prefix, Some("watch-prog")).unwrap();

    let mut runner = WatchRunner::new(WatchConfig {
        dir: Some(proj.clone()),
        out_dir: Some(stage_root.clone()),
        sign_key: Some(keygen.private_key),
        poll_interval: Duration::from_millis(20),
        debounce: Duration::from_millis(20),
        ..WatchConfig::default()
    })
    .unwrap();

    // Cycle 1: initial build + sign + atomic stage
    let cycle1 = runner.run_initial_build().unwrap();
    assert_eq!(cycle1.cycle, 1);
    assert_eq!(cycle1.mod_id, "watch_prog_mod");
    assert!(cycle1.changed_files.is_empty());
    assert!(cycle1.library_path.is_file());
    let stage_dir = stage_root.join("watch_prog_mod");
    assert!(stage_dir.join("mod.toml").is_file());
    let sig1_path = cycle1.signature_path.as_ref().expect("sig must be staged");
    assert!(sig1_path.is_file());
    assert_no_temp_files(&stage_dir);

    let lib1_bytes = fs::read(&cycle1.library_path).unwrap();
    let sig1_bytes = fs::read(sig1_path).unwrap();
    signing::verify_mod_file(
        &cycle1.library_path,
        None,
        std::slice::from_ref(&keygen.public_key),
        DEFAULT_SIGNATURE_NAMESPACE,
    )
    .unwrap();

    // No file edits -> no rebuild
    assert!(runner.poll_and_rebuild_if_changed().unwrap().is_none());

    // Modify `src/mod.c` with a different manifest version string so compiled binary bytes change
    let mod_c = proj.join("src/mod.c");
    let original_c = fs::read_to_string(&mod_c).unwrap();
    let updated_c = original_c.replace("\"0.2.0\"", "\"0.2.1-hotreload\"");
    assert_ne!(original_c, updated_c);
    fs::write(&mod_c, updated_c).unwrap();

    // Cycle 2: change detected -> incremental rebuild -> re-sign -> atomic stage
    let cycle2 = runner
        .poll_and_rebuild_if_changed()
        .unwrap()
        .expect("modified src/mod.c must trigger rebuild");
    assert_eq!(cycle2.cycle, 2);
    assert_eq!(cycle2.changed_files, vec![PathBuf::from("src/mod.c")]);
    assert_no_temp_files(&stage_dir);

    let lib2_bytes = fs::read(&cycle2.library_path).unwrap();
    let sig2_bytes = fs::read(cycle2.signature_path.as_ref().unwrap()).unwrap();
    assert_ne!(
        lib1_bytes, lib2_bytes,
        "rebuilt library bytes should change"
    );
    assert_ne!(sig1_bytes, sig2_bytes, "re-signed .sig bytes should change");

    signing::verify_mod_file(
        &cycle2.library_path,
        None,
        &[keygen.public_key],
        DEFAULT_SIGNATURE_NAMESPACE,
    )
    .unwrap();
}

#[test]
fn e2e_watch_cli_once_and_max_cycles_file_edit_loop() {
    let tmp = TempDir::new().unwrap();

    cargo_bam::run_cli([
        "cargo-bam",
        "new",
        "watch_cli_mod",
        "--lang",
        "c99",
        "--dir",
        tmp.path().to_str().unwrap(),
    ])
    .unwrap();
    let proj = tmp.path().join("watch_cli_mod");
    let once_stage = tmp.path().join("stage_once");
    let loop_stage = tmp.path().join("stage_loop");

    let key_prefix = tmp.path().join("keys/cli_watch");
    let keygen = signing::generate_keypair(&key_prefix, Some("watch-cli")).unwrap();

    // 1. `cargo bam watch --once`
    let once_out = Command::new(cargo_bam_bin())
        .args([
            "watch",
            "--dir",
            proj.to_str().unwrap(),
            "--out-dir",
            once_stage.to_str().unwrap(),
            "--key",
            keygen.private_key.to_str().unwrap(),
            "--once",
        ])
        .output()
        .unwrap();
    assert!(
        once_out.status.success(),
        "watch --once failed: {}",
        String::from_utf8_lossy(&once_out.stderr)
    );
    let once_lib = find_staged_library(&once_stage.join("watch_cli_mod"));
    assert!(signing::default_sig_path(&once_lib).is_file());
    signing::verify_mod_file(
        &once_lib,
        None,
        std::slice::from_ref(&keygen.public_key),
        DEFAULT_SIGNATURE_NAMESPACE,
    )
    .unwrap();

    // 2. `cargo bam watch --poll-ms 25 --max-cycles 2`: live file edit -> rebuild -> sign -> stage
    let child = Command::new(cargo_bam_bin())
        .args([
            "watch",
            "--dir",
            proj.to_str().unwrap(),
            "--out-dir",
            loop_stage.to_str().unwrap(),
            "--key",
            keygen.private_key.to_str().unwrap(),
            "--poll-ms",
            "25",
            "--max-cycles",
            "2",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();

    let stage_mod_dir = loop_stage.join("watch_cli_mod");
    let deadline = Instant::now() + Duration::from_secs(20);
    let staged_lib = loop {
        if stage_mod_dir.is_dir()
            && let Some(lib) = fs::read_dir(&stage_mod_dir)
                .ok()
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.path())
                .find(|p| {
                    p.extension()
                        .and_then(|e| e.to_str())
                        .is_some_and(|e| matches!(e, "so" | "dylib" | "dll"))
                })
            && signing::default_sig_path(&lib).is_file()
        {
            break lib;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for watch cycle 1 staged artifact"
        );
        thread::sleep(Duration::from_millis(20));
    };

    let cycle1_sig = fs::read(signing::default_sig_path(&staged_lib)).unwrap();

    // Trigger cycle 2 by editing `src/mod.c`
    let mod_c = proj.join("src/mod.c");
    let src = fs::read_to_string(&mod_c).unwrap();
    fs::write(&mod_c, src.replace("\"heartbeat\"", "\"heartbeat_v2\"")).unwrap();

    let output = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "watch --max-cycles 2 failed:\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(stdout.contains("[watch cycle 1]"), "stdout: {stdout}");
    assert!(stdout.contains("[watch cycle 2]"), "stdout: {stdout}");
    assert_no_temp_files(&stage_mod_dir);

    let cycle2_sig = fs::read(signing::default_sig_path(&staged_lib)).unwrap();
    assert_ne!(
        cycle1_sig, cycle2_sig,
        "watch cycle 2 must update staged .sig atomically"
    );

    signing::verify_mod_file(
        &staged_lib,
        None,
        &[keygen.public_key],
        DEFAULT_SIGNATURE_NAMESPACE,
    )
    .unwrap();
}

#[test]
fn e2e_doctor_cli_reports_toolchains_abi_layout_sdk_parity_and_dag_health() {
    let tmp = TempDir::new().unwrap();
    let mods_dir = tmp.path().join("mods");
    fs::create_dir_all(mods_dir.join("core")).unwrap();
    fs::create_dir_all(mods_dir.join("expansion")).unwrap();

    fs::write(
        mods_dir.join("core/mod.toml"),
        "[mod]\nid = \"core\"\nversion = \"1.0.0\"\nabi_version = \"^0.3.0\"\n",
    )
    .unwrap();
    fs::write(
        mods_dir.join("expansion/mod.toml"),
        "[mod]\nid = \"expansion\"\nversion = \"1.2.0\"\nabi_version = \"^0.3.0\"\n\n[dependencies]\ncore = \"^1.0\"\n",
    )
    .unwrap();

    let ok_out = Command::new(cargo_bam_bin())
        .args(["bam", "doctor", "--dir", mods_dir.to_str().unwrap()])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&ok_out.stdout);
    let stderr = String::from_utf8_lossy(&ok_out.stderr);
    assert!(
        ok_out.status.success(),
        "doctor failed:\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    for needle in [
        "0.3.0",
        "0x00030000",
        "68-slot BamHostApi",
        "[ok] cargo:",
        "[ok] rustc:",
        "[ok] rust_target_abi:",
        "msvc:",
        "clang:",
        "gcc:",
        "zig:",
        "0.14-0.16",
        "[ok] sdk_headers:",
        "68/68 BamHostApi slot offsets verified",
        "validated_mods: 2 manifest(s) in DAG order (core@1.0.0 -> expansion@1.2.0)",
    ] {
        assert!(
            stdout.contains(needle),
            "doctor output missing `{needle}`:\n{stdout}"
        );
    }

    // Corrupt ABI version in `expansion/mod.toml` -> non-zero exit code
    fs::write(
        mods_dir.join("expansion/mod.toml"),
        "[mod]\nid = \"expansion\"\nversion = \"1.2.0\"\nabi_version = \"^99.0.0\"\n",
    )
    .unwrap();
    let bad_out = Command::new(cargo_bam_bin())
        .args(["doctor", "--dir", mods_dir.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        !bad_out.status.success(),
        "doctor must fail when mod.toml ABI version is incompatible"
    );
    let bad_stderr = String::from_utf8_lossy(&bad_out.stderr);
    assert!(
        bad_stderr.contains("0.3.0"),
        "expected ABI mismatch message in stderr, got: {bad_stderr}"
    );
}

#[test]
fn e2e_zig_scaffold_uses_translate_c_and_builds_when_zig_available() {
    let tmp = TempDir::new().unwrap();

    let new_out = Command::new(cargo_bam_bin())
        .args([
            "bam",
            "new",
            "zig_scaffold_demo",
            "--lang",
            "zig",
            "--dir",
            tmp.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        new_out.status.success(),
        "new --lang zig failed: {}",
        String::from_utf8_lossy(&new_out.stderr)
    );

    let proj = tmp.path().join("zig_scaffold_demo");
    let build_zig = fs::read_to_string(proj.join("build.zig")).unwrap();
    let root_zig = fs::read_to_string(proj.join("src/root.zig")).unwrap();
    let sdk_zig = fs::read_to_string(proj.join("include/bevy_abi_mod.zig")).unwrap();

    assert!(
        build_zig.contains("b.addTranslateC("),
        "build.zig missing b.addTranslateC:\n{build_zig}"
    );
    assert!(
        build_zig.contains("mod.addImport(\"bam_c\", bam_c_mod)"),
        "build.zig missing bam_c import:\n{build_zig}"
    );
    assert!(
        root_zig.contains("@import(\"bam_c\")"),
        "src/root.zig missing @import(\"bam_c\"):\n{root_zig}"
    );
    assert!(
        sdk_zig.contains("@import(\"bam_c\")"),
        "include/bevy_abi_mod.zig missing @import(\"bam_c\"):\n{sdk_zig}"
    );
    for (label, contents) in [
        ("build.zig", &build_zig),
        ("src/root.zig", &root_zig),
        ("include/bevy_abi_mod.zig", &sdk_zig),
    ] {
        assert!(
            !contents.contains(concat!("@", "cImport"))
                && !contents.contains(concat!("@", "cInclude")),
            "{label} must not emit legacy Zig C-import directives:\n{contents}"
        );
    }

    let zig_available = Command::new("zig")
        .arg("version")
        .output()
        .is_ok_and(|out| out.status.success());
    if !zig_available {
        return;
    }

    let dist = tmp.path().join("dist");
    let build_out = Command::new(cargo_bam_bin())
        .args([
            "build",
            "--dir",
            proj.to_str().unwrap(),
            "--release",
            "--out-dir",
            dist.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        build_out.status.success(),
        "cargo bam build for Zig scaffold failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&build_out.stdout),
        String::from_utf8_lossy(&build_out.stderr)
    );
    let staged_lib = find_staged_library(&dist.join("zig_scaffold_demo"));
    assert!(staged_lib.is_file());

    // Also verify `include/bevy_abi_mod.zig` (`ZIG_SDK`) compiles cleanly as `"bevy_abi_mod"`.
    let updated_root = format!(
        "{root_zig}\nconst bam_sdk = @import(\"bevy_abi_mod\");\ncomptime {{\n    _ = bam_sdk.offsets.MIGRATE_COMPONENT_COLUMN;\n    _ = bam_sdk.coversHostApiSlot;\n    _ = bam_sdk.copyColumnNoAlias;\n    _ = bam_sdk.checkResult;\n    _ = bam_sdk.mapError;\n}}\n"
    );
    fs::write(proj.join("src/root.zig"), updated_root).unwrap();
    let sdk_build = Command::new("zig")
        .current_dir(&proj)
        .args(["build", "-Doptimize=ReleaseFast"])
        .output()
        .unwrap();
    assert!(
        sdk_build.status.success(),
        "zig build using bevy_abi_mod module failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&sdk_build.stdout),
        String::from_utf8_lossy(&sdk_build.stderr)
    );
}
