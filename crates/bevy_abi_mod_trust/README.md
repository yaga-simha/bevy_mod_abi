# `bevy_abi_mod_trust`

Pure-Rust, zero-Bevy (`#![forbid(unsafe_code)]`) cryptographic trust and archive verification engine for [`bevy_abi_mod`](../bevy_abi_mod).

## Capabilities

- **Detached OpenSSH `SSHSIG` Signatures (`.sig`):** Verifies Ed25519 signatures over arbitrary shared library byte slices with strict Curve25519 key/scalar validation (`ed25519-dalek` `verify_strict`).
- **Ed25519 CA Certificates & Validity Windows:** Validates `ssh-ed25519-cert-v01@openssh.com` certificates against trusted Root CA keys, authorized principal bindings, serial numbers, and validity timestamps (`CertificateValidityPolicy`).
- **OpenSSH `PROTOCOL.krl` (`SSHKRL`) Revocation Lists:** Parses and builds (`KrlBuilder`) binary KRLs and detached `SSHSIG`-signed KRL bundles with monotonic anti-rollback version checks.
- **Embedded `.bam_sig` Binary Sections:** Extracts, embeds, and verifies `SSHSIG` payloads directly inside ELF (`__bam_sig` / `.bam_sig`), Mach-O (`__DATA,__bam_sig`), and PE (`.bam_sig`) binaries.
- **Verified `.bampack` Mod Bundles:** Packages and verifies signed mod distribution archives (`BamPackBuilder` / `VerifiedBamPack`) using a deterministic little-endian framing format.

See [`spec.md`](../../spec.md) and [`docs/ARCHITECTURE.md`](../../docs/ARCHITECTURE.md) for the load-time verification workflow and threat model.
