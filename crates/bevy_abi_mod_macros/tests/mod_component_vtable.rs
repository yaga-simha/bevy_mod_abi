#![allow(
    clippy::undocumented_unsafe_blocks,
    clippy::ptr_as_ptr,
    clippy::cast_ptr_alignment,
    clippy::transmute_ptr_to_ptr,
    unreachable_pub
)]

use bevy_abi_mod_macros::{AbiPod, ModComponent};
use bevy_abi_mod_sys::{
    ABI_VERSION, AbiPod, BAM_COMPONENT_FLAG_IMMUTABLE, BAM_COMPONENT_FLAG_SPARSE_SET,
    BAM_COMPONENT_FLAG_SUMMARY_TICK, ComponentUuid, FfiBool, FfiEntity, FfiOption, FfiResult,
    FfiSlice, ModComponent, RawInsertable, StrView, read_descriptor,
};
use core::ffi::c_void;

#[derive(Debug, Clone, PartialEq, ModComponent)]
pub struct Vec3 {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

/// Standard `#[repr(Rust)]` struct with mixed field types:
/// `bool`, fixed-width primitives, nested `Vec3`, `String`, `Vec<u32>`,
/// and a host-private `Option<String>` skipped via `#[mod_field(skip)]`.
#[derive(Debug, Clone, PartialEq, ModComponent)]
#[mod_component(export_symbols)]
pub struct MixedComponent {
    pub active: bool,
    pub health: i32,
    pub position: Vec3,
    pub tag: String,
    pub ticks: u64,
    pub scores: Vec<u32>,
    #[mod_field(skip)]
    pub host_private_cache: Option<String>,
}

/// Tuple struct with heap-allocated `String` field.
#[derive(Debug, Clone, PartialEq, ModComponent)]
pub struct NameTag(pub String);

mod physics {
    use bevy_abi_mod_macros::ModComponent;

    #[derive(Debug, Clone, PartialEq, ModComponent)]
    #[mod_component(name = "physics::Position", export_symbols)]
    pub struct Position {
        pub x: f32,
        pub y: f32,
    }
}

mod ui {
    use bevy_abi_mod_macros::ModComponent;

    #[derive(Debug, Clone, PartialEq, ModComponent)]
    #[mod_component(namespace = "ui")]
    pub struct Position {
        pub left: f32,
        pub top: f32,
    }
}

#[test]
fn vtable_header_and_error_returning_setters() {
    let vt = MixedComponent::vtable();
    let vec3_vt = Vec3::vtable();

    // 1. Verify VTableHeader at offset 0 and ExtensibleStruct descriptor reading
    assert_eq!(core::mem::offset_of!(MixedComponentVTable, header), 0);
    assert_eq!(
        vt.header.struct_size as usize,
        core::mem::size_of::<MixedComponentVTable>()
    );
    assert_eq!(vt.header.abi_version, ABI_VERSION);
    assert_eq!(vt.header.uuid, MixedComponent::UUID);
    assert_eq!(vt.header.size, core::mem::size_of::<MixedComponent>());
    assert_eq!(vt.header.align, core::mem::align_of::<MixedComponent>());
    assert_eq!(
        vt.header.validate(
            core::mem::size_of::<MixedComponentVTable>(),
            MixedComponent::UUID
        ),
        FfiResult::OK
    );

    unsafe {
        let copy: MixedComponentVTable = read_descriptor(MixedComponent::erased_vtable_ptr())
            .expect("read_descriptor on MixedComponentVTable");
        assert_eq!(copy.header, vt.header);
    }

    let mut comp = MixedComponent {
        active: false,
        health: 100,
        position: Vec3 {
            x: 1.0,
            y: 2.0,
            z: 3.0,
        },
        tag: String::from("initial_player"),
        ticks: 999,
        scores: vec![10, 20, 30],
        host_private_cache: Some(String::from("kept_on_host")),
    };

    let comp_ptr = (&raw mut comp).cast::<c_void>();
    let comp_const_ptr = comp_ptr as *const c_void;

    // 2. Verify FfiBool and primitive getters & FfiResult-returning setters
    unsafe {
        assert_eq!((vt.get_active.unwrap())(comp_const_ptr), FfiBool::FALSE);
        assert_eq!((vt.get_health.unwrap())(comp_const_ptr), 100);
        assert_eq!((vt.get_ticks.unwrap())(comp_const_ptr), 999);

        assert_eq!(
            (vt.set_active.unwrap())(comp_ptr, FfiBool::TRUE),
            FfiResult::OK
        );
        assert_eq!((vt.set_health.unwrap())(comp_ptr, 250), FfiResult::OK);
        assert_eq!(
            (vt.set_ticks.unwrap())(comp_ptr, 123_456_789),
            FfiResult::OK
        );

        assert_eq!((vt.get_active.unwrap())(comp_const_ptr), FfiBool::TRUE);
        assert!(comp.active);
        assert_eq!((vt.get_health.unwrap())(comp_const_ptr), 250);
        assert_eq!((vt.get_ticks.unwrap())(comp_const_ptr), 123_456_789);

        // Non-canonical C boolean bytes (2 and 255) normalize cleanly to `true` via `FfiBool::get()`
        assert_eq!(
            (vt.set_active.unwrap())(comp_ptr, FfiBool(2)),
            FfiResult::OK
        );
        assert!(comp.active);
        assert_eq!((vt.get_active.unwrap())(comp_const_ptr), FfiBool::TRUE);

        assert_eq!(
            (vt.set_active.unwrap())(comp_ptr, FfiBool(255)),
            FfiResult::OK
        );
        assert!(comp.active);
        assert_eq!(
            (vt.set_active.unwrap())(comp_ptr, FfiBool::FALSE),
            FfiResult::OK
        );
        assert!(!comp.active);
    }

    // 3. Verify nested compound struct interior pointers & mutations
    unsafe {
        let pos_const_ptr = (vt.ptr_position.unwrap())(comp_const_ptr);
        assert!(!pos_const_ptr.is_null());
        assert_eq!((vec3_vt.get_x.unwrap())(pos_const_ptr), 1.0);
        assert_eq!((vec3_vt.get_y.unwrap())(pos_const_ptr), 2.0);
        assert_eq!((vec3_vt.get_z.unwrap())(pos_const_ptr), 3.0);

        let pos_mut_ptr = (vt.ptr_mut_position.unwrap())(comp_ptr);
        assert!(!pos_mut_ptr.is_null());
        assert_eq!((vec3_vt.set_x.unwrap())(pos_mut_ptr, -42.5), FfiResult::OK);
        assert_eq!((vec3_vt.set_y.unwrap())(pos_mut_ptr, 100.25), FfiResult::OK);
        assert_eq!((vec3_vt.set_z.unwrap())(pos_mut_ptr, 0.5), FfiResult::OK);
    }
    assert_eq!(
        comp.position,
        Vec3 {
            x: -42.5,
            y: 100.25,
            z: 0.5
        }
    );

    // 4. Verify String getter & StrView setter returning INVALID_UTF8 on bad UTF-8
    unsafe {
        let slice: StrView = (vt.get_tag.unwrap())(comp_const_ptr);
        assert_eq!(slice.try_as_str().unwrap(), "initial_player");

        let updated = "mutated_across_c_abi_boundary_with_long_heap_allocation";
        assert_eq!(
            (vt.set_tag.unwrap())(comp_ptr, StrView::from_str(updated)),
            FfiResult::OK
        );
        assert_eq!(
            (vt.get_tag.unwrap())(comp_const_ptr).try_as_str().unwrap(),
            updated
        );

        let invalid_utf8: [u8; 4] = [0xFF, 0xFE, 0xFD, 0x80];
        let bad_view = StrView::from_slice(&invalid_utf8);
        assert_eq!(
            (vt.set_tag.unwrap())(comp_ptr, bad_view),
            FfiResult::INVALID_UTF8
        );
        assert_eq!(
            (vt.get_tag.unwrap())(comp_const_ptr).try_as_str().unwrap(),
            updated,
            "INVALID_UTF8 setter failure must leave existing host String untouched"
        );

        // Empty C view `(NULL, 0)` clears cleanly with OK
        let c_empty = StrView {
            ptr: core::ptr::null(),
            len: 0,
        };
        assert_eq!((vt.set_tag.unwrap())(comp_ptr, c_empty), FfiResult::OK);
        assert!(comp.tag.is_empty());
    }

    // 5. Verify Vec<u32> getter, FfiSlice<u32> setter, and push_scores
    unsafe {
        let scores_slice = (vt.get_scores.unwrap())(comp_const_ptr);
        assert_eq!(scores_slice.as_slice(), &[10, 20, 30]);

        let new_scores: [u32; 5] = [1, 1, 2, 3, 5];
        assert_eq!(
            (vt.set_scores.unwrap())(comp_ptr, FfiSlice::from_slice(&new_scores)),
            FfiResult::OK
        );
        assert_eq!(
            (vt.get_scores.unwrap())(comp_const_ptr).as_slice(),
            &[1, 1, 2, 3, 5]
        );

        assert_eq!((vt.push_scores.unwrap())(comp_ptr, 8), FfiResult::OK);
        assert_eq!((vt.push_scores.unwrap())(comp_ptr, 13), FfiResult::OK);
        assert_eq!(comp.scores, vec![1, 1, 2, 3, 5, 8, 13]);
    }

    // 6. Verify NULL_POINTER and INVALID_ARGUMENT checks across all setters & getters
    let unaligned_mut = core::ptr::without_provenance_mut::<c_void>(1);
    let unaligned_const = core::ptr::without_provenance::<c_void>(1);
    unsafe {
        assert_eq!((vt.get_active.unwrap())(core::ptr::null()), FfiBool::FALSE);
        assert_eq!((vt.get_active.unwrap())(unaligned_const), FfiBool::FALSE);
        assert_eq!(
            (vt.set_active.unwrap())(core::ptr::null_mut(), FfiBool::TRUE),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            (vt.set_active.unwrap())(unaligned_mut, FfiBool::TRUE),
            FfiResult::INVALID_ARGUMENT
        );

        assert_eq!((vt.get_health.unwrap())(core::ptr::null()), 0);
        assert_eq!((vt.get_health.unwrap())(unaligned_const), 0);
        assert_eq!(
            (vt.set_health.unwrap())(core::ptr::null_mut(), 999),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            (vt.set_health.unwrap())(unaligned_mut, 999),
            FfiResult::INVALID_ARGUMENT
        );

        assert!((vt.ptr_position.unwrap())(core::ptr::null()).is_null());
        assert!((vt.ptr_position.unwrap())(unaligned_const).is_null());
        assert!((vt.ptr_mut_position.unwrap())(core::ptr::null_mut()).is_null());
        assert!((vt.ptr_mut_position.unwrap())(unaligned_mut).is_null());

        assert!((vt.get_tag.unwrap())(core::ptr::null()).is_empty());
        assert_eq!(
            (vt.set_tag.unwrap())(core::ptr::null_mut(), StrView::from_str("abc")),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            (vt.set_tag.unwrap())(
                comp_ptr,
                StrView {
                    ptr: core::ptr::null(),
                    len: 5,
                }
            ),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            (vt.set_tag.unwrap())(unaligned_mut, StrView::from_str("abc")),
            FfiResult::INVALID_ARGUMENT
        );

        assert_eq!(
            (vt.set_scores.unwrap())(core::ptr::null_mut(), FfiSlice::from_slice(&[1u32, 2])),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            (vt.set_scores.unwrap())(
                comp_ptr,
                FfiSlice {
                    ptr: core::ptr::null(),
                    len: 4,
                }
            ),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            (vt.set_scores.unwrap())(
                comp_ptr,
                FfiSlice {
                    ptr: core::ptr::without_provenance::<u32>(1),
                    len: 2,
                }
            ),
            FfiResult::INVALID_ARGUMENT
        );
        assert_eq!(
            (vt.push_scores.unwrap())(core::ptr::null_mut(), 42),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            (vt.push_scores.unwrap())(unaligned_mut, 42),
            FfiResult::INVALID_ARGUMENT
        );
    }

    assert_eq!(
        comp.host_private_cache.as_deref(),
        Some("kept_on_host"),
        "skipped field must remain untouched"
    );
}

#[test]
fn self_aliasing_string_and_vec_setters() {
    let vt = MixedComponent::vtable();
    let mut comp = MixedComponent {
        active: true,
        health: 10,
        position: Vec3 {
            x: 0.0,
            y: 0.0,
            z: 0.0,
        },
        tag: String::from("hello_world_self_aliasing"),
        ticks: 1,
        scores: vec![10, 20, 30, 40, 50],
        host_private_cache: None,
    };
    let ptr = (&raw mut comp).cast::<c_void>();

    unsafe {
        let full_slice = (vt.get_tag.unwrap())(ptr as *const c_void);
        assert_eq!((vt.set_tag.unwrap())(ptr, full_slice), FfiResult::OK);
        assert_eq!(comp.tag, "hello_world_self_aliasing");

        let sub_slice = (vt.get_tag.unwrap())(ptr as *const c_void);
        let interior = StrView {
            ptr: sub_slice.ptr.add(6),
            len: 5,
        };
        assert_eq!((vt.set_tag.unwrap())(ptr, interior), FfiResult::OK);
        assert_eq!(comp.tag, "world");

        let vec_slice = (vt.get_scores.unwrap())(ptr as *const c_void);
        assert_eq!((vt.set_scores.unwrap())(ptr, vec_slice), FfiResult::OK);
        assert_eq!(comp.scores, vec![10, 20, 30, 40, 50]);

        let vec_sub = (vt.get_scores.unwrap())(ptr as *const c_void);
        let sub_view = FfiSlice {
            ptr: vec_sub.ptr.add(1),
            len: 3,
        };
        assert_eq!((vt.set_scores.unwrap())(ptr, sub_view), FfiResult::OK);
        assert_eq!(comp.scores, vec![20, 30, 40]);
    }
}

#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, ModComponent)]
#[mod_component(name = "game::LayoutCheck")]
pub struct LayoutCheckF32 {
    pub x: f32,
    pub y: f32,
}

#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, ModComponent)]
#[mod_component(name = "game::LayoutCheck")]
pub struct LayoutCheckI32 {
    pub x: i32,
    pub y: i32,
}

#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, ModComponent)]
#[mod_component(name = "game::LayoutCheck")]
pub struct LayoutCheckSwapped {
    pub y: f32,
    pub x: f32,
}

#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, ModComponent)]
#[mod_component(
    name = "game::ExplicitUuidComp",
    uuid = "01234567-89ab-cdef-0123-456789abcdef",
    sparse_set,
    immutable
)]
pub struct ExplicitUuidComp {
    pub id: u64,
}

#[test]
fn namespaced_canonical_names_symbols_and_uuid_fingerprints() {
    // Default canonical name uses `module_path!()::StructName`
    assert_eq!(
        MixedComponent::CANONICAL_NAME,
        "mod_component_vtable::MixedComponent"
    );
    assert_eq!(
        MixedComponent::COMPONENT_CSTR.to_str().unwrap(),
        "mod_component_vtable::MixedComponent"
    );
    assert_eq!(
        MixedComponent::UUID,
        ComponentUuid::from_canonical_name("mod_component_vtable::MixedComponent")
    );

    // Namespaced `"bam.v0.<canonical_name>.*"` symbol entries
    let vt_sym =
        MixedComponent::resolve_symbol("bam.v0.mod_component_vtable::MixedComponent.vtable")
            .expect("bam.v0.*.vtable symbol");
    assert_eq!(vt_sym, MixedComponent::erased_vtable_ptr());
    assert!(
        MixedComponent::resolve_symbol("bam.v0.mod_component_vtable::MixedComponent.get_health")
            .is_some()
    );
    assert!(
        MixedComponent::resolve_symbol("bam.v0.mod_component_vtable::MixedComponent.push_scores")
            .is_some()
    );

    // Explicit `name = "physics::Position"` vs `namespace = "ui"`
    assert_eq!(physics::Position::CANONICAL_NAME, "physics::Position");
    assert_eq!(ui::Position::CANONICAL_NAME, "ui::Position");
    assert_ne!(physics::Position::UUID, ui::Position::UUID);
    assert!(physics::Position::resolve_symbol("bam.v0.physics::Position.vtable").is_some());
    assert!(physics::Position::resolve_symbol("bam.v0.physics::Position.get_x").is_some());
    assert!(ui::Position::resolve_symbol("bam.v0.ui::Position.vtable").is_some());
    assert!(ui::Position::resolve_symbol("bam.v0.ui::Position.get_left").is_some());

    // Field schema hash: identical canonical name, size (8), and align (4),
    // yet changing field types (`f32` -> `i32`) or field order (`(x, y)` -> `(y, x)`) changes UUID!
    let expected_schema = ComponentUuid::hash_field_schema(
        ComponentUuid::hash_field_schema(0, "x", "f32", 0),
        "y",
        "f32",
        4,
    );
    assert_eq!(
        LayoutCheckF32::UUID,
        ComponentUuid::from_parts("game::LayoutCheck", 8, 4, expected_schema)
    );
    assert_ne!(LayoutCheckF32::UUID, LayoutCheckI32::UUID);
    assert_ne!(LayoutCheckF32::UUID, LayoutCheckSwapped::UUID);

    // Explicit `#[mod_component(uuid = "...")]` and component registration flags
    assert_eq!(
        ExplicitUuidComp::UUID,
        ComponentUuid::parse_hex("01234567-89ab-cdef-0123-456789abcdef")
    );
    assert_eq!(
        ExplicitUuidComp::COMPONENT_FLAGS,
        BAM_COMPONENT_FLAG_SPARSE_SET | BAM_COMPONENT_FLAG_IMMUTABLE
    );
}

#[test]
fn byte_stride_array_iteration_and_tuple_struct() {
    let vt = MixedComponent::vtable();
    let vec3_vt = Vec3::vtable();

    let get_health_ptr =
        MixedComponent::resolve_symbol("bam.v0.mod_component_vtable::MixedComponent.get_health")
            .expect("get_health symbol");
    let resolved_get_health: unsafe extern "C" fn(*const c_void) -> i32 =
        unsafe { core::mem::transmute(get_health_ptr) };

    let mut column: Vec<MixedComponent> = (0..32)
        .map(|i| MixedComponent {
            active: i % 2 == 0,
            health: i * 10,
            position: Vec3 {
                x: i as f32,
                y: (i * 2) as f32,
                z: (i * 3) as f32,
            },
            tag: format!("entity_{i}"),
            ticks: i as u64,
            scores: vec![i as u32],
            host_private_cache: None,
        })
        .collect();

    let base_ptr = column.as_mut_ptr().cast::<u8>();
    let stride = vt.header.size;

    for i in 0..column.len() {
        unsafe {
            let elem_ptr = base_ptr.add(i * stride).cast::<c_void>();
            let current_hp = resolved_get_health(elem_ptr as *const c_void);
            assert_eq!(current_hp, (i as i32) * 10);

            assert_eq!(
                (vt.set_health.unwrap())(elem_ptr, current_hp + 5),
                FfiResult::OK
            );

            let pos_ptr = (vt.ptr_mut_position.unwrap())(elem_ptr);
            let cur_x = (vec3_vt.get_x.unwrap())(pos_ptr as *const c_void);
            assert_eq!(
                (vec3_vt.set_x.unwrap())(pos_ptr, cur_x + 100.0),
                FfiResult::OK
            );

            let new_tag = format!("updated_{i}");
            assert_eq!(
                (vt.set_tag.unwrap())(elem_ptr, StrView::from_str(&new_tag)),
                FfiResult::OK
            );
        }
    }

    for (i, item) in column.iter().enumerate() {
        assert_eq!(item.health, (i as i32) * 10 + 5);
        assert_eq!(item.position.x, (i as f32) + 100.0);
        assert_eq!(item.tag, format!("updated_{i}"));
    }

    let mut name_tag = NameTag(String::from("hero"));
    let name_tag_vt = NameTag::vtable();
    let nt_ptr = (&raw mut name_tag).cast::<c_void>();
    unsafe {
        assert_eq!(
            (name_tag_vt.get_0.unwrap())(nt_ptr as *const c_void)
                .try_as_str()
                .unwrap(),
            "hero"
        );
        assert_eq!(
            (name_tag_vt.set_0.unwrap())(nt_ptr, StrView::from_str("legendary_hero")),
            FfiResult::OK
        );
    }
    assert_eq!(name_tag.0, "legendary_hero");
}

#[repr(C)]
#[derive(Debug, Copy, Clone, PartialEq, ModComponent)]
pub struct PodPosition {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, PartialEq, ModComponent)]
pub struct PodEntityTransform {
    pub target: FfiEntity,
    pub origin: PodPosition,
    pub weights: [f32; 4],
}

#[repr(C)]
#[derive(Debug, Copy, Clone, PartialEq, ModComponent)]
pub struct BoolFlagReprC {
    pub enabled: bool,
    pub count: u32,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, PartialEq, ModComponent)]
pub struct HostPlatformIndex {
    pub index: usize,
    pub wide_id: u128,
}

#[derive(Debug, Clone, PartialEq, ModComponent)]
#[mod_component(opaque)]
pub struct OpaqueNamedHolder {
    pub name: NameTag,
    pub score: i32,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, PartialEq, AbiPod)]
pub struct StandalonePodPair {
    pub a: u64,
    pub b: f32,
}

#[repr(transparent)]
#[derive(Debug, Copy, Clone, PartialEq, AbiPod)]
pub struct TransparentEntityId(pub FfiEntity);

fn require_raw_insertable<T: RawInsertable>() {}
fn require_abi_pod<T: AbiPod>() {}

#[test]
fn pod_vs_raw_insertable_vs_opaque_traits() {
    const {
        assert!(!MixedComponent::IS_ABI_POD);
        assert!(!MixedComponent::IS_RAW_INSERTABLE);
        assert!(!NameTag::IS_ABI_POD);
        assert!(!NameTag::IS_RAW_INSERTABLE);
        assert!(!BoolFlagReprC::IS_ABI_POD);
        assert!(!BoolFlagReprC::IS_RAW_INSERTABLE);
        assert!(!OpaqueNamedHolder::IS_ABI_POD);
        assert!(!OpaqueNamedHolder::IS_RAW_INSERTABLE);

        // `#[repr(Rust)]` with POD primitives: RawInsertable, not AbiPod
        assert!(!Vec3::IS_ABI_POD);
        assert!(Vec3::IS_RAW_INSERTABLE);

        // `usize` / `u128` in `#[repr(C)]` ModComponent: RawInsertable, not AbiPod
        assert!(!HostPlatformIndex::IS_ABI_POD);
        assert!(HostPlatformIndex::IS_RAW_INSERTABLE);

        // `#[repr(C)]` with all-AbiPod fields: both RawInsertable and AbiPod
        assert!(PodPosition::IS_ABI_POD);
        assert!(PodPosition::IS_RAW_INSERTABLE);
        assert!(PodEntityTransform::IS_ABI_POD);
        assert!(PodEntityTransform::IS_RAW_INSERTABLE);
    }

    require_raw_insertable::<Vec3>();
    require_raw_insertable::<HostPlatformIndex>();
    require_raw_insertable::<PodPosition>();
    require_abi_pod::<PodPosition>();
    require_abi_pod::<PodEntityTransform>();
    require_abi_pod::<StandalonePodPair>();
    require_abi_pod::<TransparentEntityId>();

    let mut xform = PodEntityTransform {
        target: FfiEntity::NULL,
        origin: PodPosition {
            x: 1.0,
            y: 2.0,
            z: 3.0,
        },
        weights: [0.25, 0.25, 0.25, 0.25],
    };
    let xform_vt = PodEntityTransform::vtable();
    let xform_ptr = (&raw mut xform).cast::<c_void>();
    unsafe {
        assert!((xform_vt.get_target.unwrap())(core::ptr::null()).is_null());
        assert!((xform_vt.get_target.unwrap())(xform_ptr as *const c_void).is_null());
        assert_eq!(
            (xform_vt.set_target.unwrap())(xform_ptr, FfiEntity::from_bits(99)),
            FfiResult::OK
        );
        assert_eq!(
            (xform_vt.get_target.unwrap())(xform_ptr as *const c_void),
            FfiEntity::from_bits(99)
        );
    }

    let mut holder = OpaqueNamedHolder {
        name: NameTag(String::from("opaque_init")),
        score: 7,
    };
    let holder_vt = OpaqueNamedHolder::vtable();
    let name_vt = NameTag::vtable();
    let holder_ptr = (&raw mut holder).cast::<c_void>();
    unsafe {
        let inner_name_ptr = (holder_vt.ptr_mut_name.unwrap())(holder_ptr);
        assert_eq!(
            (name_vt.set_0.unwrap())(inner_name_ptr, StrView::from_str("opaque_mutated")),
            FfiResult::OK
        );
    }
    assert_eq!(holder.name.0, "opaque_mutated");
}

mod m06_qualified_paths_and_aliases {
    use bevy_abi_mod_macros::ModComponent;

    pub type LabelAlias = ::std::string::String;
    pub type GenericVecAlias<T> = ::std::vec::Vec<T>;
    pub type ConcreteScoresAlias = ::std::vec::Vec<u32>;
    pub type HealthPoints = i32;
    pub type ActiveFlag = bool;

    /// User-defined type named `String` exposed intentionally via `#[mod_field(nested)]`.
    #[repr(C)]
    #[derive(Debug, Copy, Clone, PartialEq, ModComponent)]
    pub struct String {
        pub code: u32,
    }

    #[derive(Debug, Clone, PartialEq, ModComponent)]
    pub struct QualifiedAndAliased {
        pub qualified_str: ::std::string::String,
        pub qualified_vec: ::std::vec::Vec<f32>,
        #[mod_field(string)]
        pub aliased_str: LabelAlias,
        #[mod_field(vec)]
        pub generic_vec: GenericVecAlias<u64>,
        #[mod_field(vec(u32))]
        pub concrete_vec: ConcreteScoresAlias,
        #[mod_field(primitive)]
        pub hp: HealthPoints,
        #[mod_field(bool)]
        pub active: ActiveFlag,
        #[mod_field(nested)]
        pub custom_string_struct: String,
    }
}

#[test]
fn m06_qualified_std_types_aliases_and_explicit_mod_field_override() {
    use m06_qualified_paths_and_aliases::{QualifiedAndAliased, String as CustomString};

    let mut item = QualifiedAndAliased {
        qualified_str: ::std::string::String::from("init"),
        qualified_vec: vec![1.0, 2.0],
        aliased_str: ::std::string::String::from("alias"),
        generic_vec: vec![100],
        concrete_vec: vec![7, 8],
        hp: 50,
        active: false,
        custom_string_struct: CustomString { code: 12 },
    };

    let vt = QualifiedAndAliased::vtable();
    let custom_vt = CustomString::vtable();
    let ptr = (&raw mut item).cast::<c_void>();

    unsafe {
        assert_eq!(
            (vt.set_qualified_str.unwrap())(ptr, StrView::from_str("qualified_ok")),
            FfiResult::OK
        );
        assert_eq!(
            (vt.set_aliased_str.unwrap())(ptr, StrView::from_str("aliased_ok")),
            FfiResult::OK
        );
        assert_eq!((vt.push_qualified_vec.unwrap())(ptr, 3.5), FfiResult::OK);
        assert_eq!((vt.push_generic_vec.unwrap())(ptr, 200), FfiResult::OK);
        assert_eq!((vt.push_concrete_vec.unwrap())(ptr, 9), FfiResult::OK);
        assert_eq!((vt.set_hp.unwrap())(ptr, 75), FfiResult::OK);
        assert_eq!((vt.set_active.unwrap())(ptr, FfiBool::TRUE), FfiResult::OK);

        let inner = (vt.ptr_mut_custom_string_struct.unwrap())(ptr);
        assert_eq!((custom_vt.set_code.unwrap())(inner, 99), FfiResult::OK);
    }

    assert_eq!(item.qualified_str, "qualified_ok");
    assert_eq!(item.aliased_str, "aliased_ok");
    assert_eq!(item.qualified_vec, vec![1.0, 2.0, 3.5]);
    assert_eq!(item.generic_vec, vec![100, 200]);
    assert_eq!(item.concrete_vec, vec![7, 8, 9]);
    assert_eq!(item.hp, 75);
    assert!(item.active);
    assert_eq!(item.custom_string_struct.code, 99);
}

mod facade_scope {
    pub mod facade {
        pub mod bevy_abi_mod_sys {
            pub use ::bevy_abi_mod_sys::*;
        }
    }

    #[repr(C)]
    #[derive(Copy, Clone, Debug, PartialEq, ::bevy_abi_mod_macros::AbiPod)]
    #[mod_component(crate = self::facade::bevy_abi_mod_sys)]
    pub struct FacadePod {
        pub x: f32,
        pub y: f32,
    }

    #[derive(Debug, Clone, PartialEq, ::bevy_abi_mod_macros::ModComponent)]
    #[mod_component(crate = self::facade::bevy_abi_mod_sys)]
    pub struct FacadeComponent {
        pub pod: FacadePod,
        pub label: String,
        pub active: bool,
    }
}

#[test]
fn crate_override_and_facade_reexport() {
    require_abi_pod::<facade_scope::FacadePod>();

    let mut comp = facade_scope::FacadeComponent {
        pod: facade_scope::FacadePod { x: 3.5, y: -1.5 },
        label: String::from("facade"),
        active: false,
    };
    let vt = facade_scope::FacadeComponent::vtable();
    let ptr = (&raw mut comp).cast::<c_void>();
    unsafe {
        assert_eq!((vt.set_active.unwrap())(ptr, FfiBool::TRUE), FfiResult::OK);
        assert_eq!(
            (vt.set_label.unwrap())(ptr, StrView::from_str("facade_updated")),
            FfiResult::OK
        );
    }
    assert!(comp.active);
    assert_eq!(comp.label, "facade_updated");
}

#[deny(non_snake_case)]
mod m02_submodules {
    pub mod scene_a {
        use bevy_abi_mod_macros::ModComponent;

        #[derive(Debug, Clone, PartialEq, ModComponent)]
        pub struct DuplicateItem {
            pub x: f32,
            pub tag: String,
        }

        #[allow(non_snake_case)]
        pub fn DuplicateItem_get_x() -> i32 {
            42
        }
    }

    pub mod scene_b {
        use bevy_abi_mod_macros::ModComponent;

        #[derive(Debug, Clone, PartialEq, ModComponent)]
        pub struct DuplicateItem {
            pub x: f64,
            pub enabled: bool,
        }
    }
}

#[test]
fn duplicate_struct_names_in_submodules() {
    assert_eq!(m02_submodules::scene_a::DuplicateItem_get_x(), 42);
    assert_ne!(
        m02_submodules::scene_a::DuplicateItem::UUID,
        m02_submodules::scene_b::DuplicateItem::UUID
    );

    let mut a = m02_submodules::scene_a::DuplicateItem {
        x: 1.25,
        tag: String::from("a"),
    };
    let mut b = m02_submodules::scene_b::DuplicateItem {
        x: 9.75,
        enabled: false,
    };

    let vt_a = m02_submodules::scene_a::DuplicateItem::vtable();
    let vt_b = m02_submodules::scene_b::DuplicateItem::vtable();

    let ptr_a = (&raw mut a).cast::<c_void>();
    let ptr_b = (&raw mut b).cast::<c_void>();

    unsafe {
        assert_eq!((vt_a.get_x.unwrap())(ptr_a as *const c_void), 1.25f32);
        assert_eq!((vt_b.get_x.unwrap())(ptr_b as *const c_void), 9.75f64);
        assert_eq!((vt_a.set_x.unwrap())(ptr_a, 5.5f32), FfiResult::OK);
        assert_eq!(
            (vt_b.set_enabled.unwrap())(ptr_b, FfiBool::TRUE),
            FfiResult::OK
        );
    }

    assert_eq!(a.x, 5.5f32);
    assert!(b.enabled);
}

#[test]
fn emits_c_decl_and_zig_decl_for_pod_and_non_pod_components() {
    // 1. `#[repr(C)]` `AbiPod` struct emits full C struct + VTable + static layout assertions,
    //    and Zig `extern struct` + `callconv(.c)` VTable.
    let pod_c = PodPosition::C_DECL;
    assert!(pod_c.contains(
        "typedef struct PodPosition {\n    float x;\n    float y;\n    float z;\n} PodPosition;"
    ));
    assert!(pod_c.contains("typedef struct PodPositionVTable {"));
    assert!(pod_c.contains("float (*get_x)(const void* ptr);"));
    assert!(pod_c.contains("BamResult (*set_x)(void* ptr, float val);"));
    assert!(pod_c.contains("BAM_STATIC_ASSERT(sizeof(PodPositionVTable) == 88u"));

    let pod_zig = PodPosition::ZIG_DECL;
    assert!(pod_zig.contains("pub const PodPosition = extern struct {"));
    assert!(pod_zig.contains("x: f32,"));
    assert!(pod_zig.contains("pub const PodPositionVTable = extern struct {"));
    assert!(pod_zig.contains("callconv(.c)"));

    // 2. Non-POD `MixedComponent` emits opaque forward typedef, Vec slice typedef, and VTable.
    let mixed_c = MixedComponent::C_DECL;
    assert!(mixed_c.contains("typedef struct MixedComponent MixedComponent;"));
    assert!(mixed_c.contains("typedef struct MixedComponentVTable_scores_Slice {"));
    assert!(mixed_c.contains("BamBool   (*get_active)(const void* ptr);"));
    assert!(mixed_c.contains("BamStrView (*get_tag)(const void* ptr);"));
    assert!(mixed_c.contains("size_t (*len_scores)(const void* ptr);"));
    assert!(mixed_c.contains("BamResult (*push_scores)(void* ptr, uint32_t val);"));
    assert!(
        mixed_c
            .contains("BamResult (*remove_scores)(void* ptr, size_t index, uint32_t* out_elem);")
    );
    assert!(mixed_c.contains("BamResult (*reserve_scores)(void* ptr, size_t additional);"));
    assert!(mixed_c.contains("uint32_t* (*as_mut_slice_scores)(void* ptr, size_t* out_len);"));
    assert!(mixed_c.contains("const void* (*ptr_position)(const void* ptr);"));
    assert!(mixed_c.contains("#ifdef __cplusplus"));
    assert!(mixed_c.contains("class MixedComponentView"));
    assert!(mixed_c.contains("HostVecView"));
    assert!(MixedComponent::ZIG_DECL.contains("MixedComponentVTable_scores_VecView"));
    assert!(MixedComponent::ZIG_DECL.contains("pub const MixedComponentView = struct"));
}

#[test]
fn full_host_vec_vtable_operations_and_bounds_safety() {
    let vt = MixedComponent::vtable();
    let mut comp = MixedComponent {
        active: true,
        health: 100,
        position: Vec3 {
            x: 0.0,
            y: 0.0,
            z: 0.0,
        },
        tag: String::from("vec_ops"),
        ticks: 1,
        scores: vec![10, 20, 30, 40],
        host_private_cache: None,
    };
    let ptr = (&raw mut comp).cast::<c_void>();
    let c_ptr = ptr as *const c_void;

    unsafe {
        assert_eq!((vt.len_scores.unwrap())(core::ptr::null()), 0);
        assert_eq!((vt.len_scores.unwrap())(c_ptr), 4);

        // reserve (normal + OOM CAPACITY_EXCEEDED)
        assert_eq!((vt.reserve_scores.unwrap())(ptr, 16), FfiResult::OK);
        assert!(comp.scores.capacity() >= 20);
        assert_eq!(
            (vt.reserve_scores.unwrap())(ptr, usize::MAX),
            FfiResult::CAPACITY_EXCEEDED
        );

        // remove (bounds checked -> OUT_OF_RANGE)
        let mut removed = 0u32;
        assert_eq!(
            (vt.remove_scores.unwrap())(ptr, 99, &raw mut removed),
            FfiResult::OUT_OF_RANGE
        );
        assert_eq!(
            (vt.remove_scores.unwrap())(ptr, 1, core::ptr::null_mut()),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            (vt.remove_scores.unwrap())(ptr, 1, &raw mut removed),
            FfiResult::OK
        );
        assert_eq!(removed, 20);
        assert_eq!(comp.scores, vec![10, 30, 40]);

        // swap_remove
        assert_eq!(
            (vt.swap_remove_scores.unwrap())(ptr, 10, &raw mut removed),
            FfiResult::OUT_OF_RANGE
        );
        assert_eq!(
            (vt.swap_remove_scores.unwrap())(ptr, 0, &raw mut removed),
            FfiResult::OK
        );
        assert_eq!(removed, 10);
        assert_eq!(comp.scores, vec![40, 30]);

        // as_mut_slice in-place mutation
        let mut slice_len = 0usize;
        let mut_ptr = (vt.as_mut_slice_scores.unwrap())(ptr, &raw mut slice_len);
        assert!(!mut_ptr.is_null());
        assert_eq!(slice_len, 2);
        *mut_ptr.add(0) += 5;
        *mut_ptr.add(1) += 5;
        assert_eq!(comp.scores, vec![45, 35]);
        assert!((vt.as_mut_slice_scores.unwrap())(ptr, core::ptr::null_mut()).is_null());

        // pop
        let mut popped = 0u32;
        assert_eq!(
            (vt.pop_scores.unwrap())(ptr, &raw mut popped),
            FfiBool::TRUE
        );
        assert_eq!(popped, 35);
        assert_eq!(comp.scores, vec![45]);

        // truncate + push + clear
        assert_eq!((vt.push_scores.unwrap())(ptr, 99), FfiResult::OK);
        assert_eq!((vt.push_scores.unwrap())(ptr, 100), FfiResult::OK);
        assert_eq!((vt.truncate_scores.unwrap())(ptr, 2), FfiResult::OK);
        assert_eq!(comp.scores, vec![45, 99]);

        assert_eq!((vt.clear_scores.unwrap())(ptr), FfiResult::OK);
        assert_eq!((vt.len_scores.unwrap())(c_ptr), 0);
        assert_eq!(
            (vt.pop_scores.unwrap())(ptr, &raw mut popped),
            FfiBool::FALSE
        );
    }
}

pub type OptU64Alias = FfiOption<u64>;

#[repr(C)]
#[derive(Debug, Copy, Clone, PartialEq, ModComponent)]
#[mod_component(name = "opt::TargetComponent", summary_tick, require(PodPosition))]
pub struct TargetWithOptions {
    pub target_id: FfiOption<u32>,
    pub pos: FfiOption<PodPosition>,
    #[mod_field(option(u64))]
    pub aliased_opt: OptU64Alias,
}

#[test]
fn ffi_option_summary_tick_and_require_metadata() {
    const {
        assert!(TargetWithOptions::IS_ABI_POD);
        assert!(TargetWithOptions::IS_RAW_INSERTABLE);
        assert!(TargetWithOptions::SUMMARY_TICK);
        assert!(TargetWithOptions::COMPONENT_FLAGS == BAM_COMPONENT_FLAG_SUMMARY_TICK);
        assert!(TargetWithOptions::REQUIRED_COMPONENT_NAMES.len() == 1);
    }
    require_abi_pod::<TargetWithOptions>();
    assert_eq!(
        TargetWithOptions::REQUIRED_COMPONENT_NAMES,
        &[PodPosition::CANONICAL_NAME]
    );
    assert_eq!(
        TargetWithOptions::REQUIRED_COMPONENT_UUIDS,
        &[PodPosition::UUID]
    );

    let mut comp = TargetWithOptions {
        target_id: FfiOption::none(),
        pos: FfiOption::some(PodPosition {
            x: 1.0,
            y: 2.0,
            z: 3.0,
        }),
        aliased_opt: FfiOption::none(),
    };
    let vt = TargetWithOptions::vtable();
    let ptr = (&raw mut comp).cast::<c_void>();
    let c_ptr = ptr as *const c_void;

    unsafe {
        assert!((vt.get_target_id.unwrap())(c_ptr).is_none());
        assert_eq!(
            (vt.set_target_id.unwrap())(ptr, FfiOption::some(42)),
            FfiResult::OK
        );
        assert_eq!((vt.get_target_id.unwrap())(c_ptr).into_option(), Some(42));
        assert_eq!((vt.clear_target_id.unwrap())(ptr), FfiResult::OK);
        assert!(comp.target_id.is_none());

        assert_eq!(
            (vt.set_aliased_opt.unwrap())(ptr, FfiOption::some(9001)),
            FfiResult::OK
        );
        assert_eq!(comp.aliased_opt.into_option(), Some(9001));
        assert_eq!((vt.clear_pos.unwrap())(ptr), FfiResult::OK);
        assert!(comp.pos.is_none());
    }

    assert!(
        TargetWithOptions::C_DECL.contains("typedef struct TargetWithOptions_target_id_Option")
    );
    assert!(TargetWithOptions::C_DECL.contains("BamResult (*clear_target_id)(void* ptr);"));
    assert!(TargetWithOptions::ZIG_DECL.contains("pub const TargetWithOptions_target_id_Option"));
}

#[repr(u8)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, ModComponent)]
#[mod_component(name = "ai::FieldlessState")]
pub enum FieldlessState {
    Idle = 0,
    Patrol = 3,
    Alert = 7,
}

#[repr(u8)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, ModComponent)]
#[mod_component(name = "ai::FieldlessState")]
pub enum FieldlessStateShifted {
    Idle = 0,
    Patrol = 4,
    Alert = 7,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, ModComponent)]
#[mod_component(name = "ai::ReprCSigned")]
pub enum ReprCSigned {
    Error = -1,
    Ok = 0,
    Retry = 1,
}

#[repr(C, u8)]
#[derive(Debug, Clone, PartialEq, ModComponent)]
#[mod_component(name = "combat::ActionPayload", export_symbols)]
pub enum ActionPayload {
    Idle = 0,
    Move { speed: f32, sprinting: bool } = 1,
    Cast(String, Vec<u32>, FfiOption<u32>, PodPosition) = 2,
}

#[test]
fn repr_c_fieldless_and_tagged_enums_vtable_and_schema_uuid() {
    const {
        assert!(!FieldlessState::IS_ABI_POD);
        assert!(!FieldlessState::IS_RAW_INSERTABLE);
        assert!(!ReprCSigned::IS_ABI_POD);
        assert!(!ReprCSigned::IS_RAW_INSERTABLE);
        assert!(!ActionPayload::IS_ABI_POD);
        assert!(!ActionPayload::IS_RAW_INSERTABLE);
    }

    // Discriminant change produces a different deterministic schema UUID
    assert_ne!(FieldlessState::UUID, FieldlessStateShifted::UUID);

    // 1. Fieldless #[repr(u8)] enum
    let mut state = FieldlessState::Idle;
    let f_vt = FieldlessState::vtable();
    let f_ptr = (&raw mut state).cast::<c_void>();
    unsafe {
        assert_eq!((f_vt.tag.unwrap())(f_ptr as *const c_void), 0);
        assert_eq!((f_vt.set_patrol.unwrap())(f_ptr), FfiResult::OK);
        assert_eq!(state, FieldlessState::Patrol);
        assert_eq!((f_vt.tag.unwrap())(f_ptr as *const c_void), 3);
        assert_eq!((f_vt.set_alert.unwrap())(f_ptr), FfiResult::OK);
        assert_eq!((f_vt.tag.unwrap())(f_ptr as *const c_void), 7);
    }

    // 2. Fieldless #[repr(C)] enum with negative discriminant
    let mut signed = ReprCSigned::Ok;
    let s_vt = ReprCSigned::vtable();
    let s_ptr = (&raw mut signed).cast::<c_void>();
    unsafe {
        assert_eq!((s_vt.tag.unwrap())(s_ptr as *const c_void), 0);
        assert_eq!((s_vt.set_error.unwrap())(s_ptr), FfiResult::OK);
        assert_eq!(signed, ReprCSigned::Error);
        assert_eq!((s_vt.tag.unwrap())(s_ptr as *const c_void), -1);
    }

    // 3. Tagged #[repr(C, u8)] data enum
    let mut action = ActionPayload::Idle;
    let a_vt = ActionPayload::vtable();
    let a_ptr = (&raw mut action).cast::<c_void>();
    let a_cptr = a_ptr as *const c_void;
    unsafe {
        assert_eq!((a_vt.tag.unwrap())(a_cptr), 0);
        let mut out_speed = 0.0f32;
        assert_eq!(
            (a_vt.try_get_move_speed.unwrap())(a_cptr, &raw mut out_speed),
            FfiResult::INVALID_STATE
        );

        assert_eq!(
            (a_vt.set_move.unwrap())(a_ptr, 12.5, FfiBool::TRUE),
            FfiResult::OK
        );
        assert_eq!((a_vt.tag.unwrap())(a_cptr), 1);
        let mut out_sprint = FfiBool::FALSE;
        assert_eq!(
            (a_vt.try_get_move_speed.unwrap())(a_cptr, &raw mut out_speed),
            FfiResult::OK
        );
        assert_eq!(
            (a_vt.try_get_move_sprinting.unwrap())(a_cptr, &raw mut out_sprint),
            FfiResult::OK
        );
        assert_eq!(out_speed, 12.5);
        assert_eq!(out_sprint, FfiBool::TRUE);

        let spell_args = [10u32, 20, 30];
        let pos = PodPosition {
            x: 1.0,
            y: 2.0,
            z: 3.0,
        };
        assert_eq!(
            (a_vt.set_cast.unwrap())(
                a_ptr,
                StrView::from_str("fireball"),
                FfiSlice::from_slice(&spell_args),
                FfiOption::some(77),
                (&raw const pos).cast::<c_void>(),
            ),
            FfiResult::OK
        );
        assert_eq!((a_vt.tag.unwrap())(a_cptr), 2);

        let mut out_str = StrView::empty();
        assert_eq!(
            (a_vt.try_get_cast_0.unwrap())(a_cptr, &raw mut out_str),
            FfiResult::OK
        );
        assert_eq!(out_str.try_as_str().unwrap(), "fireball");

        // Self-aliasing set_cast reusing existing StrView and FfiSlice must remain UB-free!
        let mut out_slice = FfiSlice::<u32>::empty();
        assert_eq!(
            (a_vt.try_get_cast_1.unwrap())(a_cptr, &raw mut out_slice),
            FfiResult::OK
        );
        assert_eq!(
            (a_vt.set_cast.unwrap())(
                a_ptr,
                out_str,
                out_slice,
                FfiOption::none(),
                (&raw const pos).cast::<c_void>(),
            ),
            FfiResult::OK
        );
        assert_eq!(
            action,
            ActionPayload::Cast(
                String::from("fireball"),
                vec![10, 20, 30],
                FfiOption::none(),
                pos
            )
        );
    }

    assert!(ActionPayload::C_DECL.contains("#define ActionPayload_Tag_Cast ((uint8_t)2)"));
    assert!(ActionPayload::C_DECL.contains("uint8_t (*tag)(const void* ptr);"));
    assert!(
        ActionPayload::C_DECL
            .contains("BamResult (*try_get_move_speed)(const void* ptr, float* out_val);")
    );
    assert!(ActionPayload::C_DECL.contains("class ActionPayloadView"));
    assert!(ActionPayload::ZIG_DECL.contains("pub const ActionPayloadTag = enum(u8)"));
    assert!(ActionPayload::ZIG_DECL.contains("pub const ActionPayloadView = struct"));
}
