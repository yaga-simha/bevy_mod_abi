//! Complete `Option<fn>` `BamHostApi` table, `BamManifest`, and per-instance lifecycle typedefs.

use core::{
    ffi::{c_char, c_void},
    marker::{PhantomData, PhantomPinned},
};

use crate::{
    commands::{
        BamCmdDespawnFn, BamCmdInsertDefaultFn, BamCmdInsertFn, BamCmdRemoveFn, BamCmdSpawnEmptyFn,
        BamCmdSpawnFn, BamCmdTriggerFn, BamCommands,
    },
    component::ComponentDesc,
    extensible::ExtensibleStruct,
    iter::{
        BamChangeTicks, BamIter, BamIterColumnInfoFn, BamIterColumnMutableFn,
        BamIterColumnMutableUntrackedFn, BamIterColumnReadonlyFn, BamIterColumnRowsMutableFn,
        BamIterColumnRowsReadonlyFn, BamIterEntitiesFn, BamIterMarkChangedRangeFn,
        BamIterNextChunkFn, BamIterResetFn, BamMsgReadBatchFn, BamMsgSendFn, BamMsgWriteBatchFn,
        BamParForEachChunkFn, BamResMutPtrFn, BamResPtrFn, BamResTicksFn,
    },
    primitives::{BamComponent, ComponentUuid, FfiEntity, FfiResult},
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

/// Complete versioned host function table passed to `bevy_abi_mod_init`
/// (`typedef struct BamHostApi BamHostApi;`).
///
/// Every function slot uses `Option<unsafe extern "C" fn(...)>` so zero-extended future slots and
/// null-pointer checks are sound without raw pointer byte-probing.
///
/// Layout:
/// - 64-bit: `MIN_V0_2_SIZE = 224` bytes (`8`B header + `27 * 8`B core slots; `size_of = 312` bytes with 11 optional extended slots), `8`-byte alignment.
/// - 32-bit: `MIN_V0_2_SIZE = 116` bytes (`8`B header + `27 * 4`B core slots; `size_of = 160` bytes with 11 optional extended slots), `4`-byte alignment.
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
}

impl BamHostApi {
    /// Frozen minimum valid `BamHostApi` size in ABI `0.2` (`224` on 64-bit, `116` on 32-bit).
    pub const MIN_V0_2_SIZE: usize =
        core::mem::offset_of!(Self, cmd_remove) + core::mem::size_of::<Option<BamCmdRemoveFn>>();

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

    /// Returns `true` if all 38 ABI `0.2` core and extended function-pointer slots are `Some`.
    #[inline]
    pub const fn has_all_slots(&self) -> bool {
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
    #[cfg(target_pointer_width = "64")]
    {
        assert!(BamHostApi::MIN_V0_2_SIZE == 224);
        assert!(core::mem::size_of::<BamHostApi>() == 312);
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
    }
    #[cfg(target_pointer_width = "32")]
    {
        assert!(BamHostApi::MIN_V0_2_SIZE == 116);
        assert!(core::mem::size_of::<BamHostApi>() == 160);
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
    }
};

// SAFETY: `BamHostApi` is `#[repr(C)]`, starts with `struct_size: u32` at offset 0, and every
// function pointer field is `Option<unsafe extern "C" fn(...)>` (all-bits-zero = `None`).
unsafe impl ExtensibleStruct for BamHostApi {
    const MIN_V0_2_SIZE: usize = Self::MIN_V0_2_SIZE;
}
