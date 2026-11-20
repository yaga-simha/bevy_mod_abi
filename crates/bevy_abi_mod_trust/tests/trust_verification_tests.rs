//! Integration and adversarial security tests for `bevy_abi_mod_trust`:
//! OpenSSH `SSHSIG`, Ed25519 CA certificates, strict Curve25519 subgroup/canonical validation,
//! `PROTOCOL.krl` (`SSHKRL`) scoping & KRL signing authority separation, PEM armor strictness,
//! bounded file reads, and `keygen` hardening.

use bevy_abi_mod_trust::{
    Algorithm, Certificate, CertificateValidityPolicy, DEFAULT_KRL_SIGNATURE_NAMESPACE,
    DEFAULT_KRL_SIGNER_PRINCIPAL, DEFAULT_SIGNATURE_NAMESPACE, HashAlg, KrlBuilder, LineEnding,
    MAX_SIGNATURE_SIDECAR_BYTES, ModTrustConfigError, ModTrustPolicy, PrivateKey, PublicKey,
    SignatureVerificationError, default_signature_path_for, sign_mod_bytes_with_certificate,
    sign_mod_bytes_with_key, sign_mod_bytes_with_openssh_certificate,
};
use signature::Signer;
use ssh_encoding::{Base64Writer, Decode, DecodePem, Encode, Reader, Writer, pem};
use ssh_key::Signature;
use ssh_key::certificate::{self, CertType, OptionsMap};
use ssh_key::public::{Ed25519PublicKey, KeyData};
use ssh_key::rand_core::OsRng;
use ssh_key::sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use tempfile::TempDir;

const SSHSIG_MAGIC_PREAMBLE: &[u8] = b"SSHSIG";
const SSHSIG_SUPPORTED_VERSION: u32 = 1;
const ED25519_CERT_ALGORITHM_ID: &str = "ssh-ed25519-cert-v01@openssh.com";
const SSHKRL_MAGIC: &[u8; 8] = b"SSHKRL\n\0";
const KRL_SECTION_FINGERPRINT_SHA1: u8 = 0x03;
const KRL_SECTION_SIGNATURE: u8 = 0x04;
const KRL_SECTION_EXTENSION: u8 = 255;

fn make_keypair() -> PrivateKey {
    PrivateKey::random(&mut OsRng, Algorithm::Ed25519).expect("generate Ed25519 key")
}

fn make_user_cert(
    ca: &PrivateKey,
    subject: &PrivateKey,
    key_id: &str,
    principals: &[&str],
    serial: u64,
    valid_after: u64,
    valid_before: u64,
) -> Certificate {
    let mut builder = certificate::Builder::new_with_random_nonce(
        &mut OsRng,
        subject.public_key(),
        valid_after,
        valid_before,
    )
    .expect("cert builder");
    builder.serial(serial).expect("serial");
    builder.key_id(key_id).expect("key_id");
    builder.cert_type(CertType::User).expect("cert_type");
    for &p in principals {
        builder.valid_principal(p).expect("principal");
    }
    builder.sign(ca).expect("sign cert")
}

fn pem_encode_sshsig(raw: &[u8]) -> String {
    let len =
        pem::encapsulated_len_wrapped("SSH SIGNATURE", 70, LineEnding::LF, raw.len()).expect("len");
    let mut buf = vec![0u8; len];
    let mut enc = pem::Encoder::new_wrapped("SSH SIGNATURE", 70, LineEnding::LF, &mut buf).unwrap();
    enc.encode(raw).unwrap();
    let written = enc.finish().unwrap();
    String::from_utf8(buf[..written].to_vec()).unwrap()
}

#[derive(Clone, Debug)]
enum TestSigner {
    PublicKey(KeyData),
    Certificate(Vec<u8>),
}

#[derive(Clone, Debug)]
struct TestSshSigEnvelope {
    version: u32,
    signer: TestSigner,
    namespace: String,
    reserved: Vec<u8>,
    hash_alg: HashAlg,
    signature: Signature,
}

impl pem::PemLabel for TestSshSigEnvelope {
    const PEM_LABEL: &'static str = "SSH SIGNATURE";
}

impl TestSshSigEnvelope {
    fn encode_raw(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(SSHSIG_MAGIC_PREAMBLE);
        self.version.encode(&mut out).unwrap();
        let mut pk_blob = Vec::new();
        match &self.signer {
            TestSigner::PublicKey(kd) => kd.encode(&mut pk_blob).unwrap(),
            TestSigner::Certificate(blob) => pk_blob.extend_from_slice(blob),
        }
        pk_blob.encode(&mut out).unwrap();
        self.namespace.encode(&mut out).unwrap();
        self.reserved.encode(&mut out).unwrap();
        self.hash_alg.encode(&mut out).unwrap();
        self.signature.encode_prefixed(&mut out).unwrap();
        out
    }

    fn encode_pem(&self) -> String {
        pem_encode_sshsig(&self.encode_raw())
    }
}

impl Decode for TestSshSigEnvelope {
    type Error = ssh_key::Error;

    fn decode(reader: &mut impl Reader) -> Result<Self, Self::Error> {
        let mut magic = [0u8; 6];
        reader.read(&mut magic)?;
        let version = u32::decode(reader)?;
        let pk_blob = Vec::<u8>::decode(reader)?;
        let mut peek = pk_blob.as_slice();
        let alg = String::decode(&mut peek)?;
        let signer = if alg.ends_with("-cert-v01@openssh.com") {
            TestSigner::Certificate(pk_blob)
        } else {
            let mut r = pk_blob.as_slice();
            let kd = KeyData::decode(&mut r)?;
            TestSigner::PublicKey(kd)
        };
        let namespace = String::decode(reader)?;
        let reserved = Vec::<u8>::decode(reader)?;
        let hash_alg = HashAlg::decode(reader)?;
        let signature = reader.read_prefixed(Signature::decode)?;
        Ok(Self {
            version,
            signer,
            namespace,
            reserved,
            hash_alg,
            signature,
        })
    }
}

impl Encode for TestSshSigEnvelope {
    fn encoded_len(&self) -> ssh_encoding::Result<usize> {
        Ok(self.encode_raw().len())
    }

    fn encode(&self, writer: &mut impl Writer) -> ssh_encoding::Result<()> {
        writer.write(&self.encode_raw())
    }
}

fn encode_signed_data(namespace: &str, reserved: &[u8], hash_alg: HashAlg, hash: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(SSHSIG_MAGIC_PREAMBLE);
    namespace.encode(&mut out).unwrap();
    reserved.encode(&mut out).unwrap();
    hash_alg.encode(&mut out).unwrap();
    hash.encode(&mut out).unwrap();
    out
}

const EIGHT_TORSION_POINTS: [[u8; 32]; 8] = [
    [
        0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00,
    ],
    [
        0x26, 0xe8, 0x95, 0x8f, 0xc2, 0xb2, 0x27, 0xb0, 0x45, 0xc3, 0xf4, 0x89, 0xf2, 0xef, 0x98,
        0xf0, 0xd5, 0xdf, 0xac, 0x05, 0xd3, 0xc6, 0x33, 0x39, 0xb1, 0x38, 0x02, 0x88, 0x6d, 0x53,
        0xfc, 0x85,
    ],
    [
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x80,
    ],
    [
        0xc7, 0x17, 0x6a, 0x70, 0x3d, 0x4d, 0xd8, 0x4f, 0xba, 0x3c, 0x0b, 0x76, 0x0d, 0x10, 0x67,
        0x0f, 0x2a, 0x20, 0x53, 0xfa, 0x2c, 0x39, 0xcc, 0xc6, 0x4e, 0xc7, 0xfd, 0x77, 0x92, 0xac,
        0x03, 0xfa,
    ],
    [
        0xec, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        0xff, 0x7f,
    ],
    [
        0xc7, 0x17, 0x6a, 0x70, 0x3d, 0x4d, 0xd8, 0x4f, 0xba, 0x3c, 0x0b, 0x76, 0x0d, 0x10, 0x67,
        0x0f, 0x2a, 0x20, 0x53, 0xfa, 0x2c, 0x39, 0xcc, 0xc6, 0x4e, 0xc7, 0xfd, 0x77, 0x92, 0xac,
        0x03, 0x7a,
    ],
    [
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00,
    ],
    [
        0x26, 0xe8, 0x95, 0x8f, 0xc2, 0xb2, 0x27, 0xb0, 0x45, 0xc3, 0xf4, 0x89, 0xf2, 0xef, 0x98,
        0xf0, 0xd5, 0xdf, 0xac, 0x05, 0xd3, 0xc6, 0x33, 0x39, 0xb1, 0x38, 0x02, 0x88, 0x6d, 0x53,
        0xfc, 0x05,
    ],
];

const NON_CANONICAL_Y_PLUS_P: [u8; 32] = [
    0xf0, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x7f,
];

const NON_CANONICAL_Y_EQUALS_P: [u8; 32] = [
    0xed, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x7f,
];

fn raw_key_to_openssh(raw: [u8; 32]) -> String {
    PublicKey::from(Ed25519PublicKey(raw))
        .to_openssh()
        .expect("encode raw Ed25519 key")
}

fn add_ed25519_order_to_s(sig64: &mut [u8; 64]) {
    const L_LE: [u8; 32] = [
        0xed, 0xd3, 0xf5, 0x5c, 0x1a, 0x63, 0x12, 0x58, 0xd6, 0x9c, 0xf7, 0xa2, 0xde, 0xf9, 0xde,
        0x14, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x10,
    ];
    let mut carry = 0u16;
    for i in 0..32 {
        let sum = u16::from(sig64[32 + i]) + u16::from(L_LE[i]) + carry;
        sig64[32 + i] = (sum & 0xff) as u8;
        carry = sum >> 8;
    }
    assert_eq!(carry, 0);
}

#[allow(clippy::too_many_arguments)]
fn build_raw_ed25519_cert_blob(
    nonce: &[u8],
    leaf_raw: [u8; 32],
    serial: u64,
    key_id: &str,
    principals: &[&str],
    valid_after: u64,
    valid_before: u64,
    ca_signing_key: &PrivateKey,
    override_ca_raw: Option<[u8; 32]>,
    tamper_ca_sig_s: bool,
) -> Vec<u8> {
    let mut tbs = Vec::new();
    ED25519_CERT_ALGORITHM_ID.encode(&mut tbs).unwrap();
    nonce.encode(&mut tbs).unwrap();
    Ed25519PublicKey(leaf_raw).encode(&mut tbs).unwrap();
    serial.encode(&mut tbs).unwrap();
    1u32.encode(&mut tbs).unwrap(); // CertType::User
    key_id.encode(&mut tbs).unwrap();
    let mut princ_buf = Vec::new();
    for p in principals {
        p.encode(&mut princ_buf).unwrap();
    }
    princ_buf.encode(&mut tbs).unwrap();
    valid_after.encode(&mut tbs).unwrap();
    valid_before.encode(&mut tbs).unwrap();
    OptionsMap::default().encode(&mut tbs).unwrap();
    OptionsMap::default().encode(&mut tbs).unwrap();
    ([] as [u8; 0]).as_slice().encode(&mut tbs).unwrap();

    let ca_kd = match override_ca_raw {
        Some(raw) => KeyData::Ed25519(Ed25519PublicKey(raw)),
        None => ca_signing_key.public_key().key_data().clone(),
    };
    let mut ca_buf = Vec::new();
    ca_kd.encode(&mut ca_buf).unwrap();
    ca_buf.encode(&mut tbs).unwrap();

    let ca_sig = ca_signing_key.try_sign(&tbs).unwrap();
    let final_ca_sig = if tamper_ca_sig_s {
        let mut bytes: [u8; 64] = ca_sig.as_bytes().try_into().unwrap();
        add_ed25519_order_to_s(&mut bytes);
        Signature::new(Algorithm::Ed25519, bytes.to_vec()).unwrap()
    } else {
        ca_sig
    };

    let mut cert_blob = tbs;
    final_ca_sig.encode_prefixed(&mut cert_blob).unwrap();
    cert_blob
}

#[test]
fn ca_certificate_and_sshsig_verification_succeeds() {
    let ca = make_keypair();
    let signer = make_keypair();
    let cert = make_user_cert(
        &ca,
        &signer,
        "studio-ci-1",
        &["combat_mod", "publisher_a"],
        42,
        1_000,
        2_000,
    );
    let payload = b"fake-mod-binary-bytes";
    let sig_pem =
        sign_mod_bytes_with_certificate(&signer, &cert, DEFAULT_SIGNATURE_NAMESPACE, payload)
            .expect("sign");

    let policy = ModTrustPolicy::from_ca_openssh(&ca.public_key().to_openssh().unwrap())
        .unwrap()
        .with_validity_policy(CertificateValidityPolicy::FixedTimestamp(1_500))
        .with_required_principal("combat_mod");

    let receipt = policy
        .verify_bytes(payload, sig_pem.as_bytes())
        .expect("verify succeeds");
    assert_eq!(
        receipt.signer_fingerprint,
        signer.public_key().fingerprint(HashAlg::Sha256).to_string()
    );
    assert_eq!(
        receipt.ca_fingerprint.as_deref(),
        Some(
            ca.public_key()
                .fingerprint(HashAlg::Sha256)
                .to_string()
                .as_str()
        )
    );
    assert_eq!(receipt.key_id.as_deref(), Some("studio-ci-1"));
    assert_eq!(receipt.principals, vec!["combat_mod", "publisher_a"]);
    assert_eq!(receipt.serial, Some(42));
    assert_eq!(receipt.namespace, DEFAULT_SIGNATURE_NAMESPACE);
    let expected_sha: [u8; 32] = Sha256::digest(payload).into();
    assert_eq!(receipt.binary_sha256, expected_sha);

    assert!(
        policy
            .with_manifest_name_principal_binding(true)
            .verify_manifest_principal(&receipt, "combat_mod")
            .is_ok()
    );
}

#[test]
fn direct_pinned_public_key_and_key_rotation_work() {
    let key1 = make_keypair();
    let key2 = make_keypair();
    let payload = b"pinned-mod";
    let sig_pem =
        sign_mod_bytes_with_key(&key2, DEFAULT_SIGNATURE_NAMESPACE, payload).expect("sign");

    let policy = ModTrustPolicy::from_public_key_openssh(&key1.public_key().to_openssh().unwrap())
        .unwrap()
        .add_public_key_openssh(&key2.public_key().to_openssh().unwrap())
        .unwrap();

    let receipt = policy
        .verify_bytes(payload, sig_pem.as_bytes())
        .expect("pinned key verifies");
    assert_eq!(receipt.ca_fingerprint, None);
    assert_eq!(receipt.serial, None);
    assert!(receipt.principals.is_empty());
}

#[test]
fn strict_policy_without_anchors_fails_closed() {
    let key = make_keypair();
    let payload = b"mod";
    let sig_pem =
        sign_mod_bytes_with_key(&key, DEFAULT_SIGNATURE_NAMESPACE, payload).expect("sign");

    let err = ModTrustPolicy::strict()
        .verify_bytes(payload, sig_pem.as_bytes())
        .expect_err("must fail closed");
    assert!(matches!(
        err,
        SignatureVerificationError::NoTrustAnchorsConfigured
    ));
}

#[test]
fn tampered_binary_and_swapped_sig_fail() {
    let key = make_keypair();
    let mut payload = b"original-payload".to_vec();
    let sig_pem =
        sign_mod_bytes_with_key(&key, DEFAULT_SIGNATURE_NAMESPACE, &payload).expect("sign");

    let policy =
        ModTrustPolicy::from_public_key_openssh(&key.public_key().to_openssh().unwrap()).unwrap();

    payload[0] ^= 0x01;
    let err = policy
        .verify_bytes(&payload, sig_pem.as_bytes())
        .expect_err("tampered binary must fail");
    assert!(matches!(err, SignatureVerificationError::InvalidSignature));

    let sig_other =
        sign_mod_bytes_with_key(&key, DEFAULT_SIGNATURE_NAMESPACE, b"different-payload")
            .expect("sign");
    let err2 = policy
        .verify_bytes(b"original-payload", sig_other.as_bytes())
        .expect_err("swapped sig must fail");
    assert!(matches!(err2, SignatureVerificationError::InvalidSignature));
}

#[test]
fn malformed_pem_version_magic_and_trailing_bytes_rejected() {
    let key = make_keypair();
    let policy =
        ModTrustPolicy::from_public_key_openssh(&key.public_key().to_openssh().unwrap()).unwrap();

    assert!(matches!(
        policy.verify_bytes(b"a", b"not pem"),
        Err(SignatureVerificationError::MalformedSignature { .. })
    ));
    assert!(matches!(
        policy.verify_bytes(
            b"a",
            b"-----BEGIN PUBLIC KEY-----\nAAAA\n-----END PUBLIC KEY-----\n"
        ),
        Err(SignatureVerificationError::MalformedSignature { .. })
    ));

    // Bad magic
    let pem_bad_magic = pem_encode_sshsig(b"BADMAG");
    assert!(matches!(
        policy.verify_bytes(b"a", pem_bad_magic.as_bytes()),
        Err(SignatureVerificationError::MalformedSignature { .. })
    ));

    // Version 2
    let mut bad_ver = Vec::new();
    bad_ver.extend_from_slice(SSHSIG_MAGIC_PREAMBLE);
    2u32.encode(&mut bad_ver).unwrap();
    let pem_bad_ver = pem_encode_sshsig(&bad_ver);
    assert!(matches!(
        policy.verify_bytes(b"a", pem_bad_ver.as_bytes()),
        Err(SignatureVerificationError::MalformedSignature { .. })
    ));

    // Trailing bytes inside envelope
    let valid_pem = sign_mod_bytes_with_key(&key, DEFAULT_SIGNATURE_NAMESPACE, b"a").expect("sign");
    let valid_env = TestSshSigEnvelope::decode_pem(valid_pem.as_bytes()).unwrap();
    let mut raw = valid_env.encode_raw();
    raw.push(0x99);
    let pem_trailing = pem_encode_sshsig(&raw);
    assert!(matches!(
        policy.verify_bytes(b"a", pem_trailing.as_bytes()),
        Err(SignatureVerificationError::MalformedSignature { .. })
    ));

    // Trailing non-whitespace garbage AFTER `-----END SSH SIGNATURE-----` MUST be rejected!
    let smuggled_after = format!("{valid_pem}SMUGGLED_PAYLOAD\n");
    assert!(matches!(
        policy.verify_bytes(b"a", smuggled_after.as_bytes()),
        Err(SignatureVerificationError::MalformedSignature { .. })
    ));

    // Leading non-whitespace garbage BEFORE `-----BEGIN SSH SIGNATURE-----` MUST be rejected!
    let smuggled_before = format!("LEADING_JUNK\n{valid_pem}");
    assert!(matches!(
        policy.verify_bytes(b"a", smuggled_before.as_bytes()),
        Err(SignatureVerificationError::MalformedSignature { .. })
    ));

    // Concatenated duplicate PEM blocks MUST be rejected!
    let doubled_pem = format!("{valid_pem}{valid_pem}");
    assert!(matches!(
        policy.verify_bytes(b"a", doubled_pem.as_bytes()),
        Err(SignatureVerificationError::MalformedSignature { .. })
    ));
}

#[test]
fn namespace_mismatch_and_untrusted_keys_rejected() {
    let ca = make_keypair();
    let rogue_ca = make_keypair();
    let signer = make_keypair();
    let payload = b"mod";

    let wrong_ns_sig = sign_mod_bytes_with_key(&signer, "file", payload).expect("sign");
    let key_policy =
        ModTrustPolicy::from_public_key_openssh(&signer.public_key().to_openssh().unwrap())
            .unwrap();
    assert!(matches!(
        key_policy.verify_bytes(payload, wrong_ns_sig.as_bytes()),
        Err(SignatureVerificationError::NamespaceMismatch { .. })
    ));

    let ca_policy =
        ModTrustPolicy::from_ca_openssh(&ca.public_key().to_openssh().unwrap()).unwrap();
    let raw_sig =
        sign_mod_bytes_with_key(&signer, DEFAULT_SIGNATURE_NAMESPACE, payload).expect("sign");
    assert!(matches!(
        ca_policy.verify_bytes(payload, raw_sig.as_bytes()),
        Err(SignatureVerificationError::UntrustedPublicKey { .. })
    ));

    let rogue_cert = make_user_cert(&rogue_ca, &signer, "rogue", &["mod"], 1, 0, 10_000);
    let rogue_sig =
        sign_mod_bytes_with_certificate(&signer, &rogue_cert, DEFAULT_SIGNATURE_NAMESPACE, payload)
            .expect("sign");
    let ca_policy_fixed =
        ca_policy.with_validity_policy(CertificateValidityPolicy::FixedTimestamp(5_000));
    assert!(matches!(
        ca_policy_fixed.verify_bytes(payload, rogue_sig.as_bytes()),
        Err(SignatureVerificationError::UntrustedCertificateAuthority { .. })
    ));

    let ca_only_same_key_cert = make_user_cert(&ca, &signer, "ok", &["mod"], 1, 0, 10_000);
    let mut tampered_cert_bytes = ca_only_same_key_cert.to_bytes().unwrap();
    let alg_len = 4 + ED25519_CERT_ALGORITHM_ID.len();
    tampered_cert_bytes[alg_len + 4] ^= 0xff;
    let forged_env = TestSshSigEnvelope {
        version: 1,
        signer: TestSigner::Certificate(tampered_cert_bytes),
        namespace: DEFAULT_SIGNATURE_NAMESPACE.to_owned(),
        reserved: Vec::new(),
        hash_alg: HashAlg::Sha512,
        signature: TestSshSigEnvelope::decode_pem(raw_sig.as_bytes())
            .unwrap()
            .signature,
    };
    let forged_pem = forged_env.encode_pem();
    assert!(matches!(
        ca_policy_fixed.verify_bytes(payload, forged_pem.as_bytes()),
        Err(SignatureVerificationError::UntrustedCertificateAuthority { .. })
    ));
}

#[test]
fn certificate_type_critical_options_and_validity_window_enforced() {
    let ca = make_keypair();
    let signer = make_keypair();
    let payload = b"mod";

    let mut host_builder =
        certificate::Builder::new_with_random_nonce(&mut OsRng, signer.public_key(), 100, 200)
            .unwrap();
    host_builder.cert_type(CertType::Host).unwrap();
    host_builder.valid_principal("mod").unwrap();
    let host_cert = host_builder.sign(&ca).unwrap();
    let host_sig =
        sign_mod_bytes_with_certificate(&signer, &host_cert, DEFAULT_SIGNATURE_NAMESPACE, payload)
            .unwrap();
    let base_policy = ModTrustPolicy::from_ca_openssh(&ca.public_key().to_openssh().unwrap())
        .unwrap()
        .with_validity_policy(CertificateValidityPolicy::FixedTimestamp(150));
    assert!(matches!(
        base_policy.verify_bytes(payload, host_sig.as_bytes()),
        Err(SignatureVerificationError::InvalidCertificateType { .. })
    ));

    let mut crit_builder =
        certificate::Builder::new_with_random_nonce(&mut OsRng, signer.public_key(), 100, 200)
            .unwrap();
    crit_builder.cert_type(CertType::User).unwrap();
    crit_builder.valid_principal("mod").unwrap();
    crit_builder
        .critical_option("force-command", "/bin/sh")
        .unwrap();
    let crit_cert = crit_builder.sign(&ca).unwrap();
    let crit_sig =
        sign_mod_bytes_with_certificate(&signer, &crit_cert, DEFAULT_SIGNATURE_NAMESPACE, payload)
            .unwrap();
    assert!(matches!(
        base_policy.verify_bytes(payload, crit_sig.as_bytes()),
        Err(SignatureVerificationError::UnrecognizedCriticalOption { .. })
    ));

    let timed_cert = make_user_cert(&ca, &signer, "timed", &["mod"], 7, 1_000, 2_000);
    let timed_sig =
        sign_mod_bytes_with_certificate(&signer, &timed_cert, DEFAULT_SIGNATURE_NAMESPACE, payload)
            .unwrap();

    assert!(matches!(
        base_policy
            .clone()
            .with_validity_policy(CertificateValidityPolicy::FixedTimestamp(999))
            .verify_bytes(payload, timed_sig.as_bytes()),
        Err(SignatureVerificationError::CertificateExpiredOrNotYetValid { .. })
    ));
    assert!(
        base_policy
            .clone()
            .with_validity_policy(CertificateValidityPolicy::FixedTimestamp(1_000))
            .verify_bytes(payload, timed_sig.as_bytes())
            .is_ok()
    );
    assert!(matches!(
        base_policy
            .clone()
            .with_validity_policy(CertificateValidityPolicy::FixedTimestamp(2_000))
            .verify_bytes(payload, timed_sig.as_bytes()),
        Err(SignatureVerificationError::CertificateExpiredOrNotYetValid { .. })
    ));
    assert!(
        base_policy
            .clone()
            .with_validity_policy(CertificateValidityPolicy::IgnoreTimestamps)
            .verify_bytes(payload, timed_sig.as_bytes())
            .is_ok()
    );
    assert!(matches!(
        base_policy
            .with_validity_policy(CertificateValidityPolicy::SystemClock)
            .verify_bytes(payload, timed_sig.as_bytes()),
        Err(SignatureVerificationError::CertificateExpiredOrNotYetValid { .. })
    ));
}

#[test]
fn revocations_principals_and_binary_size_limit_enforced() {
    let ca = make_keypair();
    let signer = make_keypair();
    let cert = make_user_cert(&ca, &signer, "k1", &["alpha_mod"], 99, 100, 500);
    let payload = b"0123456789";
    let sig = sign_mod_bytes_with_certificate(&signer, &cert, DEFAULT_SIGNATURE_NAMESPACE, payload)
        .unwrap();

    let policy = ModTrustPolicy::from_ca_openssh(&ca.public_key().to_openssh().unwrap())
        .unwrap()
        .with_validity_policy(CertificateValidityPolicy::FixedTimestamp(200));

    let rev_leaf = policy
        .clone()
        .revoke_key_openssh(&signer.public_key().to_openssh().unwrap())
        .unwrap();
    assert!(matches!(
        rev_leaf.verify_bytes(payload, sig.as_bytes()),
        Err(SignatureVerificationError::SignerRevoked { .. })
    ));

    let rev_ca = policy
        .clone()
        .revoke_fingerprint(&ca.public_key().fingerprint(HashAlg::Sha256).to_string())
        .unwrap();
    assert!(matches!(
        rev_ca.verify_bytes(payload, sig.as_bytes()),
        Err(SignatureVerificationError::SignerRevoked { .. })
    ));

    let rev_serial = policy.clone().revoke_serial(99);
    assert!(matches!(
        rev_serial.verify_bytes(payload, sig.as_bytes()),
        Err(SignatureVerificationError::CertificateSerialRevoked { serial: 99 })
    ));

    let wrong_principal = policy.clone().with_required_principal("beta_mod");
    assert!(matches!(
        wrong_principal.verify_bytes(payload, sig.as_bytes()),
        Err(SignatureVerificationError::PrincipalMismatch { .. })
    ));

    let receipt = policy.verify_bytes(payload, sig.as_bytes()).unwrap();
    let bind_policy = policy.clone().with_manifest_name_principal_binding(true);
    assert!(matches!(
        bind_policy.verify_manifest_principal(&receipt, "beta_mod"),
        Err(SignatureVerificationError::PrincipalMismatch { .. })
    ));

    let small_limit = policy.with_max_binary_bytes(4);
    assert!(matches!(
        small_limit.verify_bytes(payload, sig.as_bytes()),
        Err(SignatureVerificationError::BinaryTooLarge { size: 10, limit: 4 })
    ));
}

#[test]
fn default_signature_path_preserves_multi_dot_and_extension() {
    assert_eq!(
        default_signature_path_for(Path::new("/mods/foo.dll")),
        PathBuf::from("/mods/foo.dll.sig")
    );
    assert_eq!(
        default_signature_path_for(Path::new("/mods/libmy.mod.dylib")),
        PathBuf::from("/mods/libmy.mod.dylib.sig")
    );
}

#[test]
fn strict_ed25519_rejects_torsion_points_non_canonical_y_and_scalar_malleability() {
    for bad_raw in EIGHT_TORSION_POINTS
        .into_iter()
        .chain([NON_CANONICAL_Y_PLUS_P, NON_CANONICAL_Y_EQUALS_P])
    {
        let openssh = raw_key_to_openssh(bad_raw);
        assert!(matches!(
            ModTrustPolicy::from_ca_openssh(&openssh),
            Err(ModTrustConfigError::WeakOrInvalidPublicKey { .. })
        ));
        assert!(matches!(
            ModTrustPolicy::from_public_key_openssh(&openssh),
            Err(ModTrustConfigError::WeakOrInvalidPublicKey { .. })
        ));
    }

    let ca = make_keypair();
    let signer = make_keypair();
    let payload = b"native-mod-elf-or-macho";
    let policy = ModTrustPolicy::from_ca_openssh(&ca.public_key().to_openssh().unwrap())
        .unwrap()
        .add_public_key_openssh(&signer.public_key().to_openssh().unwrap())
        .unwrap()
        .with_validity_policy(CertificateValidityPolicy::FixedTimestamp(500));

    let dummy_sig = Signature::new(Algorithm::Ed25519, vec![0u8; 64]).unwrap();

    for bad_raw in EIGHT_TORSION_POINTS
        .into_iter()
        .chain([NON_CANONICAL_Y_PLUS_P, NON_CANONICAL_Y_EQUALS_P])
    {
        let env_raw = TestSshSigEnvelope {
            version: SSHSIG_SUPPORTED_VERSION,
            signer: TestSigner::PublicKey(KeyData::Ed25519(Ed25519PublicKey(bad_raw))),
            namespace: DEFAULT_SIGNATURE_NAMESPACE.to_owned(),
            reserved: Vec::new(),
            hash_alg: HashAlg::Sha512,
            signature: dummy_sig.clone(),
        };
        assert!(matches!(
            policy.verify_bytes(payload, env_raw.encode_pem().as_bytes()),
            Err(SignatureVerificationError::WeakOrInvalidPublicKey { .. })
        ));

        let cert_weak_leaf = build_raw_ed25519_cert_blob(
            b"0123456789abcdef",
            bad_raw,
            77,
            "weak-leaf",
            &["mod_a"],
            100,
            1000,
            &ca,
            None,
            false,
        );
        let env_cert_leaf = TestSshSigEnvelope {
            version: SSHSIG_SUPPORTED_VERSION,
            signer: TestSigner::Certificate(cert_weak_leaf),
            namespace: DEFAULT_SIGNATURE_NAMESPACE.to_owned(),
            reserved: Vec::new(),
            hash_alg: HashAlg::Sha512,
            signature: dummy_sig.clone(),
        };
        assert!(matches!(
            policy.verify_bytes(payload, env_cert_leaf.encode_pem().as_bytes()),
            Err(SignatureVerificationError::WeakOrInvalidPublicKey { .. })
        ));

        let valid_leaf: [u8; 32] = match signer.public_key().key_data() {
            KeyData::Ed25519(k) => k.0,
            _ => unreachable!(),
        };
        let cert_weak_ca = build_raw_ed25519_cert_blob(
            b"0123456789abcdef",
            valid_leaf,
            78,
            "weak-ca",
            &["mod_a"],
            100,
            1000,
            &ca,
            Some(bad_raw),
            false,
        );
        let env_cert_ca = TestSshSigEnvelope {
            version: SSHSIG_SUPPORTED_VERSION,
            signer: TestSigner::Certificate(cert_weak_ca),
            namespace: DEFAULT_SIGNATURE_NAMESPACE.to_owned(),
            reserved: Vec::new(),
            hash_alg: HashAlg::Sha512,
            signature: dummy_sig.clone(),
        };
        assert!(matches!(
            policy.verify_bytes(payload, env_cert_ca.encode_pem().as_bytes()),
            Err(SignatureVerificationError::WeakOrInvalidPublicKey { .. })
        ));
    }

    let valid_pem = sign_mod_bytes_with_key(&signer, DEFAULT_SIGNATURE_NAMESPACE, payload).unwrap();
    let mut env = TestSshSigEnvelope::decode_pem(valid_pem.as_bytes()).unwrap();
    let mut sig64: [u8; 64] = env.signature.as_bytes().try_into().unwrap();
    add_ed25519_order_to_s(&mut sig64);
    env.signature = Signature::new(Algorithm::Ed25519, sig64.to_vec()).unwrap();
    assert!(matches!(
        policy.verify_bytes(payload, env.encode_pem().as_bytes()),
        Err(SignatureVerificationError::InvalidSignature)
    ));

    let valid_leaf: [u8; 32] = match signer.public_key().key_data() {
        KeyData::Ed25519(k) => k.0,
        _ => unreachable!(),
    };
    let tampered_cert_blob = build_raw_ed25519_cert_blob(
        b"0123456789abcdef",
        valid_leaf,
        88,
        "tampered-ca-sig",
        &["mod_a"],
        100,
        1000,
        &ca,
        None,
        true,
    );
    let mut env_c = TestSshSigEnvelope::decode_pem(valid_pem.as_bytes()).unwrap();
    env_c.signer = TestSigner::Certificate(tampered_cert_blob);
    assert!(matches!(
        policy.verify_bytes(payload, env_c.encode_pem().as_bytes()),
        Err(SignatureVerificationError::UntrustedCertificateAuthority { .. })
    ));

    let mut bad_r_env = TestSshSigEnvelope::decode_pem(valid_pem.as_bytes()).unwrap();
    let mut bad_r_sig = [0u8; 64];
    bad_r_sig[..32].copy_from_slice(&EIGHT_TORSION_POINTS[1]);
    bad_r_env.signature = Signature::new(Algorithm::Ed25519, bad_r_sig.to_vec()).unwrap();
    assert!(matches!(
        policy.verify_bytes(payload, bad_r_env.encode_pem().as_bytes()),
        Err(SignatureVerificationError::InvalidSignature)
    ));
}

#[test]
fn empty_binary_nonce_forever_cert_crlf_reserved_and_dedup() {
    let ca = make_keypair();
    let signer = make_keypair();
    let ca_pub = ca.public_key().to_openssh().unwrap();

    let policy = ModTrustPolicy::from_ca_openssh(&format!("{ca_pub} comment-a"))
        .unwrap()
        .add_ca_openssh(&format!("{ca_pub} comment-b"))
        .unwrap()
        .add_public_key_openssh(&signer.public_key().to_openssh().unwrap())
        .unwrap()
        .add_public_key_openssh(&signer.public_key().to_openssh().unwrap())
        .unwrap();
    assert_eq!(policy.trusted_ca_count(), 1);
    assert_eq!(policy.trusted_public_key_count(), 1);

    let valid_pem = sign_mod_bytes_with_key(&signer, DEFAULT_SIGNATURE_NAMESPACE, b"x").unwrap();
    assert!(matches!(
        policy.verify_bytes(b"", valid_pem.as_bytes()),
        Err(SignatureVerificationError::EmptyBinary)
    ));

    let exact_policy = policy.clone().with_max_binary_bytes(4);
    let p4 = sign_mod_bytes_with_key(&signer, DEFAULT_SIGNATURE_NAMESPACE, b"1234").unwrap();
    assert!(exact_policy.verify_bytes(b"1234", p4.as_bytes()).is_ok());
    assert!(matches!(
        exact_policy.verify_bytes(b"12345", p4.as_bytes()),
        Err(SignatureVerificationError::BinaryTooLarge { size: 5, limit: 4 })
    ));

    let crlf_pem = p4.replace('\n', "\r\n");
    assert!(
        exact_policy
            .verify_bytes(b"1234", crlf_pem.as_bytes())
            .is_ok()
    );

    let reserved_bytes = b"ext=reserved-meta";
    let digest = HashAlg::Sha512.digest(b"1234");
    let sd = encode_signed_data(
        DEFAULT_SIGNATURE_NAMESPACE,
        reserved_bytes,
        HashAlg::Sha512,
        &digest,
    );
    let res_env = TestSshSigEnvelope {
        version: SSHSIG_SUPPORTED_VERSION,
        signer: TestSigner::PublicKey(signer.public_key().key_data().clone()),
        namespace: DEFAULT_SIGNATURE_NAMESPACE.to_owned(),
        reserved: reserved_bytes.to_vec(),
        hash_alg: HashAlg::Sha512,
        signature: signer.try_sign(&sd).unwrap(),
    };
    assert!(
        policy
            .verify_bytes(b"1234", res_env.encode_pem().as_bytes())
            .is_ok()
    );

    let mut tampered_res_env = res_env;
    tampered_res_env.reserved = b"tampered".to_vec();
    assert!(matches!(
        policy.verify_bytes(b"1234", tampered_res_env.encode_pem().as_bytes()),
        Err(SignatureVerificationError::InvalidSignature)
    ));

    let signer_raw: [u8; 32] = match signer.public_key().key_data() {
        KeyData::Ed25519(k) => k.0,
        _ => unreachable!(),
    };
    let empty_nonce_cert = build_raw_ed25519_cert_blob(
        b"",
        signer_raw,
        10,
        "empty-nonce",
        &["mod"],
        0,
        u64::MAX,
        &ca,
        None,
        false,
    );
    let mut env_nonce = TestSshSigEnvelope::decode_pem(p4.as_bytes()).unwrap();
    env_nonce.signer = TestSigner::Certificate(empty_nonce_cert);
    assert!(matches!(
        policy.verify_bytes(b"1234", env_nonce.encode_pem().as_bytes()),
        Err(SignatureVerificationError::MalformedSignature { .. })
    ));

    let forever_cert = build_raw_ed25519_cert_blob(
        b"forever-nonce-01",
        signer_raw,
        42,
        "forever",
        &["mod"],
        0,
        u64::MAX,
        &ca,
        None,
        false,
    );
    let mut forever_b64 = vec![0u8; forever_cert.len().div_ceil(3) * 4];
    let encoded_forever = {
        let mut w = Base64Writer::new(&mut forever_b64).unwrap();
        w.write(&forever_cert).unwrap();
        w.finish().unwrap().to_owned()
    };
    let forever_line = format!("{ED25519_CERT_ALGORITHM_ID} {encoded_forever} forever");
    let forever_sig = sign_mod_bytes_with_openssh_certificate(
        &signer,
        &forever_line,
        DEFAULT_SIGNATURE_NAMESPACE,
        b"1234",
    )
    .unwrap();
    assert!(policy.verify_bytes(b"1234", forever_sig.as_bytes()).is_ok());
    assert!(
        policy
            .clone()
            .with_validity_policy(CertificateValidityPolicy::FixedTimestamp(u64::MAX - 1))
            .verify_bytes(b"1234", forever_sig.as_bytes())
            .is_ok()
    );
    assert!(matches!(
        policy
            .with_validity_policy(CertificateValidityPolicy::FixedTimestamp(u64::MAX))
            .verify_bytes(b"1234", forever_sig.as_bytes()),
        Err(SignatureVerificationError::CertificateExpiredOrNotYetValid { .. })
    ));
}

#[test]
fn krl_sections_scoping_signed_anti_rollback_and_spec_parser() {
    let ca1 = make_keypair();
    let ca2 = make_keypair();
    let signer_a = make_keypair();
    let signer_b = make_keypair();
    let krl_delegate = make_keypair();

    let payload = b"compiled-mod-bytes";
    let cert_ca1_s7 = make_user_cert(&ca1, &signer_a, "kid-alpha", &["m"], 7, 0, 4_000_000_000);
    let cert_ca2_s7 = make_user_cert(&ca2, &signer_b, "kid-beta", &["m"], 7, 0, 4_000_000_000);
    let cert_ca1_s80 = make_user_cert(&ca1, &signer_a, "kid-hi", &["m"], 107, 0, 4_000_000_000);
    let cert_ca1_s200 = make_user_cert(&ca1, &signer_a, "kid-ok", &["m"], 200, 0, 4_000_000_000);
    let cert_krl = make_user_cert(
        &ca1,
        &krl_delegate,
        "krl-signer",
        &[DEFAULT_KRL_SIGNER_PRINCIPAL],
        999,
        0,
        4_000_000_000,
    );

    let sig_ca1_s7 = sign_mod_bytes_with_certificate(
        &signer_a,
        &cert_ca1_s7,
        DEFAULT_SIGNATURE_NAMESPACE,
        payload,
    )
    .unwrap();
    let sig_ca2_s7 = sign_mod_bytes_with_certificate(
        &signer_b,
        &cert_ca2_s7,
        DEFAULT_SIGNATURE_NAMESPACE,
        payload,
    )
    .unwrap();
    let sig_ca1_s80 = sign_mod_bytes_with_certificate(
        &signer_a,
        &cert_ca1_s80,
        DEFAULT_SIGNATURE_NAMESPACE,
        payload,
    )
    .unwrap();
    let sig_ca1_s200 = sign_mod_bytes_with_certificate(
        &signer_a,
        &cert_ca1_s200,
        DEFAULT_SIGNATURE_NAMESPACE,
        payload,
    )
    .unwrap();

    let base = ModTrustPolicy::from_ca_openssh(&ca1.public_key().to_openssh().unwrap())
        .unwrap()
        .add_ca_openssh(&ca2.public_key().to_openssh().unwrap())
        .unwrap()
        .with_validity_policy(CertificateValidityPolicy::FixedTimestamp(100));

    let krl_v5 = KrlBuilder::new()
        .version(5)
        .comment("scoped-and-global")
        .revoke_cert_serial(Some(ca1.public_key()), 7)
        .revoke_cert_serial_bitmap(Some(ca1.public_key()), 100, &[107])
        .unwrap()
        .revoke_cert_serial_range(None, 500, 510)
        .unwrap()
        .revoke_cert_key_id(Some(ca2.public_key()), "kid-other")
        .to_bytes()
        .unwrap();

    let krl_sig_by_delegate = sign_mod_bytes_with_certificate(
        &krl_delegate,
        &cert_krl,
        DEFAULT_KRL_SIGNATURE_NAMESPACE,
        &krl_v5,
    )
    .unwrap();
    let krl_sig_wrong_ns =
        sign_mod_bytes_with_key(&ca1, DEFAULT_SIGNATURE_NAMESPACE, &krl_v5).unwrap();

    assert!(matches!(
        base.clone()
            .add_signed_krl_bytes(&krl_v5, krl_sig_wrong_ns.as_bytes()),
        Err(ModTrustConfigError::KrlSignatureInvalid { .. })
    ));
    let rogue = make_keypair();
    let rogue_krl_sig =
        sign_mod_bytes_with_key(&rogue, DEFAULT_KRL_SIGNATURE_NAMESPACE, &krl_v5).unwrap();
    assert!(matches!(
        base.clone()
            .add_signed_krl_bytes(&krl_v5, rogue_krl_sig.as_bytes()),
        Err(ModTrustConfigError::KrlSignatureInvalid { .. })
    ));
    let mut tampered_krl = krl_v5.clone();
    *tampered_krl.last_mut().unwrap() ^= 0xff;
    assert!(matches!(
        base.clone()
            .add_signed_krl_bytes(&tampered_krl, krl_sig_by_delegate.as_bytes()),
        Err(ModTrustConfigError::KrlSignatureInvalid { .. })
    ));

    let p_v5 = base
        .clone()
        .add_signed_krl_bytes(&krl_v5, krl_sig_by_delegate.as_bytes())
        .unwrap();
    assert_eq!(p_v5.applied_krl_version(), Some(5));

    assert!(matches!(
        p_v5.verify_bytes(payload, sig_ca1_s7.as_bytes()),
        Err(SignatureVerificationError::CertificateSerialRevoked { serial: 7 })
    ));
    assert!(p_v5.verify_bytes(payload, sig_ca2_s7.as_bytes()).is_ok());
    assert!(matches!(
        p_v5.verify_bytes(payload, sig_ca1_s80.as_bytes()),
        Err(SignatureVerificationError::CertificateSerialRevoked { serial: 107 })
    ));
    let receipt_200 = p_v5.verify_bytes(payload, sig_ca1_s200.as_bytes()).unwrap();

    let krl_v4 = KrlBuilder::new().version(4).to_bytes().unwrap();
    let sig_v4 = sign_mod_bytes_with_key(&ca1, DEFAULT_KRL_SIGNATURE_NAMESPACE, &krl_v4).unwrap();
    assert!(matches!(
        p_v5.clone()
            .add_signed_krl_bytes(&krl_v4, sig_v4.as_bytes()),
        Err(ModTrustConfigError::KrlVersionRollback {
            offered: 4,
            min_required: 5,
        })
    ));
    assert!(matches!(
        base.clone().with_min_krl_version(6).add_krl_bytes(&krl_v5),
        Err(ModTrustConfigError::KrlVersionRollback {
            offered: 5,
            min_required: 6,
        })
    ));

    let krl_v6 = KrlBuilder::new()
        .version(6)
        .revoke_cert_key_id(None, "kid-ok")
        .revoke_cert_serial_range(None, u64::MAX - 5, u64::MAX)
        .unwrap()
        .revoke_key_openssh(&signer_b.public_key().to_openssh().unwrap())
        .unwrap()
        .to_bytes()
        .unwrap();
    let sig_v6 = sign_mod_bytes_with_key(&ca1, DEFAULT_KRL_SIGNATURE_NAMESPACE, &krl_v6).unwrap();
    let p_v6 = p_v5
        .add_signed_krl_bytes(&krl_v6, sig_v6.as_bytes())
        .unwrap();
    assert!(matches!(
        p_v6.verify_bytes(payload, sig_ca1_s200.as_bytes()),
        Err(SignatureVerificationError::CertificateKeyIdRevoked { .. })
    ));
    assert!(matches!(
        p_v6.verify_receipt_still_trusted(&receipt_200),
        Err(SignatureVerificationError::CertificateKeyIdRevoked { .. })
    ));
    assert!(matches!(
        p_v6.verify_bytes(payload, sig_ca2_s7.as_bytes()),
        Err(SignatureVerificationError::SignerRevoked { .. })
    ));

    let make_single_section_krl = |sec_type: u8, payload: &[u8]| {
        let mut b = Vec::new();
        b.extend_from_slice(SSHKRL_MAGIC);
        1u32.encode(&mut b).unwrap();
        9u64.encode(&mut b).unwrap();
        0u64.encode(&mut b).unwrap();
        0u64.encode(&mut b).unwrap();
        ([] as [u8; 0]).as_slice().encode(&mut b).unwrap();
        "".encode(&mut b).unwrap();
        sec_type.encode(&mut b).unwrap();
        payload.encode(&mut b).unwrap();
        b
    };
    assert!(matches!(
        base.clone().add_krl_bytes(&make_single_section_krl(
            KRL_SECTION_FINGERPRINT_SHA1,
            &[1, 2]
        )),
        Err(ModTrustConfigError::InvalidRevocationList { .. })
    ));
    assert!(matches!(
        base.clone()
            .add_krl_bytes(&make_single_section_krl(KRL_SECTION_SIGNATURE, &[1, 2])),
        Err(ModTrustConfigError::InvalidRevocationList { .. })
    ));

    let mut ext_crit = Vec::new();
    "vendor.crit@example".encode(&mut ext_crit).unwrap();
    1u8.encode(&mut ext_crit).unwrap();
    [42u8].as_slice().encode(&mut ext_crit).unwrap();
    assert!(matches!(
        base.clone()
            .add_krl_bytes(&make_single_section_krl(KRL_SECTION_EXTENSION, &ext_crit)),
        Err(ModTrustConfigError::InvalidRevocationList { .. })
    ));

    let mut ext_opt = Vec::new();
    "vendor.opt@example".encode(&mut ext_opt).unwrap();
    0u8.encode(&mut ext_opt).unwrap();
    [42u8].as_slice().encode(&mut ext_opt).unwrap();
    assert!(
        base.clone()
            .add_krl_bytes(&make_single_section_krl(KRL_SECTION_EXTENSION, &ext_opt))
            .is_ok()
    );

    let extra = make_keypair();
    let spec = format!(
        "# OpenSSH KRL text spec\n\nserial: 300-310\nserial: 350\nid: \"rev-id\"\nhash: {}\n",
        extra.public_key().fingerprint(HashAlg::Sha256)
    );
    let p_spec = base.clone().add_revocation_spec_str(&spec).unwrap();
    assert!(p_spec.is_cert_serial_revoked(&ca1.public_key().fingerprint(HashAlg::Sha256), 305));
    assert!(
        p_spec.is_cert_key_id_revoked(&ca1.public_key().fingerprint(HashAlg::Sha256), "rev-id")
    );
    assert!(matches!(
        base.add_revocation_spec_str("serial: 10-5"),
        Err(ModTrustConfigError::InvalidRevocationList { .. })
    ));
}

#[test]
fn krl_signing_escalation_from_mod_keys_and_unscoped_certs_is_blocked() {
    let ca = make_keypair();
    let pinned_mod_key = make_keypair();
    let mod_author = make_keypair();
    let unscoped_author = make_keypair();
    let dedicated_krl_key = make_keypair();
    let delegated_krl_signer = make_keypair();

    let policy = ModTrustPolicy::from_ca_openssh(&ca.public_key().to_openssh().unwrap())
        .unwrap()
        .add_public_key_openssh(&pinned_mod_key.public_key().to_openssh().unwrap())
        .unwrap()
        .with_validity_policy(CertificateValidityPolicy::FixedTimestamp(500));

    // Malicious KRL attempting `u64::MAX` anti-rollback DoS + revoking the Root CA!
    let dos_krl = KrlBuilder::new()
        .version(u64::MAX)
        .revoke_key_openssh(&ca.public_key().to_openssh().unwrap())
        .unwrap()
        .to_bytes()
        .unwrap();

    // 1. A pinned mod-signing key MUST NOT be allowed to sign KRLs!
    let pinned_sig =
        sign_mod_bytes_with_key(&pinned_mod_key, DEFAULT_KRL_SIGNATURE_NAMESPACE, &dos_krl)
            .unwrap();
    assert!(matches!(
        policy
            .clone()
            .add_signed_krl_bytes(&dos_krl, pinned_sig.as_bytes()),
        Err(ModTrustConfigError::KrlSignatureInvalid { .. })
    ));

    // 2. A CA-issued leaf mod-author cert (`principals = ["skin_mod"]`) MUST NOT sign KRLs!
    let mod_cert = make_user_cert(&ca, &mod_author, "skin-author", &["skin_mod"], 10, 0, 1000);
    let mod_cert_sig = sign_mod_bytes_with_certificate(
        &mod_author,
        &mod_cert,
        DEFAULT_KRL_SIGNATURE_NAMESPACE,
        &dos_krl,
    )
    .unwrap();
    assert!(matches!(
        policy
            .clone()
            .add_signed_krl_bytes(&dos_krl, mod_cert_sig.as_bytes()),
        Err(ModTrustConfigError::KrlSignatureInvalid { .. })
    ));

    // 3. An unscoped CA-issued cert (`principals = []`, OpenSSH wildcard cert) MUST NOT sign KRLs!
    let unscoped_raw: [u8; 32] = match unscoped_author.public_key().key_data() {
        KeyData::Ed25519(k) => k.0,
        _ => unreachable!(),
    };
    let unscoped_blob = build_raw_ed25519_cert_blob(
        b"unscoped-nonce-01",
        unscoped_raw,
        11,
        "unscoped",
        &[],
        0,
        1000,
        &ca,
        None,
        false,
    );
    let mut unscoped_b64 = vec![0u8; unscoped_blob.len().div_ceil(3) * 4];
    let encoded_unscoped = {
        let mut w = Base64Writer::new(&mut unscoped_b64).unwrap();
        w.write(&unscoped_blob).unwrap();
        w.finish().unwrap().to_owned()
    };
    let unscoped_cert_line = format!("{ED25519_CERT_ALGORITHM_ID} {encoded_unscoped} unscoped");
    let unscoped_sig = sign_mod_bytes_with_openssh_certificate(
        &unscoped_author,
        &unscoped_cert_line,
        DEFAULT_KRL_SIGNATURE_NAMESPACE,
        &dos_krl,
    )
    .unwrap();
    assert!(matches!(
        policy
            .clone()
            .add_signed_krl_bytes(&dos_krl, unscoped_sig.as_bytes()),
        Err(ModTrustConfigError::KrlSignatureInvalid { .. })
    ));

    // 4. Policy with ONLY pinned mod keys (no CA, no dedicated KRL signer) MUST reject KRLs!
    let pinned_only =
        ModTrustPolicy::from_public_key_openssh(&pinned_mod_key.public_key().to_openssh().unwrap())
            .unwrap();
    assert!(matches!(
        pinned_only.add_signed_krl_bytes(&dos_krl, pinned_sig.as_bytes()),
        Err(ModTrustConfigError::KrlSignatureInvalid { .. })
    ));

    // 5. Explicitly configured `trusted_krl_signers` CAN sign KRLs, but CANNOT sign mods!
    let krl_v1 = KrlBuilder::new().version(1).to_bytes().unwrap();
    let dedicated_krl_sig =
        sign_mod_bytes_with_key(&dedicated_krl_key, DEFAULT_KRL_SIGNATURE_NAMESPACE, &krl_v1)
            .unwrap();
    let policy_with_krl_key = policy
        .clone()
        .add_krl_signer_openssh(&dedicated_krl_key.public_key().to_openssh().unwrap())
        .unwrap();
    assert_eq!(policy_with_krl_key.trusted_krl_signer_count(), 1);
    let updated = policy_with_krl_key
        .clone()
        .add_signed_krl_bytes(&krl_v1, dedicated_krl_sig.as_bytes())
        .expect("explicitly trusted KRL signer must succeed");
    assert_eq!(updated.applied_krl_version(), Some(1));

    // Dedicated KRL key is NOT trusted to sign mods!
    let krl_key_mod_sig =
        sign_mod_bytes_with_key(&dedicated_krl_key, DEFAULT_SIGNATURE_NAMESPACE, b"mod").unwrap();
    assert!(matches!(
        policy_with_krl_key.verify_bytes(b"mod", krl_key_mod_sig.as_bytes()),
        Err(SignatureVerificationError::UntrustedPublicKey { .. })
    ));

    // 6. Custom KRL principal scoping works strictly!
    let custom_policy = policy
        .clone()
        .with_krl_signer_principal("ops-krl-authority")
        .unwrap();
    assert_eq!(custom_policy.krl_signer_principal(), "ops-krl-authority");
    assert!(policy.clone().with_krl_signer_principal("").is_err());

    let default_krl_cert = make_user_cert(
        &ca,
        &delegated_krl_signer,
        "default-krl",
        &[DEFAULT_KRL_SIGNER_PRINCIPAL],
        20,
        0,
        1000,
    );
    let default_krl_cert_sig = sign_mod_bytes_with_certificate(
        &delegated_krl_signer,
        &default_krl_cert,
        DEFAULT_KRL_SIGNATURE_NAMESPACE,
        &krl_v1,
    )
    .unwrap();
    // Default "krl-signer" cert rejected when policy expects "ops-krl-authority".
    assert!(matches!(
        custom_policy
            .clone()
            .add_signed_krl_bytes(&krl_v1, default_krl_cert_sig.as_bytes()),
        Err(ModTrustConfigError::KrlSignatureInvalid { .. })
    ));

    let ops_krl_cert = make_user_cert(
        &ca,
        &delegated_krl_signer,
        "ops-krl",
        &["ops-krl-authority"],
        21,
        0,
        1000,
    );
    let ops_krl_cert_sig = sign_mod_bytes_with_certificate(
        &delegated_krl_signer,
        &ops_krl_cert,
        DEFAULT_KRL_SIGNATURE_NAMESPACE,
        &krl_v1,
    )
    .unwrap();
    assert!(
        custom_policy
            .add_signed_krl_bytes(&krl_v1, ops_krl_cert_sig.as_bytes())
            .is_ok()
    );

    // 7. Revoked delegated KRL signer cert is rejected even with valid KRL principal!
    let revoked_policy = policy.revoke_serial(20);
    assert!(matches!(
        revoked_policy.add_signed_krl_bytes(&krl_v1, default_krl_cert_sig.as_bytes()),
        Err(ModTrustConfigError::KrlSignatureInvalid { .. })
    ));
}

#[test]
fn add_ca_file_and_add_public_key_file_reject_oversized_files() {
    let tmp = TempDir::new().unwrap();
    let giant = tmp.path().join("giant.pub");
    let big_bytes = vec![b'A'; (MAX_SIGNATURE_SIDECAR_BYTES as usize) + 64];
    std::fs::write(&giant, big_bytes).unwrap();

    assert!(matches!(
        ModTrustPolicy::strict().add_ca_file(&giant),
        Err(ModTrustConfigError::InvalidPublicKey { .. })
    ));
    assert!(matches!(
        ModTrustPolicy::strict().add_public_key_file(&giant),
        Err(ModTrustConfigError::InvalidPublicKey { .. })
    ));
    assert!(matches!(
        ModTrustPolicy::strict().add_krl_signer_file(&giant),
        Err(ModTrustConfigError::InvalidPublicKey { .. })
    ));
}

#[cfg(feature = "keygen")]
#[test]
fn keygen_refuses_overwrite_and_rejects_control_char_injection() {
    use bevy_abi_mod_trust::{
        generate_ed25519_keypair, issue_ed25519_certificate_openssh, write_openssh_keypair,
    };

    let tmp = TempDir::new().unwrap();
    let prefix = tmp.path().join("keys/dev_key");
    let first = write_openssh_keypair(&prefix, Some("initial")).unwrap();
    let orig_priv = std::fs::read(&first.private_key).unwrap();
    let orig_pub = std::fs::read(&first.public_key).unwrap();

    // Second write to same prefix MUST fail with AlreadyExists without clobbering existing key!
    let err = write_openssh_keypair(&prefix, Some("overwrite")).unwrap_err();
    assert!(matches!(
        err,
        ModTrustConfigError::Io { ref source, .. } if source.kind() == std::io::ErrorKind::AlreadyExists
    ));
    assert_eq!(std::fs::read(&first.private_key).unwrap(), orig_priv);
    assert_eq!(std::fs::read(&first.public_key).unwrap(), orig_pub);

    // Control characters in comment, key_id, and principals MUST be rejected!
    assert!(generate_ed25519_keypair(Some("bad\ninjected")).is_err());
    assert!(generate_ed25519_keypair(Some("bad\rcomment")).is_err());
    assert!(generate_ed25519_keypair(Some("bad\0comment")).is_err());

    let ca = generate_ed25519_keypair(Some("ca")).unwrap();
    let subj = generate_ed25519_keypair(Some("subj")).unwrap();
    assert!(
        issue_ed25519_certificate_openssh(
            &ca,
            subj.public_key(),
            1,
            "evil\nssh-ed25519 AAAA",
            &["mod_a".to_owned()],
            0,
            1000,
        )
        .is_err()
    );
    assert!(
        issue_ed25519_certificate_openssh(
            &ca,
            subj.public_key(),
            1,
            "ok-id",
            &["mod_a\r\nevil".to_owned()],
            0,
            1000,
        )
        .is_err()
    );
}

mod prop_tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn arbitrary_payload_bitflip_never_verifies(
            payload in proptest::collection::vec(any::<u8>(), 1..256),
            flip_byte in any::<prop::sample::Index>(),
            flip_bit in 0u8..8,
        ) {
            let signer = make_keypair();
            let policy = ModTrustPolicy::from_public_key_openssh(
                &signer.public_key().to_openssh().unwrap()
            ).unwrap();
            let pem = sign_mod_bytes_with_key(&signer, DEFAULT_SIGNATURE_NAMESPACE, &payload).unwrap();
            prop_assert!(policy.verify_bytes(&payload, pem.as_bytes()).is_ok());

            let mut tampered_payload = payload.clone();
            let p_idx = flip_byte.index(tampered_payload.len());
            tampered_payload[p_idx] ^= 1u8 << flip_bit;
            prop_assert!(policy.verify_bytes(&tampered_payload, pem.as_bytes()).is_err());

            let mut env = TestSshSigEnvelope::decode_pem(pem.as_bytes()).unwrap();
            let mut sig64: [u8; 64] = env.signature.as_bytes().try_into().unwrap();
            let s_idx = flip_byte.index(64);
            sig64[s_idx] ^= 1u8 << flip_bit;
            env.signature = Signature::new(Algorithm::Ed25519, sig64.to_vec()).unwrap();
            let bad_pem = env.encode_pem();
            prop_assert!(policy.verify_bytes(&payload, bad_pem.as_bytes()).is_err());
        }

        #[test]
        fn krl_serial_and_key_id_roundtrip(
            serials in proptest::collection::vec(0u64..1000, 1..16),
            bitmap_deltas in proptest::collection::vec(0u64..128, 1..16),
            offset in 1000u64..2000,
            range_start in 3000u64..3500,
            range_span in 0u64..100,
            probe in 0u64..3700,
            kid in "[a-z0-9_-]{1,16}",
        ) {
            let ca = make_keypair();
            let ca_pk = ca.public_key();
            let ca_fp = ca_pk.fingerprint(HashAlg::Sha256);

            let mut builder = KrlBuilder::new().version(1).revoke_cert_key_id(Some(ca_pk), kid.clone());
            for &s in &serials {
                builder = builder.revoke_cert_serial(Some(ca_pk), s);
            }
            let bm_serials: Vec<u64> = bitmap_deltas.iter().map(|d| offset + d).collect();
            builder = builder
                .revoke_cert_serial_bitmap(Some(ca_pk), offset, &bm_serials)
                .unwrap()
                .revoke_cert_serial_range(Some(ca_pk), range_start, range_start + range_span)
                .unwrap();

            let krl_bytes = builder.to_bytes().unwrap();
            let policy = ModTrustPolicy::from_ca_openssh(&ca_pk.to_openssh().unwrap())
                .unwrap()
                .add_krl_bytes(&krl_bytes)
                .unwrap();

            let expected = serials.contains(&probe)
                || bm_serials.contains(&probe)
                || (range_start <= probe && probe <= range_start + range_span);
            prop_assert_eq!(policy.is_cert_serial_revoked(&ca_fp, probe), expected);
            prop_assert!(policy.is_cert_key_id_revoked(&ca_fp, &kid));
        }

        #[test]
        fn parsers_never_panic_on_arbitrary_bytes(
            data in proptest::collection::vec(any::<u8>(), 0..512),
            split_idx in any::<prop::sample::Index>(),
        ) {
            let key = make_keypair();
            let policy = ModTrustPolicy::from_public_key_openssh(
                &key.public_key().to_openssh().unwrap()
            ).unwrap();
            let at = if data.is_empty() { 0 } else { split_idx.index(data.len()) };
            let (a, b) = data.split_at(at);
            let _ = policy.verify_bytes(a, b);
            let _ = policy.clone().add_krl_bytes(&data);
            let _ = policy.clone().add_signed_krl_bytes(a, b);
            if let Ok(s) = std::str::from_utf8(&data) {
                let _ = policy.add_revocation_spec_str(s);
            }
        }
    }
}
