//! Validated handle-based and batched deferred command wrappers over [`BamCommands`].

use core::marker::PhantomData;

use bevy_abi_mod_sys::{AbiPod, BamCommands, BamComponent, FfiEntity, FfiResult};

use crate::host::Host;

/// Stack scratch capacity before falling back to a temporary `Vec` in [`ValidatedCommands::spawn`].
const INLINE_SPAWN_CAP: usize = 16;

/// Type-erased `(BamComponent, *const u8)` pair for batched single-archetype [`ValidatedCommands::spawn`].
///
/// Constructed safely via [`SpawnEntry::new`] from any `T: AbiPod` reference without materializing
/// `&[u8]` over struct padding bytes.
#[derive(Clone, Copy, Debug)]
pub struct SpawnEntry<'a> {
    component: BamComponent,
    data_ptr: *const u8,
    _marker: PhantomData<&'a ()>,
}

impl<'a> SpawnEntry<'a> {
    /// Binds a pre-resolved [`BamComponent`] handle to an `AbiPod` component value for batched spawn.
    #[inline]
    pub const fn new<T: AbiPod>(component: BamComponent, value: &'a T) -> Self {
        Self {
            component,
            data_ptr: core::ptr::from_ref(value).cast::<u8>(),
            _marker: PhantomData,
        }
    }

    /// Binds a pre-resolved zero-sized marker [`BamComponent`] handle (`size == 0`) with a non-null
    /// dangling data pointer.
    #[inline]
    pub const fn marker(component: BamComponent) -> Self {
        Self {
            component,
            data_ptr: core::ptr::NonNull::<u8>::dangling().as_ptr(),
            _marker: PhantomData,
        }
    }

    /// Constructs a raw `SpawnEntry` from a caller-managed byte pointer.
    ///
    /// # Safety
    /// If the registered size of `component` is non-zero, `data_ptr` must be non-null and readable
    /// for that many bytes for lifetime `'a`.
    #[inline]
    pub const unsafe fn from_raw(component: BamComponent, data_ptr: *const u8) -> Self {
        Self {
            component,
            data_ptr,
            _marker: PhantomData,
        }
    }

    /// Returns the [`BamComponent`] handle for this entry.
    #[inline]
    pub const fn component(&self) -> BamComponent {
        self.component
    }

    /// Returns the raw payload pointer for this entry.
    #[inline]
    pub const fn data_ptr(&self) -> *const u8 {
        self.data_ptr
    }
}

/// Validated deferred command buffer wrapper bound to an active system invocation.
pub struct ValidatedCommands<'a> {
    raw: *mut BamCommands,
    host: &'a Host,
}

impl<'a> ValidatedCommands<'a> {
    /// Wraps a raw [`BamCommands`] pointer obtained from `BamHostApi::sys_commands`.
    ///
    /// A null `raw` pointer constructs a disabled buffer whose command methods fail cleanly with
    /// [`FfiResult::NULL_POINTER`].
    ///
    /// # Safety
    /// If `raw` is non-null, it must point to the active [`BamCommands`] belonging to the current
    /// system invocation on `host` and must not outlive `'a`.
    #[inline]
    pub const unsafe fn from_raw(raw: *mut BamCommands, host: &'a Host) -> Self {
        Self { raw, host }
    }

    /// Constructs a disabled command buffer (`raw = null`) that rejects operations with
    /// [`FfiResult::NULL_POINTER`].
    #[inline]
    pub const fn empty(host: &'a Host) -> Self {
        Self {
            raw: core::ptr::null_mut(),
            host,
        }
    }

    /// Returns `true` if this command buffer holds a non-null [`BamCommands`] handle.
    #[inline]
    pub fn is_valid(&self) -> bool {
        !self.raw.is_null()
    }

    /// Returns the underlying `*mut BamCommands` pointer.
    #[inline]
    pub const fn as_raw(&self) -> *mut BamCommands {
        self.raw
    }

    /// Returns the cached [`Host`] table for this system invocation.
    #[inline]
    pub const fn host(&self) -> &'a Host {
        self.host
    }

    /// Reserves a fresh empty entity atomically and returns its [`FfiEntity`] identifier.
    #[inline]
    pub fn spawn_empty(&mut self) -> Result<FfiEntity, FfiResult> {
        if self.raw.is_null() {
            return Err(FfiResult::NULL_POINTER);
        }
        let Some(spawn_empty_fn) = self.host.api().cmd_spawn_empty else {
            return Err(FfiResult::NULL_POINTER);
        };
        let mut out = FfiEntity::NULL;
        // SAFETY: `self.raw` is a valid active `BamCommands` for `'a` and `out` is writable stack storage.
        let status = unsafe { spawn_empty_fn(self.raw, &mut out) };
        if status.is_ok() { Ok(out) } else { Err(status) }
    }

    /// Reserves an entity and batch-inserts `entries` in a single archetype placement via
    /// `BamHostApi::cmd_spawn`.
    ///
    /// Note: The caller must ensure each [`SpawnEntry`]'s `T` matches the exact byte size and layout
    /// registered for its [`BamComponent`] handle.
    pub fn spawn(&mut self, entries: &[SpawnEntry<'_>]) -> Result<FfiEntity, FfiResult> {
        if entries.len() <= INLINE_SPAWN_CAP {
            let mut comps = [BamComponent::INVALID; INLINE_SPAWN_CAP];
            let mut ptrs = [core::ptr::null::<u8>(); INLINE_SPAWN_CAP];
            for (i, entry) in entries.iter().enumerate() {
                if !entry.component.is_valid() {
                    return Err(FfiResult::INVALID_COMPONENT);
                }
                comps[i] = entry.component;
                ptrs[i] = entry.data_ptr;
            }
            // SAFETY: Each `SpawnEntry` holds a valid `T: AbiPod` reference or caller-verified raw
            // payload for the duration of `entries`, and slice lengths match `entries.len()`.
            unsafe { self.spawn_raw(&comps[..entries.len()], &ptrs[..entries.len()]) }
        } else {
            let mut comps = Vec::with_capacity(entries.len());
            let mut ptrs = Vec::with_capacity(entries.len());
            for entry in entries {
                if !entry.component.is_valid() {
                    return Err(FfiResult::INVALID_COMPONENT);
                }
                comps.push(entry.component);
                ptrs.push(entry.data_ptr);
            }
            // SAFETY: `comps` and `ptrs` have identical length and point to live `SpawnEntry` payloads.
            unsafe { self.spawn_raw(&comps, &ptrs) }
        }
    }

    /// Raw batched entity spawn forwarding parallel `components` and `data_ptrs` slices to
    /// `BamHostApi::cmd_spawn`.
    ///
    /// # Safety
    /// For every `i` in `0..components.len()`, if the registered byte size of `components[i]` is
    /// non-zero, `data_ptrs[i]` must be non-null and readable for that registered byte size.
    pub unsafe fn spawn_raw(
        &mut self,
        components: &[BamComponent],
        data_ptrs: &[*const u8],
    ) -> Result<FfiEntity, FfiResult> {
        if self.raw.is_null() {
            return Err(FfiResult::NULL_POINTER);
        }
        if components.len() != data_ptrs.len() {
            return Err(FfiResult::INVALID_ARGUMENT);
        }
        let Some(spawn_fn) = self.host.api().cmd_spawn else {
            return Err(FfiResult::NULL_POINTER);
        };
        let count = components.len();
        let comps_ptr = if count == 0 {
            core::ptr::null()
        } else {
            components.as_ptr()
        };
        let ptrs_ptr = if count == 0 {
            core::ptr::null()
        } else {
            data_ptrs.as_ptr()
        };
        let mut out = FfiEntity::NULL;
        // SAFETY: `self.raw` is valid, `comps_ptr`/`ptrs_ptr` cover `count` elements, and caller
        // guarantees each `data_ptrs[i]` matches `components[i]`'s registered size.
        let status = unsafe { spawn_fn(self.raw, comps_ptr, ptrs_ptr, count, &mut out) };
        if status.is_ok() { Ok(out) } else { Err(status) }
    }

    /// Queues deferred despawning of `entity`.
    #[inline]
    pub fn despawn(&mut self, entity: FfiEntity) -> FfiResult {
        if self.raw.is_null() {
            return FfiResult::NULL_POINTER;
        }
        if entity.is_null() {
            return FfiResult::INVALID_ENTITY;
        }
        let Some(despawn_fn) = self.host.api().cmd_despawn else {
            return FfiResult::NULL_POINTER;
        };
        // SAFETY: `self.raw` is an active `BamCommands` handle for `'a`.
        unsafe { despawn_fn(self.raw, entity) }
    }

    /// Queues deferred insertion of `value` (`T: AbiPod`) onto `entity` using pre-resolved
    /// `component` handle.
    #[inline]
    pub fn insert<T: AbiPod>(
        &mut self,
        entity: FfiEntity,
        component: BamComponent,
        value: &T,
    ) -> FfiResult {
        let data_ptr = core::ptr::from_ref(value).cast::<u8>();
        let data_len = core::mem::size_of::<T>();
        // SAFETY: `value` is a live `&T` (`T: AbiPod`) readable via raw byte copy for `size_of::<T>()`
        // bytes without forming `&[u8]` over any padding bytes.
        unsafe { self.insert_raw(entity, component, data_ptr, data_len) }
    }

    /// Queues deferred raw byte insertion (`data_ptr[0..data_len]`) for `component` onto `entity`.
    ///
    /// # Safety
    /// If `data_len > 0`, `data_ptr` must be non-null and readable for `data_len` bytes matching
    /// the registered component layout (and ownership invariants if `component` has a `drop_fn`).
    #[inline]
    pub unsafe fn insert_raw(
        &mut self,
        entity: FfiEntity,
        component: BamComponent,
        data_ptr: *const u8,
        data_len: usize,
    ) -> FfiResult {
        if self.raw.is_null() {
            return FfiResult::NULL_POINTER;
        }
        if entity.is_null() {
            return FfiResult::INVALID_ENTITY;
        }
        if !component.is_valid() {
            return FfiResult::INVALID_COMPONENT;
        }
        if data_len > 0 && data_ptr.is_null() {
            return FfiResult::NULL_POINTER;
        }
        let Some(insert_fn) = self.host.api().cmd_insert else {
            return FfiResult::NULL_POINTER;
        };
        // SAFETY: `self.raw` is active and caller guarantees `data_ptr` is readable for `data_len` bytes.
        unsafe { insert_fn(self.raw, entity, component, data_ptr, data_len) }
    }

    /// Queues deferred insertion of host-constructed `T::default()` for `component` onto `entity`.
    #[inline]
    pub fn insert_default(&mut self, entity: FfiEntity, component: BamComponent) -> FfiResult {
        if self.raw.is_null() {
            return FfiResult::NULL_POINTER;
        }
        if entity.is_null() {
            return FfiResult::INVALID_ENTITY;
        }
        if !component.is_valid() {
            return FfiResult::INVALID_COMPONENT;
        }
        let Some(insert_default_fn) = self.host.api().cmd_insert_default else {
            return FfiResult::NULL_POINTER;
        };
        // SAFETY: `self.raw` is active and `entity`/`component` were checked non-zero.
        unsafe { insert_default_fn(self.raw, entity, component) }
    }

    /// Queues deferred removal of `component` from `entity`.
    #[inline]
    pub fn remove(&mut self, entity: FfiEntity, component: BamComponent) -> FfiResult {
        if self.raw.is_null() {
            return FfiResult::NULL_POINTER;
        }
        if entity.is_null() {
            return FfiResult::INVALID_ENTITY;
        }
        if !component.is_valid() {
            return FfiResult::INVALID_COMPONENT;
        }
        let Some(remove_fn) = self.host.api().cmd_remove else {
            return FfiResult::NULL_POINTER;
        };
        // SAFETY: `self.raw` is active and `entity`/`component` were checked non-zero.
        unsafe { remove_fn(self.raw, entity, component) }
    }
}
