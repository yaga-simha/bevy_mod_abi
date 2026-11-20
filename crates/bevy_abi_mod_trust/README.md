# `bevy_abi_mod_trust`

Pure-Rust, zero-Bevy (`#![forbid(unsafe_code)]`) cryptographic trust and archive verification engine for [`bevy_mod_abi`](../bevy_mod_abi) (`v0.3.0`).

## Capabilities

- **Detached OpenSSH `SSHSIG` Signatures (`.sig`):** Verifies Ed25519 signatures over arbitrary shared library byte slices with strict Curve25519 key/scalar validation (`ed25519-dalek` `verify_strict`).
- **Ed25519 CA Certificates & Validity Windows:** Validates `ssh-ed25519-cert-v01@openssh.com` certificates against trusted Root CA keys, authorized principal bindings, serial numbers, and validity timestamps (`CertificateValidityPolicy`).
- **Authority-Scoped OpenSSH `PROTOCOL.krl` (`SSHKRL`) Revocation Lists:** Parses and builds (`KrlBuilder`) binary KRLs and detached `SSHSIG`-signed KRL bundles with monotonic anti-rollback version checks and explicit KRL signer authority scoping (`DEFAULT_KRL_SIGNER_PRINCIPAL = "krl-signer"`, `add_krl_signer_openssh` / `add_krl_signer_file`).
- **Embedded `.bam_sig` Binary Sections:** Extracts, embeds, and verifies `SSHSIG` payloads directly inside ELF (`__bam_sig` / `.bam_sig`), Mach-O (`__DATA,__bam_sig`), and PE (`.bam_sig`) binaries.
- **Verified `.bampack` Mod Bundles & Hardened Extraction:** Packages, verifies, and safely extracts deterministic signed mod distribution archives (`BamPackBuilder`, `VerifiedBamPack::extract_to_dir`, `extract_to_dir_with_limits` with entry-count, path-depth, and uncompressed-byte limits plus symlink/path-traversal rejection), produced by `cargo bam pack` and verified by `cargo bam verify` and `ModTrustPolicy`.

See [`spec.md`](../../spec.md) and [`docs/ARCHITECTURE.md`](../../docs/ARCHITECTURE.md) for the load-time verification workflow and threat model.
