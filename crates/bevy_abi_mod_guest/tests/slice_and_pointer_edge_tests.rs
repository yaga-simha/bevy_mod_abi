//! Slice bounds, alignment, ZST, `isize::MAX` overflow, and raw dynamic component destructor
//! edge-case tests (`G-07`, `N-03`).

mod common;

use core::ffi::c_void;
use std::sync::atomic::{AtomicUsize, Ordering};

use bevy_abi_mod_guest::{
    prelude::*,
    raw::{ValidatedSlice, ValidatedSliceMut, register_dynamic_component_with_drop},
};
use common::MockHostWorld;

#[test]
fn slice_alignment_overflow_zst_and_stride_edge_cases() {
    let mut aligned_buf = [10u64, 20u64, 30u64];
    let base = aligned_buf.as_mut_ptr().cast::<u8>();

    // 1. Valid slice: typed borrow, stride pointers, and element access succeed.
    // SAFETY: `base` points to 3 contiguous `u64` elements (`stride = 8`).
    let valid_inner = unsafe { ValidatedSlice::from_raw_parts(base, 3, 8) };
    let valid_ro = ChunkSlice::from_validated(valid_inner);
    assert_eq!(valid_ro.len(), 3);
    assert_eq!(valid_ro.stride(), 8);
    assert_eq!(valid_ro.as_typed_slice::<u64>(), Some(&[10, 20, 30][..]));
    assert!(valid_ro.as_typed_slice::<u32>().is_none());
    assert!(valid_ro.get_ptr(2, 8).is_some());
    assert!(valid_ro.get_ptr(3, 8).is_none());
    // Mismatched stride passed to `get_ptr` (`4` vs `8`) is rejected!
    assert!(valid_ro.get_ptr(0, 4).is_none());

    // 2. Misaligned pointer (`base + 1` for 8-byte aligned `u64`) is rejected cleanly on typed cast!
    // SAFETY: `base + 1` stays within `aligned_buf` (`24` bytes total, `1 + 16 <= 24`).
    let unaligned_ptr = unsafe { base.add(1) };
    // SAFETY: `unaligned_ptr` has 16 readable bytes inside `aligned_buf`.
    let bad_inner = unsafe { ValidatedSlice::from_raw_parts(unaligned_ptr, 2, 8) };
    let bad_align = ChunkSlice::from_validated(bad_inner);
    assert!(bad_align.as_typed_slice::<u64>().is_none());

    // 3. Total byte length exceeding `isize::MAX` is rejected cleanly without wrapping!
    // SAFETY: `as_typed_slice` / `get_ptr` reject `byte_len > isize::MAX` before dereferencing `base`.
    let huge_inner =
        unsafe { ValidatedSliceMut::from_raw_parts(base, (isize::MAX as usize / 4) + 2, 8) };
    let huge = ChunkSliceMut::from_validated(huge_inner);
    assert!(huge.as_typed_slice::<u64>().is_none());
    assert!(huge.get_ptr((isize::MAX as usize / 8) + 2, 8).is_none());

    // 4. Zero-sized type (`[u8; 0]`) slice of non-zero entity count works cleanly without pointer arithmetic UB:
    let zst_dangling = core::ptr::NonNull::<[u8; 0]>::dangling()
        .as_ptr()
        .cast::<u8>();
    // SAFETY: Zero-sized `[u8; 0]` (`stride = 0`) performs zero memory reads/writes.
    let zst_inner = unsafe { ValidatedSliceMut::from_raw_parts(zst_dangling, 5, 0) };
    let mut zst_mut = ChunkSliceMut::from_validated(zst_inner);
    assert_eq!(zst_mut.len(), 5);
    assert_eq!(zst_mut.as_typed_slice::<[u8; 0]>().unwrap().len(), 5);
    assert_eq!(zst_mut.as_typed_slice_mut::<[u8; 0]>().unwrap().len(), 5);
    assert_eq!(zst_mut.get_mut_ptr(4, 0), Some(zst_dangling));
    assert!(zst_mut.get_mut_ptr(5, 0).is_none());
}

#[test]
fn raw_dynamic_component_with_drop_contains_panics_and_runs_destructor() {
    static DROPS: AtomicUsize = AtomicUsize::new(0);

    #[repr(C)]
    struct ForeignDropPayload {
        should_panic: u32,
    }

    impl Drop for ForeignDropPayload {
        fn drop(&mut self) {
            DROPS.fetch_add(1, Ordering::SeqCst);
            if self.should_panic != 0 {
                panic!("destructor panic caught before extern C boundary");
            }
        }
    }

    let world = MockHostWorld::new();
    let inst = world
        .load_fn(c"foreign_drop_mod", |app| {
            let load_ctx = app.as_raw_load_ctx();
            let uuid = ComponentUuid::from_parts(
                "foreign_drop_mod::ForeignDropPayload",
                core::mem::size_of::<ForeignDropPayload>(),
                core::mem::align_of::<ForeignDropPayload>(),
                1,
            );
            // SAFETY: `load_ctx` is valid for `app` and `ForeignDropPayload` is `#[repr(C)]`.
            let handle = unsafe {
                register_dynamic_component_with_drop::<ForeignDropPayload>(
                    app.instance_mut(),
                    load_ctx,
                    c"foreign_drop_mod::ForeignDropPayload",
                    uuid,
                    0,
                    core::ptr::null::<c_void>(),
                )
            }
            .expect("register dynamic component with drop");
            assert!(handle.is_valid());
            FfiResult::OK
        })
        .expect("load foreign_drop_mod");

    let drop_fn = world.components[0]
        .drop_fn
        .expect("drop_fn registered via raw helper");

    let mut normal = ForeignDropPayload { should_panic: 0 };
    // SAFETY: `normal` is an initialized `ForeignDropPayload`; forgotten immediately after `drop_fn`.
    unsafe { drop_fn((&raw mut normal).cast::<c_void>()) };
    core::mem::forget(normal);
    assert_eq!(DROPS.load(Ordering::SeqCst), 1);

    // Panicking destructor must NOT unwind across `extern "C"`!
    let mut panicking = ForeignDropPayload { should_panic: 1 };
    // SAFETY: `panicking` is an initialized `ForeignDropPayload`; forgotten immediately after `drop_fn`.
    unsafe { drop_fn((&raw mut panicking).cast::<c_void>()) };
    core::mem::forget(panicking);
    assert_eq!(DROPS.load(Ordering::SeqCst), 2);

    world.unload_instance(inst);
}
