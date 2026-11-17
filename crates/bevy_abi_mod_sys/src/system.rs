//! Opaque execution/load context handles and unified `BamQueryDesc` + `BamSystemDesc` descriptors.

use core::{
    ffi::{c_char, c_void},
    marker::{PhantomData, PhantomPinned},
};

use crate::{
    access::{BamTermDesc, MAX_QUERIES_PER_SYSTEM, MAX_TERMS_PER_QUERY, ScheduleId},
    commands::BamCommands,
    extensible::ExtensibleStruct,
    primitives::{BamComponent, FfiBool, FfiEntity, FfiResult},
};

/// Opaque world-scoped host registry and diagnostic context handle (`typedef struct BamHostCtx BamHostCtx;`).
#[repr(C)]
pub struct BamHostCtx {
    _private: [u8; 0],
    _marker: PhantomData<(*mut u8, PhantomPinned)>,
}

/// Opaque init-only transactional registration context handle (`typedef struct BamLoadCtx BamLoadCtx;`).
#[repr(C)]
pub struct BamLoadCtx {
    _private: [u8; 0],
    _marker: PhantomData<(*mut u8, PhantomPinned)>,
}

/// Opaque per-invocation system execution context handle (`typedef struct BamSystemCtx BamSystemCtx;`).
#[repr(C)]
pub struct BamSystemCtx {
    _private: [u8; 0],
    _marker: PhantomData<(*mut u8, PhantomPinned)>,
}

/// Unified guest system execution callback (`typedef BamResult (*BamSystemFn)(BamSystemCtx*, void*);`).
///
/// # Safety
/// - `sys_ctx` is valid strictly for the duration of the callback invocation.
/// - The guest callback must never unwind across the `extern "C"` boundary; panics in unwinding
///   Rust guests must be caught inside the guest runtime and reported as [`FfiResult::PANIC_CAUGHT`].
pub type BamSystemFn =
    unsafe extern "C" fn(sys_ctx: *mut BamSystemCtx, user_data: *mut c_void) -> FfiResult;

/// Optional guest run-condition predicate (`typedef BamBool (*BamRunIfFn)(void*);`).
///
/// Returns [`FfiBool::FALSE`] (`0`) to skip system execution for the current tick (preserving
/// `last_run` so `Changed<T>` / `Added<T>` ticks accumulated while skipped are still observed when
/// the predicate later returns non-zero), or any non-zero [`FfiBool`] to run.
///
/// # Safety
/// Must never unwind across the `extern "C"` boundary.
pub type BamRunIfFn = unsafe extern "C" fn(user_data: *mut c_void) -> FfiBool;

/// Bitmask of recognized [`BamQueryDesc::flags`] bits in ABI `0.2` (`0`).
pub const BAM_QUERY_KNOWN_FLAGS: u32 = 0;

/// Extensible query clause descriptor inside [`BamSystemDesc`] (`typedef struct BamQueryDesc BamQueryDesc;`).
///
/// Layout:
/// - 64-bit: `MIN_V0_2_SIZE = 24` bytes, `8`-byte alignment, zero implicit padding.
/// - 32-bit: `MIN_V0_2_SIZE = 16` bytes, `4`-byte alignment, zero implicit padding.
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct BamQueryDesc {
    /// Size of this struct in bytes (`offset 0`).
    pub struct_size: u32,
    /// Query bitflags (`0` in ABI `0.2`, `offset 4`).
    pub flags: u32,
    /// Pointer to `terms[0..term_count]` (null allowed iff `term_count == 0`, `offset 8`).
    pub terms: *const BamTermDesc,
    /// Number of terms in `terms` (`<= MAX_TERMS_PER_QUERY`, `offset 16` on 64-bit / `12` on 32-bit).
    pub term_count: usize,
}

impl BamQueryDesc {
    /// Frozen minimum valid `BamQueryDesc` size in ABI `0.2` (`24` on 64-bit, `16` on 32-bit).
    pub const MIN_V0_2_SIZE: usize =
        core::mem::offset_of!(Self, term_count) + core::mem::size_of::<usize>();

    /// Constructs a `BamQueryDesc` over `terms[0..term_count]` with `flags = 0`.
    #[inline]
    pub const fn new(terms: *const BamTermDesc, term_count: usize) -> Self {
        Self {
            struct_size: core::mem::size_of::<Self>() as u32,
            flags: 0,
            terms,
            term_count,
        }
    }

    /// Constructs an empty `BamQueryDesc` (`terms = null, term_count = 0`).
    #[inline]
    pub const fn empty() -> Self {
        Self::new(core::ptr::null(), 0)
    }

    /// Sets query bitflags on this descriptor.
    #[inline]
    pub const fn with_flags(mut self, flags: u32) -> Self {
        self.flags = flags;
        self
    }

    /// Validates `struct_size`, known `flags`, `term_count <= MAX_TERMS_PER_QUERY`, and pointer non-nullness.
    #[inline]
    pub const fn validate(&self) -> FfiResult {
        if (self.struct_size as usize) < Self::MIN_V0_2_SIZE {
            return FfiResult::INVALID_VERSION;
        }
        if (self.flags & !BAM_QUERY_KNOWN_FLAGS) != 0 {
            return FfiResult::UNSUPPORTED;
        }
        if self.term_count > MAX_TERMS_PER_QUERY {
            return FfiResult::CAPACITY_EXCEEDED;
        }
        if self.term_count > 0 && self.terms.is_null() {
            return FfiResult::NULL_POINTER;
        }
        FfiResult::OK
    }
}

const _: () = {
    assert!(BamQueryDesc::MIN_V0_2_SIZE <= core::mem::size_of::<BamQueryDesc>());
    #[cfg(target_pointer_width = "64")]
    {
        assert!(BamQueryDesc::MIN_V0_2_SIZE == 24);
        assert!(core::mem::size_of::<BamQueryDesc>() == 24);
        assert!(core::mem::align_of::<BamQueryDesc>() == 8);
        assert!(core::mem::offset_of!(BamQueryDesc, struct_size) == 0);
        assert!(core::mem::offset_of!(BamQueryDesc, flags) == 4);
        assert!(core::mem::offset_of!(BamQueryDesc, terms) == 8);
        assert!(core::mem::offset_of!(BamQueryDesc, term_count) == 16);
    }
    #[cfg(target_pointer_width = "32")]
    {
        assert!(BamQueryDesc::MIN_V0_2_SIZE == 16);
        assert!(core::mem::size_of::<BamQueryDesc>() == 16);
        assert!(core::mem::align_of::<BamQueryDesc>() == 4);
        assert!(core::mem::offset_of!(BamQueryDesc, struct_size) == 0);
        assert!(core::mem::offset_of!(BamQueryDesc, flags) == 4);
        assert!(core::mem::offset_of!(BamQueryDesc, terms) == 8);
        assert!(core::mem::offset_of!(BamQueryDesc, term_count) == 12);
    }
};

// SAFETY: `BamQueryDesc` is `#[repr(C)]`, starts with `struct_size: u32` at offset 0, and all
// fields are valid when zero-filled.
unsafe impl ExtensibleStruct for BamQueryDesc {
    const MIN_V0_2_SIZE: usize = Self::MIN_V0_2_SIZE;
}

/// Pins the registered system onto Bevy's main thread (`NonSend` scheduling constraint).
pub const BAM_SYSTEM_FLAG_MAIN_THREAD: u32 = 1 << 0;

/// Bitmask of all recognized [`BamSystemDesc::flags`] bits in ABI `0.2`.
pub const BAM_SYSTEM_KNOWN_FLAGS: u32 = BAM_SYSTEM_FLAG_MAIN_THREAD;

/// Unified extensible system registration descriptor (`typedef struct BamSystemDesc BamSystemDesc;`).
///
/// Replaces all six v0.1 single-query, multi-query, scheduled, and conditional descriptor variants
/// with a single zero-padding structure covering `0..=MAX_QUERIES_PER_SYSTEM` queries, custom or
/// built-in schedules, `before`/`after` system-set ordering, explicit `.in_set()` membership,
/// main-thread pinning, and optional `run_if` predicates.
///
/// Layout:
/// - 64-bit: `MIN_V0_2_SIZE = 104` bytes (`size_of = 120` bytes with `in_sets`), `8`-byte alignment, zero implicit padding (`reserved0` explicit).
/// - 32-bit: `MIN_V0_2_SIZE = 60` bytes (`size_of = 68` bytes with `in_sets`), `4`-byte alignment, zero implicit padding.
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct BamSystemDesc {
    /// Size of this struct in bytes (`offset 0`).
    pub struct_size: u32,
    /// Bitmask of `BAM_SYSTEM_FLAG_*` (`offset 4`).
    pub flags: u32,
    /// Mandatory NUL-terminated UTF-8 system name (`offset 8`).
    pub name: *const c_char,
    /// Pointer to `queries[0..query_count]` (null allowed iff `query_count == 0`, `offset 16` / `12`).
    pub queries: *const BamQueryDesc,
    /// Number of queries in `queries` (`<= MAX_QUERIES_PER_SYSTEM`, `offset 24` / `16`).
    pub query_count: usize,
    /// Mandatory system execution callback (`Some`, `offset 32` / `20`).
    pub callback: Option<BamSystemFn>,
    /// Opaque guest-owned closure state passed to `callback` (`offset 40` / `24`).
    pub user_data: *mut c_void,
    /// Target Bevy schedule (`0` = `Update`, `1..=5` built-in, `>= 1024` custom, `offset 48` / `28`).
    pub schedule: ScheduleId,
    /// Explicit zero-reserved alignment field (`must be 0`, `offset 52` / `32`).
    pub reserved0: u32,
    /// Optional array of NUL-terminated ordering target names (`after[0..after_count]`, `offset 56` / `36`).
    pub after: *const *const c_char,
    /// Number of entries in `after` (`offset 64` / `40`).
    pub after_count: usize,
    /// Optional array of NUL-terminated ordering target names (`before[0..before_count]`, `offset 72` / `44`).
    pub before: *const *const c_char,
    /// Number of entries in `before` (`offset 80` / `48`).
    pub before_count: usize,
    /// Optional run-condition predicate (`None` = always run, `offset 88` / `52`).
    pub run_if: Option<BamRunIfFn>,
    /// Opaque guest-owned state passed to `run_if` (`offset 96` / `56`).
    pub run_if_user_data: *mut c_void,
    /// Optional array of NUL-terminated system-set names that this system belongs to (`in_sets[0..in_set_count]`, `offset 104` / `60`).
    pub in_sets: *const *const c_char,
    /// Number of entries in `in_sets` (`offset 112` / `64`).
    pub in_set_count: usize,
}

impl BamSystemDesc {
    /// Frozen minimum valid `BamSystemDesc` size in ABI `0.2` (`104` on 64-bit, `60` on 32-bit).
    pub const MIN_V0_2_SIZE: usize =
        core::mem::offset_of!(Self, run_if_user_data) + core::mem::size_of::<*mut c_void>();

    /// Constructs a `BamSystemDesc` targeting [`ScheduleId::UPDATE`] with no flags, ordering
    /// constraints, set memberships, or `run_if` predicate.
    #[inline]
    pub const fn new(
        name: *const c_char,
        queries: *const BamQueryDesc,
        query_count: usize,
        callback: BamSystemFn,
        user_data: *mut c_void,
    ) -> Self {
        Self {
            struct_size: core::mem::size_of::<Self>() as u32,
            flags: 0,
            name,
            queries,
            query_count,
            callback: Some(callback),
            user_data,
            schedule: ScheduleId::UPDATE,
            reserved0: 0,
            after: core::ptr::null(),
            after_count: 0,
            before: core::ptr::null(),
            before_count: 0,
            run_if: None,
            run_if_user_data: core::ptr::null_mut(),
            in_sets: core::ptr::null(),
            in_set_count: 0,
        }
    }

    /// Sets `BAM_SYSTEM_FLAG_*` bitflags on this descriptor.
    #[inline]
    pub const fn with_flags(mut self, flags: u32) -> Self {
        self.flags = flags;
        self
    }

    /// Pins this system to Bevy's main thread (`BAM_SYSTEM_FLAG_MAIN_THREAD`).
    #[inline]
    pub const fn with_main_thread(mut self) -> Self {
        self.flags |= BAM_SYSTEM_FLAG_MAIN_THREAD;
        self
    }

    /// Selects the target [`ScheduleId`] for this system.
    #[inline]
    pub const fn with_schedule(mut self, schedule: ScheduleId) -> Self {
        self.schedule = schedule;
        self
    }

    /// Declares NUL-terminated system-set names that this system must execute after.
    #[inline]
    pub const fn with_after(mut self, after: *const *const c_char, after_count: usize) -> Self {
        self.after = after;
        self.after_count = after_count;
        self
    }

    /// Declares NUL-terminated system-set names that this system must execute before.
    #[inline]
    pub const fn with_before(mut self, before: *const *const c_char, before_count: usize) -> Self {
        self.before = before;
        self.before_count = before_count;
        self
    }

    /// Declares NUL-terminated system-set names (`.in_set(...)`) that this system joins.
    #[inline]
    pub const fn with_in_sets(
        mut self,
        in_sets: *const *const c_char,
        in_set_count: usize,
    ) -> Self {
        self.in_sets = in_sets;
        self.in_set_count = in_set_count;
        self
    }

    /// Attaches an optional `run_if` condition callback and its user-data pointer.
    #[inline]
    pub const fn with_run_if(mut self, run_if: BamRunIfFn, run_if_user_data: *mut c_void) -> Self {
        self.run_if = Some(run_if);
        self.run_if_user_data = run_if_user_data;
        self
    }

    /// Returns `true` if `BAM_SYSTEM_FLAG_MAIN_THREAD` is set.
    #[inline]
    pub const fn is_main_thread(&self) -> bool {
        (self.flags & BAM_SYSTEM_FLAG_MAIN_THREAD) != 0
    }

    /// Validates `struct_size`, known `flags`, `reserved0 == 0`, mandatory pointers, query cap,
    /// and `schedule` validity.
    #[inline]
    pub const fn validate(&self) -> FfiResult {
        if (self.struct_size as usize) < Self::MIN_V0_2_SIZE {
            return FfiResult::INVALID_VERSION;
        }
        if (self.flags & !BAM_SYSTEM_KNOWN_FLAGS) != 0 {
            return FfiResult::UNSUPPORTED;
        }
        if self.reserved0 != 0 {
            return FfiResult::INVALID_ARGUMENT;
        }
        if self.name.is_null() || self.callback.is_none() {
            return FfiResult::NULL_POINTER;
        }
        if self.query_count > MAX_QUERIES_PER_SYSTEM {
            return FfiResult::CAPACITY_EXCEEDED;
        }
        if (self.query_count > 0 && self.queries.is_null())
            || (self.after_count > 0 && self.after.is_null())
            || (self.before_count > 0 && self.before.is_null())
            || (self.in_set_count > 0 && self.in_sets.is_null())
        {
            return FfiResult::NULL_POINTER;
        }
        if !self.schedule.is_valid() {
            return FfiResult::INVALID_ARGUMENT;
        }
        FfiResult::OK
    }
}

const _: () = {
    assert!(BamSystemDesc::MIN_V0_2_SIZE <= core::mem::size_of::<BamSystemDesc>());
    #[cfg(target_pointer_width = "64")]
    {
        assert!(BamSystemDesc::MIN_V0_2_SIZE == 104);
        assert!(core::mem::size_of::<BamSystemDesc>() == 120);
        assert!(core::mem::align_of::<BamSystemDesc>() == 8);
        assert!(core::mem::offset_of!(BamSystemDesc, struct_size) == 0);
        assert!(core::mem::offset_of!(BamSystemDesc, flags) == 4);
        assert!(core::mem::offset_of!(BamSystemDesc, name) == 8);
        assert!(core::mem::offset_of!(BamSystemDesc, queries) == 16);
        assert!(core::mem::offset_of!(BamSystemDesc, query_count) == 24);
        assert!(core::mem::offset_of!(BamSystemDesc, callback) == 32);
        assert!(core::mem::offset_of!(BamSystemDesc, user_data) == 40);
        assert!(core::mem::offset_of!(BamSystemDesc, schedule) == 48);
        assert!(core::mem::offset_of!(BamSystemDesc, reserved0) == 52);
        assert!(core::mem::offset_of!(BamSystemDesc, after) == 56);
        assert!(core::mem::offset_of!(BamSystemDesc, after_count) == 64);
        assert!(core::mem::offset_of!(BamSystemDesc, before) == 72);
        assert!(core::mem::offset_of!(BamSystemDesc, before_count) == 80);
        assert!(core::mem::offset_of!(BamSystemDesc, run_if) == 88);
        assert!(core::mem::offset_of!(BamSystemDesc, run_if_user_data) == 96);
        assert!(core::mem::offset_of!(BamSystemDesc, in_sets) == 104);
        assert!(core::mem::offset_of!(BamSystemDesc, in_set_count) == 112);
    }
    #[cfg(target_pointer_width = "32")]
    {
        assert!(BamSystemDesc::MIN_V0_2_SIZE == 60);
        assert!(core::mem::size_of::<BamSystemDesc>() == 68);
        assert!(core::mem::align_of::<BamSystemDesc>() == 4);
        assert!(core::mem::offset_of!(BamSystemDesc, struct_size) == 0);
        assert!(core::mem::offset_of!(BamSystemDesc, flags) == 4);
        assert!(core::mem::offset_of!(BamSystemDesc, name) == 8);
        assert!(core::mem::offset_of!(BamSystemDesc, queries) == 12);
        assert!(core::mem::offset_of!(BamSystemDesc, query_count) == 16);
        assert!(core::mem::offset_of!(BamSystemDesc, callback) == 20);
        assert!(core::mem::offset_of!(BamSystemDesc, user_data) == 24);
        assert!(core::mem::offset_of!(BamSystemDesc, schedule) == 28);
        assert!(core::mem::offset_of!(BamSystemDesc, reserved0) == 32);
        assert!(core::mem::offset_of!(BamSystemDesc, after) == 36);
        assert!(core::mem::offset_of!(BamSystemDesc, after_count) == 40);
        assert!(core::mem::offset_of!(BamSystemDesc, before) == 44);
        assert!(core::mem::offset_of!(BamSystemDesc, before_count) == 48);
        assert!(core::mem::offset_of!(BamSystemDesc, run_if) == 52);
        assert!(core::mem::offset_of!(BamSystemDesc, run_if_user_data) == 56);
        assert!(core::mem::offset_of!(BamSystemDesc, in_sets) == 60);
        assert!(core::mem::offset_of!(BamSystemDesc, in_set_count) == 64);
    }
};

// SAFETY: `BamSystemDesc` is `#[repr(C)]`, starts with `struct_size: u32` at offset 0, and every
// field (`Option<BamSystemFn>`, `Option<BamRunIfFn>`, raw pointers, integers) is zero-valid.
unsafe impl ExtensibleStruct for BamSystemDesc {
    const MIN_V0_2_SIZE: usize = Self::MIN_V0_2_SIZE;
}

/// Guest callback invoked when an observed `Event` or `EntityEvent` fires
/// (`typedef BamResult (*BamObserverFn)(BamEntity entity, const void* event_data, BamCommands* commands, void* user_data);`).
///
/// # Safety
/// - `event_data` points to the triggered event payload valid strictly for the duration of the call.
/// - `commands` points to a valid deferred [`BamCommands`] queue scoped to this observer invocation.
/// - Must never unwind across the `extern "C"` boundary.
pub type BamObserverFn = unsafe extern "C" fn(
    entity: FfiEntity,
    event_data: *const c_void,
    commands: *mut BamCommands,
    user_data: *mut c_void,
) -> FfiResult;

/// Bitmask of recognized [`BamObserverDesc::flags`] bits in ABI `0.2` (`0`).
pub const BAM_OBSERVER_KNOWN_FLAGS: u32 = 0;

/// Extensible dynamic observer registration descriptor (`typedef struct BamObserverDesc BamObserverDesc;`).
///
/// Layout:
/// - 64-bit: `MIN_V0_2_SIZE = 48` bytes, `8`-byte alignment, zero implicit padding.
/// - 32-bit: `MIN_V0_2_SIZE = 36` bytes, `4`-byte alignment, zero implicit padding.
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct BamObserverDesc {
    /// Size of this struct in bytes (`offset 0`).
    pub struct_size: u32,
    /// Observer bitflags (`0` in ABI `0.2`, `offset 4`).
    pub flags: u32,
    /// Optional NUL-terminated UTF-8 diagnostic observer name (`offset 8`).
    pub name: *const c_char,
    /// Pre-resolved [`BamComponent`] handle of the observed `Event` / `EntityEvent` (`offset 16` / `12`).
    pub event: BamComponent,
    /// Lower 32 bits of optional target [`FfiEntity`] (`0` when global, `offset 20` / `16`).
    pub entity_lo: u32,
    /// Upper 32 bits of optional target [`FfiEntity`] (`0` when global, `offset 24` / `20`).
    pub entity_hi: u32,
    /// Explicit zero-reserved field (`must be 0`, `offset 28` / `24`).
    pub reserved0: u32,
    /// Mandatory observer callback (`Some`, `offset 32` / `28`).
    pub callback: Option<BamObserverFn>,
    /// Opaque guest-owned state passed to `callback` (`offset 40` / `32`).
    pub user_data: *mut c_void,
}

impl BamObserverDesc {
    /// Frozen minimum valid `BamObserverDesc` size (`48` on 64-bit, `36` on 32-bit).
    pub const MIN_V0_2_SIZE: usize =
        core::mem::offset_of!(Self, user_data) + core::mem::size_of::<*mut c_void>();

    /// Constructs a global observer descriptor watching `event` with `callback(entity, event_data, commands, user_data)`.
    #[inline]
    pub const fn new(event: BamComponent, callback: BamObserverFn, user_data: *mut c_void) -> Self {
        Self {
            struct_size: core::mem::size_of::<Self>() as u32,
            flags: 0,
            name: core::ptr::null(),
            event,
            entity_lo: 0,
            entity_hi: 0,
            reserved0: 0,
            callback: Some(callback),
            user_data,
        }
    }

    /// Sets an optional NUL-terminated UTF-8 diagnostic name on this observer.
    #[inline]
    pub const fn with_name(mut self, name: *const c_char) -> Self {
        self.name = name;
        self
    }

    /// Targets a specific entity (`FfiEntity::NULL` restores global observation).
    #[inline]
    pub const fn with_entity(mut self, entity: FfiEntity) -> Self {
        let bits = entity.to_bits();
        self.entity_lo = bits as u32;
        self.entity_hi = (bits >> 32) as u32;
        self
    }

    /// Reconstructs the configured target [`FfiEntity`] (`FfiEntity::NULL` when global).
    #[inline]
    pub const fn entity(&self) -> FfiEntity {
        FfiEntity::from_bits(((self.entity_hi as u64) << 32) | (self.entity_lo as u64))
    }

    /// Validates `struct_size`, known `flags`, `reserved0 == 0`, valid `event`, and non-null `callback`.
    #[inline]
    pub const fn validate(&self) -> FfiResult {
        if (self.struct_size as usize) < Self::MIN_V0_2_SIZE {
            return FfiResult::INVALID_VERSION;
        }
        if (self.flags & !BAM_OBSERVER_KNOWN_FLAGS) != 0 {
            return FfiResult::UNSUPPORTED;
        }
        if self.reserved0 != 0 {
            return FfiResult::INVALID_ARGUMENT;
        }
        if !self.event.is_valid() {
            return FfiResult::INVALID_COMPONENT;
        }
        if self.callback.is_none() {
            return FfiResult::NULL_POINTER;
        }
        FfiResult::OK
    }
}

const _: () = {
    assert!(BamObserverDesc::MIN_V0_2_SIZE <= core::mem::size_of::<BamObserverDesc>());
    #[cfg(target_pointer_width = "64")]
    {
        assert!(BamObserverDesc::MIN_V0_2_SIZE == 48);
        assert!(core::mem::size_of::<BamObserverDesc>() == 48);
        assert!(core::mem::align_of::<BamObserverDesc>() == 8);
        assert!(core::mem::offset_of!(BamObserverDesc, struct_size) == 0);
        assert!(core::mem::offset_of!(BamObserverDesc, flags) == 4);
        assert!(core::mem::offset_of!(BamObserverDesc, name) == 8);
        assert!(core::mem::offset_of!(BamObserverDesc, event) == 16);
        assert!(core::mem::offset_of!(BamObserverDesc, entity_lo) == 20);
        assert!(core::mem::offset_of!(BamObserverDesc, entity_hi) == 24);
        assert!(core::mem::offset_of!(BamObserverDesc, reserved0) == 28);
        assert!(core::mem::offset_of!(BamObserverDesc, callback) == 32);
        assert!(core::mem::offset_of!(BamObserverDesc, user_data) == 40);
    }
    #[cfg(target_pointer_width = "32")]
    {
        assert!(BamObserverDesc::MIN_V0_2_SIZE == 36);
        assert!(core::mem::size_of::<BamObserverDesc>() == 36);
        assert!(core::mem::align_of::<BamObserverDesc>() == 4);
        assert!(core::mem::offset_of!(BamObserverDesc, struct_size) == 0);
        assert!(core::mem::offset_of!(BamObserverDesc, flags) == 4);
        assert!(core::mem::offset_of!(BamObserverDesc, name) == 8);
        assert!(core::mem::offset_of!(BamObserverDesc, event) == 12);
        assert!(core::mem::offset_of!(BamObserverDesc, entity_lo) == 16);
        assert!(core::mem::offset_of!(BamObserverDesc, entity_hi) == 20);
        assert!(core::mem::offset_of!(BamObserverDesc, reserved0) == 24);
        assert!(core::mem::offset_of!(BamObserverDesc, callback) == 28);
        assert!(core::mem::offset_of!(BamObserverDesc, user_data) == 32);
    }
};

// SAFETY: `BamObserverDesc` is `#[repr(C)]`, starts with `struct_size: u32` at offset 0, and all
// fields are valid when zero-filled.
unsafe impl ExtensibleStruct for BamObserverDesc {
    const MIN_V0_2_SIZE: usize = Self::MIN_V0_2_SIZE;
}
