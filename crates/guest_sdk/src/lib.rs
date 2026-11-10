#![deny(unsafe_op_in_unsafe_fn)]
#![deny(improper_ctypes_definitions)]

//! # `guest_sdk`
//!
//! Ergonomic, Bevy-independent SDK for guest mod authors.
//! Wraps raw `abi_contract` C-ABI structs and function pointers into safe abstractions,
//! and catches unwinding panics inside the guest's own `std` runtime before crossing
//! the `extern "C"` boundary.

pub use abi_contract::{
    ComponentAccessDesc, ComponentAccessMode, ComponentUuid, DynamicSystemCallback,
    ExposableComponent, FfiEntity, FfiLayout, FfiResult, FfiSlice, GetProcAddressFn, HostApi,
    IterColumnMutableFn, IterColumnReadonlyFn, IterColumnStrideFn, IterEntitiesFn,
    IterMarkChangedFn, IterNextChunkFn, IterResetFn, ModAbiVersionFn, ModInitFn, ModShutdownFn,
    MultiQuerySystemCallback, MultiQuerySystemDescriptor, QuerySpec, RawChunkIter, RawCommandQueue,
    RegisterMultiQuerySystemFn, RegisterSystemFn, StrView, SymbolEntry, SystemDescriptor,
    ABI_VERSION_MAJOR, ABI_VERSION_MINOR, ABI_VERSION_PACKED, ABI_VERSION_PATCH,
};

/// Execute a closure inside `std::panic::catch_unwind` within the guest's own Rust runtime,
/// converting any unwinding panic into `FfiResult::PANIC_CAUGHT` before crossing the `extern "C"`
/// boundary.
///
/// Why this is done on the guest side:
/// Each `.cdylib` links its own isolated `std` runtime. Unwinding a guest panic into the host's
/// `std::panic::catch_unwind` is classified by the Rust Reference as catching a foreign exception
/// (which has unspecified behavior and may abort the process). Catching the unwind inside the
/// guest's own `std` runtime and returning `FfiResult::PANIC_CAUGHT` by value is 100% well-defined.
#[inline]
pub fn catch_guest_panic<F>(f: F) -> FfiResult
where
    F: FnOnce() -> FfiResult,
{
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(status) => status,
        Err(_) => FfiResult::PANIC_CAUGHT,
    }
}

/// Macro to export a mod's lifecycle entry points (`bevy_abi_mod_abi_version`,
/// `bevy_abi_mod_init`, and `bevy_abi_mod_shutdown`) with guest-side panic containment.
#[macro_export]
macro_rules! export_mod {
    ($init_fn:path, $shutdown_fn:path) => {
        #[no_mangle]
        pub unsafe extern "C" fn bevy_abi_mod_abi_version() -> u32 {
            $crate::ABI_VERSION_PACKED
        }

        #[no_mangle]
        pub unsafe extern "C" fn bevy_abi_mod_init(
            get_proc_address: $crate::GetProcAddressFn,
            host_api: *const $crate::HostApi,
            host_context: *mut ::core::ffi::c_void,
        ) -> $crate::FfiResult {
            $crate::catch_guest_panic(|| $init_fn(get_proc_address, host_api, host_context))
        }

        #[no_mangle]
        pub unsafe extern "C" fn bevy_abi_mod_shutdown(
            host_context: *mut ::core::ffi::c_void,
        ) -> $crate::FfiResult {
            $crate::catch_guest_panic(|| $shutdown_fn(host_context))
        }
    };
}

/// Macro to wrap a guest Rust system function into a C-ABI `DynamicSystemCallback`
/// (`unsafe extern "C" fn`) that catches guest panics and returns `FfiResult::PANIC_CAUGHT`.
#[macro_export]
macro_rules! wrap_system_callback {
    ($vis:vis fn $wrapper_name:ident calls $inner_fn:path) => {
        $vis unsafe extern "C" fn $wrapper_name(
            iter: *mut $crate::RawChunkIter,
            commands: *mut $crate::RawCommandQueue,
            user_data: *mut ::core::ffi::c_void,
        ) -> $crate::FfiResult {
            $crate::catch_guest_panic(|| {
                // SAFETY: Forwarding opaque handles received from the host executor.
                unsafe { $inner_fn(iter, commands, user_data) }
            })
        }
    };
}

/// Macro to wrap a guest Rust multi-query system function into a C-ABI `MultiQuerySystemCallback`
/// (`unsafe extern "C" fn`) that catches guest panics and returns `FfiResult::PANIC_CAUGHT`.
#[macro_export]
macro_rules! wrap_multi_query_callback {
    ($vis:vis fn $wrapper_name:ident calls $inner_fn:path) => {
        $vis unsafe extern "C" fn $wrapper_name(
            iters: *const *mut $crate::RawChunkIter,
            iter_count: usize,
            commands: *mut $crate::RawCommandQueue,
            user_data: *mut ::core::ffi::c_void,
        ) -> $crate::FfiResult {
            $crate::catch_guest_panic(|| {
                // SAFETY: Forwarding opaque handles received from the host executor.
                unsafe { $inner_fn(iters, iter_count, commands, user_data) }
            })
        }
    };
}

pub type CmdSpawnEmptyFn = unsafe extern "C" fn(queue: *mut RawCommandQueue) -> FfiEntity;
pub type CmdDespawnFn =
    unsafe extern "C" fn(queue: *mut RawCommandQueue, entity: FfiEntity) -> FfiResult;
pub type CmdInsertComponentFn = unsafe extern "C" fn(
    queue: *mut RawCommandQueue,
    entity: FfiEntity,
    component_name: *const core::ffi::c_char,
    data: *const u8,
    len: usize,
) -> FfiResult;

#[derive(Clone, Copy, Default, Debug)]
pub struct CommandBridgeVTable {
    pub spawn_empty: Option<CmdSpawnEmptyFn>,
    pub despawn: Option<CmdDespawnFn>,
    pub insert_component: Option<CmdInsertComponentFn>,
}

impl CommandBridgeVTable {
    pub fn resolve(get_proc_address: GetProcAddressFn) -> Self {
        let mut spawn_ptr = unsafe { get_proc_address(b"cmd_spawn_empty\0".as_ptr() as *const _) };
        if spawn_ptr.is_null() {
            spawn_ptr = unsafe { get_proc_address(b"host_cmd_spawn_empty\0".as_ptr() as *const _) };
        }
        let mut despawn_ptr = unsafe { get_proc_address(b"cmd_despawn\0".as_ptr() as *const _) };
        if despawn_ptr.is_null() {
            despawn_ptr = unsafe { get_proc_address(b"host_cmd_despawn\0".as_ptr() as *const _) };
        }
        let mut insert_ptr =
            unsafe { get_proc_address(b"cmd_insert_component\0".as_ptr() as *const _) };
        if insert_ptr.is_null() {
            insert_ptr =
                unsafe { get_proc_address(b"host_cmd_insert_component\0".as_ptr() as *const _) };
        }

        Self {
            spawn_empty: if !spawn_ptr.is_null() {
                // SAFETY: Pointer resolved from host GetProcAddress matching CmdSpawnEmptyFn signature.
                Some(unsafe { core::mem::transmute(spawn_ptr) })
            } else {
                None
            },
            despawn: if !despawn_ptr.is_null() {
                // SAFETY: Pointer resolved from host GetProcAddress matching CmdDespawnFn signature.
                Some(unsafe { core::mem::transmute(despawn_ptr) })
            } else {
                None
            },
            insert_component: if !insert_ptr.is_null() {
                // SAFETY: Pointer resolved from host GetProcAddress matching CmdInsertComponentFn signature.
                Some(unsafe { core::mem::transmute(insert_ptr) })
            } else {
                None
            },
        }
    }
}

pub struct GuestCommandQueue<'a> {
    raw: *mut RawCommandQueue,
    vtable: CommandBridgeVTable,
    _marker: core::marker::PhantomData<&'a mut ()>,
}

impl<'a> GuestCommandQueue<'a> {
    #[inline]
    pub fn new(raw: *mut RawCommandQueue, vtable: CommandBridgeVTable) -> Self {
        Self {
            raw,
            vtable,
            _marker: core::marker::PhantomData,
        }
    }

    #[inline]
    pub fn spawn_empty(&mut self) -> FfiEntity {
        if self.raw.is_null() {
            return FfiEntity(0);
        }
        if let Some(f) = self.vtable.spawn_empty {
            // SAFETY: Calling resolved function pointer with valid raw queue handle.
            unsafe { f(self.raw) }
        } else {
            FfiEntity(0)
        }
    }

    #[inline]
    pub fn despawn(&mut self, entity: FfiEntity) -> FfiResult {
        if self.raw.is_null() {
            return FfiResult::NULL_POINTER;
        }
        if let Some(f) = self.vtable.despawn {
            // SAFETY: Calling resolved function pointer with valid raw queue handle and entity.
            unsafe { f(self.raw, entity) }
        } else {
            FfiResult::NULL_POINTER
        }
    }

    #[inline]
    pub fn insert_bytes(
        &mut self,
        entity: FfiEntity,
        component_name: *const core::ffi::c_char,
        data: *const u8,
        len: usize,
    ) -> FfiResult {
        if self.raw.is_null() {
            return FfiResult::NULL_POINTER;
        }
        if let Some(f) = self.vtable.insert_component {
            // SAFETY: Calling resolved function pointer with valid raw queue handle, entity, and byte buffer.
            unsafe { f(self.raw, entity, component_name, data, len) }
        } else {
            FfiResult::NULL_POINTER
        }
    }
}

pub struct GuestChunkIter<'a> {
    raw: *mut RawChunkIter,
    api: &'a HostApi,
}

impl<'a> GuestChunkIter<'a> {
    #[inline]
    pub fn new(raw: *mut RawChunkIter, api: &'a HostApi) -> Self {
        Self { raw, api }
    }

    #[inline]
    pub fn next_chunk(&mut self) -> usize {
        // SAFETY: Delegating to HostApi function pointer.
        unsafe { (self.api.iter_next_chunk)(self.raw) }
    }

    #[inline]
    pub fn column_readonly(&self, index: usize) -> *const u8 {
        // SAFETY: Delegating to HostApi function pointer.
        unsafe { (self.api.iter_column_readonly)(self.raw, index) }
    }

    #[inline]
    pub fn column_mutable(&mut self, index: usize) -> *mut u8 {
        // SAFETY: Delegating to HostApi function pointer.
        unsafe { (self.api.iter_column_mutable)(self.raw, index) }
    }

    #[inline]
    pub fn mark_changed(&mut self, index: usize, row: usize) -> FfiResult {
        // SAFETY: Delegating to HostApi function pointer.
        unsafe { (self.api.iter_mark_changed)(self.raw, index, row) }
    }

    #[inline]
    pub fn entities(&self) -> *const FfiEntity {
        // SAFETY: Delegating to HostApi function pointer.
        unsafe { (self.api.iter_entities)(self.raw) }
    }

    #[inline]
    pub fn column_stride(&self, index: usize) -> usize {
        // SAFETY: Delegating to HostApi function pointer.
        unsafe { (self.api.iter_column_stride)(self.raw, index) }
    }

    /// Returns a structured, bounds-checked read-only slice for the given column.
    #[inline]
    pub fn readonly_slice(&self, index: usize, count: usize) -> ChunkSlice<'_> {
        let ptr = self.column_readonly(index);
        let stride = self.column_stride(index);
        ChunkSlice::new(ptr, stride, count)
    }

    /// Returns a structured, bounds-checked mutable slice for the given column.
    #[inline]
    pub fn mutable_slice(&mut self, index: usize, count: usize) -> ChunkSliceMut<'_> {
        let ptr = self.column_mutable(index);
        let stride = self.column_stride(index);
        ChunkSliceMut::new(ptr, stride, count)
    }
}

/// Safe read-only chunk slice view for iterating components in an archetype table chunk.
#[derive(Clone, Copy, Debug)]
pub struct ChunkSlice<'a> {
    ptr: *const u8,
    stride: usize,
    count: usize,
    _marker: core::marker::PhantomData<&'a ()>,
}

impl<'a> ChunkSlice<'a> {
    #[inline]
    pub fn new(ptr: *const u8, stride: usize, count: usize) -> Self {
        Self {
            ptr,
            stride,
            count,
            _marker: core::marker::PhantomData,
        }
    }

    #[inline]
    pub fn count(&self) -> usize {
        self.count
    }

    #[inline]
    pub fn stride(&self) -> usize {
        self.stride
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.count == 0 || self.ptr.is_null()
    }

    /// Returns a pointer to the element at the specified index within the chunk.
    #[inline]
    pub fn get_ptr(&self, index: usize) -> Option<*const u8> {
        if index < self.count && !self.ptr.is_null() {
            // SAFETY: Bounds checked, index is strictly less than count, stride is verified.
            unsafe { Some(self.ptr.add(index * self.stride)) }
        } else {
            None
        }
    }
}

/// Safe mutable chunk slice view for iterating components in an archetype table chunk.
#[derive(Debug)]
pub struct ChunkSliceMut<'a> {
    ptr: *mut u8,
    stride: usize,
    count: usize,
    _marker: core::marker::PhantomData<&'a mut ()>,
}

impl<'a> ChunkSliceMut<'a> {
    #[inline]
    pub fn new(ptr: *mut u8, stride: usize, count: usize) -> Self {
        Self {
            ptr,
            stride,
            count,
            _marker: core::marker::PhantomData,
        }
    }

    #[inline]
    pub fn count(&self) -> usize {
        self.count
    }

    #[inline]
    pub fn stride(&self) -> usize {
        self.stride
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.count == 0 || self.ptr.is_null()
    }

    /// Returns a mutable pointer to the element at the specified index within the chunk.
    #[inline]
    pub fn get_ptr_mut(&mut self, index: usize) -> Option<*mut u8> {
        if index < self.count && !self.ptr.is_null() {
            // SAFETY: Bounds checked, index is strictly less than count, stride is verified.
            unsafe { Some(self.ptr.add(index * self.stride)) }
        } else {
            None
        }
    }
}

/// Convenient prelude module for guest mod authors.
pub mod prelude {
    pub use crate::{
        catch_guest_panic, export_mod, wrap_multi_query_callback, wrap_system_callback, ChunkSlice,
        ChunkSliceMut, CommandBridgeVTable, GuestChunkIter, GuestCommandQueue,
    };
    pub use abi_contract::{
        ComponentAccessDesc, ComponentAccessMode, ComponentUuid, DynamicSystemCallback,
        ExposableComponent, FfiEntity, FfiResult, FfiSlice, GetProcAddressFn, HostApi, IterResetFn,
        MultiQuerySystemCallback, MultiQuerySystemDescriptor, QuerySpec, RawChunkIter,
        RawCommandQueue, RegisterMultiQuerySystemFn, RegisterSystemFn, StrView, SystemDescriptor,
        ABI_VERSION_MAJOR, ABI_VERSION_MINOR, ABI_VERSION_PACKED, ABI_VERSION_PATCH,
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::ffi::c_void;

    unsafe fn panicking_system(
        _iter: *mut RawChunkIter,
        _commands: *mut RawCommandQueue,
        _user_data: *mut c_void,
    ) -> FfiResult {
        panic!("mod crash");
    }

    unsafe fn healthy_system(
        _iter: *mut RawChunkIter,
        _commands: *mut RawCommandQueue,
        _user_data: *mut c_void,
    ) -> FfiResult {
        FfiResult::OK
    }

    wrap_system_callback!(fn c_panicking_system calls panicking_system);
    wrap_system_callback!(fn c_healthy_system calls healthy_system);

    #[test]
    fn test_guest_panic_containment_returns_panic_caught() {
        // SAFETY: Passing null opaque pointers to test callbacks that do not dereference them.
        let status = unsafe {
            c_panicking_system(
                core::ptr::null_mut(),
                core::ptr::null_mut(),
                core::ptr::null_mut(),
            )
        };
        assert_eq!(status, FfiResult::PANIC_CAUGHT);

        // SAFETY: Passing null opaque pointers to healthy test callback.
        let ok_status = unsafe {
            c_healthy_system(
                core::ptr::null_mut(),
                core::ptr::null_mut(),
                core::ptr::null_mut(),
            )
        };
        assert_eq!(ok_status, FfiResult::OK);
    }

    #[test]
    fn test_chunk_slice_and_mut_bounds() {
        let mut data = [10u32, 20u32, 30u32, 40u32];
        let stride = core::mem::size_of::<u32>();
        let count = data.len();

        let slice = ChunkSlice::new(data.as_ptr() as *const u8, stride, count);
        assert_eq!(slice.count(), 4);
        assert_eq!(slice.stride(), 4);
        assert!(!slice.is_empty());

        let p0 = slice.get_ptr(0).unwrap();
        // SAFETY: p0 is valid u32 pointer.
        assert_eq!(unsafe { *(p0 as *const u32) }, 10);

        let p2 = slice.get_ptr(2).unwrap();
        // SAFETY: p2 is valid u32 pointer.
        assert_eq!(unsafe { *(p2 as *const u32) }, 30);

        assert!(slice.get_ptr(4).is_none());
        assert!(slice.get_ptr(100).is_none());

        let mut slice_mut = ChunkSliceMut::new(data.as_mut_ptr() as *mut u8, stride, count);
        let p1 = slice_mut.get_ptr_mut(1).unwrap();
        // SAFETY: p1 is valid mutable u32 pointer.
        unsafe { *(p1 as *mut u32) = 99 };
        assert_eq!(data[1], 99);
        assert!(slice_mut.get_ptr_mut(4).is_none());
    }
}
