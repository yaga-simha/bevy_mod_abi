use guest_sdk::{ChunkSlice, ChunkSliceMut};

// --- Test Types for Alignment Matrix ---
#[derive(Clone, Copy, Debug, PartialEq)]
#[repr(C)]
struct ZstAlign1;

#[derive(Clone, Copy, Debug, PartialEq)]
#[repr(C, align(4))]
struct ZstAlign4;

#[derive(Clone, Copy, Debug, PartialEq)]
#[repr(C, align(16))]
struct ZstAlign16;

#[derive(Clone, Copy, Debug, PartialEq)]
#[repr(C, align(64))]
struct ZstAlign64;

#[derive(Clone, Copy, Debug, PartialEq)]
#[repr(C, align(8))]
struct ValAlign8 {
    a: u32,
    b: u32,
}

#[test]
fn test_zst_slices_adversarial_matrix() {
    // 1. ZST with align 1
    // Valid aligned pointer (0x1 is aligned to 1)
    let slice_zst1 = ChunkSlice::new(core::ptr::dangling::<u8>(), 0, 100);
    assert_eq!(slice_zst1.as_typed_slice::<ZstAlign1>().map(|s| s.len()), Some(100));

    let mut slice_mut_zst1 = ChunkSliceMut::new(core::ptr::dangling_mut::<u8>(), 0, 100);
    assert_eq!(slice_mut_zst1.as_typed_slice::<ZstAlign1>().map(|s| s.len()), Some(100));

    // 2. ZST with alignments 4, 16, 64
    let aligned_64 = 0x1000 as *const u8;
    // 0x1000 is aligned to 4, 16, and 64
    assert_eq!((aligned_64 as usize) % 4, 0);
    assert_eq!((aligned_64 as usize) % 16, 0);
    assert_eq!((aligned_64 as usize) % 64, 0);

    let slice_zst4 = ChunkSlice::new(aligned_64, 0, 50);
    assert_eq!(slice_zst4.as_typed_slice::<ZstAlign4>().map(|s| s.len()), Some(50));

    let slice_zst16 = ChunkSlice::new(aligned_64, 0, 50);
    assert_eq!(slice_zst16.as_typed_slice::<ZstAlign16>().map(|s| s.len()), Some(50));

    let slice_zst64 = ChunkSlice::new(aligned_64, 0, 50);
    assert_eq!(slice_zst64.as_typed_slice::<ZstAlign64>().map(|s| s.len()), Some(50));

    let mut slice_mut_zst64 = ChunkSliceMut::new(aligned_64 as *mut u8, 0, 50);
    assert_eq!(slice_mut_zst64.as_typed_slice::<ZstAlign64>().map(|s| s.len()), Some(50));

    // Unaligned pointer for ZST with align 64 (0x1001 % 64 != 0)
    let unaligned_64 = 0x1001 as *const u8;
    let slice_unaligned = ChunkSlice::new(unaligned_64, 0, 50);
    assert_eq!(slice_unaligned.as_typed_slice::<ZstAlign64>(), None);

    let mut slice_mut_unaligned = ChunkSliceMut::new(unaligned_64 as *mut u8, 0, 50);
    assert_eq!(slice_mut_unaligned.as_typed_slice::<ZstAlign64>(), None);

    // 3. Stride mismatch for ZST (stride != 0 must be rejected)
    for bad_stride in [1, 2, 4, 8, 16, 64, usize::MAX] {
        let slice = ChunkSlice::new(aligned_64, bad_stride, 10);
        assert_eq!(slice.as_typed_slice::<ZstAlign64>(), None);

        let mut slice_mut = ChunkSliceMut::new(aligned_64 as *mut u8, bad_stride, 10);
        assert_eq!(slice_mut.as_typed_slice::<ZstAlign64>(), None);
    }

    // 4. Massive counts for ZST:
    // Count exactly isize::MAX as usize: MUST be accepted according to Rust slice invariants
    let slice_at_isize = ChunkSlice::new(aligned_64, 0, isize::MAX as usize);
    assert_eq!(
        slice_at_isize.as_typed_slice::<ZstAlign64>().map(|s| s.len()),
        Some(isize::MAX as usize)
    );

    let mut slice_mut_at_isize = ChunkSliceMut::new(aligned_64 as *mut u8, 0, isize::MAX as usize);
    assert_eq!(
        slice_mut_at_isize.as_typed_slice::<ZstAlign64>().map(|s| s.len()),
        Some(isize::MAX as usize)
    );

    // Count strictly exceeding isize::MAX: MUST be rejected (returns None)
    let slice_over_isize = ChunkSlice::new(aligned_64, 0, (isize::MAX as usize) + 1);
    assert_eq!(slice_over_isize.as_typed_slice::<ZstAlign64>(), None);

    let mut slice_mut_over_isize = ChunkSliceMut::new(aligned_64 as *mut u8, 0, (isize::MAX as usize) + 1);
    assert_eq!(slice_mut_over_isize.as_typed_slice::<ZstAlign64>(), None);

    // Count = usize::MAX: MUST be rejected (returns None)
    let slice_max = ChunkSlice::new(aligned_64, 0, usize::MAX);
    assert_eq!(slice_max.as_typed_slice::<ZstAlign64>(), None);

    let mut slice_mut_max = ChunkSliceMut::new(aligned_64 as *mut u8, 0, usize::MAX);
    assert_eq!(slice_mut_max.as_typed_slice::<ZstAlign64>(), None);
}

#[test]
fn test_pointer_wrapping_address_space() {
    // Test near the top of the 64-bit address space
    let size = core::mem::size_of::<ValAlign8>();
    assert_eq!(size, 8);

    // 1. Pointer at usize::MAX - 7 (aligned to 8 if (usize::MAX - 7) % 8 == 0)
    // In 64-bit: usize::MAX is 0xFFFF_FFFF_FFFF_FFFF (odd, % 8 == 7).
    // So (usize::MAX - 7) ends in ...FFF8, which is divisible by 8!
    let top_aligned_8 = (usize::MAX - 7) as *const u8;
    assert_eq!((top_aligned_8 as usize) % 8, 0);

    // If count = 1: total_bytes = 8.
    // (usize::MAX - 7) + 8 = usize::MAX + 1 -> overflows usize!
    // Must return None safely due to checked_add wrap detection!
    let slice_wrap1 = ChunkSlice::new(top_aligned_8, size, 1);
    assert_eq!(slice_wrap1.as_typed_slice::<ValAlign8>(), None);

    let mut slice_mut_wrap1 = ChunkSliceMut::new(top_aligned_8 as *mut u8, size, 1);
    assert_eq!(slice_mut_wrap1.as_typed_slice::<ValAlign8>(), None);

    // If count = 2: total_bytes = 16. Also wraps around address space -> None
    let slice_wrap2 = ChunkSlice::new(top_aligned_8, size, 2);
    assert_eq!(slice_wrap2.as_typed_slice::<ValAlign8>(), None);

    let mut slice_mut_wrap2 = ChunkSliceMut::new(top_aligned_8 as *mut u8, size, 2);
    assert_eq!(slice_mut_wrap2.as_typed_slice::<ValAlign8>(), None);

    // 2. Wrap detection in get_ptr and get_ptr_mut
    let slice_ptr_wrap = ChunkSlice::new(top_aligned_8, size, 2);
    // index 0: offset = 0. (usize::MAX - 7) + 0 does not wrap!
    assert_eq!(slice_ptr_wrap.get_ptr(0), Some(top_aligned_8));
    // index 1: offset = 8. (usize::MAX - 7) + 8 wraps -> must return None!
    assert_eq!(slice_ptr_wrap.get_ptr(1), None);

    let mut slice_mut_ptr_wrap = ChunkSliceMut::new(top_aligned_8 as *mut u8, size, 2);
    assert_eq!(slice_mut_ptr_wrap.get_ptr_mut(0), Some(top_aligned_8 as *mut u8));
    assert_eq!(slice_mut_ptr_wrap.get_ptr_mut(1), None);

    // 3. Pointer at usize::MAX for u8 (align 1)
    let top_u8 = usize::MAX as *const u8;
    // count = 1: top_u8 + 1 wraps -> None
    let slice_u8_wrap = ChunkSlice::new(top_u8, 1, 1);
    assert_eq!(slice_u8_wrap.as_typed_slice::<u8>(), None);

    let mut slice_mut_u8_wrap = ChunkSliceMut::new(top_u8 as *mut u8, 1, 1);
    assert_eq!(slice_mut_u8_wrap.as_typed_slice::<u8>(), None);

    // count = 0: empty slice, should succeed regardless of pointer value
    let slice_u8_zero = ChunkSlice::new(top_u8, 1, 0);
    assert_eq!(slice_u8_zero.as_typed_slice::<u8>(), Some(&[][..]));

    let mut slice_mut_u8_zero = ChunkSliceMut::new(top_u8 as *mut u8, 1, 0);
    assert_eq!(slice_mut_u8_zero.as_typed_slice::<u8>(), Some(&mut [][..]));
}

#[test]
fn test_stride_mismatches_exhaustive() {
    let dummy_val = 0x42u32;
    let valid_ptr = &dummy_val as *const u32 as *const u8;
    let size = core::mem::size_of::<u32>();

    // Test a broad spectrum of invalid strides
    let invalid_strides = [
        0,
        1,
        2,
        3,
        5,
        6,
        7,
        8,
        12,
        16,
        32,
        64,
        1024,
        isize::MAX as usize,
        usize::MAX - 1,
        usize::MAX,
    ];

    // Case 1: count == 0. Stride validation MUST happen even when count == 0!
    for &stride in &invalid_strides {
        let slice = ChunkSlice::new(valid_ptr, stride, 0);
        assert_eq!(
            slice.as_typed_slice::<u32>(),
            None,
            "count=0 with stride={stride} must return None"
        );

        let mut slice_mut = ChunkSliceMut::new(valid_ptr as *mut u8, stride, 0);
        assert_eq!(
            slice_mut.as_typed_slice::<u32>(),
            None,
            "mut count=0 with stride={stride} must return None"
        );
    }

    // Case 2: count == 1
    for &stride in &invalid_strides {
        let slice = ChunkSlice::new(valid_ptr, stride, 1);
        assert_eq!(
            slice.as_typed_slice::<u32>(),
            None,
            "count=1 with stride={stride} must return None"
        );

        let mut slice_mut = ChunkSliceMut::new(valid_ptr as *mut u8, stride, 1);
        assert_eq!(
            slice_mut.as_typed_slice::<u32>(),
            None,
            "mut count=1 with stride={stride} must return None"
        );
    }

    // Case 3: Exact stride match must succeed
    let valid_slice = ChunkSlice::new(valid_ptr, size, 1);
    assert_eq!(valid_slice.as_typed_slice::<u32>(), Some(&[0x42u32][..]));

    let mut mut_val = 0x42u32;
    let mut valid_slice_mut = ChunkSliceMut::new(&mut mut_val as *mut u32 as *mut u8, size, 1);
    assert_eq!(valid_slice_mut.as_typed_slice::<u32>(), Some(&mut [0x42u32][..]));
}

#[test]
fn test_null_pointer_handling_exhaustive() {
    let size = core::mem::size_of::<u64>();

    // 1. Read-only slice with null pointer
    let slice_null_zero = ChunkSlice::new(core::ptr::null(), size, 0);
    assert_eq!(slice_null_zero.as_typed_slice::<u64>(), Some(&[][..]));
    assert!(slice_null_zero.is_empty());
    assert_eq!(slice_null_zero.get_ptr(0), None);

    let slice_null_nonzero = ChunkSlice::new(core::ptr::null(), size, 1);
    assert_eq!(slice_null_nonzero.as_typed_slice::<u64>(), None);
    assert!(slice_null_nonzero.is_empty());
    assert_eq!(slice_null_nonzero.get_ptr(0), None);

    let slice_null_massive = ChunkSlice::new(core::ptr::null(), size, usize::MAX);
    assert_eq!(slice_null_massive.as_typed_slice::<u64>(), None);
    assert!(slice_null_massive.is_empty());
    assert_eq!(slice_null_massive.get_ptr(0), None);

    // 2. Mutable slice with null pointer
    let mut slice_mut_null_zero = ChunkSliceMut::new(core::ptr::null_mut(), size, 0);
    assert_eq!(slice_mut_null_zero.as_typed_slice::<u64>(), Some(&mut [][..]));
    assert!(slice_mut_null_zero.is_empty());
    assert_eq!(slice_mut_null_zero.get_ptr_mut(0), None);

    let mut slice_mut_null_nonzero = ChunkSliceMut::new(core::ptr::null_mut(), size, 1);
    assert_eq!(slice_mut_null_nonzero.as_typed_slice::<u64>(), None);
    assert!(slice_mut_null_nonzero.is_empty());
    assert_eq!(slice_mut_null_nonzero.get_ptr_mut(0), None);

    let mut slice_mut_null_massive = ChunkSliceMut::new(core::ptr::null_mut(), size, usize::MAX);
    assert_eq!(slice_mut_null_massive.as_typed_slice::<u64>(), None);
    assert!(slice_mut_null_massive.is_empty());
    assert_eq!(slice_mut_null_massive.get_ptr_mut(0), None);
}

#[test]
fn test_get_ptr_and_mut_boundary_conditions() {
    let mut array = [100u64, 200u64, 300u64, 400u64];
    let stride = core::mem::size_of::<u64>();
    let count = array.len();

    // 1. Read-only retrieval
    let slice = ChunkSlice::new(array.as_ptr() as *const u8, stride, count);
    for (i, &expected) in array.iter().enumerate() {
        let ptr = slice.get_ptr(i).expect("valid index within count");
        assert_eq!(unsafe { *(ptr as *const u64) }, expected);
    }

    // Out-of-bounds indices
    assert_eq!(slice.get_ptr(count), None);
    assert_eq!(slice.get_ptr(count + 1), None);
    assert_eq!(slice.get_ptr(usize::MAX), None);
    assert_eq!(slice.get_ptr(usize::MAX / 2), None);

    // 2. Mutable retrieval and in-place modification
    let mut slice_mut = ChunkSliceMut::new(array.as_mut_ptr() as *mut u8, stride, count);
    for i in 0..count {
        let ptr = slice_mut.get_ptr_mut(i).expect("valid index within count");
        unsafe {
            *(ptr as *mut u64) += 5;
        }
    }
    assert_eq!(slice_mut.get_ptr_mut(count), None);
    assert_eq!(slice_mut.get_ptr_mut(count + 1), None);
    assert_eq!(slice_mut.get_ptr_mut(usize::MAX), None);

    assert_eq!(array, [105u64, 205u64, 305u64, 405u64]);
}
