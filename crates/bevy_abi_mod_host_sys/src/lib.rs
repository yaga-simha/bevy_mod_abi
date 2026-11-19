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
pub use ffi::assets_scene::{
    HostAssetBackend, HostSceneBackend, host_asset_get_load_state, host_asset_load,
    host_asset_release, host_cmd_spawn_scene,
};
pub use ffi::commands::{
    CommandArena, HostCommands, HostCommandsStorage, host_cmd_despawn, host_cmd_insert,
    host_cmd_insert_default, host_cmd_remove, host_cmd_spawn, host_cmd_spawn_empty,
    host_cmd_trigger,
};
pub use ffi::commands_ext::{
    host_cmd_despawn_all, host_cmd_insert_if_new, host_cmd_spawn_batch, host_cmd_spawn_persistent,
};
pub use ffi::hierarchy_ext::{ensure_children_registered_in_world, host_sys_entity_children};
pub use ffi::host_api::HOST_API;
pub use ffi::iter::{
    HostIter, QueryPlan, QueryTermMeta, SparseRowScratch, TickFilterTerm, host_iter_column_info,
    host_iter_column_mutable, host_iter_column_mutable_untracked, host_iter_column_readonly,
    host_iter_column_rows_mutable, host_iter_column_rows_readonly, host_iter_entities,
    host_iter_mark_changed_range, host_iter_next_chunk, host_iter_reset, host_msg_read_batch,
    host_msg_send, host_msg_write_batch, host_par_for_each_chunk, host_res_mut_ptr, host_res_ptr,
    host_res_ticks,
};
pub use ffi::iter_ext::host_iter_read_removed;
pub use ffi::observers_ext::{host_cmd_observe, host_cmd_trigger_ex};
pub use ffi::proc_address::host_get_proc_address;
pub use ffi::register::{
    GuestObserverBinding, LoadContext, ensure_dynamic_hook_map_in_world, host_load_host_ctx,
    host_register_component, host_register_observer, host_register_system, host_resolve_component,
};
pub use ffi::v03_ext::{
    host_bind_named_object, host_cmd_checkpoint, host_cmd_insert_relation,
    host_cmd_remove_relation, host_cmd_rollback, host_component_layout_info,
    host_configure_schedule_set, host_intern_symbol, host_iter_count, host_iter_up_cascade,
    host_log_diagnostic, host_migrate_component_column, host_mount_system_in_schedule,
    host_par_for_each_chunk_readonly, host_register_relation, host_register_system_handle,
    host_report_guest_fault, host_resolve_component_by_uuid,
};
pub use ffi::wave3_helpers::{
    WitnessFieldDescriptor, apply_entity_component_fields, apply_resource_fields,
    ensure_component_in_target_world, export_entity_component_fields, export_resource_fields,
    extract_component_between_worlds, extract_resource_between_worlds,
    insert_entity_component_from_fields, insert_or_apply_entity_component_fields,
    inspect_component_witness_fields, inspect_component_witness_fields_in_registry,
    read_entity_pod_bytes, write_entity_pod_bytes,
};
#[cfg(feature = "bridge-reflect")]
pub use ffi::wave3_helpers::{
    empty_reflect_unchecked_mut, reflect_entity_component, reflect_entity_component_mut,
};
pub use handle_header::{
    COMMANDS_MAGIC, HOST_CTX_MAGIC, HandleHeader, HostContext, ITER_MAGIC, LOAD_CTX_MAGIC,
    POISONED_MAGIC, STRICT_FFI_CHECKS, SYS_CTX_MAGIC, host_last_error,
};
#[cfg(feature = "signature-verification")]
pub use loader::{
    BAM_SIG_MACHO_SECTION_NAME, BAM_SIG_SECTION_NAME, BAMPACK_MAGIC, BAMPACK_VERSION,
    BamPackBuilder, BamPackEntry, CertificateValidityPolicy, DEFAULT_BAMPACK_NAMESPACE,
    DEFAULT_KRL_SIGNATURE_NAMESPACE, DEFAULT_MAX_MOD_BINARY_BYTES, DEFAULT_SIGNATURE_NAMESPACE,
    EmbeddedSignatureSection, KrlBuilder, MAX_BAMPACK_ENTRIES, MAX_KRL_BYTES,
    MAX_SIGNATURE_SIDECAR_BYTES, ModSigningError, ModTrustConfigError, ModTrustPolicy,
    SignatureEnforcement, SignatureVerificationError, VerifiedBamPack, VerifiedModSignature,
    default_signature_path_for, extract_embedded_bam_sig, preverify_mod_on_disk,
    sign_mod_bytes_with_certificate, sign_mod_bytes_with_key,
    sign_mod_bytes_with_openssh_certificate, zeroed_binary_for_bam_sig,
};
pub use loader::{
    BinaryFormat, LibraryResidencyInfo, LoadOptions, LoadedMod, LoadedModInfo, ModHost, ModId,
    ModLoadError, ModLoaded, ModScoped, ModUnloaded, canonical_mod_name_from_path,
    excise_and_unload_mod_in_world, inspect_library_residency, inspect_library_residency_file,
    is_abi_version_compatible, load_mod_in_world, record_mod_named_sets_in_world,
    restore_mod_state_in_world, rollback_unscheduled_mod_in_world, save_mod_state_in_world,
    unload_all_mods_in_world,
};
pub use registry::{
    ComponentCollisionError, ComponentOptions, CreateMsgCursorFn, EntityEventTriggerFn,
    ExtractEventTargetFn, ExtractRelationshipTargetsFn, HostStringBridge, HostWireBridge,
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
    ModErrorPolicyConfig, ModQuarantine, ModSystemError, ModSystemFaulted,
    PersistentMessageCursors, SystemInitError, host_sys_commands, host_sys_entity_get,
    host_sys_entity_get_mut, host_sys_host_ctx, host_sys_query, host_sys_res_mut_ptr,
    host_sys_res_ptr,
};
#[cfg(feature = "diagnostics")]
pub use system::{ModDiagnosticsCollector, ModDiagnosticsSample};
