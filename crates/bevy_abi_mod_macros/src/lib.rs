#![forbid(unsafe_code)]

//! # `bevy_abi_mod_macros`: Procedural Macros for `#[derive(ModExposable)]` and `#[derive(AbiPod)]`
//!
//! This crate provides:
//! - `#[derive(ModExposable)]` for host Bevy components and compound types: generates C-ABI field
//!   accessors (including sound `u8` boolean accessors), a `#[repr(C)]` `<Struct>VTable` with
//!   static byte strides and function pointers, compile-time layout assertions, an implementation
//!   of `ExposableComponent` with `IS_POD` and `IS_RAW_INSERTABLE` metadata, and conditional
//!   `FfiLayout` / `AbiPod` implementations when layout and bit-validity invariants hold.
//! - `#[derive(AbiPod)]` for standalone `#[repr(C)]` / `#[repr(transparent)]` Plain Old Data types:
//!   verifies non-packed deterministic representation, `Copy + 'static`, zero drop glue, and
//!   `AbiPod` conformance on every field.
//!
//! ## Macro Compilation Pipeline
//!
//! ```text
//! Rust Struct (#[derive(ModExposable)])
//!         │
//!         ▼
//!    AST Parsing & Validation
//!    ├── Reject generic structs
//!    ├── Reject enums and packed structs
//!    ├── Reject unsupported heap-owning types without #[mod_field(skip)]
//!    └── Reject colliding symbol or accessor names
//!         │
//!         ▼
//!    Field Classification (classify_type)
//!    ├── Bool: bool (Exposed across extern "C" as u8: 0 / non-zero)
//!    ├── Primitive: f32, f64, i8..i64, u8..u64, usize, isize, FfiEntity (AbiPod)
//!    ├── HostString: String (Host-allocated)
//!    ├── HostVec: Vec<T: AbiPod> (Host-allocated)
//!    └── Nested: Compound struct / array (Interior pointer accessors)
//!         │
//!         ▼
//!    Code Generation (expand_mod_exposable)
//!    ├── VTable Struct Definition (<Struct>VTable { size, align, ... })
//!    └── Hygienic `const _: () = { ... };` Scope
//!        ├── Extern "C" field accessors (<Prefix>_get_<field>, <Prefix>_set_<field>)
//!        ├── Compile-time layout assertions
//!        ├── Inherent `VTABLE` and `vtable()` methods
//!        ├── Conditional `FfiLayout` and `AbiPod` implementations
//!        └── `ExposableComponent` implementation and static symbol table
//! ```
//!
//! ## Attribute Reference
//!
//! ### Struct-Level Attributes (`#[mod_component(...)]` / `#[mod_exposable(...)]`)
//! - `#[mod_component(name = "...")]`: Specifies a custom component name (e.g. `"game::Position"`),
//!   disambiguating identically named structs across modules.
//! - `#[mod_component(prefix = "...")]`: Custom symbol prefix. Replaces non-alphanumeric characters with `_`.
//! - `#[mod_component(crate = path)]`: Overrides automatic `proc-macro-crate` resolution for the
//!   `bevy_abi_mod_sys` contract module path.
//! - `#[mod_component(export_symbols)]`: Emits `#[unsafe(export_name = "...")]` on generated accessors
//!   for direct `.cdylib` export.
//! - `#[mod_component(no_pod)]`: Opts out of automatic `AbiPod` implementation (`IS_POD = false`) even on `#[repr(C)]` structs.
//! - `#[mod_component(opaque)]`: Marks the struct as VTable-only (`IS_RAW_INSERTABLE = false`, `IS_POD = false`; omits both `FfiLayout` and `AbiPod`), allowing nested fields that implement `ExposableComponent` without `FfiLayout`.
//!
//! ### Field-Level Attributes (`#[mod_field(...)]`)
//! - `#[mod_field(skip)]`: Skips field accessor generation (mandatory for host-private fields, closures,
//!   raw pointers, or Rust collections like `Option<T>`, `Box<T>`, `HashMap<K, V>`).
//! - `#[mod_field(primitive)]`: Forces classification as primitive value type.
//! - `#[mod_field(string)]`: Forces classification as host-owned `String`.
//! - `#[mod_field(vec)]`: Forces classification as host-owned `Vec<T>`.
//! - `#[mod_field(nested)]`: Forces classification as nested compound struct exposed via interior pointers
//!   (`ptr_<field>` and `ptr_mut_<field>`).
//!
//! ## Self-Aliasing Detection in Collection Setters
//!
//! When guest code invokes a setter for a host-allocated collection (`String` or `Vec<T>`), it may pass
//! a pointer to memory that resides inside the component's own existing heap buffer:
//!
//! ```text
//! Component Buffer: [ "Hello World" ]
//!                     ▲       ▲
//!                     │       └── Guest passes subslice [6..11]: "World"
//!                     └────────── Existing buffer start
//! ```
//!
//! If the host setter naively cleared the buffer before reading the incoming slice, it would
//! deallocate or truncate the backing storage before copying from it, causing a use-after-free
//! or data corruption.
//!
//! Generated setters detect interval overlap before mutating:
//! 1. Read the existing buffer address interval `[buf_start .. buf_end)`.
//! 2. Compute the incoming source interval `[src_start .. src_end)`.
//! 3. Check interval intersection (`src_start < buf_end && buf_start < src_end`).
//! 4. Allocate a replacement buffer first when overlapping, or mutate in place when disjoint.

mod abi_pod;
mod attrs;
mod classify;
mod codegen;

use proc_macro::TokenStream;
use syn::{DeriveInput, parse_macro_input};

/// Derive macro for standalone `#[repr(C)]` / `#[repr(transparent)]` Plain Old Data (`AbiPod`) types.
///
/// Enforces at compile time that:
/// 1. The target is a non-generic struct annotated with `#[repr(C)]` or `#[repr(transparent)]` (and not `packed`).
/// 2. Every field implements `AbiPod` (`bool`, `String`, `Vec<T>`, and `#[repr(Rust)]` structs are rejected).
/// 3. The struct implements `Copy` and has no drop glue (`!core::mem::needs_drop::<Self>()`).
///
/// Note: Host components using `#[derive(ModExposable)]` with `#[repr(C)]` and all-POD fields
/// automatically implement `FfiLayout` and `AbiPod` without needing a separate `#[derive(AbiPod)]`.
#[proc_macro_derive(AbiPod, attributes(mod_component, mod_exposable))]
pub fn derive_abi_pod(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    match abi_pod::expand_abi_pod(&input) {
        Ok(ts) => ts.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

/// Derive macro for `ModExposable` components.
#[proc_macro_derive(ModExposable, attributes(mod_component, mod_exposable, mod_field))]
pub fn derive_mod_exposable(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    match codegen::expand_mod_exposable(&input) {
        Ok(ts) => ts.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

#[cfg(test)]
mod tests {
    use crate::abi_pod::expand_abi_pod;
    use crate::attrs::resolve_sys_crate_with;
    use crate::codegen::expand_mod_exposable;
    use proc_macro_crate::FoundCrate;
    use syn::{DeriveInput, Path, parse_quote};

    #[test]
    fn test_rejects_generic_structs() {
        let input: DeriveInput = parse_quote! {
            pub struct GenericComp<T> {
                pub value: T,
            }
        };
        let err = expand_mod_exposable(&input).unwrap_err();
        assert!(err.to_string().contains("does not support generic structs"));
    }

    #[test]
    fn test_rejects_enums() {
        let input: DeriveInput = parse_quote! {
            pub enum StateComp {
                Idle,
                Running,
            }
        };
        let err = expand_mod_exposable(&input).unwrap_err();
        assert!(err.to_string().contains("can only be applied to structs"));
    }

    #[test]
    fn test_rejects_packed_structs() {
        let input: DeriveInput = parse_quote! {
            #[repr(C, packed)]
            pub struct PackedComp {
                pub x: u8,
                pub y: u32,
            }
        };
        let err = expand_mod_exposable(&input).unwrap_err();
        assert!(err.to_string().contains("packed"));
    }

    #[test]
    fn test_rejects_nested_field_name_collision() {
        let input: DeriveInput = parse_quote! {
            pub struct CollidingNested {
                pub x: SubVec,
                pub mut_x: SubVec,
            }
        };
        let err = expand_mod_exposable(&input).unwrap_err();
        assert!(
            err.to_string()
                .contains("colliding interior-pointer accessor name")
        );
    }

    #[test]
    fn test_rejects_heap_owning_types_without_skip() {
        for bad_input in [
            parse_quote! { pub struct Bad1 { pub opt: Option<String> } },
            parse_quote! { pub struct Bad2 { pub boxed: Box<u32> } },
            parse_quote! { pub struct Bad3 { pub map: HashMap<u32, u32> } },
            parse_quote! { pub struct Bad4 { pub r: &'static str } },
        ] {
            let err = expand_mod_exposable(&bad_input).unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("mod_field(skip)") || msg.contains("cannot be exposed"),
                "unexpected error message: {msg}"
            );
        }
    }

    #[test]
    fn test_rejects_unknown_attributes() {
        let input: DeriveInput = parse_quote! {
            #[mod_component(nonexistent_attr)]
            pub struct BadAttr {
                pub x: f32,
            }
        };
        let err = expand_mod_exposable(&input).unwrap_err();
        assert!(err.to_string().contains("unsupported `#[mod_component"));
    }

    #[test]
    fn test_bool_generates_u8_accessors_and_omits_ffi_layout() {
        let input: DeriveInput = parse_quote! {
            #[repr(C)]
            pub struct Flagged {
                pub active: bool,
            }
        };
        let ts = expand_mod_exposable(&input).unwrap().to_string();
        assert!(ts.contains("-> u8"));
        assert!(ts.contains("val : u8"));
        assert!(ts.contains("IS_POD : bool = false"));
        assert!(ts.contains("IS_RAW_INSERTABLE : bool = false"));
        assert!(!ts.contains("impl :: bevy_abi_mod_sys :: FfiLayout for Flagged"));
        assert!(!ts.contains("impl :: bevy_abi_mod_sys :: AbiPod for Flagged"));
    }

    #[test]
    fn test_string_and_vec_omit_ffi_layout_and_pod() {
        for input in [
            parse_quote! { pub struct WithStr { pub s: String } },
            parse_quote! { pub struct WithVec { pub v: Vec<f32> } },
            parse_quote! { pub struct WithSkip { pub x: f32, #[mod_field(skip)] pub hidden: Option<u32> } },
        ] {
            let ts = expand_mod_exposable(&input).unwrap().to_string();
            assert!(ts.contains("IS_POD : bool = false"));
            assert!(ts.contains("IS_RAW_INSERTABLE : bool = false"));
            assert!(!ts.contains("impl :: bevy_abi_mod_sys :: FfiLayout"));
            assert!(!ts.contains("impl :: bevy_abi_mod_sys :: AbiPod"));
        }
    }

    #[test]
    fn test_repr_c_pod_emits_both_ffi_layout_and_abi_pod() {
        let input: DeriveInput = parse_quote! {
            #[repr(C)]
            pub struct PodPos {
                pub x: f32,
                pub y: f32,
            }
        };
        let ts = expand_mod_exposable(&input).unwrap().to_string();
        assert!(ts.contains("IS_POD : bool = true"));
        assert!(ts.contains("IS_RAW_INSERTABLE : bool = true"));
        assert!(ts.contains("impl :: bevy_abi_mod_sys :: FfiLayout for PodPos"));
        assert!(ts.contains("impl :: bevy_abi_mod_sys :: AbiPod for PodPos"));
    }

    #[test]
    fn test_abi_pod_derive_validation() {
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
        assert!(ts.contains("impl :: bevy_abi_mod_sys :: AbiPod for ValidPod"));
    }

    #[test]
    fn test_m01_crate_path_resolution_and_overrides() {
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
            #[mod_exposable(crate = my_facade::bevy_abi_mod_sys)]
            pub struct CustomCrateComp {
                pub x: f32,
            }
        };
        let ts = expand_mod_exposable(&input_with_override)
            .unwrap()
            .to_string();
        assert!(ts.contains("my_facade :: bevy_abi_mod_sys :: ExposableComponent"));
    }

    #[test]
    fn test_m02_hygienic_const_scope_and_export_name() {
        let private_fns: DeriveInput = parse_quote! {
            pub struct LocalPos {
                pub x: f32,
            }
        };
        let ts_private = expand_mod_exposable(&private_fns).unwrap().to_string();
        assert!(ts_private.contains("const _ : () ="));
        assert!(!ts_private.contains("export_name"));
        assert!(!ts_private.contains("no_mangle"));
        assert!(!ts_private.contains("pub unsafe extern \"C\" fn LocalPos_get_x"));

        let exported_fns: DeriveInput = parse_quote! {
            #[mod_component(export_symbols)]
            pub struct ExportedPos {
                pub x: f32,
            }
        };
        let ts_exported = expand_mod_exposable(&exported_fns).unwrap().to_string();
        assert!(ts_exported.contains("export_name = \"ExportedPos_get_x\""));
        assert!(ts_exported.contains("export_name = \"ExportedPos_set_x\""));
    }
}
