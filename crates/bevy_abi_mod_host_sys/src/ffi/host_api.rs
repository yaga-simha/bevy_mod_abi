//! Static 27-slot `HOST_API: BamHostApi` function table passed to `bevy_abi_mod_init` (`C-03`).

use crate::ffi::commands::{
    host_cmd_despawn, host_cmd_insert, host_cmd_insert_default, host_cmd_remove, host_cmd_spawn,
    host_cmd_spawn_empty,
};
use crate::ffi::iter::{
    host_iter_column_info, host_iter_column_mutable, host_iter_column_mutable_untracked,
    host_iter_column_readonly, host_iter_column_rows_mutable, host_iter_column_rows_readonly,
    host_iter_entities, host_iter_mark_changed_range, host_iter_next_chunk, host_iter_reset,
    host_res_mut_ptr, host_res_ptr,
};
use crate::ffi::proc_address::host_get_proc_address;
use crate::ffi::register::{
    host_load_host_ctx, host_register_component, host_register_system, host_resolve_component,
};
use crate::handle_header::host_last_error;
use crate::system::{host_sys_commands, host_sys_host_ctx, host_sys_query};
use bevy_abi_mod_sys::{ABI_VERSION, BamHostApi};

/// Global immutable 27-slot C-ABI v0.2 host function table (`C-03`).
pub static HOST_API: BamHostApi = BamHostApi {
    struct_size: core::mem::size_of::<BamHostApi>() as u32,
    abi_version: ABI_VERSION,

    // Diagnostics & custom symbols
    get_proc_address: Some(host_get_proc_address),
    last_error: Some(host_last_error),

    // Registration & handle resolution
    load_host_ctx: Some(host_load_host_ctx),
    register_component: Some(host_register_component),
    resolve_component: Some(host_resolve_component),
    register_system: Some(host_register_system),

    // System invocation context
    sys_host_ctx: Some(host_sys_host_ctx),
    sys_query: Some(host_sys_query),
    sys_commands: Some(host_sys_commands),

    // Chunk & sparse-row iteration
    iter_next_chunk: Some(host_iter_next_chunk),
    iter_reset: Some(host_iter_reset),
    iter_entities: Some(host_iter_entities),
    iter_column_readonly: Some(host_iter_column_readonly),
    iter_column_mutable: Some(host_iter_column_mutable),
    iter_column_mutable_untracked: Some(host_iter_column_mutable_untracked),
    iter_mark_changed_range: Some(host_iter_mark_changed_range),
    iter_column_rows_readonly: Some(host_iter_column_rows_readonly),
    iter_column_rows_mutable: Some(host_iter_column_rows_mutable),
    iter_column_info: Some(host_iter_column_info),

    // Declaration-indexed singleton resources
    res_ptr: Some(host_res_ptr),
    res_mut_ptr: Some(host_res_mut_ptr),

    // Handle-based & batched deferred commands
    cmd_spawn_empty: Some(host_cmd_spawn_empty),
    cmd_spawn: Some(host_cmd_spawn),
    cmd_despawn: Some(host_cmd_despawn),
    cmd_insert: Some(host_cmd_insert),
    cmd_insert_default: Some(host_cmd_insert_default),
    cmd_remove: Some(host_cmd_remove),
};

const _: () = assert!(HOST_API.has_all_v0_2_slots());
