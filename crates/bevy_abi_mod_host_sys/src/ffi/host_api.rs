//! Static `HostApi` function table passed across the C-ABI handshake.

use crate::ffi::iter::{
    host_iter_column_mutable, host_iter_column_readonly, host_iter_column_stride,
    host_iter_entities, host_iter_mark_changed, host_iter_next_chunk,
};
use crate::ffi::register::{host_register_component, host_register_system};
use bevy_abi_mod_sys::{ABI_VERSION_PACKED, HostApi};
use core::mem::size_of;

/// Static C-ABI [`HostApi`] dispatch table provided to guest modules during `init`.
pub static HOST_API: HostApi = HostApi {
    struct_size: size_of::<HostApi>() as u32,
    abi_version: ABI_VERSION_PACKED,
    register_system: host_register_system,
    iter_next_chunk: host_iter_next_chunk,
    iter_column_readonly: host_iter_column_readonly,
    iter_column_mutable: host_iter_column_mutable,
    iter_mark_changed: host_iter_mark_changed,
    iter_entities: host_iter_entities,
    iter_column_stride: host_iter_column_stride,
    register_component: host_register_component,
};
