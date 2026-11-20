#![forbid(unsafe_code)]
#![warn(missing_docs)]

//! # `bevy_abi_mod_macros`: Procedural Macros for `#[derive(ModComponent)]`, `#[derive(AbiPod)]`, and `#[derive(GuestComponent)]`
//!
//! This crate provides zero-Bevy-dependency procedural macros targeting ABI `0.3.0`:
//! - [`ModComponent`]: Generates a `#[repr(C)]` `<Struct>VTable` prefixed at byte offset `0` by
//!   `VTableHeader` (`struct_size`, `abi_version`, `uuid`, `size`, `align`) with
//!   `Option<unsafe extern "C" fn(...)>` field accessor slots, error-returning `FfiResult` setters,
//!   schema-evolution witness slots (`field_info`, `export_fields`, `import_field`),
//!   namespaced `"bam.v0.<canonical_name>.*"` symbols, deterministic 128-bit SipHash-2-4
//!   `ComponentUuid`s, 64-bit FNV-1a `SCHEMA_FINGERPRINT`, static `BAM_FIELDS: &[BamFieldDesc]`,
//!   `REQUIRED_COMPONENTS`, `C_DECL` / `ZIG_DECL` declarations, and conditional `RawInsertable` /
//!   `AbiPod` / `GuestComponent` implementations across named structs, tuple structs, `Option<T>`,
//!   and `#[repr(i8..u64)]` enums.
//! - [`AbiPod`]: Verifies standalone `#[repr(C)]` / `#[repr(transparent)]` non-packed Plain Old
//!   Data structs (`Copy + Send + Sync + 'static`, zero drop glue, zero implicit compiler padding via
//!   `size_of::<Self>() == sum(size_of::<FieldTy>())`, and `assert_abi_pod::<FieldTy>()` on every
//!   field, rejecting `u128`, `i128`, `usize`, `isize`, `bool`, `char`, enums, heap types, and
//!   padded structs).
//! - [`GuestComponent`]: Emits ONLY `impl bevy_abi_mod_guest::GuestComponent for #Ident` with
//!   bit-for-bit UUID parity against host `#[derive(ModComponent)]` (via `FIELD_SCHEMA_HASH`),
//!   optional `#[guest_component(...)]` attributes (falling back to `#[mod_component(...)]` keys),
//!   and zero VTable/C-decl overhead when paired with `#[derive(AbiPod, GuestComponent)]`.
//!
//! ## Syntactic Type Classification & `#[mod_field(...)]` Precedence
//!
//! Because procedural macros inspect AST syntax prior to Rust type resolution, unannotated fields
//! are classified by their syntactic type path (`bool`, fixed-width primitives, `usize`/`isize`,
//! `String`, `Vec<T>`, or nested compound/array types) and **verified at compile time** via
//! generated trait/type guards:
//! - Explicit `#[mod_field(...)]` attributes **always override** syntactic path inspection, allowing
//!   type aliases (`type Label = String;` with `#[mod_field(string)]`, `type Scores = Vec<u32>;`
//!   with `#[mod_field(vec(u32))]`, `type Flag = bool;` with `#[mod_field(bool)]`) or custom types
//!   named `String` (`#[mod_field(nested)]` / `#[mod_field(skip)]`).
//! - Spoofed types (such as a user-defined `struct String;` without `#[mod_field(...)]`) fail a
//!   compile-time `std::string::String` / `std::vec::Vec<T>` trait assertion cleanly instead of
//!   generating unsound accessors.
//!
//! ## Attribute Reference
//!
//! ### Struct-Level Attributes (`#[mod_component(...)]`)
//! - `#[mod_component(name = "...")]`: Custom component name. Names containing `"::"` (e.g.
//!   `"physics::Position"`) set the full canonical name; unqualified names (e.g. `"CustomPos"`)
//!   are prefixed with `module_path!()` (or `namespace = "..."` if provided).
//! - `#[mod_component(namespace = "...")]`: Explicit namespace prefix overriding `module_path!()`.
//! - `#[mod_component(uuid = "xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx")]`: Explicit 128-bit UUID hex
//!   override (36-char hyphenated or 32-char hex string) evaluated at compile time via
//!   `ComponentUuid::parse_hex`.
//! - `#[mod_component(prefix = "...")]`: Custom symbol stem for `export_symbols`.
//! - `#[mod_component(crate = path)]`: Explicit path to `bevy_abi_mod_sys`.
//! - `#[mod_component(guest)]` / `#[mod_component(guest_crate = path)]`: Also emits
//!   `impl bevy_abi_mod_guest::GuestComponent for #Ident` delegating to `<Self as ModComponent>`
//!   (requires an `AbiPod` struct).
//! - `#[mod_component(export_symbols)]`: Emits `#[unsafe(export_name = "bam_<ns>_<Struct>_...")]`
//!   on generated `extern "C"` functions.
//! - `#[mod_component(no_pod)]`: Opts out of automatic `AbiPod` implementation (`IS_ABI_POD = false`)
//!   even on `#[repr(C)]` structs while keeping `RawInsertable` when eligible.
//! - `#[mod_component(opaque)]`: Marks the struct VTable-only (`IS_RAW_INSERTABLE = false`,
//!   `IS_ABI_POD = false`), allowing nested fields that implement `ModComponent` without
//!   `RawInsertable`.
//! - `#[mod_component(sparse_set)]` / `#[mod_component(immutable)]` /
//!   `#[mod_component(summary_tick)]`: Populates `ModComponent::COMPONENT_FLAGS`.
//! - `#[mod_component(require(CompA, CompB = expr))]`: Declares compile-time verified required
//!   `ModComponent` dependencies (`REQUIRED_COMPONENT_NAMES`, `REQUIRED_COMPONENT_UUIDS`, and
//!   optional custom initialization byte descriptors in `ModComponent::REQUIRED_COMPONENTS`).
//!
//! ### Struct-Level Attributes (`#[guest_component(...)]`)
//! - `name = "..."`, `namespace = "..."`, `uuid = "..."`, `no_schema_hash`, `sparse_set`,
//!   `immutable`, `summary_tick`, `crate = path`, `guest_crate = path`.
//! - When `#[guest_component(...)]` is absent, falls back to compatible `#[mod_component(...)]`
//!   keys so `#[derive(ModComponent, GuestComponent)] #[mod_component(name = "...")]` works
//!   without repeating attributes.
//!
//! ### Field-Level Attributes (`#[mod_field(...)]`)
//! - `#[mod_field(skip)]`: Skips accessor generation for host-private fields.
//! - `#[mod_field(bool)]`: Forces `FfiBool` accessor generation (`bool` or `bool` type alias).
//! - `#[mod_field(primitive)]`: Forces by-value primitive accessor generation.
//! - `#[mod_field(string)]`: Forces `StrView` host-allocated `::std::string::String` accessors.
//! - `#[mod_field(vec)]` / `#[mod_field(vec(ElemTy))]`: Forces full `HostVec` VTable accessors
//!   (`get`, `set`, `len`, `clear`, `push`, `pop`, `remove`, `swap_remove`, `truncate`, `reserve`,
//!   `as_mut_slice`) for `::std::vec::Vec<ElemTy>` (`ElemTy: AbiPod`).
//! - `#[mod_field(option)]` / `#[mod_field(option(InnerTy))]`: Forces `FfiOption<InnerTy>`
//!   (`get`, `set`, `clear`) accessors (`InnerTy: AbiPod`).
//! - `#[mod_field(nested)]`: Forces `*const c_void` / `*mut c_void` interior pointer accessors.

mod abi_pod;
mod attrs;
mod classify;
mod codegen;
mod guest_component;

#[cfg(test)]
mod tests;

use proc_macro::TokenStream;
use syn::{DeriveInput, parse_macro_input};

/// Derive macro for standalone `#[repr(C)]` / `#[repr(transparent)]` cross-language Plain Old Data
/// (`AbiPod`) types.
///
/// # Intent
///
/// `AbiPod` marks types whose in-memory layout is deterministic across Rust, C99, C++, and Zig
/// compilers, allowing direct zero-copy slice and column access across the C-ABI boundary.
///
/// # Expansion & Compile-Time Checks
///
/// Enforces at compile time that:
/// 1. The target is a non-generic struct annotated with `#[repr(C)]` or `#[repr(transparent)]` (and not `packed`).
/// 2. Every field implements `AbiPod` via `assert_abi_pod::<FieldTy>()` (`u128`, `i128`, `usize`, `isize`, `bool`, `char`, `String`, `Vec<T>`, enums, and `#[repr(Rust)]` structs are rejected).
/// 3. The struct implements `Copy`, has no drop glue (`!core::mem::needs_drop::<Self>()`), and has zero implicit padding (`core::mem::size_of::<Self>() == sum(size_of::<FieldTy>())`).
#[proc_macro_derive(AbiPod, attributes(mod_component))]
pub fn derive_abi_pod(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    match abi_pod::expand_abi_pod(&input) {
        Ok(ts) => ts.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

/// Derive macro for `ModComponent` host components and nested sub-structs.
///
/// # Intent
///
/// Enables Bevy host components (including `#[repr(Rust)]` structs or types containing host-owned
/// `String` / `Vec<T>` fields) to be inspected and mutated by native guest mods over `extern "C"`
/// without JSON serialization or cross-allocator freeing.
///
/// # Expansion Behavior
///
/// Generates:
/// - A `#[repr(C)]` `<Struct>VTable` prefixed by `VTableHeader` (`struct_size`, `abi_version`,
///   `uuid`, `size`, `align`) with `Option<unsafe extern "C" fn(...)>` field accessor slots.
/// - `extern "C"` getter, setter (returning `FfiResult`), and `Vec` push trampolines for each
///   non-skipped field.
/// - A `ModComponent` implementation with deterministic 128-bit SipHash-2-4 `ComponentUuid`,
///   64-bit FNV-1a `SCHEMA_FINGERPRINT`, static `BAM_FIELDS`, `REQUIRED_COMPONENTS`, `C_DECL` /
///   `ZIG_DECL`, witness reflection slots, `SymbolEntry` metadata (`"bam.v0.<canonical_name>.*"`),
///   and conditional `RawInsertable` / `AbiPod` / `GuestComponent` implementations when layout and
///   fields qualify.
#[proc_macro_derive(ModComponent, attributes(mod_component, mod_field))]
pub fn derive_mod_component(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    match codegen::expand_mod_component(&input) {
        Ok(ts) => ts.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

/// Derive macro for lightweight `bevy_abi_mod_guest::GuestComponent` implementations.
///
/// # Intent
///
/// Eliminates manual `impl GuestComponent` boilerplate for guest POD components while preserving
/// bit-for-bit `ComponentUuid` parity with host `#[derive(ModComponent)]` via `FIELD_SCHEMA_HASH`.
///
/// Emits **only** `impl bevy_abi_mod_guest::GuestComponent for #Ident` (never emits `AbiPod` or
/// `RawInsertable`), so it composes cleanly with both `#[derive(AbiPod, GuestComponent)]` and
/// `#[derive(ModComponent, GuestComponent)]`.
#[proc_macro_derive(GuestComponent, attributes(guest_component, mod_component))]
pub fn derive_guest_component(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    match guest_component::expand_guest_component(&input) {
        Ok(ts) => ts.into(),
        Err(err) => err.to_compile_error().into(),
    }
}
