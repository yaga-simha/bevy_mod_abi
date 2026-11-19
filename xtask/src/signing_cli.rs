//! Pure-Rust OpenSSH `SSHSIG` and Ed25519 Certificate Authority CLI (`cargo xtask sign`),
//! delegated to [`bevy_abi_mod_trust`].

use bevy_abi_mod_trust::{
    DEFAULT_KRL_SIGNATURE_NAMESPACE, DEFAULT_SIGNATURE_NAMESPACE, KrlBuilder, ModTrustPolicy,
    default_signature_path_for, issue_ed25519_certificate_openssh, sign_mod_bytes_with_key,
    sign_mod_bytes_with_openssh_certificate,
    ssh_key::{
        Algorithm, Fingerprint, HashAlg, PrivateKey, PublicKey,
        sha2::{Digest, Sha256},
    },
    write_openssh_keypair,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

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
    let generated = write_openssh_keypair(&priv_path, comment.as_deref())
        .map_err(|e| format!("failed to generate Ed25519 keypair: {e}"))?;

    println!("Generated Ed25519 keypair:");
    println!("  private: {}", generated.private_key.display());
    println!("  public:  {}", generated.public_key.display());
    println!("  fingerprint: {}", generated.fingerprint);
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
    let pub_text = fs::read_to_string(&pubkey_path)
        .map_err(|e| format!("failed to read public key {}: {e}", pubkey_path.display()))?;
    let subject_key = PublicKey::from_openssh(pub_text.trim())
        .map_err(|e| format!("invalid OpenSSH public key {}: {e}", pubkey_path.display()))?;

    let issued = issue_ed25519_certificate_openssh(
        &ca_key,
        &subject_key,
        serial,
        &key_id,
        &principals,
        valid_after,
        valid_before,
    )
    .map_err(|e| e.to_string())?;

    let cert_out = out_path.unwrap_or_else(|| default_cert_out_path(&pubkey_path));
    if let Some(parent) = cert_out.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create {}: {e}", parent.display()))?;
    }
    let mut encoded = issued.certificate_openssh;
    encoded.push('\n');
    fs::write(&cert_out, encoded.as_bytes())
        .map_err(|e| format!("failed to write {}: {e}", cert_out.display()))?;

    println!("Issued OpenSSH Ed25519 certificate:");
    println!("  cert:         {}", cert_out.display());
    println!("  key_id:       {key_id}");
    println!("  serial:       {serial}");
    println!("  principals:   {}", principals.join(", "));
    println!("  subject_fp:   {}", issued.subject_fingerprint);
    println!("  ca_fp:        {}", issued.ca_fingerprint);
    Ok(())
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
    let pem = if let Some(cert_p) = cert_path {
        let cert_str = fs::read_to_string(cert_p)
            .map_err(|e| format!("failed to read certificate {}: {e}", cert_p.display()))?;
        sign_mod_bytes_with_openssh_certificate(&signing_key, cert_str.trim(), namespace, bytes)
            .map_err(|e| e.to_string())?
    } else {
        sign_mod_bytes_with_key(&signing_key, namespace, bytes).map_err(|e| e.to_string())?
    };

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
    let mut namespace: String = DEFAULT_SIGNATURE_NAMESPACE.to_owned();
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
    let sig_out = out_path.unwrap_or_else(|| default_signature_path_for(&mod_path));
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
    let mut revoke_fps: Vec<String> = Vec::new();
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
                revoke_fps.push(
                    take_flag_value(args, &mut i, "--revoke-fingerprint")?
                        .trim()
                        .to_owned(),
                );
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
        Some(p) => {
            let text = fs::read_to_string(&p)
                .map_err(|e| format!("failed to read public key {}: {e}", p.display()))?;
            let pk = PublicKey::from_openssh(text.trim())
                .map_err(|e| format!("invalid OpenSSH public key {}: {e}", p.display()))?;
            if pk.algorithm() != Algorithm::Ed25519 {
                return Err(format!("key {} uses unsupported algorithm", p.display()));
            }
            Some(pk)
        }
        None => None,
    };

    let generated_date = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());

    let mut builder = KrlBuilder::new()
        .version(krl_version)
        .generated_date(generated_date);
    for serial in revoke_serials {
        builder = builder.revoke_cert_serial(ca_key.as_ref(), serial);
    }
    for (min, max) in revoke_ranges {
        builder = builder
            .revoke_cert_serial_range(ca_key.as_ref(), min, max)
            .map_err(|e| e.to_string())?;
    }
    for kid in revoke_key_ids {
        builder = builder.revoke_cert_key_id(ca_key.as_ref(), kid);
    }
    for key_path in revoke_key_paths {
        let text = fs::read_to_string(&key_path)
            .map_err(|e| format!("failed to read public key {}: {e}", key_path.display()))?;
        builder = builder
            .revoke_key_openssh(text.trim())
            .map_err(|e| e.to_string())?;
    }
    for fp in revoke_fps {
        builder = builder.revoke_fingerprint(&fp).map_err(|e| e.to_string())?;
    }

    let krl_bytes = builder.to_bytes().map_err(|e| e.to_string())?;

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
        let sig_out = default_signature_path_for(&out_path);
        let (fp, _) = sign_bytes_to_pem_file(
            &krl_bytes,
            DEFAULT_KRL_SIGNATURE_NAMESPACE,
            &sign_key,
            sign_cert_path.as_deref(),
            &sig_out,
        )?;
        println!(
            "  sig:     {} (namespace '{DEFAULT_KRL_SIGNATURE_NAMESPACE}', signer {fp})",
            sig_out.display()
        );
    }
    Ok(())
}

fn cmd_verify(args: &[String]) -> Result<(), String> {
    let mut ca_paths: Vec<PathBuf> = Vec::new();
    let mut pubkey_paths: Vec<PathBuf> = Vec::new();
    let mut krl_paths: Vec<PathBuf> = Vec::new();
    let mut namespace = DEFAULT_SIGNATURE_NAMESPACE.to_owned();
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

    let mut policy = ModTrustPolicy::strict()
        .with_namespace(&namespace)
        .map_err(|e| e.to_string())?;
    for p in &ca_paths {
        policy = policy.add_ca_file(p).map_err(|e| e.to_string())?;
    }
    for p in &pubkey_paths {
        policy = policy.add_public_key_file(p).map_err(|e| e.to_string())?;
    }
    for p in &krl_paths {
        policy = policy.add_krl_file(p).map_err(|e| e.to_string())?;
    }
    if let Some(principal) = required_principal {
        policy = policy.with_required_principal(principal);
    }

    let binary_bytes = fs::read(&mod_path)
        .map_err(|e| format!("failed to read mod binary {}: {e}", mod_path.display()))?;
    let verified = match sig_path {
        Some(explicit) => {
            let sig_bytes = fs::read(&explicit)
                .map_err(|e| format!("failed to read signature {}: {e}", explicit.display()))?;
            policy
                .verify_bytes(&binary_bytes, &sig_bytes)
                .map_err(|e| e.to_string())?
        }
        None => {
            let default_sig = default_signature_path_for(&mod_path);
            let sidecar = if default_sig.is_file() {
                Some(fs::read(&default_sig).map_err(|e| {
                    format!("failed to read signature {}: {e}", default_sig.display())
                })?)
            } else {
                None
            };
            policy
                .verify_binary_or_sidecar(&binary_bytes, sidecar.as_deref())
                .map_err(|e| e.to_string())?
        }
    };

    let sha_hex: String = verified
        .binary_sha256
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();

    println!("Verified mod signature:");
    println!("  signer_fp:  {}", verified.signer_fingerprint);
    if let Some(ca_fp) = &verified.ca_fingerprint {
        println!("  ca_fp:      {ca_fp}");
    }
    if let Some(id) = &verified.key_id {
        println!("  key_id:     {id}");
    }
    if !verified.principals.is_empty() {
        println!("  principals: {}", verified.principals.join(", "));
    }
    if let Some(serial) = verified.serial {
        println!("  serial:     {serial}");
    }
    println!("  namespace:  {}", verified.namespace);
    println!("  sha256:     {sha_hex}");
    Ok(())
}
