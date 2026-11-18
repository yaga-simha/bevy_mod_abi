#![no_main]

//! Fuzz Target: OpenSSH `SSHSIG`, certificate wire parsing, binary `SSHKRL`, signed KRL, and
//! revocation spec parser inside `ModTrustPolicy`.

use bevy_abi_mod_host_sys::ModTrustPolicy;
use libfuzzer_sys::fuzz_target;
use std::hint::black_box;

// Fixed deterministic Ed25519 trust anchor used solely for parser/verifier fuzzing.
const FUZZ_ED25519_PUB: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIFhmZmZmZmZmZmZmZmZmZmZmZmZmZmZmZmZmZmZmZmZm fuzz@abi";

fuzz_target!(|data: &[u8]| {
    let Ok(policy) = ModTrustPolicy::from_ca_openssh(FUZZ_ED25519_PUB) else {
        return;
    };
    let policy = match policy.add_public_key_openssh(FUZZ_ED25519_PUB) {
        Ok(p) => p,
        Err(_) => return,
    };

    let _ = black_box(policy.clone().add_krl_bytes(data));

    if let Some((&selector, rest)) = data.split_first() {
        let split_at = if rest.is_empty() {
            0
        } else {
            usize::from(selector) % rest.len()
        };
        let (part_a, part_b) = rest.split_at(split_at);
        let _ = black_box(policy.verify_bytes(part_a, part_b));
        let _ = black_box(policy.clone().add_signed_krl_bytes(part_a, part_b));
    }

    if let Ok(spec_text) = core::str::from_utf8(data) {
        let _ = black_box(policy.add_revocation_spec_str(spec_text));
    }
});
