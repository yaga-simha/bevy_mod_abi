use crate::abi_pod::expand_abi_pod;
use crate::attrs::{resolve_guest_crate_with, resolve_sys_crate_with};
use crate::codegen::{expand_mod_component, expand_mod_component_with_crate};
use crate::guest_component::{expand_guest_component, expand_guest_component_with_crates};
use proc_macro_crate::FoundCrate;
use quote::quote;
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
fn rejects_invalid_enum_reprs_and_abi_pod_enums() {
    let rust_repr_enum: DeriveInput = parse_quote! {
        pub enum StateComp {
            Idle,
            Running,
        }
    };
    let err = expand_mod_component(&rust_repr_enum).unwrap_err();
    assert!(err.to_string().contains("requires `#[repr("));

    let data_enum_missing_c: DeriveInput = parse_quote! {
        #[repr(u8)]
        pub enum MissingC {
            Idle,
            Move(f32),
        }
    };
    let err = expand_mod_component(&data_enum_missing_c).unwrap_err();
    assert!(err.to_string().contains("#[repr(C,"));

    let empty_enum: DeriveInput = parse_quote! {
        #[repr(u8)]
        pub enum EmptyEnum {}
    };
    assert!(
        expand_mod_component(&empty_enum)
            .unwrap_err()
            .to_string()
            .contains("zero-variant")
    );

    let abi_pod_enum: DeriveInput = parse_quote! {
        #[repr(C, u8)]
        pub enum NotPodEnum {
            A = 0,
            B(u32) = 1,
        }
    };
    assert!(
        expand_abi_pod(&abi_pod_enum)
            .unwrap_err()
            .to_string()
            .contains("can only be applied to structs")
    );
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
    for slot in [
        "pub get_items",
        "pub set_items",
        "pub len_items",
        "pub clear_items",
        "pub push_items",
        "pub pop_items",
        "pub remove_items",
        "pub swap_remove_items",
        "pub truncate_items",
        "pub reserve_items",
        "pub as_mut_slice_items",
    ] {
        assert!(vec_ts.contains(slot), "missing Vec slot `{slot}`");
    }
}

#[test]
fn supports_enums_ffi_option_summary_tick_and_require() {
    let fieldless_enum: DeriveInput = parse_quote! {
        #[repr(u8)]
        pub enum AiMode {
            Idle = 0,
            Patrol = 2,
        }
    };
    let ts_fieldless = expand_mod_component(&fieldless_enum).unwrap().to_string();
    assert!(ts_fieldless.contains("IS_ABI_POD : bool = false"));
    assert!(ts_fieldless.contains("IS_RAW_INSERTABLE : bool = false"));
    assert!(ts_fieldless.contains("pub tag :"));
    assert!(ts_fieldless.contains("pub set_idle :"));
    assert!(ts_fieldless.contains("pub set_patrol :"));
    assert!(!ts_fieldless.contains("unsafe impl :: bevy_abi_mod_sys :: AbiPod"));

    let tagged_enum: DeriveInput = parse_quote! {
        #[repr(C, u8)]
        pub enum Action {
            None = 0,
            Move { speed: f32, active: bool } = 1,
            Speak(String) = 2,
        }
    };
    let ts_tagged = expand_mod_component(&tagged_enum).unwrap().to_string();
    assert!(ts_tagged.contains("pub tag :"));
    assert!(ts_tagged.contains("pub set_none :"));
    assert!(ts_tagged.contains("pub set_move :"));
    assert!(ts_tagged.contains("pub set_speak :"));
    assert!(ts_tagged.contains("pub try_get_move_speed :"));
    assert!(ts_tagged.contains("pub try_get_move_active :"));
    assert!(ts_tagged.contains("pub try_get_speak_0 :"));

    let opt_and_attrs: DeriveInput = parse_quote! {
        #[repr(C)]
        #[mod_component(summary_tick, require(AiMode, Action))]
        pub struct SummaryComp {
            pub target: FfiOption<u32>,
        }
    };
    let ts_opt = expand_mod_component(&opt_and_attrs).unwrap().to_string();
    assert!(ts_opt.contains("BAM_COMPONENT_FLAG_SUMMARY_TICK"));
    assert!(ts_opt.contains("SUMMARY_TICK : bool = true"));
    assert!(ts_opt.contains("REQUIRED_COMPONENT_NAMES"));
    assert!(ts_opt.contains("REQUIRED_COMPONENT_UUIDS"));
    assert!(ts_opt.contains("pub get_target :"));
    assert!(ts_opt.contains("pub set_target :"));
    assert!(ts_opt.contains("pub clear_target :"));
    assert!(ts_opt.contains("IS_ABI_POD : bool = true"));

    let bad_combo: DeriveInput = parse_quote! {
        #[mod_component(sparse_set, summary_tick)]
        pub struct BadCombo {
            pub x: f32,
        }
    };
    assert!(
        expand_mod_component(&bad_combo)
            .unwrap_err()
            .to_string()
            .contains("cannot combine")
    );
}

#[test]
fn emits_witness_symbols_for_non_opaque_and_omits_for_opaque() {
    let non_opaque: DeriveInput = parse_quote! {
        #[mod_component(name = "schema::Item")]
        pub struct Item {
            pub hp: i32,
        }
    };
    let ts_non_opaque = expand_mod_component(&non_opaque).unwrap().to_string();
    assert!(ts_non_opaque.contains("bam.v0.schema::Item.field_info"));
    assert!(ts_non_opaque.contains("bam.v0.schema::Item.export_fields"));
    assert!(ts_non_opaque.contains("bam.v0.schema::Item.import_field"));

    let opaque: DeriveInput = parse_quote! {
        #[mod_component(name = "schema::OpaqueItem", opaque)]
        pub struct OpaqueItem {
            pub hp: i32,
        }
    };
    let ts_opaque = expand_mod_component(&opaque).unwrap().to_string();
    assert!(!ts_opaque.contains("bam.v0.schema::OpaqueItem.field_info"));
    assert!(!ts_opaque.contains("bam.v0.schema::OpaqueItem.export_fields"));
    assert!(!ts_opaque.contains("bam.v0.schema::OpaqueItem.import_field"));
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
            pub _pad: u32,
            pub y: u64,
        }
    };
    let ts = expand_abi_pod(&valid_pod).unwrap().to_string();
    assert!(ts.contains("assert_abi_pod :: < f32 > ()"));
    assert!(ts.contains("assert_abi_pod :: < u64 > ()"));
    assert!(ts.contains("size_of :: < ValidPod > () =="));
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

    assert_eq!(
        resolve_sys_crate_with(None, Some("my_game"), |pkg| {
            (pkg == "bevy_mod_abi").then(|| FoundCrate::Name("bevy_mod_abi".to_owned()))
        })
        .to_string(),
        ":: bevy_mod_abi :: bevy_abi_mod_sys"
    );
    assert_eq!(
        resolve_sys_crate_with(None, Some("bevy_mod_abi"), |pkg| {
            (pkg == "bevy_mod_abi").then_some(FoundCrate::Itself)
        })
        .to_string(),
        ":: bevy_mod_abi :: bevy_abi_mod_sys"
    );
    assert_eq!(
        resolve_sys_crate_with(None, Some("umbrella_reexports"), |pkg| {
            (pkg == "bevy_mod_abi").then_some(FoundCrate::Itself)
        })
        .to_string(),
        ":: bevy_mod_abi :: bevy_abi_mod_sys"
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

    let guest_override: Path = parse_quote!(my_mod::guest_sdk);
    assert_eq!(
        resolve_guest_crate_with(Some(&guest_override), None, |_| None).to_string(),
        "my_mod :: guest_sdk"
    );
    assert_eq!(
        resolve_guest_crate_with(None, Some("bevy_abi_mod_guest"), |pkg| {
            (pkg == "bevy_abi_mod_guest").then_some(FoundCrate::Itself)
        })
        .to_string(),
        "crate"
    );
    assert_eq!(
        resolve_guest_crate_with(None, Some("guest_integration_test"), |pkg| {
            (pkg == "bevy_abi_mod_guest").then_some(FoundCrate::Itself)
        })
        .to_string(),
        ":: bevy_abi_mod_guest"
    );
    assert_eq!(
        resolve_guest_crate_with(None, Some("my_guest_mod"), |pkg| {
            (pkg == "bevy_abi_mod_guest").then(|| FoundCrate::Name("renamed_guest".to_owned()))
        })
        .to_string(),
        ":: renamed_guest"
    );

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

#[test]
fn guest_component_derive_expansion_and_errors() {
    let basic: DeriveInput = parse_quote! {
        #[repr(C)]
        #[guest_component(name = "example::Position", summary_tick, immutable)]
        pub struct Position {
            pub x: f32,
            pub y: f32,
        }
    };
    let ts = expand_guest_component_with_crates(
        &basic,
        &quote!(::bevy_abi_mod_sys),
        &quote!(::bevy_abi_mod_guest),
    )
    .unwrap()
    .to_string();
    assert!(ts.contains("impl :: bevy_abi_mod_guest :: GuestComponent for Position"));
    assert!(ts.contains("c\"example::Position\""));
    assert!(ts.contains("hash_field_schema"));
    assert!(ts.contains("BAM_COMPONENT_FLAG_IMMUTABLE"));
    assert!(ts.contains("BAM_COMPONENT_FLAG_SUMMARY_TICK"));
    assert!(!ts.contains("unsafe impl"));
    assert!(!ts.contains("AbiPod"));
    assert!(!ts.contains("const UUID"));

    let no_hash_and_uuid: DeriveInput = parse_quote! {
        #[repr(transparent)]
        #[guest_component(
            namespace = "net",
            name = "Tick",
            uuid = "11111111-2222-3333-4444-555555555555",
            no_schema_hash,
            sparse_set,
            guest_crate = custom_guest
        )]
        pub struct Tick(pub u32);
    };
    let ts_custom = expand_guest_component(&no_hash_and_uuid)
        .unwrap()
        .to_string();
    assert!(ts_custom.contains("impl custom_guest :: GuestComponent for Tick"));
    assert!(ts_custom.contains("c\"net::Tick\""));
    assert!(ts_custom.contains("FIELD_SCHEMA_HASH : u64 = 0u64"));
    assert!(ts_custom.contains("BAM_COMPONENT_FLAG_SPARSE_SET"));
    assert!(ts_custom.contains("parse_hex (\"11111111-2222-3333-4444-555555555555\")"));

    // Fallback to `#[mod_component(...)]` when `#[guest_component(...)]` is absent.
    let fallback: DeriveInput = parse_quote! {
        #[repr(C)]
        #[mod_component(name = "game::Velocity", summary_tick)]
        pub struct Velocity {
            pub vx: f32,
            pub vy: f32,
        }
    };
    let ts_fallback = expand_guest_component(&fallback).unwrap().to_string();
    assert!(ts_fallback.contains("c\"game::Velocity\""));
    assert!(ts_fallback.contains("BAM_COMPONENT_FLAG_SUMMARY_TICK"));

    // Rejects `sparse_set` + `summary_tick`.
    let bad_flags: DeriveInput = parse_quote! {
        #[repr(C)]
        #[guest_component(sparse_set, summary_tick)]
        pub struct BadFlags {
            pub x: f32,
        }
    };
    assert!(
        expand_guest_component(&bad_flags)
            .unwrap_err()
            .to_string()
            .contains("cannot combine")
    );

    // Rejects enum, generic, packed, missing repr.
    let bad_enum: DeriveInput = parse_quote! {
        #[repr(u8)]
        pub enum BadEnum { A = 0 }
    };
    assert!(
        expand_guest_component(&bad_enum)
            .unwrap_err()
            .to_string()
            .contains("only be applied to structs")
    );
    let bad_repr: DeriveInput = parse_quote! {
        pub struct MissingRepr { pub x: f32 }
    };
    assert!(
        expand_guest_component(&bad_repr)
            .unwrap_err()
            .to_string()
            .contains("requires `#[repr(C)]` or `#[repr(transparent)]`")
    );
}

#[test]
fn mod_component_guest_shorthand_and_non_pod_rejection() {
    let valid: DeriveInput = parse_quote! {
        #[repr(C)]
        #[mod_component(name = "example::Health", summary_tick, guest, guest_crate = my_guest)]
        pub struct Health {
            pub hp: f32,
        }
    };
    let ts = expand_mod_component(&valid).unwrap().to_string();
    assert!(ts.contains("impl my_guest :: GuestComponent for Health"));
    assert!(ts.contains("COMPONENT_CSTR"));
    assert!(ts.contains("COMPONENT_FLAGS"));

    let non_pod_string: DeriveInput = parse_quote! {
        #[repr(C)]
        #[mod_component(name = "example::Bad", guest)]
        pub struct BadString {
            pub label: String,
        }
    };
    assert!(
        expand_mod_component(&non_pod_string)
            .unwrap_err()
            .to_string()
            .contains("requires an `AbiPod` struct")
    );

    let non_pod_no_pod: DeriveInput = parse_quote! {
        #[repr(C)]
        #[mod_component(name = "example::Bad", no_pod, guest)]
        pub struct BadNoPod {
            pub x: f32,
        }
    };
    assert!(
        expand_mod_component(&non_pod_no_pod)
            .unwrap_err()
            .to_string()
            .contains("requires an `AbiPod` struct")
    );

    let bad_enum: DeriveInput = parse_quote! {
        #[repr(u8)]
        #[mod_component(guest)]
        pub enum BadEnum {
            A = 0,
        }
    };
    assert!(
        expand_mod_component(&bad_enum)
            .unwrap_err()
            .to_string()
            .contains("cannot be applied to enums")
    );
}
