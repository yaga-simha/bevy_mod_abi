//! Pure-Rust OpenSSH `SSHSIG` and Ed25519 Certificate Authority CLI (`cargo xtask sign`).

use signature::Signer;
use ssh_encoding::pem::PemLabel;
use ssh_encoding::{CheckedSum, Decode, DecodePem, Encode, EncodePem, LineEnding, Reader, Writer};
use ssh_key::certificate::{Builder as CertBuilder, CertType, OptionsMap};
use ssh_key::public::{Ed25519PublicKey, KeyData};
use ssh_key::rand_core::OsRng;
use ssh_key::sha2::{Digest, Sha256};
use ssh_key::{Algorithm, Fingerprint, HashAlg, PrivateKey, PublicKey, Signature};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const DEFAULT_NAMESPACE: &str = "bevy_abi_mod.v1";
const DEFAULT_KRL_NAMESPACE: &str = "bevy_abi_mod.krl.v1";
const SSHSIG_MAGIC: &[u8] = b"SSHSIG";
const SSHSIG_VERSION: u32 = 1;
const ED25519_CERT_ALG_ID: &str = "ssh-ed25519-cert-v01@openssh.com";

const SSHKRL_MAGIC: &[u8; 8] = b"SSHKRL\n\0";
const SSHKRL_VERSION: u32 = 1;
const KRL_SECTION_CERTIFICATES: u8 = 0x01;
const KRL_SECTION_EXPLICIT_KEY: u8 = 0x02;
const KRL_SECTION_FINGERPRINT_SHA1: u8 = 0x03;
const KRL_SECTION_SIGNATURE: u8 = 0x04;
const KRL_SECTION_FINGERPRINT_SHA256: u8 = 0x05;
const KRL_SECTION_EXTENSION: u8 = 255;
const KRL_SECTION_CERT_SERIAL_LIST: u8 = 0x20;
const KRL_SECTION_CERT_SERIAL_RANGE: u8 = 0x21;
const KRL_SECTION_CERT_SERIAL_BITMAP: u8 = 0x22;
const KRL_SECTION_CERT_KEY_ID: u8 = 0x23;
const KRL_SECTION_CERT_EXTENSION: u8 = 255;

pub(crate) fn cmd_sign(args: &[String]) -> Result<(), String> {
    let Some(sub) = args.first().map(String::as_str) else {
        print_sign_help();
        return Ok(());
    };
    let rest = &args[1..];
    match sub {
        "help" | "--help" | "-h" => {
            print_sign_help();
            Ok(())
        }
        "keygen" => cmd_keygen(rest),
        "cert" => cmd_cert(rest),
        "file" => cmd_file(rest),
        "krl" => cmd_krl(rest),
        "verify" => cmd_verify(rest),
        other => Err(format!(
            "unknown `sign` subcommand `{other}` (expected `keygen`, `cert`, `file`, `krl`, or `verify`)"
        )),
    }
}

fn print_sign_help() {
    println!("Usage: cargo xtask sign <subcommand> [options]");
    println!();
    println!("Subcommands:");
    println!("  keygen --out <prefix> [--comment <str>]");
    println!(
        "  cert   --ca <ca_priv> --pubkey <signer.pub> --id <key_id> [--principal <name>]... [--serial <u64>] [--valid-after <secs>] [--valid-before <secs>] [--out <cert_path>]"
    );
    println!(
        "  file   --key <signer_priv> [--cert <cert_path>] [--namespace <ns>] [--out <sig_path>] <mod_path>"
    );
    println!(
        "  krl    --out <krl_path> [--version <u64>] [--ca <ca.pub>] [--revoke-serial <u64|min-max>]... [--revoke-key-id <id>]... [--revoke-key <pub_path>]... [--revoke-fingerprint <SHA256:...>]... [--sign-key <priv_path> [--sign-cert <cert_path>]]"
    );
    println!(
        "  verify [--ca <ca.pub>]... [--pubkey <pinned.pub>]... [--krl <krl_path>]... [--namespace <ns>] [--principal <name>] [--sig <sig_path>] <mod_path>"
    );
}

fn take_flag_value<'a>(args: &'a [String], idx: &mut usize, flag: &str) -> Result<&'a str, String> {
    let next_idx = *idx + 1;
    let val = args
        .get(next_idx)
        .ok_or_else(|| format!("missing value for `{flag}`"))?;
    *idx = next_idx;
    Ok(val.as_str())
}

fn cmd_keygen(args: &[String]) -> Result<(), String> {
    let mut out_prefix: Option<PathBuf> = None;
    let mut comment: Option<String> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--out" => {
                out_prefix = Some(PathBuf::from(take_flag_value(args, &mut i, "--out")?));
            }
            "--comment" => {
                comment = Some(take_flag_value(args, &mut i, "--comment")?.to_owned());
            }
            other => return Err(format!("unknown `sign keygen` argument `{other}`")),
        }
        i += 1;
    }

    let priv_path = out_prefix.ok_or_else(|| "missing required `--out <prefix>`".to_owned())?;
    if let Some(parent) = priv_path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create {}: {e}", parent.display()))?;
    }

    let mut key = PrivateKey::random(&mut OsRng, Algorithm::Ed25519)
        .map_err(|e| format!("failed to generate Ed25519 keypair: {e}"))?;
    if let Some(c) = comment {
        key.set_comment(c);
    }

    key.write_openssh_file(&priv_path, LineEnding::LF)
        .map_err(|e| format!("failed to write {}: {e}", priv_path.display()))?;

    let mut pub_os = priv_path.as_os_str().to_os_string();
    pub_os.push(".pub");
    let pub_path = PathBuf::from(pub_os);
    let mut pub_text = key
        .public_key()
        .to_openssh()
        .map_err(|e| format!("failed to encode OpenSSH public key: {e}"))?;
    pub_text.push('\n');
    fs::write(&pub_path, pub_text.as_bytes())
        .map_err(|e| format!("failed to write {}: {e}", pub_path.display()))?;

    let fp = key.public_key().fingerprint(HashAlg::Sha256);
    println!("Generated Ed25519 keypair:");
    println!("  private: {}", priv_path.display());
    println!("  public:  {}", pub_path.display());
    println!("  fingerprint: {fp}");
    Ok(())
}

fn default_cert_out_path(pubkey_path: &Path) -> PathBuf {
    let raw = pubkey_path.to_string_lossy();
    if let Some(stem) = raw.strip_suffix(".pub") {
        PathBuf::from(format!("{stem}-cert.pub"))
    } else {
        let mut os = pubkey_path.as_os_str().to_os_string();
        os.push("-cert.pub");
        PathBuf::from(os)
    }
}

fn cmd_cert(args: &[String]) -> Result<(), String> {
    let mut ca_path: Option<PathBuf> = None;
    let mut pubkey_path: Option<PathBuf> = None;
    let mut key_id: Option<String> = None;
    let mut principals: Vec<String> = Vec::new();
    let mut serial: u64 = 0;
    let mut valid_after: u64 = 0;
    let mut valid_before: u64 = i64::MAX as u64;
    let mut out_path: Option<PathBuf> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--ca" => ca_path = Some(PathBuf::from(take_flag_value(args, &mut i, "--ca")?)),
            "--pubkey" => {
                pubkey_path = Some(PathBuf::from(take_flag_value(args, &mut i, "--pubkey")?));
            }
            "--id" => key_id = Some(take_flag_value(args, &mut i, "--id")?.to_owned()),
            "--principal" => {
                principals.push(take_flag_value(args, &mut i, "--principal")?.to_owned());
            }
            "--serial" => {
                let raw = take_flag_value(args, &mut i, "--serial")?;
                serial = raw
                    .parse::<u64>()
                    .map_err(|e| format!("invalid `--serial` '{raw}': {e}"))?;
            }
            "--valid-after" => {
                let raw = take_flag_value(args, &mut i, "--valid-after")?;
                valid_after = raw
                    .parse::<u64>()
                    .map_err(|e| format!("invalid `--valid-after` '{raw}': {e}"))?;
            }
            "--valid-before" => {
                let raw = take_flag_value(args, &mut i, "--valid-before")?;
                valid_before = raw
                    .parse::<u64>()
                    .map_err(|e| format!("invalid `--valid-before` '{raw}': {e}"))?;
            }
            "--out" => out_path = Some(PathBuf::from(take_flag_value(args, &mut i, "--out")?)),
            other => return Err(format!("unknown `sign cert` argument `{other}`")),
        }
        i += 1;
    }

    let ca_path = ca_path.ok_or_else(|| "missing required `--ca <ca_priv>`".to_owned())?;
    let pubkey_path =
        pubkey_path.ok_or_else(|| "missing required `--pubkey <signer.pub>`".to_owned())?;
    let key_id = key_id.ok_or_else(|| "missing required `--id <key_id>`".to_owned())?;

    let ca_key = PrivateKey::read_openssh_file(&ca_path)
        .map_err(|e| format!("failed to read CA private key {}: {e}", ca_path.display()))?;
    if ca_key.algorithm() != Algorithm::Ed25519 {
        return Err(format!(
            "CA key {} uses unsupported algorithm {}",
            ca_path.display(),
            ca_key.algorithm()
        ));
    }

    let pub_text = fs::read_to_string(&pubkey_path)
        .map_err(|e| format!("failed to read public key {}: {e}", pubkey_path.display()))?;
    let subject_key = PublicKey::from_openssh(pub_text.trim())
        .map_err(|e| format!("invalid OpenSSH public key {}: {e}", pubkey_path.display()))?;
    if subject_key.algorithm() != Algorithm::Ed25519 {
        return Err(format!(
            "subject key {} uses unsupported algorithm {}",
            pubkey_path.display(),
            subject_key.algorithm()
        ));
    }

    let mut builder = CertBuilder::new_with_random_nonce(
        &mut OsRng,
        subject_key.key_data().clone(),
        valid_after,
        valid_before,
    )
    .map_err(|e| format!("invalid certificate validity window: {e}"))?;
    builder
        .cert_type(CertType::User)
        .map_err(|e| format!("failed to set certificate type: {e}"))?;
    builder
        .serial(serial)
        .map_err(|e| format!("failed to set serial: {e}"))?;
    builder
        .key_id(&key_id)
        .map_err(|e| format!("failed to set key ID: {e}"))?;
    if principals.is_empty() {
        builder
            .all_principals_valid()
            .map_err(|e| format!("failed to mark all principals valid: {e}"))?;
    } else {
        for p in &principals {
            builder
                .valid_principal(p)
                .map_err(|e| format!("failed to add principal '{p}': {e}"))?;
        }
    }

    let cert = builder
        .sign(&ca_key)
        .map_err(|e| format!("failed to sign certificate: {e}"))?;

    let cert_out = out_path.unwrap_or_else(|| default_cert_out_path(&pubkey_path));
    if let Some(parent) = cert_out.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create {}: {e}", parent.display()))?;
    }
    let mut encoded = cert
        .to_openssh()
        .map_err(|e| format!("failed to encode OpenSSH certificate: {e}"))?;
    encoded.push('\n');
    fs::write(&cert_out, encoded.as_bytes())
        .map_err(|e| format!("failed to write {}: {e}", cert_out.display()))?;

    println!("Issued OpenSSH Ed25519 certificate:");
    println!("  cert:         {}", cert_out.display());
    println!("  key_id:       {key_id}");
    println!("  serial:       {serial}");
    println!("  principals:   {}", principals.join(", "));
    println!(
        "  subject_fp:   {}",
        subject_key.fingerprint(HashAlg::Sha256)
    );
    println!(
        "  ca_fp:        {}",
        ca_key.public_key().fingerprint(HashAlg::Sha256)
    );
    Ok(())
}

fn default_sig_path(mod_path: &Path) -> PathBuf {
    let mut os = mod_path.as_os_str().to_os_string();
    os.push(".sig");
    PathBuf::from(os)
}

fn is_canonical_ed25519_y(raw: &[u8; 32]) -> bool {
    let mut y_bytes = *raw;
    y_bytes[31] &= 0x7f;
    let limbs = [
        u64::from_le_bytes(y_bytes[0..8].try_into().unwrap()),
        u64::from_le_bytes(y_bytes[8..16].try_into().unwrap()),
        u64::from_le_bytes(y_bytes[16..24].try_into().unwrap()),
        u64::from_le_bytes(y_bytes[24..32].try_into().unwrap()),
    ];
    const P_LIMBS: [u64; 4] = [
        0xffff_ffff_ffff_ffed,
        u64::MAX,
        u64::MAX,
        0x7fff_ffff_ffff_ffff,
    ];
    (limbs[3], limbs[2], limbs[1], limbs[0]) < (P_LIMBS[3], P_LIMBS[2], P_LIMBS[1], P_LIMBS[0])
}

fn validate_strict_ed25519_kd(kd: &KeyData) -> Result<ed25519_dalek::VerifyingKey, String> {
    let raw = match kd {
        KeyData::Ed25519(pk) => &pk.0,
        other => {
            return Err(format!(
                "expected Ed25519 key, found '{}'",
                other.algorithm()
            ));
        }
    };
    if !is_canonical_ed25519_y(raw) {
        return Err("non-canonical compressed Ed25519 public key encoding (y >= p)".to_owned());
    }
    let vk = ed25519_dalek::VerifyingKey::from_bytes(raw)
        .map_err(|e| format!("invalid Curve25519 point: {e}"))?;
    if vk.is_weak() {
        return Err("small-order Curve25519 torsion public key is rejected".to_owned());
    }
    Ok(vk)
}

fn verify_strict_ed25519_sig(kd: &KeyData, msg: &[u8], sig: &Signature) -> Result<(), String> {
    let vk = validate_strict_ed25519_kd(kd)?;
    if sig.algorithm() != Algorithm::Ed25519 {
        return Err(format!(
            "unsupported signature algorithm {}",
            sig.algorithm()
        ));
    }
    let sig_bytes: [u8; 64] = sig
        .as_bytes()
        .try_into()
        .map_err(|_| "invalid Ed25519 signature byte length".to_owned())?;
    let dalek_sig = ed25519_dalek::Signature::from_bytes(&sig_bytes);
    vk.verify_strict(msg, &dalek_sig)
        .map_err(|_| "cryptographic signature verification failed".to_owned())
}

fn sign_bytes_to_pem_file(
    bytes: &[u8],
    namespace: &str,
    key_path: &Path,
    cert_path: Option<&Path>,
    sig_out: &Path,
) -> Result<(Fingerprint, String), String> {
    let signing_key = PrivateKey::read_openssh_file(key_path)
        .map_err(|e| format!("failed to read signing key {}: {e}", key_path.display()))?;
    if signing_key.algorithm() != Algorithm::Ed25519 {
        return Err(format!(
            "signing key {} uses unsupported algorithm {}",
            key_path.display(),
            signing_key.algorithm()
        ));
    }

    let signer = if let Some(cert_p) = cert_path {
        let cert_str = fs::read_to_string(cert_p)
            .map_err(|e| format!("failed to read certificate {}: {e}", cert_p.display()))?;
        let cert_blob = decode_openssh_cert_line(&cert_str)?;
        let parsed = ParsedCertificate::decode(&cert_blob)?;
        if &parsed.public_key != signing_key.public_key().key_data() {
            return Err(format!(
                "signing key {} does not match public key embedded in {}",
                key_path.display(),
                cert_p.display()
            ));
        }
        SshSigSigner::Certificate(cert_blob)
    } else {
        SshSigSigner::PublicKey(signing_key.public_key().key_data().clone())
    };

    let hash_alg = HashAlg::Sha512;
    let digest = hash_alg.digest(bytes);
    let signed_data = encode_signed_data(namespace, &[], hash_alg, &digest)
        .map_err(|e| format!("failed to encode SSHSIG signed payload: {e}"))?;
    let signature = signing_key
        .try_sign(&signed_data)
        .map_err(|e| format!("failed to sign payload: {e}"))?;

    let envelope = RawSshSigEnvelope {
        version: SSHSIG_VERSION,
        signer,
        namespace: namespace.to_owned(),
        reserved: Vec::new(),
        hash_alg,
        signature,
    };
    let pem = envelope
        .encode_pem_string(LineEnding::LF)
        .map_err(|e| format!("failed to encode SSHSIG PEM: {e}"))?;

    if let Some(parent) = sig_out.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create {}: {e}", parent.display()))?;
    }
    fs::write(sig_out, pem.as_bytes())
        .map_err(|e| format!("failed to write {}: {e}", sig_out.display()))?;

    let sha256: [u8; 32] = Sha256::digest(bytes).into();
    let sha_hex: String = sha256.iter().map(|b| format!("{b:02x}")).collect();
    Ok((
        signing_key.public_key().fingerprint(HashAlg::Sha256),
        sha_hex,
    ))
}

fn cmd_file(args: &[String]) -> Result<(), String> {
    let mut key_path: Option<PathBuf> = None;
    let mut cert_path: Option<PathBuf> = None;
    let mut namespace: String = DEFAULT_NAMESPACE.to_owned();
    let mut out_path: Option<PathBuf> = None;
    let mut mod_path: Option<PathBuf> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--key" => key_path = Some(PathBuf::from(take_flag_value(args, &mut i, "--key")?)),
            "--cert" => cert_path = Some(PathBuf::from(take_flag_value(args, &mut i, "--cert")?)),
            "--namespace" => namespace = take_flag_value(args, &mut i, "--namespace")?.to_owned(),
            "--out" => out_path = Some(PathBuf::from(take_flag_value(args, &mut i, "--out")?)),
            flag if flag.starts_with('-') => {
                return Err(format!("unknown `sign file` flag `{flag}`"));
            }
            positional => {
                if mod_path.is_some() {
                    return Err(format!(
                        "unexpected extra positional argument `{positional}`"
                    ));
                }
                mod_path = Some(PathBuf::from(positional));
            }
        }
        i += 1;
    }

    if namespace.is_empty() {
        return Err("`--namespace` must not be empty".to_owned());
    }
    let key_path = key_path.ok_or_else(|| "missing required `--key <signer_priv>`".to_owned())?;
    let mod_path = mod_path.ok_or_else(|| "missing required `<mod_path>`".to_owned())?;

    let binary_bytes = fs::read(&mod_path)
        .map_err(|e| format!("failed to read mod binary {}: {e}", mod_path.display()))?;
    let sig_out = out_path.unwrap_or_else(|| default_sig_path(&mod_path));
    let (fp, sha_hex) = sign_bytes_to_pem_file(
        &binary_bytes,
        &namespace,
        &key_path,
        cert_path.as_deref(),
        &sig_out,
    )?;

    println!("Signed mod library:");
    println!("  binary:    {}", mod_path.display());
    println!("  signature: {}", sig_out.display());
    println!("  namespace: {namespace}");
    println!("  signer_fp: {fp}");
    println!("  sha256:    {sha_hex}");
    Ok(())
}

fn cmd_krl(args: &[String]) -> Result<(), String> {
    let mut out_path: Option<PathBuf> = None;
    let mut krl_version: u64 = 0;
    let mut ca_path: Option<PathBuf> = None;
    let mut revoke_serials: Vec<u64> = Vec::new();
    let mut revoke_ranges: Vec<(u64, u64)> = Vec::new();
    let mut revoke_key_ids: Vec<String> = Vec::new();
    let mut revoke_key_paths: Vec<PathBuf> = Vec::new();
    let mut revoke_fps: Vec<[u8; 32]> = Vec::new();
    let mut sign_key_path: Option<PathBuf> = None;
    let mut sign_cert_path: Option<PathBuf> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--out" => out_path = Some(PathBuf::from(take_flag_value(args, &mut i, "--out")?)),
            "--version" => {
                let raw = take_flag_value(args, &mut i, "--version")?;
                krl_version = raw
                    .parse::<u64>()
                    .map_err(|e| format!("invalid `--version` '{raw}': {e}"))?;
            }
            "--ca" => ca_path = Some(PathBuf::from(take_flag_value(args, &mut i, "--ca")?)),
            "--revoke-serial" => {
                let raw = take_flag_value(args, &mut i, "--revoke-serial")?;
                if let Some((a, b)) = raw.split_once('-') {
                    let min = a
                        .trim()
                        .parse::<u64>()
                        .map_err(|e| format!("invalid serial range start '{a}': {e}"))?;
                    let max = b
                        .trim()
                        .parse::<u64>()
                        .map_err(|e| format!("invalid serial range end '{b}': {e}"))?;
                    if min > max {
                        return Err(format!("inverted serial range: {min} > {max}"));
                    }
                    revoke_ranges.push((min, max));
                } else {
                    let s = raw
                        .trim()
                        .parse::<u64>()
                        .map_err(|e| format!("invalid serial '{raw}': {e}"))?;
                    revoke_serials.push(s);
                }
            }
            "--revoke-key-id" => {
                revoke_key_ids.push(take_flag_value(args, &mut i, "--revoke-key-id")?.to_owned());
            }
            "--revoke-key" => {
                revoke_key_paths.push(PathBuf::from(take_flag_value(
                    args,
                    &mut i,
                    "--revoke-key",
                )?));
            }
            "--revoke-fingerprint" => {
                let raw = take_flag_value(args, &mut i, "--revoke-fingerprint")?.trim();
                let fp: Fingerprint = raw
                    .parse()
                    .map_err(|e: ssh_key::Error| format!("invalid fingerprint '{raw}': {e}"))?;
                let Some(digest) = fp.sha256() else {
                    return Err(format!("only SHA256 fingerprints are supported: '{raw}'"));
                };
                revoke_fps.push(digest);
            }
            "--sign-key" => {
                sign_key_path = Some(PathBuf::from(take_flag_value(args, &mut i, "--sign-key")?));
            }
            "--sign-cert" => {
                sign_cert_path = Some(PathBuf::from(take_flag_value(args, &mut i, "--sign-cert")?));
            }
            flag => return Err(format!("unknown `sign krl` flag `{flag}`")),
        }
        i += 1;
    }

    if sign_cert_path.is_some() && sign_key_path.is_none() {
        return Err("`--sign-cert` requires `--sign-key`".to_owned());
    }
    let out_path = out_path.ok_or_else(|| "missing required `--out <krl_path>`".to_owned())?;
    let ca_key = match ca_path {
        Some(p) => Some(read_ed25519_pubkeys(&[p])?.remove(0)),
        None => None,
    };
    let explicit_keys = read_ed25519_pubkeys(&revoke_key_paths)?;

    let generated_date = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());

    let map_enc = |e: ssh_encoding::Error| format!("failed to encode KRL: {e}");
    let mut krl_bytes = Vec::new();
    krl_bytes.extend_from_slice(SSHKRL_MAGIC);
    SSHKRL_VERSION.encode(&mut krl_bytes).map_err(map_enc)?;
    krl_version.encode(&mut krl_bytes).map_err(map_enc)?;
    generated_date.encode(&mut krl_bytes).map_err(map_enc)?;
    0u64.encode(&mut krl_bytes).map_err(map_enc)?;
    ([] as [u8; 0])
        .as_slice()
        .encode(&mut krl_bytes)
        .map_err(map_enc)?;
    "".encode(&mut krl_bytes).map_err(map_enc)?;

    if !revoke_serials.is_empty() || !revoke_ranges.is_empty() || !revoke_key_ids.is_empty() {
        let mut sec = Vec::new();
        match &ca_key {
            None => ([] as [u8; 0])
                .as_slice()
                .encode(&mut sec)
                .map_err(map_enc)?,
            Some(pk) => {
                let mut kd_buf = Vec::new();
                pk.key_data().encode(&mut kd_buf).map_err(map_enc)?;
                kd_buf.encode(&mut sec).map_err(map_enc)?;
            }
        }
        ([] as [u8; 0])
            .as_slice()
            .encode(&mut sec)
            .map_err(map_enc)?;

        if !revoke_serials.is_empty() {
            KRL_SECTION_CERT_SERIAL_LIST
                .encode(&mut sec)
                .map_err(map_enc)?;
            let mut sub = Vec::with_capacity(revoke_serials.len() * 8);
            for s in &revoke_serials {
                s.encode(&mut sub).map_err(map_enc)?;
            }
            sub.encode(&mut sec).map_err(map_enc)?;
        }
        for &(min, max) in &revoke_ranges {
            KRL_SECTION_CERT_SERIAL_RANGE
                .encode(&mut sec)
                .map_err(map_enc)?;
            let mut sub = Vec::with_capacity(16);
            min.encode(&mut sub).map_err(map_enc)?;
            max.encode(&mut sub).map_err(map_enc)?;
            sub.encode(&mut sec).map_err(map_enc)?;
        }
        if !revoke_key_ids.is_empty() {
            KRL_SECTION_CERT_KEY_ID.encode(&mut sec).map_err(map_enc)?;
            let mut sub = Vec::new();
            for kid in &revoke_key_ids {
                kid.encode(&mut sub).map_err(map_enc)?;
            }
            sub.encode(&mut sec).map_err(map_enc)?;
        }

        KRL_SECTION_CERTIFICATES
            .encode(&mut krl_bytes)
            .map_err(map_enc)?;
        sec.encode(&mut krl_bytes).map_err(map_enc)?;
    }

    if !explicit_keys.is_empty() {
        let mut sec = Vec::new();
        for pk in &explicit_keys {
            let mut kd_buf = Vec::new();
            pk.key_data().encode(&mut kd_buf).map_err(map_enc)?;
            kd_buf.encode(&mut sec).map_err(map_enc)?;
        }
        KRL_SECTION_EXPLICIT_KEY
            .encode(&mut krl_bytes)
            .map_err(map_enc)?;
        sec.encode(&mut krl_bytes).map_err(map_enc)?;
    }

    if !revoke_fps.is_empty() {
        let mut sec = Vec::new();
        for digest in &revoke_fps {
            digest.as_slice().encode(&mut sec).map_err(map_enc)?;
        }
        KRL_SECTION_FINGERPRINT_SHA256
            .encode(&mut krl_bytes)
            .map_err(map_enc)?;
        sec.encode(&mut krl_bytes).map_err(map_enc)?;
    }

    if let Some(parent) = out_path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create {}: {e}", parent.display()))?;
    }
    fs::write(&out_path, &krl_bytes)
        .map_err(|e| format!("failed to write {}: {e}", out_path.display()))?;

    println!("Generated OpenSSH KRL:");
    println!("  krl:     {}", out_path.display());
    println!("  version: {krl_version}");

    if let Some(sign_key) = sign_key_path {
        let sig_out = default_sig_path(&out_path);
        let (fp, _) = sign_bytes_to_pem_file(
            &krl_bytes,
            DEFAULT_KRL_NAMESPACE,
            &sign_key,
            sign_cert_path.as_deref(),
            &sig_out,
        )?;
        println!(
            "  sig:     {} (namespace '{DEFAULT_KRL_NAMESPACE}', signer {fp})",
            sig_out.display()
        );
    }
    Ok(())
}

fn cmd_verify(args: &[String]) -> Result<(), String> {
    let mut ca_paths: Vec<PathBuf> = Vec::new();
    let mut pubkey_paths: Vec<PathBuf> = Vec::new();
    let mut krl_paths: Vec<PathBuf> = Vec::new();
    let mut namespace = DEFAULT_NAMESPACE.to_owned();
    let mut required_principal: Option<String> = None;
    let mut sig_path: Option<PathBuf> = None;
    let mut mod_path: Option<PathBuf> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--ca" => ca_paths.push(PathBuf::from(take_flag_value(args, &mut i, "--ca")?)),
            "--pubkey" => {
                pubkey_paths.push(PathBuf::from(take_flag_value(args, &mut i, "--pubkey")?));
            }
            "--krl" => krl_paths.push(PathBuf::from(take_flag_value(args, &mut i, "--krl")?)),
            "--namespace" => namespace = take_flag_value(args, &mut i, "--namespace")?.to_owned(),
            "--principal" => {
                required_principal = Some(take_flag_value(args, &mut i, "--principal")?.to_owned());
            }
            "--sig" => sig_path = Some(PathBuf::from(take_flag_value(args, &mut i, "--sig")?)),
            flag if flag.starts_with('-') => {
                return Err(format!("unknown `sign verify` flag `{flag}`"));
            }
            positional => {
                if mod_path.is_some() {
                    return Err(format!(
                        "unexpected extra positional argument `{positional}`"
                    ));
                }
                mod_path = Some(PathBuf::from(positional));
            }
        }
        i += 1;
    }

    if ca_paths.is_empty() && pubkey_paths.is_empty() {
        return Err(
            "at least one `--ca <ca.pub>` or `--pubkey <pinned.pub>` is required".to_owned(),
        );
    }
    let mod_path = mod_path.ok_or_else(|| "missing required `<mod_path>`".to_owned())?;
    let sig_path = sig_path.unwrap_or_else(|| default_sig_path(&mod_path));

    let trusted_cas = read_ed25519_pubkeys(&ca_paths)?;
    let trusted_keys = read_ed25519_pubkeys(&pubkey_paths)?;
    let mut krls = Vec::with_capacity(krl_paths.len());
    for kp in &krl_paths {
        let bytes =
            fs::read(kp).map_err(|e| format!("failed to read KRL {}: {e}", kp.display()))?;
        krls.push(ParsedKrl::decode(&bytes)?);
    }

    let binary_bytes = fs::read(&mod_path)
        .map_err(|e| format!("failed to read mod binary {}: {e}", mod_path.display()))?;
    if binary_bytes.is_empty() {
        return Err(format!(
            "mod binary {} is empty (0 bytes)",
            mod_path.display()
        ));
    }
    let sig_bytes = fs::read(&sig_path)
        .map_err(|e| format!("failed to read signature {}: {e}", sig_path.display()))?;

    let envelope = RawSshSigEnvelope::decode_pem(&sig_bytes)
        .map_err(|e| format!("failed to parse {}: {e}", sig_path.display()))?;
    if envelope.namespace != namespace {
        return Err(format!(
            "namespace mismatch: expected '{namespace}', found '{}'",
            envelope.namespace
        ));
    }
    if envelope.signature.algorithm() != Algorithm::Ed25519 {
        return Err(format!(
            "unsupported signature algorithm {}",
            envelope.signature.algorithm()
        ));
    }

    let (verify_key, ca_fp_opt, key_id_opt, principals, serial_opt) = match &envelope.signer {
        SshSigSigner::Certificate(cert_blob) => {
            let cert = ParsedCertificate::decode(cert_blob)?;
            if cert.cert_type != CertType::User {
                return Err(format!("expected User cert, found {:?}", cert.cert_type));
            }
            if cert.public_key.algorithm() != Algorithm::Ed25519
                || cert.signature_key.algorithm() != Algorithm::Ed25519
            {
                return Err("certificate keys must use Ed25519".to_owned());
            }
            validate_strict_ed25519_kd(&cert.signature_key)?;
            validate_strict_ed25519_kd(&cert.public_key)?;
            if !cert.critical_options.is_empty() {
                return Err(format!(
                    "certificate has unrecognized critical options: {}",
                    cert.critical_options
                        .keys()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            let ca_fp = cert.signature_key.fingerprint(HashAlg::Sha256);
            let signer_fp = cert.public_key.fingerprint(HashAlg::Sha256);
            if !trusted_cas
                .iter()
                .any(|ca| ca.key_data() == &cert.signature_key)
            {
                return Err(format!("untrusted CA fingerprint {ca_fp}"));
            }
            verify_strict_ed25519_sig(&cert.signature_key, cert.tbs_bytes, &cert.signature)
                .map_err(|_| {
                    format!("CA signature on certificate failed verification ({ca_fp})")
                })?;

            for krl in &krls {
                krl.check_key(&ca_fp)?;
                krl.check_key(&signer_fp)?;
                krl.check_cert(&ca_fp, cert.serial, &cert.key_id)?;
            }

            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|_| "system clock precedes UNIX_EPOCH".to_owned())?
                .as_secs();
            if !(cert.valid_after <= now && now < cert.valid_before) {
                return Err(format!(
                    "certificate not valid at {now} (valid [{}, {}))",
                    cert.valid_after, cert.valid_before
                ));
            }
            if let Some(expected) = &required_principal
                && !cert.valid_principals.iter().any(|p| p == expected)
            {
                return Err(format!(
                    "principal '{expected}' not authorized by certificate [{}]",
                    cert.valid_principals.join(", ")
                ));
            }
            (
                cert.public_key,
                Some(ca_fp.to_string()),
                Some(cert.key_id),
                cert.valid_principals,
                Some(cert.serial),
            )
        }
        SshSigSigner::PublicKey(kd) => {
            if kd.algorithm() != Algorithm::Ed25519 {
                return Err(format!("unsupported key algorithm {}", kd.algorithm()));
            }
            validate_strict_ed25519_kd(kd)?;
            let signer_fp = kd.fingerprint(HashAlg::Sha256);
            for krl in &krls {
                krl.check_key(&signer_fp)?;
            }
            if !trusted_keys.iter().any(|k| k.key_data() == kd) {
                return Err(format!("untrusted signing key {signer_fp}"));
            }
            if let Some(expected) = &required_principal {
                return Err(format!(
                    "principal '{expected}' required, but signature used an uncertified key"
                ));
            }
            (kd.clone(), None, None, Vec::new(), None)
        }
    };

    let digest = envelope.hash_alg.digest(&binary_bytes);
    let signed_data = encode_signed_data(
        &envelope.namespace,
        &envelope.reserved,
        envelope.hash_alg,
        &digest,
    )
    .map_err(|e| format!("failed to encode signed payload: {e}"))?;
    verify_strict_ed25519_sig(&verify_key, &signed_data, &envelope.signature)?;

    let sha256: [u8; 32] = Sha256::digest(&binary_bytes).into();
    let sha_hex: String = sha256.iter().map(|b| format!("{b:02x}")).collect();

    println!("Verified mod signature:");
    println!("  signer_fp:  {}", verify_key.fingerprint(HashAlg::Sha256));
    if let Some(ca_fp) = ca_fp_opt {
        println!("  ca_fp:      {ca_fp}");
    }
    if let Some(id) = key_id_opt {
        println!("  key_id:     {id}");
    }
    if !principals.is_empty() {
        println!("  principals: {}", principals.join(", "));
    }
    if let Some(serial) = serial_opt {
        println!("  serial:     {serial}");
    }
    println!("  namespace:  {}", envelope.namespace);
    println!("  sha256:     {sha_hex}");
    Ok(())
}

fn read_ed25519_pubkeys(paths: &[PathBuf]) -> Result<Vec<PublicKey>, String> {
    let mut keys = Vec::with_capacity(paths.len());
    for p in paths {
        let text = fs::read_to_string(p)
            .map_err(|e| format!("failed to read public key {}: {e}", p.display()))?;
        let key = PublicKey::from_openssh(text.trim())
            .map_err(|e| format!("invalid OpenSSH public key {}: {e}", p.display()))?;
        if key.algorithm() != Algorithm::Ed25519 {
            return Err(format!(
                "key {} uses unsupported algorithm {}",
                p.display(),
                key.algorithm()
            ));
        }
        validate_strict_ed25519_kd(key.key_data())
            .map_err(|detail| format!("key {} rejected: {detail}", p.display()))?;
        keys.push(key);
    }
    Ok(keys)
}

fn decode_openssh_cert_line(line: &str) -> Result<Vec<u8>, String> {
    let mut parts = line.split_whitespace();
    let Some(alg_id) = parts.next() else {
        return Err("empty OpenSSH certificate file".to_owned());
    };
    if alg_id != ED25519_CERT_ALG_ID {
        return Err(format!("unsupported certificate algorithm '{alg_id}'"));
    }
    let Some(b64) = parts.next() else {
        return Err("missing base64 certificate payload".to_owned());
    };
    let mut reader = ssh_encoding::Base64Reader::new(b64.as_bytes())
        .map_err(|e| format!("invalid certificate base64: {e}"))?;
    let mut out = vec![0u8; reader.remaining_len()];
    reader
        .read(&mut out)
        .map_err(|e| format!("failed to decode certificate base64: {e}"))?;
    reader
        .finish(())
        .map_err(|e| format!("trailing data in certificate base64: {e}"))?;
    Ok(out)
}

enum SshSigSigner {
    PublicKey(KeyData),
    Certificate(Vec<u8>),
}

impl SshSigSigner {
    fn encoded_len_inner(&self) -> ssh_encoding::Result<usize> {
        match self {
            Self::PublicKey(kd) => kd.encoded_len(),
            Self::Certificate(blob) => Ok(blob.len()),
        }
    }

    fn encode_inner(&self, writer: &mut impl Writer) -> ssh_encoding::Result<()> {
        match self {
            Self::PublicKey(kd) => kd.encode(writer),
            Self::Certificate(blob) => writer.write(blob),
        }
    }
}

struct RawSshSigEnvelope {
    version: u32,
    signer: SshSigSigner,
    namespace: String,
    reserved: Vec<u8>,
    hash_alg: HashAlg,
    signature: Signature,
}

impl PemLabel for RawSshSigEnvelope {
    const PEM_LABEL: &'static str = "SSH SIGNATURE";
}

#[derive(Debug)]
struct WireDecodeError(String);

impl std::fmt::Display for WireDecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<ssh_encoding::Error> for WireDecodeError {
    fn from(err: ssh_encoding::Error) -> Self {
        Self(err.to_string())
    }
}

impl From<ssh_key::Error> for WireDecodeError {
    fn from(err: ssh_key::Error) -> Self {
        Self(err.to_string())
    }
}

impl Decode for RawSshSigEnvelope {
    type Error = WireDecodeError;

    fn decode(reader: &mut impl Reader) -> Result<Self, Self::Error> {
        let mut magic = [0u8; SSHSIG_MAGIC.len()];
        reader.read(&mut magic)?;
        if magic != SSHSIG_MAGIC {
            return Err(WireDecodeError("missing SSHSIG magic preamble".to_owned()));
        }
        let version = u32::decode(reader)?;
        if version != SSHSIG_VERSION {
            return Err(WireDecodeError(format!(
                "unsupported SSHSIG version {version}"
            )));
        }
        let pk_blob = Vec::<u8>::decode(reader)?;
        let mut peek = pk_blob.as_slice();
        let alg = String::decode(&mut peek)?;
        let signer = if alg.ends_with("-cert-v01@openssh.com") {
            if alg != ED25519_CERT_ALG_ID {
                return Err(WireDecodeError(format!(
                    "unsupported certificate algorithm '{alg}'"
                )));
            }
            SshSigSigner::Certificate(pk_blob)
        } else {
            let mut pk_reader = pk_blob.as_slice();
            let kd = KeyData::decode(&mut pk_reader)?;
            pk_reader.finish(())?;
            if kd.algorithm() != Algorithm::Ed25519 {
                return Err(WireDecodeError(format!(
                    "unsupported key algorithm '{}'",
                    kd.algorithm()
                )));
            }
            SshSigSigner::PublicKey(kd)
        };
        let namespace = String::decode(reader)?;
        if namespace.is_empty() {
            return Err(WireDecodeError(
                "SSHSIG namespace must not be empty".to_owned(),
            ));
        }
        let reserved = Vec::<u8>::decode(reader)?;
        let hash_alg = HashAlg::decode(reader)?;
        let signature = reader.read_prefixed(|r| {
            let sig = Signature::decode(r)?;
            if !r.is_finished() {
                return Err(ssh_key::Error::from(ssh_encoding::Error::TrailingData {
                    remaining: r.remaining_len(),
                }));
            }
            Ok::<_, ssh_key::Error>(sig)
        })?;

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

impl Encode for RawSshSigEnvelope {
    fn encoded_len(&self) -> ssh_encoding::Result<usize> {
        [
            SSHSIG_MAGIC.len(),
            self.version.encoded_len()?,
            4,
            self.signer.encoded_len_inner()?,
            self.namespace.encoded_len()?,
            self.reserved.encoded_len()?,
            self.hash_alg.encoded_len()?,
            self.signature.encoded_len_prefixed()?,
        ]
        .checked_sum()
    }

    fn encode(&self, writer: &mut impl Writer) -> ssh_encoding::Result<()> {
        writer.write(SSHSIG_MAGIC)?;
        self.version.encode(writer)?;
        self.signer.encoded_len_inner()?.encode(writer)?;
        self.signer.encode_inner(writer)?;
        self.namespace.encode(writer)?;
        self.reserved.encode(writer)?;
        self.hash_alg.encode(writer)?;
        self.signature.encode_prefixed(writer)?;
        Ok(())
    }
}

struct ParsedCertificate<'a> {
    public_key: KeyData,
    serial: u64,
    cert_type: CertType,
    key_id: String,
    valid_principals: Vec<String>,
    valid_after: u64,
    valid_before: u64,
    critical_options: OptionsMap,
    signature_key: KeyData,
    signature: Signature,
    tbs_bytes: &'a [u8],
}

impl<'a> ParsedCertificate<'a> {
    fn decode(cert_bytes: &'a [u8]) -> Result<Self, String> {
        let mut reader = cert_bytes;
        let cert_alg = String::decode(&mut reader).map_err(|e| e.to_string())?;
        if cert_alg != ED25519_CERT_ALG_ID {
            return Err(format!("unsupported certificate algorithm '{cert_alg}'"));
        }
        let nonce = Vec::<u8>::decode(&mut reader).map_err(|e| e.to_string())?;
        if nonce.is_empty() {
            return Err("certificate nonce must not be empty".to_owned());
        }
        let ed_pk = Ed25519PublicKey::decode(&mut reader).map_err(|e| e.to_string())?;
        let public_key = KeyData::Ed25519(ed_pk);
        let serial = u64::decode(&mut reader).map_err(|e| e.to_string())?;
        let cert_type = CertType::decode(&mut reader).map_err(|e| e.to_string())?;
        let key_id = String::decode(&mut reader).map_err(|e| e.to_string())?;
        let valid_principals = Vec::<String>::decode(&mut reader).map_err(|e| e.to_string())?;
        let valid_after = u64::decode(&mut reader).map_err(|e| e.to_string())?;
        let valid_before = u64::decode(&mut reader).map_err(|e| e.to_string())?;
        let critical_options = OptionsMap::decode(&mut reader).map_err(|e| e.to_string())?;
        let _extensions = OptionsMap::decode(&mut reader).map_err(|e| e.to_string())?;
        let _reserved = Vec::<u8>::decode(&mut reader).map_err(|e| e.to_string())?;
        let signature_key = reader
            .read_prefixed(|r| {
                let key = KeyData::decode(r)?;
                if !r.is_finished() {
                    return Err(ssh_key::Error::from(ssh_encoding::Error::TrailingData {
                        remaining: r.remaining_len(),
                    }));
                }
                Ok::<_, ssh_key::Error>(key)
            })
            .map_err(|e| e.to_string())?;
        let tbs_len = cert_bytes.len().saturating_sub(reader.remaining_len());
        let tbs_bytes = &cert_bytes[..tbs_len];
        let signature = reader
            .read_prefixed(|r| {
                let sig = Signature::decode(r)?;
                if !r.is_finished() {
                    return Err(ssh_key::Error::from(ssh_encoding::Error::TrailingData {
                        remaining: r.remaining_len(),
                    }));
                }
                Ok::<_, ssh_key::Error>(sig)
            })
            .map_err(|e| e.to_string())?;
        reader.finish(()).map_err(|e| e.to_string())?;

        Ok(Self {
            public_key,
            serial,
            cert_type,
            key_id,
            valid_principals,
            valid_after,
            valid_before,
            critical_options,
            signature_key,
            signature,
            tbs_bytes,
        })
    }
}

struct SignedData<'a> {
    namespace: &'a str,
    reserved: &'a [u8],
    hash_alg: HashAlg,
    hash: &'a [u8],
}

impl Encode for SignedData<'_> {
    fn encoded_len(&self) -> ssh_encoding::Result<usize> {
        [
            SSHSIG_MAGIC.len(),
            self.namespace.encoded_len()?,
            self.reserved.encoded_len()?,
            self.hash_alg.encoded_len()?,
            self.hash.encoded_len()?,
        ]
        .checked_sum()
    }

    fn encode(&self, writer: &mut impl Writer) -> ssh_encoding::Result<()> {
        writer.write(SSHSIG_MAGIC)?;
        self.namespace.encode(writer)?;
        self.reserved.encode(writer)?;
        self.hash_alg.encode(writer)?;
        self.hash.encode(writer)?;
        Ok(())
    }
}

fn encode_signed_data(
    namespace: &str,
    reserved: &[u8],
    hash_alg: HashAlg,
    hash: &[u8],
) -> ssh_encoding::Result<Vec<u8>> {
    let data = SignedData {
        namespace,
        reserved,
        hash_alg,
        hash,
    };
    let mut out = Vec::with_capacity(data.encoded_len()?);
    data.encode(&mut out)?;
    Ok(out)
}

#[derive(Default)]
struct CertRevocationScope {
    serials: HashSet<u64>,
    ranges: Vec<(u64, u64)>,
    bitmaps: Vec<(u64, Vec<u8>)>,
    key_ids: HashSet<String>,
}

impl CertRevocationScope {
    fn is_revoked(&self, serial: u64, key_id: &str) -> Result<(), String> {
        if self.serials.contains(&serial)
            || self.ranges.iter().any(|&(a, b)| a <= serial && serial <= b)
            || self.bitmaps.iter().any(|(off, be)| {
                let Some(delta) = serial.checked_sub(*off) else {
                    return false;
                };
                let Ok(byte_rev) = usize::try_from(delta / 8) else {
                    return false;
                };
                byte_rev < be.len() && ((be[be.len() - 1 - byte_rev] >> (delta % 8)) & 1) != 0
            })
        {
            return Err(format!(
                "certificate serial {serial} has been revoked by KRL"
            ));
        }
        if self.key_ids.contains(key_id) {
            return Err(format!(
                "certificate key_id '{key_id}' has been revoked by KRL"
            ));
        }
        Ok(())
    }

    fn merge(&mut self, other: Self) {
        self.serials.extend(other.serials);
        self.ranges.extend(other.ranges);
        self.bitmaps.extend(other.bitmaps);
        self.key_ids.extend(other.key_ids);
    }
}

struct ParsedKrl {
    revoked_keys: BTreeSet<Fingerprint>,
    global_certs: CertRevocationScope,
    scoped_certs: BTreeMap<Fingerprint, CertRevocationScope>,
}

impl ParsedKrl {
    fn decode(bytes: &[u8]) -> Result<Self, String> {
        let mut reader = bytes;
        let mut magic = [0u8; SSHKRL_MAGIC.len()];
        Reader::read(&mut reader, &mut magic).map_err(|e| format!("invalid KRL header: {e}"))?;
        if magic != *SSHKRL_MAGIC {
            return Err("missing SSHKRL magic header".to_owned());
        }
        let fmt_ver = u32::decode(&mut reader).map_err(|e| e.to_string())?;
        if fmt_ver != SSHKRL_VERSION {
            return Err(format!("unsupported KRL format version {fmt_ver}"));
        }
        let _krl_version = u64::decode(&mut reader).map_err(|e| e.to_string())?;
        let _generated = u64::decode(&mut reader).map_err(|e| e.to_string())?;
        let flags = u64::decode(&mut reader).map_err(|e| e.to_string())?;
        if flags != 0 {
            return Err(format!("unsupported non-zero KRL flags {flags}"));
        }
        let _reserved = Vec::<u8>::decode(&mut reader).map_err(|e| e.to_string())?;
        let _comment = Vec::<u8>::decode(&mut reader).map_err(|e| e.to_string())?;

        let mut revoked_keys = BTreeSet::new();
        let mut global_certs = CertRevocationScope::default();
        let mut scoped_certs: BTreeMap<Fingerprint, CertRevocationScope> = BTreeMap::new();

        while !reader.is_finished() {
            let sec_type = u8::decode(&mut reader).map_err(|e| e.to_string())?;
            let sec_data = Vec::<u8>::decode(&mut reader).map_err(|e| e.to_string())?;
            let mut sec_r = sec_data.as_slice();
            match sec_type {
                KRL_SECTION_CERTIFICATES => {
                    let ca_blob = Vec::<u8>::decode(&mut sec_r).map_err(|e| e.to_string())?;
                    let ca_fp = if ca_blob.is_empty() {
                        None
                    } else {
                        let mut cr = ca_blob.as_slice();
                        let kd = KeyData::decode(&mut cr).map_err(|e| e.to_string())?;
                        cr.finish(()).map_err(|e| e.to_string())?;
                        Some(kd.fingerprint(HashAlg::Sha256))
                    };
                    let _res = Vec::<u8>::decode(&mut sec_r).map_err(|e| e.to_string())?;
                    let mut scope = CertRevocationScope::default();
                    while !sec_r.is_finished() {
                        let sub_t = u8::decode(&mut sec_r).map_err(|e| e.to_string())?;
                        let sub_d = Vec::<u8>::decode(&mut sec_r).map_err(|e| e.to_string())?;
                        let mut sr = sub_d.as_slice();
                        match sub_t {
                            KRL_SECTION_CERT_SERIAL_LIST => {
                                while !sr.is_finished() {
                                    scope
                                        .serials
                                        .insert(u64::decode(&mut sr).map_err(|e| e.to_string())?);
                                }
                            }
                            KRL_SECTION_CERT_SERIAL_RANGE => {
                                let min = u64::decode(&mut sr).map_err(|e| e.to_string())?;
                                let max = u64::decode(&mut sr).map_err(|e| e.to_string())?;
                                if min > max {
                                    return Err(format!("inverted KRL serial range {min} > {max}"));
                                }
                                scope.ranges.push((min, max));
                            }
                            KRL_SECTION_CERT_SERIAL_BITMAP => {
                                let off = u64::decode(&mut sr).map_err(|e| e.to_string())?;
                                let mp = Vec::<u8>::decode(&mut sr).map_err(|e| e.to_string())?;
                                if !mp.is_empty() && (mp[0] & 0x80) != 0 {
                                    return Err("negative KRL serial bitmap".to_owned());
                                }
                                scope.bitmaps.push((off, mp));
                            }
                            KRL_SECTION_CERT_KEY_ID => {
                                while !sr.is_finished() {
                                    scope.key_ids.insert(
                                        String::decode(&mut sr).map_err(|e| e.to_string())?,
                                    );
                                }
                            }
                            KRL_SECTION_CERT_EXTENSION => {
                                let name = String::decode(&mut sr).map_err(|e| e.to_string())?;
                                let crit = u8::decode(&mut sr).map_err(|e| e.to_string())?;
                                let _v = Vec::<u8>::decode(&mut sr).map_err(|e| e.to_string())?;
                                if crit != 0 {
                                    return Err(format!("critical KRL cert extension '{name}'"));
                                }
                            }
                            other => {
                                return Err(format!("unknown KRL cert sub-section 0x{other:02x}"));
                            }
                        }
                        sr.finish(()).map_err(|e| e.to_string())?;
                    }
                    match ca_fp {
                        Some(fp) => scoped_certs.entry(fp).or_default().merge(scope),
                        None => global_certs.merge(scope),
                    }
                }
                KRL_SECTION_EXPLICIT_KEY => {
                    while !sec_r.is_finished() {
                        let kb = Vec::<u8>::decode(&mut sec_r).map_err(|e| e.to_string())?;
                        let mut kr = kb.as_slice();
                        let kd = KeyData::decode(&mut kr).map_err(|e| e.to_string())?;
                        kr.finish(()).map_err(|e| e.to_string())?;
                        revoked_keys.insert(kd.fingerprint(HashAlg::Sha256));
                    }
                }
                KRL_SECTION_FINGERPRINT_SHA256 => {
                    while !sec_r.is_finished() {
                        let dg = Vec::<u8>::decode(&mut sec_r).map_err(|e| e.to_string())?;
                        let arr: [u8; 32] = dg
                            .try_into()
                            .map_err(|_| "invalid SHA256 KRL digest length".to_owned())?;
                        revoked_keys.insert(Fingerprint::Sha256(arr));
                    }
                }
                KRL_SECTION_FINGERPRINT_SHA1 => {
                    return Err("deprecated SHA-1 KRL section (0x03) rejected".to_owned());
                }
                KRL_SECTION_SIGNATURE => {
                    return Err(
                        "deprecated inline KRL signature section (0x04) rejected".to_owned()
                    );
                }
                KRL_SECTION_EXTENSION => {
                    let name = String::decode(&mut sec_r).map_err(|e| e.to_string())?;
                    let crit = u8::decode(&mut sec_r).map_err(|e| e.to_string())?;
                    let _v = Vec::<u8>::decode(&mut sec_r).map_err(|e| e.to_string())?;
                    if crit != 0 {
                        return Err(format!("critical KRL extension '{name}'"));
                    }
                }
                other => return Err(format!("unknown KRL section 0x{other:02x}")),
            }
            sec_r.finish(()).map_err(|e| e.to_string())?;
        }

        Ok(Self {
            revoked_keys,
            global_certs,
            scoped_certs,
        })
    }

    fn check_key(&self, fp: &Fingerprint) -> Result<(), String> {
        if self.revoked_keys.contains(fp) {
            return Err(format!("key '{fp}' has been revoked by KRL"));
        }
        Ok(())
    }

    fn check_cert(&self, ca_fp: &Fingerprint, serial: u64, key_id: &str) -> Result<(), String> {
        self.global_certs.is_revoked(serial, key_id)?;
        if let Some(scope) = self.scoped_certs.get(ca_fp) {
            scope.is_revoked(serial, key_id)?;
        }
        Ok(())
    }
}
