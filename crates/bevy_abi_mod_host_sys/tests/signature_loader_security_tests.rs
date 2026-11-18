#![cfg(feature = "signature-verification")]
#![forbid(unsafe_code)]

//! Level 3 OS, filesystem, permission, and shadow-staging security tests for verified mod
//! loading inside `bevy_abi_mod_host_sys`.

use bevy_abi_mod_host_sys::{
    CertificateValidityPolicy, DEFAULT_SIGNATURE_NAMESPACE, LoadOptions,
    MAX_SIGNATURE_SIDECAR_BYTES, ModHost, ModLoadError, ModTrustPolicy, SignatureVerificationError,
    default_signature_path_for, load_mod_in_world, sign_mod_bytes_with_certificate,
    sign_mod_bytes_with_key, unload_all_mods_in_world,
};
use bevy_ecs::world::World;
use core::sync::atomic::{AtomicU64, Ordering};
use ssh_key::certificate;
use ssh_key::rand_core::OsRng;
use ssh_key::{Algorithm, PrivateKey};
use std::path::{Path, PathBuf};

static NEXT_DIR_ID: AtomicU64 = AtomicU64::new(1);

struct TempTestDir {
    path: PathBuf,
}

impl TempTestDir {
    fn new(prefix: &str) -> Self {
        let id = NEXT_DIR_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "bam_sig_sec_{prefix}_{}_{}",
            std::process::id(),
            id
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create temp test dir");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempTestDir {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o700));
        }
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn fixture_dylib(stem: &str) -> PathBuf {
    let dir = std::env::var_os("BAM_FIXTURE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/fixtures"));
    let prefix = if cfg!(windows) { "" } else { "lib" };
    let ext = std::env::consts::DLL_EXTENSION;
    let candidate = dir.join(format!("{prefix}{stem}.{ext}"));
    assert!(
        candidate.exists(),
        "missing fixture '{}'; run `cargo xtask fixtures`",
        candidate.display()
    );
    candidate
}

fn dir_entry_count(dir: &Path) -> usize {
    std::fs::read_dir(dir)
        .expect("read dir")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect dir entries")
        .len()
}

fn make_signed_fixture_copy(
    dir: &Path,
    stem: &str,
    ca: &PrivateKey,
    signer: &PrivateKey,
) -> (PathBuf, PathBuf) {
    let src = fixture_dylib(stem);
    let ext = std::env::consts::DLL_EXTENSION;
    let mod_path = dir.join(format!("{stem}.{ext}"));
    let sig_path = default_signature_path_for(&mod_path);
    std::fs::copy(&src, &mod_path).expect("copy fixture binary");
    let bytes = std::fs::read(&mod_path).expect("read copied binary");

    let mut cert_builder =
        certificate::Builder::new_with_random_nonce(&mut OsRng, signer.public_key(), 100, 10_000)
            .expect("cert builder");
    cert_builder.serial(42).expect("serial");
    cert_builder.key_id("sec-test-cert").expect("key_id");
    cert_builder.valid_principal(stem).expect("principal");
    let cert = cert_builder.sign(ca).expect("sign cert");
    let pem = sign_mod_bytes_with_certificate(signer, &cert, DEFAULT_SIGNATURE_NAMESPACE, &bytes)
        .expect("sign mod bytes");
    std::fs::write(&sig_path, pem.as_bytes()).expect("write sig");
    (mod_path, sig_path)
}

#[test]
fn directory_inputs_empty_binary_oversized_sig_and_custom_sig_path_enforced() {
    let tmp = TempTestDir::new("inputs");
    let key = PrivateKey::random(&mut OsRng, Algorithm::Ed25519).expect("keygen");
    let policy =
        ModTrustPolicy::from_public_key_openssh(&key.public_key().to_openssh().unwrap()).unwrap();

    let mut world = World::new();
    world.insert_resource(policy.clone());

    // 1. Passing a directory as source_path fails cleanly before staging.
    let sub_dir = tmp.path().join("mod_dir");
    std::fs::create_dir(&sub_dir).unwrap();
    let err_dir_src = load_mod_in_world(&mut world, &sub_dir, &LoadOptions::default())
        .err()
        .expect("directory source_path must fail");
    assert!(matches!(
        err_dir_src,
        ModLoadError::SignatureVerification {
            source: SignatureVerificationError::SourceBinaryRead { .. },
            ..
        }
    ));

    // 2. Passing a directory as signature_path fails cleanly.
    let mod_copy = tmp.path().join(format!(
        "asymmetric_c_mod.{}",
        std::env::consts::DLL_EXTENSION
    ));
    std::fs::copy(fixture_dylib("asymmetric_c_mod"), &mod_copy).unwrap();
    let sig_dir = tmp.path().join("sig_dir");
    std::fs::create_dir(&sig_dir).unwrap();
    let err_dir_sig = load_mod_in_world(
        &mut world,
        &mod_copy,
        &LoadOptions::default().with_signature_path(&sig_dir),
    )
    .err()
    .expect("directory signature_path must fail");
    assert!(matches!(
        err_dir_sig,
        ModLoadError::SignatureVerification {
            source: SignatureVerificationError::SignatureFileRead { .. },
            ..
        }
    ));

    // 3. Empty 0-byte binary file rejected with EmptyBinary.
    let empty_bin = tmp
        .path()
        .join(format!("empty.{}", std::env::consts::DLL_EXTENSION));
    std::fs::write(&empty_bin, b"").unwrap();
    let dummy_sig = sign_mod_bytes_with_key(&key, DEFAULT_SIGNATURE_NAMESPACE, b"dummy").unwrap();
    std::fs::write(default_signature_path_for(&empty_bin), dummy_sig.as_bytes()).unwrap();
    let err_empty = load_mod_in_world(&mut world, &empty_bin, &LoadOptions::default())
        .err()
        .expect("empty binary must fail");
    assert!(matches!(
        err_empty,
        ModLoadError::SignatureVerification {
            source: SignatureVerificationError::EmptyBinary,
            ..
        }
    ));

    // 4. Oversized .sig sidecar (> MAX_SIGNATURE_SIDECAR_BYTES) rejected before PEM decode.
    let big_sig = tmp.path().join("too_big.sig");
    std::fs::write(
        &big_sig,
        vec![b'A'; usize::try_from(MAX_SIGNATURE_SIDECAR_BYTES).unwrap() + 1],
    )
    .unwrap();
    let err_big_sig = load_mod_in_world(
        &mut world,
        &mod_copy,
        &LoadOptions::default().with_signature_path(&big_sig),
    )
    .err()
    .expect("oversized .sig must fail");
    assert!(matches!(
        err_big_sig,
        ModLoadError::SignatureVerification {
            source: SignatureVerificationError::SignatureFileTooLarge { .. },
            ..
        }
    ));

    // 5. Binary exceeding `max_binary_bytes` rejected before full buffering / staging.
    let raw_bytes = std::fs::read(&mod_copy).unwrap();
    let custom_sig = tmp.path().join("detached_elsewhere.pem");
    let pem = sign_mod_bytes_with_key(&key, DEFAULT_SIGNATURE_NAMESPACE, &raw_bytes).unwrap();
    std::fs::write(&custom_sig, pem.as_bytes()).unwrap();
    world.insert_resource(policy.clone().with_max_binary_bytes(64));
    let err_too_big_bin = load_mod_in_world(
        &mut world,
        &mod_copy,
        &LoadOptions::default().with_signature_path(&custom_sig),
    )
    .err()
    .expect("binary above max_binary_bytes must fail");
    assert!(matches!(
        err_too_big_bin,
        ModLoadError::SignatureVerification {
            source: SignatureVerificationError::BinaryTooLarge { .. },
            ..
        }
    ));
    world.insert_resource(policy);

    // 6. Custom signature path overrides `<mod>.sig` when valid and cleans up cleanly on unload.
    assert!(matches!(
        load_mod_in_world(&mut world, &mod_copy, &LoadOptions::default()),
        Err(ModLoadError::SignatureVerification {
            source: SignatureVerificationError::MissingSignatureFile { .. },
            ..
        })
    ));
    let (mod_id, systems) = load_mod_in_world(
        &mut world,
        &mod_copy,
        &LoadOptions::default().with_signature_path(&custom_sig),
    )
    .expect("custom signature path load succeeds");
    assert_eq!(
        world.resource::<ModHost>().get_mod(mod_id).unwrap().name(),
        "asymmetric_c_mod"
    );
    drop(systems);
    unload_all_mods_in_world(&mut world, |_, _, _, _, _| {});
}

#[cfg(unix)]
#[test]
fn unix_shadow_dir_rejects_symlinks_world_writable_and_enforces_0700_perms() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = TempTestDir::new("unix_perms");
    let ca = PrivateKey::random(&mut OsRng, Algorithm::Ed25519).unwrap();
    let signer = PrivateKey::random(&mut OsRng, Algorithm::Ed25519).unwrap();
    let (mod_path, _) = make_signed_fixture_copy(tmp.path(), "asymmetric_c_mod", &ca, &signer);

    let policy = ModTrustPolicy::from_ca_openssh(&ca.public_key().to_openssh().unwrap())
        .unwrap()
        .with_validity_policy(CertificateValidityPolicy::FixedTimestamp(500));

    let mut world = World::new();
    world.insert_resource(policy);

    // 1. Live symlink as shadow_dir must be rejected and target directory left empty.
    let target_dir = tmp.path().join("real_target_dir");
    std::fs::create_dir(&target_dir).unwrap();
    let live_link = tmp.path().join("live_shadow_link");
    std::os::unix::fs::symlink(&target_dir, &live_link).unwrap();
    let err_link = load_mod_in_world(
        &mut world,
        &mod_path,
        &LoadOptions::default().with_shadow_dir(&live_link),
    )
    .err()
    .expect("live symlink shadow_dir must be rejected");
    assert!(matches!(
        &err_link,
        ModLoadError::ShadowCopy { source, .. }
            if source.kind() == std::io::ErrorKind::PermissionDenied
    ));
    assert_eq!(dir_entry_count(&target_dir), 0);

    // 2. Dangling symlink as shadow_dir must be rejected without creating target.
    let missing_target = tmp.path().join("does_not_exist");
    let dangling_link = tmp.path().join("dangling_shadow_link");
    std::os::unix::fs::symlink(&missing_target, &dangling_link).unwrap();
    let err_dangling = load_mod_in_world(
        &mut world,
        &mod_path,
        &LoadOptions::default().with_shadow_dir(&dangling_link),
    )
    .err()
    .expect("dangling symlink shadow_dir must be rejected");
    assert!(matches!(
        &err_dangling,
        ModLoadError::ShadowCopy { source, .. }
            if source.kind() == std::io::ErrorKind::PermissionDenied
    ));
    assert!(!missing_target.exists());

    // 3. World-writable (0o777) shadow_dir must be rejected and untouched.
    let open_dir = tmp.path().join("world_writable_dir");
    std::fs::create_dir(&open_dir).unwrap();
    std::fs::set_permissions(&open_dir, std::fs::Permissions::from_mode(0o777)).unwrap();
    let err_open = load_mod_in_world(
        &mut world,
        &mod_path,
        &LoadOptions::default().with_shadow_dir(&open_dir),
    )
    .err()
    .expect("world-writable shadow_dir must be rejected");
    assert!(matches!(
        &err_open,
        ModLoadError::ShadowCopy { source, .. }
            if source.kind() == std::io::ErrorKind::PermissionDenied
    ));
    assert_eq!(dir_entry_count(&open_dir), 0);

    // 4. Fresh shadow_dir created with 0o700, staged file created with 0o700, unlinked on unload.
    let private_dir = tmp.path().join("private_verified_shadows");
    let (mod_id, systems) = load_mod_in_world(
        &mut world,
        &mod_path,
        &LoadOptions::default().with_shadow_dir(&private_dir),
    )
    .expect("private verified shadow dir load succeeds");

    let dir_mode = std::fs::symlink_metadata(&private_dir)
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(dir_mode, 0o700);

    let staged_path = world
        .resource::<ModHost>()
        .get_mod(mod_id)
        .unwrap()
        .loaded_path()
        .to_path_buf();
    assert_eq!(staged_path.parent(), Some(private_dir.as_path()));
    let file_mode = std::fs::symlink_metadata(&staged_path)
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(file_mode, 0o700);

    drop(systems);
    unload_all_mods_in_world(&mut world, |_, _, _, _, _| {});
    assert_eq!(
        dir_entry_count(&private_dir),
        0,
        "shadow copy must be unlinked after unload"
    );
}

#[test]
fn no_shadow_file_leaked_on_dlopen_or_init_failure() {
    let tmp = TempTestDir::new("rollback_cleanup");
    let shadow_dir = tmp.path().join("shadows");
    let ca = PrivateKey::random(&mut OsRng, Algorithm::Ed25519).unwrap();
    let signer = PrivateKey::random(&mut OsRng, Algorithm::Ed25519).unwrap();

    let policy = ModTrustPolicy::from_ca_openssh(&ca.public_key().to_openssh().unwrap())
        .unwrap()
        .with_validity_policy(CertificateValidityPolicy::FixedTimestamp(500));
    let mut world = World::new();
    world.insert_resource(policy);

    // Subcase A: Signed garbage bytes pass signature check, fail `dlopen` -> zero shadow files left.
    let mut cert_b =
        certificate::Builder::new_with_random_nonce(&mut OsRng, signer.public_key(), 100, 10_000)
            .unwrap();
    cert_b.serial(55).unwrap();
    cert_b.key_id("bad-elf").unwrap();
    cert_b.valid_principal("bad_mod").unwrap();
    let cert = cert_b.sign(&ca).unwrap();

    let garbage_path = tmp
        .path()
        .join(format!("not_a_dylib.{}", std::env::consts::DLL_EXTENSION));
    let garbage = b"THIS_IS_SIGNED_BUT_NOT_A_NATIVE_SHARED_LIBRARY";
    std::fs::write(&garbage_path, garbage).unwrap();
    let pem = sign_mod_bytes_with_certificate(&signer, &cert, DEFAULT_SIGNATURE_NAMESPACE, garbage)
        .unwrap();
    std::fs::write(default_signature_path_for(&garbage_path), pem.as_bytes()).unwrap();

    let err_dl = load_mod_in_world(
        &mut world,
        &garbage_path,
        &LoadOptions::default().with_shadow_dir(&shadow_dir),
    )
    .err()
    .expect("garbage bytes must fail dlopen");
    assert!(matches!(err_dl, ModLoadError::DlOpen { .. }));
    assert_eq!(
        dir_entry_count(&shadow_dir),
        0,
        "failed dlopen must not leave shadow file in {}",
        shadow_dir.display()
    );

    // Subcase B: Signed `init_rollback_mod` fails `bevy_abi_mod_init` -> zero shadow files left.
    let (rollback_path, _) =
        make_signed_fixture_copy(tmp.path(), "init_rollback_mod", &ca, &signer);
    let err_init = load_mod_in_world(
        &mut world,
        &rollback_path,
        &LoadOptions::default().with_shadow_dir(&shadow_dir),
    )
    .err()
    .expect("init_rollback_mod must fail init");
    assert!(matches!(err_init, ModLoadError::InitFailed { .. }));
    assert_eq!(
        dir_entry_count(&shadow_dir),
        0,
        "failed bevy_abi_mod_init must not leave shadow file in {}",
        shadow_dir.display()
    );
}
