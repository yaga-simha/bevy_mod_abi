//! Dynamic system callbacks, query specifications, and extensible system descriptors.

use core::ffi::{c_char, c_void};

use crate::{
    access::{ComponentAccessDesc, ScheduleLabelId},
    commands::RawCommandQueue,
    extensible::ExtensibleStruct,
    iter::RawChunkIter,
    primitives::{FFI_BOOL_FALSE, FfiBool, FfiResult},
};

// Keep doc link to FFI_BOOL_FALSE active without unused-import warnings.
const _: FfiBool = FFI_BOOL_FALSE;

/// Optional guest run-condition predicate evaluated before a dynamic system executes.
///
/// Returns [`FFI_BOOL_FALSE`] (`0`) to skip system execution for the current tick (preserving
/// `last_run` so `Changed<T>` and `Added<T>` ticks accumulated while skipped are still observed
/// when the condition later returns non-zero), or any non-zero [`FfiBool`] value to execute.
///
/// # Safety
/// - Must contain any internal panics; unwinding across the `extern "C"` boundary is undefined behavior.
pub type RunConditionCallback = unsafe extern "C" fn(user_data: *mut c_void) -> FfiBool;

/// Dynamic system callback invoked by the host during schedule execution.
///
/// # Safety
/// - Invoked by the host ECS scheduler.
/// - The guest system must contain any internal panics and return [`FfiResult::PANIC_CAUGHT`].
/// - Handles passed in `iter` and `commands` are valid only for the duration of the callback.
pub type DynamicSystemCallback = unsafe extern "C" fn(
    iter: *mut RawChunkIter,
    commands: *mut RawCommandQueue,
    user_data: *mut c_void,
) -> FfiResult;

/// Specification of one query clause within a multi-query dynamic system.
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct QuerySpec {
    /// Pointer to an array of `ComponentAccessDesc` of length `query_count`.
    pub queries: *const ComponentAccessDesc,
    /// Number of elements in `queries`.
    pub query_count: usize,
}

/// Callback signature for a dynamic system executing multiple queries (e.g., nested queries).
///
/// # Safety
/// - `iters` points to an array of `*mut RawChunkIter` of length `iter_count`.
/// - Guest must contain panics and return [`FfiResult::PANIC_CAUGHT`].
pub type MultiQuerySystemCallback = unsafe extern "C" fn(
    iters: *const *mut RawChunkIter,
    iter_count: usize,
    commands: *mut RawCommandQueue,
    user_data: *mut c_void,
) -> FfiResult;

/// Descriptor used by mods to register a multi-query dynamic system via `register_multi_query_system`.
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct MultiQuerySystemDescriptor {
    /// Size of this struct in bytes (`core::mem::size_of::<MultiQuerySystemDescriptor>() as u32`).
    pub struct_size: u32,
    /// Null-terminated UTF-8 system name.
    pub name: *const c_char,
    /// Pointer to an array of `QuerySpec` of length `query_spec_count`.
    pub query_specs: *const QuerySpec,
    /// Number of query clauses in `query_specs`.
    pub query_spec_count: usize,
    /// Multi-query system execution callback.
    pub callback: MultiQuerySystemCallback,
    /// Opaque guest-owned context pointer forwarded to `callback`.
    pub user_data: *mut c_void,
}

impl MultiQuerySystemDescriptor {
    /// Frozen minimum valid v0.1 descriptor size in bytes (`48` on 64-bit, `24` on 32-bit),
    /// computed from the end of `user_data` rather than `size_of::<Self>()` so appending
    /// optional trailing fields never rejects older callers.
    pub const MIN_V1_SIZE: usize =
        core::mem::offset_of!(Self, user_data) + core::mem::size_of::<*mut c_void>();

    /// Constructs a v0.1 multi-query system descriptor.
    #[inline]
    pub const fn new(
        name: *const c_char,
        query_specs: *const QuerySpec,
        query_spec_count: usize,
        callback: MultiQuerySystemCallback,
        user_data: *mut c_void,
    ) -> Self {
        Self {
            struct_size: core::mem::size_of::<Self>() as u32,
            name,
            query_specs,
            query_spec_count,
            callback,
            user_data,
        }
    }

    /// Promotes this descriptor into a [`ScheduledMultiQuerySystemDescriptor`] targeting `schedule`.
    #[inline]
    pub const fn with_schedule(
        self,
        schedule: ScheduleLabelId,
    ) -> ScheduledMultiQuerySystemDescriptor {
        ScheduledMultiQuerySystemDescriptor::from_base(self, schedule)
    }

    /// Promotes this descriptor into a [`ConditionalMultiQuerySystemDescriptor`] with a `run_if` predicate.
    #[inline]
    pub const fn with_run_if(
        self,
        run_if: RunConditionCallback,
        run_if_user_data: *mut c_void,
    ) -> ConditionalMultiQuerySystemDescriptor {
        ConditionalMultiQuerySystemDescriptor::from_scheduled(
            ScheduledMultiQuerySystemDescriptor::from_base(self, ScheduleLabelId::UPDATE),
            Some(run_if),
            run_if_user_data,
        )
    }

    /// Validates `struct_size` and mandatory pointer invariants.
    #[inline]
    pub fn validate(&self) -> FfiResult {
        if (self.struct_size as usize) < Self::MIN_V1_SIZE {
            return FfiResult::INVALID_VERSION;
        }
        if self.name.is_null() || (self.query_spec_count > 0 && self.query_specs.is_null()) {
            return FfiResult::NULL_POINTER;
        }
        FfiResult::OK
    }
}

const _: () = {
    assert!(
        MultiQuerySystemDescriptor::MIN_V1_SIZE
            <= core::mem::size_of::<MultiQuerySystemDescriptor>()
    );
    #[cfg(target_pointer_width = "64")]
    assert!(MultiQuerySystemDescriptor::MIN_V1_SIZE == 48);
    #[cfg(target_pointer_width = "32")]
    assert!(MultiQuerySystemDescriptor::MIN_V1_SIZE == 24);
};

// SAFETY: `MultiQuerySystemDescriptor` is `#[repr(C)]` with `struct_size: u32` at offset 0 and
// `validate_raw_slot` verifies the non-nullable `callback` pointer before `assume_init()`.
unsafe impl ExtensibleStruct for MultiQuerySystemDescriptor {
    const MIN_V1_SIZE: usize = Self::MIN_V1_SIZE;

    #[inline]
    unsafe fn validate_raw_slot(slot_ptr: *const Self) -> FfiResult {
        let cb_offset = core::mem::offset_of!(Self, callback);
        // SAFETY: `cb_offset + size_of::<usize>() <= size_of::<Self>()` inside `slot_ptr`.
        let cb_addr = unsafe {
            core::ptr::read_unaligned(slot_ptr.cast::<u8>().add(cb_offset).cast::<usize>())
        };
        if cb_addr == 0 {
            return FfiResult::NULL_POINTER;
        }
        FfiResult::OK
    }
}

/// Extended multi-query system descriptor adding Bevy schedule selection and intra-mod ordering.
///
/// Layout-prefixed by [`MultiQuerySystemDescriptor`] at offset `0`, so older v0.1 descriptors
/// zero-extend cleanly into `ScheduledMultiQuerySystemDescriptor` (`schedule = UPDATE`, empty
/// `before`/`after` arrays), and newer guests may pass [`Self::as_base_ptr`] directly to
/// [`RegisterMultiQuerySystemFn`].
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct ScheduledMultiQuerySystemDescriptor {
    /// Base v0.1 multi-query descriptor header (`base.struct_size` set to `size_of::<Self>() as u32`).
    pub base: MultiQuerySystemDescriptor,
    /// Target Bevy schedule (`ScheduleLabelId::UPDATE` = `0` when zero-extended).
    pub schedule: ScheduleLabelId,
    /// Bitflags word (`0` in v0.1; unknown bits rejected with [`FfiResult::UNSUPPORTED`]).
    pub flags: u32,
    /// Optional array of NUL-terminated system names this system must run after.
    pub after: *const *const c_char,
    /// Number of entries in `after`.
    pub after_count: usize,
    /// Optional array of NUL-terminated system names this system must run before.
    pub before: *const *const c_char,
    /// Number of entries in `before`.
    pub before_count: usize,
}

/// Alias for [`ScheduledMultiQuerySystemDescriptor`].
pub type MultiQuerySystemDescriptorV2 = ScheduledMultiQuerySystemDescriptor;

impl ScheduledMultiQuerySystemDescriptor {
    /// Minimum valid descriptor size (`MultiQuerySystemDescriptor::MIN_V1_SIZE` for backward compatibility).
    pub const MIN_V1_SIZE: usize = MultiQuerySystemDescriptor::MIN_V1_SIZE;
    /// Mask of currently recognized `flags` bits (`0` in v0.1).
    pub const KNOWN_FLAGS: u32 = 0;

    /// Wraps a base [`MultiQuerySystemDescriptor`] with a target [`ScheduleLabelId`].
    #[inline]
    pub const fn from_base(
        mut base: MultiQuerySystemDescriptor,
        schedule: ScheduleLabelId,
    ) -> Self {
        base.struct_size = core::mem::size_of::<Self>() as u32;
        Self {
            base,
            schedule,
            flags: 0,
            after: core::ptr::null(),
            after_count: 0,
            before: core::ptr::null(),
            before_count: 0,
        }
    }

    /// Declares NUL-terminated system names that this system must run after.
    #[inline]
    pub const fn with_after(mut self, after: *const *const c_char, after_count: usize) -> Self {
        self.after = after;
        self.after_count = after_count;
        self
    }

    /// Declares NUL-terminated system names that this system must run before.
    #[inline]
    pub const fn with_before(mut self, before: *const *const c_char, before_count: usize) -> Self {
        self.before = before;
        self.before_count = before_count;
        self
    }

    /// Promotes this scheduled descriptor into a [`ConditionalMultiQuerySystemDescriptor`] with a `run_if` predicate.
    #[inline]
    pub const fn with_run_if(
        self,
        run_if: RunConditionCallback,
        run_if_user_data: *mut c_void,
    ) -> ConditionalMultiQuerySystemDescriptor {
        ConditionalMultiQuerySystemDescriptor::from_scheduled(self, Some(run_if), run_if_user_data)
    }

    /// Returns a base-compatible pointer suitable for passing to [`RegisterMultiQuerySystemFn`].
    #[inline]
    pub const fn as_base_ptr(&self) -> *const MultiQuerySystemDescriptor {
        &self.base
    }

    /// Validates base descriptor fields, known flag bits, schedule label validity, and ordering array pointers.
    #[inline]
    pub fn validate(&self) -> FfiResult {
        let base_res = self.base.validate();
        if base_res.is_err() {
            return base_res;
        }
        if (self.flags & !Self::KNOWN_FLAGS) != 0 {
            return FfiResult::UNSUPPORTED;
        }
        if !self.schedule.is_valid() {
            return FfiResult::INVALID_VERSION;
        }
        if (self.after_count > 0 && self.after.is_null())
            || (self.before_count > 0 && self.before.is_null())
        {
            return FfiResult::NULL_POINTER;
        }
        FfiResult::OK
    }
}

// SAFETY: `ScheduledMultiQuerySystemDescriptor` starts with `base: MultiQuerySystemDescriptor`
// (`struct_size` at offset 0), all fields after `MIN_V1_SIZE` (`schedule`, `flags`, `after*`, `before*`)
// are valid when zero-filled, and `validate_raw_slot` checks non-zero `base.callback`.
unsafe impl ExtensibleStruct for ScheduledMultiQuerySystemDescriptor {
    const MIN_V1_SIZE: usize = Self::MIN_V1_SIZE;

    #[inline]
    unsafe fn validate_raw_slot(slot_ptr: *const Self) -> FfiResult {
        // SAFETY: `base` is at byte offset 0 of `Self`.
        unsafe {
            MultiQuerySystemDescriptor::validate_raw_slot(
                slot_ptr.cast::<MultiQuerySystemDescriptor>(),
            )
        }
    }
}

/// Extended v3 multi-query system descriptor adding an optional `run_if` condition callback.
///
/// Layout-prefixed by [`ScheduledMultiQuerySystemDescriptor`] at offset `0` (`104` bytes on 64-bit,
/// `56` bytes on 32-bit). Older v1 (`48`B) and v2 (`88`B) descriptors zero-extend cleanly into
/// `ConditionalMultiQuerySystemDescriptor` (`run_if = None`, `run_if_user_data = null`).
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct ConditionalMultiQuerySystemDescriptor {
    /// Scheduled v2 multi-query system descriptor prefix (`sched.base.struct_size` = `size_of::<Self>() as u32`).
    pub sched: ScheduledMultiQuerySystemDescriptor,
    /// Optional run-condition predicate (`None` / `NULL` means execute unconditionally).
    pub run_if: Option<RunConditionCallback>,
    /// Opaque guest-owned context pointer forwarded to `run_if`.
    pub run_if_user_data: *mut c_void,
}

/// Alias for [`ConditionalMultiQuerySystemDescriptor`].
pub type MultiQuerySystemDescriptorV3 = ConditionalMultiQuerySystemDescriptor;

impl ConditionalMultiQuerySystemDescriptor {
    /// Minimum valid descriptor size (`MultiQuerySystemDescriptor::MIN_V1_SIZE` for backward compatibility).
    pub const MIN_V1_SIZE: usize = MultiQuerySystemDescriptor::MIN_V1_SIZE;

    /// Wraps a [`ScheduledMultiQuerySystemDescriptor`] with an optional `run_if` predicate.
    #[inline]
    pub const fn from_scheduled(
        mut sched: ScheduledMultiQuerySystemDescriptor,
        run_if: Option<RunConditionCallback>,
        run_if_user_data: *mut c_void,
    ) -> Self {
        sched.base.struct_size = core::mem::size_of::<Self>() as u32;
        Self {
            sched,
            run_if,
            run_if_user_data,
        }
    }

    /// Returns a base-compatible pointer suitable for passing to [`RegisterMultiQuerySystemFn`].
    #[inline]
    pub const fn as_base_ptr(&self) -> *const MultiQuerySystemDescriptor {
        &self.sched.base
    }

    /// Validates base, flag, schedule, and ordering invariants.
    #[inline]
    pub fn validate(&self) -> FfiResult {
        self.sched.validate()
    }
}

// SAFETY: `ConditionalMultiQuerySystemDescriptor` starts with `sched: ScheduledMultiQuerySystemDescriptor`
// (`struct_size` at offset 0), all fields after `MIN_V1_SIZE` (`schedule`, `flags`, `after*`, `before*`,
// `run_if: Option<...>`, `run_if_user_data`) are valid when zero-filled, and `validate_raw_slot` checks non-zero `callback`.
unsafe impl ExtensibleStruct for ConditionalMultiQuerySystemDescriptor {
    const MIN_V1_SIZE: usize = Self::MIN_V1_SIZE;

    #[inline]
    unsafe fn validate_raw_slot(slot_ptr: *const Self) -> FfiResult {
        // SAFETY: `sched.base` is at byte offset 0 of `Self`.
        unsafe {
            MultiQuerySystemDescriptor::validate_raw_slot(
                slot_ptr.cast::<MultiQuerySystemDescriptor>(),
            )
        }
    }
}

/// Multi-query system registration function signature resolvable via [`crate::GetProcAddressFn`].
///
/// # Safety
/// `host_context` must point to host initialization context; `descriptor` must point to a valid descriptor.
pub type RegisterMultiQuerySystemFn = unsafe extern "C" fn(
    host_context: *mut c_void,
    descriptor: *const MultiQuerySystemDescriptor,
) -> FfiResult;

/// Explicit conditional multi-query system registration function signature resolvable via [`crate::GetProcAddressFn`].
///
/// # Safety
/// `host_context` must point to host initialization context; `descriptor` must point to a valid descriptor.
pub type RegisterConditionalMultiQuerySystemFn = unsafe extern "C" fn(
    host_context: *mut c_void,
    descriptor: *const ConditionalMultiQuerySystemDescriptor,
) -> FfiResult;

/// Descriptor used by mods to register a single-query dynamic system with the host.
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct SystemDescriptor {
    /// Size of this struct in bytes (`core::mem::size_of::<SystemDescriptor>() as u32`) for ABI forward compatibility.
    pub struct_size: u32,
    /// Null-terminated UTF-8 system name.
    pub name: *const c_char,
    /// Pointer to an array of `ComponentAccessDesc` of length `query_count`.
    pub queries: *const ComponentAccessDesc,
    /// Number of elements in `queries`.
    pub query_count: usize,
    /// System execution callback.
    pub callback: DynamicSystemCallback,
    /// Opaque guest-owned context pointer forwarded to `callback`.
    pub user_data: *mut c_void,
}

impl SystemDescriptor {
    /// Frozen minimum valid v0.1 descriptor size in bytes (`48` on 64-bit, `24` on 32-bit),
    /// computed from the end of `user_data` rather than `size_of::<Self>()` so appending
    /// optional trailing fields never rejects older callers.
    pub const MIN_V1_SIZE: usize =
        core::mem::offset_of!(Self, user_data) + core::mem::size_of::<*mut c_void>();

    /// Constructs a v0.1 `SystemDescriptor`.
    #[inline]
    pub const fn new(
        name: *const c_char,
        queries: *const ComponentAccessDesc,
        query_count: usize,
        callback: DynamicSystemCallback,
        user_data: *mut c_void,
    ) -> Self {
        Self {
            struct_size: core::mem::size_of::<Self>() as u32,
            name,
            queries,
            query_count,
            callback,
            user_data,
        }
    }

    /// Promotes this descriptor into a [`ScheduledSystemDescriptor`] targeting `schedule`.
    #[inline]
    pub const fn with_schedule(self, schedule: ScheduleLabelId) -> ScheduledSystemDescriptor {
        ScheduledSystemDescriptor::from_base(self, schedule)
    }

    /// Promotes this descriptor into a [`ConditionalSystemDescriptor`] with a `run_if` predicate.
    #[inline]
    pub const fn with_run_if(
        self,
        run_if: RunConditionCallback,
        run_if_user_data: *mut c_void,
    ) -> ConditionalSystemDescriptor {
        ConditionalSystemDescriptor::from_scheduled(
            ScheduledSystemDescriptor::from_base(self, ScheduleLabelId::UPDATE),
            Some(run_if),
            run_if_user_data,
        )
    }

    /// Validates `struct_size` and mandatory pointer invariants.
    #[inline]
    pub fn validate(&self) -> FfiResult {
        if (self.struct_size as usize) < Self::MIN_V1_SIZE {
            return FfiResult::INVALID_VERSION;
        }
        if self.name.is_null() || (self.query_count > 0 && self.queries.is_null()) {
            return FfiResult::NULL_POINTER;
        }
        FfiResult::OK
    }
}

const _: () = {
    assert!(SystemDescriptor::MIN_V1_SIZE <= core::mem::size_of::<SystemDescriptor>());
    #[cfg(target_pointer_width = "64")]
    assert!(SystemDescriptor::MIN_V1_SIZE == 48);
    #[cfg(target_pointer_width = "32")]
    assert!(SystemDescriptor::MIN_V1_SIZE == 24);
};

// SAFETY: `SystemDescriptor` is `#[repr(C)]` with `struct_size: u32` at offset 0 and
// `validate_raw_slot` verifies the non-nullable `callback` pointer before `assume_init()`.
unsafe impl ExtensibleStruct for SystemDescriptor {
    const MIN_V1_SIZE: usize = Self::MIN_V1_SIZE;

    #[inline]
    unsafe fn validate_raw_slot(slot_ptr: *const Self) -> FfiResult {
        let cb_offset = core::mem::offset_of!(Self, callback);
        // SAFETY: `cb_offset + size_of::<usize>() <= size_of::<Self>()` inside `slot_ptr`.
        let cb_addr = unsafe {
            core::ptr::read_unaligned(slot_ptr.cast::<u8>().add(cb_offset).cast::<usize>())
        };
        if cb_addr == 0 {
            return FfiResult::NULL_POINTER;
        }
        FfiResult::OK
    }
}

/// Extended system registration descriptor adding Bevy schedule selection and intra-mod ordering.
///
/// Layout-prefixed by [`SystemDescriptor`] at offset `0` (`88` bytes on 64-bit, `48` bytes on 32-bit).
/// When an older v0.1 [`SystemDescriptor`] (`48` bytes) is read via
/// `read_descriptor::<ScheduledSystemDescriptor>`, the trailing fields are zero-filled to
/// `schedule = ScheduleLabelId::UPDATE` (`0`), `flags = 0`, and empty `after`/`before` slices.
/// Newer guests can pass [`Self::as_base_ptr`] directly to `HostApi::register_system`.
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct ScheduledSystemDescriptor {
    /// Base v0.1 system descriptor header (`base.struct_size` set to `size_of::<Self>() as u32`).
    pub base: SystemDescriptor,
    /// Target Bevy schedule (`ScheduleLabelId::UPDATE` = `0` when zero-extended).
    pub schedule: ScheduleLabelId,
    /// Bitflags word (`0` in v0.1; unknown bits rejected with [`FfiResult::UNSUPPORTED`]).
    pub flags: u32,
    /// Optional array of NUL-terminated system names this system must run after.
    pub after: *const *const c_char,
    /// Number of entries in `after`.
    pub after_count: usize,
    /// Optional array of NUL-terminated system names this system must run before.
    pub before: *const *const c_char,
    /// Number of entries in `before`.
    pub before_count: usize,
}

/// Alias for [`ScheduledSystemDescriptor`].
pub type SystemDescriptorV2 = ScheduledSystemDescriptor;

impl ScheduledSystemDescriptor {
    /// Minimum valid descriptor size (`SystemDescriptor::MIN_V1_SIZE` for backward compatibility).
    pub const MIN_V1_SIZE: usize = SystemDescriptor::MIN_V1_SIZE;
    /// Mask of currently recognized `flags` bits (`0` in v0.1).
    pub const KNOWN_FLAGS: u32 = 0;

    /// Wraps a base [`SystemDescriptor`] with a target [`ScheduleLabelId`].
    #[inline]
    pub const fn from_base(mut base: SystemDescriptor, schedule: ScheduleLabelId) -> Self {
        base.struct_size = core::mem::size_of::<Self>() as u32;
        Self {
            base,
            schedule,
            flags: 0,
            after: core::ptr::null(),
            after_count: 0,
            before: core::ptr::null(),
            before_count: 0,
        }
    }

    /// Declares NUL-terminated system names that this system must run after.
    #[inline]
    pub const fn with_after(mut self, after: *const *const c_char, after_count: usize) -> Self {
        self.after = after;
        self.after_count = after_count;
        self
    }

    /// Declares NUL-terminated system names that this system must run before.
    #[inline]
    pub const fn with_before(mut self, before: *const *const c_char, before_count: usize) -> Self {
        self.before = before;
        self.before_count = before_count;
        self
    }

    /// Promotes this scheduled descriptor into a [`ConditionalSystemDescriptor`] with a `run_if` predicate.
    #[inline]
    pub const fn with_run_if(
        self,
        run_if: RunConditionCallback,
        run_if_user_data: *mut c_void,
    ) -> ConditionalSystemDescriptor {
        ConditionalSystemDescriptor::from_scheduled(self, Some(run_if), run_if_user_data)
    }

    /// Returns a base-compatible pointer suitable for passing to [`RegisterSystemFn`].
    #[inline]
    pub const fn as_base_ptr(&self) -> *const SystemDescriptor {
        &self.base
    }

    /// Validates base descriptor fields, known flag bits, schedule label validity, and ordering array pointers.
    #[inline]
    pub fn validate(&self) -> FfiResult {
        let base_res = self.base.validate();
        if base_res.is_err() {
            return base_res;
        }
        if (self.flags & !Self::KNOWN_FLAGS) != 0 {
            return FfiResult::UNSUPPORTED;
        }
        if !self.schedule.is_valid() {
            return FfiResult::INVALID_VERSION;
        }
        if (self.after_count > 0 && self.after.is_null())
            || (self.before_count > 0 && self.before.is_null())
        {
            return FfiResult::NULL_POINTER;
        }
        FfiResult::OK
    }
}

// SAFETY: `ScheduledSystemDescriptor` starts with `base: SystemDescriptor` (`struct_size` at offset 0),
// all fields after `MIN_V1_SIZE` (`schedule`, `flags`, `after*`, `before*`) are valid when zero-filled,
// and `validate_raw_slot` checks non-zero `base.callback`.
unsafe impl ExtensibleStruct for ScheduledSystemDescriptor {
    const MIN_V1_SIZE: usize = Self::MIN_V1_SIZE;

    #[inline]
    unsafe fn validate_raw_slot(slot_ptr: *const Self) -> FfiResult {
        // SAFETY: `base` is at byte offset 0 of `Self`.
        unsafe { SystemDescriptor::validate_raw_slot(slot_ptr.cast::<SystemDescriptor>()) }
    }
}

/// Extended v3 system registration descriptor adding an optional `run_if` condition callback.
///
/// Layout-prefixed by [`ScheduledSystemDescriptor`] at offset `0` (`104` bytes on 64-bit,
/// `56` bytes on 32-bit). When an older v1 [`SystemDescriptor`] (`48`B) or v2
/// [`ScheduledSystemDescriptor`] (`88`B) is read via `read_descriptor::<ConditionalSystemDescriptor>`,
/// trailing fields are zero-filled (`run_if = None`, `run_if_user_data = null`).
/// Newer guests can pass [`Self::as_base_ptr`] directly to `HostApi::register_system`,
/// [`Self::as_scheduled_ptr`] to `register_scheduled_system`, or `&self` to `register_conditional_system`.
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct ConditionalSystemDescriptor {
    /// Scheduled v2 system descriptor prefix (`sched.base.struct_size` = `size_of::<Self>() as u32`).
    pub sched: ScheduledSystemDescriptor,
    /// Optional run-condition predicate (`None` / `NULL` means execute unconditionally).
    pub run_if: Option<RunConditionCallback>,
    /// Opaque guest-owned context pointer forwarded to `run_if`.
    pub run_if_user_data: *mut c_void,
}

/// Alias for [`ConditionalSystemDescriptor`].
pub type SystemDescriptorV3 = ConditionalSystemDescriptor;

impl ConditionalSystemDescriptor {
    /// Minimum valid descriptor size (`SystemDescriptor::MIN_V1_SIZE` for backward compatibility).
    pub const MIN_V1_SIZE: usize = SystemDescriptor::MIN_V1_SIZE;

    /// Wraps a [`ScheduledSystemDescriptor`] with an optional `run_if` predicate.
    #[inline]
    pub const fn from_scheduled(
        mut sched: ScheduledSystemDescriptor,
        run_if: Option<RunConditionCallback>,
        run_if_user_data: *mut c_void,
    ) -> Self {
        sched.base.struct_size = core::mem::size_of::<Self>() as u32;
        Self {
            sched,
            run_if,
            run_if_user_data,
        }
    }

    /// Returns a base-compatible pointer suitable for passing to [`RegisterSystemFn`].
    #[inline]
    pub const fn as_base_ptr(&self) -> *const SystemDescriptor {
        &self.sched.base
    }

    /// Returns a v2-compatible pointer suitable for passing to [`RegisterScheduledSystemFn`].
    #[inline]
    pub const fn as_scheduled_ptr(&self) -> *const ScheduledSystemDescriptor {
        &self.sched
    }

    /// Validates base, flag, schedule, and ordering invariants.
    #[inline]
    pub fn validate(&self) -> FfiResult {
        self.sched.validate()
    }
}

// SAFETY: `ConditionalSystemDescriptor` starts with `sched: ScheduledSystemDescriptor`
// (`struct_size` at offset 0), all fields after `MIN_V1_SIZE` (`schedule`, `flags`, `after*`, `before*`,
// `run_if: Option<...>`, `run_if_user_data`) are valid when zero-filled, and `validate_raw_slot` checks non-zero `callback`.
unsafe impl ExtensibleStruct for ConditionalSystemDescriptor {
    const MIN_V1_SIZE: usize = Self::MIN_V1_SIZE;

    #[inline]
    unsafe fn validate_raw_slot(slot_ptr: *const Self) -> FfiResult {
        // SAFETY: `sched.base` is at byte offset 0 of `Self`.
        unsafe { SystemDescriptor::validate_raw_slot(slot_ptr.cast::<SystemDescriptor>()) }
    }
}

/// System registration function signature exposed in [`crate::HostApi`].
///
/// # Safety
/// `host_context` must point to host initialization context; `descriptor` must point to a valid descriptor.
pub type RegisterSystemFn = unsafe extern "C" fn(
    host_context: *mut c_void,
    descriptor: *const SystemDescriptor,
) -> FfiResult;

/// Explicit scheduled system registration function signature resolvable via [`crate::GetProcAddressFn`].
///
/// # Safety
/// `host_context` must point to host initialization context; `descriptor` must point to a valid descriptor.
pub type RegisterScheduledSystemFn = unsafe extern "C" fn(
    host_context: *mut c_void,
    descriptor: *const ScheduledSystemDescriptor,
) -> FfiResult;

/// Explicit conditional system registration function signature resolvable via [`crate::GetProcAddressFn`].
///
/// # Safety
/// `host_context` must point to host initialization context; `descriptor` must point to a valid descriptor.
pub type RegisterConditionalSystemFn = unsafe extern "C" fn(
    host_context: *mut c_void,
    descriptor: *const ConditionalSystemDescriptor,
) -> FfiResult;
