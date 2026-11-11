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
//! 2. **Zero-Copy Disjoint Column Iteration ([`GuestChunkIter`], [`Chunk`])**:
//!    Bevy ECS stores components in archetype tables. [`GuestChunkIter::next_chunk_view`] yields
//!    per-chunk [`Chunk`] handles supporting simultaneous disjoint borrows of entity IDs, read-only
//!    columns, and multiple mutable columns (`read_write`, `columns_mut`, `split_slices`) with
//!    strict index and byte-range non-overlap validation.
//! 3. **Leak-Free App Builder ([`GuestApp`], [`SystemContext`])**:
//!    Eliminates `static AtomicPtr<HostApi>` boilerplate and `Box::into_raw` leaks by managing
//!    system closures and custom `user_data` allocations in a guest-owned registry that rolls back
//!    on failed initialization and frees all state automatically during `bevy_abi_mod_shutdown`.
//! 4. **Deferred Commands ([`GuestCommandQueue`])**:
//!    Direct structural mutation (entity spawning, despawning, component insertion) during query
//!    iteration is illegal in ECS. [`GuestCommandQueue`] bridges commands to the host's Bevy command
//!    queue, copying component bytes to maintain strict host allocator isolation.
//!
//! ## Idiomatic Mod Definition
//!
//! ```no_run
//! use guest_sdk::prelude::*;
//!
//! #[repr(C)]
//! #[derive(Clone, Copy)]
//! struct Position { x: f32, y: f32, z: f32 }
//!
//! #[repr(C)]
//! #[derive(Clone, Copy)]
//! struct Velocity { x: f32, y: f32, z: f32 }
//!
//! fn setup(app: &mut GuestApp<'_>) -> FfiResult {
//!     let queries = [
//!         ComponentAccessDesc::write(c"Position".as_ptr()),
//!         ComponentAccessDesc::read(c"Velocity".as_ptr()),
//!     ];
//!     app.add_system(c"movement_system", &queries, |ctx| {
//!         while let Some(mut chunk) = ctx.iter().next_chunk_view() {
//!             let Some((vel, pos)) = chunk.read_write::<Velocity, Position>(1, 0) else {
//!                 return FfiResult::INTERNAL_ERROR;
//!             };
//!             for (p, v) in pos.iter_mut().zip(vel.iter()) {
//!                 p.x += v.x;
//!                 p.y += v.y;
//!                 p.z += v.z;
//!             }
//!         }
//!         FfiResult::OK
//!     })
//! }
//!
//! export_mod!(setup);
//! ```

use core::ffi::{c_char, c_void, CStr};
use std::sync::Mutex;

pub use abi_contract::{
    ComponentAccessDesc, ComponentAccessMode, ComponentDropFn, ComponentUuid,
    DynamicComponentDescriptor, DynamicSystemCallback, ExposableComponent, FfiEntity, FfiLayout,
    FfiResult, FfiSlice, GetProcAddressFn, HostApi, IterColumnMutableFn, IterColumnReadonlyFn,
    IterColumnStrideFn, IterEntitiesFn, IterMarkChangedFn, IterNextChunkFn, IterResetFn,
    ModAbiVersionFn, ModComponentDescriptor, ModInitFn, ModShutdownFn, MultiQuerySystemCallback,
    MultiQuerySystemDescriptor, QuerySpec, RawChunkIter, RawCommandQueue, RegisterComponentFn,
    RegisterMultiQuerySystemFn, RegisterSystemFn, StrView, SymbolEntry, SystemDescriptor,
    ABI_VERSION_MAJOR, ABI_VERSION_MINOR, ABI_VERSION_PACKED, ABI_VERSION_PATCH,
};

/// Maximum number of simultaneous queries supported per [`MultiQueryContext`] without heap allocation.
pub const MAX_GUEST_MULTI_QUERIES: usize = 8;

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

// ============================================================================
// Guest-Owned Allocation & Lifecycle Registry
// ============================================================================

struct ErasedAllocation {
    ptr: usize,
    drop_fn: unsafe fn(*mut c_void),
}

unsafe fn drop_boxed_allocation<T>(ptr: *mut c_void) {
    if !ptr.is_null() {
        // SAFETY: `ptr` was allocated via `Box::into_raw(Box::new(...))` of type `T`
        // inside this guest `.cdylib`'s allocator and is dropped at most once.
        drop(unsafe { Box::from_raw(ptr as *mut T) });
    }
}

struct ShutdownHook<F: FnOnce()> {
    hook: Option<F>,
}

impl<F: FnOnce()> Drop for ShutdownHook<F> {
    fn drop(&mut self) {
        if let Some(f) = self.hook.take() {
            f();
        }
    }
}

static GUEST_ALLOCATIONS: Mutex<Vec<ErasedAllocation>> = Mutex::new(Vec::new());

fn drop_allocation_list_lifo(mut list: Vec<ErasedAllocation>) {
    while let Some(alloc) = list.pop() {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // SAFETY: Each `ErasedAllocation` owns a unique guest-heap allocation paired with
            // its matching monomorphized `drop_fn`.
            unsafe { (alloc.drop_fn)(alloc.ptr as *mut c_void) };
        }));
    }
}

/// Drops all guest-side system closures, `user_data` allocations, and shutdown hooks
/// registered via [`GuestApp`] in reverse registration (LIFO) order.
///
/// This function is invoked automatically by [`export_mod!`] during `bevy_abi_mod_shutdown`,
/// after the host has deactivated all scheduled systems belonging to this shared library.
///
/// # Safety Contract Note
/// Safe to call once all host systems referencing committed [`GuestApp`] callbacks have ceased
/// executing (guaranteed during `bevy_abi_mod_shutdown` or in standalone tests after simulation).
pub fn cleanup_registered_guest_state() {
    let drained = {
        let mut guard = GUEST_ALLOCATIONS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        core::mem::take(&mut *guard)
    };
    drop_allocation_list_lifo(drained);
}

/// Macro to export a mod's lifecycle entry points (`bevy_abi_mod_abi_version`,
/// `bevy_abi_mod_init`, and `bevy_abi_mod_shutdown`) with guest-side panic containment
/// and automatic [`GuestApp`] state teardown.
///
/// Supports three forms:
/// 1. `export_mod!(setup_fn)` — takes `fn(&mut GuestApp<'_>) -> FfiResult`.
/// 2. `export_mod!(plugin: MyModType)` — takes a type implementing [`GuestPlugin`].
/// 3. `export_mod!(init_fn, shutdown_fn)` — low-level C-ABI init/shutdown functions.
///
/// # Example
///
/// ```no_run
/// use guest_sdk::{export_mod, FfiResult, GuestApp};
///
/// fn setup(_app: &mut GuestApp<'_>) -> FfiResult {
///     FfiResult::OK
/// }
/// export_mod!(setup);
/// ```
#[macro_export]
macro_rules! export_mod {
    ($build_fn:path) => {
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
            // SAFETY: Entry point invoked by host during `load_mod` with valid handshake pointers.
            unsafe { $crate::run_guest_app_init(get_proc_address, host_api, host_context, $build_fn) }
        }

        #[no_mangle]
        pub unsafe extern "C" fn bevy_abi_mod_shutdown(
            _host_context: *mut ::core::ffi::c_void,
        ) -> $crate::FfiResult {
            $crate::catch_guest_panic(|| {
                $crate::cleanup_registered_guest_state();
                $crate::FfiResult::OK
            })
        }
    };
    (plugin: $plugin_ty:ty) => {
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
            // SAFETY: Entry point invoked by host during `load_mod` with valid handshake pointers.
            unsafe {
                $crate::run_guest_app_init(
                    get_proc_address,
                    host_api,
                    host_context,
                    <$plugin_ty as $crate::GuestPlugin>::build,
                )
            }
        }

        #[no_mangle]
        pub unsafe extern "C" fn bevy_abi_mod_shutdown(
            _host_context: *mut ::core::ffi::c_void,
        ) -> $crate::FfiResult {
            let status = $crate::catch_guest_panic(|| <$plugin_ty as $crate::GuestPlugin>::shutdown());
            $crate::cleanup_registered_guest_state();
            status
        }
    };
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
            let status = $crate::catch_guest_panic(|| $shutdown_fn(host_context));
            $crate::cleanup_registered_guest_state();
            status
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

// ============================================================================
// Deferred Command Queue Bridge
// ============================================================================

/// C-ABI function pointer type for deferred empty entity creation.
pub type CmdSpawnEmptyFn = unsafe extern "C" fn(queue: *mut RawCommandQueue) -> FfiEntity;

/// C-ABI function pointer type for deferred entity despawning.
pub type CmdDespawnFn =
    unsafe extern "C" fn(queue: *mut RawCommandQueue, entity: FfiEntity) -> FfiResult;

/// C-ABI function pointer type for deferred raw component byte insertion.
pub type CmdInsertComponentFn = unsafe extern "C" fn(
    queue: *mut RawCommandQueue,
    entity: FfiEntity,
    component_name: *const c_char,
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
    ///
    /// # Raw Symbol Contract
    /// Expects `get_proc_address` to be the valid symbol resolver supplied by the host
    /// during `bevy_abi_mod_init`, returning function pointers matching the C-ABI signatures
    /// for `"cmd_spawn_empty"`, `"cmd_despawn"`, and `"cmd_insert_component"`.
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
                Some(unsafe { core::mem::transmute::<*const c_void, CmdSpawnEmptyFn>(spawn_ptr) })
            } else {
                None
            },
            despawn: if !despawn_ptr.is_null() {
                // SAFETY: Pointer resolved from host GetProcAddress matching CmdDespawnFn signature.
                Some(unsafe { core::mem::transmute::<*const c_void, CmdDespawnFn>(despawn_ptr) })
            } else {
                None
            },
            insert_component: if !insert_ptr.is_null() {
                // SAFETY: Pointer resolved from host GetProcAddress matching CmdInsertComponentFn signature.
                Some(unsafe {
                    core::mem::transmute::<*const c_void, CmdInsertComponentFn>(insert_ptr)
                })
            } else {
                None
            },
        }
    }
}

/// Resolves the `iter_reset` host function pointer via `get_proc_address`, if supported by the host.
#[inline]
pub fn resolve_iter_reset(get_proc_address: GetProcAddressFn) -> Option<IterResetFn> {
    let mut ptr = unsafe { get_proc_address(c"iter_reset".as_ptr()) };
    if ptr.is_null() {
        ptr = unsafe { get_proc_address(c"host_iter_reset".as_ptr()) };
    }
    if ptr.is_null() {
        None
    } else {
        // SAFETY: Symbol `"iter_reset"` resolved from host `GetProcAddressFn` has signature `IterResetFn`.
        Some(unsafe { core::mem::transmute::<*const c_void, IterResetFn>(ptr) })
    }
}

/// Resolves `register_multi_query_system` via `get_proc_address`, if supported by the host.
#[inline]
pub fn resolve_register_multi_query(
    get_proc_address: GetProcAddressFn,
) -> Option<RegisterMultiQuerySystemFn> {
    let mut ptr = unsafe { get_proc_address(c"register_multi_query_system".as_ptr()) };
    if ptr.is_null() {
        ptr = unsafe { get_proc_address(c"host_register_multi_query_system".as_ptr()) };
    }
    if ptr.is_null() {
        None
    } else {
        // SAFETY: Symbol `"register_multi_query_system"` resolved from host matches `RegisterMultiQuerySystemFn`.
        Some(unsafe { core::mem::transmute::<*const c_void, RegisterMultiQuerySystemFn>(ptr) })
    }
}

/// Wrapper around the host's deferred command queue handle ([`RawCommandQueue`]).
///
/// Prefer obtaining a `GuestCommandQueue` safely through [`SystemContext::commands`] or
/// [`SystemContext::iter_and_commands`] when using [`GuestApp`].
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
    ///
    /// # Raw Handle Assumptions
    /// If `raw` is non-null, it must be the valid `*mut RawCommandQueue` handle supplied by the
    /// host for the current system callback (or a compatible mock context matching `vtable` in tests).
    /// Prefer [`SystemContext::commands`] in [`GuestApp`] systems for sound-by-construction access.
    #[inline]
    pub fn new(raw: *mut RawCommandQueue, vtable: CommandBridgeVTable) -> Self {
        Self {
            raw,
            vtable,
            _marker: core::marker::PhantomData,
        }
    }

    /// Returns `true` if the underlying `RawCommandQueue` pointer is null.
    #[inline]
    pub fn is_null(&self) -> bool {
        self.raw.is_null()
    }

    /// Enqueues the creation of a new empty entity and returns its allocated [`FfiEntity`].
    ///
    /// Returns `FfiEntity(0)` (`NULL`) if the command queue handle or `spawn_empty` function is unavailable.
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
        component_name: *const c_char,
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

    /// Safely enqueues the insertion of a typed slice of POD elements for the target entity.
    #[inline]
    pub fn insert_slice<T: Copy>(
        &mut self,
        entity: FfiEntity,
        component_name: &CStr,
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

    /// Safely enqueues the insertion of a single typed POD component value for the target entity.
    #[inline]
    pub fn insert_value<T: Copy>(
        &mut self,
        entity: FfiEntity,
        component_name: &CStr,
        value: &T,
    ) -> FfiResult {
        self.insert_slice(entity, component_name, core::slice::from_ref(value))
    }

    /// Safely enqueues the deferred insertion of a typed [`GuestComponent`] value onto `entity`.
    #[inline]
    pub fn insert_component<T: GuestComponent>(
        &mut self,
        entity: FfiEntity,
        value: &T,
    ) -> FfiResult {
        self.insert_value(entity, T::NAME, value)
    }

    /// Safely enqueues the deferred insertion of a typed runtime dynamic component value onto the target entity.
    ///
    /// The host runtime copies the raw byte representation into host-allocated memory before
    /// scheduling the deferred ECS insertion, maintaining strict memory allocator isolation (Rule 2).
    #[inline]
    pub fn insert_runtime_component<T: Copy>(
        &mut self,
        entity: FfiEntity,
        name: &CStr,
        value: &T,
    ) -> FfiResult {
        self.insert_value(entity, name, value)
    }
}

// ============================================================================
// Chunk Iteration & Disjoint Multi-Column Split Borrows
// ============================================================================

#[inline]
fn byte_ranges_overlap(addr_a: usize, len_a: usize, addr_b: usize, len_b: usize) -> bool {
    if len_a == 0 || len_b == 0 {
        return false;
    }
    let end_a = addr_a.saturating_add(len_a);
    let end_b = addr_b.saturating_add(len_b);
    addr_a < end_b && addr_b < end_a
}

#[inline]
fn validated_entities_slice<'a>(ptr: *const FfiEntity, count: usize) -> &'a [FfiEntity] {
    if count == 0 || ptr.is_null() {
        return &[];
    }
    let elem_size = core::mem::size_of::<FfiEntity>();
    let elem_align = core::mem::align_of::<FfiEntity>();
    if !(ptr as usize).is_multiple_of(elem_align) || count > (isize::MAX as usize) {
        return &[];
    }
    let Some(total_bytes) = count.checked_mul(elem_size) else {
        return &[];
    };
    if total_bytes > (isize::MAX as usize) || (ptr as usize).checked_add(total_bytes).is_none() {
        return &[];
    }
    // SAFETY:
    // - `ptr` is non-null and aligned for `FfiEntity`.
    // - `total_bytes <= isize::MAX as usize` without address-space wrap-around.
    // - Caller ties `'a` to the active chunk borrow on `GuestChunkIter`.
    unsafe { core::slice::from_raw_parts(ptr, count) }
}

/// Wrapper around the host's chunk iterator handle ([`RawChunkIter`]).
///
/// Supports both low-level pointer/stride access and safe multi-column disjoint borrows via
/// [`GuestChunkIter::next_chunk_view`], [`GuestChunkIter::read_write`],
/// [`GuestChunkIter::columns_mut`], and [`GuestChunkIter::split_slices`].
pub struct GuestChunkIter<'a> {
    raw: *mut RawChunkIter,
    api: &'a HostApi,
    reset_fn: Option<IterResetFn>,
}

impl<'a> GuestChunkIter<'a> {
    /// Creates a new guest chunk iterator wrapper from an opaque handle and host API table.
    ///
    /// # Raw Handle Assumptions
    /// If `raw` is non-null, it must point to an active `RawChunkIter` provided by the host
    /// for the duration of the enclosing system callback. Prefer [`SystemContext::iter`] in
    /// [`GuestApp`] systems for sound-by-construction access.
    #[inline]
    pub fn new(raw: *mut RawChunkIter, api: &'a HostApi) -> Self {
        Self {
            raw,
            api,
            reset_fn: None,
        }
    }

    /// Creates a new guest chunk iterator wrapper with a pre-resolved [`IterResetFn`] for nested loops.
    #[inline]
    pub fn with_reset(raw: *mut RawChunkIter, api: &'a HostApi, reset_fn: Option<IterResetFn>) -> Self {
        Self { raw, api, reset_fn }
    }

    /// Attaches a resolved [`IterResetFn`] to enable [`GuestChunkIter::reset`].
    #[inline]
    pub fn set_reset_fn(&mut self, reset_fn: Option<IterResetFn>) {
        self.reset_fn = reset_fn;
    }

    /// Rewinds the chunk iterator cursor back to the first matched chunk so inner queries
    /// in nested multi-query systems can be re-traversed with zero heap allocations.
    ///
    /// Returns [`FfiResult::NULL_POINTER`] if the underlying handle is null or no [`IterResetFn`]
    /// was attached (use [`GuestApp::add_multi_query_system`], [`GuestChunkIter::with_reset`], or
    /// [`GuestChunkIter::reset_with`]).
    #[inline]
    pub fn reset(&mut self) -> FfiResult {
        if self.raw.is_null() {
            return FfiResult::NULL_POINTER;
        }
        let Some(reset_fn) = self.reset_fn else {
            return FfiResult::NULL_POINTER;
        };
        // SAFETY: `self.raw` is non-null and exclusively borrowed via `&mut self`.
        unsafe { reset_fn(self.raw) }
    }

    /// Rewinds the chunk iterator cursor using an explicitly supplied [`IterResetFn`].
    #[inline]
    pub fn reset_with(&mut self, reset_fn: IterResetFn) -> FfiResult {
        if self.raw.is_null() {
            return FfiResult::NULL_POINTER;
        }
        // SAFETY: `self.raw` is non-null and exclusively borrowed via `&mut self`.
        unsafe { reset_fn(self.raw) }
    }

    /// Advances to the next archetype table chunk and returns the entity count (`0` when exhausted).
    #[inline]
    pub fn next_chunk(&mut self) -> usize {
        if self.raw.is_null() {
            return 0;
        }
        // SAFETY: Delegating to HostApi function pointer with non-null `self.raw`.
        unsafe { (self.api.iter_next_chunk)(self.raw) }
    }

    /// Advances to the next chunk and returns a scoped [`Chunk`] view if non-empty (`None` when exhausted).
    ///
    /// Holding a [`Chunk`] borrows `self` mutably until the chunk step finishes, statically preventing
    /// advancing or resetting the iterator while any entity or column slices from that chunk remain live.
    #[inline]
    pub fn next_chunk_view(&mut self) -> Option<Chunk<'_, 'a>> {
        let count = self.next_chunk();
        if count == 0 {
            None
        } else {
            Some(Chunk { iter: self, count })
        }
    }

    /// Obtains a read-only pointer to the start of the specified column in the current chunk.
    #[inline]
    pub fn column_readonly(&self, index: usize) -> *const u8 {
        if self.raw.is_null() {
            return core::ptr::null();
        }
        // SAFETY: Delegating to HostApi function pointer with non-null `self.raw`.
        unsafe { (self.api.iter_column_readonly)(self.raw, index) }
    }

    /// Obtains a mutable pointer to the start of the specified column in the current chunk.
    #[inline]
    pub fn column_mutable(&mut self, index: usize) -> *mut u8 {
        if self.raw.is_null() {
            return core::ptr::null_mut();
        }
        // SAFETY: Delegating to HostApi function pointer with non-null `self.raw`.
        unsafe { (self.api.iter_column_mutable)(self.raw, index) }
    }

    /// Explicitly marks the given row within the column as changed.
    #[inline]
    pub fn mark_changed(&mut self, index: usize, row: usize) -> FfiResult {
        if self.raw.is_null() {
            return FfiResult::NULL_POINTER;
        }
        // SAFETY: Delegating to HostApi function pointer with non-null `self.raw`.
        unsafe { (self.api.iter_mark_changed)(self.raw, index, row) }
    }

    /// Obtains a raw pointer to the contiguous array of entity IDs in the current chunk.
    ///
    /// Prefer [`GuestChunkIter::entities_slice`] or [`Chunk::entities`] for safe bounds-checked access.
    #[inline]
    pub fn entities(&self) -> *const FfiEntity {
        if self.raw.is_null() {
            return core::ptr::null();
        }
        // SAFETY: Delegating to HostApi function pointer with non-null `self.raw`.
        unsafe { (self.api.iter_entities)(self.raw) }
    }

    /// Returns a validated slice of [`FfiEntity`] identifiers for the current chunk.
    ///
    /// Returns `&[]` if `count == 0`, the iterator handle or entity pointer is null/misaligned,
    /// or `count` exceeds Rust slice bounds.
    #[inline]
    pub fn entities_slice(&self, count: usize) -> &[FfiEntity] {
        if self.raw.is_null() || count == 0 {
            return &[];
        }
        validated_entities_slice(self.entities(), count)
    }

    /// Returns the byte stride for elements in the specified column.
    #[inline]
    pub fn column_stride(&self, index: usize) -> usize {
        if self.raw.is_null() {
            return 0;
        }
        // SAFETY: Delegating to HostApi function pointer with non-null `self.raw`.
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

    /// Simultaneously borrows `entities`, `R` read-only columns, and `W` disjoint mutable columns
    /// from the active chunk.
    ///
    /// Returns `None` if:
    /// - Any two write column indices alias (`write_cols[i] == write_cols[j]`),
    /// - Any read column index equals any write column index (`read_cols[r] == write_cols[w]`),
    /// - When `count > 0`, any requested column returns a null pointer or its byte range overlaps
    ///   any mutable column or the entities slice.
    pub fn split_slices<const R: usize, const W: usize>(
        &mut self,
        read_cols: [usize; R],
        write_cols: [usize; W],
        count: usize,
    ) -> Option<(&[FfiEntity], [ChunkSlice<'_>; R], [ChunkSliceMut<'_>; W])> {
        // 1. Verify pairwise disjointness of write indices and read-vs-write indices BEFORE mutating ticks.
        for i in 0..W {
            for j in (i + 1)..W {
                if write_cols[i] == write_cols[j] {
                    return None;
                }
            }
            for &r_col in &read_cols {
                if r_col == write_cols[i] {
                    return None;
                }
            }
        }

        if self.raw.is_null() && count > 0 {
            return None;
        }

        let ent_ptr = self.entities();
        let ent_slice = validated_entities_slice(ent_ptr, count);
        let ent_addr = ent_slice.as_ptr() as usize;
        let ent_bytes = core::mem::size_of_val(ent_slice);

        let mut ro_ptrs = [core::ptr::null::<u8>(); R];
        let mut ro_strides = [0usize; R];
        let mut ro_bytes = [0usize; R];

        for i in 0..R {
            let ptr = self.column_readonly(read_cols[i]);
            let stride = self.column_stride(read_cols[i]);
            if count > 0 && ptr.is_null() {
                return None;
            }
            let bytes = count.checked_mul(stride)?;
            if bytes > (isize::MAX as usize) {
                return None;
            }
            if count > 0 && (ptr as usize).checked_add(bytes).is_none() {
                return None;
            }
            ro_ptrs[i] = ptr;
            ro_strides[i] = stride;
            ro_bytes[i] = bytes;
        }

        let mut rw_ptrs = [core::ptr::null_mut::<u8>(); W];
        let mut rw_strides = [0usize; W];
        let mut rw_bytes = [0usize; W];

        for i in 0..W {
            let ptr = self.column_mutable(write_cols[i]);
            let stride = self.column_stride(write_cols[i]);
            if count > 0 && ptr.is_null() {
                return None;
            }
            let bytes = count.checked_mul(stride)?;
            if bytes > (isize::MAX as usize) {
                return None;
            }
            if count > 0 && (ptr as usize).checked_add(bytes).is_none() {
                return None;
            }
            rw_ptrs[i] = ptr;
            rw_strides[i] = stride;
            rw_bytes[i] = bytes;
        }

        // 2. Verify physical memory byte-range non-overlap for every mutable column against
        //    other mutable columns, read-only columns, and the entity slice.
        if count > 0 {
            for i in 0..W {
                let addr_w = rw_ptrs[i] as usize;
                let len_w = rw_bytes[i];

                if byte_ranges_overlap(addr_w, len_w, ent_addr, ent_bytes) {
                    return None;
                }
                for j in (i + 1)..W {
                    if byte_ranges_overlap(addr_w, len_w, rw_ptrs[j] as usize, rw_bytes[j]) {
                        return None;
                    }
                }
                for r in 0..R {
                    if byte_ranges_overlap(addr_w, len_w, ro_ptrs[r] as usize, ro_bytes[r]) {
                        return None;
                    }
                }
            }
        }

        let ro_slices = core::array::from_fn(|i| ChunkSlice::new(ro_ptrs[i], ro_strides[i], count));
        let rw_slices =
            core::array::from_fn(|i| ChunkSliceMut::new(rw_ptrs[i], rw_strides[i], count));

        Some((ent_slice, ro_slices, rw_slices))
    }

    /// Borrows `N` disjoint mutable columns simultaneously from the active chunk.
    ///
    /// Returns `None` if any two indices in `indices` are equal, or if any column pointer is null
    /// or overlaps another column when `count > 0`.
    #[inline]
    pub fn columns_mut<const N: usize>(
        &mut self,
        indices: [usize; N],
        count: usize,
    ) -> Option<[ChunkSliceMut<'_>; N]> {
        let (_, _, rw) = self.split_slices([], indices, count)?;
        Some(rw)
    }

    /// Simultaneously borrows one typed read-only column (`&[R]`) and one typed mutable column (`&mut [W]`).
    ///
    /// Returns `None` if `read_col == write_col`, if either column fails stride/alignment/null checks,
    /// or if their memory ranges overlap.
    #[inline]
    pub fn read_write<R: Copy, W: Copy>(
        &mut self,
        read_col: usize,
        write_col: usize,
        count: usize,
    ) -> Option<(&[R], &mut [W])> {
        let (_, [ro], [rw]) = self.split_slices([read_col], [write_col], count)?;
        let r_typed = ro.as_typed_slice::<R>()?;
        let w_typed = rw.into_typed_slice::<W>()?;
        Some((r_typed, w_typed))
    }

    /// Simultaneously borrows two disjoint typed mutable columns (`&mut [W1]`, `&mut [W2]`).
    ///
    /// Returns `None` if `col_a == col_b`, if either column fails stride/alignment/null checks,
    /// or if their memory ranges overlap.
    #[inline]
    pub fn write_2<W1: Copy, W2: Copy>(
        &mut self,
        col_a: usize,
        col_b: usize,
        count: usize,
    ) -> Option<(&mut [W1], &mut [W2])> {
        let [rw_a, rw_b] = self.columns_mut([col_a, col_b], count)?;
        let a_typed = rw_a.into_typed_slice::<W1>()?;
        let b_typed = rw_b.into_typed_slice::<W2>()?;
        Some((a_typed, b_typed))
    }
}

/// Scoped handle to a single non-empty chunk yielded by [`GuestChunkIter::next_chunk_view`].
///
/// Automatically tracks the chunk's entity count and enforces borrow lifetimes so slices from
/// the current chunk cannot outlive the chunk iteration step.
pub struct Chunk<'iter, 'api> {
    iter: &'iter mut GuestChunkIter<'api>,
    count: usize,
}

impl<'iter, 'api> Chunk<'iter, 'api> {
    /// Number of entities in this chunk (guaranteed `>= 1` when yielded by `next_chunk_view`).
    #[inline]
    pub fn len(&self) -> usize {
        self.count
    }

    /// Alias for [`Chunk::len`].
    #[inline]
    pub fn count(&self) -> usize {
        self.count
    }

    /// Returns `true` if this chunk has zero entities.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Returns the validated slice of [`FfiEntity`] handles for all rows in this chunk.
    #[inline]
    pub fn entities(&self) -> &[FfiEntity] {
        self.iter.entities_slice(self.count)
    }

    /// Returns an untyped bounds-checked read-only [`ChunkSlice`] for column `index`.
    #[inline]
    pub fn readonly_slice(&self, index: usize) -> ChunkSlice<'_> {
        self.iter.readonly_slice(index, self.count)
    }

    /// Returns an untyped bounds-checked mutable [`ChunkSliceMut`] for column `index`.
    #[inline]
    pub fn mutable_slice(&mut self, index: usize) -> ChunkSliceMut<'_> {
        self.iter.mutable_slice(index, self.count)
    }

    /// Borrows a typed read-only slice `&[T]` for column `index`.
    #[inline]
    pub fn column<T: Copy>(&self, index: usize) -> Option<&[T]> {
        self.iter
            .readonly_slice(index, self.count)
            .as_typed_slice::<T>()
    }

    /// Borrows a typed mutable slice `&mut [T]` for column `index`.
    #[inline]
    pub fn column_mut<T: Copy>(&mut self, index: usize) -> Option<&mut [T]> {
        self.iter
            .mutable_slice(index, self.count)
            .into_typed_slice::<T>()
    }

    /// Simultaneously borrows the chunk's [`FfiEntity`] slice and one typed mutable column `&mut [T]`.
    #[inline]
    pub fn entities_and_column_mut<T: Copy>(
        &mut self,
        index: usize,
    ) -> Option<(&[FfiEntity], &mut [T])> {
        let count = self.count;
        let (entities, _, [rw]) = self.iter.split_slices([], [index], count)?;
        let typed = rw.into_typed_slice::<T>()?;
        Some((entities, typed))
    }

    /// Simultaneously borrows `entities`, `R` read-only [`ChunkSlice`]s, and `W` disjoint [`ChunkSliceMut`]s.
    #[inline]
    pub fn split_slices<const R: usize, const W: usize>(
        &mut self,
        read_cols: [usize; R],
        write_cols: [usize; W],
    ) -> Option<(&[FfiEntity], [ChunkSlice<'_>; R], [ChunkSliceMut<'_>; W])> {
        let count = self.count;
        self.iter.split_slices(read_cols, write_cols, count)
    }

    /// Simultaneously borrows `N` disjoint mutable [`ChunkSliceMut`] columns.
    #[inline]
    pub fn columns_mut<const N: usize>(
        &mut self,
        indices: [usize; N],
    ) -> Option<[ChunkSliceMut<'_>; N]> {
        let count = self.count;
        self.iter.columns_mut(indices, count)
    }

    /// Simultaneously borrows one typed read-only column `&[R]` and one disjoint typed mutable column `&mut [W]`.
    #[inline]
    pub fn read_write<R: Copy, W: Copy>(
        &mut self,
        read_col: usize,
        write_col: usize,
    ) -> Option<(&[R], &mut [W])> {
        let count = self.count;
        self.iter.read_write(read_col, write_col, count)
    }

    /// Simultaneously borrows `entities`, one typed read-only column `&[R]`, and one disjoint typed mutable column `&mut [W]`.
    #[inline]
    pub fn entities_and_read_write<R: Copy, W: Copy>(
        &mut self,
        read_col: usize,
        write_col: usize,
    ) -> Option<(&[FfiEntity], &[R], &mut [W])> {
        let count = self.count;
        let (entities, [ro], [rw]) = self.iter.split_slices([read_col], [write_col], count)?;
        let r_typed = ro.as_typed_slice::<R>()?;
        let w_typed = rw.into_typed_slice::<W>()?;
        Some((entities, r_typed, w_typed))
    }

    /// Simultaneously borrows two disjoint typed mutable columns (`&mut [W1]`, `&mut [W2]`).
    #[inline]
    pub fn write_2<W1: Copy, W2: Copy>(
        &mut self,
        col_a: usize,
        col_b: usize,
    ) -> Option<(&mut [W1], &mut [W2])> {
        let count = self.count;
        self.iter.write_2(col_a, col_b, count)
    }

    /// Explicitly marks `row` in column `index` as changed.
    #[inline]
    pub fn mark_changed(&mut self, index: usize, row: usize) -> FfiResult {
        self.iter.mark_changed(index, row)
    }
}

// ============================================================================
// ChunkSlice & ChunkSliceMut
// ============================================================================

/// Read-only chunk slice view for iterating components in an archetype table chunk.
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
    ///
    /// # Raw Pointer Contract
    /// When `count > 0` and `ptr` is non-null, callers who subsequently convert this slice via
    /// [`ChunkSlice::as_typed_slice`] or dereference pointers from [`ChunkSlice::get_ptr`] rely on
    /// `ptr` pointing to `count * stride` initialized, immutable bytes valid for `'a`.
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
            let offset = index.checked_mul(self.stride)?;
            if offset > (isize::MAX as usize) {
                return None;
            }
            (self.ptr as usize).checked_add(offset)?;
            // SAFETY: Bounds checked, index is strictly less than count, offset fits within isize::MAX without wrapping.
            unsafe { Some(self.ptr.add(offset)) }
        } else {
            None
        }
    }

    /// Converts this raw chunk slice into a typed slice of `#[repr(C)]` POD `T: Copy` elements.
    ///
    /// Validates that:
    /// 1. `self.stride == core::mem::size_of::<T>()`.
    /// 2. If `self.count == 0`, returns `Some(&[])`.
    /// 3. If `self.count > 0`:
    ///    - `self.ptr` is non-null and `(self.ptr as usize) % core::mem::align_of::<T>() == 0`.
    ///    - `self.count <= isize::MAX as usize`.
    ///    - `self.count * core::mem::size_of::<T>() <= isize::MAX as usize` without integer overflow.
    ///    - `(self.ptr as usize) + (self.count * size_of::<T>())` does not wrap around the address space.
    ///
    /// Note: `T` must be a `#[repr(C)]` plain-old-data type whose bit patterns in host storage are all valid for `T`.
    #[inline]
    pub fn as_typed_slice<T: Copy>(&self) -> Option<&'a [T]> {
        if self.stride != core::mem::size_of::<T>() {
            return None;
        }
        if self.count == 0 {
            return Some(&[]);
        }
        if self.ptr.is_null() || !(self.ptr as usize).is_multiple_of(core::mem::align_of::<T>()) {
            return None;
        }
        if self.count > (isize::MAX as usize) {
            return None;
        }
        let total_bytes = match self.count.checked_mul(core::mem::size_of::<T>()) {
            Some(bytes) if bytes <= (isize::MAX as usize) => bytes,
            _ => return None,
        };
        (self.ptr as usize).checked_add(total_bytes)?;
        // SAFETY:
        // - self.ptr is non-null and aligned for T.
        // - self.stride == size_of::<T>(), guaranteeing elements are contiguous in table storage.
        // - self.count <= isize::MAX as usize and total_bytes <= isize::MAX as usize.
        // - Adding total_bytes to self.ptr does not wrap around the address space.
        // - self.count elements are valid and initialized for reads during lifetime 'a.
        // - T: Copy guarantees no drop glue or ownership transfer.
        Some(unsafe { core::slice::from_raw_parts(self.ptr as *const T, self.count) })
    }
}

/// Mutable chunk slice view for iterating components in an archetype table chunk.
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
    ///
    /// # Raw Pointer Contract
    /// When `count > 0` and `ptr` is non-null, callers who subsequently convert this slice via
    /// [`ChunkSliceMut::as_typed_slice`] / [`ChunkSliceMut::into_typed_slice`] or dereference
    /// pointers from [`ChunkSliceMut::get_ptr_mut`] rely on `ptr` pointing to `count * stride`
    /// initialized, exclusively borrowed bytes valid for `'a`.
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
            let offset = index.checked_mul(self.stride)?;
            if offset > (isize::MAX as usize) {
                return None;
            }
            (self.ptr as usize).checked_add(offset)?;
            // SAFETY: Bounds checked, index is strictly less than count, offset fits within isize::MAX without wrapping.
            unsafe { Some(self.ptr.add(offset)) }
        } else {
            None
        }
    }

    #[inline]
    fn validate_typed_mut<T: Copy>(&self) -> Option<(*mut T, usize)> {
        if self.stride != core::mem::size_of::<T>() {
            return None;
        }
        if self.count == 0 {
            return Some((core::ptr::NonNull::<T>::dangling().as_ptr(), 0));
        }
        if self.ptr.is_null() || !(self.ptr as usize).is_multiple_of(core::mem::align_of::<T>()) {
            return None;
        }
        if self.count > (isize::MAX as usize) {
            return None;
        }
        let total_bytes = match self.count.checked_mul(core::mem::size_of::<T>()) {
            Some(bytes) if bytes <= (isize::MAX as usize) => bytes,
            _ => return None,
        };
        (self.ptr as usize).checked_add(total_bytes)?;
        Some((self.ptr as *mut T, self.count))
    }

    /// Reborrows this mutable chunk slice as a typed `&mut [T]` slice of `#[repr(C)]` POD elements.
    ///
    /// Validates stride, alignment, non-null pointer (when `count > 0`), `isize::MAX` bounds,
    /// and address-space non-wrapping.
    #[inline]
    pub fn as_typed_slice<T: Copy>(&mut self) -> Option<&mut [T]> {
        let (ptr, len) = self.validate_typed_mut::<T>()?;
        if len == 0 {
            return Some(&mut []);
        }
        // SAFETY:
        // - `validate_typed_mut` verified alignment, non-null `ptr`, exact `size_of::<T>()` stride,
        //   `isize::MAX` byte limit, and non-wrapping address range.
        // - Exclusive reborrow of `&mut self` prevents aliasing for the returned slice lifetime.
        Some(unsafe { core::slice::from_raw_parts_mut(ptr, len) })
    }

    /// Consumes this `ChunkSliceMut<'a>` and converts it into `&'a mut [T]`, transferring the
    /// full `'a` exclusive column borrow without intermediate temporary lifetime restrictions.
    #[inline]
    pub fn into_typed_slice<T: Copy>(self) -> Option<&'a mut [T]> {
        let (ptr, len) = self.validate_typed_mut::<T>()?;
        if len == 0 {
            return Some(&mut []);
        }
        // SAFETY:
        // - Consuming `self` by value ensures no other `ChunkSliceMut` reference to this column exists for `'a`.
        // - `validate_typed_mut` verified alignment, non-null `ptr`, exact `size_of::<T>()` stride,
        //   `isize::MAX` byte limit, and non-wrapping address range.
        Some(unsafe { core::slice::from_raw_parts_mut(ptr, len) })
    }
}

// ============================================================================
// Runtime Component Registration & GuestComponent Trait
// ============================================================================

/// Marker trait for `#[repr(C)]` plain-old-data component structs defined or mirrored by guest mods.
///
/// # Safety
/// Implementors must ensure:
/// 1. `Self` is `#[repr(C)]` (or `#[repr(transparent)]`) with deterministic cross-compiler layout.
/// 2. Every bit pattern stored in the host's component column is valid for `Self` (no `bool`,
///    Rust `enum`, references, or heap pointers).
pub unsafe trait GuestComponent: Copy + 'static {
    /// Registered C-ABI component name (e.g. `c"Position"`).
    const NAME: &'static CStr;

    /// Builds a read-only (`&T`) query access descriptor for this component.
    #[inline]
    fn read_desc() -> ComponentAccessDesc {
        ComponentAccessDesc::read(Self::NAME.as_ptr())
    }

    /// Builds a mutable (`&mut T`) query access descriptor for this component.
    #[inline]
    fn write_desc() -> ComponentAccessDesc {
        ComponentAccessDesc::write(Self::NAME.as_ptr())
    }

    /// Builds a presence filter (`With<T>`) query access descriptor for this component.
    #[inline]
    fn with_desc() -> ComponentAccessDesc {
        ComponentAccessDesc::filter_only(Self::NAME.as_ptr())
    }

    /// Builds an absence filter (`Without<T>`) query access descriptor for this component.
    #[inline]
    fn without_desc() -> ComponentAccessDesc {
        ComponentAccessDesc::without(Self::NAME.as_ptr())
    }
}

/// Helper trampoline for invoking `drop_in_place` on non-POD dynamic components.
unsafe extern "C" fn generic_drop_trampoline<T>(ptr: *mut c_void) {
    if !ptr.is_null() {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // SAFETY: ptr is non-null and points to an initialized instance of T.
            unsafe {
                core::ptr::drop_in_place(ptr as *mut T);
            }
        }));
    }
}

#[inline]
const fn min_host_api_v1_size() -> usize {
    core::mem::offset_of!(HostApi, register_component) + core::mem::size_of::<RegisterComponentFn>()
}

/// Registers a runtime dynamic component with the host Bevy ECS World.
///
/// Plain Old Data (POD) components (`needs_drop::<T>() == false`) set `drop_fn` to `None`, avoiding
/// unnecessary drop hook overhead on the host.
///
/// # Safety
/// - `host_api` must point to a valid, initialized [`HostApi`] table provided by the host engine.
/// - `host_context` must point to the valid initialization context passed to `bevy_abi_mod_init`.
/// - This function must only be called during mod initialization (`bevy_abi_mod_init`) while the host
///   retains mutable access to the Bevy `World`.
#[inline]
pub unsafe fn register_runtime_component<T: 'static>(
    host_api: *const HostApi,
    host_context: *mut c_void,
    name: &CStr,
) -> FfiResult {
    if host_api.is_null() || host_context.is_null() {
        return FfiResult::NULL_POINTER;
    }

    // SAFETY: Reading the first 4-byte `struct_size` header of `HostApi` before reading trailing fields.
    let struct_size = unsafe { *(host_api as *const u32) } as usize;
    if struct_size < min_host_api_v1_size() {
        return FfiResult::INVALID_VERSION;
    }

    // SAFETY: `host_api` is non-null and `struct_size` covers `register_component` without assuming full `size_of::<HostApi>()`.
    let register_component_fn =
        unsafe { core::ptr::addr_of!((*host_api).register_component).read() };

    let Ok(name_str) = name.to_str() else {
        return FfiResult::INVALID_UTF8;
    };

    let uuid = ComponentUuid::from_name(name_str);
    let size = core::mem::size_of::<T>();
    let align = core::mem::align_of::<T>();

    let drop_fn = if core::mem::needs_drop::<T>() {
        Some(generic_drop_trampoline::<T> as unsafe extern "C" fn(*mut c_void))
    } else {
        None
    };

    let desc = ModComponentDescriptor {
        struct_size: core::mem::size_of::<ModComponentDescriptor>() as u32,
        name: name.as_ptr(),
        uuid,
        size,
        align,
        drop_fn,
        vtable_ptr: core::ptr::null(),
    };

    // SAFETY: Forwarding valid descriptor and host_context to host registration callback.
    unsafe { register_component_fn(host_context, &desc) }
}

// ============================================================================
// High-Level Ergonomic Builder: GuestPlugin, GuestApp, SystemContext
// ============================================================================

/// Trait for modular Bevy-like guest plugin definitions exported via `export_mod!(plugin: MyMod)`.
pub trait GuestPlugin {
    /// Registers components, systems, and optional shutdown hooks on `app`.
    fn build(app: &mut GuestApp<'_>) -> FfiResult;

    /// Optional custom teardown callback invoked during `bevy_abi_mod_shutdown` prior to
    /// automatic [`GuestApp`] allocation cleanup.
    fn shutdown() -> FfiResult {
        FfiResult::OK
    }
}

/// Execution context passed to systems registered via [`GuestApp::add_system`].
///
/// Provides sound-by-construction access to the active [`GuestChunkIter`] and [`GuestCommandQueue`]
/// without requiring guest authors to handle raw FFI pointers or global `AtomicPtr<HostApi>` state.
pub struct SystemContext<'a> {
    iter: GuestChunkIter<'a>,
    commands: GuestCommandQueue<'a>,
    host_api: &'a HostApi,
}

impl<'a> SystemContext<'a> {
    /// Borrows the system's [`GuestChunkIter`] mutably.
    #[inline]
    pub fn iter(&mut self) -> &mut GuestChunkIter<'a> {
        &mut self.iter
    }

    /// Borrows the system's deferred [`GuestCommandQueue`] mutably.
    #[inline]
    pub fn commands(&mut self) -> &mut GuestCommandQueue<'a> {
        &mut self.commands
    }

    /// Simultaneously borrows both the [`GuestChunkIter`] and the deferred [`GuestCommandQueue`],
    /// allowing commands (such as `despawn` or `spawn_empty`) to be queued inside a chunk loop.
    #[inline]
    pub fn iter_and_commands(&mut self) -> (&mut GuestChunkIter<'a>, &mut GuestCommandQueue<'a>) {
        (&mut self.iter, &mut self.commands)
    }

    /// Returns a shared reference to the host's [`HostApi`] VTable.
    #[inline]
    pub fn host_api(&self) -> &'a HostApi {
        self.host_api
    }
}

/// Execution context passed to multi-query systems registered via [`GuestApp::add_multi_query_system`].
///
/// Stores up to [`MAX_GUEST_MULTI_QUERIES`] (8) pre-configured [`GuestChunkIter`] handles inline on
/// the stack with [`GuestChunkIter::reset`] already wired, performing zero per-frame heap allocations.
pub struct MultiQueryContext<'a> {
    iters: [Option<GuestChunkIter<'a>>; MAX_GUEST_MULTI_QUERIES],
    iter_count: usize,
    commands: GuestCommandQueue<'a>,
    host_api: &'a HostApi,
}

impl<'a> MultiQueryContext<'a> {
    /// Number of query iterators passed to this multi-query system.
    #[inline]
    pub fn iter_count(&self) -> usize {
        self.iter_count
    }

    /// Borrows query iterator `index` mutably.
    #[inline]
    pub fn iter_mut(&mut self, index: usize) -> Option<&mut GuestChunkIter<'a>> {
        if index < self.iter_count {
            self.iters[index].as_mut()
        } else {
            None
        }
    }

    /// Simultaneously borrows two disjoint query iterators `(index_a, index_b)` for nested query loops.
    ///
    /// Returns `None` if `index_a == index_b` or either index is out of bounds.
    #[inline]
    pub fn iters_2_mut(
        &mut self,
        index_a: usize,
        index_b: usize,
    ) -> Option<(&mut GuestChunkIter<'a>, &mut GuestChunkIter<'a>)> {
        if index_a == index_b || index_a >= self.iter_count || index_b >= self.iter_count {
            return None;
        }
        if index_a < index_b {
            let (left, right) = self.iters.split_at_mut(index_b);
            Some((left[index_a].as_mut()?, right[0].as_mut()?))
        } else {
            let (left, right) = self.iters.split_at_mut(index_a);
            Some((right[0].as_mut()?, left[index_b].as_mut()?))
        }
    }

    /// Borrows the deferred [`GuestCommandQueue`] mutably.
    #[inline]
    pub fn commands(&mut self) -> &mut GuestCommandQueue<'a> {
        &mut self.commands
    }

    /// Simultaneously borrows two disjoint query iterators AND the deferred [`GuestCommandQueue`].
    #[inline]
    pub fn iters_2_and_commands(
        &mut self,
        index_a: usize,
        index_b: usize,
    ) -> Option<(
        &mut GuestChunkIter<'a>,
        &mut GuestChunkIter<'a>,
        &mut GuestCommandQueue<'a>,
    )> {
        if index_a == index_b || index_a >= self.iter_count || index_b >= self.iter_count {
            return None;
        }
        let commands = &mut self.commands;
        if index_a < index_b {
            let (left, right) = self.iters.split_at_mut(index_b);
            Some((left[index_a].as_mut()?, right[0].as_mut()?, commands))
        } else {
            let (left, right) = self.iters.split_at_mut(index_a);
            Some((right[0].as_mut()?, left[index_b].as_mut()?, commands))
        }
    }

    /// Returns a shared reference to the host's [`HostApi`] VTable.
    #[inline]
    pub fn host_api(&self) -> &'a HostApi {
        self.host_api
    }
}

struct SystemClosureState<F> {
    host_api: *const HostApi,
    cmd_vtable: CommandBridgeVTable,
    iter_reset: Option<IterResetFn>,
    callback: Mutex<F>,
}

unsafe extern "C" fn c_system_closure_trampoline<F>(
    iter: *mut RawChunkIter,
    commands: *mut RawCommandQueue,
    user_data: *mut c_void,
) -> FfiResult
where
    F: FnMut(&mut SystemContext<'_>) -> FfiResult + Send + 'static,
{
    catch_guest_panic(|| {
        if user_data.is_null() {
            return FfiResult::NULL_POINTER;
        }
        // SAFETY: `user_data` points to an initialized `SystemClosureState<F>` owned by `GUEST_ALLOCATIONS`
        // that remains alive until `bevy_abi_mod_shutdown` runs after all systems are deactivated.
        let state = unsafe { &*(user_data as *const SystemClosureState<F>) };
        if state.host_api.is_null() {
            return FfiResult::NULL_POINTER;
        }
        // `try_lock` prevents `&mut F` aliasing UB even if a buggy host or test invokes the same
        // `user_data` concurrently or re-entrantly; under Bevy's scheduler it is always uncontended.
        let Ok(mut cb_guard) = state.callback.try_lock() else {
            return FfiResult::ACCESS_CONFLICT;
        };
        // SAFETY: `state.host_api` was validated during `GuestApp` initialization and remains valid
        // for the duration of the system callback.
        let api = unsafe { &*state.host_api };
        let mut ctx = SystemContext {
            iter: GuestChunkIter::with_reset(iter, api, state.iter_reset),
            commands: GuestCommandQueue::new(commands, state.cmd_vtable),
            host_api: api,
        };
        (*cb_guard)(&mut ctx)
    })
}

struct MultiQueryClosureState<F> {
    host_api: *const HostApi,
    cmd_vtable: CommandBridgeVTable,
    iter_reset: Option<IterResetFn>,
    callback: Mutex<F>,
}

unsafe extern "C" fn c_multi_query_closure_trampoline<F>(
    iters: *const *mut RawChunkIter,
    iter_count: usize,
    commands: *mut RawCommandQueue,
    user_data: *mut c_void,
) -> FfiResult
where
    F: FnMut(&mut MultiQueryContext<'_>) -> FfiResult + Send + 'static,
{
    catch_guest_panic(|| {
        if user_data.is_null() {
            return FfiResult::NULL_POINTER;
        }
        if iter_count > MAX_GUEST_MULTI_QUERIES {
            return FfiResult::INVALID_COMPONENT;
        }
        if iter_count > 0 && iters.is_null() {
            return FfiResult::NULL_POINTER;
        }
        // SAFETY: `user_data` points to an initialized `MultiQueryClosureState<F>` owned by `GUEST_ALLOCATIONS`.
        let state = unsafe { &*(user_data as *const MultiQueryClosureState<F>) };
        if state.host_api.is_null() {
            return FfiResult::NULL_POINTER;
        }
        let Ok(mut cb_guard) = state.callback.try_lock() else {
            return FfiResult::ACCESS_CONFLICT;
        };
        // SAFETY: `state.host_api` is valid for the duration of the callback.
        let api = unsafe { &*state.host_api };

        let mut iter_array: [Option<GuestChunkIter<'_>>; MAX_GUEST_MULTI_QUERIES] =
            core::array::from_fn(|_| None);
        for (slot, item) in iter_array.iter_mut().enumerate().take(iter_count) {
            // SAFETY: `iters` is non-null and holds `iter_count` pointers per `MultiQuerySystemCallback` contract.
            let raw_iter = unsafe { *iters.add(slot) };
            *item = Some(GuestChunkIter::with_reset(raw_iter, api, state.iter_reset));
        }

        let mut ctx = MultiQueryContext {
            iters: iter_array,
            iter_count,
            commands: GuestCommandQueue::new(commands, state.cmd_vtable),
            host_api: api,
        };
        (*cb_guard)(&mut ctx)
    })
}

/// High-level mod initialization builder eliminating raw FFI globals and `user_data` memory leaks.
///
/// Any system closures, [`GuestApp::alloc_user_data`] boxes, and [`GuestApp::on_shutdown`] hooks
/// created on `GuestApp` are staged transactionally:
/// - If `init` succeeds (`FfiResult::OK`), they are committed to the guest's lifecycle registry and
///   freed automatically in LIFO order when `bevy_abi_mod_shutdown` runs.
/// - If `init` returns an error or panics, [`Drop`] immediately rolls back and frees all staged
///   allocations so partial initialization failures never leak memory.
pub struct GuestApp<'a> {
    get_proc_address: GetProcAddressFn,
    host_api: &'a HostApi,
    host_context: *mut c_void,
    cmd_vtable: CommandBridgeVTable,
    iter_reset: Option<IterResetFn>,
    register_multi_query: Option<RegisterMultiQuerySystemFn>,
    staged_allocations: Vec<ErasedAllocation>,
}

impl<'a> GuestApp<'a> {
    /// Constructs a [`GuestApp`] from the raw C-ABI arguments passed to `bevy_abi_mod_init`.
    ///
    /// Validates non-null pointers, `HostApi::struct_size`, and semver ABI compatibility
    /// (`host_major == ABI_VERSION_MAJOR && host_minor >= ABI_VERSION_MINOR`).
    ///
    /// # Safety
    /// - `get_proc_address` must be a valid host symbol resolver function pointer.
    /// - If non-null, `host_api` must point to an initialized `HostApi` struct that outlives all
    ///   registered system executions for this mod load session.
    /// - If non-null, `host_context` must point to the host's initialization context for `bevy_abi_mod_init`.
    pub unsafe fn from_raw(
        get_proc_address: GetProcAddressFn,
        host_api: *const HostApi,
        host_context: *mut c_void,
    ) -> Result<Self, FfiResult> {
        if host_api.is_null() || host_context.is_null() {
            return Err(FfiResult::NULL_POINTER);
        }

        // SAFETY: Reading 4-byte `struct_size` header at offset 0 of non-null `host_api`.
        let struct_size = unsafe { *(host_api as *const u32) } as usize;
        if struct_size < core::mem::size_of::<HostApi>() {
            return Err(FfiResult::INVALID_VERSION);
        }

        // SAFETY: `struct_size >= size_of::<HostApi>()` covers the entire `HostApi` struct.
        let api_ref = unsafe { &*host_api };
        let host_major = (api_ref.abi_version >> 24) & 0xFF;
        let host_minor = (api_ref.abi_version >> 16) & 0xFF;
        if host_major != ABI_VERSION_MAJOR || host_minor < ABI_VERSION_MINOR {
            return Err(FfiResult::INVALID_VERSION);
        }

        let cmd_vtable = CommandBridgeVTable::resolve(get_proc_address);
        let iter_reset = resolve_iter_reset(get_proc_address);
        let register_multi_query = resolve_register_multi_query(get_proc_address);

        Ok(Self {
            get_proc_address,
            host_api: api_ref,
            host_context,
            cmd_vtable,
            iter_reset,
            register_multi_query,
            staged_allocations: Vec::new(),
        })
    }

    /// Returns the host's [`HostApi`] table.
    #[inline]
    pub fn host_api(&self) -> &'a HostApi {
        self.host_api
    }

    /// Returns the raw `host_context` pointer for interoperability with low-level registration helpers.
    #[inline]
    pub fn host_context(&self) -> *mut c_void {
        self.host_context
    }

    /// Returns the host's `GetProcAddressFn` symbol resolver.
    #[inline]
    pub fn get_proc_address(&self) -> GetProcAddressFn {
        self.get_proc_address
    }

    /// Returns the pre-resolved [`CommandBridgeVTable`] for this world.
    #[inline]
    pub fn command_vtable(&self) -> CommandBridgeVTable {
        self.cmd_vtable
    }

    /// Resolves a raw symbol pointer by nul-terminated name via `get_proc_address`.
    #[inline]
    pub fn resolve_symbol(&self, symbol_name: &CStr) -> *const c_void {
        // SAFETY: `symbol_name` is a valid nul-terminated CStr.
        unsafe { (self.get_proc_address)(symbol_name.as_ptr()) }
    }

    /// Resolves a typed static component VTable reference (e.g. `c"Position_vtable"`).
    ///
    /// # Safety
    /// Caller must ensure `V` matches the exact `#[repr(C)]` VTable layout exported by the host
    /// under `symbol_name`.
    #[inline]
    pub unsafe fn resolve_vtable<V: Copy + 'static>(&self, symbol_name: &CStr) -> Option<&'a V> {
        let ptr = self.resolve_symbol(symbol_name);
        if ptr.is_null() || !(ptr as usize).is_multiple_of(core::mem::align_of::<V>()) {
            None
        } else {
            // SAFETY: Caller guarantees `symbol_name` points to a valid static `V` outliving `'a`.
            Some(unsafe { &*(ptr as *const V) })
        }
    }

    /// Registers a runtime dynamic component `T` under `name`.
    #[inline]
    pub fn register_component<T: 'static>(&mut self, name: &CStr) -> FfiResult {
        // SAFETY: `self.host_api` and `self.host_context` were validated in `GuestApp::from_raw`.
        unsafe { register_runtime_component::<T>(self.host_api, self.host_context, name) }
    }

    /// Registers a runtime dynamic component implementing [`GuestComponent`] using `T::NAME`.
    #[inline]
    pub fn register_guest_component<T: GuestComponent>(&mut self) -> FfiResult {
        self.register_component::<T>(T::NAME)
    }

    /// Allocates a guest-owned value on the heap and returns a stable `*mut T` suitable for
    /// `SystemDescriptor::user_data`, automatically freeing it on `bevy_abi_mod_shutdown`
    /// (or immediately if `bevy_abi_mod_init` fails).
    pub fn alloc_user_data<T: Send + 'static>(&mut self, value: T) -> *mut T {
        let raw = Box::into_raw(Box::new(value));
        self.staged_allocations.push(ErasedAllocation {
            ptr: raw as usize,
            drop_fn: drop_boxed_allocation::<T>,
        });
        raw
    }

    /// Registers a custom cleanup closure to execute automatically during `bevy_abi_mod_shutdown`
    /// (or on rollback if `bevy_abi_mod_init` fails).
    pub fn on_shutdown<F>(&mut self, cleanup: F)
    where
        F: FnOnce() + Send + 'static,
    {
        let raw = Box::into_raw(Box::new(ShutdownHook {
            hook: Some(cleanup),
        }));
        self.staged_allocations.push(ErasedAllocation {
            ptr: raw as usize,
            drop_fn: drop_boxed_allocation::<ShutdownHook<F>>,
        });
    }

    /// Registers a panic-safe, leak-free Rust system closure with the host ECS schedule.
    ///
    /// Any captured state inside `system` is stored per mod-load instance (no global statics required)
    /// and automatically dropped when `bevy_abi_mod_shutdown` runs.
    pub fn add_system<F>(
        &mut self,
        name: &CStr,
        queries: &[ComponentAccessDesc],
        system: F,
    ) -> FfiResult
    where
        F: FnMut(&mut SystemContext<'_>) -> FfiResult + Send + 'static,
    {
        let state = Box::new(SystemClosureState {
            host_api: self.host_api as *const HostApi,
            cmd_vtable: self.cmd_vtable,
            iter_reset: self.iter_reset,
            callback: Mutex::new(system),
        });
        let state_ptr = Box::into_raw(state);

        let desc = SystemDescriptor {
            struct_size: core::mem::size_of::<SystemDescriptor>() as u32,
            name: name.as_ptr(),
            queries: if queries.is_empty() {
                core::ptr::null()
            } else {
                queries.as_ptr()
            },
            query_count: queries.len(),
            callback: c_system_closure_trampoline::<F>,
            user_data: state_ptr as *mut c_void,
        };

        // SAFETY: `self.host_context` and `desc` are valid for the duration of `register_system`.
        let status = unsafe { (self.host_api.register_system)(self.host_context, &desc) };
        if status.is_ok() {
            self.staged_allocations.push(ErasedAllocation {
                ptr: state_ptr as usize,
                drop_fn: drop_boxed_allocation::<SystemClosureState<F>>,
            });
        } else {
            // Host rejected registration and did not retain `user_data`; free immediately.
            // SAFETY: `state_ptr` was just allocated via `Box::into_raw` above and was not retained.
            drop(unsafe { Box::from_raw(state_ptr) });
        }
        status
    }

    /// Registers a multi-query Rust system closure (up to [`MAX_GUEST_MULTI_QUERIES`] query groups)
    /// with automatic [`GuestChunkIter::reset`] support and zero per-frame heap allocations.
    pub fn add_multi_query_system<F>(
        &mut self,
        name: &CStr,
        query_groups: &[&[ComponentAccessDesc]],
        system: F,
    ) -> FfiResult
    where
        F: FnMut(&mut MultiQueryContext<'_>) -> FfiResult + Send + 'static,
    {
        if query_groups.len() > MAX_GUEST_MULTI_QUERIES {
            return FfiResult::INVALID_COMPONENT;
        }
        let Some(register_mq) = self.register_multi_query else {
            return FfiResult::NULL_POINTER;
        };

        let mut raw_specs = [QuerySpec {
            queries: core::ptr::null(),
            query_count: 0,
        }; MAX_GUEST_MULTI_QUERIES];
        for (slot, group) in raw_specs.iter_mut().zip(query_groups.iter()) {
            *slot = QuerySpec {
                queries: if group.is_empty() {
                    core::ptr::null()
                } else {
                    group.as_ptr()
                },
                query_count: group.len(),
            };
        }

        let state = Box::new(MultiQueryClosureState {
            host_api: self.host_api as *const HostApi,
            cmd_vtable: self.cmd_vtable,
            iter_reset: self.iter_reset,
            callback: Mutex::new(system),
        });
        let state_ptr = Box::into_raw(state);

        let desc = MultiQuerySystemDescriptor {
            struct_size: core::mem::size_of::<MultiQuerySystemDescriptor>() as u32,
            name: name.as_ptr(),
            query_specs: if query_groups.is_empty() {
                core::ptr::null()
            } else {
                raw_specs.as_ptr()
            },
            query_spec_count: query_groups.len(),
            callback: c_multi_query_closure_trampoline::<F>,
            user_data: state_ptr as *mut c_void,
        };

        // SAFETY: `self.host_context` and `desc` (backed by stack `raw_specs`) remain valid for the call.
        let status = unsafe { register_mq(self.host_context, &desc) };
        if status.is_ok() {
            self.staged_allocations.push(ErasedAllocation {
                ptr: state_ptr as usize,
                drop_fn: drop_boxed_allocation::<MultiQueryClosureState<F>>,
            });
        } else {
            // SAFETY: `state_ptr` was just allocated above and was not retained by the host.
            drop(unsafe { Box::from_raw(state_ptr) });
        }
        status
    }

    /// Commits all staged allocations from this initialization session to the guest lifecycle
    /// registry so they remain alive until `bevy_abi_mod_shutdown`.
    pub fn commit(mut self) {
        if self.staged_allocations.is_empty() {
            return;
        }
        let mut guard = GUEST_ALLOCATIONS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.append(&mut self.staged_allocations);
    }
}

impl Drop for GuestApp<'_> {
    fn drop(&mut self) {
        if !self.staged_allocations.is_empty() {
            let aborted = core::mem::take(&mut self.staged_allocations);
            drop_allocation_list_lifo(aborted);
        }
    }
}

/// Executes a [`GuestApp`] setup function inside [`catch_guest_panic`], committing allocations
/// on [`FfiResult::OK`] or automatically rolling them back on error/panic.
///
/// # Safety
/// Must be called with valid handshake arguments supplied to `bevy_abi_mod_init`.
pub unsafe fn run_guest_app_init<F>(
    get_proc_address: GetProcAddressFn,
    host_api: *const HostApi,
    host_context: *mut c_void,
    build_fn: F,
) -> FfiResult
where
    F: FnOnce(&mut GuestApp<'_>) -> FfiResult,
{
    catch_guest_panic(|| {
        // SAFETY: Forwarding C-ABI `bevy_abi_mod_init` handshake arguments validated by `from_raw`.
        let mut app = match unsafe { GuestApp::from_raw(get_proc_address, host_api, host_context) }
        {
            Ok(app) => app,
            Err(err) => return err,
        };
        let status = build_fn(&mut app);
        if status.is_ok() {
            app.commit();
        }
        status
    })
}

/// Convenient prelude module for guest mod authors.
pub mod prelude {
    pub use crate::{
        catch_guest_panic, cleanup_registered_guest_state, export_mod, register_runtime_component,
        resolve_iter_reset, resolve_register_multi_query, run_guest_app_init,
        wrap_multi_query_callback, wrap_system_callback, Chunk, ChunkSlice, ChunkSliceMut,
        CommandBridgeVTable, GuestApp, GuestChunkIter, GuestCommandQueue, GuestComponent,
        GuestPlugin, MultiQueryContext, SystemContext, MAX_GUEST_MULTI_QUERIES,
    };
    pub use abi_contract::{
        ComponentAccessDesc, ComponentAccessMode, ComponentDropFn, ComponentUuid,
        DynamicComponentDescriptor, DynamicSystemCallback, ExposableComponent, FfiEntity,
        FfiResult, FfiSlice, GetProcAddressFn, HostApi, IterResetFn, ModComponentDescriptor,
        MultiQuerySystemCallback, MultiQuerySystemDescriptor, QuerySpec, RawChunkIter,
        RawCommandQueue, RegisterComponentFn, RegisterMultiQuerySystemFn, RegisterSystemFn,
        StrView, SymbolEntry, SystemDescriptor, ABI_VERSION_MAJOR, ABI_VERSION_MINOR,
        ABI_VERSION_PACKED, ABI_VERSION_PATCH,
    };
}

#[cfg(test)]
mod tests {
    use super::*;

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

        // Arithmetic overflow and address wrapping protection in get_ptr
        let massive_slice = ChunkSlice::new(data.as_ptr() as *const u8, stride, usize::MAX);
        assert!(massive_slice.get_ptr(usize::MAX / 2).is_none());

        let mut slice_mut = ChunkSliceMut::new(data.as_mut_ptr() as *mut u8, stride, count);
        let p1 = slice_mut.get_ptr_mut(1).unwrap();
        // SAFETY: p1 is valid mutable u32 pointer.
        unsafe { *(p1 as *mut u32) = 99 };
        assert_eq!(data[1], 99);
        assert!(slice_mut.get_ptr_mut(4).is_none());

        // Arithmetic overflow and address wrapping protection in get_ptr_mut
        let mut massive_slice_mut =
            ChunkSliceMut::new(data.as_mut_ptr() as *mut u8, stride, usize::MAX);
        assert!(massive_slice_mut.get_ptr_mut(usize::MAX / 2).is_none());
    }

    #[derive(Copy, Clone, Debug, PartialEq)]
    #[repr(C)]
    struct PosData {
        x: f32,
        y: f32,
    }

    #[test]
    fn test_chunk_slice_as_typed_slice() {
        let data = [PosData { x: 1.0, y: 2.0 }, PosData { x: 3.0, y: 4.0 }];
        let stride = core::mem::size_of::<PosData>();
        let slice = ChunkSlice::new(data.as_ptr() as *const u8, stride, 2);

        let typed = slice
            .as_typed_slice::<PosData>()
            .expect("valid typed slice");
        assert_eq!(typed.len(), 2);
        assert_eq!(typed[0], PosData { x: 1.0, y: 2.0 });
        assert_eq!(typed[1], PosData { x: 3.0, y: 4.0 });

        // Stride mismatch
        let wrong_stride = ChunkSlice::new(data.as_ptr() as *const u8, stride + 4, 2);
        assert!(wrong_stride.as_typed_slice::<PosData>().is_none());

        // Misaligned pointer
        let misaligned_ptr = unsafe { (data.as_ptr() as *const u8).add(1) };
        let misaligned = ChunkSlice::new(misaligned_ptr, stride, 1);
        assert!(misaligned.as_typed_slice::<PosData>().is_none());

        // Empty slice with valid stride
        let empty = ChunkSlice::new(core::ptr::null(), stride, 0);
        assert_eq!(empty.as_typed_slice::<PosData>(), Some(&[][..]));

        // Empty slice with invalid stride
        let empty_bad_stride = ChunkSlice::new(core::ptr::null(), stride + 1, 0);
        assert!(empty_bad_stride.as_typed_slice::<PosData>().is_none());

        // Massive count check (exceeding isize::MAX)
        let massive = ChunkSlice::new(data.as_ptr() as *const u8, stride, usize::MAX);
        assert!(massive.as_typed_slice::<PosData>().is_none());
    }

    #[test]
    fn test_chunk_slice_mut_as_typed_slice() {
        let mut data = [PosData { x: 1.0, y: 2.0 }, PosData { x: 3.0, y: 4.0 }];
        let stride = core::mem::size_of::<PosData>();
        let mut slice_mut = ChunkSliceMut::new(data.as_mut_ptr() as *mut u8, stride, 2);

        {
            let typed_mut = slice_mut
                .as_typed_slice::<PosData>()
                .expect("valid typed slice mut");
            typed_mut[0].x = 10.0;
            typed_mut[1].y = 40.0;
        }

        assert_eq!(data[0], PosData { x: 10.0, y: 2.0 });
        assert_eq!(data[1], PosData { x: 3.0, y: 40.0 });

        // Stride mismatch
        let mut bad_stride_slice = ChunkSliceMut::new(data.as_mut_ptr() as *mut u8, stride + 4, 2);
        assert!(bad_stride_slice.as_typed_slice::<PosData>().is_none());

        // Empty slice with valid stride
        let mut empty = ChunkSliceMut::new(core::ptr::null_mut(), stride, 0);
        assert_eq!(empty.as_typed_slice::<PosData>(), Some(&mut [][..]));

        // Massive count check (exceeding isize::MAX)
        let mut massive_mut = ChunkSliceMut::new(data.as_mut_ptr() as *mut u8, stride, usize::MAX);
        assert!(massive_mut.as_typed_slice::<PosData>().is_none());
    }

    #[test]
    fn test_guest_command_queue_insert_runtime_component() {
        static mut CAPTURED_LEN: usize = 0;
        static mut CAPTURED_ENTITY: u64 = 0;

        unsafe extern "C" fn mock_insert(
            _queue: *mut RawCommandQueue,
            entity: FfiEntity,
            _name: *const c_char,
            _data: *const u8,
            len: usize,
        ) -> FfiResult {
            unsafe {
                CAPTURED_LEN = len;
                CAPTURED_ENTITY = entity.0;
            }
            FfiResult::OK
        }

        let mut queue = GuestCommandQueue::new(
            0x1234 as *mut RawCommandQueue,
            CommandBridgeVTable {
                spawn_empty: None,
                despawn: None,
                insert_component: Some(mock_insert),
            },
        );

        let pos = PosData { x: 5.0, y: 7.0 };
        let res = queue.insert_runtime_component(FfiEntity(42), c"PosData", &pos);
        assert_eq!(res, FfiResult::OK);
        assert_eq!(unsafe { CAPTURED_LEN }, core::mem::size_of::<PosData>());
        assert_eq!(unsafe { CAPTURED_ENTITY }, 42);
    }

    #[test]
    fn test_register_runtime_component_mock() {
        static CAPTURED_NAME: Mutex<Option<std::string::String>> = Mutex::new(None);
        static CAPTURED_SIZE: std::sync::atomic::AtomicUsize =
            std::sync::atomic::AtomicUsize::new(0);
        static CAPTURED_ALIGN: std::sync::atomic::AtomicUsize =
            std::sync::atomic::AtomicUsize::new(0);
        static CAPTURED_DROP: std::sync::atomic::AtomicBool =
            std::sync::atomic::AtomicBool::new(false);

        unsafe extern "C" fn mock_register_component(
            _ctx: *mut c_void,
            desc: *const ModComponentDescriptor,
        ) -> FfiResult {
            assert!(!desc.is_null());
            let d = unsafe { &*desc };
            *CAPTURED_NAME.lock().unwrap() = Some(
                unsafe { CStr::from_ptr(d.name) }
                    .to_str()
                    .unwrap()
                    .to_string(),
            );
            CAPTURED_SIZE.store(d.size, std::sync::atomic::Ordering::SeqCst);
            CAPTURED_ALIGN.store(d.align, std::sync::atomic::Ordering::SeqCst);
            CAPTURED_DROP.store(d.drop_fn.is_some(), std::sync::atomic::Ordering::SeqCst);
            FfiResult::OK
        }

        unsafe extern "C" fn stub_register_system(
            _: *mut c_void,
            _: *const abi_contract::SystemDescriptor,
        ) -> FfiResult {
            FfiResult::OK
        }
        unsafe extern "C" fn stub_iter_next_chunk(_: *mut RawChunkIter) -> usize {
            0
        }
        unsafe extern "C" fn stub_iter_column_readonly(
            _: *const RawChunkIter,
            _: usize,
        ) -> *const u8 {
            core::ptr::null()
        }
        unsafe extern "C" fn stub_iter_column_mutable(
            _: *mut RawChunkIter,
            _: usize,
        ) -> *mut u8 {
            core::ptr::null_mut()
        }
        unsafe extern "C" fn stub_iter_mark_changed(
            _: *mut RawChunkIter,
            _: usize,
            _: usize,
        ) -> FfiResult {
            FfiResult::OK
        }
        unsafe extern "C" fn stub_iter_entities(
            _: *const RawChunkIter,
        ) -> *const abi_contract::FfiEntity {
            core::ptr::null()
        }
        unsafe extern "C" fn stub_iter_column_stride(_: *const RawChunkIter, _: usize) -> usize {
            0
        }

        let dummy_host_api = HostApi {
            struct_size: core::mem::size_of::<HostApi>() as u32,
            abi_version: abi_contract::ABI_VERSION_PACKED,
            register_system: stub_register_system,
            iter_next_chunk: stub_iter_next_chunk,
            iter_column_readonly: stub_iter_column_readonly,
            iter_column_mutable: stub_iter_column_mutable,
            iter_mark_changed: stub_iter_mark_changed,
            iter_entities: stub_iter_entities,
            iter_column_stride: stub_iter_column_stride,
            register_component: mock_register_component,
        };

        let dummy_ctx = 0x1234usize as *mut c_void;

        // 1. POD component -> drop_fn == None
        let res = unsafe {
            register_runtime_component::<PosData>(&dummy_host_api, dummy_ctx, c"PosData")
        };
        assert_eq!(res, FfiResult::OK);
        assert_eq!(CAPTURED_NAME.lock().unwrap().as_deref(), Some("PosData"));
        assert_eq!(
            CAPTURED_SIZE.load(std::sync::atomic::Ordering::SeqCst),
            core::mem::size_of::<PosData>()
        );
        assert_eq!(
            CAPTURED_ALIGN.load(std::sync::atomic::Ordering::SeqCst),
            core::mem::align_of::<PosData>()
        );
        assert!(!CAPTURED_DROP.load(std::sync::atomic::Ordering::SeqCst));

        // 2. Non-POD component -> drop_fn != None
        #[allow(dead_code)]
        struct HasDrop(std::string::String);
        let res_drop = unsafe {
            register_runtime_component::<HasDrop>(&dummy_host_api, dummy_ctx, c"HasDrop")
        };
        assert_eq!(res_drop, FfiResult::OK);
        assert_eq!(CAPTURED_NAME.lock().unwrap().as_deref(), Some("HasDrop"));
        assert_eq!(
            CAPTURED_SIZE.load(std::sync::atomic::Ordering::SeqCst),
            core::mem::size_of::<HasDrop>()
        );
        assert_eq!(
            CAPTURED_ALIGN.load(std::sync::atomic::Ordering::SeqCst),
            core::mem::align_of::<HasDrop>()
        );
        assert!(CAPTURED_DROP.load(std::sync::atomic::Ordering::SeqCst));

        // 3. Null pointer checks
        assert_eq!(
            unsafe {
                register_runtime_component::<PosData>(core::ptr::null(), dummy_ctx, c"PosData")
            },
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            unsafe {
                register_runtime_component::<PosData>(
                    &dummy_host_api,
                    core::ptr::null_mut(),
                    c"PosData",
                )
            },
            FfiResult::NULL_POINTER
        );
    }
}
