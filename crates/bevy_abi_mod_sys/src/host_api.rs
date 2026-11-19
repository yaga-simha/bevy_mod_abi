//! Complete `Option<fn>` `BamHostApi` table, `BamManifest`, and per-instance lifecycle typedefs.

use core::{
    ffi::{c_char, c_void},
    marker::{PhantomData, PhantomPinned},
};

use crate::{
    access::ScheduleId,
    commands::{
        BamCmdDespawnAllFn, BamCmdDespawnFn, BamCmdInsertDefaultFn, BamCmdInsertFn,
        BamCmdObserveFn, BamCmdRemoveFn, BamCmdSpawnBatchFn, BamCmdSpawnEmptyFn, BamCmdSpawnFn,
        BamCmdSpawnSceneFn, BamCmdTriggerExFn, BamCmdTriggerFn, BamCommands,
    },
    component::{
        BamLayoutInfo, BamRelationDesc, BamRowMigrateFn, BamScheduleSetDesc, ComponentDesc,
    },
    extensible::ExtensibleStruct,
    iter::{
        BamChangeTicks, BamIter, BamIterColumnInfoFn, BamIterColumnMutableFn,
        BamIterColumnMutableUntrackedFn, BamIterColumnReadonlyFn, BamIterColumnRowsMutableFn,
        BamIterColumnRowsReadonlyFn, BamIterEntitiesFn, BamIterMarkChangedRangeFn,
        BamIterNextChunkFn, BamIterReadRemovedFn, BamIterResetFn, BamMsgReadBatchFn, BamMsgSendFn,
        BamMsgWriteBatchFn, BamParForEachChunkFn, BamResMutPtrFn, BamResPtrFn, BamResTicksFn,
    },
    primitives::{
        BamComponent, BamSymbol, BamSystem, BamSystemSet, ComponentUuid, FfiAssetHandle,
        FfiAssetLoadState, FfiEntity, FfiResult, StrView,
    },
    system::{BamHostCtx, BamLoadCtx, BamObserverDesc, BamSystemCtx, BamSystemDesc},
    version::{ABI_VERSION, abi_version_is_compatible},
};

/// Opaque guest-owned per-World mod instance handle (`typedef struct BamInstance BamInstance;`).
///
/// Returned via `instance_out` from `bevy_abi_mod_init` and passed back to `bevy_abi_mod_shutdown`
/// when that specific load session unloads from its `World`.
#[repr(C)]
pub struct BamInstance {
    _private: [u8; 0],
    _marker: PhantomData<(*mut u8, PhantomPinned)>,
}

/// Host-provided byte-sink callback passed to [`BamSaveStateFn`] so serialized hot-reload
/// state bytes are copied directly into a host-allocated `Vec<u8>` without cross-ABI heap ownership.
///
/// # Safety
/// - `ctx` must be the opaque `writer_ctx` supplied by the host to `save_state`.
/// - When `len > 0`, `bytes` must point to `len` readable initialized bytes.
pub type BamStateWriterFn =
    unsafe extern "C" fn(ctx: *mut c_void, bytes: *const u8, len: usize) -> FfiResult;

/// Optional guest hot-reload state snapshot callback on [`BamManifest`].
///
/// Invoked by `world.reload_mod(id)` prior to `bevy_abi_mod_shutdown(instance)`.
///
/// # Safety
/// - `instance` is the `*mut BamInstance` returned by `bevy_abi_mod_init` for the active load session.
/// - `writer` and `writer_ctx` are valid strictly for the synchronous duration of `save_state`.
/// - The callback must never unwind across `extern "C"`.
pub type BamSaveStateFn = unsafe extern "C" fn(
    instance: *mut BamInstance,
    writer: Option<BamStateWriterFn>,
    writer_ctx: *mut c_void,
) -> FfiResult;

/// Optional guest hot-reload state restoration callback on [`BamManifest`].
///
/// Invoked by `world.reload_mod(id)` immediately after the replacement library's `bevy_abi_mod_init`
/// succeeds.
///
/// # Safety
/// - `instance` is the newly initialized `*mut BamInstance` for the reloaded session.
/// - When `len > 0`, `bytes` points to `len` readable bytes valid strictly for the synchronous
///   duration of `restore_state`.
/// - The callback must never unwind across `extern "C"`.
pub type BamRestoreStateFn =
    unsafe extern "C" fn(instance: *mut BamInstance, bytes: *const u8, len: usize) -> FfiResult;

/// Bitmask of recognized [`BamManifest::flags`] bits in ABI `0.2` (`0`).
pub const BAM_MANIFEST_KNOWN_FLAGS: u32 = 0;

/// Mandatory static mod identity manifest returned by `bevy_abi_mod_manifest()`
/// (`typedef struct BamManifest BamManifest;`).
///
/// Layout:
/// - 64-bit: `MIN_V0_2_SIZE = 32` bytes (`size_of = 48` bytes with optional state-transfer slots), `8`-byte alignment, zero implicit padding.
/// - 32-bit: `MIN_V0_2_SIZE = 24` bytes (`size_of = 32` bytes with optional state-transfer slots), `4`-byte alignment, zero implicit padding.
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct BamManifest {
    /// Size of this struct in bytes (`offset 0`).
    pub struct_size: u32,
    /// Packed guest ABI version (`ABI_VERSION`, `offset 4`).
    pub abi_version: u32,
    /// Canonical NUL-terminated UTF-8 mod identifier, e.g. `c"combat_mod"` (`offset 8`).
    pub name: *const c_char,
    /// NUL-terminated UTF-8 SemVer mod version string, e.g. `c"0.1.0"` (`offset 16` / `12`).
    pub version: *const c_char,
    /// Manifest bitflags (`0` in ABI `0.2`, `offset 24` / `16`).
    pub flags: u32,
    /// Explicit zero-reserved field (`must be 0`, `offset 28` / `20`).
    pub reserved0: u32,
    /// Optional hot-reload state snapshot callback (`offset 32` / `24`).
    pub save_state: Option<BamSaveStateFn>,
    /// Optional hot-reload state restoration callback (`offset 40` / `28`).
    pub restore_state: Option<BamRestoreStateFn>,
}

// SAFETY: `BamManifest` holds integer fields, `'static` C-string pointers, and optional `extern "C"`
// function pointers that require `unsafe` to dereference or invoke; sharing static `BamManifest`
// instances across threads cannot cause data races in safe Rust.
unsafe impl Send for BamManifest {}
// SAFETY: See `Send` implementation above; enables `static MANIFEST: BamManifest` in `export_mod!`.
unsafe impl Sync for BamManifest {}

impl BamManifest {
    /// Frozen minimum valid `BamManifest` size in ABI `0.2` (`32` on 64-bit, `24` on 32-bit).
    pub const MIN_V0_2_SIZE: usize =
        core::mem::offset_of!(Self, reserved0) + core::mem::size_of::<u32>();

    /// Constructs a `BamManifest` stamped with [`ABI_VERSION`] and zero flags.
    #[inline]
    pub const fn new(name: *const c_char, version: *const c_char) -> Self {
        Self {
            struct_size: core::mem::size_of::<Self>() as u32,
            abi_version: ABI_VERSION,
            name,
            version,
            flags: 0,
            reserved0: 0,
            save_state: None,
            restore_state: None,
        }
    }

    /// Returns `true` if `self.struct_size` covers a field of `slot_size` bytes at `slot_offset`.
    #[inline]
    pub const fn covers_slot(&self, slot_offset: usize, slot_size: usize) -> bool {
        match slot_offset.checked_add(slot_size) {
            Some(end) => (self.struct_size as usize) >= end,
            None => false,
        }
    }

    /// Sets `BAM_MANIFEST_*` bitflags on this manifest.
    #[inline]
    pub const fn with_flags(mut self, flags: u32) -> Self {
        self.flags = flags;
        self
    }

    /// Attaches an optional hot-reload `save_state` callback.
    #[inline]
    pub const fn with_save_state(mut self, save_state: BamSaveStateFn) -> Self {
        self.save_state = Some(save_state);
        self
    }

    /// Attaches an optional hot-reload `restore_state` callback.
    #[inline]
    pub const fn with_restore_state(mut self, restore_state: BamRestoreStateFn) -> Self {
        self.restore_state = Some(restore_state);
        self
    }

    /// Attaches both hot-reload `save_state` and `restore_state` callbacks.
    #[inline]
    pub const fn with_state_callbacks(
        mut self,
        save_state: BamSaveStateFn,
        restore_state: BamRestoreStateFn,
    ) -> Self {
        self.save_state = Some(save_state);
        self.restore_state = Some(restore_state);
        self
    }

    /// Validates `struct_size`, SemVer compatibility against `host_version`, `flags`,
    /// `reserved0 == 0`, and non-null `name`/`version` pointers.
    #[inline]
    pub const fn validate_for_host(&self, host_version: u32) -> FfiResult {
        if (self.struct_size as usize) < Self::MIN_V0_2_SIZE {
            return FfiResult::INVALID_VERSION;
        }
        if !abi_version_is_compatible(host_version, self.abi_version) {
            return FfiResult::INVALID_VERSION;
        }
        if (self.flags & !BAM_MANIFEST_KNOWN_FLAGS) != 0 {
            return FfiResult::UNSUPPORTED;
        }
        if self.reserved0 != 0 {
            return FfiResult::INVALID_ARGUMENT;
        }
        if self.name.is_null() || self.version.is_null() {
            return FfiResult::NULL_POINTER;
        }
        FfiResult::OK
    }

    /// Validates this manifest against the local [`ABI_VERSION`].
    #[inline]
    pub const fn validate(&self) -> FfiResult {
        self.validate_for_host(ABI_VERSION)
    }
}

const _: () = {
    assert!(BamManifest::MIN_V0_2_SIZE <= core::mem::size_of::<BamManifest>());
    #[cfg(target_pointer_width = "64")]
    {
        assert!(BamManifest::MIN_V0_2_SIZE == 32);
        assert!(core::mem::size_of::<BamManifest>() == 48);
        assert!(core::mem::align_of::<BamManifest>() == 8);
        assert!(core::mem::offset_of!(BamManifest, struct_size) == 0);
        assert!(core::mem::offset_of!(BamManifest, abi_version) == 4);
        assert!(core::mem::offset_of!(BamManifest, name) == 8);
        assert!(core::mem::offset_of!(BamManifest, version) == 16);
        assert!(core::mem::offset_of!(BamManifest, flags) == 24);
        assert!(core::mem::offset_of!(BamManifest, reserved0) == 28);
        assert!(core::mem::offset_of!(BamManifest, save_state) == 32);
        assert!(core::mem::offset_of!(BamManifest, restore_state) == 40);
    }
    #[cfg(target_pointer_width = "32")]
    {
        assert!(BamManifest::MIN_V0_2_SIZE == 24);
        assert!(core::mem::size_of::<BamManifest>() == 32);
        assert!(core::mem::align_of::<BamManifest>() == 4);
        assert!(core::mem::offset_of!(BamManifest, struct_size) == 0);
        assert!(core::mem::offset_of!(BamManifest, abi_version) == 4);
        assert!(core::mem::offset_of!(BamManifest, name) == 8);
        assert!(core::mem::offset_of!(BamManifest, version) == 12);
        assert!(core::mem::offset_of!(BamManifest, flags) == 16);
        assert!(core::mem::offset_of!(BamManifest, reserved0) == 20);
        assert!(core::mem::offset_of!(BamManifest, save_state) == 24);
        assert!(core::mem::offset_of!(BamManifest, restore_state) == 28);
    }
};

// SAFETY: `BamManifest` is `#[repr(C)]`, starts with `struct_size: u32` at offset 0, and all
// fields are valid when zero-filled.
unsafe impl ExtensibleStruct for BamManifest {
    const MIN_V0_2_SIZE: usize = Self::MIN_V0_2_SIZE;
}

/// Mandatory guest manifest entry-point signature (`bevy_abi_mod_manifest`).
///
/// # Safety
/// Must be side-effect free and return a pointer to a static [`BamManifest`] valid until `dlclose`.
pub type BamManifestFn = unsafe extern "C" fn() -> *const BamManifest;

/// Mandatory guest per-World initialization entry-point signature (`bevy_abi_mod_init`).
///
/// # Safety
/// - `api` must point to a valid `'static` [`BamHostApi`] table.
/// - `load_ctx` is valid only for the duration of `bevy_abi_mod_init`.
/// - `instance_out` must point to writable `*mut BamInstance` storage; stateless C99 mods may
///   leave `*instance_out = null` on success, while stateful mods store a guest-heap pointer freed
///   in `bevy_abi_mod_shutdown`.
pub type BamInitFn = unsafe extern "C" fn(
    api: *const BamHostApi,
    load_ctx: *mut BamLoadCtx,
    instance_out: *mut *mut BamInstance,
) -> FfiResult;

/// Mandatory guest per-World teardown entry-point signature (`bevy_abi_mod_shutdown`).
///
/// # Safety
/// Invoked once per successful `bevy_abi_mod_init` with the `*mut BamInstance` returned by that
/// load session (`null` is a defined no-op) while the shared library remains mapped.
pub type BamShutdownFn = unsafe extern "C" fn(instance: *mut BamInstance);

// --- 1. Diagnostics & Context-Carrying Custom Symbol Discovery ---

/// Resolves component VTables and custom game symbols namespaced `"bam.v0.<name>"` on `ctx`.
///
/// # Safety
/// - `ctx` must point to an active [`BamHostCtx`] for the target `World`.
/// - If non-null, `name` must point to a NUL-terminated UTF-8 C string.
pub type BamGetProcAddressFn =
    unsafe extern "C" fn(ctx: *const BamHostCtx, name: *const c_char) -> *const c_void;

/// Copies the most recent human-readable UTF-8 diagnostic message recorded on `ctx` into
/// `buf[0..cap]` (NUL-terminated when `buf != null && cap > 0`) and returns the full message byte
/// length excluding the trailing NUL (`snprintf` sizing convention).
///
/// # Safety
/// - `ctx` must point to an active [`BamHostCtx`].
/// - If `buf` is non-null and `cap > 0`, `buf` must point to at least `cap` writable bytes.
pub type BamLastErrorFn =
    unsafe extern "C" fn(ctx: *const BamHostCtx, buf: *mut c_char, cap: usize) -> usize;

// --- 2. Init-Time Registration & Handle Resolution ---

/// Obtains the world-scoped [`BamHostCtx`] pointer from an active [`BamLoadCtx`].
///
/// # Safety
/// `load_ctx` must point to the active [`BamLoadCtx`] supplied to `bevy_abi_mod_init`.
pub type BamLoadHostCtxFn = unsafe extern "C" fn(load_ctx: *const BamLoadCtx) -> *const BamHostCtx;

/// Registers a dynamic component described by `desc` inside `load_ctx` and writes its 1-based
/// [`BamComponent`] handle into `*out_component`.
///
/// # Safety
/// - `load_ctx` must point to the active [`BamLoadCtx`] during `bevy_abi_mod_init`.
/// - `desc` must point to a valid [`ComponentDesc`].
/// - `out_component` must point to writable [`BamComponent`] storage.
pub type BamRegisterComponentFn = unsafe extern "C" fn(
    load_ctx: *mut BamLoadCtx,
    desc: *const ComponentDesc,
    out_component: *mut BamComponent,
) -> FfiResult;

/// Resolves an existing component or resource by canonical `name`, `uuid`, or both (verifying
/// `entry.uuid == *uuid` when both are non-null, returning [`FfiResult::TYPE_MISMATCH`] on
/// layout/identity disagreement), writing its 1-based [`BamComponent`] handle into `*out_component`.
///
/// # Safety
/// - `ctx` must point to an active [`BamHostCtx`].
/// - If non-null, `name` must point to a NUL-terminated UTF-8 C string.
/// - If non-null, `uuid` must point to a readable [`ComponentUuid`].
/// - `out_component` must point to writable [`BamComponent`] storage.
pub type BamResolveComponentFn = unsafe extern "C" fn(
    ctx: *const BamHostCtx,
    name: *const c_char,
    uuid: *const ComponentUuid,
    out_component: *mut BamComponent,
) -> FfiResult;

/// Registers a unified [`BamSystemDesc`] inside `load_ctx`.
///
/// # Safety
/// - `load_ctx` must point to the active [`BamLoadCtx`] during `bevy_abi_mod_init`.
/// - `desc` must point to a valid [`BamSystemDesc`].
pub type BamRegisterSystemFn =
    unsafe extern "C" fn(load_ctx: *mut BamLoadCtx, desc: *const BamSystemDesc) -> FfiResult;

// --- 3. System Execution Context Accessors ---

/// Obtains the world-scoped [`BamHostCtx`] from an active [`BamSystemCtx`].
///
/// # Safety
/// `sys_ctx` must point to the active [`BamSystemCtx`] passed to [`crate::BamSystemFn`].
pub type BamSysHostCtxFn = unsafe extern "C" fn(sys_ctx: *const BamSystemCtx) -> *const BamHostCtx;

/// Borrows the [`BamIter`] handle for declared query `query_index` (`0..query_count`), writing it
/// to `*out_iter`.
///
/// # Safety
/// - `sys_ctx` must point to the active [`BamSystemCtx`] passed to [`crate::BamSystemFn`].
/// - `out_iter` must point to writable `*mut BamIter` storage.
pub type BamSysQueryFn = unsafe extern "C" fn(
    sys_ctx: *mut BamSystemCtx,
    query_index: usize,
    out_iter: *mut *mut BamIter,
) -> FfiResult;

/// Borrows the per-system [`BamCommands`] deferred command buffer from `sys_ctx`, writing it to
/// `*out_commands`.
///
/// # Safety
/// - `sys_ctx` must point to the active [`BamSystemCtx`] passed to [`crate::BamSystemFn`].
/// - `out_commands` must point to writable `*mut BamCommands` storage.
pub type BamSysCommandsFn = unsafe extern "C" fn(
    sys_ctx: *mut BamSystemCtx,
    out_commands: *mut *mut BamCommands,
) -> FfiResult;

/// Random O(1) read-only per-entity component lookup for `entity` under declared `query_index`
/// and `term_index`, returning null if `entity` does not match `query_index`'s archetype
/// filters, lacks the component, or `term_index` did not declare entity read/write access.
///
/// # Safety
/// `sys_ctx` must point to the active [`BamSystemCtx`], and the caller must not alias any active
/// mutable borrow of the same component on `entity`.
pub type BamSysEntityGetFn = unsafe extern "C" fn(
    sys_ctx: *const BamSystemCtx,
    query_index: usize,
    entity: FfiEntity,
    term_index: usize,
) -> *const u8;

/// Random O(1) mutable per-entity component lookup for `entity` under declared `query_index` and
/// `term_index`, marking `entity`'s component change tick and returning null if `entity`
/// does not match `query_index`'s archetype filters, lacks the component, or `term_index` did not
/// declare `WRITE` / `OPTIONAL_WRITE` access.
///
/// # Safety
/// `sys_ctx` must point to the active [`BamSystemCtx`], and the caller must not create overlapping
/// mutable or shared borrows to the same component on `entity`.
pub type BamSysEntityGetMutFn = unsafe extern "C" fn(
    sys_ctx: *mut BamSystemCtx,
    query_index: usize,
    entity: FfiEntity,
    term_index: usize,
) -> *mut u8;

/// Direct read-only resource lookup by pre-resolved [`BamComponent`] handle on `sys_ctx`,
/// optionally writing change-detection ticks into `*out_ticks` when non-null.
///
/// Returns null if `component` is not declared as `RESOURCE_READ` or `RESOURCE_WRITE` in the
/// system's `FilteredAccessSet` or if the resource does not exist in the `World`.
///
/// # Safety
/// - `sys_ctx` must point to an active [`BamSystemCtx`].
/// - If non-null, `out_ticks` must point to writable [`BamChangeTicks`] storage.
pub type BamSysResPtrFn = unsafe extern "C" fn(
    sys_ctx: *const BamSystemCtx,
    component: BamComponent,
    out_ticks: *mut BamChangeTicks,
) -> *const u8;

/// Direct exclusive mutable resource lookup by pre-resolved [`BamComponent`] handle on `sys_ctx`,
/// reading prior change-detection ticks into `*out_ticks` (when non-null) and marking the
/// resource's `changed` tick as modified in this run.
///
/// Returns null if `component` is not declared as `RESOURCE_WRITE` in the system's
/// `FilteredAccessSet`, is immutable, or does not exist in the `World`.
///
/// # Safety
/// - `sys_ctx` must point to an active [`BamSystemCtx`], and the caller must not create aliasing
///   references to the resource.
/// - If non-null, `out_ticks` must point to writable [`BamChangeTicks`] storage.
pub type BamSysResMutPtrFn = unsafe extern "C" fn(
    sys_ctx: *mut BamSystemCtx,
    component: BamComponent,
    out_ticks: *mut BamChangeTicks,
) -> *mut u8;

/// Registers a dynamic observer described by [`BamObserverDesc`] during `bevy_abi_mod_init`.
///
/// # Safety
/// - `load_ctx` must point to the active [`BamLoadCtx`] during `bevy_abi_mod_init`.
/// - `desc` must point to a valid [`BamObserverDesc`].
pub type BamRegisterObserverFn =
    unsafe extern "C" fn(load_ctx: *mut BamLoadCtx, desc: *const BamObserverDesc) -> FfiResult;

/// Borrows the contiguous slice of child or relationship-target [`FfiEntity`] identifiers attached
/// to `entity` under `relationship_target` (`BamComponent::INVALID` defaults to Bevy's built-in
/// `Children` component), writing the zero-copy pointer and element count into `*out_entities` and
/// `*out_len`.
///
/// When `entity` has zero children/targets, writes `*out_len = 0` and returns [`FfiResult::OK`]
/// (`*out_entities` may be null when `*out_len == 0`). When `*out_len > 0`, `*out_entities` is
/// non-null, 8-byte aligned, and valid for shared reads throughout the enclosing
/// [`BamSystemFn`](crate::BamSystemFn) invocation (or until a mutable borrow of the same component).
///
/// Requires declared shared read (or write) access to `Children` / `relationship_target` in the
/// running system's `FilteredAccessSet`.
///
/// # Safety
/// - `sys_ctx` must point to an active [`BamSystemCtx`].
/// - `out_entities` must point to writable `*const FfiEntity` storage.
/// - `out_len` must point to writable `usize` storage.
pub type BamSysEntityChildrenFn = unsafe extern "C" fn(
    sys_ctx: *const BamSystemCtx,
    entity: FfiEntity,
    relationship_target: BamComponent,
    out_entities: *mut *const FfiEntity,
    out_len: *mut usize,
) -> FfiResult;

/// Requests asynchronous asset loading for `path` (`StrView`, e.g. `"mod://combat/sword.png"`) of
/// pre-resolved `asset_type` (`BamComponent::INVALID` for untyped load when supported), retaining a
/// strong handle on the host and writing its [`FfiAssetHandle`] into `*out_handle`.
///
/// # Safety
/// - `ctx` must point to an active [`BamHostCtx`].
/// - `path` must point to valid UTF-8 bytes when `path.len > 0`.
/// - `out_handle` must point to writable [`FfiAssetHandle`] storage.
pub type BamAssetLoadFn = unsafe extern "C" fn(
    ctx: *const BamHostCtx,
    asset_type: BamComponent,
    path: StrView,
    out_handle: *mut FfiAssetHandle,
) -> FfiResult;

/// Queries the current [`FfiAssetLoadState`] of `handle` from the host `AssetServer`, writing it
/// into `*out_state`.
///
/// # Safety
/// - `ctx` must point to an active [`BamHostCtx`].
/// - `handle` must point to a readable [`FfiAssetHandle`].
/// - `out_state` must point to writable [`FfiAssetLoadState`] storage.
pub type BamAssetGetLoadStateFn = unsafe extern "C" fn(
    ctx: *const BamHostCtx,
    handle: *const FfiAssetHandle,
    out_state: *mut FfiAssetLoadState,
) -> FfiResult;

/// Releases the host-retained strong handle for `handle`.
///
/// # Safety
/// - `ctx` must point to an active [`BamHostCtx`].
/// - `handle` must point to a readable [`FfiAssetHandle`].
pub type BamAssetReleaseFn =
    unsafe extern "C" fn(ctx: *const BamHostCtx, handle: *const FfiAssetHandle) -> FfiResult;

/// Trace severity passed to [`BamLogDiagnosticFn`] (`0`).
pub const BAM_LOG_TRACE: u32 = 0;
/// Debug severity passed to [`BamLogDiagnosticFn`] (`1`).
pub const BAM_LOG_DEBUG: u32 = 1;
/// Info severity passed to [`BamLogDiagnosticFn`] (`2`).
pub const BAM_LOG_INFO: u32 = 2;
/// Warning severity passed to [`BamLogDiagnosticFn`] (`3`).
pub const BAM_LOG_WARN: u32 = 3;
/// Error severity passed to [`BamLogDiagnosticFn`] (`4`).
pub const BAM_LOG_ERROR: u32 = 4;

/// Emits a structured guest diagnostic message via the host `tracing` subscriber
/// (`typedef BamResult (*BamLogDiagnosticFn)(const BamHostCtx* ctx, uint32_t level, BamStrView message);`).
pub type BamLogDiagnosticFn =
    unsafe extern "C" fn(ctx: *const BamHostCtx, level: u32, message: StrView) -> FfiResult;

/// Records a guest-side fatal or recoverable fault on `ctx` without unwinding across FFI
/// (`typedef BamResult (*BamReportGuestFaultFn)(const BamHostCtx* ctx, BamStrView message);`).
pub type BamReportGuestFaultFn =
    unsafe extern "C" fn(ctx: *const BamHostCtx, message: StrView) -> FfiResult;

/// Resolves a registered component or resource strictly by its 128-bit [`ComponentUuid`]
/// (`typedef BamResult (*BamResolveComponentByUuidFn)(const BamHostCtx* ctx, const BamComponentUuid* uuid, BamComponent* out_component);`).
pub type BamResolveComponentByUuidFn = unsafe extern "C" fn(
    ctx: *const BamHostCtx,
    uuid: *const ComponentUuid,
    out_component: *mut BamComponent,
) -> FfiResult;

/// Queries registered layout, schema fingerprint, and [`crate::BamFieldDesc`] reflection metadata
/// (`typedef BamResult (*BamComponentLayoutInfoFn)(const BamHostCtx* ctx, BamComponent component, BamLayoutInfo* out_info);`).
pub type BamComponentLayoutInfoFn = unsafe extern "C" fn(
    ctx: *const BamHostCtx,
    component: BamComponent,
    out_info: *mut BamLayoutInfo,
) -> FfiResult;

/// Registers a typed entity relationship edge descriptor during mod initialization
/// (`typedef BamResult (*BamRegisterRelationFn)(BamLoadCtx* load_ctx, const BamRelationDesc* desc, BamComponent* out_component);`).
pub type BamRegisterRelationFn = unsafe extern "C" fn(
    load_ctx: *mut BamLoadCtx,
    desc: *const BamRelationDesc,
    out_component: *mut BamComponent,
) -> FfiResult;

/// Interns an arbitrary UTF-8 symbol string to an `O(1)` comparable [`BamSymbol`] handle
/// (`typedef BamResult (*BamInternSymbolFn)(const BamHostCtx* ctx, BamStrView symbol, BamSymbol* out_symbol);`).
pub type BamInternSymbolFn = unsafe extern "C" fn(
    ctx: *const BamHostCtx,
    symbol: StrView,
    out_symbol: *mut BamSymbol,
) -> FfiResult;

/// Binds a canonical lookup name to an entity in the host symbol registry
/// (`typedef BamResult (*BamBindNamedObjectFn)(const BamHostCtx* ctx, BamStrView name, BamEntity entity);`).
pub type BamBindNamedObjectFn =
    unsafe extern "C" fn(ctx: *const BamHostCtx, name: StrView, entity: FfiEntity) -> FfiResult;

/// Counts matching entities in an active query iterator without destroying chunks
/// (`typedef BamResult (*BamIterCountFn)(BamIter* iter, size_t* out_count);`).
pub type BamIterCountFn =
    unsafe extern "C" fn(iter: *mut BamIter, out_count: *mut usize) -> FfiResult;

/// Walks an ancestor relationship chain upward until finding `term_index`
/// (`typedef BamResult (*BamIterUpCascadeFn)(BamIter* iter, BamComponent relation, BamEntity entity, size_t term_index, const void** out_ptr, BamEntity* out_owner);`).
pub type BamIterUpCascadeFn = unsafe extern "C" fn(
    iter: *mut BamIter,
    relation: BamComponent,
    entity: FfiEntity,
    term_index: usize,
    out_ptr: *mut *const c_void,
    out_owner: *mut FfiEntity,
) -> FfiResult;

/// Registers a unified [`BamSystemDesc`] and returns its assigned [`BamSystem`] handle
/// (`typedef BamResult (*BamRegisterSystemHandleFn)(BamLoadCtx* load_ctx, const BamSystemDesc* desc, BamSystem* out_system);`).
pub type BamRegisterSystemHandleFn = unsafe extern "C" fn(
    load_ctx: *mut BamLoadCtx,
    desc: *const BamSystemDesc,
    out_system: *mut BamSystem,
) -> FfiResult;

/// Configures a named schedule system set and its `before`/`after` ordering edges
/// (`typedef BamResult (*BamConfigureScheduleSetFn)(BamLoadCtx* load_ctx, const BamScheduleSetDesc* desc, BamSystemSet* out_set);`).
pub type BamConfigureScheduleSetFn = unsafe extern "C" fn(
    load_ctx: *mut BamLoadCtx,
    desc: *const BamScheduleSetDesc,
    out_set: *mut BamSystemSet,
) -> FfiResult;

/// Mounts a registered [`BamSystem`] into `schedule` under `set`
/// (`typedef BamResult (*BamMountSystemInScheduleFn)(BamLoadCtx* load_ctx, BamSystem system, BamScheduleId schedule, BamSystemSet set);`).
pub type BamMountSystemInScheduleFn = unsafe extern "C" fn(
    load_ctx: *mut BamLoadCtx,
    system: BamSystem,
    schedule: ScheduleId,
    set: BamSystemSet,
) -> FfiResult;

/// Captures a deferred command-buffer checkpoint token
/// (`typedef BamResult (*BamCmdCheckpointFn)(BamCommands* commands, size_t* out_checkpoint);`).
pub type BamCmdCheckpointFn =
    unsafe extern "C" fn(commands: *mut BamCommands, out_checkpoint: *mut usize) -> FfiResult;

/// Rolls back deferred commands queued after `checkpoint`, invoking uncommitted guest destructors
/// (`typedef BamResult (*BamCmdRollbackFn)(BamCommands* commands, size_t checkpoint);`).
pub type BamCmdRollbackFn =
    unsafe extern "C" fn(commands: *mut BamCommands, checkpoint: usize) -> FfiResult;

/// Queues insertion of a typed relationship edge `entity --[relation]--> target`
/// (`typedef BamResult (*BamCmdInsertRelationFn)(BamCommands* commands, BamEntity entity, BamComponent relation, BamEntity target, const void* data, size_t data_size);`).
pub type BamCmdInsertRelationFn = unsafe extern "C" fn(
    commands: *mut BamCommands,
    entity: FfiEntity,
    relation: BamComponent,
    target: FfiEntity,
    data: *const c_void,
    data_size: usize,
) -> FfiResult;

/// Queues removal of a typed relationship edge `entity --[relation]--> target`
/// (`typedef BamResult (*BamCmdRemoveRelationFn)(BamCommands* commands, BamEntity entity, BamComponent relation, BamEntity target);`).
pub type BamCmdRemoveRelationFn = unsafe extern "C" fn(
    commands: *mut BamCommands,
    entity: FfiEntity,
    relation: BamComponent,
    target: FfiEntity,
) -> FfiResult;

/// Migrates live ECS rows of `component` from `old_fingerprint` to `new_desc` using `migrate_fn`
/// (`typedef BamResult (*BamMigrateComponentColumnFn)(BamLoadCtx* load_ctx, BamComponent component, uint64_t old_fingerprint, const BamComponentDesc* new_desc, BamRowMigrateFn migrate_fn, void* user_data);`).
pub type BamMigrateComponentColumnFn = unsafe extern "C" fn(
    load_ctx: *mut BamLoadCtx,
    component: BamComponent,
    old_fingerprint: u64,
    new_desc: *const ComponentDesc,
    migrate_fn: Option<BamRowMigrateFn>,
    user_data: *mut c_void,
) -> FfiResult;

/// Total number of function-pointer slots in the ABI `0.3.0` [`BamHostApi`] table (`68`).
pub const BAM_HOST_API_SLOT_COUNT: usize = 68;

/// Complete versioned host function table passed to `bevy_abi_mod_init`
/// (`typedef struct BamHostApi BamHostApi;`).
///
/// Every function slot uses `Option<unsafe extern "C" fn(...)>` so zero-extended future slots and
/// null-pointer checks are sound without raw pointer byte-probing.
///
/// Layout:
/// - 64-bit: `MIN_V0_2_SIZE = 224` bytes (`8`B header + `27 * 8`B core slots; `size_of = 552` bytes with all 68 ABI `0.3` slots), `8`-byte alignment.
/// - 32-bit: `MIN_V0_2_SIZE = 116` bytes (`8`B header + `27 * 4`B core slots; `size_of = 280` bytes with all 68 ABI `0.3` slots), `4`-byte alignment.
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct BamHostApi {
    /// Size of this struct in bytes (`offset 0`).
    pub struct_size: u32,
    /// Packed host ABI version (`ABI_VERSION`, `offset 4`).
    pub abi_version: u32,

    // Diagnostics & custom symbols
    /// Context-carrying symbol lookup for `"bam.v0.*"` symbols and VTables (`offset 8` / `8`).
    pub get_proc_address: Option<BamGetProcAddressFn>,
    /// Per-context human-readable diagnostic retrieval (`offset 16` / `12`).
    pub last_error: Option<BamLastErrorFn>,

    // Registration & handle resolution
    /// Extract [`BamHostCtx`] from [`BamLoadCtx`] (`offset 24` / `16`).
    pub load_host_ctx: Option<BamLoadHostCtxFn>,
    /// Register a dynamic [`ComponentDesc`] -> [`BamComponent`] (`offset 32` / `20`).
    pub register_component: Option<BamRegisterComponentFn>,
    /// Resolve a component or resource by name and/or UUID -> [`BamComponent`] (`offset 40` / `24`).
    pub resolve_component: Option<BamResolveComponentFn>,
    /// Register a unified [`BamSystemDesc`] (`offset 48` / `28`).
    pub register_system: Option<BamRegisterSystemFn>,

    // System invocation context
    /// Extract [`BamHostCtx`] from [`BamSystemCtx`] (`offset 56` / `32`).
    pub sys_host_ctx: Option<BamSysHostCtxFn>,
    /// Obtain [`BamIter`] for `query_index` (`offset 64` / `36`).
    pub sys_query: Option<BamSysQueryFn>,
    /// Obtain [`BamCommands`] for the running system (`offset 72` / `40`).
    pub sys_commands: Option<BamSysCommandsFn>,

    // Chunk & sparse-row iteration
    /// Advance [`BamIter`] to the next chunk (`offset 80` / `44`).
    pub iter_next_chunk: Option<BamIterNextChunkFn>,
    /// Reset [`BamIter`] back to the first chunk (`offset 88` / `48`).
    pub iter_reset: Option<BamIterResetFn>,
    /// Borrow `[FfiEntity; chunk_len]` for the current chunk (`offset 96` / `52`).
    pub iter_entities: Option<BamIterEntitiesFn>,
    /// Borrow read-only dense column bytes for `term_index` (`offset 104` / `56`).
    pub iter_column_readonly: Option<BamIterColumnReadonlyFn>,
    /// Borrow mutable dense column bytes for `term_index`, marking chunk ticks once (`offset 112` / `60`).
    pub iter_column_mutable: Option<BamIterColumnMutableFn>,
    /// Borrow mutable dense column bytes without marking change ticks (`offset 120` / `64`).
    pub iter_column_mutable_untracked: Option<BamIterColumnMutableUntrackedFn>,
    /// Mark `start_row..start_row + row_count` changed on `term_index` (`offset 128` / `68`).
    pub iter_mark_changed_range: Option<BamIterMarkChangedRangeFn>,
    /// Borrow indirect `[ *const u8; chunk_len ]` row pointers for sparse-set read columns (`offset 136` / `72`).
    pub iter_column_rows_readonly: Option<BamIterColumnRowsReadonlyFn>,
    /// Borrow indirect `[ *mut u8; chunk_len ]` row pointers for sparse-set write columns (`offset 144` / `76`).
    pub iter_column_rows_mutable: Option<BamIterColumnRowsMutableFn>,
    /// Introspect UUID, layout, mode, and storage flags for `term_index` (`offset 152` / `80`).
    pub iter_column_info: Option<BamIterColumnInfoFn>,

    // Declaration-indexed singleton resources
    /// Borrow read-only singleton resource bytes at `term_index` (`offset 160` / `84`).
    pub res_ptr: Option<BamResPtrFn>,
    /// Borrow mutable singleton resource bytes at `term_index` (`offset 168` / `88`).
    pub res_mut_ptr: Option<BamResMutPtrFn>,

    // Handle-based & batched deferred commands
    /// Reserve a fresh empty entity (`offset 176` / `92`).
    pub cmd_spawn_empty: Option<BamCmdSpawnEmptyFn>,
    /// Reserve an entity and batch-insert `count` components in one archetype placement (`offset 184` / `96`).
    pub cmd_spawn: Option<BamCmdSpawnFn>,
    /// Queue entity despawn (`offset 192` / `100`).
    pub cmd_despawn: Option<BamCmdDespawnFn>,
    /// Queue raw byte component insertion by [`BamComponent`] handle (`offset 200` / `104`).
    pub cmd_insert: Option<BamCmdInsertFn>,
    /// Queue `T::default()` component insertion by [`BamComponent`] handle (`offset 208` / `108`).
    pub cmd_insert_default: Option<BamCmdInsertDefaultFn>,
    /// Queue component removal by [`BamComponent`] handle (`offset 216` / `112`).
    pub cmd_remove: Option<BamCmdRemoveFn>,

    // Optional extended engine slots (appended after `MIN_V0_2_SIZE`)
    /// Host-driven parallel chunk iteration across `ComputeTaskPool` (`offset 224` / `116`).
    pub par_for_each_chunk: Option<BamParForEachChunkFn>,
    /// Random O(1) read-only per-entity component lookup (`offset 232` / `120`).
    pub sys_entity_get: Option<BamSysEntityGetFn>,
    /// Random O(1) mutable per-entity component lookup (`offset 240` / `124`).
    pub sys_entity_get_mut: Option<BamSysEntityGetMutFn>,
    /// Buffered Bevy `Messages<M>` batch reader (`offset 248` / `128`).
    pub msg_read_batch: Option<BamMsgReadBatchFn>,
    /// Buffered Bevy `Messages<M>` writer (`offset 256` / `132`).
    pub msg_send: Option<BamMsgSendFn>,
    /// Buffered Bevy `Messages<M>` batch writer (`offset 264` / `136`).
    pub msg_write_batch: Option<BamMsgWriteBatchFn>,
    /// Singleton resource change-detection ticks at `term_index` (`offset 272` / `140`).
    pub res_ticks: Option<BamResTicksFn>,
    /// Direct pre-resolved read-only resource lookup with change ticks (`offset 280` / `144`).
    pub sys_res_ptr: Option<BamSysResPtrFn>,
    /// Direct pre-resolved mutable resource lookup with change ticks (`offset 288` / `148`).
    pub sys_res_mut_ptr: Option<BamSysResMutPtrFn>,
    /// Dynamic `Event` / `EntityEvent` observer registration (`offset 296` / `152`).
    pub register_observer: Option<BamRegisterObserverFn>,
    /// Deferred `world.trigger(...)` event emission (`offset 304` / `156`).
    pub cmd_trigger: Option<BamCmdTriggerFn>,

    // Extended entity commands, lifecycle observers, removed components, hierarchy, assets & scenes
    /// Spawn an entity that persists across mod unload (`offset 312` / `160`).
    pub cmd_spawn_persistent: Option<BamCmdSpawnFn>,
    /// Bulk SoA multi-entity spawn (`offset 320` / `164`).
    pub cmd_spawn_batch: Option<BamCmdSpawnBatchFn>,
    /// Bulk filter-matched `World::despawn_all` (`offset 328` / `168`).
    pub cmd_despawn_all: Option<BamCmdDespawnAllFn>,
    /// Conditional component insertion (`InsertMode::Keep`, `offset 336` / `172`).
    pub cmd_insert_if_new: Option<BamCmdInsertFn>,
    /// Extended trigger with lifecycle, component targets, and propagation (`offset 344` / `176`).
    pub cmd_trigger_ex: Option<BamCmdTriggerExFn>,
    /// Deferred runtime observer spawning (`offset 352` / `180`).
    pub cmd_observe: Option<BamCmdObserveFn>,
    /// Lifecycle `RemovedComponents<T>` batch entity reader (`offset 360` / `184`).
    pub iter_read_removed: Option<BamIterReadRemovedFn>,
    /// Read `Children` or custom `RelationshipTarget` entities on `entity` (`offset 368` / `188`).
    pub sys_entity_children: Option<BamSysEntityChildrenFn>,
    /// Request async asset load via `AssetServer` (`offset 376` / `192`).
    pub asset_load: Option<BamAssetLoadFn>,
    /// Query `FfiAssetLoadState` for an `FfiAssetHandle` (`offset 384` / `196`).
    pub asset_get_load_state: Option<BamAssetGetLoadStateFn>,
    /// Release host-retained strong `FfiAssetHandle` (`offset 392` / `200`).
    pub asset_release: Option<BamAssetReleaseFn>,
    /// Deferred scene asset spawning (`offset 400` / `204`).
    pub cmd_spawn_scene: Option<BamCmdSpawnSceneFn>,

    // ABI `0.3.0` extended diagnostics, schema/layout introspection, relations, scheduling & checkpoints
    /// Emit structured guest log via host `tracing` (`offset 408` / `208`).
    pub log_diagnostic: Option<BamLogDiagnosticFn>,
    /// Report a guest-side fault on `BamHostCtx` (`offset 416` / `212`).
    pub report_guest_fault: Option<BamReportGuestFaultFn>,
    /// Resolve a component/resource by 128-bit UUID (`offset 424` / `216`).
    pub resolve_component_by_uuid: Option<BamResolveComponentByUuidFn>,
    /// Query registered `BamLayoutInfo`, schema fingerprint & fields (`offset 432` / `220`).
    pub component_layout_info: Option<BamComponentLayoutInfoFn>,
    /// Register a typed entity relationship edge (`offset 440` / `224`).
    pub register_relation: Option<BamRegisterRelationFn>,
    /// Intern a UTF-8 symbol string to an `O(1)` `BamSymbol` (`offset 448` / `228`).
    pub intern_symbol: Option<BamInternSymbolFn>,
    /// Bind a canonical name to an entity (`offset 456` / `232`).
    pub bind_named_object: Option<BamBindNamedObjectFn>,
    /// Count matching entities in an active iterator (`offset 464` / `236`).
    pub iter_count: Option<BamIterCountFn>,
    /// Walk ancestor relationship chain upward (`offset 472` / `240`).
    pub iter_up_cascade: Option<BamIterUpCascadeFn>,
    /// Host-driven parallel read-only chunk iteration (`offset 480` / `244`).
    pub par_for_each_chunk_readonly: Option<BamParForEachChunkFn>,
    /// Register a system and receive its `BamSystem` handle (`offset 488` / `248`).
    pub register_system_handle: Option<BamRegisterSystemHandleFn>,
    /// Configure a schedule system set (`offset 496` / `252`).
    pub configure_schedule_set: Option<BamConfigureScheduleSetFn>,
    /// Mount a `BamSystem` into a schedule and set (`offset 504` / `256`).
    pub mount_system_in_schedule: Option<BamMountSystemInScheduleFn>,
    /// Capture a command-buffer checkpoint (`offset 512` / `260`).
    pub cmd_checkpoint: Option<BamCmdCheckpointFn>,
    /// Roll back queued commands to a checkpoint (`offset 520` / `264`).
    pub cmd_rollback: Option<BamCmdRollbackFn>,
    /// Queue typed relationship insertion (`offset 528` / `268`).
    pub cmd_insert_relation: Option<BamCmdInsertRelationFn>,
    /// Queue typed relationship removal (`offset 536` / `272`).
    pub cmd_remove_relation: Option<BamCmdRemoveRelationFn>,
    /// Migrate live component rows across schemas (`offset 544` / `276`).
    pub migrate_component_column: Option<BamMigrateComponentColumnFn>,
}

impl BamHostApi {
    /// Frozen minimum valid `BamHostApi` size in ABI `0.2` (`224` on 64-bit, `116` on 32-bit).
    pub const MIN_V0_2_SIZE: usize =
        core::mem::offset_of!(Self, cmd_remove) + core::mem::size_of::<Option<BamCmdRemoveFn>>();

    /// Full ABI `0.3` size of `BamHostApi` (`552` on 64-bit, `280` on 32-bit, all 68 slots).
    pub const MIN_V0_3_SIZE: usize = core::mem::size_of::<Self>();

    /// Safely reads the `struct_size` header at offset `0` from a potentially smaller `BamHostApi`
    /// allocation without forming a full `&BamHostApi` reference.
    ///
    /// # Safety
    /// If `api_ptr` is non-null, it must be aligned and readable for at least `size_of::<u32>()` bytes.
    #[inline]
    pub unsafe fn raw_struct_size(api_ptr: *const Self) -> Option<u32> {
        if api_ptr.is_null() {
            return None;
        }
        // SAFETY: Caller guarantees at least 4 readable, aligned bytes at offset 0 (`struct_size`).
        Some(unsafe { core::ptr::read(core::ptr::addr_of!((*api_ptr).struct_size)) })
    }

    /// Safely reads `abi_version` at offset `4` from a `BamHostApi` allocation without forming a
    /// full `&BamHostApi` reference.
    ///
    /// # Safety
    /// If `api_ptr` is non-null, it must be aligned and readable for at least 8 bytes when
    /// `struct_size >= 8`.
    #[inline]
    pub unsafe fn raw_abi_version(api_ptr: *const Self) -> Option<u32> {
        // SAFETY: Forwarding `api_ptr` under caller's alignment and header-readability contract.
        let sz = unsafe { Self::raw_struct_size(api_ptr) }?;
        if (sz as usize) < core::mem::offset_of!(Self, get_proc_address) {
            return None;
        }
        // SAFETY: Verified `struct_size >= 8`, so `abi_version` at offset 4 is initialized and readable.
        Some(unsafe { core::ptr::read(core::ptr::addr_of!((*api_ptr).abi_version)) })
    }

    /// Returns `true` if `self.struct_size` covers a slot of size `slot_size` at `slot_offset`.
    #[inline]
    pub const fn covers_slot(&self, slot_offset: usize, slot_size: usize) -> bool {
        match slot_offset.checked_add(slot_size) {
            Some(end) => (self.struct_size as usize) >= end,
            None => false,
        }
    }

    /// Returns `true` if all 27 mandatory core ABI `0.2` function-pointer slots are `Some`.
    #[inline]
    pub const fn has_all_v0_2_slots(&self) -> bool {
        self.get_proc_address.is_some()
            && self.last_error.is_some()
            && self.load_host_ctx.is_some()
            && self.register_component.is_some()
            && self.resolve_component.is_some()
            && self.register_system.is_some()
            && self.sys_host_ctx.is_some()
            && self.sys_query.is_some()
            && self.sys_commands.is_some()
            && self.iter_next_chunk.is_some()
            && self.iter_reset.is_some()
            && self.iter_entities.is_some()
            && self.iter_column_readonly.is_some()
            && self.iter_column_mutable.is_some()
            && self.iter_column_mutable_untracked.is_some()
            && self.iter_mark_changed_range.is_some()
            && self.iter_column_rows_readonly.is_some()
            && self.iter_column_rows_mutable.is_some()
            && self.iter_column_info.is_some()
            && self.res_ptr.is_some()
            && self.res_mut_ptr.is_some()
            && self.cmd_spawn_empty.is_some()
            && self.cmd_spawn.is_some()
            && self.cmd_despawn.is_some()
            && self.cmd_insert.is_some()
            && self.cmd_insert_default.is_some()
            && self.cmd_remove.is_some()
    }

    /// Returns `true` if all 50 ABI `0.2` core and extended function-pointer slots are `Some`.
    #[inline]
    pub const fn has_all_v0_2_extended_slots(&self) -> bool {
        self.has_all_v0_2_slots()
            && self.par_for_each_chunk.is_some()
            && self.sys_entity_get.is_some()
            && self.sys_entity_get_mut.is_some()
            && self.msg_read_batch.is_some()
            && self.msg_send.is_some()
            && self.msg_write_batch.is_some()
            && self.res_ticks.is_some()
            && self.sys_res_ptr.is_some()
            && self.sys_res_mut_ptr.is_some()
            && self.register_observer.is_some()
            && self.cmd_trigger.is_some()
            && self.cmd_spawn_persistent.is_some()
            && self.cmd_spawn_batch.is_some()
            && self.cmd_despawn_all.is_some()
            && self.cmd_insert_if_new.is_some()
            && self.cmd_trigger_ex.is_some()
            && self.cmd_observe.is_some()
            && self.iter_read_removed.is_some()
            && self.sys_entity_children.is_some()
            && self.asset_load.is_some()
            && self.asset_get_load_state.is_some()
            && self.asset_release.is_some()
            && self.cmd_spawn_scene.is_some()
    }

    /// Returns `true` if all 18 ABI `0.3` function-pointer slots are `Some`.
    #[inline]
    pub const fn has_all_v0_3_slots(&self) -> bool {
        self.log_diagnostic.is_some()
            && self.report_guest_fault.is_some()
            && self.resolve_component_by_uuid.is_some()
            && self.component_layout_info.is_some()
            && self.register_relation.is_some()
            && self.intern_symbol.is_some()
            && self.bind_named_object.is_some()
            && self.iter_count.is_some()
            && self.iter_up_cascade.is_some()
            && self.par_for_each_chunk_readonly.is_some()
            && self.register_system_handle.is_some()
            && self.configure_schedule_set.is_some()
            && self.mount_system_in_schedule.is_some()
            && self.cmd_checkpoint.is_some()
            && self.cmd_rollback.is_some()
            && self.cmd_insert_relation.is_some()
            && self.cmd_remove_relation.is_some()
            && self.migrate_component_column.is_some()
    }

    /// Returns `true` if all 68 ABI `0.3` function-pointer slots are `Some`.
    #[inline]
    pub const fn has_all_slots(&self) -> bool {
        self.has_all_v0_2_extended_slots() && self.has_all_v0_3_slots()
    }

    /// Validates `struct_size >= MIN_V0_2_SIZE`, SemVer compatibility against `guest_version`,
    /// and non-nullness of all 27 core `0.2` slots.
    #[inline]
    pub const fn validate_for_guest(&self, guest_version: u32) -> FfiResult {
        if (self.struct_size as usize) < Self::MIN_V0_2_SIZE {
            return FfiResult::INVALID_VERSION;
        }
        if !abi_version_is_compatible(self.abi_version, guest_version) {
            return FfiResult::INVALID_VERSION;
        }
        if !self.has_all_v0_2_slots() {
            return FfiResult::NULL_POINTER;
        }
        FfiResult::OK
    }
}

const _: () = {
    assert!(BamHostApi::MIN_V0_2_SIZE <= core::mem::size_of::<BamHostApi>());
    assert!(BamHostApi::MIN_V0_3_SIZE == core::mem::size_of::<BamHostApi>());
    #[cfg(target_pointer_width = "64")]
    {
        assert!(BamHostApi::MIN_V0_2_SIZE == 224);
        assert!(core::mem::size_of::<BamHostApi>() == 552);
        assert!(core::mem::align_of::<BamHostApi>() == 8);
        assert!(core::mem::offset_of!(BamHostApi, struct_size) == 0);
        assert!(core::mem::offset_of!(BamHostApi, abi_version) == 4);
        assert!(core::mem::offset_of!(BamHostApi, get_proc_address) == 8);
        assert!(core::mem::offset_of!(BamHostApi, last_error) == 16);
        assert!(core::mem::offset_of!(BamHostApi, load_host_ctx) == 24);
        assert!(core::mem::offset_of!(BamHostApi, register_component) == 32);
        assert!(core::mem::offset_of!(BamHostApi, resolve_component) == 40);
        assert!(core::mem::offset_of!(BamHostApi, register_system) == 48);
        assert!(core::mem::offset_of!(BamHostApi, sys_host_ctx) == 56);
        assert!(core::mem::offset_of!(BamHostApi, sys_query) == 64);
        assert!(core::mem::offset_of!(BamHostApi, sys_commands) == 72);
        assert!(core::mem::offset_of!(BamHostApi, iter_next_chunk) == 80);
        assert!(core::mem::offset_of!(BamHostApi, iter_reset) == 88);
        assert!(core::mem::offset_of!(BamHostApi, iter_entities) == 96);
        assert!(core::mem::offset_of!(BamHostApi, iter_column_readonly) == 104);
        assert!(core::mem::offset_of!(BamHostApi, iter_column_mutable) == 112);
        assert!(core::mem::offset_of!(BamHostApi, iter_column_mutable_untracked) == 120);
        assert!(core::mem::offset_of!(BamHostApi, iter_mark_changed_range) == 128);
        assert!(core::mem::offset_of!(BamHostApi, iter_column_rows_readonly) == 136);
        assert!(core::mem::offset_of!(BamHostApi, iter_column_rows_mutable) == 144);
        assert!(core::mem::offset_of!(BamHostApi, iter_column_info) == 152);
        assert!(core::mem::offset_of!(BamHostApi, res_ptr) == 160);
        assert!(core::mem::offset_of!(BamHostApi, res_mut_ptr) == 168);
        assert!(core::mem::offset_of!(BamHostApi, cmd_spawn_empty) == 176);
        assert!(core::mem::offset_of!(BamHostApi, cmd_spawn) == 184);
        assert!(core::mem::offset_of!(BamHostApi, cmd_despawn) == 192);
        assert!(core::mem::offset_of!(BamHostApi, cmd_insert) == 200);
        assert!(core::mem::offset_of!(BamHostApi, cmd_insert_default) == 208);
        assert!(core::mem::offset_of!(BamHostApi, cmd_remove) == 216);
        assert!(core::mem::offset_of!(BamHostApi, par_for_each_chunk) == 224);
        assert!(core::mem::offset_of!(BamHostApi, sys_entity_get) == 232);
        assert!(core::mem::offset_of!(BamHostApi, sys_entity_get_mut) == 240);
        assert!(core::mem::offset_of!(BamHostApi, msg_read_batch) == 248);
        assert!(core::mem::offset_of!(BamHostApi, msg_send) == 256);
        assert!(core::mem::offset_of!(BamHostApi, msg_write_batch) == 264);
        assert!(core::mem::offset_of!(BamHostApi, res_ticks) == 272);
        assert!(core::mem::offset_of!(BamHostApi, sys_res_ptr) == 280);
        assert!(core::mem::offset_of!(BamHostApi, sys_res_mut_ptr) == 288);
        assert!(core::mem::offset_of!(BamHostApi, register_observer) == 296);
        assert!(core::mem::offset_of!(BamHostApi, cmd_trigger) == 304);
        assert!(core::mem::offset_of!(BamHostApi, cmd_spawn_persistent) == 312);
        assert!(core::mem::offset_of!(BamHostApi, cmd_spawn_batch) == 320);
        assert!(core::mem::offset_of!(BamHostApi, cmd_despawn_all) == 328);
        assert!(core::mem::offset_of!(BamHostApi, cmd_insert_if_new) == 336);
        assert!(core::mem::offset_of!(BamHostApi, cmd_trigger_ex) == 344);
        assert!(core::mem::offset_of!(BamHostApi, cmd_observe) == 352);
        assert!(core::mem::offset_of!(BamHostApi, iter_read_removed) == 360);
        assert!(core::mem::offset_of!(BamHostApi, sys_entity_children) == 368);
        assert!(core::mem::offset_of!(BamHostApi, asset_load) == 376);
        assert!(core::mem::offset_of!(BamHostApi, asset_get_load_state) == 384);
        assert!(core::mem::offset_of!(BamHostApi, asset_release) == 392);
        assert!(core::mem::offset_of!(BamHostApi, cmd_spawn_scene) == 400);
        assert!(core::mem::offset_of!(BamHostApi, log_diagnostic) == 408);
        assert!(core::mem::offset_of!(BamHostApi, report_guest_fault) == 416);
        assert!(core::mem::offset_of!(BamHostApi, resolve_component_by_uuid) == 424);
        assert!(core::mem::offset_of!(BamHostApi, component_layout_info) == 432);
        assert!(core::mem::offset_of!(BamHostApi, register_relation) == 440);
        assert!(core::mem::offset_of!(BamHostApi, intern_symbol) == 448);
        assert!(core::mem::offset_of!(BamHostApi, bind_named_object) == 456);
        assert!(core::mem::offset_of!(BamHostApi, iter_count) == 464);
        assert!(core::mem::offset_of!(BamHostApi, iter_up_cascade) == 472);
        assert!(core::mem::offset_of!(BamHostApi, par_for_each_chunk_readonly) == 480);
        assert!(core::mem::offset_of!(BamHostApi, register_system_handle) == 488);
        assert!(core::mem::offset_of!(BamHostApi, configure_schedule_set) == 496);
        assert!(core::mem::offset_of!(BamHostApi, mount_system_in_schedule) == 504);
        assert!(core::mem::offset_of!(BamHostApi, cmd_checkpoint) == 512);
        assert!(core::mem::offset_of!(BamHostApi, cmd_rollback) == 520);
        assert!(core::mem::offset_of!(BamHostApi, cmd_insert_relation) == 528);
        assert!(core::mem::offset_of!(BamHostApi, cmd_remove_relation) == 536);
        assert!(core::mem::offset_of!(BamHostApi, migrate_component_column) == 544);
    }
    #[cfg(target_pointer_width = "32")]
    {
        assert!(BamHostApi::MIN_V0_2_SIZE == 116);
        assert!(core::mem::size_of::<BamHostApi>() == 280);
        assert!(core::mem::align_of::<BamHostApi>() == 4);
        assert!(core::mem::offset_of!(BamHostApi, struct_size) == 0);
        assert!(core::mem::offset_of!(BamHostApi, abi_version) == 4);
        assert!(core::mem::offset_of!(BamHostApi, get_proc_address) == 8);
        assert!(core::mem::offset_of!(BamHostApi, last_error) == 12);
        assert!(core::mem::offset_of!(BamHostApi, load_host_ctx) == 16);
        assert!(core::mem::offset_of!(BamHostApi, register_component) == 20);
        assert!(core::mem::offset_of!(BamHostApi, resolve_component) == 24);
        assert!(core::mem::offset_of!(BamHostApi, register_system) == 28);
        assert!(core::mem::offset_of!(BamHostApi, sys_host_ctx) == 32);
        assert!(core::mem::offset_of!(BamHostApi, sys_query) == 36);
        assert!(core::mem::offset_of!(BamHostApi, sys_commands) == 40);
        assert!(core::mem::offset_of!(BamHostApi, iter_next_chunk) == 44);
        assert!(core::mem::offset_of!(BamHostApi, iter_reset) == 48);
        assert!(core::mem::offset_of!(BamHostApi, iter_entities) == 52);
        assert!(core::mem::offset_of!(BamHostApi, iter_column_readonly) == 56);
        assert!(core::mem::offset_of!(BamHostApi, iter_column_mutable) == 60);
        assert!(core::mem::offset_of!(BamHostApi, iter_column_mutable_untracked) == 64);
        assert!(core::mem::offset_of!(BamHostApi, iter_mark_changed_range) == 68);
        assert!(core::mem::offset_of!(BamHostApi, iter_column_rows_readonly) == 72);
        assert!(core::mem::offset_of!(BamHostApi, iter_column_rows_mutable) == 76);
        assert!(core::mem::offset_of!(BamHostApi, iter_column_info) == 80);
        assert!(core::mem::offset_of!(BamHostApi, res_ptr) == 84);
        assert!(core::mem::offset_of!(BamHostApi, res_mut_ptr) == 88);
        assert!(core::mem::offset_of!(BamHostApi, cmd_spawn_empty) == 92);
        assert!(core::mem::offset_of!(BamHostApi, cmd_spawn) == 96);
        assert!(core::mem::offset_of!(BamHostApi, cmd_despawn) == 100);
        assert!(core::mem::offset_of!(BamHostApi, cmd_insert) == 104);
        assert!(core::mem::offset_of!(BamHostApi, cmd_insert_default) == 108);
        assert!(core::mem::offset_of!(BamHostApi, cmd_remove) == 112);
        assert!(core::mem::offset_of!(BamHostApi, par_for_each_chunk) == 116);
        assert!(core::mem::offset_of!(BamHostApi, sys_entity_get) == 120);
        assert!(core::mem::offset_of!(BamHostApi, sys_entity_get_mut) == 124);
        assert!(core::mem::offset_of!(BamHostApi, msg_read_batch) == 128);
        assert!(core::mem::offset_of!(BamHostApi, msg_send) == 132);
        assert!(core::mem::offset_of!(BamHostApi, msg_write_batch) == 136);
        assert!(core::mem::offset_of!(BamHostApi, res_ticks) == 140);
        assert!(core::mem::offset_of!(BamHostApi, sys_res_ptr) == 144);
        assert!(core::mem::offset_of!(BamHostApi, sys_res_mut_ptr) == 148);
        assert!(core::mem::offset_of!(BamHostApi, register_observer) == 152);
        assert!(core::mem::offset_of!(BamHostApi, cmd_trigger) == 156);
        assert!(core::mem::offset_of!(BamHostApi, cmd_spawn_persistent) == 160);
        assert!(core::mem::offset_of!(BamHostApi, cmd_spawn_batch) == 164);
        assert!(core::mem::offset_of!(BamHostApi, cmd_despawn_all) == 168);
        assert!(core::mem::offset_of!(BamHostApi, cmd_insert_if_new) == 172);
        assert!(core::mem::offset_of!(BamHostApi, cmd_trigger_ex) == 176);
        assert!(core::mem::offset_of!(BamHostApi, cmd_observe) == 180);
        assert!(core::mem::offset_of!(BamHostApi, iter_read_removed) == 184);
        assert!(core::mem::offset_of!(BamHostApi, sys_entity_children) == 188);
        assert!(core::mem::offset_of!(BamHostApi, asset_load) == 192);
        assert!(core::mem::offset_of!(BamHostApi, asset_get_load_state) == 196);
        assert!(core::mem::offset_of!(BamHostApi, asset_release) == 200);
        assert!(core::mem::offset_of!(BamHostApi, cmd_spawn_scene) == 204);
        assert!(core::mem::offset_of!(BamHostApi, log_diagnostic) == 208);
        assert!(core::mem::offset_of!(BamHostApi, report_guest_fault) == 212);
        assert!(core::mem::offset_of!(BamHostApi, resolve_component_by_uuid) == 216);
        assert!(core::mem::offset_of!(BamHostApi, component_layout_info) == 220);
        assert!(core::mem::offset_of!(BamHostApi, register_relation) == 224);
        assert!(core::mem::offset_of!(BamHostApi, intern_symbol) == 228);
        assert!(core::mem::offset_of!(BamHostApi, bind_named_object) == 232);
        assert!(core::mem::offset_of!(BamHostApi, iter_count) == 236);
        assert!(core::mem::offset_of!(BamHostApi, iter_up_cascade) == 240);
        assert!(core::mem::offset_of!(BamHostApi, par_for_each_chunk_readonly) == 244);
        assert!(core::mem::offset_of!(BamHostApi, register_system_handle) == 248);
        assert!(core::mem::offset_of!(BamHostApi, configure_schedule_set) == 252);
        assert!(core::mem::offset_of!(BamHostApi, mount_system_in_schedule) == 256);
        assert!(core::mem::offset_of!(BamHostApi, cmd_checkpoint) == 260);
        assert!(core::mem::offset_of!(BamHostApi, cmd_rollback) == 264);
        assert!(core::mem::offset_of!(BamHostApi, cmd_insert_relation) == 268);
        assert!(core::mem::offset_of!(BamHostApi, cmd_remove_relation) == 272);
        assert!(core::mem::offset_of!(BamHostApi, migrate_component_column) == 276);
    }
};

// SAFETY: `BamHostApi` is `#[repr(C)]`, starts with `struct_size: u32` at offset 0, and every
// function pointer field is `Option<unsafe extern "C" fn(...)>` (all-bits-zero = `None`).
unsafe impl ExtensibleStruct for BamHostApi {
    const MIN_V0_2_SIZE: usize = Self::MIN_V0_2_SIZE;
}

/// Compile-time byte offsets for every header field and function-pointer slot of [`BamHostApi`].
///
/// Exports three constant families for all 70 fields of [`BamHostApi`] (`struct_size`,
/// `abi_version`, and all 68 function-pointer slots):
///
/// - `<FIELD>`: Pointer-width-native byte offset (`core::mem::offset_of!(BamHostApi, <field>)`)
///   for the active compilation target.
/// - `OFF64_<FIELD>`: Architecture-independent 64-bit byte offset (`struct_size` at `0`,
///   `abi_version` at `4`, slot `k` at `8 + 8 * k`, total [`offsets::SIZE_64`] = `552` bytes).
/// - `OFF32_<FIELD>`: Architecture-independent 32-bit byte offset (`struct_size` at `0`,
///   `abi_version` at `4`, slot `k` at `8 + 4 * k`, total [`offsets::SIZE_32`] = `280` bytes).
///
/// Cross-target code generators and JIT backends (such as `bql_jit`) can import exact slot
/// offsets without hardcoding magic numbers, while compile-time assertions guarantee agreement
/// with `core::mem::offset_of!` on both 64-bit and 32-bit targets.
pub mod offsets {
    use super::{BAM_HOST_API_SLOT_COUNT, BamHostApi};

    /// Total number of function-pointer slots in ABI `0.3.0` [`BamHostApi`] (`68`).
    pub const SLOT_COUNT: usize = BAM_HOST_API_SLOT_COUNT;

    /// Total byte size of ABI `0.3.0` [`BamHostApi`] on 64-bit targets (`552` bytes).
    pub const SIZE_64: usize = 552;

    /// Total byte size of ABI `0.3.0` [`BamHostApi`] on 32-bit targets (`280` bytes).
    pub const SIZE_32: usize = 280;

    macro_rules! define_host_api_offsets {
        (
            header: [
                $(($h_field:ident, $h_native:ident, $h_off64:ident, $h_val64:expr, $h_off32:ident, $h_val32:expr)),* $(,)?
            ],
            slots: [
                $(($slot_idx:expr, $s_field:ident, $s_native:ident, $s_off64:ident, $s_val64:expr, $s_off32:ident, $s_val32:expr)),* $(,)?
            ]
        ) => {
            $(
                #[doc = concat!(
                    "Target-native byte offset of header field `BamHostApi::",
                    stringify!($h_field),
                    "` (`",
                    stringify!($h_val64),
                    "` on 64-bit, `",
                    stringify!($h_val32),
                    "` on 32-bit)."
                )]
                pub const $h_native: usize = core::mem::offset_of!(BamHostApi, $h_field);

                #[doc = concat!(
                    "64-bit byte offset of header field `BamHostApi::",
                    stringify!($h_field),
                    "` (`",
                    stringify!($h_val64),
                    "` bytes)."
                )]
                pub const $h_off64: usize = $h_val64;

                #[doc = concat!(
                    "32-bit byte offset of header field `BamHostApi::",
                    stringify!($h_field),
                    "` (`",
                    stringify!($h_val32),
                    "` bytes)."
                )]
                pub const $h_off32: usize = $h_val32;
            )*

            $(
                #[doc = concat!(
                    "Target-native byte offset of slot ",
                    stringify!($slot_idx),
                    " (`BamHostApi::",
                    stringify!($s_field),
                    "`, `",
                    stringify!($s_val64),
                    "` on 64-bit, `",
                    stringify!($s_val32),
                    "` on 32-bit)."
                )]
                pub const $s_native: usize = core::mem::offset_of!(BamHostApi, $s_field);

                #[doc = concat!(
                    "64-bit byte offset of slot ",
                    stringify!($slot_idx),
                    " (`BamHostApi::",
                    stringify!($s_field),
                    "`, `",
                    stringify!($s_val64),
                    "` bytes)."
                )]
                pub const $s_off64: usize = $s_val64;

                #[doc = concat!(
                    "32-bit byte offset of slot ",
                    stringify!($slot_idx),
                    " (`BamHostApi::",
                    stringify!($s_field),
                    "`, `",
                    stringify!($s_val32),
                    "` bytes)."
                )]
                pub const $s_off32: usize = $s_val32;
            )*

            const _: () = {
                assert!(SLOT_COUNT == 68);
                assert!(SIZE_64 == 8 + 8 * SLOT_COUNT);
                assert!(SIZE_32 == 8 + 4 * SLOT_COUNT);
                $(
                    assert!($s_off64 == 8 + 8 * $slot_idx);
                    assert!($s_off32 == 8 + 4 * $slot_idx);
                )*

                #[cfg(target_pointer_width = "64")]
                {
                    assert!(core::mem::size_of::<BamHostApi>() == SIZE_64);
                    $(
                        assert!($h_native == $h_off64);
                    )*
                    $(
                        assert!($s_native == $s_off64);
                    )*
                }

                #[cfg(target_pointer_width = "32")]
                {
                    assert!(core::mem::size_of::<BamHostApi>() == SIZE_32);
                    $(
                        assert!($h_native == $h_off32);
                    )*
                    $(
                        assert!($s_native == $s_off32);
                    )*
                }
            };
        };
    }

    define_host_api_offsets! {
        header: [
            (struct_size, STRUCT_SIZE, OFF64_STRUCT_SIZE, 0, OFF32_STRUCT_SIZE, 0),
            (abi_version, ABI_VERSION, OFF64_ABI_VERSION, 4, OFF32_ABI_VERSION, 4),
        ],
        slots: [
            (0, get_proc_address, GET_PROC_ADDRESS, OFF64_GET_PROC_ADDRESS, 8, OFF32_GET_PROC_ADDRESS, 8),
            (1, last_error, LAST_ERROR, OFF64_LAST_ERROR, 16, OFF32_LAST_ERROR, 12),
            (2, load_host_ctx, LOAD_HOST_CTX, OFF64_LOAD_HOST_CTX, 24, OFF32_LOAD_HOST_CTX, 16),
            (3, register_component, REGISTER_COMPONENT, OFF64_REGISTER_COMPONENT, 32, OFF32_REGISTER_COMPONENT, 20),
            (4, resolve_component, RESOLVE_COMPONENT, OFF64_RESOLVE_COMPONENT, 40, OFF32_RESOLVE_COMPONENT, 24),
            (5, register_system, REGISTER_SYSTEM, OFF64_REGISTER_SYSTEM, 48, OFF32_REGISTER_SYSTEM, 28),
            (6, sys_host_ctx, SYS_HOST_CTX, OFF64_SYS_HOST_CTX, 56, OFF32_SYS_HOST_CTX, 32),
            (7, sys_query, SYS_QUERY, OFF64_SYS_QUERY, 64, OFF32_SYS_QUERY, 36),
            (8, sys_commands, SYS_COMMANDS, OFF64_SYS_COMMANDS, 72, OFF32_SYS_COMMANDS, 40),
            (9, iter_next_chunk, ITER_NEXT_CHUNK, OFF64_ITER_NEXT_CHUNK, 80, OFF32_ITER_NEXT_CHUNK, 44),
            (10, iter_reset, ITER_RESET, OFF64_ITER_RESET, 88, OFF32_ITER_RESET, 48),
            (11, iter_entities, ITER_ENTITIES, OFF64_ITER_ENTITIES, 96, OFF32_ITER_ENTITIES, 52),
            (12, iter_column_readonly, ITER_COLUMN_READONLY, OFF64_ITER_COLUMN_READONLY, 104, OFF32_ITER_COLUMN_READONLY, 56),
            (13, iter_column_mutable, ITER_COLUMN_MUTABLE, OFF64_ITER_COLUMN_MUTABLE, 112, OFF32_ITER_COLUMN_MUTABLE, 60),
            (14, iter_column_mutable_untracked, ITER_COLUMN_MUTABLE_UNTRACKED, OFF64_ITER_COLUMN_MUTABLE_UNTRACKED, 120, OFF32_ITER_COLUMN_MUTABLE_UNTRACKED, 64),
            (15, iter_mark_changed_range, ITER_MARK_CHANGED_RANGE, OFF64_ITER_MARK_CHANGED_RANGE, 128, OFF32_ITER_MARK_CHANGED_RANGE, 68),
            (16, iter_column_rows_readonly, ITER_COLUMN_ROWS_READONLY, OFF64_ITER_COLUMN_ROWS_READONLY, 136, OFF32_ITER_COLUMN_ROWS_READONLY, 72),
            (17, iter_column_rows_mutable, ITER_COLUMN_ROWS_MUTABLE, OFF64_ITER_COLUMN_ROWS_MUTABLE, 144, OFF32_ITER_COLUMN_ROWS_MUTABLE, 76),
            (18, iter_column_info, ITER_COLUMN_INFO, OFF64_ITER_COLUMN_INFO, 152, OFF32_ITER_COLUMN_INFO, 80),
            (19, res_ptr, RES_PTR, OFF64_RES_PTR, 160, OFF32_RES_PTR, 84),
            (20, res_mut_ptr, RES_MUT_PTR, OFF64_RES_MUT_PTR, 168, OFF32_RES_MUT_PTR, 88),
            (21, cmd_spawn_empty, CMD_SPAWN_EMPTY, OFF64_CMD_SPAWN_EMPTY, 176, OFF32_CMD_SPAWN_EMPTY, 92),
            (22, cmd_spawn, CMD_SPAWN, OFF64_CMD_SPAWN, 184, OFF32_CMD_SPAWN, 96),
            (23, cmd_despawn, CMD_DESPAWN, OFF64_CMD_DESPAWN, 192, OFF32_CMD_DESPAWN, 100),
            (24, cmd_insert, CMD_INSERT, OFF64_CMD_INSERT, 200, OFF32_CMD_INSERT, 104),
            (25, cmd_insert_default, CMD_INSERT_DEFAULT, OFF64_CMD_INSERT_DEFAULT, 208, OFF32_CMD_INSERT_DEFAULT, 108),
            (26, cmd_remove, CMD_REMOVE, OFF64_CMD_REMOVE, 216, OFF32_CMD_REMOVE, 112),
            (27, par_for_each_chunk, PAR_FOR_EACH_CHUNK, OFF64_PAR_FOR_EACH_CHUNK, 224, OFF32_PAR_FOR_EACH_CHUNK, 116),
            (28, sys_entity_get, SYS_ENTITY_GET, OFF64_SYS_ENTITY_GET, 232, OFF32_SYS_ENTITY_GET, 120),
            (29, sys_entity_get_mut, SYS_ENTITY_GET_MUT, OFF64_SYS_ENTITY_GET_MUT, 240, OFF32_SYS_ENTITY_GET_MUT, 124),
            (30, msg_read_batch, MSG_READ_BATCH, OFF64_MSG_READ_BATCH, 248, OFF32_MSG_READ_BATCH, 128),
            (31, msg_send, MSG_SEND, OFF64_MSG_SEND, 256, OFF32_MSG_SEND, 132),
            (32, msg_write_batch, MSG_WRITE_BATCH, OFF64_MSG_WRITE_BATCH, 264, OFF32_MSG_WRITE_BATCH, 136),
            (33, res_ticks, RES_TICKS, OFF64_RES_TICKS, 272, OFF32_RES_TICKS, 140),
            (34, sys_res_ptr, SYS_RES_PTR, OFF64_SYS_RES_PTR, 280, OFF32_SYS_RES_PTR, 144),
            (35, sys_res_mut_ptr, SYS_RES_MUT_PTR, OFF64_SYS_RES_MUT_PTR, 288, OFF32_SYS_RES_MUT_PTR, 148),
            (36, register_observer, REGISTER_OBSERVER, OFF64_REGISTER_OBSERVER, 296, OFF32_REGISTER_OBSERVER, 152),
            (37, cmd_trigger, CMD_TRIGGER, OFF64_CMD_TRIGGER, 304, OFF32_CMD_TRIGGER, 156),
            (38, cmd_spawn_persistent, CMD_SPAWN_PERSISTENT, OFF64_CMD_SPAWN_PERSISTENT, 312, OFF32_CMD_SPAWN_PERSISTENT, 160),
            (39, cmd_spawn_batch, CMD_SPAWN_BATCH, OFF64_CMD_SPAWN_BATCH, 320, OFF32_CMD_SPAWN_BATCH, 164),
            (40, cmd_despawn_all, CMD_DESPAWN_ALL, OFF64_CMD_DESPAWN_ALL, 328, OFF32_CMD_DESPAWN_ALL, 168),
            (41, cmd_insert_if_new, CMD_INSERT_IF_NEW, OFF64_CMD_INSERT_IF_NEW, 336, OFF32_CMD_INSERT_IF_NEW, 172),
            (42, cmd_trigger_ex, CMD_TRIGGER_EX, OFF64_CMD_TRIGGER_EX, 344, OFF32_CMD_TRIGGER_EX, 176),
            (43, cmd_observe, CMD_OBSERVE, OFF64_CMD_OBSERVE, 352, OFF32_CMD_OBSERVE, 180),
            (44, iter_read_removed, ITER_READ_REMOVED, OFF64_ITER_READ_REMOVED, 360, OFF32_ITER_READ_REMOVED, 184),
            (45, sys_entity_children, SYS_ENTITY_CHILDREN, OFF64_SYS_ENTITY_CHILDREN, 368, OFF32_SYS_ENTITY_CHILDREN, 188),
            (46, asset_load, ASSET_LOAD, OFF64_ASSET_LOAD, 376, OFF32_ASSET_LOAD, 192),
            (47, asset_get_load_state, ASSET_GET_LOAD_STATE, OFF64_ASSET_GET_LOAD_STATE, 384, OFF32_ASSET_GET_LOAD_STATE, 196),
            (48, asset_release, ASSET_RELEASE, OFF64_ASSET_RELEASE, 392, OFF32_ASSET_RELEASE, 200),
            (49, cmd_spawn_scene, CMD_SPAWN_SCENE, OFF64_CMD_SPAWN_SCENE, 400, OFF32_CMD_SPAWN_SCENE, 204),
            (50, log_diagnostic, LOG_DIAGNOSTIC, OFF64_LOG_DIAGNOSTIC, 408, OFF32_LOG_DIAGNOSTIC, 208),
            (51, report_guest_fault, REPORT_GUEST_FAULT, OFF64_REPORT_GUEST_FAULT, 416, OFF32_REPORT_GUEST_FAULT, 212),
            (52, resolve_component_by_uuid, RESOLVE_COMPONENT_BY_UUID, OFF64_RESOLVE_COMPONENT_BY_UUID, 424, OFF32_RESOLVE_COMPONENT_BY_UUID, 216),
            (53, component_layout_info, COMPONENT_LAYOUT_INFO, OFF64_COMPONENT_LAYOUT_INFO, 432, OFF32_COMPONENT_LAYOUT_INFO, 220),
            (54, register_relation, REGISTER_RELATION, OFF64_REGISTER_RELATION, 440, OFF32_REGISTER_RELATION, 224),
            (55, intern_symbol, INTERN_SYMBOL, OFF64_INTERN_SYMBOL, 448, OFF32_INTERN_SYMBOL, 228),
            (56, bind_named_object, BIND_NAMED_OBJECT, OFF64_BIND_NAMED_OBJECT, 456, OFF32_BIND_NAMED_OBJECT, 232),
            (57, iter_count, ITER_COUNT, OFF64_ITER_COUNT, 464, OFF32_ITER_COUNT, 236),
            (58, iter_up_cascade, ITER_UP_CASCADE, OFF64_ITER_UP_CASCADE, 472, OFF32_ITER_UP_CASCADE, 240),
            (59, par_for_each_chunk_readonly, PAR_FOR_EACH_CHUNK_READONLY, OFF64_PAR_FOR_EACH_CHUNK_READONLY, 480, OFF32_PAR_FOR_EACH_CHUNK_READONLY, 244),
            (60, register_system_handle, REGISTER_SYSTEM_HANDLE, OFF64_REGISTER_SYSTEM_HANDLE, 488, OFF32_REGISTER_SYSTEM_HANDLE, 248),
            (61, configure_schedule_set, CONFIGURE_SCHEDULE_SET, OFF64_CONFIGURE_SCHEDULE_SET, 496, OFF32_CONFIGURE_SCHEDULE_SET, 252),
            (62, mount_system_in_schedule, MOUNT_SYSTEM_IN_SCHEDULE, OFF64_MOUNT_SYSTEM_IN_SCHEDULE, 504, OFF32_MOUNT_SYSTEM_IN_SCHEDULE, 256),
            (63, cmd_checkpoint, CMD_CHECKPOINT, OFF64_CMD_CHECKPOINT, 512, OFF32_CMD_CHECKPOINT, 260),
            (64, cmd_rollback, CMD_ROLLBACK, OFF64_CMD_ROLLBACK, 520, OFF32_CMD_ROLLBACK, 264),
            (65, cmd_insert_relation, CMD_INSERT_RELATION, OFF64_CMD_INSERT_RELATION, 528, OFF32_CMD_INSERT_RELATION, 268),
            (66, cmd_remove_relation, CMD_REMOVE_RELATION, OFF64_CMD_REMOVE_RELATION, 536, OFF32_CMD_REMOVE_RELATION, 272),
            (67, migrate_component_column, MIGRATE_COMPONENT_COLUMN, OFF64_MIGRATE_COMPONENT_COLUMN, 544, OFF32_MIGRATE_COMPONENT_COLUMN, 276),
        ]
    }
}

#[cfg(test)]
mod offset_tests {
    use super::{BAM_HOST_API_SLOT_COUNT, BamHostApi, offsets};

    #[test]
    fn host_api_offset_constants_cover_all_68_slots_monotonically() {
        assert_eq!(BAM_HOST_API_SLOT_COUNT, 68);
        assert_eq!(offsets::SLOT_COUNT, 68);
        assert_eq!(offsets::SIZE_64, 552);
        assert_eq!(offsets::SIZE_32, 280);
        assert_eq!(offsets::STRUCT_SIZE, 0);
        assert_eq!(offsets::OFF64_STRUCT_SIZE, 0);
        assert_eq!(offsets::OFF32_STRUCT_SIZE, 0);
        assert_eq!(offsets::ABI_VERSION, 4);
        assert_eq!(offsets::OFF64_ABI_VERSION, 4);
        assert_eq!(offsets::OFF32_ABI_VERSION, 4);

        let all_slots: [(usize, usize, usize); BAM_HOST_API_SLOT_COUNT] = [
            (
                offsets::GET_PROC_ADDRESS,
                offsets::OFF64_GET_PROC_ADDRESS,
                offsets::OFF32_GET_PROC_ADDRESS,
            ),
            (
                offsets::LAST_ERROR,
                offsets::OFF64_LAST_ERROR,
                offsets::OFF32_LAST_ERROR,
            ),
            (
                offsets::LOAD_HOST_CTX,
                offsets::OFF64_LOAD_HOST_CTX,
                offsets::OFF32_LOAD_HOST_CTX,
            ),
            (
                offsets::REGISTER_COMPONENT,
                offsets::OFF64_REGISTER_COMPONENT,
                offsets::OFF32_REGISTER_COMPONENT,
            ),
            (
                offsets::RESOLVE_COMPONENT,
                offsets::OFF64_RESOLVE_COMPONENT,
                offsets::OFF32_RESOLVE_COMPONENT,
            ),
            (
                offsets::REGISTER_SYSTEM,
                offsets::OFF64_REGISTER_SYSTEM,
                offsets::OFF32_REGISTER_SYSTEM,
            ),
            (
                offsets::SYS_HOST_CTX,
                offsets::OFF64_SYS_HOST_CTX,
                offsets::OFF32_SYS_HOST_CTX,
            ),
            (
                offsets::SYS_QUERY,
                offsets::OFF64_SYS_QUERY,
                offsets::OFF32_SYS_QUERY,
            ),
            (
                offsets::SYS_COMMANDS,
                offsets::OFF64_SYS_COMMANDS,
                offsets::OFF32_SYS_COMMANDS,
            ),
            (
                offsets::ITER_NEXT_CHUNK,
                offsets::OFF64_ITER_NEXT_CHUNK,
                offsets::OFF32_ITER_NEXT_CHUNK,
            ),
            (
                offsets::ITER_RESET,
                offsets::OFF64_ITER_RESET,
                offsets::OFF32_ITER_RESET,
            ),
            (
                offsets::ITER_ENTITIES,
                offsets::OFF64_ITER_ENTITIES,
                offsets::OFF32_ITER_ENTITIES,
            ),
            (
                offsets::ITER_COLUMN_READONLY,
                offsets::OFF64_ITER_COLUMN_READONLY,
                offsets::OFF32_ITER_COLUMN_READONLY,
            ),
            (
                offsets::ITER_COLUMN_MUTABLE,
                offsets::OFF64_ITER_COLUMN_MUTABLE,
                offsets::OFF32_ITER_COLUMN_MUTABLE,
            ),
            (
                offsets::ITER_COLUMN_MUTABLE_UNTRACKED,
                offsets::OFF64_ITER_COLUMN_MUTABLE_UNTRACKED,
                offsets::OFF32_ITER_COLUMN_MUTABLE_UNTRACKED,
            ),
            (
                offsets::ITER_MARK_CHANGED_RANGE,
                offsets::OFF64_ITER_MARK_CHANGED_RANGE,
                offsets::OFF32_ITER_MARK_CHANGED_RANGE,
            ),
            (
                offsets::ITER_COLUMN_ROWS_READONLY,
                offsets::OFF64_ITER_COLUMN_ROWS_READONLY,
                offsets::OFF32_ITER_COLUMN_ROWS_READONLY,
            ),
            (
                offsets::ITER_COLUMN_ROWS_MUTABLE,
                offsets::OFF64_ITER_COLUMN_ROWS_MUTABLE,
                offsets::OFF32_ITER_COLUMN_ROWS_MUTABLE,
            ),
            (
                offsets::ITER_COLUMN_INFO,
                offsets::OFF64_ITER_COLUMN_INFO,
                offsets::OFF32_ITER_COLUMN_INFO,
            ),
            (
                offsets::RES_PTR,
                offsets::OFF64_RES_PTR,
                offsets::OFF32_RES_PTR,
            ),
            (
                offsets::RES_MUT_PTR,
                offsets::OFF64_RES_MUT_PTR,
                offsets::OFF32_RES_MUT_PTR,
            ),
            (
                offsets::CMD_SPAWN_EMPTY,
                offsets::OFF64_CMD_SPAWN_EMPTY,
                offsets::OFF32_CMD_SPAWN_EMPTY,
            ),
            (
                offsets::CMD_SPAWN,
                offsets::OFF64_CMD_SPAWN,
                offsets::OFF32_CMD_SPAWN,
            ),
            (
                offsets::CMD_DESPAWN,
                offsets::OFF64_CMD_DESPAWN,
                offsets::OFF32_CMD_DESPAWN,
            ),
            (
                offsets::CMD_INSERT,
                offsets::OFF64_CMD_INSERT,
                offsets::OFF32_CMD_INSERT,
            ),
            (
                offsets::CMD_INSERT_DEFAULT,
                offsets::OFF64_CMD_INSERT_DEFAULT,
                offsets::OFF32_CMD_INSERT_DEFAULT,
            ),
            (
                offsets::CMD_REMOVE,
                offsets::OFF64_CMD_REMOVE,
                offsets::OFF32_CMD_REMOVE,
            ),
            (
                offsets::PAR_FOR_EACH_CHUNK,
                offsets::OFF64_PAR_FOR_EACH_CHUNK,
                offsets::OFF32_PAR_FOR_EACH_CHUNK,
            ),
            (
                offsets::SYS_ENTITY_GET,
                offsets::OFF64_SYS_ENTITY_GET,
                offsets::OFF32_SYS_ENTITY_GET,
            ),
            (
                offsets::SYS_ENTITY_GET_MUT,
                offsets::OFF64_SYS_ENTITY_GET_MUT,
                offsets::OFF32_SYS_ENTITY_GET_MUT,
            ),
            (
                offsets::MSG_READ_BATCH,
                offsets::OFF64_MSG_READ_BATCH,
                offsets::OFF32_MSG_READ_BATCH,
            ),
            (
                offsets::MSG_SEND,
                offsets::OFF64_MSG_SEND,
                offsets::OFF32_MSG_SEND,
            ),
            (
                offsets::MSG_WRITE_BATCH,
                offsets::OFF64_MSG_WRITE_BATCH,
                offsets::OFF32_MSG_WRITE_BATCH,
            ),
            (
                offsets::RES_TICKS,
                offsets::OFF64_RES_TICKS,
                offsets::OFF32_RES_TICKS,
            ),
            (
                offsets::SYS_RES_PTR,
                offsets::OFF64_SYS_RES_PTR,
                offsets::OFF32_SYS_RES_PTR,
            ),
            (
                offsets::SYS_RES_MUT_PTR,
                offsets::OFF64_SYS_RES_MUT_PTR,
                offsets::OFF32_SYS_RES_MUT_PTR,
            ),
            (
                offsets::REGISTER_OBSERVER,
                offsets::OFF64_REGISTER_OBSERVER,
                offsets::OFF32_REGISTER_OBSERVER,
            ),
            (
                offsets::CMD_TRIGGER,
                offsets::OFF64_CMD_TRIGGER,
                offsets::OFF32_CMD_TRIGGER,
            ),
            (
                offsets::CMD_SPAWN_PERSISTENT,
                offsets::OFF64_CMD_SPAWN_PERSISTENT,
                offsets::OFF32_CMD_SPAWN_PERSISTENT,
            ),
            (
                offsets::CMD_SPAWN_BATCH,
                offsets::OFF64_CMD_SPAWN_BATCH,
                offsets::OFF32_CMD_SPAWN_BATCH,
            ),
            (
                offsets::CMD_DESPAWN_ALL,
                offsets::OFF64_CMD_DESPAWN_ALL,
                offsets::OFF32_CMD_DESPAWN_ALL,
            ),
            (
                offsets::CMD_INSERT_IF_NEW,
                offsets::OFF64_CMD_INSERT_IF_NEW,
                offsets::OFF32_CMD_INSERT_IF_NEW,
            ),
            (
                offsets::CMD_TRIGGER_EX,
                offsets::OFF64_CMD_TRIGGER_EX,
                offsets::OFF32_CMD_TRIGGER_EX,
            ),
            (
                offsets::CMD_OBSERVE,
                offsets::OFF64_CMD_OBSERVE,
                offsets::OFF32_CMD_OBSERVE,
            ),
            (
                offsets::ITER_READ_REMOVED,
                offsets::OFF64_ITER_READ_REMOVED,
                offsets::OFF32_ITER_READ_REMOVED,
            ),
            (
                offsets::SYS_ENTITY_CHILDREN,
                offsets::OFF64_SYS_ENTITY_CHILDREN,
                offsets::OFF32_SYS_ENTITY_CHILDREN,
            ),
            (
                offsets::ASSET_LOAD,
                offsets::OFF64_ASSET_LOAD,
                offsets::OFF32_ASSET_LOAD,
            ),
            (
                offsets::ASSET_GET_LOAD_STATE,
                offsets::OFF64_ASSET_GET_LOAD_STATE,
                offsets::OFF32_ASSET_GET_LOAD_STATE,
            ),
            (
                offsets::ASSET_RELEASE,
                offsets::OFF64_ASSET_RELEASE,
                offsets::OFF32_ASSET_RELEASE,
            ),
            (
                offsets::CMD_SPAWN_SCENE,
                offsets::OFF64_CMD_SPAWN_SCENE,
                offsets::OFF32_CMD_SPAWN_SCENE,
            ),
            (
                offsets::LOG_DIAGNOSTIC,
                offsets::OFF64_LOG_DIAGNOSTIC,
                offsets::OFF32_LOG_DIAGNOSTIC,
            ),
            (
                offsets::REPORT_GUEST_FAULT,
                offsets::OFF64_REPORT_GUEST_FAULT,
                offsets::OFF32_REPORT_GUEST_FAULT,
            ),
            (
                offsets::RESOLVE_COMPONENT_BY_UUID,
                offsets::OFF64_RESOLVE_COMPONENT_BY_UUID,
                offsets::OFF32_RESOLVE_COMPONENT_BY_UUID,
            ),
            (
                offsets::COMPONENT_LAYOUT_INFO,
                offsets::OFF64_COMPONENT_LAYOUT_INFO,
                offsets::OFF32_COMPONENT_LAYOUT_INFO,
            ),
            (
                offsets::REGISTER_RELATION,
                offsets::OFF64_REGISTER_RELATION,
                offsets::OFF32_REGISTER_RELATION,
            ),
            (
                offsets::INTERN_SYMBOL,
                offsets::OFF64_INTERN_SYMBOL,
                offsets::OFF32_INTERN_SYMBOL,
            ),
            (
                offsets::BIND_NAMED_OBJECT,
                offsets::OFF64_BIND_NAMED_OBJECT,
                offsets::OFF32_BIND_NAMED_OBJECT,
            ),
            (
                offsets::ITER_COUNT,
                offsets::OFF64_ITER_COUNT,
                offsets::OFF32_ITER_COUNT,
            ),
            (
                offsets::ITER_UP_CASCADE,
                offsets::OFF64_ITER_UP_CASCADE,
                offsets::OFF32_ITER_UP_CASCADE,
            ),
            (
                offsets::PAR_FOR_EACH_CHUNK_READONLY,
                offsets::OFF64_PAR_FOR_EACH_CHUNK_READONLY,
                offsets::OFF32_PAR_FOR_EACH_CHUNK_READONLY,
            ),
            (
                offsets::REGISTER_SYSTEM_HANDLE,
                offsets::OFF64_REGISTER_SYSTEM_HANDLE,
                offsets::OFF32_REGISTER_SYSTEM_HANDLE,
            ),
            (
                offsets::CONFIGURE_SCHEDULE_SET,
                offsets::OFF64_CONFIGURE_SCHEDULE_SET,
                offsets::OFF32_CONFIGURE_SCHEDULE_SET,
            ),
            (
                offsets::MOUNT_SYSTEM_IN_SCHEDULE,
                offsets::OFF64_MOUNT_SYSTEM_IN_SCHEDULE,
                offsets::OFF32_MOUNT_SYSTEM_IN_SCHEDULE,
            ),
            (
                offsets::CMD_CHECKPOINT,
                offsets::OFF64_CMD_CHECKPOINT,
                offsets::OFF32_CMD_CHECKPOINT,
            ),
            (
                offsets::CMD_ROLLBACK,
                offsets::OFF64_CMD_ROLLBACK,
                offsets::OFF32_CMD_ROLLBACK,
            ),
            (
                offsets::CMD_INSERT_RELATION,
                offsets::OFF64_CMD_INSERT_RELATION,
                offsets::OFF32_CMD_INSERT_RELATION,
            ),
            (
                offsets::CMD_REMOVE_RELATION,
                offsets::OFF64_CMD_REMOVE_RELATION,
                offsets::OFF32_CMD_REMOVE_RELATION,
            ),
            (
                offsets::MIGRATE_COMPONENT_COLUMN,
                offsets::OFF64_MIGRATE_COMPONENT_COLUMN,
                offsets::OFF32_MIGRATE_COMPONENT_COLUMN,
            ),
        ];

        for (slot_idx, (native, off64, off32)) in all_slots.iter().enumerate() {
            assert_eq!(*off64, 8 + 8 * slot_idx);
            assert_eq!(*off32, 8 + 4 * slot_idx);
            #[cfg(target_pointer_width = "64")]
            assert_eq!(*native, *off64);
            #[cfg(target_pointer_width = "32")]
            assert_eq!(*native, *off32);
        }

        assert_eq!(
            BamHostApi::MIN_V0_2_SIZE,
            offsets::CMD_REMOVE + core::mem::size_of::<usize>()
        );
        assert_eq!(
            BamHostApi::MIN_V0_3_SIZE,
            offsets::MIGRATE_COMPONENT_COLUMN + core::mem::size_of::<usize>()
        );
    }
}
