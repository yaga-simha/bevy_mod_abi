# Agent Guide: `bevy_abi_mod_trust`

Crate guide for **`bevy_abi_mod_trust`** (`v0.3.0`) — pure-Rust, zero-Bevy cryptographic signature, certificate authority, revocation list, `.bam_sig`, and `.bampack` archive verification engine.

---

## 1. Crate Identity
- **Name:** `bevy_abi_mod_trust` (`version = "0.3.0"`, `edition = "2024"`, MSRV `1.95`)
- **Role:** Verifies Ed25519 OpenSSH `SSHSIG` detached signatures (`.sig`), CA certificates (`ssh-ed25519-cert-v01@openssh.com`), authority-scoped `SSHKRL` revocation lists, embedded binary `.bam_sig` sections, and signed `.bampack` distribution archives.

---

## 2. Safety & Dependency Boundaries
- **Safety:** **`#![forbid(unsafe_code)]`** at crate root. Never allow `unsafe` code, FFI, or C crypto bindings.
- **Zero Bevy Dependencies:** Pure-Rust crypto/parsing only (`ed25519-dalek`, `sha2`, `signature`, `ssh-encoding`, `ssh-key`, `thiserror`; `ssh-key` compiled with `default-features = false, features = ["std", "ed25519"]`, plus optional `keygen` -> `ssh-key/getrandom`). **NEVER** enable `rsa`, `p256`, or any `bevy*` dependency.

---

## 3. Owned Surface & Feature Flags
- **Features:** `default = []`, `keygen = ["ssh-key/getrandom"]`.
- **Signature & Trust Policy (`lib.rs`, `keygen.rs`):** `ModTrustPolicy`, `CertificateValidityPolicy`, `VerificationReport`, strict Curve25519 scalar validation (`ed25519-dalek::VerifyingKey::verify_strict`), CA certificate principal/serial/validity checks, authority-scoped KRL signers (`DEFAULT_KRL_SIGNER_PRINCIPAL = "krl-signer"`, `with_krl_signer_principal`, `add_krl_signer_openssh`, `add_krl_signer_file`, monotonic version anti-rollback), and `keygen` helpers (rejecting existing files via `AlreadyExists` and control chars in comments/principals).
- **Embedded Binary Signatures (`embedded_sig.rs`):** ELF (`__bam_sig` / `.bam_sig`), Mach-O (`__DATA,__bam_sig`, thin + fat), and PE (`.bam_sig`) extraction & verification — rejecting PE padding smuggling (`virt_size < raw_size`), interior NULs, duplicate sections, executable sections, and header/section overlaps.
- **Hardened `.bampack` (`bampack.rs`):** `BamPackBuilder`, `VerifiedBamPack`, `VerifiedBamPack::extract_to_dir` / `extract_to_dir_with_limits` (`BamPackExtractError`, `BamPackExtractLimits`), `BAMPACK_MAGIC`, `BAMPACK_VERSION`, `MAX_BAMPACK_ENTRIES`, `MAX_BAMPACK_PATH_DEPTH`, `MAX_BAMPACK_PATH_LEN`, `DEFAULT_MAX_BAMPACK_UNCOMPRESSED_BYTES`.

---

## 4. Strict Engineering Rules
1. **Verify Before Extracting or Allocating:** Run zero-copy structural checks and Ed25519 verification BEFORE decompressing/copying or writing `.bampack` entries to disk. Reject archive bombs, empty archives, case-insensitive duplicates, file-vs-directory prefix collisions, `..`/absolute paths, symlinks, NTFS ADS (`:`), trailing dots/spaces, and Windows reserved names (`CON`/`PRN`/`AUX`/`NUL`/`COM1..9`/`LPT1..9`).
2. **No KRL Privilege Escalation:** Ordinary mod-signing keys MUST NEVER be trusted to sign KRLs; enforce dedicated KRL signer keys or `krl-signer` CA principal scoping.
3. **Zero Panics on Untrusted Input:** Never `.unwrap()` or `.expect()` on untrusted keys, signatures, binaries, or archive bytes; return typed `thiserror` errors and bound file reads.

---

## 5. Standalone Verification Commands

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo clippy --all-targets --no-default-features -- -D warnings
cargo test --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features
cargo tree | grep -i bevy   # MUST print NOTHING
typos
```

---

## 6. Standalone Repository Split Checklist
1. **Replace Workspace Dependencies:** In `Cargo.toml`, replace `{ workspace = true }` on `ed25519-dalek`, `sha2`, `signature`, `ssh-encoding` (`default-features = false, features = ["std", "pem", "sha2"]`), `ssh-key` (`default-features = false, features = ["std", "ed25519"]`), `thiserror`, and `[dev-dependencies]` (`proptest`, `ssh-key`, `tempfile`) with explicit versions matching root `Cargo.toml`.
2. **Preserve `ssh-key` Feature Guard:** Keep `deny.toml` advisory note / `default-features = false` on `ssh-key` so optional `rsa`/`p256`/`crypto-bigint` advisories remain excluded.
3. **Root Tooling Configs:** Copy `.gitignore`, `.gitattributes`, `.typos.toml`, `clippy.toml`, `deny.toml`, `rust-toolchain.toml`, and `LICENSE-MIT` / `LICENSE-APACHE`; update `repository` in `Cargo.toml` if moved to its own repo.
