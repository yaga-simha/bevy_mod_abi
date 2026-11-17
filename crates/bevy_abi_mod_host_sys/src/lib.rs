#![warn(missing_docs)]
#![deny(
    improper_ctypes_definitions,
    clippy::undocumented_unsafe_blocks,
    clippy::missing_safety_doc
)]
//! Low-level `unsafe` FFI and Bevy ECS storage internals for `bevy_abi_mod_host` (`ABI 0.2.0`).
//!
//! Isolates all host-side `unsafe` code (`HostIter` chunk/sparse iteration over `UnsafeWorldCell`,
//! arena-backed `HostCommands`, `GuestSystem`, CoW `ModRegistry` handle/VTable/symbol lookup,
//! `ArcSwapOption` drop trampolines with two-condition `WorldDropGuard` reclamation, and
//! `libloading` shadow-copy shared-library lifecycle) behind audited invariants so
//! `bevy_abi_mod_host` and `bevy_abi_mod` enforce `#![forbid(unsafe_code)]`.

pub mod drop_trampoline;
pub mod ffi;
pub mod handle_header;
pub mod loader;
pub mod registry;
pub mod schedule;
pub mod sync_ptr;
pub mod system;

pub use drop_trampoline::{MAX_DROP_TRAMPOLINES, WorldDropGuard};
pub use ffi::commands::{
    CommandArena, HostCommands, HostCommandsStorage, host_cmd_despawn, host_cmd_insert,
    host_cmd_insert_default, host_cmd_remove, host_cmd_spawn, host_cmd_spawn_empty,
    host_cmd_trigger,
};
pub use ffi::host_api::HOST_API;
pub use ffi::iter::{
    HostIter, QueryPlan, QueryTermMeta, SparseRowScratch, TickFilterTerm, host_iter_column_info,
    host_iter_column_mutable, host_iter_column_mutable_untracked, host_iter_column_readonly,
    host_iter_column_rows_mutable, host_iter_column_rows_readonly, host_iter_entities,
    host_iter_mark_changed_range, host_iter_next_chunk, host_iter_reset, host_msg_read_batch,
    host_msg_send, host_msg_write_batch, host_par_for_each_chunk, host_res_mut_ptr, host_res_ptr,
    host_res_ticks,
};
pub use ffi::proc_address::host_get_proc_address;
pub use ffi::register::{
    GuestObserverBinding, LoadContext, host_load_host_ctx, host_register_component,
    host_register_observer, host_register_system, host_resolve_component,
};
pub use handle_header::{
    COMMANDS_MAGIC, HOST_CTX_MAGIC, HandleHeader, HostContext, ITER_MAGIC, LOAD_CTX_MAGIC,
    POISONED_MAGIC, STRICT_FFI_CHECKS, SYS_CTX_MAGIC, host_last_error,
};
pub use loader::{
    LoadOptions, LoadedMod, LoadedModInfo, ModHost, ModId, ModLoadError, ModLoaded, ModUnloaded,
    canonical_mod_name_from_path, excise_and_unload_mod_in_world, is_abi_version_compatible,
    load_mod_in_world, record_mod_named_sets_in_world, restore_mod_state_in_world,
    rollback_unscheduled_mod_in_world, save_mod_state_in_world, unload_all_mods_in_world,
};
pub use registry::{
    ComponentCollisionError, ComponentOptions, CreateMsgCursorFn, ExtractEventTargetFn,
    InsertDefaultFn, InsertRawFn, ModRegistry, MsgReadBatchFn, MsgSendFn, MsgWriteBatchFn,
    RegisteredComponent, RegistrySnapshot, TargetEntity, TriggerEventFn,
};
pub use schedule::{
    ModNamedSystemSet, ModScheduleMap, ModSystemSet, ModVisibleSets, StartupCompleted,
    mark_startup_completed, resolve_builtin_schedule_id,
};
pub use sync_ptr::{GuestFnPtr, GuestInstancePtr, GuestUserData, SymbolAddress};
pub use system::{
    BamQuerySpecOwned, GuestSystem, HostSystemCtx, MAX_SYSTEM_QUERIES, ModErrorPolicy,
    ModErrorPolicyConfig, ModQuarantine, ModSystemError, ModSystemFaulted, SystemInitError,
    host_sys_commands, host_sys_entity_get, host_sys_entity_get_mut, host_sys_host_ctx,
    host_sys_query, host_sys_res_mut_ptr, host_sys_res_ptr,
};
#[cfg(feature = "diagnostics")]
pub use system::{ModDiagnosticsCollector, ModDiagnosticsSample};
