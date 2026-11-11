//! Host API function table, symbol lookup, and guest lifecycle entry-point typedefs.

use core::ffi::{c_char, c_void};

use crate::{
    component::RegisterComponentFn,
    extensible::ExtensibleStruct,
    iter::{
        IterColumnMutableFn, IterColumnReadonlyFn, IterColumnStrideFn, IterEntitiesFn,
        IterMarkChangedFn, IterNextChunkFn,
    },
    primitives::FfiResult,
    system::RegisterSystemFn,
    version::abi_version_is_compatible,
};

/// Symbol-resolver function pointer passed to the guest mod entry point (Godot GDExtension style).
///
/// # Safety
/// `name` must be a valid nul-terminated C string or null.
pub type GetProcAddressFn = unsafe extern "C" fn(name: *const c_char) -> *const c_void;

/// Host API VTable passed to the guest mod during `bevy_abi_mod_init`.
///
/// Must have `'static` lifetime on the host so guest mods may store the pointer across frames.
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct HostApi {
    /// Size of this struct in bytes (`core::mem::size_of::<HostApi>() as u32`).
    pub struct_size: u32,
    /// Packed host ABI version (`ABI_VERSION_PACKED`).
    pub abi_version: u32,
    /// Register a dynamic ECS system with the host schedule.
    pub register_system: RegisterSystemFn,
    /// Advance a `RawChunkIter` to the next table chunk.
    pub iter_next_chunk: IterNextChunkFn,
    /// Borrow a read-only column byte pointer from a `RawChunkIter`.
    pub iter_column_readonly: IterColumnReadonlyFn,
    /// Borrow a mutable column byte pointer from a `RawChunkIter` (marks chunk ticks changed).
    pub iter_column_mutable: IterColumnMutableFn,
    /// Mark an individual row's component tick changed.
    pub iter_mark_changed: IterMarkChangedFn,
    /// Borrow the contiguous `FfiEntity` column from a `RawChunkIter`.
    pub iter_entities: IterEntitiesFn,
    /// Query the byte stride of a column in a `RawChunkIter`.
    pub iter_column_stride: IterColumnStrideFn,
    /// Register a runtime dynamic component with the host Bevy ECS World.
    pub register_component: RegisterComponentFn,
}

impl HostApi {
    /// Size of the legacy 7-function `HostApi` prior to `register_component` (`64` on 64-bit, `36` on 32-bit).
    pub const LEGACY_64_SIZE: usize = core::mem::offset_of!(Self, register_component);

    /// Frozen minimum valid full v0.1 `HostApi` size (`72` on 64-bit, `40` on 32-bit),
    /// computed from the end of `register_component` rather than `size_of::<Self>()` so
    /// appending optional trailing slots never rejects older callers.
    pub const MIN_V1_SIZE: usize = core::mem::offset_of!(Self, register_component)
        + core::mem::size_of::<RegisterComponentFn>();

    /// Safely reads the `struct_size` header from a potentially smaller/older `HostApi` pointer
    /// without forming a full `&HostApi` reference (preventing out-of-bounds provenance UB).
    ///
    /// # Safety
    /// If `api_ptr` is non-null, it must be aligned and readable for at least `size_of::<u32>()` bytes.
    #[inline]
    pub unsafe fn raw_struct_size(api_ptr: *const Self) -> Option<u32> {
        if api_ptr.is_null() {
            return None;
        }
        // SAFETY: Caller guarantees at least 4 readable, aligned bytes at offset 0 (`struct_size`).
        Some(unsafe { core::ptr::read(core::ptr::addr_of!((*api_ptr).struct_size)) })
    }

    /// Safely reads the `abi_version` header from a potentially smaller/older `HostApi` pointer
    /// without forming a full `&HostApi` reference.
    ///
    /// # Safety
    /// If `api_ptr` is non-null, it must be aligned and readable for at least 8 bytes.
    #[inline]
    pub unsafe fn raw_abi_version(api_ptr: *const Self) -> Option<u32> {
        // SAFETY: Forwarding `api_ptr` under caller's alignment and header-readability contract.
        let sz = unsafe { Self::raw_struct_size(api_ptr) }?;
        if (sz as usize) < core::mem::offset_of!(Self, register_system) {
            return None;
        }
        // SAFETY: Verified `struct_size >= 8`, so `abi_version` at offset 4 is initialized and readable.
        Some(unsafe { core::ptr::read(core::ptr::addr_of!((*api_ptr).abi_version)) })
    }

    /// Returns `true` if `api_ptr` is non-null and its `struct_size` covers `register_component`.
    ///
    /// # Safety
    /// If `api_ptr` is non-null, it must be aligned and readable for at least `size_of::<u32>()` bytes.
    #[inline]
    pub unsafe fn has_register_component_raw(api_ptr: *const Self) -> bool {
        // SAFETY: Forwarding `api_ptr` under caller's 4-byte header-readability contract.
        match unsafe { Self::raw_struct_size(api_ptr) } {
            Some(sz) => (sz as usize) >= Self::MIN_V1_SIZE,
            None => false,
        }
    }

    /// Validates `struct_size` and SemVer compatibility against a guest's `guest_packed_version`.
    #[inline]
    pub fn validate_for_guest(&self, guest_packed_version: u32) -> FfiResult {
        if (self.struct_size as usize) < Self::MIN_V1_SIZE {
            return FfiResult::INVALID_VERSION;
        }
        if !abi_version_is_compatible(self.abi_version, guest_packed_version) {
            return FfiResult::INVALID_VERSION;
        }
        FfiResult::OK
    }
}

const _: () = {
    assert!(HostApi::MIN_V1_SIZE <= core::mem::size_of::<HostApi>());
    #[cfg(target_pointer_width = "64")]
    assert!(HostApi::MIN_V1_SIZE == 72);
    #[cfg(target_pointer_width = "32")]
    assert!(HostApi::MIN_V1_SIZE == 40);
};

// SAFETY: `HostApi` is `#[repr(C)]` with `struct_size: u32` at offset 0 and `validate_raw_slot`
// verifies all 8 non-nullable function pointers before `assume_init()`.
unsafe impl ExtensibleStruct for HostApi {
    const MIN_V1_SIZE: usize = Self::MIN_V1_SIZE;

    #[inline]
    unsafe fn validate_raw_slot(slot_ptr: *const Self) -> FfiResult {
        let fn_offsets = [
            core::mem::offset_of!(Self, register_system),
            core::mem::offset_of!(Self, iter_next_chunk),
            core::mem::offset_of!(Self, iter_column_readonly),
            core::mem::offset_of!(Self, iter_column_mutable),
            core::mem::offset_of!(Self, iter_mark_changed),
            core::mem::offset_of!(Self, iter_entities),
            core::mem::offset_of!(Self, iter_column_stride),
            core::mem::offset_of!(Self, register_component),
        ];
        let mut i = 0;
        while i < fn_offsets.len() {
            // SAFETY: Every offset in `fn_offsets` lies strictly within `0..size_of::<Self>()`.
            let addr = unsafe {
                core::ptr::read_unaligned(slot_ptr.cast::<u8>().add(fn_offsets[i]).cast::<usize>())
            };
            if addr == 0 {
                return FfiResult::NULL_POINTER;
            }
            i += 1;
        }
        FfiResult::OK
    }
}

/// Mod initialization entry point function signature (`bevy_abi_mod_init`).
///
/// # Safety
/// Invoked dynamically by host during `load_mod`. Pointers must remain valid for the duration of the handshake.
pub type ModInitFn = unsafe extern "C" fn(
    get_proc_address: GetProcAddressFn,
    host_api: *const HostApi,
    host_context: *mut c_void,
) -> FfiResult;

/// Mod teardown function signature (`bevy_abi_mod_shutdown`).
///
/// # Safety
/// Invoked dynamically by host during `unload_mod` while library memory is still mapped.
pub type ModShutdownFn = unsafe extern "C" fn(host_context: *mut c_void) -> FfiResult;

/// Optional ABI version query entry point (`bevy_abi_mod_abi_version`).
///
/// # Safety
/// Must be side-effect free and return packed ABI version.
pub type ModAbiVersionFn = unsafe extern "C" fn() -> u32;
