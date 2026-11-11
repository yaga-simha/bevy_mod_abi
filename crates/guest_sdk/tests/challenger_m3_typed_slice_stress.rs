use guest_sdk::{ChunkSlice, ChunkSliceMut};

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

#[test]
fn test_as_typed_slice_alignment_matrix() {
    let mut buffer = [0u8; 64];
    let base_addr = buffer.as_mut_ptr() as usize;

    // Find offset within buffer that gives 16-byte alignment
    let offset_to_align16 = (16 - (base_addr % 16)) % 16;
    let aligned_ptr = unsafe { buffer.as_mut_ptr().add(offset_to_align16) };
    assert_eq!((aligned_ptr as usize) % 16, 0);

    // Test alignments 1, 2, 4, 8, 16 across offsets 0..16
    for offset in 0..16 {
        let test_ptr = unsafe { aligned_ptr.add(offset) };
        let addr = test_ptr as usize;

        // Align1
        let slice1 = ChunkSlice::new(test_ptr, 1, 1);
        assert!(slice1.as_typed_slice::<Align1>().is_some());

        // Align2
        let slice2 = ChunkSlice::new(test_ptr, 2, 1);
        if addr % 2 == 0 {
            assert!(slice2.as_typed_slice::<Align2>().is_some());
        } else {
            assert!(slice2.as_typed_slice::<Align2>().is_none());
        }

        // Align4
        let slice4 = ChunkSlice::new(test_ptr, 4, 1);
        if addr % 4 == 0 {
            assert!(slice4.as_typed_slice::<Align4>().is_some());
        } else {
            assert!(slice4.as_typed_slice::<Align4>().is_none());
        }

        // Align8
        let slice8 = ChunkSlice::new(test_ptr, 8, 1);
        if addr % 8 == 0 {
            assert!(slice8.as_typed_slice::<Align8>().is_some());
        } else {
            assert!(slice8.as_typed_slice::<Align8>().is_none());
        }

        // Align16
        let slice16 = ChunkSlice::new(test_ptr, 16, 1);
        if addr % 16 == 0 {
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

        let mut slice4 = ChunkSliceMut::new(test_ptr, 4, 1);
        if addr % 4 == 0 {
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

    // Test stride offsets: -2, -1, +1, +2, 0, usize::MAX
    let strides = [
        size.saturating_sub(2),
        size - 1,
        size + 1,
        size + 2,
        0,
        usize::MAX,
    ];
    for &stride in &strides {
        let slice = ChunkSlice::new(ptr, stride, 1);
        assert!(
            slice.as_typed_slice::<u32>().is_none(),
            "Stride {stride} should be rejected"
        );

        let mut slice_mut = ChunkSliceMut::new(ptr as *mut u8, stride, 1);
        assert!(
            slice_mut.as_typed_slice::<u32>().is_none(),
            "Stride {stride} should be rejected"
        );
    }
}

#[test]
fn test_as_typed_slice_zero_count_edge_cases() {
    let size = core::mem::size_of::<u32>();

    // 1. count == 0, ptr == null, valid stride
    let slice_null = ChunkSlice::new(core::ptr::null(), size, 0);
    let typed_null = slice_null.as_typed_slice::<u32>();
    assert_eq!(typed_null, Some(&[][..]));

    let mut slice_mut_null = ChunkSliceMut::new(core::ptr::null_mut(), size, 0);
    let typed_mut_null = slice_mut_null.as_typed_slice::<u32>();
    assert_eq!(typed_mut_null, Some(&mut [][..]));

    // 2. count == 0, ptr == unaligned dangling (0x1), valid stride
    let slice_unaligned = ChunkSlice::new(0x1 as *const u8, size, 0);
    let typed_unaligned = slice_unaligned.as_typed_slice::<u32>();
    assert_eq!(typed_unaligned, Some(&[][..]));

    let mut slice_mut_unaligned = ChunkSliceMut::new(0x1 as *mut u8, size, 0);
    let typed_mut_unaligned = slice_mut_unaligned.as_typed_slice::<u32>();
    assert_eq!(typed_mut_unaligned, Some(&mut [][..]));

    // 3. count == 0 with INVALID stride (stride check must precede count check)
    let slice_bad_stride = ChunkSlice::new(core::ptr::null(), size + 1, 0);
    assert!(slice_bad_stride.as_typed_slice::<u32>().is_none());

    let mut slice_mut_bad_stride = ChunkSliceMut::new(core::ptr::null_mut(), size + 1, 0);
    assert!(slice_mut_bad_stride.as_typed_slice::<u32>().is_none());
}

#[test]
fn test_as_typed_slice_null_with_nonzero_count() {
    let size = core::mem::size_of::<u32>();

    // count > 0 with null pointer must return None
    let slice = ChunkSlice::new(core::ptr::null(), size, 1);
    assert!(slice.as_typed_slice::<u32>().is_none());

    let mut slice_mut = ChunkSliceMut::new(core::ptr::null_mut(), size, 1);
    assert!(slice_mut.as_typed_slice::<u32>().is_none());
}

#[test]
fn test_as_typed_slice_massive_count() {
    let val = 1234u32;
    let ptr = &val as *const u32 as *const u8;
    let size = core::mem::size_of::<u32>();

    // Test with count = usize::MAX
    let slice = ChunkSlice::new(ptr, size, usize::MAX);
    let typed = slice.as_typed_slice::<u32>();
    // In Rust, does this return Some with len usize::MAX?
    println!("typed with usize::MAX: {:?}", typed.map(|s| s.len()));
}
