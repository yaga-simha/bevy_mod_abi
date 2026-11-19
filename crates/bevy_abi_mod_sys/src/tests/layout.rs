//! Comprehensive ABI `0.2` layout, validation, SipHash-2-4-128, and extensibility tests.

use core::{
    ffi::{c_char, c_void},
    fmt::Write,
    mem::{align_of, offset_of, size_of},
};

use crate::{
    ABI_VERSION, ABI_VERSION_MAJOR, ABI_VERSION_MINOR, ABI_VERSION_PATCH, AccessMode,
    BAM_COMPONENT_FLAG_IMMUTABLE, BAM_COMPONENT_FLAG_SPARSE_SET, BAM_COMPONENT_FLAG_SUMMARY_TICK,
    BAM_LOG_DEBUG, BAM_LOG_ERROR, BAM_LOG_INFO, BAM_LOG_TRACE, BAM_LOG_WARN,
    BAM_OBSERVER_FLAG_PROPAGATE, BAM_RELATION_FLAG_ACYCLIC, BAM_RELATION_FLAG_CASCADE_DELETE,
    BAM_RELATION_FLAG_CASCADE_DESPAWN, BAM_RELATION_FLAG_CLEANUP_REMOVE,
    BAM_RELATION_FLAG_EXCLUSIVE, BAM_RELATION_FLAG_SYMMETRIC, BAM_RELATION_KNOWN_FLAGS,
    BAM_REQUIRED_FLAG_CUSTOM_BYTES, BAM_REQUIRED_FLAG_INIT_FN, BAM_REQUIRED_FLAG_RESOLVE_BY_UUID,
    BAM_SCHEDULE_SET_FLAG_AFTER_WEAK, BAM_SCHEDULE_SET_FLAG_BEFORE_WEAK,
    BAM_SCHEDULE_SET_FLAG_PARALLEL, BAM_SCHEDULE_SET_FLAG_SEQUENCE,
    BAM_SCHEDULE_SET_FLAG_SEQUENCE_WEAK, BAM_SCHEDULE_SET_KNOWN_FLAGS,
    BAM_STORAGE_FLAG_ENTITY_EVENT, BAM_STORAGE_FLAG_EVENT, BAM_STORAGE_FLAG_IMMUTABLE,
    BAM_STORAGE_FLAG_MESSAGE, BAM_STORAGE_FLAG_RESOURCE, BAM_STORAGE_FLAG_SPARSE_SET,
    BAM_STORAGE_FLAG_SUMMARY_TICK, BAM_SYSTEM_FLAG_AMBIGUOUS_WITH_ALL, BAM_SYSTEM_FLAG_MAIN_THREAD,
    BAM_TRIGGER_FLAG_PROPAGATE, BamAssetGetLoadStateFn, BamAssetLoadFn, BamAssetReleaseFn,
    BamChangeTicks, BamChunkTaskFn, BamCmdDespawnAllFn, BamCmdDespawnFn, BamCmdInsertDefaultFn,
    BamCmdInsertFn, BamCmdObserveFn, BamCmdRemoveFn, BamCmdSpawnBatchFn, BamCmdSpawnEmptyFn,
    BamCmdSpawnFn, BamCmdSpawnSceneFn, BamCmdTriggerExFn, BamCmdTriggerFn, BamColumnInfo,
    BamCommands, BamComponent, BamEventKind, BamFieldDesc, BamFieldKind, BamGetProcAddressFn,
    BamHostApi, BamHostCtx, BamInitFn, BamInstance, BamIter, BamIterColumnInfoFn,
    BamIterColumnMutableFn, BamIterColumnMutableUntrackedFn, BamIterColumnReadonlyFn,
    BamIterColumnRowsMutableFn, BamIterColumnRowsReadonlyFn, BamIterEntitiesFn,
    BamIterMarkChangedRangeFn, BamIterNextChunkFn, BamIterReadRemovedFn, BamIterResetFn,
    BamLastErrorFn, BamLayoutInfo, BamLoadCtx, BamLoadHostCtxFn, BamManifest, BamManifestFn,
    BamMsgReadBatchFn, BamMsgSendFn, BamMsgWriteBatchFn, BamObserverDesc, BamObserverFn,
    BamObserverMutFn, BamParForEachChunkFn, BamQueryDesc, BamRegisterComponentFn,
    BamRegisterObserverFn, BamRegisterSystemFn, BamRelationDesc, BamRequiredComponent,
    BamResMutPtrFn, BamResPtrFn, BamResTicksFn, BamResolveComponentFn, BamRestoreStateFn,
    BamRunIfFn, BamSaveStateFn, BamScheduleSetDesc, BamShutdownFn, BamStateWriterFn, BamSymbol,
    BamSysCommandsFn, BamSysEntityChildrenFn, BamSysEntityGetFn, BamSysEntityGetMutFn,
    BamSysHostCtxFn, BamSysQueryFn, BamSysResMutPtrFn, BamSysResPtrFn, BamSystem, BamSystemCtx,
    BamSystemDesc, BamSystemFn, BamSystemSet, BamTermDesc, BamTriggerDesc, ComponentCloneFn,
    ComponentDesc, ComponentHookFn, ComponentUuid, ExtensibleStruct, FfiAssetHandle,
    FfiAssetLoadState, FfiBool, FfiChildOf, FfiEntity, FfiOption, FfiResult, FfiSlice,
    MAX_QUERIES_PER_SYSTEM, MAX_TERMS_PER_QUERY, ModComponent, SIPHASH_KEY_0, SIPHASH_KEY_1,
    ScheduleId, StrView, SymbolEntry, VTableHeader, abi_version_is_compatible, abi_version_major,
    abi_version_minor, abi_version_patch, assert_abi_pod, assert_raw_insertable, fnv1a_64_init,
    fnv1a_64_str, fnv1a_64_update_bytes, pack_abi_version, read_descriptor, read_extensible_struct,
    siphash24_128, unpack_abi_version,
};

struct StackFmtBuf<const N: usize> {
    bytes: [u8; N],
    len: usize,
}

impl<const N: usize> StackFmtBuf<N> {
    const fn new() -> Self {
        Self {
            bytes: [0u8; N],
            len: 0,
        }
    }

    fn as_str(&self) -> &str {
        core::str::from_utf8(&self.bytes[..self.len]).expect("StackFmtBuf holds valid UTF-8")
    }
}

impl<const N: usize> Write for StackFmtBuf<N> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let rem = N.saturating_sub(self.len);
        if s.len() > rem {
            return Err(core::fmt::Error);
        }
        self.bytes[self.len..self.len + s.len()].copy_from_slice(s.as_bytes());
        self.len += s.len();
        Ok(())
    }
}

// --- Dummy callbacks for populating `BamHostApi` and descriptors ---

unsafe extern "C" fn stub_get_proc_address(
    _ctx: *const BamHostCtx,
    _name: *const c_char,
) -> *const c_void {
    core::ptr::null()
}

unsafe extern "C" fn stub_last_error(
    _ctx: *const BamHostCtx,
    _buf: *mut c_char,
    _cap: usize,
) -> usize {
    0
}

unsafe extern "C" fn stub_load_host_ctx(_load_ctx: *const BamLoadCtx) -> *const BamHostCtx {
    core::ptr::null()
}

unsafe extern "C" fn stub_register_component(
    _load_ctx: *mut BamLoadCtx,
    _desc: *const ComponentDesc,
    out_component: *mut BamComponent,
) -> FfiResult {
    if out_component.is_null() {
        return FfiResult::NULL_POINTER;
    }
    // SAFETY: Non-null out-pointer provided by test caller.
    unsafe {
        *out_component = BamComponent::from_raw(1);
    }
    FfiResult::OK
}

unsafe extern "C" fn stub_resolve_component(
    _ctx: *const BamHostCtx,
    _name: *const c_char,
    _uuid: *const ComponentUuid,
    out_component: *mut BamComponent,
) -> FfiResult {
    if out_component.is_null() {
        return FfiResult::NULL_POINTER;
    }
    // SAFETY: Non-null out-pointer provided by test caller.
    unsafe {
        *out_component = BamComponent::from_raw(1);
    }
    FfiResult::OK
}

unsafe extern "C" fn stub_register_system(
    _load_ctx: *mut BamLoadCtx,
    _desc: *const BamSystemDesc,
) -> FfiResult {
    FfiResult::OK
}

unsafe extern "C" fn stub_sys_host_ctx(_sys_ctx: *const BamSystemCtx) -> *const BamHostCtx {
    core::ptr::null()
}

unsafe extern "C" fn stub_sys_query(
    _sys_ctx: *mut BamSystemCtx,
    _query_index: usize,
    _out_iter: *mut *mut BamIter,
) -> FfiResult {
    FfiResult::OK
}

unsafe extern "C" fn stub_sys_commands(
    _sys_ctx: *mut BamSystemCtx,
    _out_commands: *mut *mut BamCommands,
) -> FfiResult {
    FfiResult::OK
}

unsafe extern "C" fn stub_iter_next_chunk(_iter: *mut BamIter, _out_len: *mut usize) -> FfiBool {
    FfiBool::FALSE
}

unsafe extern "C" fn stub_iter_reset(_iter: *mut BamIter) -> FfiResult {
    FfiResult::OK
}

unsafe extern "C" fn stub_iter_entities(_iter: *const BamIter) -> *const FfiEntity {
    core::ptr::null()
}

unsafe extern "C" fn stub_iter_column_readonly(
    _iter: *const BamIter,
    _term_index: usize,
) -> *const u8 {
    core::ptr::null()
}

unsafe extern "C" fn stub_iter_column_mutable(_iter: *mut BamIter, _term_index: usize) -> *mut u8 {
    core::ptr::null_mut()
}

unsafe extern "C" fn stub_iter_column_mutable_untracked(
    _iter: *mut BamIter,
    _term_index: usize,
) -> *mut u8 {
    core::ptr::null_mut()
}

unsafe extern "C" fn stub_iter_mark_changed_range(
    _iter: *mut BamIter,
    _term_index: usize,
    _start_row: usize,
    _row_count: usize,
) -> FfiResult {
    FfiResult::OK
}

unsafe extern "C" fn stub_iter_column_rows_readonly(
    _iter: *mut BamIter,
    _term_index: usize,
) -> *const *const u8 {
    core::ptr::null()
}

unsafe extern "C" fn stub_iter_column_rows_mutable(
    _iter: *mut BamIter,
    _term_index: usize,
) -> *const *mut u8 {
    core::ptr::null()
}

unsafe extern "C" fn stub_iter_column_info(
    _iter: *const BamIter,
    _term_index: usize,
    _out_info: *mut BamColumnInfo,
) -> FfiResult {
    FfiResult::OK
}

unsafe extern "C" fn stub_res_ptr(_iter: *const BamIter, _term_index: usize) -> *const u8 {
    core::ptr::null()
}

unsafe extern "C" fn stub_res_mut_ptr(_iter: *mut BamIter, _term_index: usize) -> *mut u8 {
    core::ptr::null_mut()
}

unsafe extern "C" fn stub_cmd_spawn_empty(
    _commands: *mut BamCommands,
    _out_entity: *mut FfiEntity,
) -> FfiResult {
    FfiResult::OK
}

unsafe extern "C" fn stub_cmd_spawn(
    _commands: *mut BamCommands,
    _components: *const BamComponent,
    _data_ptrs: *const *const u8,
    _count: usize,
    _out_entity: *mut FfiEntity,
) -> FfiResult {
    FfiResult::OK
}

unsafe extern "C" fn stub_cmd_despawn(
    _commands: *mut BamCommands,
    _entity: FfiEntity,
) -> FfiResult {
    FfiResult::OK
}

unsafe extern "C" fn stub_cmd_insert(
    _commands: *mut BamCommands,
    _entity: FfiEntity,
    _component: BamComponent,
    _data_ptr: *const u8,
    _data_len: usize,
) -> FfiResult {
    FfiResult::OK
}

unsafe extern "C" fn stub_cmd_insert_default(
    _commands: *mut BamCommands,
    _entity: FfiEntity,
    _component: BamComponent,
) -> FfiResult {
    FfiResult::OK
}

unsafe extern "C" fn stub_cmd_remove(
    _commands: *mut BamCommands,
    _entity: FfiEntity,
    _component: BamComponent,
) -> FfiResult {
    FfiResult::OK
}

unsafe extern "C" fn stub_system_cb(
    _sys_ctx: *mut BamSystemCtx,
    _user_data: *mut c_void,
) -> FfiResult {
    FfiResult::OK
}

unsafe extern "C" fn stub_run_if(_user_data: *mut c_void) -> FfiBool {
    FfiBool::TRUE
}

unsafe extern "C" fn stub_drop(_ptr: *mut c_void) {}

fn populated_host_api() -> BamHostApi {
    BamHostApi {
        struct_size: size_of::<BamHostApi>() as u32,
        abi_version: ABI_VERSION,
        get_proc_address: Some(stub_get_proc_address),
        last_error: Some(stub_last_error),
        load_host_ctx: Some(stub_load_host_ctx),
        register_component: Some(stub_register_component),
        resolve_component: Some(stub_resolve_component),
        register_system: Some(stub_register_system),
        sys_host_ctx: Some(stub_sys_host_ctx),
        sys_query: Some(stub_sys_query),
        sys_commands: Some(stub_sys_commands),
        iter_next_chunk: Some(stub_iter_next_chunk),
        iter_reset: Some(stub_iter_reset),
        iter_entities: Some(stub_iter_entities),
        iter_column_readonly: Some(stub_iter_column_readonly),
        iter_column_mutable: Some(stub_iter_column_mutable),
        iter_column_mutable_untracked: Some(stub_iter_column_mutable_untracked),
        iter_mark_changed_range: Some(stub_iter_mark_changed_range),
        iter_column_rows_readonly: Some(stub_iter_column_rows_readonly),
        iter_column_rows_mutable: Some(stub_iter_column_rows_mutable),
        iter_column_info: Some(stub_iter_column_info),
        res_ptr: Some(stub_res_ptr),
        res_mut_ptr: Some(stub_res_mut_ptr),
        cmd_spawn_empty: Some(stub_cmd_spawn_empty),
        cmd_spawn: Some(stub_cmd_spawn),
        cmd_despawn: Some(stub_cmd_despawn),
        cmd_insert: Some(stub_cmd_insert),
        cmd_insert_default: Some(stub_cmd_insert_default),
        cmd_remove: Some(stub_cmd_remove),
        par_for_each_chunk: None,
        sys_entity_get: None,
        sys_entity_get_mut: None,
        msg_read_batch: None,
        msg_send: None,
        msg_write_batch: None,
        res_ticks: None,
        sys_res_ptr: None,
        sys_res_mut_ptr: None,
        register_observer: None,
        cmd_trigger: None,
        cmd_spawn_persistent: None,
        cmd_spawn_batch: None,
        cmd_despawn_all: None,
        cmd_insert_if_new: None,
        cmd_trigger_ex: None,
        cmd_observe: None,
        iter_read_removed: None,
        sys_entity_children: None,
        asset_load: None,
        asset_get_load_state: None,
        asset_release: None,
        cmd_spawn_scene: None,
        log_diagnostic: None,
        report_guest_fault: None,
        resolve_component_by_uuid: None,
        component_layout_info: None,
        register_relation: None,
        intern_symbol: None,
        bind_named_object: None,
        iter_count: None,
        iter_up_cascade: None,
        par_for_each_chunk_readonly: None,
        register_system_handle: None,
        configure_schedule_set: None,
        mount_system_in_schedule: None,
        cmd_checkpoint: None,
        cmd_rollback: None,
        cmd_insert_relation: None,
        cmd_remove_relation: None,
        migrate_component_column: None,
    }
}

#[test]
fn abi_v0_2_exact_sizes_alignments_and_offsets() {
    assert_eq!(size_of::<FfiBool>(), 1);
    assert_eq!(align_of::<FfiBool>(), 1);
    assert_eq!(size_of::<BamComponent>(), 4);
    assert_eq!(align_of::<BamComponent>(), 4);
    assert_eq!(size_of::<FfiEntity>(), 8);
    assert_eq!(size_of::<FfiChildOf>(), 8);
    assert_eq!(size_of::<BamChangeTicks>(), 16);
    assert_eq!(align_of::<BamChangeTicks>(), 4);
    assert_eq!(size_of::<FfiResult>(), 4);
    assert_eq!(align_of::<FfiResult>(), 4);
    assert_eq!(size_of::<AccessMode>(), 4);
    assert_eq!(align_of::<AccessMode>(), 4);
    assert_eq!(size_of::<ScheduleId>(), 4);
    assert_eq!(align_of::<ScheduleId>(), 4);
    assert_eq!(size_of::<BamEventKind>(), 4);
    assert_eq!(align_of::<BamEventKind>(), 4);
    assert_eq!(size_of::<FfiAssetLoadState>(), 4);
    assert_eq!(align_of::<FfiAssetLoadState>(), 4);
    assert_eq!(size_of::<FfiAssetHandle>(), 32);
    assert_eq!(align_of::<FfiAssetHandle>(), 4);
    assert_eq!(offset_of!(FfiAssetHandle, kind), 0);
    assert_eq!(offset_of!(FfiAssetHandle, reserved0), 1);
    assert_eq!(offset_of!(FfiAssetHandle, asset_type), 4);
    assert_eq!(offset_of!(FfiAssetHandle, index), 8);
    assert_eq!(offset_of!(FfiAssetHandle, generation), 12);
    assert_eq!(offset_of!(FfiAssetHandle, uuid), 16);
    assert_eq!(size_of::<ComponentUuid>(), 16);
    assert_eq!(align_of::<ComponentUuid>(), 1);
    assert_eq!(size_of::<BamTermDesc>(), 12);
    assert_eq!(align_of::<BamTermDesc>(), 4);
    assert_eq!(offset_of!(BamTermDesc, component), 0);
    assert_eq!(offset_of!(BamTermDesc, mode), 4);
    assert_eq!(offset_of!(BamTermDesc, flags), 8);
    assert_eq!(offset_of!(BamTermDesc, or_group), 10);
    assert_eq!(offset_of!(BamTermDesc, reserved0), 11);

    // Every `Option<unsafe extern "C" fn(...)>` and opaque pointer must equal one pointer width.
    let ptr_sz = size_of::<*const ()>();
    assert_eq!(size_of::<*mut BamInstance>(), ptr_sz);
    assert_eq!(size_of::<Option<BamManifestFn>>(), ptr_sz);
    assert_eq!(size_of::<Option<BamInitFn>>(), ptr_sz);
    assert_eq!(size_of::<Option<BamShutdownFn>>(), ptr_sz);
    assert_eq!(size_of::<Option<BamSystemFn>>(), ptr_sz);
    assert_eq!(size_of::<Option<BamRunIfFn>>(), ptr_sz);
    assert_eq!(size_of::<Option<BamGetProcAddressFn>>(), ptr_sz);
    assert_eq!(size_of::<Option<BamLastErrorFn>>(), ptr_sz);
    assert_eq!(size_of::<Option<BamRegisterComponentFn>>(), ptr_sz);
    assert_eq!(size_of::<Option<BamResolveComponentFn>>(), ptr_sz);
    assert_eq!(size_of::<Option<BamRegisterSystemFn>>(), ptr_sz);
    assert_eq!(size_of::<Option<BamSysHostCtxFn>>(), ptr_sz);
    assert_eq!(size_of::<Option<BamSysQueryFn>>(), ptr_sz);
    assert_eq!(size_of::<Option<BamSysCommandsFn>>(), ptr_sz);
    assert_eq!(size_of::<Option<BamIterNextChunkFn>>(), ptr_sz);
    assert_eq!(size_of::<Option<BamIterResetFn>>(), ptr_sz);
    assert_eq!(size_of::<Option<BamIterEntitiesFn>>(), ptr_sz);
    assert_eq!(size_of::<Option<BamIterColumnReadonlyFn>>(), ptr_sz);
    assert_eq!(size_of::<Option<BamIterColumnMutableFn>>(), ptr_sz);
    assert_eq!(size_of::<Option<BamIterColumnMutableUntrackedFn>>(), ptr_sz);
    assert_eq!(size_of::<Option<BamIterMarkChangedRangeFn>>(), ptr_sz);
    assert_eq!(size_of::<Option<BamIterColumnRowsReadonlyFn>>(), ptr_sz);
    assert_eq!(size_of::<Option<BamIterColumnRowsMutableFn>>(), ptr_sz);
    assert_eq!(size_of::<Option<BamIterColumnInfoFn>>(), ptr_sz);
    assert_eq!(size_of::<Option<BamResPtrFn>>(), ptr_sz);
    assert_eq!(size_of::<Option<BamResMutPtrFn>>(), ptr_sz);
    assert_eq!(size_of::<Option<BamCmdSpawnEmptyFn>>(), ptr_sz);
    assert_eq!(size_of::<Option<BamCmdSpawnFn>>(), ptr_sz);
    assert_eq!(size_of::<Option<BamCmdDespawnFn>>(), ptr_sz);
    assert_eq!(size_of::<Option<BamCmdInsertFn>>(), ptr_sz);
    assert_eq!(size_of::<Option<BamCmdInsertDefaultFn>>(), ptr_sz);
    assert_eq!(size_of::<Option<BamCmdRemoveFn>>(), ptr_sz);

    assert_eq!(size_of::<Option<BamLoadHostCtxFn>>(), ptr_sz);
    assert_eq!(size_of::<Option<BamChunkTaskFn>>(), ptr_sz);
    assert_eq!(size_of::<Option<BamParForEachChunkFn>>(), ptr_sz);
    assert_eq!(size_of::<Option<BamSysEntityGetFn>>(), ptr_sz);
    assert_eq!(size_of::<Option<BamSysEntityGetMutFn>>(), ptr_sz);
    assert_eq!(size_of::<Option<BamMsgReadBatchFn>>(), ptr_sz);
    assert_eq!(size_of::<Option<BamMsgSendFn>>(), ptr_sz);
    assert_eq!(size_of::<Option<BamMsgWriteBatchFn>>(), ptr_sz);
    assert_eq!(size_of::<Option<BamResTicksFn>>(), ptr_sz);
    assert_eq!(size_of::<Option<BamSysResPtrFn>>(), ptr_sz);
    assert_eq!(size_of::<Option<BamSysResMutPtrFn>>(), ptr_sz);
    assert_eq!(size_of::<Option<BamRegisterObserverFn>>(), ptr_sz);
    assert_eq!(size_of::<Option<BamCmdTriggerFn>>(), ptr_sz);
    assert_eq!(size_of::<Option<BamObserverFn>>(), ptr_sz);
    assert_eq!(size_of::<Option<BamStateWriterFn>>(), ptr_sz);
    assert_eq!(size_of::<Option<BamSaveStateFn>>(), ptr_sz);
    assert_eq!(size_of::<Option<BamRestoreStateFn>>(), ptr_sz);
    assert_eq!(size_of::<Option<ComponentHookFn>>(), ptr_sz);
    assert_eq!(size_of::<Option<ComponentCloneFn>>(), ptr_sz);
    assert_eq!(size_of::<Option<BamObserverMutFn>>(), ptr_sz);
    assert_eq!(size_of::<Option<BamCmdSpawnBatchFn>>(), ptr_sz);
    assert_eq!(size_of::<Option<BamCmdDespawnAllFn>>(), ptr_sz);
    assert_eq!(size_of::<Option<BamCmdTriggerExFn>>(), ptr_sz);
    assert_eq!(size_of::<Option<BamCmdObserveFn>>(), ptr_sz);
    assert_eq!(size_of::<Option<BamIterReadRemovedFn>>(), ptr_sz);
    assert_eq!(size_of::<Option<BamSysEntityChildrenFn>>(), ptr_sz);
    assert_eq!(size_of::<Option<BamAssetLoadFn>>(), ptr_sz);
    assert_eq!(size_of::<Option<BamAssetGetLoadStateFn>>(), ptr_sz);
    assert_eq!(size_of::<Option<BamAssetReleaseFn>>(), ptr_sz);
    assert_eq!(size_of::<Option<BamCmdSpawnSceneFn>>(), ptr_sz);

    #[cfg(target_pointer_width = "64")]
    {
        assert_eq!(size_of::<StrView>(), 16);
        assert_eq!(align_of::<StrView>(), 8);
        assert_eq!(size_of::<BamManifest>(), 48);
        assert_eq!(BamManifest::MIN_V0_2_SIZE, 32);
        assert_eq!(size_of::<BamQueryDesc>(), 24);
        assert_eq!(BamQueryDesc::MIN_V0_2_SIZE, 24);
        assert_eq!(size_of::<BamSystemDesc>(), 168);
        assert_eq!(BamSystemDesc::MIN_V0_2_SIZE, 104);
        assert_eq!(size_of::<BamObserverDesc>(), 88);
        assert_eq!(BamObserverDesc::MIN_V0_2_SIZE, 48);
        assert_eq!(size_of::<BamTriggerDesc>(), 64);
        assert_eq!(BamTriggerDesc::MIN_V0_2_SIZE, 64);
        assert_eq!(size_of::<ComponentDesc>(), 176);
        assert_eq!(ComponentDesc::MIN_V0_2_SIZE, 64);
        assert_eq!(ComponentDesc::MIN_V0_3_SIZE, 176);
        assert_eq!(size_of::<BamColumnInfo>(), 48);
        assert_eq!(size_of::<VTableHeader>(), 40);
        assert_eq!(VTableHeader::MIN_V0_2_SIZE, 40);
        assert_eq!(size_of::<BamHostApi>(), 552);
        assert_eq!(BamHostApi::MIN_V0_2_SIZE, 224);
        assert_eq!(BamHostApi::MIN_V0_3_SIZE, 552);
    }

    #[cfg(target_pointer_width = "32")]
    {
        assert_eq!(size_of::<StrView>(), 8);
        assert_eq!(align_of::<StrView>(), 4);
        assert_eq!(size_of::<BamManifest>(), 32);
        assert_eq!(BamManifest::MIN_V0_2_SIZE, 24);
        assert_eq!(size_of::<BamQueryDesc>(), 16);
        assert_eq!(BamQueryDesc::MIN_V0_2_SIZE, 16);
        assert_eq!(size_of::<BamSystemDesc>(), 92);
        assert_eq!(BamSystemDesc::MIN_V0_2_SIZE, 60);
        assert_eq!(size_of::<BamObserverDesc>(), 64);
        assert_eq!(BamObserverDesc::MIN_V0_2_SIZE, 36);
        assert_eq!(size_of::<BamTriggerDesc>(), 48);
        assert_eq!(BamTriggerDesc::MIN_V0_2_SIZE, 48);
        assert_eq!(size_of::<ComponentDesc>(), 104);
        assert_eq!(ComponentDesc::MIN_V0_2_SIZE, 44);
        assert_eq!(ComponentDesc::MIN_V0_3_SIZE, 104);
        assert_eq!(size_of::<BamColumnInfo>(), 40);
        assert_eq!(size_of::<VTableHeader>(), 32);
        assert_eq!(VTableHeader::MIN_V0_2_SIZE, 32);
        assert_eq!(size_of::<BamHostApi>(), 280);
        assert_eq!(BamHostApi::MIN_V0_2_SIZE, 116);
        assert_eq!(BamHostApi::MIN_V0_3_SIZE, 280);
    }
}

#[test]
fn zero_major_and_post_one_semver_handshake() {
    assert_eq!(ABI_VERSION_MAJOR, 0);
    assert_eq!(ABI_VERSION_MINOR, 3);
    assert_eq!(ABI_VERSION_PATCH, 0);
    assert_eq!(ABI_VERSION, 0x0003_0000);
    assert_eq!(unpack_abi_version(ABI_VERSION), (0, 3, 0));
    assert_eq!(abi_version_major(0x0102_0003), 1);
    assert_eq!(abi_version_minor(0x0102_0003), 2);
    assert_eq!(abi_version_patch(0x0102_0003), 3);

    // Pre-1.0 (`major == 0`): minor differences are breaking; patch differences are compatible.
    let v0_1_0 = pack_abi_version(0, 1, 0);
    let v0_2_0 = pack_abi_version(0, 2, 0);
    let v0_2_9 = pack_abi_version(0, 2, 9);
    let v0_3_0 = pack_abi_version(0, 3, 0);
    assert!(abi_version_is_compatible(v0_2_0, v0_2_9));
    assert!(abi_version_is_compatible(v0_2_9, v0_2_0));
    assert!(!abi_version_is_compatible(v0_2_0, v0_1_0));
    assert!(!abi_version_is_compatible(v0_1_0, v0_2_0));
    assert!(!abi_version_is_compatible(v0_3_0, v0_2_0));

    // Post-1.0 (`major >= 1`): newer minor host accepts older minor guest, never vice versa.
    let v1_0_0 = pack_abi_version(1, 0, 0);
    let v1_2_1 = pack_abi_version(1, 2, 1);
    let v2_0_0 = pack_abi_version(2, 0, 0);
    assert!(abi_version_is_compatible(v1_2_1, v1_0_0));
    assert!(!abi_version_is_compatible(v1_0_0, v1_2_1));
    assert!(!abi_version_is_compatible(v2_0_0, v1_2_1));
    assert!(!abi_version_is_compatible(v1_0_0, v0_2_0));
}

#[test]
#[should_panic(expected = "ABI major version exceeds 8-bit range")]
fn pack_abi_version_rejects_major_overflow() {
    let _ = pack_abi_version(256, 0, 0);
}

#[test]
#[should_panic(expected = "ABI minor version exceeds 8-bit range")]
fn pack_abi_version_rejects_minor_overflow() {
    let _ = pack_abi_version(0, 256, 0);
}

#[test]
#[should_panic(expected = "ABI patch version exceeds 16-bit range")]
fn pack_abi_version_rejects_patch_overflow() {
    let _ = pack_abi_version(0, 2, 65_536);
}

#[test]
fn ffi_result_taxonomy_and_display() {
    let expected = [
        (FfiResult::OK, 0, "OK"),
        (FfiResult::INVALID_COMPONENT, 1, "INVALID_COMPONENT"),
        (FfiResult::ACCESS_CONFLICT, 2, "ACCESS_CONFLICT"),
        (FfiResult::PANIC_CAUGHT, 3, "PANIC_CAUGHT"),
        (FfiResult::NULL_POINTER, 4, "NULL_POINTER"),
        (FfiResult::INVALID_UTF8, 5, "INVALID_UTF8"),
        (FfiResult::PERMISSION_DENIED, 6, "PERMISSION_DENIED"),
        (FfiResult::INVALID_VERSION, 7, "INVALID_VERSION"),
        (FfiResult::INVALID_ARGUMENT, 8, "INVALID_ARGUMENT"),
        (FfiResult::OUT_OF_RANGE, 9, "OUT_OF_RANGE"),
        (FfiResult::NOT_FOUND, 10, "NOT_FOUND"),
        (FfiResult::INVALID_STATE, 11, "INVALID_STATE"),
        (FfiResult::INVALID_ENTITY, 12, "INVALID_ENTITY"),
        (FfiResult::CAPACITY_EXCEEDED, 13, "CAPACITY_EXCEEDED"),
        (FfiResult::UNSUPPORTED, 14, "UNSUPPORTED"),
        (FfiResult::TYPE_MISMATCH, 15, "TYPE_MISMATCH"),
        (FfiResult::CYCLE_DETECTED, 16, "CYCLE_DETECTED"),
        (FfiResult::INTERNAL_ERROR, 99, "INTERNAL_ERROR"),
    ];

    for (code, raw, label) in expected {
        assert_eq!(code.0, raw);
        assert_eq!(code.name(), label);
        assert_eq!(code.is_ok(), raw == 0);
        assert_eq!(code.is_err(), raw != 0);
        let mut buf = StackFmtBuf::<64>::new();
        write!(&mut buf, "{code}").unwrap();
        assert_eq!(buf.as_str(), label);
    }

    // Foreign unrecognized integer status values never cause UB and format cleanly.
    let foreign = FfiResult(777);
    assert!(foreign.is_err());
    assert_eq!(foreign.name(), "UNKNOWN");
    let mut buf = StackFmtBuf::<64>::new();
    write!(&mut buf, "{foreign}").unwrap();
    assert_eq!(buf.as_str(), "UNKNOWN(777)");
}

#[test]
fn extensible_struct_zero_extension_and_truncation() {
    type FutureSlotFn = unsafe extern "C" fn(ctx: *const BamHostCtx) -> FfiResult;

    #[repr(C)]
    #[derive(Copy, Clone)]
    struct SyntheticHostApiV03 {
        v0_2: BamHostApi,
        future_hook_a: Option<FutureSlotFn>,
        future_hook_b: Option<FutureSlotFn>,
    }

    // SAFETY: `SyntheticHostApiV03` is `#[repr(C)]`, starts with `BamHostApi`'s `struct_size` at
    // offset 0, and appends only `Option<unsafe extern "C" fn(...)>` slots whose zero pattern is `None`.
    unsafe impl ExtensibleStruct for SyntheticHostApiV03 {
        const MIN_V0_2_SIZE: usize = BamHostApi::MIN_V0_2_SIZE;
    }

    unsafe extern "C" fn dummy_future(_ctx: *const BamHostCtx) -> FfiResult {
        FfiResult::OK
    }

    // 1. Newer v0.3 consumer reading an older v0.2 `BamHostApi`: trailing `Option<fn>` slots zero-fill to `None`.
    let api_v02 = populated_host_api();
    assert_eq!(api_v02.validate_for_guest(ABI_VERSION), FfiResult::OK);
    // SAFETY: `api_v02` is a valid initialized `BamHostApi` on the stack.
    let widened =
        unsafe { read_descriptor::<SyntheticHostApiV03>((&raw const api_v02).cast::<c_void>()) }
            .expect("v0.3 reader must accept v0.2 BamHostApi");
    assert_eq!(widened.v0_2.validate_for_guest(ABI_VERSION), FfiResult::OK);
    assert!(widened.future_hook_a.is_none());
    assert!(widened.future_hook_b.is_none());
    assert!(!widened.v0_2.covers_slot(
        offset_of!(SyntheticHostApiV03, future_hook_a),
        size_of::<Option<FutureSlotFn>>(),
    ));

    // 2. Older v0.2 consumer reading a newer v0.3 `SyntheticHostApiV03`: extra trailing bytes ignored cleanly.
    let api_v03 = SyntheticHostApiV03 {
        v0_2: BamHostApi {
            struct_size: size_of::<SyntheticHostApiV03>() as u32,
            ..api_v02
        },
        future_hook_a: Some(dummy_future),
        future_hook_b: None,
    };
    // SAFETY: `api_v03` is a valid initialized `SyntheticHostApiV03` on the stack.
    let narrowed = unsafe { read_descriptor::<BamHostApi>((&raw const api_v03).cast::<c_void>()) }
        .expect("v0.2 reader must accept larger v0.3 BamHostApi");
    assert_eq!(narrowed.validate_for_guest(ABI_VERSION), FfiResult::OK);
    assert!(narrowed.covers_slot(
        offset_of!(SyntheticHostApiV03, future_hook_a),
        size_of::<Option<FutureSlotFn>>(),
    ));

    // 3. Truncated descriptor smaller than `MIN_V0_2_SIZE` fails with `INVALID_VERSION` without OOB read.
    #[repr(C, align(8))]
    struct AlignedTinyHeader([u32; 2]);
    let tiny_header = AlignedTinyHeader([8, ABI_VERSION]);
    let tiny_api_ptr = (&raw const tiny_header).cast::<BamHostApi>();
    // SAFETY: `tiny_header` has 8 initialized bytes and `struct_size == 8 < BamHostApi::MIN_V0_2_SIZE`.
    let err = unsafe { read_descriptor::<BamHostApi>((&raw const tiny_header).cast::<c_void>()) }
        .unwrap_err();
    assert_eq!(err, FfiResult::INVALID_VERSION);

    // SAFETY: `tiny_header` is 8-byte aligned and has 8 readable bytes (`struct_size == 8`).
    unsafe {
        assert_eq!(BamHostApi::raw_struct_size(tiny_api_ptr), Some(8));
        assert_eq!(BamHostApi::raw_abi_version(tiny_api_ptr), Some(ABI_VERSION));
        assert_eq!(BamHostApi::raw_struct_size(core::ptr::null()), None);
    }

    // 4. Null pointer fails with `NULL_POINTER`.
    // SAFETY: Null pointer check is explicitly handled without dereferencing.
    let null_desc_res = unsafe { read_descriptor::<BamSystemDesc>(core::ptr::null()) };
    assert_eq!(null_desc_res.unwrap_err(), FfiResult::NULL_POINTER);
    // SAFETY: Null pointer check is explicitly handled without dereferencing.
    let null_ext_res = unsafe { read_extensible_struct::<BamQueryDesc>(core::ptr::null(), 64) };
    assert_eq!(null_ext_res.unwrap_err(), FfiResult::NULL_POINTER);

    // 5. Missing mandatory slot in `BamHostApi` returns `NULL_POINTER`.
    let mut missing_slot = api_v02;
    missing_slot.cmd_spawn = None;
    assert!(!missing_slot.has_all_v0_2_slots());
    assert_eq!(
        missing_slot.validate_for_guest(ABI_VERSION),
        FfiResult::NULL_POINTER
    );
}

#[test]
fn unknown_flags_and_nonzero_reserved_rejected() {
    // BamManifest
    let manifest = BamManifest::new(c"combat_mod".as_ptr(), c"0.2.0".as_ptr());
    assert_eq!(manifest.validate(), FfiResult::OK);
    assert_eq!(
        manifest.with_flags(1 << 0).validate(),
        FfiResult::UNSUPPORTED
    );
    let mut bad_manifest = manifest;
    bad_manifest.reserved0 = 1;
    assert_eq!(bad_manifest.validate(), FfiResult::INVALID_ARGUMENT);
    bad_manifest.reserved0 = 0;
    bad_manifest.name = core::ptr::null();
    assert_eq!(bad_manifest.validate(), FfiResult::NULL_POINTER);

    // BamTermDesc
    let comp = BamComponent::from_raw(1);
    let term = BamTermDesc::read(comp);
    assert_eq!(term.validate(), FfiResult::OK);
    assert_eq!(term.with_flags(1 << 3).validate(), FfiResult::UNSUPPORTED);
    assert_eq!(
        BamTermDesc::read(BamComponent::INVALID).validate(),
        FfiResult::INVALID_COMPONENT
    );
    assert_eq!(
        BamTermDesc::new(comp, AccessMode(99)).validate(),
        FfiResult::INVALID_ARGUMENT
    );

    // BamQueryDesc
    let query = BamQueryDesc::new(&raw const term, 1);
    assert_eq!(query.validate(), FfiResult::OK);
    assert_eq!(BamQueryDesc::empty().validate(), FfiResult::OK);
    assert_eq!(query.with_flags(1 << 0).validate(), FfiResult::UNSUPPORTED);
    assert_eq!(
        BamQueryDesc::new(core::ptr::null(), 1).validate(),
        FfiResult::NULL_POINTER
    );
    assert_eq!(
        BamQueryDesc::new(&raw const term, MAX_TERMS_PER_QUERY + 1).validate(),
        FfiResult::CAPACITY_EXCEEDED
    );

    // BamSystemDesc (`custom schedule + `main thread + `flags/reserved)
    let after_names = [c"input_set".as_ptr()];
    let before_names = [c"render_set".as_ptr()];
    let sys = BamSystemDesc::new(
        c"physics_step".as_ptr(),
        &raw const query,
        1,
        stub_system_cb,
        core::ptr::null_mut(),
    )
    .with_main_thread()
    .with_schedule(ScheduleId::custom(ScheduleId::CUSTOM_BASE + 7))
    .with_after(after_names.as_ptr(), after_names.len())
    .with_before(before_names.as_ptr(), before_names.len())
    .with_run_if(stub_run_if, core::ptr::null_mut());
    assert!(sys.is_main_thread());
    assert_eq!(sys.flags, BAM_SYSTEM_FLAG_MAIN_THREAD);
    assert_eq!(sys.validate(), FfiResult::OK);

    // Unknown system flag -> UNSUPPORTED
    assert_eq!(
        sys.with_flags(BAM_SYSTEM_FLAG_MAIN_THREAD | (1 << 7))
            .validate(),
        FfiResult::UNSUPPORTED
    );

    // Non-zero reserved0 -> INVALID_ARGUMENT
    let mut bad_sys = sys;
    bad_sys.reserved0 = 42;
    assert_eq!(bad_sys.validate(), FfiResult::INVALID_ARGUMENT);

    // Reserved schedule IDs 6..1024 -> INVALID_ARGUMENT
    for reserved_sched in [6u32, 42, 1023] {
        assert!(!ScheduleId(reserved_sched).is_valid());
        assert_eq!(
            sys.with_schedule(ScheduleId(reserved_sched)).validate(),
            FfiResult::INVALID_ARGUMENT
        );
    }
    for valid_sched in [0u32, 1, 2, 3, 4, 5, 1024, 2048] {
        assert!(ScheduleId(valid_sched).is_valid());
    }

    // Exceeding MAX_QUERIES_PER_SYSTEM -> CAPACITY_EXCEEDED
    let mut too_many_queries = sys;
    too_many_queries.query_count = MAX_QUERIES_PER_SYSTEM + 1;
    assert_eq!(too_many_queries.validate(), FfiResult::CAPACITY_EXCEEDED);

    // ComponentDesc (`sparse-set + immutable + `unknown flag)
    let uuid = ComponentUuid::from_parts("combat_mod::Health", 4, 4, 0);
    let comp_desc = ComponentDesc::new(c"combat_mod::Health".as_ptr(), uuid, 4, 4)
        .with_flags(BAM_COMPONENT_FLAG_SPARSE_SET | BAM_COMPONENT_FLAG_IMMUTABLE)
        .with_drop(stub_drop);
    assert!(comp_desc.is_sparse_set());
    assert!(comp_desc.is_immutable());
    assert!(!comp_desc.is_pod());
    assert!(!comp_desc.is_zst());
    assert!(comp_desc.is_valid());
    // SAFETY: `comp_desc.name` points to static C literal `c"combat_mod::Health"`.
    let c_name = unsafe { comp_desc.name_cstr() };
    assert_eq!(c_name.unwrap().to_bytes(), b"combat_mod::Health");

    // Summary tick on Table component succeeds; SPARSE_SET | SUMMARY_TICK -> INVALID_ARGUMENT; unknown bit -> UNSUPPORTED
    let summary_desc = ComponentDesc::new(c"combat_mod::FastTick".as_ptr(), uuid, 4, 4)
        .with_flags(BAM_COMPONENT_FLAG_SUMMARY_TICK);
    assert!(summary_desc.is_summary_tick());
    assert_eq!(summary_desc.validate(), FfiResult::OK);
    assert_eq!(
        comp_desc
            .with_flags(BAM_COMPONENT_FLAG_SPARSE_SET | BAM_COMPONENT_FLAG_SUMMARY_TICK)
            .validate(),
        FfiResult::INVALID_ARGUMENT
    );
    assert_eq!(
        comp_desc.with_flags(1 << 3).validate(),
        FfiResult::UNSUPPORTED
    );

    // ZST marker component (`size = 0, align = 1`) and over-aligned SIMD (`size = 32, align = 32`)
    let zst_desc = ComponentDesc::new(
        c"combat_mod::BossTag".as_ptr(),
        ComponentUuid::from_parts("combat_mod::BossTag", 0, 1, 0),
        0,
        1,
    );
    assert!(zst_desc.is_zst());
    assert!(zst_desc.is_pod());
    assert_eq!(zst_desc.validate(), FfiResult::OK);

    let simd_desc = ComponentDesc::new(
        c"combat_mod::SimdVec".as_ptr(),
        ComponentUuid::from_parts("combat_mod::SimdVec", 32, 32, 0),
        32,
        32,
    );
    assert_eq!(simd_desc.validate(), FfiResult::OK);

    // Invalid size/align (`align = 0`, `align = 3`, `size = 6` with `align = 4`)
    assert_eq!(
        ComponentDesc::new(c"bad".as_ptr(), uuid, 4, 0).validate(),
        FfiResult::INVALID_ARGUMENT
    );
    assert_eq!(
        ComponentDesc::new(c"bad".as_ptr(), uuid, 6, 4).validate(),
        FfiResult::INVALID_ARGUMENT
    );
}

#[test]
fn siphash24_128_vectors_and_component_uuid() {
    // Official Aumasson & Bernstein SipHash-2-4-128 test vectors for key 000102..0f:
    let k0 = 0x0706_0504_0302_0100u64;
    let k1 = 0x0f0e_0d0c_0b0a_0908u64;

    let mut input = [0u8; 64];
    for (idx, b) in input.iter_mut().enumerate() {
        *b = idx as u8;
    }

    assert_eq!(
        siphash24_128(&input[..0], k0, k1),
        [
            0xa3, 0x81, 0x7f, 0x04, 0xba, 0x25, 0xa8, 0xe6, 0x6d, 0xf6, 0x72, 0x14, 0xc7, 0x55,
            0x02, 0x93
        ]
    );
    assert_eq!(
        siphash24_128(&input[..1], k0, k1),
        [
            0xda, 0x87, 0xc1, 0xd8, 0x6b, 0x99, 0xaf, 0x44, 0x34, 0x76, 0x59, 0x11, 0x9b, 0x22,
            0xfc, 0x45
        ]
    );
    assert_eq!(
        siphash24_128(&input[..8], k0, k1),
        [
            0x3b, 0x62, 0xa9, 0xba, 0x62, 0x58, 0xf5, 0x61, 0x0f, 0x83, 0xe2, 0x64, 0xf3, 0x14,
            0x97, 0xb4
        ]
    );
    assert_eq!(
        siphash24_128(&input[..15], k0, k1),
        [
            0x54, 0x93, 0xe9, 0x99, 0x33, 0xb0, 0xa8, 0x11, 0x7e, 0x08, 0xec, 0x0f, 0x97, 0xcf,
            0xc3, 0xd9
        ]
    );
    assert_eq!(
        siphash24_128(&input[..63], k0, k1),
        [
            0x51, 0x50, 0xd1, 0x77, 0x2f, 0x50, 0x83, 0x4a, 0x50, 0x3e, 0x06, 0x9a, 0x97, 0x3f,
            0xbd, 0x7c
        ]
    );

    // Streaming `ComponentUuid::from_parts` must match hashing the concatenated payload in one shot.
    let name = "my_game::Position";
    let schema = ComponentUuid::hash_field_schema(
        ComponentUuid::hash_field_schema(0, "x", "f32", 0),
        "y",
        "f32",
        4,
    );
    let streamed = ComponentUuid::from_parts(name, 8, 4, schema);
    assert_eq!(streamed, ComponentUuid::from_namespaced(name, 8, 4, schema));
    assert!(!streamed.is_zero());
    assert!(ComponentUuid::ZERO.is_zero());

    let mut flat = [0u8; 17 + 1 + 24];
    flat[..17].copy_from_slice(name.as_bytes());
    flat[17] = 0xFF;
    flat[18..26].copy_from_slice(&8u64.to_le_bytes());
    flat[26..34].copy_from_slice(&4u64.to_le_bytes());
    flat[34..42].copy_from_slice(&schema.to_le_bytes());
    assert_eq!(
        streamed.bytes,
        siphash24_128(&flat, SIPHASH_KEY_0, SIPHASH_KEY_1)
    );

    // Cross-mod namespace isolation & layout sensitivity:
    let mod_a_health = ComponentUuid::from_parts("mod_a::Health", 4, 4, 0);
    let mod_b_health = ComponentUuid::from_parts("mod_b::Health", 4, 4, 0);
    let mod_a_health_wide = ComponentUuid::from_parts("mod_a::Health", 8, 8, 0);
    let mod_a_health_schema = ComponentUuid::from_parts("mod_a::Health", 4, 4, 1);
    assert_ne!(mod_a_health, mod_b_health);
    assert_ne!(mod_a_health, mod_a_health_wide);
    assert_ne!(mod_a_health, mod_a_health_schema);
    assert_eq!(
        ComponentUuid::from_canonical_name("my_game::Inventory"),
        ComponentUuid::from_parts("my_game::Inventory", 0, 0, 0)
    );

    // Explicit UUID hex parsing & `u128` round-trip:
    let parsed_hyphen = ComponentUuid::parse_hex("01234567-89ab-cdef-FEDC-9876543210CD");
    let parsed_compact = ComponentUuid::parse_hex("0123456789abcdefFEDC9876543210CD");
    assert_eq!(parsed_hyphen, parsed_compact);
    assert_eq!(
        parsed_hyphen.to_u128(),
        0x0123_4567_89ab_cdef_fedc_9876_5432_10cd
    );
    assert_eq!(
        ComponentUuid::from_u128(parsed_hyphen.to_u128()),
        parsed_hyphen
    );

    assert!(ComponentUuid::try_parse_hex("too-short").is_none());
    assert!(ComponentUuid::try_parse_hex("0123456789abcdef-fedc9876543210cdxxxx").is_none());
    assert!(ComponentUuid::try_parse_hex("01234567-89ab-cdef-fedc-9876543210cg").is_none());
}

#[test]
fn abi_pod_and_vtable_header_and_column_info() {
    // Permitted cross-language `AbiPod` primitives & newtypes:
    assert_abi_pod::<u8>();
    assert_abi_pod::<i8>();
    assert_abi_pod::<u16>();
    assert_abi_pod::<i16>();
    assert_abi_pod::<u32>();
    assert_abi_pod::<i32>();
    assert_abi_pod::<u64>();
    assert_abi_pod::<i64>();
    assert_abi_pod::<f32>();
    assert_abi_pod::<f64>();
    assert_abi_pod::<FfiBool>();
    assert_abi_pod::<BamComponent>();
    assert_abi_pod::<FfiEntity>();
    assert_abi_pod::<ComponentUuid>();
    assert_abi_pod::<[f32; 4]>();

    // Same-process `RawInsertable` includes platform/128-bit integers excluded from `AbiPod`:
    assert_raw_insertable::<u128>();
    assert_raw_insertable::<i128>();
    assert_raw_insertable::<usize>();
    assert_raw_insertable::<isize>();
    assert_raw_insertable::<[u128; 2]>();

    // Mock `ModComponent` with `VTableHeader` at offset 0:
    #[repr(C)]
    #[derive(Copy, Clone)]
    struct MockPos {
        x: f32,
        y: f32,
    }

    #[repr(C)]
    #[derive(Copy, Clone)]
    struct MockPosVTable {
        header: VTableHeader,
        get_x: Option<unsafe extern "C" fn(*const MockPos) -> f32>,
    }

    unsafe extern "C" fn mock_pos_get_x(ptr: *const MockPos) -> f32 {
        // SAFETY: Validated non-null `MockPos` pointer in test.
        unsafe { (*ptr).x }
    }

    const MOCK_UUID: ComponentUuid = ComponentUuid::from_parts("test::MockPos", 8, 4, 0);
    static MOCK_VTABLE: MockPosVTable = MockPosVTable {
        header: VTableHeader::new::<MockPosVTable, MockPos>(MOCK_UUID),
        get_x: Some(mock_pos_get_x),
    };
    static MOCK_SYMBOLS: [SymbolEntry; 1] = [SymbolEntry {
        name: "bam.v0.test::MockPos.vtable",
        ptr: (&raw const MOCK_VTABLE).cast::<c_void>(),
    }];

    impl ModComponent for MockPos {
        const CANONICAL_NAME: &'static str = "test::MockPos";
        const COMPONENT_CSTR: &'static core::ffi::CStr = c"test::MockPos";
        const UUID: ComponentUuid = MOCK_UUID;
        const IS_ABI_POD: bool = true;
        const IS_RAW_INSERTABLE: bool = true;
        type VTable = MockPosVTable;

        fn vtable() -> &'static Self::VTable {
            &MOCK_VTABLE
        }

        fn symbols() -> &'static [SymbolEntry] {
            &MOCK_SYMBOLS
        }
    }

    assert_eq!(
        MOCK_VTABLE
            .header
            .validate(size_of::<MockPosVTable>(), MOCK_UUID),
        FfiResult::OK
    );
    assert_eq!(
        MOCK_VTABLE.header.validate(
            size_of::<MockPosVTable>(),
            ComponentUuid::from_parts("other::MockPos", 8, 4, 0)
        ),
        FfiResult::TYPE_MISMATCH
    );
    assert_eq!(
        MOCK_VTABLE
            .header
            .validate(size_of::<MockPosVTable>() + 16, MOCK_UUID),
        FfiResult::INVALID_VERSION
    );
    assert_eq!(MockPos::size(), 8);
    assert_eq!(MockPos::align(), 4);
    assert_eq!(
        MockPos::resolve_symbol("bam.v0.test::MockPos.vtable"),
        Some(MockPos::erased_vtable_ptr())
    );
    assert_eq!(MockPos::resolve_symbol("bam.v0.unknown"), None);

    let pos = MockPos { x: 12.5, y: -3.0 };
    // SAFETY: `pos` is a valid initialized `MockPos` on the stack.
    let x = unsafe { (MockPos::vtable().get_x.unwrap())(&raw const pos) };
    assert_eq!(x, 12.5);

    // `BamColumnInfo` flags & layout helpers:
    let info = BamColumnInfo {
        component: BamComponent::from_raw(3),
        mode: AccessMode::READ,
        uuid: MOCK_UUID,
        size: 8,
        align: 4,
        storage_flags: BAM_STORAGE_FLAG_SPARSE_SET | BAM_STORAGE_FLAG_IMMUTABLE,
        reserved0: 0,
    };
    assert!(info.is_sparse_set());
    assert!(info.is_immutable());
    assert!(!info.is_resource());
    assert!(info.matches_layout(MOCK_UUID, 8, 4));
    assert!(!info.matches_layout(MOCK_UUID, 16, 4));

    let res_info = BamColumnInfo {
        storage_flags: BAM_STORAGE_FLAG_RESOURCE,
        ..info
    };
    assert!(res_info.is_resource());
    assert!(!res_info.is_message());

    let msg_info = BamColumnInfo {
        storage_flags: BAM_STORAGE_FLAG_MESSAGE,
        mode: AccessMode::MESSAGE_READ,
        ..info
    };
    assert!(msg_info.is_message());
    assert!(AccessMode::MESSAGE_READ.is_message());
    assert!(AccessMode::MESSAGE_WRITE.is_message());
    assert!(AccessMode::MESSAGE_WRITE.is_mutable());
    assert!(!AccessMode::MESSAGE_READ.is_mutable());

    let event_info = BamColumnInfo {
        storage_flags: BAM_STORAGE_FLAG_EVENT | BAM_STORAGE_FLAG_ENTITY_EVENT,
        ..info
    };
    assert!(event_info.is_event());
    assert!(event_info.is_entity_event());

    // `BamChangeTicks` wraparound-safe change detection:
    let ticks = BamChangeTicks {
        added: 10,
        changed: 12,
        last_run: 10,
        this_run: 15,
    };
    assert!(!ticks.is_added());
    assert!(ticks.is_changed());
    let wrapped = BamChangeTicks {
        added: 5,
        changed: 2,
        last_run: u32::MAX - 2,
        this_run: 8,
    };
    assert!(wrapped.is_added());
    assert!(wrapped.is_changed());

    // `BamObserverDesc` validation & entity packing:
    unsafe extern "C" fn stub_obs(
        _entity: FfiEntity,
        _event_data: *const c_void,
        _commands: *mut BamCommands,
        _user_data: *mut c_void,
    ) -> FfiResult {
        FfiResult::OK
    }
    let obs = BamObserverDesc::new(BamComponent::from_raw(7), stub_obs, core::ptr::null_mut())
        .with_name(c"test_obs".as_ptr())
        .with_entity(FfiEntity::from_bits(0xDEAD_BEEF_CAFE_BABE));
    assert_eq!(obs.validate(), FfiResult::OK);
    assert_eq!(obs.entity().to_bits(), 0xDEAD_BEEF_CAFE_BABE);

    // `FfiChildOf` Pod invariants:
    assert_abi_pod::<FfiChildOf>();
    assert_abi_pod::<BamChangeTicks>();
    assert_eq!(
        FfiChildOf::new(FfiEntity::from_bits(99)).parent.to_bits(),
        99
    );
}

#[test]
fn min_v0_2_backwards_compatibility_for_manifest_component_desc_and_host_api() {
    #[repr(C)]
    #[derive(Copy, Clone)]
    struct MinimalV02Manifest {
        struct_size: u32,
        abi_version: u32,
        name: *const c_char,
        version: *const c_char,
        flags: u32,
        reserved0: u32,
    }
    assert_eq!(size_of::<MinimalV02Manifest>(), BamManifest::MIN_V0_2_SIZE);

    let legacy_manifest = MinimalV02Manifest {
        struct_size: size_of::<MinimalV02Manifest>() as u32,
        abi_version: ABI_VERSION,
        name: c"legacy_min_v0_2_mod".as_ptr(),
        version: c"0.2.0".as_ptr(),
        flags: 0,
        reserved0: 0,
    };
    // SAFETY: `legacy_manifest` is initialized on the local stack with `MIN_V0_2_SIZE` bytes.
    let widened_manifest =
        unsafe { read_descriptor::<BamManifest>((&raw const legacy_manifest).cast::<c_void>()) }
            .expect("MIN_V0_2_SIZE BamManifest must be accepted");
    assert_eq!(widened_manifest.validate(), FfiResult::OK);
    assert!(widened_manifest.save_state.is_none());
    assert!(widened_manifest.restore_state.is_none());
    assert!(!widened_manifest.covers_slot(
        offset_of!(BamManifest, save_state),
        size_of::<Option<BamSaveStateFn>>(),
    ));

    #[repr(C)]
    #[derive(Copy, Clone)]
    struct MinimalV02ComponentDesc {
        struct_size: u32,
        flags: u32,
        name: *const c_char,
        uuid: ComponentUuid,
        size: usize,
        align: usize,
        drop_fn: Option<crate::ComponentDropFn>,
        vtable_ptr: *const c_void,
    }
    assert_eq!(
        size_of::<MinimalV02ComponentDesc>(),
        ComponentDesc::MIN_V0_2_SIZE
    );

    let legacy_comp = MinimalV02ComponentDesc {
        struct_size: size_of::<MinimalV02ComponentDesc>() as u32,
        flags: 0,
        name: c"legacy::Comp".as_ptr(),
        uuid: ComponentUuid::from_parts("legacy::Comp", 4, 4, 0),
        size: 4,
        align: 4,
        drop_fn: None,
        vtable_ptr: core::ptr::null(),
    };
    // SAFETY: `legacy_comp` is initialized on the local stack with `MIN_V0_2_SIZE` bytes.
    let widened_comp =
        unsafe { read_descriptor::<ComponentDesc>((&raw const legacy_comp).cast::<c_void>()) }
            .expect("MIN_V0_2_SIZE ComponentDesc must be accepted");
    assert_eq!(widened_comp.validate(), FfiResult::OK);
    assert!(!widened_comp.has_hooks());
    assert!(!widened_comp.covers_slot(
        offset_of!(ComponentDesc, on_add),
        size_of::<Option<ComponentHookFn>>(),
    ));
}

#[test]
fn ffi_slice_and_str_view_null_zero_and_alignment_safety() {
    // Empty C slice `(NULL, 0)` must be Miri-clean without UB in `core::slice::from_raw_parts`.
    let c_empty: FfiSlice<u32> = FfiSlice {
        ptr: core::ptr::null(),
        len: 0,
    };
    assert!(c_empty.is_empty());
    assert_eq!(c_empty.validate(), FfiResult::OK);
    // SAFETY: `len == 0` substitutes `NonNull::dangling()`.
    let empty_ref = unsafe { c_empty.try_as_slice() }.unwrap();
    assert!(empty_ref.is_empty());

    // Non-empty null slice -> NULL_POINTER
    let null_non_empty: FfiSlice<u32> = FfiSlice {
        ptr: core::ptr::null(),
        len: 2,
    };
    // SAFETY: `try_as_slice` rejects null pointers before dereferencing.
    let null_res = unsafe { null_non_empty.try_as_slice() };
    assert_eq!(null_res.unwrap_err(), FfiResult::NULL_POINTER);

    // Unaligned pointer -> INVALID_ARGUMENT
    let aligned_words = [0x1122_3344u32, 0x5566_7788u32];
    let unaligned_ptr = aligned_words.as_ptr().wrapping_byte_add(1);
    let unaligned_slice = FfiSlice {
        ptr: unaligned_ptr,
        len: 1,
    };
    // SAFETY: `try_as_slice` rejects unaligned pointers before dereferencing.
    let unaligned_res = unsafe { unaligned_slice.try_as_slice() };
    assert_eq!(unaligned_res.unwrap_err(), FfiResult::INVALID_ARGUMENT);

    // Invalid UTF-8 in `StrView` -> INVALID_UTF8
    let bad_utf8 = [0xffu8, 0xfe];
    let bad_view = StrView::from_slice(&bad_utf8);
    // SAFETY: `bad_utf8` is initialized on the local stack.
    let bad_utf8_res = unsafe { bad_view.try_as_str() };
    assert_eq!(bad_utf8_res.unwrap_err(), FfiResult::INVALID_UTF8);
}

#[test]
fn ffi_option_asset_handles_triggers_lifecycle_observers_and_modes() {
    // 1. FfiOption<T>
    assert_abi_pod::<FfiOption<u32>>();
    assert_abi_pod::<FfiOption<FfiEntity>>();
    assert_raw_insertable::<FfiOption<usize>>();

    let mut opt_some = FfiOption::some(77u32);
    assert!(opt_some.is_some());
    assert!(!opt_some.is_none());
    assert_eq!(opt_some.as_ref(), Some(&77));
    if let Some(slot) = opt_some.as_mut() {
        *slot = 88;
    }
    assert_eq!(opt_some.into_option(), Some(88));

    let opt_none = FfiOption::<u32>::none();
    assert!(opt_none.is_none());
    assert_eq!(opt_none.as_ref(), None);
    assert_eq!(opt_none.into_option(), None);
    assert_eq!(FfiOption::<u32>::default(), opt_none);
    assert_eq!(FfiOption::from_option(Some(5u32)), FfiOption::some(5u32));
    assert_eq!(FfiOption::<u32>::from_option(None), opt_none);

    // 2. FfiAssetHandle & FfiAssetLoadState
    assert_abi_pod::<FfiAssetHandle>();
    assert_abi_pod::<FfiAssetLoadState>();
    let h_none = FfiAssetHandle::default();
    assert!(h_none.is_none());
    assert!(!h_none.is_some());
    assert_eq!(h_none.validate(), FfiResult::OK);
    assert_eq!(h_none.to_index_bits(), None);

    let asset_ty = BamComponent::from_raw(9);
    let h_idx = FfiAssetHandle::from_index(asset_ty, 12, 3);
    assert!(h_idx.is_some());
    assert_eq!(h_idx.validate(), FfiResult::OK);
    let bits = h_idx.to_index_bits().unwrap();
    assert_eq!(FfiAssetHandle::from_index_bits(asset_ty, bits), h_idx);

    let h_uuid = FfiAssetHandle::from_uuid(
        asset_ty,
        ComponentUuid::parse_hex("00112233-4455-6677-8899-aabbccddeeff"),
    );
    assert!(h_uuid.is_some());
    assert_eq!(h_uuid.validate(), FfiResult::OK);

    let mut bad_handle = h_idx;
    bad_handle.asset_type = BamComponent::INVALID;
    assert_eq!(bad_handle.validate(), FfiResult::INVALID_COMPONENT);
    bad_handle = h_idx;
    bad_handle.reserved0[1] = 1;
    assert_eq!(bad_handle.validate(), FfiResult::INVALID_ARGUMENT);
    bad_handle = h_idx;
    bad_handle.kind = 99;
    assert_eq!(bad_handle.validate(), FfiResult::INVALID_ARGUMENT);

    assert!(FfiAssetLoadState::NOT_LOADED.is_valid());
    assert!(FfiAssetLoadState::LOADING.is_valid());
    assert!(FfiAssetLoadState::LOADED.is_loaded());
    assert!(FfiAssetLoadState::FAILED.is_failed());
    assert!(!FfiAssetLoadState(4).is_valid());

    // 3. AccessMode ALLOW / HAS / REMOVED_READ & BamTermDesc or_group
    assert!(AccessMode::ALLOW.is_valid());
    assert!(AccessMode::ALLOW.is_allow());
    assert!(!AccessMode::ALLOW.has_entity_column_data());
    assert!(AccessMode::HAS.is_valid());
    assert!(AccessMode::HAS.is_has());
    assert!(!AccessMode::HAS.has_entity_column_data());
    assert!(AccessMode::REMOVED_READ.is_valid());
    assert!(AccessMode::REMOVED_READ.is_removed_read());
    assert!(!AccessMode(15).is_valid());

    let or_term = BamTermDesc::filter_only(asset_ty).with_or_group(2);
    assert_eq!(or_term.or_group, 2);
    assert_eq!(or_term.validate(), FfiResult::OK);
    let mut bad_res_term = or_term;
    bad_res_term.reserved0 = 1;
    assert_eq!(bad_res_term.validate(), FfiResult::INVALID_ARGUMENT);

    // 4. Lifecycle & mutable BamObserverDesc + BamTriggerDesc
    unsafe extern "C" fn stub_obs_ro(
        _entity: FfiEntity,
        _event_data: *const c_void,
        _commands: *mut BamCommands,
        _user_data: *mut c_void,
    ) -> FfiResult {
        FfiResult::OK
    }
    unsafe extern "C" fn stub_obs_mut(
        _entity: FfiEntity,
        _event_data: *mut u8,
        _propagate: *mut FfiBool,
        _commands: *mut BamCommands,
        _user_data: *mut c_void,
    ) -> FfiResult {
        FfiResult::OK
    }

    let watched = [BamComponent::from_raw(2), BamComponent::from_raw(3)];
    let life_obs = BamObserverDesc::new_lifecycle(
        BamEventKind::ADD,
        watched.as_ptr(),
        watched.len(),
        stub_obs_ro,
        core::ptr::null_mut(),
    );
    assert_eq!(life_obs.validate(), FfiResult::OK);

    let ready_obs = BamObserverDesc::new_lifecycle(
        BamEventKind::READY,
        core::ptr::null(),
        0,
        stub_obs_ro,
        core::ptr::null_mut(),
    );
    assert_eq!(ready_obs.validate(), FfiResult::OK);

    let mut_obs = BamObserverDesc::new_mut(asset_ty, stub_obs_mut, core::ptr::null_mut())
        .with_propagate(BamComponent::from_raw(1));
    assert!(mut_obs.is_propagating());
    assert_eq!(mut_obs.flags, BAM_OBSERVER_FLAG_PROPAGATE);
    assert_eq!(mut_obs.validate(), FfiResult::OK);

    let mut payload = 42u32;
    let trig = BamTriggerDesc::new(asset_ty, (&raw mut payload).cast::<u8>(), size_of::<u32>())
        .with_entity(FfiEntity::from_bits(1234))
        .with_propagate(BamComponent::INVALID);
    assert!(trig.is_propagating());
    assert_eq!(trig.flags, BAM_TRIGGER_FLAG_PROPAGATE);
    assert_eq!(trig.entity().to_bits(), 1234);
    assert_eq!(trig.validate(), FfiResult::OK);

    // 5. Extended BamSystemDesc & BamColumnInfo summary_tick
    let weak_sets = [c"host::Physics".as_ptr()];
    let sys_ext = BamSystemDesc::new(
        c"ext_sys".as_ptr(),
        core::ptr::null(),
        0,
        stub_system_cb,
        core::ptr::null_mut(),
    )
    .with_ambiguous_with_all()
    .with_after_weak(weak_sets.as_ptr(), weak_sets.len())
    .with_before_weak(weak_sets.as_ptr(), weak_sets.len())
    .with_ambiguous_with(weak_sets.as_ptr(), weak_sets.len());
    assert!(sys_ext.is_ambiguous_with_all());
    assert_eq!(sys_ext.flags, BAM_SYSTEM_FLAG_AMBIGUOUS_WITH_ALL);
    assert_eq!(sys_ext.validate(), FfiResult::OK);

    let col_summary = BamColumnInfo {
        component: asset_ty,
        mode: AccessMode::READ,
        uuid: ComponentUuid::ZERO,
        size: 4,
        align: 4,
        storage_flags: BAM_STORAGE_FLAG_SUMMARY_TICK,
        reserved0: 0,
    };
    assert!(col_summary.is_summary_tick());
}

#[test]
fn abi_v0_3_extension_layouts_and_validation() {
    // 1. Typed handles (`BamSystem`, `BamSystemSet`, `BamSymbol`)
    assert_abi_pod::<BamSystem>();
    assert_abi_pod::<BamSystemSet>();
    assert_abi_pod::<BamSymbol>();
    assert_abi_pod::<BamFieldKind>();
    assert_eq!(BamSystem::default(), BamSystem::INVALID);
    assert_eq!(BamSystemSet::default(), BamSystemSet::INVALID);
    assert_eq!(BamSymbol::default(), BamSymbol::INVALID);
    assert_eq!(size_of::<BamSystem>(), 4);
    assert_eq!(size_of::<BamSystemSet>(), 4);
    assert_eq!(size_of::<BamSymbol>(), 4);

    // 2. Log level constants & FNV-1a determinism
    assert_eq!(
        [
            BAM_LOG_TRACE,
            BAM_LOG_DEBUG,
            BAM_LOG_INFO,
            BAM_LOG_WARN,
            BAM_LOG_ERROR
        ],
        [0, 1, 2, 3, 4]
    );
    assert_eq!(
        fnv1a_64_str("combat::Health"),
        fnv1a_64_update_bytes(fnv1a_64_init(), b"combat::Health")
    );
    assert_ne!(
        fnv1a_64_str("combat::Health"),
        fnv1a_64_str("combat::HealthV2")
    );

    // 3. Pointer-width ABI 0.3 struct sizes and offsets
    #[cfg(target_pointer_width = "64")]
    {
        assert_eq!(size_of::<BamFieldDesc>(), 64);
        assert_eq!(size_of::<BamRequiredComponent>(), 64);
        assert_eq!(size_of::<BamRelationDesc>(), 56);
        assert_eq!(size_of::<BamLayoutInfo>(), 64);
        assert_eq!(size_of::<BamScheduleSetDesc>(), 48);
        assert_eq!(offset_of!(ComponentDesc, schema_fingerprint), 136);
        assert_eq!(offset_of!(BamLayoutInfo, schema_fingerprint), 24);
    }
    #[cfg(target_pointer_width = "32")]
    {
        assert_eq!(size_of::<BamFieldDesc>(), 36);
        assert_eq!(size_of::<BamRequiredComponent>(), 48);
        assert_eq!(size_of::<BamRelationDesc>(), 44);
        assert_eq!(size_of::<BamLayoutInfo>(), 48);
        assert_eq!(size_of::<BamScheduleSetDesc>(), 36);
        assert_eq!(offset_of!(ComponentDesc, schema_fingerprint), 80);
        assert_eq!(offset_of!(BamLayoutInfo, schema_fingerprint), 24);
    }

    // 4. BamFieldDesc & BamRequiredComponent constructors
    let field = BamFieldDesc::new(
        BamFieldKind::F32,
        StrView::from_str("hp"),
        StrView::from_str("f32"),
        0,
        4,
        4,
    );
    assert_eq!(field.struct_size as usize, size_of::<BamFieldDesc>());

    static INIT_BYTES: [u8; 4] = [0x01, 0x02, 0x03, 0x04];
    unsafe extern "C" fn stub_req_init(_dst: *mut c_void, _ud: *mut c_void) -> FfiResult {
        FfiResult::OK
    }
    let req_bytes = BamRequiredComponent::by_uuid(ComponentUuid::from_parts("dep::Tag", 4, 4, 0))
        .with_custom_bytes(&INIT_BYTES);
    assert_eq!(
        req_bytes.flags,
        BAM_REQUIRED_FLAG_RESOLVE_BY_UUID | BAM_REQUIRED_FLAG_CUSTOM_BYTES
    );
    let req_fn = BamRequiredComponent::by_handle(BamComponent::from_raw(5)).with_init_fn(
        stub_req_init,
        8,
        core::ptr::null_mut(),
    );
    assert_eq!(req_fn.flags, BAM_REQUIRED_FLAG_INIT_FN);

    // 5. BamRelationDesc & BamScheduleSetDesc validation
    assert_eq!(
        BAM_RELATION_FLAG_CASCADE_DELETE,
        BAM_RELATION_FLAG_CASCADE_DESPAWN
    );
    assert_eq!(BAM_RELATION_KNOWN_FLAGS, 0x1F);
    assert_eq!(BAM_SCHEDULE_SET_KNOWN_FLAGS, 0x1F);

    let rel_uuid = ComponentUuid::from_parts("combat::TargetOf", 8, 8, 0);
    let rel = BamRelationDesc::new(c"combat::TargetOf".as_ptr(), rel_uuid)
        .with_flags(
            BAM_RELATION_FLAG_EXCLUSIVE
                | BAM_RELATION_FLAG_CASCADE_DELETE
                | BAM_RELATION_FLAG_ACYCLIC
                | BAM_RELATION_FLAG_CLEANUP_REMOVE,
        )
        .with_payload_layout(8, 8);
    assert_eq!(rel.validate(), FfiResult::OK);
    let bad_rel = BamRelationDesc::new(c"combat::TargetOf".as_ptr(), rel_uuid)
        .with_flags(BAM_RELATION_FLAG_SYMMETRIC | (1 << 31));
    assert_eq!(bad_rel.validate(), FfiResult::UNSUPPORTED);
    let mut bad_align = rel;
    bad_align.data_align = 3;
    assert_eq!(bad_align.validate(), FfiResult::INVALID_ARGUMENT);

    let set_ok = BamScheduleSetDesc::new(c"combat::CombatSet".as_ptr());
    assert_eq!(set_ok.validate(), FfiResult::OK);
    assert_eq!(
        set_ok
            .with_flags(
                BAM_SCHEDULE_SET_FLAG_SEQUENCE
                    | BAM_SCHEDULE_SET_FLAG_BEFORE_WEAK
                    | BAM_SCHEDULE_SET_FLAG_AFTER_WEAK
            )
            .validate(),
        FfiResult::OK
    );
    assert_eq!(
        set_ok
            .with_flags(BAM_SCHEDULE_SET_FLAG_SEQUENCE_WEAK)
            .validate(),
        FfiResult::OK
    );
    assert_eq!(
        set_ok.with_flags(BAM_SCHEDULE_SET_FLAG_PARALLEL).validate(),
        FfiResult::OK
    );
    assert_eq!(
        set_ok
            .with_flags(BAM_SCHEDULE_SET_FLAG_SEQUENCE | BAM_SCHEDULE_SET_FLAG_SEQUENCE_WEAK)
            .validate(),
        FfiResult::INVALID_ARGUMENT
    );
    assert_eq!(
        set_ok
            .with_flags(BAM_SCHEDULE_SET_FLAG_SEQUENCE | BAM_SCHEDULE_SET_FLAG_PARALLEL)
            .validate(),
        FfiResult::INVALID_ARGUMENT
    );
    assert_eq!(
        set_ok
            .with_flags(BAM_SCHEDULE_SET_FLAG_SEQUENCE_WEAK | BAM_SCHEDULE_SET_FLAG_PARALLEL)
            .validate(),
        FfiResult::INVALID_ARGUMENT
    );
    assert_eq!(set_ok.with_flags(1 << 5).validate(), FfiResult::UNSUPPORTED);
    assert_eq!(
        set_ok.with_flags(1 << 31).validate(),
        FfiResult::UNSUPPORTED
    );
    let mut set_bad_edge = set_ok;
    set_bad_edge.before_count = 1;
    assert_eq!(set_bad_edge.validate(), FfiResult::NULL_POINTER);

    let layout = BamLayoutInfo::empty();
    assert_eq!(layout.struct_size as usize, size_of::<BamLayoutInfo>());

    // 6. BamHostApi v0.3 slot completeness & partial population
    let api = populated_host_api();
    assert!(!api.has_all_v0_3_slots());
    assert!(!api.has_all_slots());
}
