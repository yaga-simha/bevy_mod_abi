//! Integration and security tests for the `cargo-bam` CLI (`new`, `export-sdk`, `build`,
//! `pack`, `watch`, `check-mods`, `doctor`, `keygen`, `sign`, `verify`).

use bevy_abi_mod_trust::{
    DEFAULT_BAMPACK_NAMESPACE, DEFAULT_SIGNATURE_NAMESPACE, KrlBuilder, PrivateKey, PublicKey,
    issue_ed25519_certificate_openssh,
};
use cargo_bam::{CliError, dag, doctor, pack, run_cli, signing, watch};
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use tempfile::TempDir;

fn local_guest_sdk_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("bevy_abi_mod_guest")
}

#[test]
fn cli_export_sdk_writes_c_cpp_and_zig_headers() {
    let tmp = TempDir::new().unwrap();
    let out_dir = tmp.path().join("sdk");

    run_cli([
        "cargo-bam",
        "export-sdk",
        "--out",
        out_dir.to_str().unwrap(),
    ])
    .unwrap();
    assert!(out_dir.join("bevy_abi_mod.h").is_file());
    assert!(out_dir.join("bevy_abi_mod.hpp").is_file());
    assert!(out_dir.join("bevy_abi_mod.zig").is_file());

    let h = fs::read_to_string(out_dir.join("bevy_abi_mod.h")).unwrap();
    assert!(h.contains("BAM_ABI_VERSION"));
}

#[test]
fn cli_new_build_keygen_sign_and_verify_c99_and_cpp17_mods() {
    let tmp = TempDir::new().unwrap();

    // 1. Test `cargo bam new` (Cargo subcommand prefix stripping) for C99 & C++17
    run_cli([
        "cargo-bam",
        "bam",
        "new",
        "sample_c_mod",
        "--lang",
        "c99",
        "--dir",
        tmp.path().to_str().unwrap(),
    ])
    .unwrap();
    let c_proj = tmp.path().join("sample_c_mod");
    assert!(c_proj.join("mod.toml").is_file());
    assert!(c_proj.join("src/mod.c").is_file());
    let c_src = fs::read_to_string(c_proj.join("src/mod.c")).unwrap();
    assert!(c_src.contains("bam_host_api_copy_bounded"));
    assert!(!c_src.contains("g_state.api = *api"));

    // 2. Build C99 mod & stage into `dist/sample_c_mod/`
    let dist_dir = tmp.path().join("dist_c");
    run_cli([
        "cargo-bam",
        "build",
        "--manifest-path",
        c_proj.join("mod.toml").to_str().unwrap(),
        "--release",
        "--out-dir",
        dist_dir.to_str().unwrap(),
    ])
    .unwrap();
    let staged_c_dir = dist_dir.join("sample_c_mod");
    assert!(staged_c_dir.join("mod.toml").is_file());
    let staged_c_lib = fs::read_dir(&staged_c_dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| {
            p.extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| matches!(e, "so" | "dylib" | "dll"))
        })
        .expect("staged C99 shared library must exist");

    // 3. Keygen, sign, and verify the staged C99 mod binary
    let key_prefix = tmp.path().join("keys/dev_signer");
    run_cli([
        "cargo-bam",
        "keygen",
        "--out",
        key_prefix.to_str().unwrap(),
        "--comment",
        "ci-test",
    ])
    .unwrap();
    assert!(key_prefix.is_file());
    let pub_path = tmp.path().join("keys/dev_signer.pub");
    assert!(pub_path.is_file());

    run_cli([
        "cargo-bam",
        "sign",
        "--key",
        key_prefix.to_str().unwrap(),
        staged_c_lib.to_str().unwrap(),
    ])
    .unwrap();
    let sig_path = signing::default_sig_path(&staged_c_lib);
    assert!(sig_path.is_file());

    run_cli([
        "cargo-bam",
        "verify",
        "--pubkey",
        pub_path.to_str().unwrap(),
        staged_c_lib.to_str().unwrap(),
    ])
    .unwrap();

    // Tampering the binary must fail verification!
    let mut tampered = fs::read(&staged_c_lib).unwrap();
    tampered[0] ^= 0xff;
    fs::write(&staged_c_lib, tampered).unwrap();
    assert!(
        run_cli([
            "cargo-bam",
            "verify",
            "--pubkey",
            pub_path.to_str().unwrap(),
            staged_c_lib.to_str().unwrap(),
        ])
        .is_err()
    );

    // 4. Scaffold & build C++17 mod with `--sign-key` staged into `dist_cpp/sample_cpp_mod/`
    run_cli([
        "cargo-bam",
        "new",
        "sample_cpp_mod",
        "--lang",
        "cpp17",
        "--dir",
        tmp.path().to_str().unwrap(),
    ])
    .unwrap();
    let cpp_proj = tmp.path().join("sample_cpp_mod");
    let cpp_src = fs::read_to_string(cpp_proj.join("src/mod.cpp")).unwrap();
    assert!(cpp_src.contains("bam_host_api_copy_bounded"));
    assert!(!cpp_src.contains("make_unique<CppModInstance>(*api)"));

    let dist_cpp = tmp.path().join("dist_cpp");
    run_cli([
        "cargo-bam",
        "build",
        "--dir",
        cpp_proj.to_str().unwrap(),
        "--out-dir",
        dist_cpp.to_str().unwrap(),
        "--sign-key",
        key_prefix.to_str().unwrap(),
    ])
    .unwrap();
    let staged_cpp_dir = dist_cpp.join("sample_cpp_mod");
    assert!(staged_cpp_dir.join("mod.toml").is_file());
    let staged_cpp_lib = fs::read_dir(&staged_cpp_dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| {
            p.extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| matches!(e, "so" | "dylib" | "dll"))
        })
        .expect("staged C++17 shared library must exist");
    assert!(signing::default_sig_path(&staged_cpp_lib).is_file());
    run_cli([
        "cargo-bam",
        "verify",
        "--pubkey",
        pub_path.to_str().unwrap(),
        staged_cpp_lib.to_str().unwrap(),
    ])
    .unwrap();
}

#[test]
fn cli_new_rust_template_has_no_path_leak_and_uses_zero_boilerplate_api() {
    let tmp = TempDir::new().unwrap();

    // Default `cargo bam new --lang rust` (WITHOUT `--guest-sdk-path`) must NOT leak build paths!
    run_cli([
        "cargo-bam",
        "new",
        "portable_mod",
        "--lang",
        "rust",
        "--dir",
        tmp.path().to_str().unwrap(),
    ])
    .unwrap();

    let proj = tmp.path().join("portable_mod");
    let cargo_toml = fs::read_to_string(proj.join("Cargo.toml")).unwrap();
    let mod_toml = fs::read_to_string(proj.join("mod.toml")).unwrap();
    let lib_rs = fs::read_to_string(proj.join("src/lib.rs")).unwrap();

    assert!(
        cargo_toml
            .contains("bevy_abi_mod_guest = { version = \"0.3.0\", features = [\"derive\"] }"),
        "expected crates.io 0.3.0 dep with derive feature, got:\n{cargo_toml}"
    );
    assert!(
        !cargo_toml.contains("path =") && !cargo_toml.contains(env!("CARGO_MANIFEST_DIR")),
        "default Cargo.toml must not leak build-machine path:\n{cargo_toml}"
    );
    assert!(mod_toml.contains("version = \"0.1.0\""));
    assert!(mod_toml.contains("abi_version = \"0.3.0\""));

    // Locked B3 zero-boilerplate derive & typed query API assertions:
    assert!(
        lib_rs
            .contains("#[derive(Clone, Copy, Debug, Default, PartialEq, AbiPod, GuestComponent)]")
    );
    assert!(lib_rs.contains("#[guest_component(name = \"portable_mod::Counter\")]"));
    assert!(lib_rs.contains(".query_typed::<&mut Counter>()"));
    assert!(lib_rs.contains("ctx.single_query::<&mut Counter>().for_each("));
    assert!(!lib_rs.contains("impl GuestComponent"));
    assert!(!lib_rs.contains("component_mut(0)"));
}

#[test]
fn cli_new_and_build_rust_cdylib_mod() {
    let tmp = TempDir::new().unwrap();
    let guest_sdk = local_guest_sdk_path();
    run_cli([
        "cargo-bam",
        "new",
        "sample_rust_mod",
        "--lang",
        "rust",
        "--dir",
        tmp.path().to_str().unwrap(),
        "--guest-sdk-path",
        guest_sdk.to_str().unwrap(),
    ])
    .unwrap();

    let rust_proj = tmp.path().join("sample_rust_mod");
    assert!(rust_proj.join("mod.toml").is_file());
    assert!(rust_proj.join("Cargo.toml").is_file());
    assert!(rust_proj.join("src/lib.rs").is_file());

    let cargo_toml = fs::read_to_string(rust_proj.join("Cargo.toml")).unwrap();
    assert!(cargo_toml.contains("path = "));
    assert!(cargo_toml.contains("features = [\"derive\"]"));

    // Compile the unmodified zero-boilerplate `#[derive(..., AbiPod, GuestComponent)]` +
    // `query_typed` scaffold template end-to-end against the local workspace `bevy_abi_mod_guest`.
    let scaffold_lib = fs::read_to_string(rust_proj.join("src/lib.rs")).unwrap();
    assert!(scaffold_lib.contains("AbiPod, GuestComponent"));
    assert!(scaffold_lib.contains(".query_typed::<&mut Counter>()"));

    let dist_rust = tmp.path().join("dist_rust");
    run_cli([
        "cargo-bam",
        "build",
        "--dir",
        rust_proj.to_str().unwrap(),
        "--out-dir",
        dist_rust.to_str().unwrap(),
    ])
    .unwrap();
    let staged_rust_dir = dist_rust.join("sample_rust_mod");
    assert!(staged_rust_dir.join("mod.toml").is_file());
    let has_lib = fs::read_dir(&staged_rust_dir).unwrap().flatten().any(|e| {
        e.path()
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| matches!(ext, "so" | "dylib" | "dll"))
    });
    assert!(
        has_lib,
        "built Rust cdylib must be staged into dist_rust/<mod_id>/"
    );
}

#[test]
fn cli_check_mods_validates_dag_semver_mismatch_and_cycles() {
    let tmp = TempDir::new().unwrap();
    let mods_dir = tmp.path().join("mods");
    fs::create_dir_all(mods_dir.join("base")).unwrap();
    fs::create_dir_all(mods_dir.join("addon")).unwrap();
    fs::create_dir_all(mods_dir.join("ui")).unwrap();

    fs::write(
        mods_dir.join("base/mod.toml"),
        "[mod]\nid = \"base\"\nversion = \"1.2.0\"\nabi_version = \"^0.3.0\"\n",
    )
    .unwrap();
    fs::write(
        mods_dir.join("addon/mod.toml"),
        "[mod]\nid = \"addon\"\nversion = \"1.0.0\"\nabi_version = \"^0.3.0\"\n\n[dependencies]\nbase = \"^1.0\"\n",
    )
    .unwrap();
    fs::write(
        mods_dir.join("ui/mod.toml"),
        "[mod]\nid = \"ui\"\nversion = \"0.5.0\"\n\n[dependencies]\naddon = \"^1.0\"\n",
    )
    .unwrap();

    let order = dag::check_mods_dir(&mods_dir).unwrap();
    let ids: Vec<&str> = order.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, vec!["base", "addon", "ui"]);
    run_cli(["cargo-bam", "check-mods", mods_dir.to_str().unwrap()]).unwrap();

    fs::write(
        mods_dir.join("addon/mod.toml"),
        "[mod]\nid = \"addon\"\nversion = \"1.0.0\"\n\n[dependencies]\nbase = \"^2.0\"\n",
    )
    .unwrap();
    let err = run_cli(["cargo-bam", "check-mods", mods_dir.to_str().unwrap()]).unwrap_err();
    assert!(
        err.to_string().contains("requires 'base' ^2.0"),
        "unexpected error: {err}"
    );

    fs::write(
        mods_dir.join("addon/mod.toml"),
        "[mod]\nid = \"addon\"\nversion = \"1.0.0\"\n\n[dependencies]\nbase = \"^1.0\"\nui = \"^0.5\"\n",
    )
    .unwrap();
    let cyc_err = run_cli(["cargo-bam", "check-mods", mods_dir.to_str().unwrap()]).unwrap_err();
    assert!(
        cyc_err.to_string().contains("dependency cycle"),
        "unexpected error: {cyc_err}"
    );
}

#[test]
fn cli_new_scaffolds_zig_mod_with_bounded_host_api_copy() {
    let tmp = TempDir::new().unwrap();
    run_cli([
        "cargo-bam",
        "new",
        "sample_zig_mod",
        "--lang",
        "zig",
        "--dir",
        tmp.path().to_str().unwrap(),
    ])
    .unwrap();
    let zig_proj = tmp.path().join("sample_zig_mod");
    assert!(zig_proj.join("mod.toml").is_file());
    assert!(zig_proj.join("build.zig").is_file());
    assert!(zig_proj.join("src/root.zig").is_file());
    assert!(zig_proj.join("include/bevy_abi_mod.zig").is_file());

    let root_zig = fs::read_to_string(zig_proj.join("src/root.zig")).unwrap();
    assert!(root_zig.contains("c.bam_host_api_copy_bounded("));
    assert!(!root_zig.contains(".api = host_api.*"));
}

#[test]
fn cli_header_alias_and_doctor_validate_environment_and_mods() {
    let tmp = TempDir::new().unwrap();
    let out_dir = tmp.path().join("headers");
    run_cli(["cargo", "bam", "header", "--out", out_dir.to_str().unwrap()]).unwrap();
    assert!(out_dir.join("bevy_abi_mod.h").is_file());
    assert!(out_dir.join("bevy_abi_mod.hpp").is_file());
    assert!(out_dir.join("bevy_abi_mod.zig").is_file());

    let mods_dir = tmp.path().join("mods");
    fs::create_dir_all(mods_dir.join("core")).unwrap();
    fs::write(
        mods_dir.join("core/mod.toml"),
        "[mod]\nid = \"core\"\nversion = \"0.2.0\"\nabi_version = \"^0.3.0\"\n",
    )
    .unwrap();

    run_cli(["cargo-bam", "doctor", "--dir", mods_dir.to_str().unwrap()]).unwrap();

    let report = doctor::run_doctor(Some(&mods_dir), false).unwrap();
    assert!(report.ok);
    assert_eq!(report.host_abi_version, "0.3.0");
    assert!(
        report
            .tools
            .iter()
            .any(|t| t.name == "cargo" && t.available)
    );
    assert!(
        report
            .tools
            .iter()
            .any(|t| t.name == "rustc" && t.available)
    );
    assert!(
        report
            .tools
            .iter()
            .any(|t| t.name == "sdk_headers" && t.available)
    );
    assert_eq!(report.validated_mods.as_ref().map(Vec::len), Some(1));

    fs::write(
        mods_dir.join("core/mod.toml"),
        "[mod]\nid = \"core\"\nversion = \"0.2.0\"\nabi_version = \"^99.0.0\"\n",
    )
    .unwrap();
    let err = run_cli(["cargo-bam", "doctor", "--dir", mods_dir.to_str().unwrap()]).unwrap_err();
    assert!(
        err.to_string().contains("0.3.0"),
        "expected ABI compatibility error mentioning 0.3.0, got: {err}"
    );
}

#[test]
fn cli_rejects_missing_sign_key_invalid_mod_id_and_traversal_in_build_watch_pack() {
    let tmp = TempDir::new().unwrap();
    for bad_name in ["../escape", "CON", "foo:bar", "a/b"] {
        let err = run_cli([
            "cargo-bam",
            "new",
            bad_name,
            "--dir",
            tmp.path().to_str().unwrap(),
        ])
        .unwrap_err();
        assert!(
            matches!(err, CliError::InvalidArgument(_)),
            "expected InvalidArgument for {bad_name:?}, got {err:?}"
        );
    }

    let proj = tmp.path().join("c_sample");
    run_cli([
        "cargo-bam",
        "new",
        "c_sample",
        "--lang",
        "c99",
        "--dir",
        tmp.path().to_str().unwrap(),
    ])
    .unwrap();
    let missing_key = tmp.path().join("does_not_exist.key");
    let build_err = run_cli([
        "cargo-bam",
        "build",
        "--dir",
        proj.to_str().unwrap(),
        "--sign-key",
        missing_key.to_str().unwrap(),
    ])
    .unwrap_err();
    assert!(
        matches!(build_err, CliError::Io(_) | CliError::Crypto(_)),
        "expected missing key error, got {build_err:?}"
    );

    // Malicious `mod.toml` with traversal in `[mod].id` or `[mod].entry` MUST be blocked by
    // `build`, `watch`, and `pack` before writing anything outside the project / staging root!
    let evil_proj = tmp.path().join("evil_proj");
    fs::create_dir_all(evil_proj.join("src")).unwrap();
    fs::write(evil_proj.join("src/mod.c"), "int x;\n").unwrap();
    fs::write(
        evil_proj.join("mod.toml"),
        "[mod]\nid = \"../../escape_stage\"\nversion = \"1.0.0\"\nlanguage = \"c99\"\n",
    )
    .unwrap();
    let stage_root = tmp.path().join("stage_root");

    assert!(
        run_cli([
            "cargo-bam",
            "build",
            "--dir",
            evil_proj.to_str().unwrap(),
            "--out-dir",
            stage_root.to_str().unwrap(),
        ])
        .is_err()
    );
    assert!(
        watch::WatchRunner::new(watch::WatchConfig {
            dir: Some(evil_proj.clone()),
            out_dir: Some(stage_root),
            once: true,
            ..watch::WatchConfig::default()
        })
        .is_err()
    );
    assert!(
        pack::pack_mod(&pack::PackOptions {
            dir: Some(evil_proj),
            skip_build: true,
            ..pack::PackOptions::default()
        })
        .is_err()
    );
    assert!(!tmp.path().join("escape_stage").exists());
}

#[test]
fn cli_verify_supports_krl_revocation_for_loose_libs_and_bampacks() {
    let tmp = TempDir::new().unwrap();
    let ca_prefix = tmp.path().join("keys/ca");
    let signer_prefix = tmp.path().join("keys/signer");
    let ca_gen = signing::generate_keypair(&ca_prefix, Some("ca")).unwrap();
    let signer_gen = signing::generate_keypair(&signer_prefix, Some("signer")).unwrap();

    let ca_priv = PrivateKey::read_openssh_file(&ca_gen.private_key).unwrap();
    let signer_pub = PublicKey::read_openssh_file(&signer_gen.public_key).unwrap();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let cert = issue_ed25519_certificate_openssh(
        &ca_priv,
        &signer_pub,
        1234,
        "revoked-cert",
        &["krl_demo".to_owned()],
        now.saturating_sub(60),
        now + 3600,
    )
    .unwrap();
    let cert_path = tmp.path().join("keys/signer-cert.pub");
    fs::write(&cert_path, &cert.certificate_openssh).unwrap();

    // Scaffold, build, sign loose binary, and pack cert-signed .bampack
    run_cli([
        "cargo-bam",
        "new",
        "krl_demo",
        "--lang",
        "c99",
        "--dir",
        tmp.path().to_str().unwrap(),
    ])
    .unwrap();
    let proj = tmp.path().join("krl_demo");
    let pack_out = tmp.path().join("krl_demo.bampack");
    let packed = pack::pack_mod(&pack::PackOptions {
        dir: Some(proj),
        out: Some(pack_out.clone()),
        key: Some(signer_gen.private_key.clone()),
        cert: Some(cert_path),
        ..pack::PackOptions::default()
    })
    .unwrap();
    assert!(packed.signed);

    // Without KRL, verification succeeds
    run_cli([
        "cargo-bam",
        "verify",
        "--ca",
        ca_gen.public_key.to_str().unwrap(),
        "--principal",
        "krl_demo",
        "--namespace",
        DEFAULT_BAMPACK_NAMESPACE,
        pack_out.to_str().unwrap(),
    ])
    .unwrap();

    // Build KRL revoking serial 1234 & signer key
    let krl_bytes = KrlBuilder::new()
        .version(1)
        .revoke_cert_serial(Some(ca_priv.public_key()), 1234)
        .revoke_key_openssh(&signer_pub.to_openssh().unwrap())
        .unwrap()
        .to_bytes()
        .unwrap();
    let krl_path = tmp.path().join("revoked.krl");
    fs::write(&krl_path, &krl_bytes).unwrap();

    // With `--krl`, `.bampack` verification MUST fail!
    assert!(
        run_cli([
            "cargo-bam",
            "verify",
            "--ca",
            ca_gen.public_key.to_str().unwrap(),
            "--krl",
            krl_path.to_str().unwrap(),
            "--principal",
            "krl_demo",
            pack_out.to_str().unwrap(),
        ])
        .is_err()
    );

    // Loose binary signed by revoked key MUST also fail with `--krl`!
    let dummy_bin = tmp.path().join("dummy.so");
    fs::write(&dummy_bin, b"fake-binary").unwrap();
    signing::sign_mod_file(
        &signer_gen.private_key,
        &dummy_bin,
        None,
        DEFAULT_SIGNATURE_NAMESPACE,
    )
    .unwrap();
    assert!(
        run_cli([
            "cargo-bam",
            "verify",
            "--pubkey",
            signer_gen.public_key.to_str().unwrap(),
            "--krl",
            krl_path.to_str().unwrap(),
            dummy_bin.to_str().unwrap(),
        ])
        .is_err()
    );
}
