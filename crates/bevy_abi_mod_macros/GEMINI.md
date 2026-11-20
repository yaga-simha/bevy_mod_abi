# Agent Guide: `bevy_abi_mod_macros`

Crate guide for **`bevy_abi_mod_macros`** (`v0.3.0`, ABI `0x0003_0000`) — zero-Bevy procedural macros generating VTables, UUIDs, reflection descriptors, and C/Zig declarations.

---

## 1. Crate Identity
- **Name:** `bevy_abi_mod_macros` (`version = "0.3.0"`, `edition = "2024"`, MSRV `1.87`, `proc-macro = true`)
- **Role:** Generates compile-time C-ABI VTables, keyed SipHash-2-4-128 `ComponentUuid`s, `SCHEMA_FINGERPRINT` hashes, declarative `BAM_FIELDS`, `REQUIRED_COMPONENTS`, `C_DECL`/`ZIG_DECL`, and zero-boilerplate `AbiPod` / `GuestComponent` implementations.

---

## 2. Safety & Dependency Boundaries
- **Safety:** **`#![forbid(unsafe_code)]`** at crate root (generated token streams may emit `unsafe impl` for `AbiPod` / `RawInsertable` / `ModComponent` with explicit compile-time layout and zero-padding guards).
- **Zero Bevy Dependencies:** Allowed proc-macro dependencies are `syn`, `quote`, `proc-macro2`, and `proc-macro-crate` ONLY (plus `bevy_abi_mod_sys` and `trybuild` in `[dev-dependencies]`). **NEVER** depend on any `bevy*` crate or on `bevy_abi_mod_guest`.

---

## 3. Owned Surface & Features
- **`#[derive(ModComponent)]` (`lib.rs`, `attrs.rs`, `codegen/`):** Generates `<Struct>VTable` (`VTableHeader` at offset `0`, getters, error-returning `FfiResult` setters, `Vec`/interior-pointer/witness slots), 64-bit FNV-1a `SCHEMA_FINGERPRINT`, static `BAM_FIELDS: &[BamFieldDesc]`, `REQUIRED_COMPONENTS`, `"bam.v0.<name>.*"` metadata, optional `export_symbols`, `C_DECL`/`ZIG_DECL`, and conditional `RawInsertable` / `AbiPod` / `GuestComponent` (`#[mod_component(..., guest)]`).
  - Attributes: `#[mod_component(name, namespace, uuid, prefix, crate, guest, guest_crate, export_symbols, no_pod, opaque, sparse_set, immutable, summary_tick, require(...))]` and `#[mod_field(skip, bool, primitive, string, vec(T), option(T), nested)]`.
- **`#[derive(AbiPod)]` (`abi_pod.rs`):** Validates `#[repr(C)]` / `#[repr(transparent)]` (non-packed), zero drop glue, compile-time zero implicit padding (`size_of::<Self>() == sum(fields)`), and emits `AbiPod` + `RawInsertable`.
- **`#[derive(GuestComponent)]` (`guest_component.rs`):** Lightweight zero-boilerplate guest derive (`#[guest_component(name, namespace, uuid, no_schema_hash, sparse_set, immutable, summary_tick, crate, guest_crate)]`, falling back to compatible `#[mod_component(...)]` keys), emitting ONLY `impl GuestComponent` with `FIELD_SCHEMA_HASH` parity and rejecting `sparse_set + summary_tick`, packed, or enum/generic types.
- **Hygienic Crate Resolution (`attrs.rs`):** `proc-macro-crate` probes `bevy_mod_abi` BEFORE deprecated `bevy_abi_mod` (and resolves `bevy_abi_mod_guest` without Cargo dependency coupling).

---

## 4. Strict Engineering Rules
1. **Zero Implicit Padding & Sound Generated Code:** Never emit `AbiPod` or `RawInsertable` for packed structs, structs with padding, enums with unvalidated discriminants, or non-POD fields.
2. **UUID Parity:** `#[derive(GuestComponent)]` `FIELD_SCHEMA_HASH` MUST stay bit-for-bit aligned with `ModComponent`'s `ComponentUuid::hash_field_schema` so guest and host derive identical UUIDs.
3. **Compile-Fail Regression Coverage:** Maintain `trybuild` UI tests in `tests/compile_fail_tests.rs` + `tests/ui/` for every rejected attribute combination or layout hazard.

---

## 5. Standalone Verification Commands

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features
rustup run nightly cargo miri test --test mod_component_vtable
cargo tree | sed 's/ (.*//' | grep -v bevy_abi_mod | grep -i bevy   # MUST print NOTHING
typos
```

---

## 6. Standalone Repository Split Checklist
1. **Replace Workspace Dependencies:** In `Cargo.toml`, replace `{ workspace = true }` on `syn`, `quote`, `proc-macro2`, `proc-macro-crate`, and `[dev-dependencies]` (`bevy_abi_mod_sys`, `trybuild`) with explicit crates.io versions (matching `0.3.0` for `bevy_abi_mod_sys`).
2. **Update Repository URL & Root Configs:** Set `repository` in `Cargo.toml` if moved to a separate repo; copy `.gitignore`, `.gitattributes`, `.typos.toml`, `clippy.toml`, `deny.toml`, `rust-toolchain.toml`, and `LICENSE-MIT` / `LICENSE-APACHE`.
3. **Host/Guest Smoke Verification:** When changing macro expansions, verify against both `bevy_mod_abi` (`#[derive(ModComponent)]`) and `bevy_abi_mod_guest` (`features = ["derive"]`, `#[derive(AbiPod, GuestComponent)]`).
