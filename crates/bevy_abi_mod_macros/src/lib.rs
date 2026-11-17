#![forbid(unsafe_code)]

//! # `bevy_abi_mod_macros`: Procedural Macros for `#[derive(ModComponent)]` and `#[derive(AbiPod)]`
//!
//! This crate provides zero-Bevy-dependency procedural macros targeting ABI `0.2`:
//! - [`ModComponent`]: Generates a `#[repr(C)]` `<Struct>VTable` prefixed at byte offset `0` by
//!   `VTableHeader` (`struct_size`, `abi_version`, `uuid`, `size`, `align`) with
//!   `Option<unsafe extern "C" fn(...)>` field accessor slots, error-returning `FfiResult` setters,
//!   namespaced `"bam.v0.<canonical_name>.*"` symbols, deterministic 128-bit SipHash-2-4
//!   `ComponentUuid`s, `ModComponent` metadata (`IS_ABI_POD`, `IS_RAW_INSERTABLE`), and conditional
//!   `RawInsertable` / `AbiPod` implementations.
//! - [`AbiPod`]: Verifies standalone `#[repr(C)]` / `#[repr(transparent)]` non-packed Plain Old
//!   Data structs (`Copy + Send + Sync + 'static`, zero drop glue, and `assert_abi_pod::<FieldTy>()`
//!   on every field, rejecting `u128`, `i128`, `usize`, `isize`, `bool`, `char`, enums, and heap types).
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
//! - `#[mod_component(export_symbols)]`: Emits `#[unsafe(export_name = "bam_<ns>_<Struct>_...")]`
//!   on generated `extern "C"` functions.
//! - `#[mod_component(no_pod)]`: Opts out of automatic `AbiPod` implementation (`IS_ABI_POD = false`)
//!   even on `#[repr(C)]` structs while keeping `RawInsertable` when eligible.
//! - `#[mod_component(opaque)]`: Marks the struct VTable-only (`IS_RAW_INSERTABLE = false`,
//!   `IS_ABI_POD = false`), allowing nested fields that implement `ModComponent` without
//!   `RawInsertable`.
//! - `#[mod_component(sparse_set)]` / `#[mod_component(immutable)]`: Populates
//!   `ModComponent::COMPONENT_FLAGS`.
//!
//! ### Field-Level Attributes (`#[mod_field(...)]`)
//! - `#[mod_field(skip)]`: Skips accessor generation for host-private fields.
//! - `#[mod_field(bool)]`: Forces `FfiBool` accessor generation (`bool` or `bool` type alias).
//! - `#[mod_field(primitive)]`: Forces by-value primitive accessor generation.
//! - `#[mod_field(string)]`: Forces `StrView` host-allocated `::std::string::String` accessors.
//! - `#[mod_field(vec)]` / `#[mod_field(vec(ElemTy))]`: Forces `FfiSlice<ElemTy>` + `push_<field>`
//!   host-allocated `::std::vec::Vec<ElemTy>` accessors (`ElemTy: AbiPod`).
//! - `#[mod_field(nested)]`: Forces `*const c_void` / `*mut c_void` interior pointer accessors.

mod abi_pod;
mod attrs;
mod classify;
mod codegen;

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
/// 3. The struct implements `Copy` and has no drop glue (`!core::mem::needs_drop::<Self>()`).
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
///   `SymbolEntry` registration metadata (`"bam.v0.<canonical_name>.*"`), and conditional
///   `RawInsertable` / `AbiPod` implementations when the struct's layout and fields qualify.
#[proc_macro_derive(ModComponent, attributes(mod_component, mod_field))]
pub fn derive_mod_component(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    match codegen::expand_mod_component(&input) {
        Ok(ts) => ts.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

#[cfg(test)]
mod tests {
    use crate::abi_pod::expand_abi_pod;
    use crate::attrs::resolve_sys_crate_with;
    use crate::codegen::{expand_mod_component, expand_mod_component_with_crate};
    use proc_macro_crate::FoundCrate;
    use syn::{DeriveInput, Path, parse_quote};

    #[test]
    fn rejects_generic_structs() {
        let input: DeriveInput = parse_quote! {
            pub struct GenericComp<T> {
                pub value: T,
            }
        };
        let err = expand_mod_component(&input).unwrap_err();
        assert!(err.to_string().contains("does not support generic structs"));
    }

    #[test]
    fn rejects_enums() {
        let input: DeriveInput = parse_quote! {
            pub enum StateComp {
                Idle,
                Running,
            }
        };
        let err = expand_mod_component(&input).unwrap_err();
        assert!(err.to_string().contains("can only be applied to structs"));
    }

    #[test]
    fn rejects_packed_structs() {
        let input: DeriveInput = parse_quote! {
            #[repr(C, packed)]
            pub struct PackedComp {
                pub x: u8,
                pub y: u32,
            }
        };
        let err = expand_mod_component(&input).unwrap_err();
        assert!(err.to_string().contains("packed"));
    }

    #[test]
    fn rejects_nested_field_name_collision() {
        let input: DeriveInput = parse_quote! {
            pub struct CollidingNested {
                pub x: SubVec,
                pub mut_x: SubVec,
            }
        };
        let err = expand_mod_component(&input).unwrap_err();
        assert!(
            err.to_string()
                .contains("colliding interior-pointer accessor name")
        );
    }

    #[test]
    fn rejects_heap_owning_types_without_skip() {
        for bad_input in [
            parse_quote! { pub struct Bad1 { pub opt: Option<String> } },
            parse_quote! { pub struct Bad2 { pub boxed: Box<u32> } },
            parse_quote! { pub struct Bad3 { pub map: HashMap<u32, u32> } },
            parse_quote! { pub struct Bad4 { pub r: &'static str } },
        ] {
            let err = expand_mod_component(&bad_input).unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("mod_field(skip)") || msg.contains("cannot be exposed"),
                "unexpected error message: {msg}"
            );
        }
    }

    #[test]
    fn rejects_unknown_attributes_and_invalid_uuids() {
        let bad_attr: DeriveInput = parse_quote! {
            #[mod_component(nonexistent_attr)]
            pub struct BadAttr {
                pub x: f32,
            }
        };
        assert!(
            expand_mod_component(&bad_attr)
                .unwrap_err()
                .to_string()
                .contains("unsupported `#[mod_component")
        );

        let bad_uuid: DeriveInput = parse_quote! {
            #[mod_component(uuid = "not-a-valid-uuid")]
            pub struct BadUuid {
                pub x: f32,
            }
        };
        assert!(
            expand_mod_component(&bad_uuid)
                .unwrap_err()
                .to_string()
                .contains("invalid `#[mod_component(uuid")
        );

        let conflicting_ns: DeriveInput = parse_quote! {
            #[mod_component(namespace = "game", name = "other::Pos")]
            pub struct BadNs {
                pub x: f32,
            }
        };
        assert!(
            expand_mod_component(&conflicting_ns)
                .unwrap_err()
                .to_string()
                .contains("cannot combine")
        );
    }

    #[test]
    fn bool_generates_ffi_bool_and_ffi_result_and_omits_raw_insertable() {
        let input: DeriveInput = parse_quote! {
            #[repr(C)]
            pub struct Flagged {
                pub active: bool,
            }
        };
        let ts = expand_mod_component(&input).unwrap().to_string();
        assert!(ts.contains("FfiBool"));
        assert!(ts.contains("FfiResult"));
        assert!(ts.contains("VTableHeader"));
        assert!(ts.contains("IS_ABI_POD : bool = false"));
        assert!(ts.contains("IS_RAW_INSERTABLE : bool = false"));
        assert!(!ts.contains("unsafe impl :: bevy_abi_mod_sys :: RawInsertable for Flagged"));
        assert!(!ts.contains("unsafe impl :: bevy_abi_mod_sys :: AbiPod for Flagged"));
    }

    #[test]
    fn string_and_vec_omit_raw_insertable_and_emit_push() {
        for input in [
            parse_quote! { pub struct WithStr { pub s: String } },
            parse_quote! { pub struct WithVec { pub v: Vec<f32> } },
            parse_quote! { pub struct WithSkip { pub x: f32, #[mod_field(skip)] pub hidden: Option<u32> } },
        ] {
            let ts = expand_mod_component(&input).unwrap().to_string();
            assert!(ts.contains("IS_ABI_POD : bool = false"));
            assert!(ts.contains("IS_RAW_INSERTABLE : bool = false"));
            assert!(!ts.contains("unsafe impl :: bevy_abi_mod_sys :: RawInsertable"));
            assert!(!ts.contains("unsafe impl :: bevy_abi_mod_sys :: AbiPod"));
        }

        let vec_input: DeriveInput = parse_quote! {
            pub struct Inventory {
                pub items: Vec<u32>,
            }
        };
        let vec_ts = expand_mod_component(&vec_input).unwrap().to_string();
        assert!(vec_ts.contains("pub get_items"));
        assert!(vec_ts.contains("pub set_items"));
        assert!(vec_ts.contains("pub push_items"));
    }

    #[test]
    fn repr_c_pod_emits_raw_insertable_abi_pod_and_schema_uuid() {
        let input: DeriveInput = parse_quote! {
            #[repr(C)]
            pub struct PodPos {
                pub x: f32,
                pub y: f32,
            }
        };
        let ts = expand_mod_component(&input).unwrap().to_string();
        assert!(ts.contains("IS_ABI_POD : bool = true"));
        assert!(ts.contains("IS_RAW_INSERTABLE : bool = true"));
        assert!(ts.contains("hash_field_schema"));
        assert!(ts.contains("ComponentUuid :: from_parts"));
        assert!(ts.contains("unsafe impl :: bevy_abi_mod_sys :: RawInsertable for PodPos"));
        assert!(ts.contains("unsafe impl :: bevy_abi_mod_sys :: AbiPod for PodPos"));
    }

    #[test]
    fn usize_in_mod_component_is_raw_insertable_but_not_abi_pod() {
        let input: DeriveInput = parse_quote! {
            #[repr(C)]
            pub struct HostCounter {
                pub count: usize,
            }
        };
        let ts = expand_mod_component(&input).unwrap().to_string();
        assert!(ts.contains("IS_RAW_INSERTABLE : bool = true"));
        assert!(ts.contains("IS_ABI_POD : bool = false"));
        assert!(ts.contains("unsafe impl :: bevy_abi_mod_sys :: RawInsertable for HostCounter"));
        assert!(!ts.contains("unsafe impl :: bevy_abi_mod_sys :: AbiPod for HostCounter"));
    }

    #[test]
    fn abi_pod_derive_uses_assert_abi_pod_and_raw_insertable() {
        let missing_repr: DeriveInput = parse_quote! {
            pub struct NotReprC {
                pub x: f32,
            }
        };
        assert!(
            expand_abi_pod(&missing_repr)
                .unwrap_err()
                .to_string()
                .contains("requires `#[repr(C)]` or `#[repr(transparent)]`")
        );

        let packed: DeriveInput = parse_quote! {
            #[repr(C, packed)]
            pub struct PackedPod {
                pub x: u8,
                pub y: u32,
            }
        };
        assert!(
            expand_abi_pod(&packed)
                .unwrap_err()
                .to_string()
                .contains("packed")
        );

        let valid_pod: DeriveInput = parse_quote! {
            #[repr(C)]
            pub struct ValidPod {
                pub x: f32,
                pub y: u64,
            }
        };
        let ts = expand_abi_pod(&valid_pod).unwrap().to_string();
        assert!(ts.contains("assert_abi_pod :: < f32 > ()"));
        assert!(ts.contains("assert_abi_pod :: < u64 > ()"));
        assert!(ts.contains("unsafe impl :: bevy_abi_mod_sys :: RawInsertable for ValidPod"));
        assert!(ts.contains("unsafe impl :: bevy_abi_mod_sys :: AbiPod for ValidPod"));
    }

    #[test]
    fn crate_path_resolution_and_overrides() {
        let override_path: Path = parse_quote!(crate::reexport::sys);
        assert_eq!(
            resolve_sys_crate_with(Some(&override_path), None, |_| None).to_string(),
            "crate :: reexport :: sys"
        );

        assert_eq!(
            resolve_sys_crate_with(None, Some("my_game"), |pkg| {
                (pkg == "bevy_abi_mod_sys").then(|| FoundCrate::Name("renamed_sys".to_owned()))
            })
            .to_string(),
            ":: renamed_sys"
        );

        for facade in ["bevy_abi_mod", "bevy_abi_mod_host", "bevy_abi_mod_guest"] {
            assert_eq!(
                resolve_sys_crate_with(None, Some("my_game"), |pkg| {
                    (pkg == facade).then(|| FoundCrate::Name(facade.to_owned()))
                })
                .to_string(),
                format!(":: {facade} :: bevy_abi_mod_sys")
            );
            assert_eq!(
                resolve_sys_crate_with(None, Some(facade), |pkg| {
                    (pkg == facade).then_some(FoundCrate::Itself)
                })
                .to_string(),
                "crate :: bevy_abi_mod_sys"
            );
            assert_eq!(
                resolve_sys_crate_with(None, Some("integration_test"), |pkg| {
                    (pkg == facade).then_some(FoundCrate::Itself)
                })
                .to_string(),
                format!(":: {facade} :: bevy_abi_mod_sys")
            );
        }

        let input_with_override: DeriveInput = parse_quote! {
            #[repr(C)]
            #[mod_component(crate = my_facade::bevy_abi_mod_sys)]
            pub struct CustomCrateComp {
                pub x: f32,
            }
        };
        let ts = expand_mod_component(&input_with_override)
            .unwrap()
            .to_string();
        assert!(ts.contains("my_facade :: bevy_abi_mod_sys :: ModComponent"));
    }

    #[test]
    fn hygienic_const_scope_and_namespaced_bam_export_symbols() {
        let private_fns: DeriveInput = parse_quote! {
            pub struct LocalPos {
                pub x: f32,
            }
        };
        let ts_private = expand_mod_component(&private_fns).unwrap().to_string();
        assert!(ts_private.contains("const _ : () ="));
        assert!(!ts_private.contains("export_name"));
        assert!(!ts_private.contains("no_mangle"));

        let exported_default_ns: DeriveInput = parse_quote! {
            #[mod_component(export_symbols)]
            pub struct ExportedPos {
                pub x: f32,
            }
        };
        let ts_exported = expand_mod_component_with_crate(&exported_default_ns, Some("my_game"))
            .unwrap()
            .to_string();
        assert!(ts_exported.contains("export_name = \"bam_my_game_ExportedPos_get_x\""));
        assert!(ts_exported.contains("export_name = \"bam_my_game_ExportedPos_set_x\""));

        let exported_explicit_ns: DeriveInput = parse_quote! {
            #[mod_component(name = "physics::Position", export_symbols)]
            pub struct Position {
                pub x: f32,
            }
        };
        let ts_ns = expand_mod_component_with_crate(&exported_explicit_ns, Some("my_game"))
            .unwrap()
            .to_string();
        assert!(ts_ns.contains("export_name = \"bam_physics_Position_get_x\""));
        assert!(ts_ns.contains("export_name = \"bam_physics_Position_set_x\""));
        assert!(ts_ns.contains("\"bam.v0.physics::Position.vtable\""));
        assert!(ts_ns.contains("\"bam.v0.physics::Position.get_x\""));
    }
}
