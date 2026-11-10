#![deny(unsafe_op_in_unsafe_fn)]
#![deny(improper_ctypes_definitions)]

//! # `guest_sdk`: Zero-Bevy Ergonomic Rust SDK for Mod Authors
//!
//! This crate provides safe, idiomatic Rust abstractions for creating dynamic mods
//! (`.cdylib`) that integrate seamlessly into Bevy ECS via the `bevy_abi_mod` framework.
//!
//! ## Architecture: Why Zero Bevy Dependency?
//! Mod dynamic libraries must compile independently of the host game engine. If guest mods
//! linked `bevy`, any discrepancy in `rustc` compiler version, Cargo feature flags, or standard
//! library implementations would trigger catastrophic ABI incompatibility and undefined behavior.
//! By depending solely on the `#![no_std]` [`abi_contract`] crate, `guest_sdk` enables mods
//! to interact with the host exclusively over a stable `extern "C"` boundary.
//!
//! ## Core Architectural Features
//! 1. **Cross-Runtime Panic Containment ([`catch_guest_panic`])**:
//!    Each compiled `.cdylib` links its own Rust runtime. Unwinding a panic across the `extern "C"`
//!    boundary into the host process violates the C ABI and aborts the application. The macros
//!    [`export_mod!`] and [`wrap_system_callback!`] intercept panics using `std::panic::catch_unwind`
//!    inside the guest runtime, translating any unwind into [`FfiResult::PANIC_CAUGHT`].
//! 2. **Zero-Copy Archetype Iteration ([`GuestChunkIter`])**:
//!    Bevy ECS stores components in archetype tables. The host passes a raw chunk iterator handle.
//!    [`GuestChunkIter`] traverses chunks table-by-table and yields bounds-checked [`ChunkSlice`]
//!    and [`ChunkSliceMut`] views with known byte strides.
//! 3. **Deferred Commands ([`GuestCommandQueue`])**:
//!    Direct structural mutation (entity spawning, despawning, component insertion) during query
//!    iteration is illegal in ECS. [`GuestCommandQueue`] bridges commands to the host's Bevy command
//!    queue, copying component bytes to maintain strict host allocator isolation.
//!
//! ## Example Mod Definition
//!
//! ```no_run
//! use guest_sdk::prelude::*;
//! use core::ffi::c_void;
//!
//! unsafe fn my_system(
//!     _iter: *mut RawChunkIter,
//!     _commands: *mut RawCommandQueue,
//!     _user_data: *mut c_void,
//! ) -> FfiResult {
//!     FfiResult::OK
//! }
//!
//! wrap_system_callback!(fn c_my_system calls my_system);
//!
//! fn mod_init(_get_proc_address: GetProcAddressFn, _host_api: *const HostApi, _ctx: *mut c_void) -> FfiResult {
//!     FfiResult::OK
//! }
//! fn mod_shutdown(_ctx: *mut c_void) -> FfiResult { FfiResult::OK }
//! export_mod!(mod_init, mod_shutdown);
//! ```

pub use abi_contract::{
    ComponentAccessDesc, ComponentAccessMode, ComponentUuid, DynamicSystemCallback,
    ExposableComponent, FfiEntity, FfiLayout, FfiResult, FfiSlice, GetProcAddressFn, HostApi,
    IterColumnMutableFn, IterColumnReadonlyFn, IterColumnStrideFn, IterEntitiesFn,
    IterMarkChangedFn, IterNextChunkFn, IterResetFn, ModAbiVersionFn, ModInitFn, ModShutdownFn,
    MultiQuerySystemCallback, MultiQuerySystemDescriptor, QuerySpec, RawChunkIter, RawCommandQueue,
    RegisterMultiQuerySystemFn, RegisterSystemFn, StrView, SymbolEntry, SystemDescriptor,
    ABI_VERSION_MAJOR, ABI_VERSION_MINOR, ABI_VERSION_PACKED, ABI_VERSION_PATCH,
};

/// Executes a closure inside `std::panic::catch_unwind` within the guest's own Rust runtime,
/// converting any unwinding panic into `FfiResult::PANIC_CAUGHT` before crossing the `extern "C"`
/// boundary.
///
/// ### Why Panic Containment is Done on the Guest Side
/// Each `.cdylib` links its own isolated `std` runtime. Unwinding a guest panic into the host's
/// `std::panic::catch_unwind` is classified by the Rust Reference as catching a foreign exception
/// (which has unspecified behavior and may abort the process). Catching the unwind inside the
/// guest's own `std` runtime and returning `FfiResult::PANIC_CAUGHT` by value is 100% well-defined.
///
/// # Example
///
/// ```
/// use guest_sdk::{catch_guest_panic, FfiResult};
///
/// let status = catch_guest_panic(|| {
///     FfiResult::OK
/// });
/// assert_eq!(status, FfiResult::OK);
///
/// let panic_status = catch_guest_panic(|| {
///     panic!("Simulated mod panic");
/// });
/// assert_eq!(panic_status, FfiResult::PANIC_CAUGHT);
/// ```
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
///
/// # Example
///
/// ```no_run
/// use guest_sdk::{export_mod, FfiResult, GetProcAddressFn, HostApi};
/// use core::ffi::c_void;
///
/// fn init(_get_proc: GetProcAddressFn, _api: *const HostApi, _ctx: *mut c_void) -> FfiResult {
///     FfiResult::OK
/// }
/// fn shutdown(_ctx: *mut c_void) -> FfiResult { FfiResult::OK }
/// export_mod!(init, shutdown);
/// ```
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
///
/// # Example
///
/// ```
/// use guest_sdk::{wrap_system_callback, FfiResult, RawChunkIter, RawCommandQueue};
/// use core::ffi::c_void;
///
/// unsafe fn movement(
///     _iter: *mut RawChunkIter,
///     _commands: *mut RawCommandQueue,
///     _user_data: *mut c_void,
/// ) -> FfiResult {
///     FfiResult::OK
/// }
/// wrap_system_callback!(fn c_movement calls movement);
/// ```
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
///
/// # Example
///
/// ```
/// use guest_sdk::{wrap_multi_query_callback, FfiResult, RawChunkIter, RawCommandQueue};
/// use core::ffi::c_void;
///
/// unsafe fn multi_query(
///     _iters: *const *mut RawChunkIter,
///     _count: usize,
///     _commands: *mut RawCommandQueue,
///     _user_data: *mut c_void,
/// ) -> FfiResult {
///     FfiResult::OK
/// }
/// wrap_multi_query_callback!(fn c_multi_query calls multi_query);
/// ```
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

/// C-ABI function pointer type for deferred empty entity creation.
pub type CmdSpawnEmptyFn = unsafe extern "C" fn(queue: *mut RawCommandQueue) -> FfiEntity;

/// C-ABI function pointer type for deferred entity despawning.
pub type CmdDespawnFn =
    unsafe extern "C" fn(queue: *mut RawCommandQueue, entity: FfiEntity) -> FfiResult;

/// C-ABI function pointer type for deferred raw component byte insertion.
pub type CmdInsertComponentFn = unsafe extern "C" fn(
    queue: *mut RawCommandQueue,
    entity: FfiEntity,
    component_name: *const core::ffi::c_char,
    data: *const u8,
    len: usize,
) -> FfiResult;

/// Cached function pointers for host command queue operations.
#[derive(Clone, Copy, Default, Debug)]
pub struct CommandBridgeVTable {
    /// Function pointer for `cmd_spawn_empty`.
    pub spawn_empty: Option<CmdSpawnEmptyFn>,
    /// Function pointer for `cmd_despawn`.
    pub despawn: Option<CmdDespawnFn>,
    /// Function pointer for `cmd_insert_component`.
    pub insert_component: Option<CmdInsertComponentFn>,
}

impl CommandBridgeVTable {
    /// Resolves command bridge functions from the host's symbol provider.
    pub fn resolve(get_proc_address: GetProcAddressFn) -> Self {
        let mut spawn_ptr = unsafe { get_proc_address(c"cmd_spawn_empty".as_ptr()) };
        if spawn_ptr.is_null() {
            spawn_ptr = unsafe { get_proc_address(c"host_cmd_spawn_empty".as_ptr()) };
        }
        let mut despawn_ptr = unsafe { get_proc_address(c"cmd_despawn".as_ptr()) };
        if despawn_ptr.is_null() {
            despawn_ptr = unsafe { get_proc_address(c"host_cmd_despawn".as_ptr()) };
        }
        let mut insert_ptr = unsafe { get_proc_address(c"cmd_insert_component".as_ptr()) };
        if insert_ptr.is_null() {
            insert_ptr = unsafe { get_proc_address(c"host_cmd_insert_component".as_ptr()) };
        }

        Self {
            spawn_empty: if !spawn_ptr.is_null() {
                // SAFETY: Pointer resolved from host GetProcAddress matching CmdSpawnEmptyFn signature.
                Some(unsafe {
                    core::mem::transmute::<*const core::ffi::c_void, CmdSpawnEmptyFn>(spawn_ptr)
                })
            } else {
                None
            },
            despawn: if !despawn_ptr.is_null() {
                // SAFETY: Pointer resolved from host GetProcAddress matching CmdDespawnFn signature.
                Some(unsafe {
                    core::mem::transmute::<*const core::ffi::c_void, CmdDespawnFn>(despawn_ptr)
                })
            } else {
                None
            },
            insert_component: if !insert_ptr.is_null() {
                // SAFETY: Pointer resolved from host GetProcAddress matching CmdInsertComponentFn signature.
                Some(unsafe {
                    core::mem::transmute::<*const core::ffi::c_void, CmdInsertComponentFn>(
                        insert_ptr,
                    )
                })
            } else {
                None
            },
        }
    }
}

/// Safe wrapper around the host's deferred command queue handle ([`RawCommandQueue`]).
///
/// # Example
///
/// ```
/// use guest_sdk::{GuestCommandQueue, CommandBridgeVTable, FfiEntity, FfiResult};
///
/// let mut queue = GuestCommandQueue::new(core::ptr::null_mut(), CommandBridgeVTable::default());
/// assert_eq!(queue.spawn_empty(), FfiEntity(0));
/// assert_eq!(queue.despawn(FfiEntity(1)), FfiResult::NULL_POINTER);
/// assert_eq!(queue.insert_value(FfiEntity(1), c"Position", &42.0f32), FfiResult::NULL_POINTER);
/// ```
pub struct GuestCommandQueue<'a> {
    raw: *mut RawCommandQueue,
    vtable: CommandBridgeVTable,
    _marker: core::marker::PhantomData<&'a mut ()>,
}

impl<'a> GuestCommandQueue<'a> {
    /// Creates a new guest command queue wrapper from an opaque handle and resolved VTable.
    #[inline]
    pub fn new(raw: *mut RawCommandQueue, vtable: CommandBridgeVTable) -> Self {
        Self {
            raw,
            vtable,
            _marker: core::marker::PhantomData,
        }
    }

    /// Enqueues the creation of a new empty entity and returns its allocated [`FfiEntity`].
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

    /// Enqueues the despawning of the specified entity.
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

    /// Enqueues the insertion of raw component bytes for the target entity.
    ///
    /// # Safety
    /// - `component_name` must point to a valid, nul-terminated UTF-8 C string.
    /// - `data` must point to at least `len` contiguous initialized bytes matching the component layout.
    /// - If `len > 0`, `data` must not be null.
    /// - The memory pointed to by `data` must not be concurrently mutated during this call.
    #[inline]
    pub unsafe fn insert_bytes(
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
            // SAFETY: Forwarding valid raw pointers per caller's safety contract.
            unsafe { f(self.raw, entity, component_name, data, len) }
        } else {
            FfiResult::NULL_POINTER
        }
    }

    /// Safely enqueues the insertion of a typed slice of elements for the target entity.
    ///
    /// Safe because `&core::ffi::CStr` guarantees a valid nul-terminated C string,
    /// and `&[T]` where `T: Copy` guarantees valid initialized memory.
    #[inline]
    pub fn insert_slice<T: Copy>(
        &mut self,
        entity: FfiEntity,
        component_name: &core::ffi::CStr,
        data: &[T],
    ) -> FfiResult {
        // SAFETY: CStr is nul-terminated and &[T] is a valid contiguous byte range.
        unsafe {
            self.insert_bytes(
                entity,
                component_name.as_ptr(),
                data.as_ptr() as *const u8,
                core::mem::size_of_val(data),
            )
        }
    }

    /// Safely enqueues the insertion of a single typed component value for the target entity.
    #[inline]
    pub fn insert_value<T: Copy>(
        &mut self,
        entity: FfiEntity,
        component_name: &core::ffi::CStr,
        value: &T,
    ) -> FfiResult {
        self.insert_slice(entity, component_name, core::slice::from_ref(value))
    }
}

/// Safe wrapper around the host's chunk iterator handle ([`RawChunkIter`]).
pub struct GuestChunkIter<'a> {
    raw: *mut RawChunkIter,
    api: &'a HostApi,
}

impl<'a> GuestChunkIter<'a> {
    /// Creates a new guest chunk iterator wrapper from an opaque handle and host API table.
    #[inline]
    pub fn new(raw: *mut RawChunkIter, api: &'a HostApi) -> Self {
        Self { raw, api }
    }

    /// Advances to the next archetype table chunk and returns the entity count (0 when exhausted).
    #[inline]
    pub fn next_chunk(&mut self) -> usize {
        // SAFETY: Delegating to HostApi function pointer.
        unsafe { (self.api.iter_next_chunk)(self.raw) }
    }

    /// Obtains a read-only pointer to the start of the specified column in the current chunk.
    #[inline]
    pub fn column_readonly(&self, index: usize) -> *const u8 {
        // SAFETY: Delegating to HostApi function pointer.
        unsafe { (self.api.iter_column_readonly)(self.raw, index) }
    }

    /// Obtains a mutable pointer to the start of the specified column in the current chunk.
    #[inline]
    pub fn column_mutable(&mut self, index: usize) -> *mut u8 {
        // SAFETY: Delegating to HostApi function pointer.
        unsafe { (self.api.iter_column_mutable)(self.raw, index) }
    }

    /// Explicitly marks the given row within the column as changed.
    #[inline]
    pub fn mark_changed(&mut self, index: usize, row: usize) -> FfiResult {
        // SAFETY: Delegating to HostApi function pointer.
        unsafe { (self.api.iter_mark_changed)(self.raw, index, row) }
    }

    /// Obtains a pointer to the contiguous array of entity IDs in the current chunk.
    #[inline]
    pub fn entities(&self) -> *const FfiEntity {
        // SAFETY: Delegating to HostApi function pointer.
        unsafe { (self.api.iter_entities)(self.raw) }
    }

    /// Returns the byte stride for elements in the specified column.
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
///
/// # Example
///
/// ```
/// use guest_sdk::ChunkSlice;
///
/// let numbers = [10u32, 20, 30];
/// let slice = ChunkSlice::new(numbers.as_ptr() as *const u8, core::mem::size_of::<u32>(), numbers.len());
/// assert_eq!(slice.count(), 3);
/// assert_eq!(slice.stride(), 4);
/// assert!(!slice.is_empty());
/// let ptr = slice.get_ptr(1).unwrap();
/// assert_eq!(unsafe { *(ptr as *const u32) }, 20);
/// ```
#[derive(Clone, Copy, Debug)]
pub struct ChunkSlice<'a> {
    ptr: *const u8,
    stride: usize,
    count: usize,
    _marker: core::marker::PhantomData<&'a ()>,
}

impl<'a> ChunkSlice<'a> {
    /// Creates a new read-only chunk slice view.
    #[inline]
    pub fn new(ptr: *const u8, stride: usize, count: usize) -> Self {
        Self {
            ptr,
            stride,
            count,
            _marker: core::marker::PhantomData,
        }
    }

    /// Returns the number of entity elements in the current chunk.
    #[inline]
    pub fn count(&self) -> usize {
        self.count
    }

    /// Returns the byte stride between consecutive elements in the column.
    #[inline]
    pub fn stride(&self) -> usize {
        self.stride
    }

    /// Returns `true` if the chunk slice contains zero elements or points to null.
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
///
/// # Example
///
/// ```
/// use guest_sdk::ChunkSliceMut;
///
/// let mut numbers = [10u32, 20, 30];
/// let mut slice = ChunkSliceMut::new(numbers.as_mut_ptr() as *mut u8, core::mem::size_of::<u32>(), numbers.len());
/// let ptr = slice.get_ptr_mut(1).unwrap();
/// unsafe { *(ptr as *mut u32) = 99 };
/// assert_eq!(numbers[1], 99);
/// ```
#[derive(Debug)]
pub struct ChunkSliceMut<'a> {
    ptr: *mut u8,
    stride: usize,
    count: usize,
    _marker: core::marker::PhantomData<&'a mut ()>,
}

impl<'a> ChunkSliceMut<'a> {
    /// Creates a new mutable chunk slice view.
    #[inline]
    pub fn new(ptr: *mut u8, stride: usize, count: usize) -> Self {
        Self {
            ptr,
            stride,
            count,
            _marker: core::marker::PhantomData,
        }
    }

    /// Returns the number of entity elements in the current chunk.
    #[inline]
    pub fn count(&self) -> usize {
        self.count
    }

    /// Returns the byte stride between consecutive elements in the column.
    #[inline]
    pub fn stride(&self) -> usize {
        self.stride
    }

    /// Returns `true` if the chunk slice contains zero elements or points to null.
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
        RawCommandQueue, RegisterMultiQuerySystemFn, RegisterSystemFn, StrView, SymbolEntry,
        SystemDescriptor, ABI_VERSION_MAJOR, ABI_VERSION_MINOR, ABI_VERSION_PACKED,
        ABI_VERSION_PATCH,
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
