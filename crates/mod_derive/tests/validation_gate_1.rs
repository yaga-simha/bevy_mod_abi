#![deny(unsafe_op_in_unsafe_fn)]
#![deny(improper_ctypes_definitions)]

use abi_contract::{ExposableComponent, FfiSlice};
use core::ffi::c_void;
use mod_derive::ModExposable;

#[derive(Debug, Clone, PartialEq, ModExposable)]
pub struct Vec3 {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

/// Standard `#[repr(Rust)]` struct with mixed field types:
/// primitives of varying alignments, nested compound struct, `String`, and `Vec<u32>`.
#[derive(Debug, Clone, PartialEq, ModExposable)]
pub struct MixedComponent {
    pub active: bool,
    pub health: i32,
    pub position: Vec3,
    pub tag: String,
    pub ticks: u64,
    pub scores: Vec<u32>,
}

/// Tuple struct with heap-allocated `String` field (as used in `NameTag(String)`).
#[derive(Debug, Clone, PartialEq, ModExposable)]
pub struct NameTag(pub String);

#[test]
fn validation_gate_1_vtable_layout_and_field_mutations() {
    let vt = MixedComponent::vtable();
    let vec3_vt = Vec3::vtable();

    // 1. Verify exact size_of byte stride and alignment reporting
    assert_eq!(vt.size, core::mem::size_of::<MixedComponent>());
    assert_eq!(vt.align, core::mem::align_of::<MixedComponent>());
    assert_eq!(
        <MixedComponent as ExposableComponent>::size(),
        core::mem::size_of::<MixedComponent>()
    );
    assert_eq!(
        <MixedComponent as ExposableComponent>::align(),
        core::mem::align_of::<MixedComponent>()
    );
    assert_eq!(vec3_vt.size, core::mem::size_of::<Vec3>());
    assert_eq!(vec3_vt.align, core::mem::align_of::<Vec3>());

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
    };

    let comp_ptr = &mut comp as *mut MixedComponent as *mut c_void;
    let comp_const_ptr = comp_ptr as *const c_void;

    // 2. Verify primitive getters & setters via VTable function pointers
    // SAFETY: `comp_const_ptr` and `comp_ptr` point to a valid, aligned `MixedComponent`.
    unsafe {
        assert!(!(vt.get_active)(comp_const_ptr));
        assert_eq!((vt.get_health)(comp_const_ptr), 100);
        assert_eq!((vt.get_ticks)(comp_const_ptr), 999);

        (vt.set_active)(comp_ptr, true);
        (vt.set_health)(comp_ptr, 250);
        (vt.set_ticks)(comp_ptr, 123_456_789);

        assert!((vt.get_active)(comp_const_ptr));
        assert_eq!((vt.get_health)(comp_const_ptr), 250);
        assert_eq!((vt.get_ticks)(comp_const_ptr), 123_456_789);
    }

    // 3. Verify nested compound struct interior pointers & mutations
    // SAFETY: `comp_const_ptr` and `comp_ptr` point to `comp`, and `ptr_position` / `ptr_mut_position`
    // return valid interior pointers to `comp.position` (`Vec3`).
    unsafe {
        let pos_const_ptr = (vt.ptr_position)(comp_const_ptr);
        assert!(!pos_const_ptr.is_null());
        assert_eq!((vec3_vt.get_x)(pos_const_ptr), 1.0);
        assert_eq!((vec3_vt.get_y)(pos_const_ptr), 2.0);
        assert_eq!((vec3_vt.get_z)(pos_const_ptr), 3.0);

        let pos_mut_ptr = (vt.ptr_mut_position)(comp_ptr);
        assert!(!pos_mut_ptr.is_null());
        (vec3_vt.set_x)(pos_mut_ptr, -42.5);
        (vec3_vt.set_y)(pos_mut_ptr, 100.25);
        (vec3_vt.set_z)(pos_mut_ptr, 0.5);
    }
    assert_eq!(
        comp.position,
        Vec3 {
            x: -42.5,
            y: 100.25,
            z: 0.5
        }
    );

    // 4. Verify String getter & host-allocated setter
    // SAFETY: `comp_const_ptr` and `comp_ptr` point to valid `comp`.
    unsafe {
        let slice: FfiSlice<u8> = (vt.get_tag)(comp_const_ptr);
        assert_eq!(slice.as_str().unwrap(), "initial_player");

        let updated = "mutated_across_c_abi_boundary_with_long_heap_allocation";
        (vt.set_tag)(comp_ptr, updated.as_ptr(), updated.len());

        let slice_after = (vt.get_tag)(comp_const_ptr);
        assert_eq!(slice_after.as_str().unwrap(), updated);

        // Verify defensive UTF-8 validation: invalid UTF-8 must NOT corrupt the host String
        let invalid_utf8: [u8; 4] = [0xFF, 0xFE, 0xFD, 0x80];
        (vt.set_tag)(comp_ptr, invalid_utf8.as_ptr(), invalid_utf8.len());
        assert_eq!((vt.get_tag)(comp_const_ptr).as_str().unwrap(), updated);
    }

    // 5. Verify Vec<u32> getter & host-allocated setter
    // SAFETY: `comp_const_ptr` and `comp_ptr` point to valid `comp`.
    unsafe {
        let scores_slice = (vt.get_scores)(comp_const_ptr);
        assert_eq!(scores_slice.as_slice(), &[10, 20, 30]);

        let new_scores: [u32; 5] = [1, 1, 2, 3, 5];
        (vt.set_scores)(comp_ptr, new_scores.as_ptr(), new_scores.len());
        assert_eq!((vt.get_scores)(comp_const_ptr).as_slice(), &[1, 1, 2, 3, 5]);
    }

    // 6. Verify defensive null-pointer handling (zero UB on null inputs)
    // SAFETY: Testing defensive null checks on all generated extern "C-unwind" functions.
    unsafe {
        assert!(!(vt.get_active)(core::ptr::null()));
        assert_eq!((vt.get_health)(core::ptr::null()), 0);
        (vt.set_health)(core::ptr::null_mut(), 999);
        assert!((vt.ptr_position)(core::ptr::null()).is_null());
        assert!((vt.ptr_mut_position)(core::ptr::null_mut()).is_null());
        assert!((vt.get_tag)(core::ptr::null()).is_empty());
        (vt.set_tag)(core::ptr::null_mut(), b"abc".as_ptr(), 3);
        (vt.set_tag)(comp_ptr, core::ptr::null(), 5);
    }
}

#[test]
fn validation_gate_1_byte_stride_array_iteration_and_symbol_resolution() {
    let vt = MixedComponent::vtable();
    let vec3_vt = Vec3::vtable();

    // Resolve accessors dynamically via `ExposableComponent::resolve_symbol` (simulating `get_proc_address`)
    let resolved_vt_ptr =
        MixedComponent::resolve_symbol("MixedComponent_vtable").expect("vtable symbol")
            as *const MixedComponentVTable;
    assert_eq!(resolved_vt_ptr, MixedComponent::vtable_ptr());

    let get_health_ptr = MixedComponent::resolve_symbol("MixedComponent_get_health")
        .expect("MixedComponent_get_health symbol");
    // SAFETY: `MixedComponent_get_health` has signature `unsafe extern "C-unwind" fn(*const c_void) -> i32`.
    let resolved_get_health: unsafe extern "C-unwind" fn(*const c_void) -> i32 =
        unsafe { core::mem::transmute(get_health_ptr) };

    let mut column: Vec<MixedComponent> = (0..64)
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
        })
        .collect();

    let base_ptr = column.as_mut_ptr() as *mut u8;
    let stride = vt.size;

    // Iterate over contiguous array using byte-stride pointer arithmetic
    for i in 0..column.len() {
        // SAFETY: `i < column.len()` and `stride == size_of::<MixedComponent>()`,
        // so `elem_ptr` points to the `i`-th valid `MixedComponent` in `column`.
        unsafe {
            let elem_ptr = base_ptr.add(i * stride) as *mut c_void;
            let current_hp = resolved_get_health(elem_ptr as *const c_void);
            assert_eq!(current_hp, (i as i32) * 10);

            (vt.set_health)(elem_ptr, current_hp + 5);

            let pos_ptr = (vt.ptr_mut_position)(elem_ptr);
            let cur_x = (vec3_vt.get_x)(pos_ptr as *const c_void);
            (vec3_vt.set_x)(pos_ptr, cur_x + 100.0);

            let new_tag = format!("updated_{i}");
            (vt.set_tag)(elem_ptr, new_tag.as_ptr(), new_tag.len());
        }
    }

    for (i, item) in column.iter().enumerate() {
        assert_eq!(item.health, (i as i32) * 10 + 5);
        assert_eq!(item.position.x, (i as f32) + 100.0);
        assert_eq!(item.tag, format!("updated_{i}"));
    }

    // Also test tuple struct `NameTag(String)`
    let mut name_tag = NameTag(String::from("hero"));
    let name_tag_vt = NameTag::vtable();
    let nt_ptr = &mut name_tag as *mut NameTag as *mut c_void;
    // SAFETY: `nt_ptr` points to a valid `NameTag`.
    unsafe {
        assert_eq!(
            (name_tag_vt.get_0)(nt_ptr as *const c_void)
                .as_str()
                .unwrap(),
            "hero"
        );
        let replacement = "legendary_hero";
        (name_tag_vt.set_0)(nt_ptr, replacement.as_ptr(), replacement.len());
    }
    assert_eq!(name_tag.0, "legendary_hero");
}
