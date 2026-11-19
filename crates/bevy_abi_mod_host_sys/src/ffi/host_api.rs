//! Static 68-slot `HOST_API: BamHostApi` function table passed to `bevy_abi_mod_init`.

use crate::ffi::assets_scene::{
    host_asset_get_load_state, host_asset_load, host_asset_release, host_cmd_spawn_scene,
};
use crate::ffi::commands::{
    host_cmd_despawn, host_cmd_insert, host_cmd_insert_default, host_cmd_remove, host_cmd_spawn,
    host_cmd_spawn_empty, host_cmd_trigger,
};
use crate::ffi::commands_ext::{
    host_cmd_despawn_all, host_cmd_insert_if_new, host_cmd_spawn_batch, host_cmd_spawn_persistent,
};
use crate::ffi::hierarchy_ext::host_sys_entity_children;
use crate::ffi::iter::{
    host_iter_column_info, host_iter_column_mutable, host_iter_column_mutable_untracked,
    host_iter_column_readonly, host_iter_column_rows_mutable, host_iter_column_rows_readonly,
    host_iter_entities, host_iter_mark_changed_range, host_iter_next_chunk, host_iter_reset,
    host_msg_read_batch, host_msg_send, host_msg_write_batch, host_par_for_each_chunk,
    host_res_mut_ptr, host_res_ptr, host_res_ticks,
};
use crate::ffi::iter_ext::host_iter_read_removed;
use crate::ffi::observers_ext::{host_cmd_observe, host_cmd_trigger_ex};
use crate::ffi::proc_address::host_get_proc_address;
use crate::ffi::register::{
    host_load_host_ctx, host_register_component, host_register_observer, host_register_system,
    host_resolve_component,
};
use crate::ffi::v03_ext::{
    host_bind_named_object, host_cmd_checkpoint, host_cmd_insert_relation,
    host_cmd_remove_relation, host_cmd_rollback, host_component_layout_info,
    host_configure_schedule_set, host_intern_symbol, host_iter_count, host_iter_up_cascade,
    host_log_diagnostic, host_migrate_component_column, host_mount_system_in_schedule,
    host_par_for_each_chunk_readonly, host_register_relation, host_register_system_handle,
    host_report_guest_fault, host_resolve_component_by_uuid,
};
use crate::handle_header::host_last_error;
use crate::system::{
    host_sys_commands, host_sys_entity_get, host_sys_entity_get_mut, host_sys_host_ctx,
    host_sys_query, host_sys_res_mut_ptr, host_sys_res_ptr,
};
use bevy_abi_mod_sys::{ABI_VERSION, BamHostApi};

/// Global immutable 68-slot C-ABI v0.3 host function table.
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

    // Extended parallel iteration, entity lookups, and messaging slots
    par_for_each_chunk: Some(host_par_for_each_chunk),
    sys_entity_get: Some(host_sys_entity_get),
    sys_entity_get_mut: Some(host_sys_entity_get_mut),
    msg_read_batch: Some(host_msg_read_batch),
    msg_send: Some(host_msg_send),

    // Extended resources, batch messages, dynamic observers, and triggers
    msg_write_batch: Some(host_msg_write_batch),
    res_ticks: Some(host_res_ticks),
    sys_res_ptr: Some(host_sys_res_ptr),
    sys_res_mut_ptr: Some(host_sys_res_mut_ptr),
    register_observer: Some(host_register_observer),
    cmd_trigger: Some(host_cmd_trigger),

    // Extended entity commands, lifecycle observers, removed components, hierarchy, assets & scenes
    cmd_spawn_persistent: Some(host_cmd_spawn_persistent),
    cmd_spawn_batch: Some(host_cmd_spawn_batch),
    cmd_despawn_all: Some(host_cmd_despawn_all),
    cmd_insert_if_new: Some(host_cmd_insert_if_new),
    cmd_trigger_ex: Some(host_cmd_trigger_ex),
    cmd_observe: Some(host_cmd_observe),
    iter_read_removed: Some(host_iter_read_removed),
    sys_entity_children: Some(host_sys_entity_children),
    asset_load: Some(host_asset_load),
    asset_get_load_state: Some(host_asset_get_load_state),
    asset_release: Some(host_asset_release),
    cmd_spawn_scene: Some(host_cmd_spawn_scene),

    // ABI `0.3.0` extended diagnostics, schema/layout introspection, relations, scheduling & checkpoints
    log_diagnostic: Some(host_log_diagnostic),
    report_guest_fault: Some(host_report_guest_fault),
    resolve_component_by_uuid: Some(host_resolve_component_by_uuid),
    component_layout_info: Some(host_component_layout_info),
    register_relation: Some(host_register_relation),
    intern_symbol: Some(host_intern_symbol),
    bind_named_object: Some(host_bind_named_object),
    iter_count: Some(host_iter_count),
    iter_up_cascade: Some(host_iter_up_cascade),
    par_for_each_chunk_readonly: Some(host_par_for_each_chunk_readonly),
    register_system_handle: Some(host_register_system_handle),
    configure_schedule_set: Some(host_configure_schedule_set),
    mount_system_in_schedule: Some(host_mount_system_in_schedule),
    cmd_checkpoint: Some(host_cmd_checkpoint),
    cmd_rollback: Some(host_cmd_rollback),
    cmd_insert_relation: Some(host_cmd_insert_relation),
    cmd_remove_relation: Some(host_cmd_remove_relation),
    migrate_component_column: Some(host_migrate_component_column),
};

const _: () = assert!(HOST_API.has_all_v0_2_slots());
const _: () = assert!(HOST_API.has_all_v0_3_slots());
const _: () = assert!(HOST_API.has_all_slots());
