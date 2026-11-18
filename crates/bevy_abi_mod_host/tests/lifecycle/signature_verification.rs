use crate::DROP_SLOT_POOL_RWLOCK;
use crate::support::{Position, TempDylibCopy, Velocity, ensure_guest_mods_built, fixture_dylib};
use bevy_abi_mod_host::prelude::*;
use bevy_app::{App, FixedUpdate, PostUpdate, Update};
use bevy_ecs::message::Messages;
use bevy_ecs::prelude::Resource;
use bevy_ecs::system::Commands;
use ssh_key::rand_core::OsRng;
use ssh_key::{Algorithm, Certificate, HashAlg, PrivateKey, certificate};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

struct TempWorkDir(PathBuf);

impl TempWorkDir {
    fn new(tag: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("bam_sig_test_{tag}_{}_{}", std::process::id(), id));
        fs::create_dir_all(&dir).expect("create temp test dir");
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempWorkDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn gen_ed25519() -> PrivateKey {
    PrivateKey::random(&mut OsRng, Algorithm::Ed25519).expect("generate Ed25519 key")
}

fn issue_user_cert(
    ca: &PrivateKey,
    subject: &PrivateKey,
    key_id: &str,
    principals: &[&str],
    serial: u64,
) -> Certificate {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("unix epoch")
        .as_secs();
    let valid_after = now.saturating_sub(60);
    let valid_before = now.saturating_add(3_600);
    let mut builder = certificate::Builder::new_with_random_nonce(
        &mut OsRng,
        subject.public_key(),
        valid_after,
        valid_before,
    )
    .expect("cert builder");
    builder
        .cert_type(certificate::CertType::User)
        .expect("user cert");
    builder.serial(serial).expect("serial");
    builder.key_id(key_id).expect("key_id");
    if principals.is_empty() {
        builder.all_principals_valid().expect("all principals");
    } else {
        for &p in principals {
            builder.valid_principal(p).expect("principal");
        }
    }
    builder.sign(ca).expect("sign cert")
}

fn write_sig_for_binary(binary_path: &Path, signer: &PrivateKey, cert: &Certificate) {
    let bytes = fs::read(binary_path).expect("read binary for signing");
    let pem = sign_mod_bytes_with_certificate(signer, cert, DEFAULT_SIGNATURE_NAMESPACE, &bytes)
        .expect("sign binary");
    let sig_path = default_signature_path_for(binary_path);
    fs::write(&sig_path, pem.as_bytes()).expect("write .sig sidecar");
}

fn setup_host_app(policy: ModTrustPolicy) -> App {
    let mut app = App::new();
    app.add_plugins(AbiModPlugin::with_trust_policy(policy));
    app.register_mod_component::<Position>(ComponentOptions::new().with_default())
        .expect("register Position");
    app.register_mod_component::<Velocity>(ComponentOptions::new().with_default())
        .expect("register Velocity");
    app
}

#[test]
fn unsigned_mods_rejected_before_dlopen() {
    let _pool_guard = DROP_SLOT_POOL_RWLOCK
        .read()
        .unwrap_or_else(|e| e.into_inner());
    let (rust_dylib, c_dylib) = ensure_guest_mods_built();
    let rust_copy = TempDylibCopy::new(&rust_dylib, "unsigned_rust");
    let c_copy = TempDylibCopy::new(&c_dylib, "unsigned_c");

    let ca = gen_ed25519();
    let ca_pub = ca.public_key().to_openssh().expect("ca openssh");
    let policy = ModTrustPolicy::from_ca_openssh(&ca_pub).expect("policy");
    let mut app = setup_host_app(policy);

    for path in [c_copy.path(), rust_copy.path()] {
        let err = app
            .load_mod_with(path, LoadOptions::new().with_shadow_copy(false))
            .expect_err("unsigned mod must be rejected");
        assert!(matches!(
            err,
            ModLoadError::SignatureVerification {
                source: SignatureVerificationError::MissingSignatureFile { .. },
                ..
            }
        ));
    }

    assert!(app.world().resource::<ModHost>().is_empty());
    assert_eq!(app.world().resource::<Messages<ModLoaded>>().len(), 0);
}

#[test]
fn signed_rust_and_c_mods_force_shadow_stage_and_attach_verified_component() {
    let _pool_guard = DROP_SLOT_POOL_RWLOCK
        .read()
        .unwrap_or_else(|e| e.into_inner());
    let (rust_dylib, c_dylib) = ensure_guest_mods_built();
    let rust_copy = TempDylibCopy::new(&rust_dylib, "signed_rust");
    let c_copy = TempDylibCopy::new(&c_dylib, "signed_c");

    let ca = gen_ed25519();
    let signer = gen_ed25519();
    let cert = issue_user_cert(
        &ca,
        &signer,
        "studio-ci",
        &["asymmetric_rust_mod", "asymmetric_c_mod"],
        101,
    );
    write_sig_for_binary(rust_copy.path(), &signer, &cert);
    write_sig_for_binary(c_copy.path(), &signer, &cert);

    let ca_pub = ca.public_key().to_openssh().expect("ca pub");
    let policy = ModTrustPolicy::from_ca_openssh(&ca_pub).expect("policy");
    let mut app = setup_host_app(policy);

    // Even with `shadow_copy: false`, enforced verification must stage through an isolated shadow file.
    let c_id = app
        .load_mod_with(c_copy.path(), LoadOptions::new().with_shadow_copy(false))
        .expect("signed C mod loads");
    let r_id = app
        .load_mod_with(rust_copy.path(), LoadOptions::new().with_shadow_copy(false))
        .expect("signed Rust mod loads");

    let expected_signer_fp = signer.public_key().fingerprint(HashAlg::Sha256).to_string();
    let expected_ca_fp = ca.public_key().fingerprint(HashAlg::Sha256).to_string();

    {
        let host = app.world().resource::<ModHost>();
        for (id, src_path) in [(c_id, c_copy.path()), (r_id, rust_copy.path())] {
            let loaded = host.get_mod(id).expect("loaded mod");
            assert_ne!(
                loaded.loaded_path(),
                src_path,
                "enforced verification must always stage a shadow file to prevent TOCTOU"
            );
            let sig = loaded
                .verified_signature()
                .expect("VerifiedModSignature stored on LoadedMod");
            assert_eq!(sig.signer_fingerprint, expected_signer_fp);
            assert_eq!(sig.ca_fingerprint.as_deref(), Some(expected_ca_fp.as_str()));
            assert_eq!(sig.serial, Some(101));
        }

        let mut q = app
            .world_mut()
            .query::<(&LoadedModInfo, &VerifiedModSignature)>();
        let items: Vec<_> = q.iter(app.world()).collect();
        assert_eq!(items.len(), 2);
        for (_info, sig) in items {
            assert_eq!(sig.key_id.as_deref(), Some("studio-ci"));
            assert_eq!(sig.namespace, DEFAULT_SIGNATURE_NAMESPACE);
            assert_eq!(sig.serial, Some(101));
        }
    }

    let entity = app
        .world_mut()
        .spawn((
            Position {
                x: 0.0,
                y: 0.0,
                z: 0.0,
            },
            Velocity {
                x: 1.0,
                y: 2.0,
                z: 3.0,
            },
        ))
        .id();
    app.update();
    let pos = app.world().entity(entity).get::<Position>().unwrap();
    assert!(pos.x > 0.0 || pos.y > 0.0 || pos.z > 0.0);
}

#[test]
fn manifest_name_principal_binding_accepts_match_and_rolls_back_mismatch() {
    let _pool_guard = DROP_SLOT_POOL_RWLOCK
        .read()
        .unwrap_or_else(|e| e.into_inner());
    let (_rust_dylib, c_dylib) = ensure_guest_mods_built();
    let matching_copy = TempDylibCopy::new(&c_dylib, "principal_match");
    let mismatched_copy = TempDylibCopy::new(&c_dylib, "principal_mismatch");

    let ca = gen_ed25519();
    let signer = gen_ed25519();
    let match_cert = issue_user_cert(&ca, &signer, "match", &["asymmetric_c_mod"], 1);
    let mismatch_cert = issue_user_cert(&ca, &signer, "mismatch", &["some_other_mod"], 2);

    write_sig_for_binary(matching_copy.path(), &signer, &match_cert);
    write_sig_for_binary(mismatched_copy.path(), &signer, &mismatch_cert);

    let ca_pub = ca.public_key().to_openssh().expect("ca pub");
    let policy = ModTrustPolicy::from_ca_openssh(&ca_pub)
        .expect("policy")
        .with_manifest_name_principal_binding(true);

    let mut mismatch_app = setup_host_app(policy.clone());
    let err = mismatch_app
        .load_mod(mismatched_copy.path())
        .expect_err("mismatched BamManifest::name must roll back");
    assert!(matches!(
        err,
        ModLoadError::SignatureVerification {
            source: SignatureVerificationError::PrincipalMismatch { ref expected, .. },
            ..
        } if expected == "asymmetric_c_mod"
    ));
    assert!(mismatch_app.world().resource::<ModHost>().is_empty());
    assert_eq!(
        mismatch_app.world().resource::<Messages<ModLoaded>>().len(),
        0
    );

    let mut match_app = setup_host_app(policy);
    let mod_id = match_app
        .load_mod(matching_copy.path())
        .expect("matching principal should load");
    assert!(match_app.world().resource::<ModHost>().contains_mod(mod_id));
}

#[test]
fn hot_reload_preverifies_and_supports_leaf_cert_rotation() {
    let _pool_guard = DROP_SLOT_POOL_RWLOCK
        .read()
        .unwrap_or_else(|e| e.into_inner());
    let (_rust_dylib, c_dylib) = ensure_guest_mods_built();
    let live_copy = TempDylibCopy::new(&c_dylib, "hot_reload_sig");
    let sig_path = default_signature_path_for(live_copy.path());

    let ca = gen_ed25519();
    let signer_v1 = gen_ed25519();
    let signer_v2 = gen_ed25519();
    let cert_v1 = issue_user_cert(&ca, &signer_v1, "v1", &["asymmetric_c_mod"], 10);
    let cert_v2 = issue_user_cert(&ca, &signer_v2, "v2", &["asymmetric_c_mod"], 20);

    write_sig_for_binary(live_copy.path(), &signer_v1, &cert_v1);

    let ca_pub = ca.public_key().to_openssh().expect("ca pub");
    let mut app = setup_host_app(ModTrustPolicy::from_ca_openssh(&ca_pub).expect("policy"));

    let entity = app
        .world_mut()
        .spawn((
            Position {
                x: 0.0,
                y: 0.0,
                z: 0.0,
            },
            Velocity {
                x: 1.0,
                y: 0.0,
                z: 0.0,
            },
        ))
        .id();

    let id_v1 = app.load_mod(live_copy.path()).expect("initial signed load");
    app.update();
    let z_after_1 = app.world().entity(entity).get::<Position>().unwrap().z;
    assert!(z_after_1 > 0.0);

    // 1. Delete `.sig` -> reload must fail BEFORE unloading `id_v1`.
    let saved_sig = fs::read(&sig_path).expect("backup sig");
    fs::remove_file(&sig_path).expect("remove sig");
    let missing_err = app
        .reload_mod(id_v1)
        .expect_err("reload without .sig must fail");
    assert!(matches!(
        missing_err,
        ModLoadError::SignatureVerification {
            source: SignatureVerificationError::MissingSignatureFile { .. },
            ..
        }
    ));
    assert!(
        app.world().resource::<ModHost>().contains_mod(id_v1),
        "original verified mod must stay loaded after failed reload"
    );

    // 2. Restore `.sig` and tamper binary -> reload must fail with InvalidSignature and keep `id_v1`.
    fs::write(&sig_path, &saved_sig).expect("restore sig");
    let orig_bytes = fs::read(live_copy.path()).expect("read orig bytes");
    let mut tampered = orig_bytes.clone();
    tampered[0] ^= 0x55;
    fs::write(live_copy.path(), &tampered).expect("write tampered binary");
    let invalid_err = app
        .reload_mod(id_v1)
        .expect_err("reload with tampered binary must fail");
    assert!(matches!(
        invalid_err,
        ModLoadError::SignatureVerification {
            source: SignatureVerificationError::InvalidSignature,
            ..
        }
    ));
    assert!(app.world().resource::<ModHost>().contains_mod(id_v1));
    app.update();
    let z_after_2 = app.world().entity(entity).get::<Position>().unwrap().z;
    assert!(
        z_after_2 > z_after_1,
        "original mod must keep executing after failed reload"
    );

    // 3. Restore binary and sign with rotated leaf key + cert_v2 under the same Root CA.
    fs::write(live_copy.path(), &orig_bytes).expect("restore clean binary");
    write_sig_for_binary(live_copy.path(), &signer_v2, &cert_v2);

    let id_v2 = app.reload_mod(id_v1).expect("rotated cert reload succeeds");
    assert_ne!(id_v1, id_v2);
    assert!(!app.world().resource::<ModHost>().contains_mod(id_v1));

    let expected_v2_fp = signer_v2
        .public_key()
        .fingerprint(HashAlg::Sha256)
        .to_string();
    let mut q = app
        .world_mut()
        .query::<(&LoadedModInfo, &VerifiedModSignature)>();
    let items: Vec<_> = q.iter(app.world()).collect();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].0.id, id_v2);
    assert_eq!(items[0].1.serial, Some(20));
    assert_eq!(items[0].1.signer_fingerprint, expected_v2_fp);
}

#[test]
fn deferred_commands_load_mod_enforces_trust_policy() {
    let _pool_guard = DROP_SLOT_POOL_RWLOCK
        .read()
        .unwrap_or_else(|e| e.into_inner());
    let (_rust_dylib, c_dylib) = ensure_guest_mods_built();
    let unsigned = TempDylibCopy::new(&c_dylib, "cmd_unsigned");
    let signed = TempDylibCopy::new(&c_dylib, "cmd_signed");

    let ca = gen_ed25519();
    let signer = gen_ed25519();
    let cert = issue_user_cert(&ca, &signer, "cmd", &["asymmetric_c_mod"], 55);
    write_sig_for_binary(signed.path(), &signer, &cert);

    let ca_pub = ca.public_key().to_openssh().expect("ca pub");
    let mut app = setup_host_app(ModTrustPolicy::from_ca_openssh(&ca_pub).expect("policy"));

    let unsigned_path = unsigned.path().to_path_buf();
    let signed_path = signed.path().to_path_buf();
    app.add_systems(
        Update,
        move |mut commands: Commands, mut ran: bevy_ecs::prelude::Local<bool>| {
            if !*ran {
                *ran = true;
                commands.load_mod(&unsigned_path);
                commands.load_mod(&signed_path);
            }
        },
    );

    app.update();

    let host = app.world().resource::<ModHost>();
    assert_eq!(host.len(), 1);
    assert_eq!(host.loaded_mods()[0].source_path(), signed.path());
    assert_eq!(
        host.loaded_mods()[0]
            .verified_signature()
            .and_then(|s| s.serial),
        Some(55)
    );
}

#[test]
fn openssh_ssh_keygen_bidirectional_interop() {
    let Ok(check) = Command::new("ssh-keygen").arg("-V").output() else {
        eprintln!("skipping ssh-keygen interop test: `ssh-keygen` not found on PATH");
        return;
    };
    let _ = check;

    let _pool_guard = DROP_SLOT_POOL_RWLOCK
        .read()
        .unwrap_or_else(|e| e.into_inner());
    let (_rust_dylib, c_dylib) = ensure_guest_mods_built();
    let c_copy = TempDylibCopy::new(&c_dylib, "ssh_keygen_interop");
    let work = TempWorkDir::new("ssh_keygen_interop");

    let ca_priv = work.path().join("ca");
    let ca_pub = work.path().join("ca.pub");
    let signer_priv = work.path().join("signer");
    let signer_pub = work.path().join("signer.pub");
    let signer_cert = work.path().join("signer-cert.pub");

    // 1. `ssh-keygen` creates CA + signer + user cert (without `-V`, so valid_before == u64::MAX)
    //    and signs the real C99 mod library via `ssh-keygen -Y sign`.
    let out = Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-f"])
        .arg(&ca_priv)
        .output()
        .expect("ssh-keygen ca");
    assert!(out.status.success(), "ssh-keygen ca failed: {out:?}");

    let out = Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-f"])
        .arg(&signer_priv)
        .output()
        .expect("ssh-keygen signer");
    assert!(out.status.success(), "ssh-keygen signer failed: {out:?}");

    let out = Command::new("ssh-keygen")
        .args(["-q", "-s"])
        .arg(&ca_priv)
        .args(["-I", "openssh-cli-signer", "-n", "asymmetric_c_mod"])
        .arg(&signer_pub)
        .output()
        .expect("ssh-keygen cert");
    assert!(out.status.success(), "ssh-keygen cert failed: {out:?}");

    let out = Command::new("ssh-keygen")
        .args(["-q", "-Y", "sign", "-f"])
        .arg(&signer_cert)
        .args(["-n", DEFAULT_SIGNATURE_NAMESPACE])
        .arg(c_copy.path())
        .output()
        .expect("ssh-keygen -Y sign");
    assert!(
        out.status.success(),
        "ssh-keygen -Y sign failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let policy = ModTrustPolicy::disabled()
        .add_ca_file(&ca_pub)
        .expect("load ssh-keygen ca.pub")
        .with_manifest_name_principal_binding(true);
    let mut app = setup_host_app(policy);
    let mod_id = app
        .load_mod(c_copy.path())
        .expect("host must verify and load ssh-keygen signed mod with u64::MAX forever cert");
    {
        let loaded = app.world().resource::<ModHost>().get_mod(mod_id).unwrap();
        let sig = loaded.verified_signature().unwrap();
        assert_eq!(sig.key_id.as_deref(), Some("openssh-cli-signer"));
        assert_eq!(sig.principals, vec!["asymmetric_c_mod"]);
    }
    app.update();

    // 2. Pure-Rust signing (`sign_mod_bytes_with_openssh_certificate`) -> `ssh-keygen -Y verify`.
    let rust_ca = gen_ed25519();
    let rust_signer = gen_ed25519();
    let rust_cert = issue_user_cert(&rust_ca, &rust_signer, "rust-ci", &["asymmetric_c_mod"], 77);
    let rust_cert_str = rust_cert.to_openssh().expect("cert openssh");
    let c_bytes = fs::read(c_copy.path()).expect("read c mod bytes");
    let rust_sig_pem = sign_mod_bytes_with_openssh_certificate(
        &rust_signer,
        &rust_cert_str,
        DEFAULT_SIGNATURE_NAMESPACE,
        &c_bytes,
    )
    .expect("rust sign with cert");
    let rust_sig_path = work.path().join("rust.sig");
    fs::write(&rust_sig_path, rust_sig_pem.as_bytes()).expect("write rust sig");

    let allowed_signers = work.path().join("allowed_signers");
    let rust_ca_pub = rust_ca.public_key().to_openssh().expect("rust ca pub");
    fs::write(
        &allowed_signers,
        format!(
            "asymmetric_c_mod cert-authority,namespaces=\"{DEFAULT_SIGNATURE_NAMESPACE}\" {rust_ca_pub}\n"
        ),
    )
    .expect("write allowed_signers");

    let src_file = fs::File::open(c_copy.path()).expect("open c_copy for stdin");
    let verify_out = Command::new("ssh-keygen")
        .args(["-Y", "verify", "-f"])
        .arg(&allowed_signers)
        .args([
            "-I",
            "asymmetric_c_mod",
            "-n",
            DEFAULT_SIGNATURE_NAMESPACE,
            "-s",
        ])
        .arg(&rust_sig_path)
        .stdin(src_file)
        .output()
        .expect("ssh-keygen -Y verify");
    assert!(
        verify_out.status.success(),
        "ssh-keygen -Y verify failed: stdout={}, stderr={}",
        String::from_utf8_lossy(&verify_out.stdout),
        String::from_utf8_lossy(&verify_out.stderr)
    );
}

#[repr(C)]
#[derive(Resource, Clone, Copy, Debug, Default, PartialEq, Eq, ModComponent)]
#[mod_component(name = "sched_reg::OrderTraceRes")]
struct OrderTraceRes {
    steps: [u32; 4],
    len: u32,
}

#[repr(C)]
#[derive(Resource, Clone, Copy, Debug, Default, PartialEq, Eq, ModComponent)]
#[mod_component(name = "ext::ReloadStateProbe")]
struct ReloadStateProbe {
    reload_epoch: u64,
    accumulated_ticks: u64,
    fail_next_save: u32,
    fail_next_restore: u32,
}

fn find_xtask_cmd() -> (PathBuf, bool) {
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root");
    let direct_bin = workspace_root
        .join("target/debug")
        .join(format!("xtask{}", std::env::consts::EXE_SUFFIX));
    if direct_bin.is_file() {
        (direct_bin, false)
    } else {
        let cargo = PathBuf::from(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
        (cargo, true)
    }
}

fn run_xtask_sign(args: &[&str]) -> std::process::Output {
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let (exe, use_cargo) = find_xtask_cmd();
    let mut cmd = Command::new(&exe);
    cmd.current_dir(&workspace_root);
    if use_cargo {
        cmd.args(["run", "--quiet", "-p", "xtask", "--"]);
    }
    cmd.arg("sign");
    cmd.args(args);
    cmd.output().expect("execute xtask sign")
}

#[test]
fn multi_world_ca_isolation() {
    let _pool_guard = DROP_SLOT_POOL_RWLOCK
        .read()
        .unwrap_or_else(|e| e.into_inner());
    let (_rust_dylib, c_dylib) = ensure_guest_mods_built();
    let copy_a = TempDylibCopy::new(&c_dylib, "multi_world_ca_a");
    let copy_b = TempDylibCopy::new(&c_dylib, "multi_world_ca_b");

    let ca_a = gen_ed25519();
    let ca_b = gen_ed25519();
    let signer_a = gen_ed25519();
    let signer_b = gen_ed25519();
    let cert_a = issue_user_cert(&ca_a, &signer_a, "ca-a-user", &["asymmetric_c_mod"], 1);
    let cert_b = issue_user_cert(&ca_b, &signer_b, "ca-b-user", &["asymmetric_c_mod"], 1);

    write_sig_for_binary(copy_a.path(), &signer_a, &cert_a);
    write_sig_for_binary(copy_b.path(), &signer_b, &cert_b);

    let policy_a =
        ModTrustPolicy::from_ca_openssh(&ca_a.public_key().to_openssh().unwrap()).unwrap();
    let policy_b =
        ModTrustPolicy::from_ca_openssh(&ca_b.public_key().to_openssh().unwrap()).unwrap();

    let mut app_a = setup_host_app(policy_a);
    let mut app_b = setup_host_app(policy_b);

    // Cross-CA rejected in both Worlds
    assert!(matches!(
        app_a.load_mod(copy_b.path()),
        Err(ModLoadError::SignatureVerification {
            source: SignatureVerificationError::UntrustedCertificateAuthority { .. },
            ..
        })
    ));
    assert!(matches!(
        app_b.load_mod(copy_a.path()),
        Err(ModLoadError::SignatureVerification {
            source: SignatureVerificationError::UntrustedCertificateAuthority { .. },
            ..
        })
    ));

    // Matching CA loads and runs independently in each World
    let id_a = app_a.load_mod(copy_a.path()).expect("app_a loads CA_A mod");
    let id_b = app_b.load_mod(copy_b.path()).expect("app_b loads CA_B mod");
    app_a.update();
    app_b.update();
    assert_eq!(
        app_a
            .world()
            .resource::<ModHost>()
            .get_mod(id_a)
            .unwrap()
            .name(),
        "asymmetric_c_mod"
    );
    assert_eq!(
        app_b
            .world()
            .resource::<ModHost>()
            .get_mod(id_b)
            .unwrap()
            .name(),
        "asymmetric_c_mod"
    );
}

#[test]
fn runtime_evict_revoked_mods_unloads_revoked_and_preserves_siblings() {
    let _pool_guard = DROP_SLOT_POOL_RWLOCK
        .read()
        .unwrap_or_else(|e| e.into_inner());
    let _ = ensure_guest_mods_built();
    let alpha_copy = TempDylibCopy::new(&fixture_dylib("mod_alpha"), "evict_alpha");
    let beta_copy = TempDylibCopy::new(&fixture_dylib("mod_beta"), "evict_beta");

    let ca = gen_ed25519();
    let signer_alpha = gen_ed25519();
    let signer_beta = gen_ed25519();
    let cert_alpha = issue_user_cert(&ca, &signer_alpha, "alpha-kid", &["mod_alpha"], 10);
    let cert_beta = issue_user_cert(&ca, &signer_beta, "beta-kid", &["mod_beta"], 20);
    write_sig_for_binary(alpha_copy.path(), &signer_alpha, &cert_alpha);
    write_sig_for_binary(beta_copy.path(), &signer_beta, &cert_beta);

    let policy = ModTrustPolicy::from_ca_openssh(&ca.public_key().to_openssh().unwrap())
        .unwrap()
        .with_manifest_name_principal_binding(true);

    let mut app = App::new();
    app.add_plugins(AbiModPlugin::with_trust_policy(policy.clone()));
    app.insert_resource(OrderTraceRes::default());
    app.register_mod_resource::<OrderTraceRes>().unwrap();

    let alpha_id = app.load_mod(alpha_copy.path()).expect("load mod_alpha");
    let beta_id = app.load_mod(beta_copy.path()).expect("load mod_beta");
    app.update();
    assert_eq!(app.world().resource::<OrderTraceRes>().len, 4);

    // Apply signed KRL revoking CA serial 10 (mod_alpha) while keeping serial 20 (mod_beta) valid
    let krl_v1 = KrlBuilder::new()
        .version(1)
        .revoke_cert_serial(Some(ca.public_key()), 10)
        .to_bytes()
        .unwrap();
    let krl_v1_sig =
        sign_mod_bytes_with_key(&ca, DEFAULT_KRL_SIGNATURE_NAMESPACE, &krl_v1).unwrap();
    let updated_policy = policy
        .add_signed_krl_bytes(&krl_v1, krl_v1_sig.as_bytes())
        .unwrap();
    app.set_mod_trust_policy(updated_policy.clone());

    let evicted = app.evict_revoked_mods();
    assert_eq!(evicted, vec![alpha_id]);
    assert!(
        app.world()
            .resource::<ModHost>()
            .get_mod(alpha_id)
            .is_none()
    );
    assert!(app.world().resource::<ModHost>().get_mod(beta_id).is_some());

    // LoadedModInfo entities reflect only mod_beta remaining, ModUnloaded emitted for mod_alpha
    {
        let w = app.world_mut();
        let mut q = w.query::<&LoadedModInfo>();
        let names: Vec<String> = q.iter(w).map(|i| i.name.clone()).collect();
        assert_eq!(names, vec!["mod_beta".to_string()]);
        let msgs = w.resource::<Messages<ModUnloaded>>();
        let mut cur = msgs.get_cursor();
        assert!(
            cur.read(msgs)
                .any(|m| m.id == alpha_id && m.name == "mod_alpha")
        );
    }

    // Sibling mod_beta continues ticking unaffected!
    *app.world_mut().resource_mut::<OrderTraceRes>() = OrderTraceRes::default();
    app.update();
    let trace = *app.world().resource::<OrderTraceRes>();
    assert_eq!(trace.len, 2);
    assert_eq!(&trace.steps[..2], &[20, 21]);

    // Now revoke beta by certificate key_id via KRL v2 and evict beta as well
    let krl_v2 = KrlBuilder::new()
        .version(2)
        .revoke_cert_key_id(Some(ca.public_key()), "beta-kid")
        .to_bytes()
        .unwrap();
    let krl_v2_sig =
        sign_mod_bytes_with_key(&ca, DEFAULT_KRL_SIGNATURE_NAMESPACE, &krl_v2).unwrap();
    let policy_v2 = updated_policy
        .add_signed_krl_bytes(&krl_v2, krl_v2_sig.as_bytes())
        .unwrap();
    app.set_mod_trust_policy(policy_v2);
    let evicted_2 = app.evict_revoked_mods();
    assert_eq!(evicted_2, vec![beta_id]);
    assert!(app.world().resource::<ModHost>().loaded_mods().is_empty());

    *app.world_mut().resource_mut::<OrderTraceRes>() = OrderTraceRes::default();
    app.update();
    assert_eq!(app.world().resource::<OrderTraceRes>().len, 0);
}

#[test]
fn hot_reload_manifest_principal_precheck_preserves_live_mod() {
    let _pool_guard = DROP_SLOT_POOL_RWLOCK
        .read()
        .unwrap_or_else(|e| e.into_inner());
    let _ = ensure_guest_mods_built();
    let alpha_copy = TempDylibCopy::new(&fixture_dylib("mod_alpha"), "reload_precheck_alpha");
    let beta_copy = TempDylibCopy::new(&fixture_dylib("mod_beta"), "reload_precheck_beta");

    let ca = gen_ed25519();
    let signer = gen_ed25519();
    let cert_alpha = issue_user_cert(&ca, &signer, "alpha-cert", &["mod_alpha"], 1);
    let cert_wrong_principal = issue_user_cert(&ca, &signer, "beta-cert", &["mod_beta"], 2);
    write_sig_for_binary(alpha_copy.path(), &signer, &cert_alpha);
    write_sig_for_binary(beta_copy.path(), &signer, &cert_wrong_principal);

    let policy = ModTrustPolicy::from_ca_openssh(&ca.public_key().to_openssh().unwrap())
        .unwrap()
        .with_manifest_name_principal_binding(true);

    let mut app = App::new();
    app.add_plugins(AbiModPlugin::with_trust_policy(policy));
    app.insert_resource(OrderTraceRes::default());
    app.register_mod_resource::<OrderTraceRes>().unwrap();

    let alpha_id = app.load_mod(alpha_copy.path()).expect("load mod_alpha");

    // 1. reload_mod_from with replacement carrying principal "mod_beta" fails BEFORE unloading mod_alpha
    let err_from = app
        .reload_mod_from(alpha_id, beta_copy.path(), LoadOptions::default())
        .expect_err("principal mismatch on reload_mod_from must fail");
    assert!(matches!(
        err_from,
        ModLoadError::SignatureVerification {
            source: SignatureVerificationError::PrincipalMismatch { .. },
            ..
        }
    ));
    assert!(
        app.world()
            .resource::<ModHost>()
            .get_mod(alpha_id)
            .is_some()
    );

    // 2. Replace sidecar on alpha_copy with cert_wrong_principal -> reload_mod_with fails non-destructively
    write_sig_for_binary(alpha_copy.path(), &signer, &cert_wrong_principal);
    let err_with = app
        .reload_mod_with(alpha_id, LoadOptions::default())
        .expect_err("principal mismatch on reload_mod_with must fail");
    assert!(matches!(
        err_with,
        ModLoadError::SignatureVerification {
            source: SignatureVerificationError::PrincipalMismatch { .. },
            ..
        }
    ));
    assert!(
        app.world()
            .resource::<ModHost>()
            .get_mod(alpha_id)
            .is_some()
    );

    // Live mod_alpha continues running cleanly!
    app.update();
    let trace = *app.world().resource::<OrderTraceRes>();
    assert_eq!(trace.len, 2);
    assert_eq!(&trace.steps[..2], &[10, 11]);
}

#[test]
fn stateful_hot_reload_with_cert_rotation_and_signed_krl_anti_rollback() {
    let _pool_guard = DROP_SLOT_POOL_RWLOCK
        .read()
        .unwrap_or_else(|e| e.into_inner());
    let _ = ensure_guest_mods_built();
    let mod_copy = TempDylibCopy::new(
        &fixture_dylib("state_hooks_and_observers_mod"),
        "stateful_krl_reload",
    );

    let ca = gen_ed25519();
    let signer_v1 = gen_ed25519();
    let signer_v2 = gen_ed25519();
    let cert_v1 = issue_user_cert(
        &ca,
        &signer_v1,
        "state-v1",
        &["state_hooks_and_observers_mod"],
        1,
    );
    let cert_v2 = issue_user_cert(
        &ca,
        &signer_v2,
        "state-v2",
        &["state_hooks_and_observers_mod"],
        2,
    );
    write_sig_for_binary(mod_copy.path(), &signer_v1, &cert_v1);

    let policy_v0 = ModTrustPolicy::from_ca_openssh(&ca.public_key().to_openssh().unwrap())
        .unwrap()
        .with_manifest_name_principal_binding(true);

    let mut app = App::new();
    app.add_plugins(AbiModPlugin::with_trust_policy(policy_v0.clone()));
    app.init_schedule(FixedUpdate);
    app.init_schedule(PostUpdate);
    app.insert_resource(ReloadStateProbe::default());
    app.register_mod_resource::<ReloadStateProbe>().unwrap();

    let id_v1 = app
        .load_mod_with(
            mod_copy.path(),
            LoadOptions::new().with_shadow_copy(true).strict(),
        )
        .expect("initial signed state mod load");

    for _ in 0..3 {
        app.update();
    }
    assert_eq!(
        *app.world().resource::<ReloadStateProbe>(),
        ReloadStateProbe {
            reload_epoch: 0,
            accumulated_ticks: 3,
            fail_next_save: 0,
            fail_next_restore: 0,
        }
    );

    // Apply signed KRL v2 revoking serial 1
    let krl_v2 = KrlBuilder::new()
        .version(2)
        .revoke_cert_serial(Some(ca.public_key()), 1)
        .to_bytes()
        .unwrap();
    let sig_krl_v2 =
        sign_mod_bytes_with_key(&ca, DEFAULT_KRL_SIGNATURE_NAMESPACE, &krl_v2).unwrap();
    let policy_v2 = policy_v0
        .add_signed_krl_bytes(&krl_v2, sig_krl_v2.as_bytes())
        .unwrap();
    app.set_mod_trust_policy(policy_v2.clone());

    // Reloading against revoked cert_v1 fails non-destructively while live instance keeps ticking!
    let err_revoked = app
        .reload_mod_with(id_v1, LoadOptions::new().with_shadow_copy(true).strict())
        .expect_err("revoked cert_v1 reload must fail");
    assert!(matches!(
        err_revoked,
        ModLoadError::SignatureVerification {
            source: SignatureVerificationError::CertificateSerialRevoked { serial: 1 },
            ..
        }
    ));
    app.update();
    assert_eq!(
        app.world().resource::<ReloadStateProbe>().accumulated_ticks,
        4
    );

    // Anti-rollback: even validly signed KRL v1 cannot downgrade policy_v2
    let krl_v1 = KrlBuilder::new().version(1).to_bytes().unwrap();
    let sig_krl_v1 =
        sign_mod_bytes_with_key(&ca, DEFAULT_KRL_SIGNATURE_NAMESPACE, &krl_v1).unwrap();
    assert!(matches!(
        policy_v2.add_signed_krl_bytes(&krl_v1, sig_krl_v1.as_bytes()),
        Err(ModTrustConfigError::KrlVersionRollback {
            offered: 1,
            min_required: 2,
        })
    ));

    // Rotate disk signature to cert_v2 and hot-reload -> state preserved across rotation!
    write_sig_for_binary(mod_copy.path(), &signer_v2, &cert_v2);
    let id_v2 = app
        .reload_mod_with(id_v1, LoadOptions::new().with_shadow_copy(true).strict())
        .expect("rotated cert_v2 hot-reload succeeds");
    assert_ne!(id_v2, id_v1);
    app.update();
    assert_eq!(
        *app.world().resource::<ReloadStateProbe>(),
        ReloadStateProbe {
            reload_epoch: 1,
            accumulated_ticks: 5,
            fail_next_save: 0,
            fail_next_restore: 0,
        }
    );

    let sig_component = app
        .world()
        .resource::<ModHost>()
        .get_mod(id_v2)
        .unwrap()
        .verified_signature()
        .unwrap();
    assert_eq!(sig_component.serial, Some(2));
    assert_eq!(sig_component.key_id.as_deref(), Some("state-v2"));
}

#[test]
fn ssh_keygen_krl_bidirectional_interop() {
    let _pool_guard = DROP_SLOT_POOL_RWLOCK
        .read()
        .unwrap_or_else(|e| e.into_inner());
    let Ok(version_out) = Command::new("ssh-keygen").arg("-V").output() else {
        return;
    };
    let _ = version_out;

    let work = TempWorkDir::new("krl_interop");
    let ca = gen_ed25519();
    let other_ca = gen_ed25519();
    let signer = gen_ed25519();
    let explicit_key = gen_ed25519();
    let hash_key = gen_ed25519();
    let clean_key = gen_ed25519();

    let ca_pub_path = work.path().join("ca.pub");
    fs::write(
        &ca_pub_path,
        format!("{}\n", ca.public_key().to_openssh().unwrap()),
    )
    .unwrap();

    // Part 1: `ssh-keygen -k` -> `ModTrustPolicy::add_krl_file`
    let spec_path = work.path().join("revocations.txt");
    let cli_krl_path = work.path().join("cli.krl");
    let cli_spec = format!(
        "serial: 11\nserial: 100-110\nid: cli-revoked-id\nhash: {}\nkey: {}\n",
        hash_key.public_key().fingerprint(HashAlg::Sha256),
        explicit_key.public_key().to_openssh().unwrap(),
    );
    fs::write(&spec_path, &cli_spec).unwrap();

    let gen_out = Command::new("ssh-keygen")
        .args(["-k", "-f"])
        .arg(&cli_krl_path)
        .args(["-s"])
        .arg(&ca_pub_path)
        .args(["-z", "7"])
        .arg(&spec_path)
        .output()
        .expect("ssh-keygen -k");
    assert!(
        gen_out.status.success(),
        "ssh-keygen -k failed: {}",
        String::from_utf8_lossy(&gen_out.stderr)
    );

    let policy = ModTrustPolicy::from_ca_openssh(&ca.public_key().to_openssh().unwrap())
        .unwrap()
        .add_public_key_openssh(&clean_key.public_key().to_openssh().unwrap())
        .unwrap()
        .add_public_key_openssh(&hash_key.public_key().to_openssh().unwrap())
        .unwrap()
        .add_public_key_openssh(&explicit_key.public_key().to_openssh().unwrap())
        .unwrap()
        .with_validity_policy(CertificateValidityPolicy::IgnoreTimestamps)
        .add_krl_file(&cli_krl_path)
        .expect("parse ssh-keygen generated KRL");
    assert_eq!(policy.applied_krl_version(), Some(7));

    let payload = b"interop-mod-bytes";
    let assert_cert_rejected = |serial: u64, kid: &str| {
        let c = issue_user_cert(&ca, &signer, kid, &["mod"], serial);
        let sig =
            sign_mod_bytes_with_certificate(&signer, &c, DEFAULT_SIGNATURE_NAMESPACE, payload)
                .unwrap();
        assert!(
            policy.verify_bytes(payload, sig.as_bytes()).is_err(),
            "expected cert serial={serial} kid={kid} to be rejected by cli.krl"
        );
    };
    assert_cert_rejected(11, "ok");
    assert_cert_rejected(105, "ok");
    assert_cert_rejected(999, "cli-revoked-id");

    let c_ok = issue_user_cert(&ca, &signer, "ok", &["mod"], 12);
    let sig_ok =
        sign_mod_bytes_with_certificate(&signer, &c_ok, DEFAULT_SIGNATURE_NAMESPACE, payload)
            .unwrap();
    assert!(policy.verify_bytes(payload, sig_ok.as_bytes()).is_ok());

    let sig_hash =
        sign_mod_bytes_with_key(&hash_key, DEFAULT_SIGNATURE_NAMESPACE, payload).unwrap();
    assert!(matches!(
        policy.verify_bytes(payload, sig_hash.as_bytes()),
        Err(SignatureVerificationError::SignerRevoked { .. })
    ));
    let sig_expl =
        sign_mod_bytes_with_key(&explicit_key, DEFAULT_SIGNATURE_NAMESPACE, payload).unwrap();
    assert!(matches!(
        policy.verify_bytes(payload, sig_expl.as_bytes()),
        Err(SignatureVerificationError::SignerRevoked { .. })
    ));
    let sig_clean =
        sign_mod_bytes_with_key(&clean_key, DEFAULT_SIGNATURE_NAMESPACE, payload).unwrap();
    assert!(policy.verify_bytes(payload, sig_clean.as_bytes()).is_ok());

    // Part 2: Pure-Rust `KrlBuilder` -> `ssh-keygen -Q` verification
    let rust_krl_path = work.path().join("rust.krl");
    let rust_krl_bytes = KrlBuilder::new()
        .version(9)
        .revoke_cert_serial(Some(ca.public_key()), 12)
        .revoke_cert_serial_range(Some(ca.public_key()), 50, 60)
        .unwrap()
        .revoke_cert_serial_bitmap(Some(ca.public_key()), 100, &[100, 107, 115])
        .unwrap()
        .revoke_cert_key_id(Some(ca.public_key()), "revoked-by-rust")
        .revoke_key_openssh(&explicit_key.public_key().to_openssh().unwrap())
        .unwrap()
        .revoke_fingerprint(
            &hash_key
                .public_key()
                .fingerprint(HashAlg::Sha256)
                .to_string(),
        )
        .unwrap()
        .to_bytes()
        .unwrap();
    fs::write(&rust_krl_path, &rust_krl_bytes).unwrap();

    let query_target = |filename: &str, contents: String, expected_revoked: bool| {
        let p = work.path().join(filename);
        fs::write(&p, format!("{contents}\n")).unwrap();
        let out = Command::new("ssh-keygen")
            .args(["-Q", "-f"])
            .arg(&rust_krl_path)
            .arg(&p)
            .output()
            .expect("ssh-keygen -Q");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        if expected_revoked {
            assert!(
                !out.status.success() && stdout.contains("REVOKED"),
                "expected {filename} to be REVOKED by ssh-keygen -Q, got status={:?}, stdout={stdout}, stderr={stderr}",
                out.status.code()
            );
        } else {
            assert!(
                out.status.success(),
                "expected {filename} to pass ssh-keygen -Q, got status={:?}, stdout={stdout}, stderr={stderr}",
                out.status.code()
            );
        }
    };

    query_target(
        "c_s12.pub",
        issue_user_cert(&ca, &signer, "ok", &["m"], 12)
            .to_openssh()
            .unwrap(),
        true,
    );
    query_target(
        "c_s55.pub",
        issue_user_cert(&ca, &signer, "ok", &["m"], 55)
            .to_openssh()
            .unwrap(),
        true,
    );
    query_target(
        "c_bm107.pub",
        issue_user_cert(&ca, &signer, "ok", &["m"], 107)
            .to_openssh()
            .unwrap(),
        true,
    );
    query_target(
        "c_bm101_ok.pub",
        issue_user_cert(&ca, &signer, "ok", &["m"], 101)
            .to_openssh()
            .unwrap(),
        false,
    );
    query_target(
        "c_kid.pub",
        issue_user_cert(&ca, &signer, "revoked-by-rust", &["m"], 888)
            .to_openssh()
            .unwrap(),
        true,
    );
    query_target(
        "c_other_ca_s12.pub",
        issue_user_cert(&other_ca, &signer, "ok", &["m"], 12)
            .to_openssh()
            .unwrap(),
        false,
    );
    query_target(
        "k_expl.pub",
        explicit_key.public_key().to_openssh().unwrap(),
        true,
    );
    query_target(
        "k_hash.pub",
        hash_key.public_key().to_openssh().unwrap(),
        true,
    );
    query_target(
        "k_clean.pub",
        clean_key.public_key().to_openssh().unwrap(),
        false,
    );
}

#[test]
fn xtask_sign_cli_end_to_end_with_krl_workflow() {
    let _pool_guard = DROP_SLOT_POOL_RWLOCK
        .read()
        .unwrap_or_else(|e| e.into_inner());
    let (_rust_dylib, c_dylib) = ensure_guest_mods_built();
    let mod_copy = TempDylibCopy::new(&c_dylib, "xtask_cli_mod");
    let work = TempWorkDir::new("xtask_sign");

    let ca_prefix = work.path().join("ca");
    let signer_prefix = work.path().join("signer");
    let ca_pub = work.path().join("ca.pub");
    let signer_pub = work.path().join("signer.pub");
    let cert_path = work.path().join("signer-cert.pub");
    let krl_path = work.path().join("revocations.krl");

    let assert_ok = |label: &str, out: std::process::Output| {
        assert!(
            out.status.success(),
            "{label} failed: stdout={}, stderr={}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    };

    assert_ok(
        "keygen ca",
        run_xtask_sign(&[
            "keygen",
            "--out",
            ca_prefix.to_str().unwrap(),
            "--comment",
            "ci-ca",
        ]),
    );
    assert_ok(
        "keygen signer",
        run_xtask_sign(&["keygen", "--out", signer_prefix.to_str().unwrap()]),
    );
    assert_ok(
        "cert",
        run_xtask_sign(&[
            "cert",
            "--ca",
            ca_prefix.to_str().unwrap(),
            "--pubkey",
            signer_pub.to_str().unwrap(),
            "--id",
            "xtask-ci-signer",
            "--principal",
            "asymmetric_c_mod",
            "--serial",
            "77",
            "--out",
            cert_path.to_str().unwrap(),
        ]),
    );
    assert_ok(
        "file",
        run_xtask_sign(&[
            "file",
            "--key",
            signer_prefix.to_str().unwrap(),
            "--cert",
            cert_path.to_str().unwrap(),
            mod_copy.path().to_str().unwrap(),
        ]),
    );
    assert_ok(
        "verify ok",
        run_xtask_sign(&[
            "verify",
            "--ca",
            ca_pub.to_str().unwrap(),
            "--principal",
            "asymmetric_c_mod",
            mod_copy.path().to_str().unwrap(),
        ]),
    );
    assert_ok(
        "krl",
        run_xtask_sign(&[
            "krl",
            "--out",
            krl_path.to_str().unwrap(),
            "--version",
            "3",
            "--ca",
            ca_pub.to_str().unwrap(),
            "--revoke-serial",
            "77",
            "--sign-key",
            ca_prefix.to_str().unwrap(),
        ]),
    );

    let revoked_verify = run_xtask_sign(&[
        "verify",
        "--ca",
        ca_pub.to_str().unwrap(),
        "--krl",
        krl_path.to_str().unwrap(),
        mod_copy.path().to_str().unwrap(),
    ]);
    assert!(
        !revoked_verify.status.success(),
        "xtask verify --krl must fail on revoked serial 77"
    );

    // Host runtime verifies signed KRL and rejects the revoked mod cleanly!
    let host_policy = ModTrustPolicy::disabled()
        .add_ca_file(&ca_pub)
        .unwrap()
        .with_manifest_name_principal_binding(true)
        .add_signed_krl_file(&krl_path, None)
        .expect("load signed KRL produced by xtask sign krl");
    assert_eq!(host_policy.applied_krl_version(), Some(3));
    let mut app = setup_host_app(host_policy);
    assert!(matches!(
        app.load_mod(mod_copy.path()),
        Err(ModLoadError::SignatureVerification {
            source: SignatureVerificationError::CertificateSerialRevoked { serial: 77 },
            ..
        })
    ));
}
