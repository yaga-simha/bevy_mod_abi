# `bevy_abi_mod_macros`

Zero-Bevy-dependency procedural macros (`#![forbid(unsafe_code)]`) for [`bevy_abi_mod`](../bevy_abi_mod) (`ABI 0.2.0`).

Provides two derive macros usable in both Bevy host applications and decoupled guest `.cdylib` crates:

- **`#[derive(ModComponent)]`**:
  - Generates a `#[repr(C)]` `<Struct>VTable` prefixed by [`VTableHeader`](../bevy_abi_mod_sys) and populated with `extern "C"` getter, error-returning `FfiResult` setter, interior-pointer, and `Vec::push` slots.
  - Emits `"bam.v0.<canonical_name>.*"` symbol metadata, optional C-linkage symbol exports (`#[mod_component(export_symbols)]`), and compile-time `C_DECL` / `ZIG_DECL` layout constants for POD structs.
  - Computes a deterministic 128-bit SipHash-2-4 [`ComponentUuid`](../bevy_abi_mod_sys) from canonical name, size, alignment, and field schema hashes, and conditionally implements `RawInsertable` and `AbiPod` when safe.
  - Supports explicit `#[mod_component(...)]` and `#[mod_field(...)]` overrides (`skip`, `bool`, `primitive`, `string`, `vec(T)`, `nested`, `opaque`, `sparse_set`, `immutable`, `no_pod`, `name`, `namespace`, `uuid`, `prefix`, `crate`).
- **`#[derive(AbiPod)]`**:
  - Validates standalone `#[repr(C)]` / `#[repr(transparent)]` (non-packed) Plain Old Data types (`Copy + Send + Sync + 'static`, zero drop glue, fixed-width C-compatible fields) and emits `unsafe impl AbiPod` + `unsafe impl RawInsertable`.
