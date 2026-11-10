#![no_std]

//! # `abi_contract`
//!
//! Zero-dependency `#![no_std]` C-ABI contract definitions for `bevy_abi_mod`.
//! All types are `#[repr(C)]` or primitive equivalents to ensure zero shared Rust ABI assumptions.

use core::ffi::c_void;

pub const ABI_VERSION_MAJOR: u32 = 0;
pub const ABI_VERSION_MINOR: u32 = 1;
pub const ABI_VERSION_PATCH: u32 = 0;

/// Standard status return codes for cross-ABI calls.
#[repr(i32)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum ModStatus {
    Success = 0,
    ErrInvalidVersion = -1,
    ErrNullPointer = -2,
    ErrComponentNotFound = -3,
    ErrAccessConflict = -4,
    ErrInternal = -99,
}

/// Opaque string view across the C ABI boundary.
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct StrView {
    pub ptr: *const u8,
    pub len: usize,
}

impl StrView {
    pub const fn from_str(s: &'static str) -> Self {
        Self {
            ptr: s.as_ptr(),
            len: s.len(),
        }
    }
}

/// Unique 128-bit component identifier (UUID-compatible).
#[repr(C)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub struct ComponentUuid {
    pub bytes: [u8; 16],
}

/// Entity identifier representation (e.g., Bevy Entity: index + generation).
#[repr(C)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub struct EntityId {
    pub index: u32,
    pub generation: u32,
}

/// Access mode declaration for an ECS system.
#[repr(u32)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum ComponentAccessMode {
    Read = 0,
    Write = 1,
}

/// Single component access requirement.
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct ComponentAccessRequirement {
    pub component_uuid: ComponentUuid,
    pub mode: ComponentAccessMode,
}

/// System access descriptor passed during system registration.
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct SystemAccessDescriptor {
    pub name: StrView,
    pub requirements: *const ComponentAccessRequirement,
    pub requirements_count: usize,
}

/// Flecs-style contiguous table chunk iterator.
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct EcsChunkIter {
    /// Number of matching entities in this contiguous chunk.
    pub count: usize,
    /// Pointer to contiguous array of EntityId (length = count).
    pub entities: *const EntityId,
    /// Array of column base pointers for requested components.
    pub column_ptrs: *const *mut c_void,
    /// Byte strides between successive elements in each column.
    pub column_strides: *const usize,
    /// Number of columns requested.
    pub column_count: usize,
}

/// System execution callback type provided by guest mods.
pub type SystemRunCallback = extern "C" fn(user_data: *mut c_void, chunk: *mut EcsChunkIter);

/// System registration function signature.
pub type RegisterSystemFn = extern "C" fn(
    host_context: *mut c_void,
    descriptor: *const SystemAccessDescriptor,
    callback: SystemRunCallback,
    user_data: *mut c_void,
) -> ModStatus;

/// Symbol-resolver function pointer passed to the guest mod entry point (Godot GDExtension style).
pub type GetProcAddressFn = extern "C" fn(proc_name: *const core::ffi::c_char) -> *const c_void;

/// Host API VTable populated or queried at handshake time.
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct HostApi {
    pub register_system: RegisterSystemFn,
}

/// Mod initialization entry point function signature.
pub type ModInitFn = extern "C" fn(
    get_proc_address: GetProcAddressFn,
    host_api: *const HostApi,
    host_context: *mut c_void,
) -> ModStatus;

/// Mod teardown function signature.
pub type ModShutdownFn = extern "C" fn(host_context: *mut c_void) -> ModStatus;
