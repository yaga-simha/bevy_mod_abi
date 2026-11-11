//! Centralized C-ABI symbol resolution, bridge VTables, and `HostApi` handshake validation.

use core::ffi::{CStr, c_void};

use bevy_abi_mod_sys::{
    ABI_VERSION_PACKED, CmdDespawnFn, CmdInsertComponentFn, CmdInsertDefaultFn,
    CmdRemoveComponentFn, CmdSpawnEmptyFn, FfiResult, GetProcAddressFn, HostApi,
    IterColumnStrideFn, IterResetFn, IterResourceMutableFn, IterResourceReadonlyFn,
    ModComponentDescriptor, RegisterComponentFn, RegisterConditionalMultiQuerySystemFn,
    RegisterConditionalSystemFn, RegisterMultiQuerySystemFn, abi_version_is_compatible,
};

/// Minimum `HostApi::struct_size` required by `GuestApp` for system registration and chunk iteration.
///
/// Covers every core slot up to and including `iter_column_stride` so guests remain compatible
/// with hosts exposing only the core iteration prefix, checking trailing slots on demand.
pub const MIN_GUEST_APP_HOST_API_SIZE: usize =
    core::mem::offset_of!(HostApi, iter_column_stride) + core::mem::size_of::<IterColumnStrideFn>();

/// Minimum `HostApi::struct_size` required when calling `HostApi::register_component`.
pub const MIN_REGISTER_COMPONENT_HOST_API_SIZE: usize =
    core::mem::offset_of!(HostApi, register_component)
        + core::mem::size_of::<RegisterComponentFn>();

unsafe extern "C" fn unsupported_register_component(
    _host_context: *mut c_void,
    _desc: *const ModComponentDescriptor,
) -> FfiResult {
    FfiResult::INVALID_VERSION
}

/// Resolves a raw symbol pointer from `get_proc_address` using a validated [`CStr`].
///
/// # Safety
/// `get_proc_address` must be a valid host symbol-resolver function pointer following the
/// [`GetProcAddressFn`] C-ABI contract.
#[inline]
pub unsafe fn resolve_symbol_cstr(
    get_proc_address: GetProcAddressFn,
    symbol_name: &CStr,
) -> *const c_void {
    // SAFETY: Caller guarantees `get_proc_address` is a valid host symbol resolver, and
    // `symbol_name` is a nul-terminated C string.
    unsafe { get_proc_address(symbol_name.as_ptr()) }
}

/// Resolves a typed C-ABI function pointer `F` from `gpa` under `name`.
///
/// Enforces at compile time that `F` has pointer size (`size_of::<*const c_void>()`).
///
/// # Safety
/// - `gpa` must be a valid host symbol-resolver function pointer.
/// - If `gpa` returns a non-null address for `name`, that address must point to an `extern "C"`
///   function matching the exact signature and calling convention of `F`.
#[inline]
pub unsafe fn resolve_fn<F: Copy>(gpa: GetProcAddressFn, name: &CStr) -> Option<F> {
    const {
        assert!(
            core::mem::size_of::<F>() == core::mem::size_of::<*const c_void>(),
            "resolved FFI function pointer type must equal pointer size"
        );
    }
    // SAFETY: Caller guarantees `gpa` is a valid host symbol resolver.
    let ptr = unsafe { resolve_symbol_cstr(gpa, name) };
    if ptr.is_null() {
        None
    } else {
        // SAFETY: Compile-time assertion guarantees `size_of::<F>() == size_of::<*const c_void>()`,
        // `ptr` is non-null, and caller guarantees `name` resolves to a valid function of type `F`.
        Some(unsafe { core::mem::transmute_copy::<*const c_void, F>(&ptr) })
    }
}

#[inline]
unsafe fn resolve_fn_aliases<F: Copy>(gpa: GetProcAddressFn, aliases: &[&CStr]) -> Option<F> {
    for &alias in aliases {
        // SAFETY: Forwarding caller's `gpa` and signature guarantee for each candidate symbol name.
        if let Some(resolved) = unsafe { resolve_fn::<F>(gpa, alias) } {
            return Some(resolved);
        }
    }
    None
}

/// Resolves a typed static component VTable reference (for example `c"Position_vtable"`).
///
/// # Safety
/// - `get_proc_address` must be a valid host symbol-resolver function pointer.
/// - Caller must ensure `V` matches the exact `#[repr(C)]` VTable layout exported by the host
///   under `symbol_name` and outlives `'a`.
#[inline]
pub unsafe fn resolve_vtable_raw<'a, V: Copy + 'static>(
    get_proc_address: GetProcAddressFn,
    symbol_name: &CStr,
) -> Option<&'a V> {
    // SAFETY: Caller guarantees `get_proc_address` is valid.
    let ptr = unsafe { resolve_symbol_cstr(get_proc_address, symbol_name) };
    if ptr.is_null() || !(ptr as usize).is_multiple_of(core::mem::align_of::<V>()) {
        None
    } else {
        // SAFETY: Caller guarantees `symbol_name` points to a valid static `V` outliving `'a`.
        Some(unsafe { &*ptr.cast::<V>() })
    }
}

/// Validates raw C-ABI `bevy_abi_mod_init` handshake pointers, `struct_size` prefix coverage,
/// and semver ABI compatibility, returning a validated copy of [`HostApi`].
///
/// Checks only the minimum offset of the core slots used by `GuestApp`
/// ([`MIN_GUEST_APP_HOST_API_SIZE`]) rather than `size_of::<HostApi>()`. If `register_component`
/// lies beyond `struct_size`, it is populated with a stub returning [`FfiResult::INVALID_VERSION`].
///
/// # Safety
/// If `host_api` is non-null, it must be aligned for [`HostApi`] and point to at least 4 readable
/// bytes (`struct_size`) and, when `struct_size >= MIN_GUEST_APP_HOST_API_SIZE`, at least
/// `struct_size` readable initialized bytes of a [`HostApi`] prefix.
pub unsafe fn validate_host_api(
    host_api: *const HostApi,
    host_context: *mut c_void,
) -> Result<HostApi, FfiResult> {
    if host_api.is_null() || host_context.is_null() {
        return Err(FfiResult::NULL_POINTER);
    }
    if !(host_api as usize).is_multiple_of(core::mem::align_of::<HostApi>()) {
        return Err(FfiResult::INVALID_VERSION);
    }

    // SAFETY: `host_api` is non-null and aligned; reading the 4-byte `struct_size` header at offset 0.
    let raw_struct_size = unsafe { *host_api.cast::<u32>() };
    let struct_size = raw_struct_size as usize;
    if struct_size < MIN_GUEST_APP_HOST_API_SIZE {
        return Err(FfiResult::INVALID_VERSION);
    }

    // SAFETY: `struct_size >= MIN_GUEST_APP_HOST_API_SIZE` covers `abi_version` through `iter_column_stride`.
    let abi_version = unsafe { core::ptr::addr_of!((*host_api).abi_version).read() };
    if !abi_version_is_compatible(abi_version, ABI_VERSION_PACKED) {
        return Err(FfiResult::INVALID_VERSION);
    }

    // SAFETY: All core slots below lie within `0..MIN_GUEST_APP_HOST_API_SIZE <= struct_size`.
    let (
        register_system,
        iter_next_chunk,
        iter_column_readonly,
        iter_column_mutable,
        iter_mark_changed,
        iter_entities,
        iter_column_stride,
    ) = unsafe {
        (
            core::ptr::addr_of!((*host_api).register_system).read(),
            core::ptr::addr_of!((*host_api).iter_next_chunk).read(),
            core::ptr::addr_of!((*host_api).iter_column_readonly).read(),
            core::ptr::addr_of!((*host_api).iter_column_mutable).read(),
            core::ptr::addr_of!((*host_api).iter_mark_changed).read(),
            core::ptr::addr_of!((*host_api).iter_entities).read(),
            core::ptr::addr_of!((*host_api).iter_column_stride).read(),
        )
    };

    let register_component = if struct_size >= MIN_REGISTER_COMPONENT_HOST_API_SIZE {
        // SAFETY: `struct_size` covers the `register_component` field.
        unsafe { core::ptr::addr_of!((*host_api).register_component).read() }
    } else {
        unsupported_register_component
    };

    Ok(HostApi {
        struct_size: raw_struct_size,
        abi_version,
        register_system,
        iter_next_chunk,
        iter_column_readonly,
        iter_column_mutable,
        iter_mark_changed,
        iter_entities,
        iter_column_stride,
        register_component,
    })
}

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
    /// # Safety
    /// `get_proc_address` must be a valid host symbol-resolver function pointer whose command
    /// bridge symbols match the expected C-ABI function signatures.
    pub unsafe fn resolve(get_proc_address: GetProcAddressFn) -> Self {
        // SAFETY: Forwarding caller's `get_proc_address` contract to `resolve_fn_aliases`.
        unsafe {
            Self {
                spawn_empty: resolve_fn_aliases(
                    get_proc_address,
                    &[c"cmd_spawn_empty", c"host_cmd_spawn_empty"],
                ),
                despawn: resolve_fn_aliases(
                    get_proc_address,
                    &[c"cmd_despawn", c"host_cmd_despawn"],
                ),
                insert_component: resolve_fn_aliases(
                    get_proc_address,
                    &[c"cmd_insert_component", c"host_cmd_insert_component"],
                ),
            }
        }
    }
}

/// Extended cached function pointers for host command queue operations, including
/// `cmd_remove_component` and `cmd_insert_default` while preserving literal compatibility for
/// [`CommandBridgeVTable`].
#[derive(Clone, Copy, Default, Debug)]
pub struct CommandBridgeVTableExt {
    /// Core command pointers (`spawn_empty`, `despawn`, `insert_component`).
    pub base: CommandBridgeVTable,
    /// Function pointer for `cmd_remove_component`.
    pub remove_component: Option<CmdRemoveComponentFn>,
    /// Function pointer for `cmd_insert_default`.
    pub insert_default: Option<CmdInsertDefaultFn>,
}

impl CommandBridgeVTableExt {
    /// Resolves all command bridge functions (including `cmd_remove_component` and `cmd_insert_default`)
    /// from the host's symbol provider.
    ///
    /// # Safety
    /// `get_proc_address` must be a valid host symbol-resolver function pointer whose command
    /// bridge symbols match the expected C-ABI function signatures.
    pub unsafe fn resolve(get_proc_address: GetProcAddressFn) -> Self {
        // SAFETY: Forwarding caller's `get_proc_address` contract to `CommandBridgeVTable::resolve`
        // and `resolve_fn_aliases`.
        unsafe {
            Self {
                base: CommandBridgeVTable::resolve(get_proc_address),
                remove_component: resolve_fn_aliases(
                    get_proc_address,
                    &[c"cmd_remove_component", c"host_cmd_remove_component"],
                ),
                insert_default: resolve_fn_aliases(
                    get_proc_address,
                    &[c"cmd_insert_default", c"host_cmd_insert_default"],
                ),
            }
        }
    }
}

/// Cached function pointers for host ECS resource read/write operations (`Res<R>` / `ResMut<R>`).
#[derive(Clone, Copy, Default, Debug)]
pub struct ResourceBridgeVTable {
    /// Function pointer for `res_readonly` (`iter_resource_readonly`).
    pub res_readonly: Option<IterResourceReadonlyFn>,
    /// Function pointer for `res_mutable` (`iter_resource_mutable`).
    pub res_mutable: Option<IterResourceMutableFn>,
}

impl ResourceBridgeVTable {
    /// Resolves resource access functions (`res_readonly`, `res_mutable`) from `get_proc_address`.
    ///
    /// # Safety
    /// `get_proc_address` must be a valid host symbol-resolver function pointer whose resource
    /// bridge symbols match the expected C-ABI function signatures.
    pub unsafe fn resolve(get_proc_address: GetProcAddressFn) -> Self {
        // SAFETY: Forwarding caller's `get_proc_address` contract to `resolve_fn_aliases`.
        unsafe {
            Self {
                res_readonly: resolve_fn_aliases(
                    get_proc_address,
                    &[
                        c"res_readonly",
                        c"iter_resource_readonly",
                        c"host_iter_resource_readonly",
                    ],
                ),
                res_mutable: resolve_fn_aliases(
                    get_proc_address,
                    &[
                        c"res_mutable",
                        c"iter_resource_mutable",
                        c"host_iter_resource_mutable",
                    ],
                ),
            }
        }
    }
}

/// Resolves the `iter_reset` host function pointer via `get_proc_address`, if supported by the host.
///
/// # Safety
/// `get_proc_address` must be a valid host symbol-resolver function pointer whose `iter_reset`
/// symbol matches [`IterResetFn`].
#[inline]
pub unsafe fn resolve_iter_reset(get_proc_address: GetProcAddressFn) -> Option<IterResetFn> {
    // SAFETY: Forwarding caller's `get_proc_address` contract.
    unsafe { resolve_fn_aliases(get_proc_address, &[c"iter_reset", c"host_iter_reset"]) }
}

/// Resolves `register_multi_query_system` via `get_proc_address`, if supported by the host.
///
/// # Safety
/// `get_proc_address` must be a valid host symbol-resolver function pointer whose
/// `register_multi_query_system` symbol matches [`RegisterMultiQuerySystemFn`].
#[inline]
pub unsafe fn resolve_register_multi_query(
    get_proc_address: GetProcAddressFn,
) -> Option<RegisterMultiQuerySystemFn> {
    // SAFETY: Forwarding caller's `get_proc_address` contract.
    unsafe {
        resolve_fn_aliases(
            get_proc_address,
            &[
                c"register_multi_query_system",
                c"host_register_multi_query_system",
            ],
        )
    }
}

/// Resolves `register_conditional_system` via `get_proc_address`, if supported by the host.
///
/// # Safety
/// `get_proc_address` must be a valid host symbol-resolver function pointer whose
/// `register_conditional_system` symbol matches [`RegisterConditionalSystemFn`].
#[inline]
pub unsafe fn resolve_register_conditional_system(
    get_proc_address: GetProcAddressFn,
) -> Option<RegisterConditionalSystemFn> {
    // SAFETY: Forwarding caller's `get_proc_address` contract.
    unsafe {
        resolve_fn_aliases(
            get_proc_address,
            &[
                c"register_conditional_system",
                c"host_register_conditional_system",
            ],
        )
    }
}

/// Resolves `register_conditional_multi_query_system` via `get_proc_address`, if supported by the host.
///
/// # Safety
/// `get_proc_address` must be a valid host symbol-resolver function pointer whose
/// `register_conditional_multi_query_system` symbol matches [`RegisterConditionalMultiQuerySystemFn`].
#[inline]
pub unsafe fn resolve_register_conditional_multi_query(
    get_proc_address: GetProcAddressFn,
) -> Option<RegisterConditionalMultiQuerySystemFn> {
    // SAFETY: Forwarding caller's `get_proc_address` contract.
    unsafe {
        resolve_fn_aliases(
            get_proc_address,
            &[
                c"register_conditional_multi_query_system",
                c"host_register_conditional_multi_query_system",
            ],
        )
    }
}
