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
fn test_as_typed_slice_zst_safety() {
    // Zero-Sized Types: size == 0, align == 1
    let zst_ptr = 0x1000 as *const u8;
    let slice_zst = ChunkSlice::new(zst_ptr, 0, 100);
    let typed = slice_zst.as_typed_slice::<()>();
    assert!(typed.is_some());
    assert_eq!(typed.unwrap().len(), 100);

    // Stride mismatch for ZST (stride != 0)
    let bad_zst = ChunkSlice::new(zst_ptr, 1, 100);
    assert!(bad_zst.as_typed_slice::<()>().is_none());

    // Null pointer for ZST with count > 0
    let null_zst = ChunkSlice::new(core::ptr::null(), 0, 100);
    assert!(null_zst.as_typed_slice::<()>().is_none());

    // Count == 0 for ZST with null pointer
    let empty_null_zst = ChunkSlice::new(core::ptr::null(), 0, 0);
    assert_eq!(empty_null_zst.as_typed_slice::<()>(), Some(&[][..]));
}

#[test]
fn test_as_typed_slice_massive_count_soundness_audit() {
    // When an external environment or corrupted chunk iterator passes a massive count
    // (e.g. usize::MAX), a safe function ChunkSlice::as_typed_slice<T> must not violate
    // core::slice::from_raw_parts safety contract (total size <= isize::MAX).
    //
    // In the current implementation, calling as_typed_slice with count = usize::MAX invokes
    // core::slice::from_raw_parts, which triggers Rust's stdlib runtime assertion:
    // "unsafe precondition(s) violated: slice::from_raw_parts requires the pointer to be aligned
    // and non-null, and the total size of the slice not to exceed `isize::MAX`"
    // and aborts the process via SIGABRT (signal 6).
    //
    // This test spawns a child process executing the massive slice call to empirically verify
    // whether the current implementation aborts (demonstrating the vulnerability).
    if std::env::var("RUN_MASSIVE_SLICE_SUBPROCESS").is_ok() {
        let mut val = 1234u32;
        let ptr = &mut val as *mut u32 as *mut u8;
        let size = core::mem::size_of::<u32>();

        if std::env::var("RUN_MUTABLE_SLICE").is_ok() {
            let mut slice_mut = ChunkSliceMut::new(ptr, size, usize::MAX);
            let _ = slice_mut.as_typed_slice::<u32>();
        } else {
            let slice = ChunkSlice::new(ptr, size, usize::MAX);
            let _ = slice.as_typed_slice::<u32>();
        }
        return;
    }

    let current_exe = std::env::current_exe().expect("Failed to get current test binary path");

    // 1. Audit ChunkSlice (read-only)
    let output_ro = std::process::Command::new(&current_exe)
        .arg("test_as_typed_slice_massive_count_soundness_audit")
        .env("RUN_MASSIVE_SLICE_SUBPROCESS", "1")
        .output()
        .expect("Failed to execute subprocess");

    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        let signal = output_ro.status.signal();
        let stderr = String::from_utf8_lossy(&output_ro.stderr);
        eprintln!(
            "[ReadOnly] Subprocess status: {:?}, signal: {:?}, stderr: {}",
            output_ro.status, signal, stderr
        );
        assert!(
            signal == Some(6) || stderr.contains("precondition(s) violated") || !output_ro.status.success(),
            "Empirically confirmed: ChunkSlice::as_typed_slice violates from_raw_parts safety precondition on massive counts"
        );
    }

    // 2. Audit ChunkSliceMut (mutable)
    let output_mut = std::process::Command::new(&current_exe)
        .arg("test_as_typed_slice_massive_count_soundness_audit")
        .env("RUN_MASSIVE_SLICE_SUBPROCESS", "1")
        .env("RUN_MUTABLE_SLICE", "1")
        .output()
        .expect("Failed to execute subprocess");

    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        let signal = output_mut.status.signal();
        let stderr = String::from_utf8_lossy(&output_mut.stderr);
        eprintln!(
            "[Mutable] Subprocess status: {:?}, signal: {:?}, stderr: {}",
            output_mut.status, signal, stderr
        );
        assert!(
            signal == Some(6) || stderr.contains("precondition(s) violated") || !output_mut.status.success(),
            "Empirically confirmed: ChunkSliceMut::as_typed_slice violates from_raw_parts_mut safety precondition on massive counts"
        );
    }
}
