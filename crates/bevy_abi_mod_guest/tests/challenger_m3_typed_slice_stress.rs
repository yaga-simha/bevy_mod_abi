#![allow(
    clippy::undocumented_unsafe_blocks,
    clippy::ptr_as_ptr,
    clippy::cast_ptr_alignment,
    clippy::transmute_ptr_to_ptr,
    unreachable_pub
)]
use bevy_abi_mod_guest::{
    AbiPod, ChunkSlice, ChunkSliceMut, ChunkSliceMutRawExt, ChunkSliceRawExt, FfiLayout,
};

macro_rules! impl_test_pod {
    ($($ty:ty),+ $(,)?) => {
        $(
            unsafe impl FfiLayout for $ty {}
            unsafe impl AbiPod for $ty {}
        )+
    };
}

#[derive(Clone, Copy, Debug, PartialEq)]
#[repr(C)]
struct Align1(u8);

#[derive(Clone, Copy, Debug, PartialEq)]
#[repr(C, align(2))]
struct Align2(u16);

#[derive(Clone, Copy, Debug, PartialEq)]
#[repr(C, align(4))]
struct Align4(u32);

#[derive(Clone, Copy, Debug, PartialEq)]
#[repr(C, align(8))]
struct Align8(u64);

#[derive(Clone, Copy, Debug, PartialEq)]
#[repr(C, align(16))]
struct Align16([u8; 16]);

impl_test_pod!(Align1, Align2, Align4, Align8, Align16);

#[test]
fn test_as_typed_slice_alignment_matrix() {
    let mut buffer = [0u8; 64];
    let base_addr = buffer.as_mut_ptr() as usize;

    let offset_to_align16 = (16 - (base_addr % 16)) % 16;
    let aligned_ptr = unsafe { buffer.as_mut_ptr().add(offset_to_align16) };
    assert_eq!((aligned_ptr as usize) % 16, 0);

    for offset in 0..16 {
        let test_ptr = unsafe { aligned_ptr.add(offset) };
        let addr = test_ptr as usize;

        let slice1 = unsafe { ChunkSlice::new(test_ptr, 1, 1) };
        assert!(slice1.as_typed_slice::<Align1>().is_some());

        let slice2 = unsafe { ChunkSlice::new(test_ptr, 2, 1) };
        if addr.is_multiple_of(2) {
            assert!(slice2.as_typed_slice::<Align2>().is_some());
        } else {
            assert!(slice2.as_typed_slice::<Align2>().is_none());
        }

        let slice4 = unsafe { ChunkSlice::new(test_ptr, 4, 1) };
        if addr.is_multiple_of(4) {
            assert!(slice4.as_typed_slice::<Align4>().is_some());
        } else {
            assert!(slice4.as_typed_slice::<Align4>().is_none());
        }

        let slice8 = unsafe { ChunkSlice::new(test_ptr, 8, 1) };
        if addr.is_multiple_of(8) {
            assert!(slice8.as_typed_slice::<Align8>().is_some());
        } else {
            assert!(slice8.as_typed_slice::<Align8>().is_none());
        }

        let slice16 = unsafe { ChunkSlice::new(test_ptr, 16, 1) };
        if addr.is_multiple_of(16) {
            assert!(slice16.as_typed_slice::<Align16>().is_some());
        } else {
            assert!(slice16.as_typed_slice::<Align16>().is_none());
        }
    }
}

#[test]
fn test_as_typed_slice_mut_alignment_matrix() {
    let mut buffer = [0u8; 64];
    let base_addr = buffer.as_mut_ptr() as usize;
    let offset_to_align16 = (16 - (base_addr % 16)) % 16;
    let aligned_ptr = unsafe { buffer.as_mut_ptr().add(offset_to_align16) };

    for offset in 0..16 {
        let test_ptr = unsafe { aligned_ptr.add(offset) };
        let addr = test_ptr as usize;

        let mut slice4 = unsafe { ChunkSliceMut::new(test_ptr, 4, 1) };
        if addr.is_multiple_of(4) {
            let res = slice4.as_typed_slice::<Align4>();
            assert!(res.is_some());
        } else {
            assert!(slice4.as_typed_slice::<Align4>().is_none());
        }
    }
}

#[test]
fn test_as_typed_slice_stride_mismatches() {
    let val = 42u32;
    let ptr = &val as *const u32 as *const u8;
    let size = core::mem::size_of::<u32>();

    let strides = [
        size.saturating_sub(2),
        size - 1,
        size + 1,
        size + 2,
        0,
        usize::MAX,
    ];
    for &stride in &strides {
        let slice = unsafe { ChunkSlice::new(ptr, stride, 1) };
        assert!(
            slice.as_typed_slice::<u32>().is_none(),
            "Stride {stride} should be rejected"
        );

        let mut slice_mut = unsafe { ChunkSliceMut::new(ptr as *mut u8, stride, 1) };
        assert!(
            slice_mut.as_typed_slice::<u32>().is_none(),
            "Stride {stride} should be rejected"
        );
    }
}

#[test]
fn test_as_typed_slice_zero_count_edge_cases() {
    let size = core::mem::size_of::<u32>();

    let slice_null = unsafe { ChunkSlice::new(core::ptr::null(), size, 0) };
    let typed_null = slice_null.as_typed_slice::<u32>();
    assert_eq!(typed_null, Some(&[][..]));

    let mut slice_mut_null = unsafe { ChunkSliceMut::new(core::ptr::null_mut(), size, 0) };
    let typed_mut_null = slice_mut_null.as_typed_slice::<u32>();
    assert_eq!(typed_mut_null, Some(&mut [][..]));

    let slice_unaligned = unsafe { ChunkSlice::new(core::ptr::dangling::<u8>(), size, 0) };
    let typed_unaligned = slice_unaligned.as_typed_slice::<u32>();
    assert_eq!(typed_unaligned, Some(&[][..]));

    let mut slice_mut_unaligned =
        unsafe { ChunkSliceMut::new(core::ptr::dangling_mut::<u8>(), size, 0) };
    let typed_mut_unaligned = slice_mut_unaligned.as_typed_slice::<u32>();
    assert_eq!(typed_mut_unaligned, Some(&mut [][..]));

    let slice_bad_stride = unsafe { ChunkSlice::new(core::ptr::null(), size + 1, 0) };
    assert!(slice_bad_stride.as_typed_slice::<u32>().is_none());

    let mut slice_mut_bad_stride =
        unsafe { ChunkSliceMut::new(core::ptr::null_mut(), size + 1, 0) };
    assert!(slice_mut_bad_stride.as_typed_slice::<u32>().is_none());
}

#[test]
fn test_as_typed_slice_null_with_nonzero_count() {
    let size = core::mem::size_of::<u32>();

    let slice = unsafe { ChunkSlice::new(core::ptr::null(), size, 1) };
    assert!(slice.as_typed_slice::<u32>().is_none());

    let mut slice_mut = unsafe { ChunkSliceMut::new(core::ptr::null_mut(), size, 1) };
    assert!(slice_mut.as_typed_slice::<u32>().is_none());
}

#[test]
fn test_as_typed_slice_zst_safety() {
    type Zst = [u8; 0];
    let zst_ptr = core::ptr::without_provenance::<u8>(0x1000);
    let zst_ptr_mut = core::ptr::without_provenance_mut::<u8>(0x1000);
    let slice_zst = unsafe { ChunkSlice::new(zst_ptr, 0, 100) };
    let typed = slice_zst.as_typed_slice::<Zst>();
    assert!(typed.is_some());
    assert_eq!(typed.unwrap().len(), 100);

    let bad_zst = unsafe { ChunkSlice::new(zst_ptr, 1, 100) };
    assert!(bad_zst.as_typed_slice::<Zst>().is_none());

    let null_zst = unsafe { ChunkSlice::new(core::ptr::null(), 0, 100) };
    assert!(null_zst.as_typed_slice::<Zst>().is_none());

    let empty_null_zst = unsafe { ChunkSlice::new(core::ptr::null(), 0, 0) };
    assert_eq!(empty_null_zst.as_typed_slice::<Zst>(), Some(&[][..]));

    let massive_zst = unsafe { ChunkSlice::new(zst_ptr, 0, usize::MAX) };
    assert_eq!(massive_zst.as_typed_slice::<Zst>(), None);

    let mut massive_mut_zst = unsafe { ChunkSliceMut::new(zst_ptr_mut, 0, usize::MAX) };
    assert_eq!(massive_mut_zst.as_typed_slice::<Zst>(), None);

    let over_isize_zst = unsafe { ChunkSlice::new(zst_ptr, 0, (isize::MAX as usize) + 1) };
    assert_eq!(over_isize_zst.as_typed_slice::<Zst>(), None);

    let mut over_isize_mut_zst =
        unsafe { ChunkSliceMut::new(zst_ptr_mut, 0, (isize::MAX as usize) + 1) };
    assert_eq!(over_isize_mut_zst.as_typed_slice::<Zst>(), None);
}

#[test]
fn test_as_typed_slice_massive_count() {
    let mut val = 1234u32;
    let ptr = &mut val as *mut u32 as *mut u8;
    let size = core::mem::size_of::<u32>();

    let slice = unsafe { ChunkSlice::new(ptr, size, usize::MAX) };
    assert_eq!(slice.as_typed_slice::<u32>(), None);

    let mut slice_mut = unsafe { ChunkSliceMut::new(ptr, size, usize::MAX) };
    assert_eq!(slice_mut.as_typed_slice::<u32>(), None);

    let over_isize_count = (isize::MAX as usize / size) + 1;
    let slice_over = unsafe { ChunkSlice::new(ptr, size, over_isize_count) };
    assert_eq!(slice_over.as_typed_slice::<u32>(), None);

    let mut slice_over_mut = unsafe { ChunkSliceMut::new(ptr, size, over_isize_count) };
    assert_eq!(slice_over_mut.as_typed_slice::<u32>(), None);

    let high_ptr = core::ptr::without_provenance_mut::<u8>(usize::MAX - 2);
    let slice_wrap = unsafe { ChunkSlice::new(high_ptr, 1, 4) };
    assert_eq!(slice_wrap.as_typed_slice::<u8>(), None);

    let mut slice_wrap_mut = unsafe { ChunkSliceMut::new(high_ptr, 1, 4) };
    assert_eq!(slice_wrap_mut.as_typed_slice::<u8>(), None);

    let valid_count = 10;
    let mut buffer = [0u32; 10];
    let slice_valid = unsafe { ChunkSlice::new(buffer.as_ptr() as *const u8, size, valid_count) };
    assert!(slice_valid.as_typed_slice::<u32>().is_some());
    assert_eq!(slice_valid.as_typed_slice::<u32>().unwrap().len(), 10);

    let mut slice_valid_mut =
        unsafe { ChunkSliceMut::new(buffer.as_mut_ptr() as *mut u8, size, valid_count) };
    assert!(slice_valid_mut.as_typed_slice::<u32>().is_some());
    assert_eq!(slice_valid_mut.as_typed_slice::<u32>().unwrap().len(), 10);
}

#[test]
fn test_as_typed_slice_massive_count_soundness_audit() {
    test_as_typed_slice_massive_count();
}
