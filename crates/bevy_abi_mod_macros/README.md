# `bevy_abi_mod_macros`

Zero-Bevy-dependency procedural macros (`#![forbid(unsafe_code)]`) for [`bevy_abi_mod`](../bevy_abi_mod) (`ABI 0.3.0`, `v0.3.0`).

Provides two derive macros usable in both Bevy host applications (resolving `bevy_mod_abi`, `bevy_abi_mod`, `bevy_abi_mod_host`, or `bevy_abi_mod_sys` hygienically via `proc-macro-crate`) and decoupled guest `.cdylib` crates across named structs, tuple structs, `Option<T>`, and C-ABI `#[repr(i8..u64)]` enums:

- **`#[derive(ModComponent)]`**:
  - Generates a `#[repr(C)]` `<Struct>VTable` prefixed by [`VTableHeader`](../bevy_abi_mod_sys) and populated with `extern "C"` getter, error-returning `FfiResult` setter, interior-pointer, `Vec`, and schema-evolution witness (`field_info`, `export_fields`, `import_field`) slots.
  - Emits deterministic 64-bit FNV-1a `SCHEMA_FINGERPRINT` (driving zero-copy reload retention & `migrate_component_column`), static `BAM_FIELDS: &[BamFieldDesc]` (driving declarative `ReflectFromPtr` & BRP reflection), `#[mod_component(require(Comp = expr))]` custom-byte initialization descriptors (`REQUIRED_COMPONENTS`), `"bam.v0.<canonical_name>.*"` symbol metadata, optional C-linkage exports (`#[mod_component(export_symbols)]`), and compile-time `C_DECL` / `ZIG_DECL` layout constants.
  - Computes a deterministic 128-bit keyed SipHash-2-4 [`ComponentUuid`](../bevy_abi_mod_sys) from canonical name, size, alignment, and field schema hashes, and conditionally implements `RawInsertable` and `AbiPod` when safe.
  - Supports explicit `#[mod_component(...)]` (`name`, `namespace`, `uuid`, `prefix`, `crate`, `export_symbols`, `no_pod`, `opaque`, `sparse_set`, `immutable`, `summary_tick`, `require(...)`) and `#[mod_field(...)]` (`skip`, `bool`, `primitive`, `string`, `vec(T)`, `option(T)`, `nested`) overrides.
- **`#[derive(AbiPod)]`**:
  - Validates standalone `#[repr(C)]` / `#[repr(transparent)]` (non-packed) Plain Old Data types (`Copy + Send + Sync + 'static`, zero drop glue, compile-time zero implicit padding `size_of::<Self>() == sum(size_of::<Fields>())` matching `bytemuck::NoUninit`, and fixed-width C-compatible fields) and emits `unsafe impl AbiPod` + `unsafe impl RawInsertable`.
