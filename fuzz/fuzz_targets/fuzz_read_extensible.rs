#![no_main]
#![deny(unsafe_op_in_unsafe_fn)]

//! Fuzz Target 1: `read_extensible_struct::<T>` & `read_descriptor::<T>` across all six
//! extensible `#[repr(C)]` ABI `0.2.0` wire descriptors (`BamManifest`, `BamHostApi`,
//! `BamQueryDesc`, `BamSystemDesc`, `ComponentDesc`, `VTableHeader`), including misaligned buffers,
//! truncated headers, oversized `struct_size` prefixes, and flag/version validation.

use bevy_abi_mod_sys::{
    ABI_VERSION, BamHostApi, BamManifest, BamQueryDesc, BamSystemDesc, ComponentDesc,
    ComponentUuid, ExtensibleStruct, VTableHeader, read_descriptor, read_extensible_struct,
};
use core::ffi::c_void;
use libfuzzer_sys::fuzz_target;
use std::hint::black_box;

/// Exercises both `read_extensible_struct::<T>` and `read_descriptor::<T>` safely by ensuring the
/// backing allocation is always at least `max(4, min(declared_size, size_of::<T>() + 32))` bytes
/// while honoring arbitrary fuzzer-supplied byte patterns and alignments (`0..8`).
fn fuzz_one_extensible<T: ExtensibleStruct>(
    declared_size: u32,
    misalign: usize,
    payload: &[u8],
) -> Option<T> {
    let cap_size = (declared_size as usize).clamp(4, core::mem::size_of::<T>() + 32);
    let mut backing = vec![0u8; misalign + cap_size];
    let slice = &mut backing[misalign..misalign + cap_size];
    let copy_n = payload.len().min(cap_size);
    slice[..copy_n].copy_from_slice(&payload[..copy_n]);
    slice[..4].copy_from_slice(&declared_size.to_ne_bytes());

    // 1. Direct `read_extensible_struct` with bounded readable length
    let bounded_struct_size = (declared_size as usize).min(cap_size);
    // SAFETY: `slice` is non-null and readable for `cap_size >= bounded_struct_size` bytes;
    // every field of `T: ExtensibleStruct` is valid for arbitrary bit patterns (`Option<fn>`,
    // integer newtypes, raw pointers, `[u8; 16]`).
    let res_ext = unsafe { read_extensible_struct::<T>(slice.as_ptr(), bounded_struct_size) };
    let _ = black_box(res_ext);

    // 2. `read_descriptor` reading `struct_size` from leading 4 bytes when readable bytes suffice
    if (declared_size as usize) <= cap_size || (declared_size as usize) >= core::mem::size_of::<T>()
    {
        // SAFETY: Either `cap_size >= declared_size` or `cap_size >= size_of::<T>()`, so `slice`
        // is readable for `min(declared_size, size_of::<T>())` bytes plus the 4-byte header.
        if let Ok(parsed) = unsafe { read_descriptor::<T>(slice.as_ptr().cast::<c_void>()) } {
            return Some(parsed);
        }
    }
    None
}

fuzz_target!(|data: &[u8]| {
    // Null checks must always return `NULL_POINTER` cleanly.
    // SAFETY: Passing null is explicitly handled by `read_descriptor` / `read_extensible_struct`.
    unsafe {
        let _ = black_box(read_descriptor::<BamManifest>(core::ptr::null()));
        let _ = black_box(read_extensible_struct::<BamHostApi>(core::ptr::null(), 256));
    }

    if data.len() < 6 {
        return;
    }

    let declared_size = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
    let selector = data[4];
    let misalign = (data[5] as usize) & 7;
    let payload = &data[6..];

    match selector % 6 {
        0 => {
            if let Some(m) = fuzz_one_extensible::<BamManifest>(declared_size, misalign, payload) {
                let _ = black_box(m.validate_for_host(ABI_VERSION));
            }
        }
        1 => {
            if let Some(api) = fuzz_one_extensible::<BamHostApi>(declared_size, misalign, payload) {
                let _ = black_box(api.has_all_v0_2_slots());
                let _ = black_box(api.covers_slot(0, 8));
                let _ = black_box(api.covers_slot(declared_size as usize, 8));
            }
        }
        2 => {
            if let Some(q) = fuzz_one_extensible::<BamQueryDesc>(declared_size, misalign, payload) {
                let _ = black_box(q.validate());
            }
        }
        3 => {
            if let Some(s) = fuzz_one_extensible::<BamSystemDesc>(declared_size, misalign, payload)
            {
                let _ = black_box(s.validate());
                let _ = black_box(s.is_main_thread());
            }
        }
        4 => {
            if let Some(c) = fuzz_one_extensible::<ComponentDesc>(declared_size, misalign, payload)
            {
                let _ = black_box(c.validate());
                let _ = black_box(c.layout());
            }
        }
        _ => {
            if let Some(vt) = fuzz_one_extensible::<VTableHeader>(declared_size, misalign, payload)
            {
                let expected_uuid = ComponentUuid::from_canonical_name("fuzz::Comp");
                let _ = black_box(vt.validate(VTableHeader::MIN_V0_2_SIZE, expected_uuid));
            }
        }
    }
});
