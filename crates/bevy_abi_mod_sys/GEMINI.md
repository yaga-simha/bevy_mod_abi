# Agent Guide: `bevy_abi_mod_sys`

Crate guide for **`bevy_abi_mod_sys`** (`v0.3.0`, ABI `0x0003_0000`) — the `#![no_std]`, zero-dependency C-ABI wire contract for the `bevy_mod_abi` ecosystem.

---

## 1. Crate Identity
- **Name:** `bevy_abi_mod_sys` (`version = "0.3.0"`, `edition = "2024"`, MSRV `1.87`, `categories = [..., "no-std"]`)
- **Role:** Single source of truth for the asymmetric C-ABI (`0x0003_0000`, `ABI_VERSION = 0.3.0`) shared across host crates, procedural macros, Rust guest crates, `bevy_abi_mod_sdk_gen`, and C99/C++17/Zig guests.
- **Core Principle:** **Zero Shared Rust ABI Assumptions**. No Bevy types, no `std`, no `alloc`, and no Rust layout coupling across the FFI boundary.

---

## 2. Safety & Dependency Boundaries
- **`#![no_std]` & Zero External Dependencies:** NEVER add `std`, `alloc`, `bevy*`, or any external dependency to `Cargo.toml`.
- **`unsafe` Policy:** Permitted here ONLY for `unsafe trait` definitions (`ModComponent`, `AbiPod`, `RawInsertable`) and low-level ABI invariants. Enforces `unsafe_op_in_unsafe_fn = "deny"`, `improper_ctypes_definitions = "deny"`, `#![deny(clippy::undocumented_unsafe_blocks, clippy::missing_safety_doc)]`. Every `unsafe` block MUST carry `// SAFETY:`; every `unsafe fn` / `unsafe trait` MUST carry `# Safety` docs.

---

## 3. Owned Surface & Frozen Wire Contract
- **Version & Host API (`version.rs`, `host_api.rs`):** `BAM_ABI_VERSION` (`0x0003_0000`), `BamManifest`, all **68/68** `BamHostApi` slots (`Option<unsafe extern "C" fn(...)>`), frozen `MIN_V0_2_SIZE` (`224` B / `116` B) & `MIN_V0_3_SIZE` (`552` B / `280` B), and `offsets` (`<FIELD>`, `OFF64_*`, `OFF32_*`, `SLOT_COUNT = 68`, `SIZE_64 = 552`, `SIZE_32 = 280`).
- **Primitives (`primitives/`):** `FfiResult` (`CYCLE_DETECTED = 16`, etc.), `FfiBool`, `FfiOption<T>`, `StrView`, `FfiSlice<T>`, `FfiEntity`, `FfiChildOf`, `FfiAssetHandle`, `FfiAssetLoadState`, `BamComponent`, `BamSystem`, `BamSystemSet`, `BamSymbol`, `ComponentUuid` (const keyed SipHash-2-4-128), `ScheduleId`, `AccessMode`, `BamEventKind`, `BAM_LOG_*`.
- **Descriptors & Traits (`component.rs`, `system.rs`, `commands.rs`, `iter.rs`, `access.rs`, `extensible.rs`, `traits.rs`):** `ComponentDesc`, `BAM_COMPONENT_FLAG_SUMMARY_TICK`, `BAM_STORAGE_FLAG_SUMMARY_TICK`, `BamFieldDesc`, `BamFieldKind`, `BamRequiredComponent`, `BamSystemDesc`, `BamQueryDesc`, `BamTermDesc`, `BamObserverDesc`, `BamTriggerDesc`, `BamRelationDesc` (`BAM_RELATION_FLAG_*`), `BamLayoutInfo`, `BamRowMigrateFn`, `BamScheduleSetDesc` (`BAM_SCHEDULE_SET_FLAG_*`), `BamChangeTicks`, `BamColumnInfo`, `VTableHeader`, `ExtensibleStruct`, `ModComponent`, `AbiPod`, `RawInsertable`.

---

## 4. Strict Engineering Rules
1. **ABI Freeze (`0x0003_0000`):** Do NOT reorder fields, resize frozen prefix structs (`MIN_V0_2_SIZE`, `MIN_V0_3_SIZE`), or change `BamHostApi` slot count/offsets (`68` slots) without a formal minor/major ABI bump.
2. **Wire Purity:** Every wire struct MUST be `#[repr(C)]` or `#[repr(transparent)]` with zero implicit padding bytes; all strings cross FFI via `StrView` or NUL-terminated C pointers. Never expose Rust `Box`, `Vec`, `String`, or fat pointers.
3. **SemVer & Layout Tests:** Preserve compile-time size/alignment/offset assertions in `src/tests/layout.rs` and `[package.metadata.cargo-semver-checks.lints]` in `Cargo.toml`.

---

## 5. Standalone Verification Commands

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features
MIRIFLAGS="-Zmiri-tree-borrows -Zmiri-strict-provenance" rustup run nightly cargo miri test
cargo tree | grep -i bevy   # MUST print NOTHING
typos
```

---

## 6. Standalone Repository Split Checklist
1. **Dependencies:** Zero workspace or external dependencies exist (`[dependencies]` is empty); only update `repository` URL in `Cargo.toml` if this crate moves to its own Git repo.
2. **Root Tooling Configs:** Copy `.gitignore`, `.gitattributes`, `.typos.toml`, `clippy.toml`, `deny.toml`, `rust-toolchain.toml`, and `LICENSE-MIT` / `LICENSE-APACHE` from the monorepo root.
3. **Downstream Coordination:** Any layout or `offsets` change MUST stay synchronized with `bevy_abi_mod_sdk_gen` (`include/bevy_abi_mod.h`), `bevy_abi_mod_macros`, `bevy_abi_mod_host_sys`, and `bevy_abi_mod_guest_sys`.
