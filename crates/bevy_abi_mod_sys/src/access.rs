//! Query access modes, schedule identifiers, architectural limits, and query term descriptors.

use crate::primitives::{BamComponent, FfiResult};

/// Maximum number of component, filter, or resource terms permitted in a single [`crate::BamQueryDesc`]
/// (`32`, C `BAM_MAX_TERMS_PER_QUERY`).
pub const MAX_TERMS_PER_QUERY: usize = 32;

/// Maximum number of [`crate::BamQueryDesc`] clauses permitted in a single [`crate::BamSystemDesc`]
/// (`8`, C `BAM_MAX_QUERIES_PER_SYSTEM`).
pub const MAX_QUERIES_PER_SYSTEM: usize = 8;

/// FFI-safe query term access mode (`typedef uint32_t BamAccessMode;`).
///
/// Represented as a `#[repr(transparent)]` `u32` newtype so unrecognized values from foreign code
/// can be rejected cleanly without Rust `enum` discriminant undefined behavior.
///
/// # Example
///
/// ```
/// use bevy_abi_mod_sys::{AccessMode, BamComponent, BamTermDesc};
///
/// let comp = BamComponent::from_raw(1);
/// let term = BamTermDesc::write(comp);
/// assert!(term.mode.is_valid());
/// assert!(term.mode.is_mutable());
/// ```
#[repr(transparent)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, Default)]
pub struct AccessMode(pub u32);

impl AccessMode {
    /// Shared read-only entity column access (`&T`, C `BAM_ACCESS_READ`).
    pub const READ: Self = Self(0);
    /// Exclusive mutable entity column access (`&mut T`, C `BAM_ACCESS_WRITE`).
    pub const WRITE: Self = Self(1);
    /// Archetype presence filter (`With<T>`, C `BAM_ACCESS_FILTER_ONLY`).
    pub const FILTER_ONLY: Self = Self(2);
    /// Archetype absence filter (`Without<T>`, C `BAM_ACCESS_WITHOUT`).
    pub const WITHOUT: Self = Self(3);
    /// Optional shared read-only column access (`Option<&T>`, C `BAM_ACCESS_OPTIONAL_READ`).
    pub const OPTIONAL_READ: Self = Self(4);
    /// Optional exclusive mutable column access (`Option<&mut T>`, C `BAM_ACCESS_OPTIONAL_WRITE`).
    pub const OPTIONAL_WRITE: Self = Self(5);
    /// Row-level change-tick filter with read-only column access (`Changed<T> + &T`, C `BAM_ACCESS_CHANGED`).
    pub const CHANGED: Self = Self(6);
    /// Row-level added-tick filter with read-only column access (`Added<T> + &T`, C `BAM_ACCESS_ADDED`).
    pub const ADDED: Self = Self(7);
    /// Shared read-only singleton resource access (`Res<R>`, C `BAM_ACCESS_RESOURCE_READ`).
    pub const RESOURCE_READ: Self = Self(8);
    /// Exclusive mutable singleton resource access (`ResMut<R>`, C `BAM_ACCESS_RESOURCE_WRITE`).
    pub const RESOURCE_WRITE: Self = Self(9);
    /// Buffered Bevy `Messages<M>` reader access (`MessageReader<M>`, C `BAM_ACCESS_MESSAGE_READ`).
    pub const MESSAGE_READ: Self = Self(10);
    /// Buffered Bevy `Messages<M>` writer/mutator access (`MessageWriter<M>`, C `BAM_ACCESS_MESSAGE_WRITE`).
    pub const MESSAGE_WRITE: Self = Self(11);

    /// Returns `true` if this mode is one of the recognized variants (`0..=11`).
    #[inline]
    pub const fn is_valid(self) -> bool {
        self.0 <= Self::MESSAGE_WRITE.0
    }

    /// Returns `true` if this mode requires exclusive mutable access (`WRITE`, `OPTIONAL_WRITE`,
    /// `RESOURCE_WRITE`, or `MESSAGE_WRITE`).
    #[inline]
    pub const fn is_mutable(self) -> bool {
        self.0 == Self::WRITE.0
            || self.0 == Self::OPTIONAL_WRITE.0
            || self.0 == Self::RESOURCE_WRITE.0
            || self.0 == Self::MESSAGE_WRITE.0
    }

    /// Returns `true` if this mode is an archetype presence filter (`With<T>`).
    #[inline]
    pub const fn is_filter_only(self) -> bool {
        self.0 == Self::FILTER_ONLY.0
    }

    /// Returns `true` if this mode is an archetype absence filter (`Without<T>`).
    #[inline]
    pub const fn is_without(self) -> bool {
        self.0 == Self::WITHOUT.0
    }

    /// Returns `true` if this mode is optional (`Option<&T>` or `Option<&mut T>`).
    #[inline]
    pub const fn is_optional(self) -> bool {
        self.0 == Self::OPTIONAL_READ.0 || self.0 == Self::OPTIONAL_WRITE.0
    }

    /// Returns `true` if this mode filters rows by component ticks (`Changed<T>` or `Added<T>`).
    #[inline]
    pub const fn is_tick_filter(self) -> bool {
        self.0 == Self::CHANGED.0 || self.0 == Self::ADDED.0
    }

    /// Returns `true` if this mode targets a singleton Bevy `Resource` (`Res<R>` or `ResMut<R>`).
    #[inline]
    pub const fn is_resource(self) -> bool {
        self.0 == Self::RESOURCE_READ.0 || self.0 == Self::RESOURCE_WRITE.0
    }

    /// Returns `true` if this mode targets a buffered Bevy `Messages<M>` stream (`MESSAGE_READ` or `MESSAGE_WRITE`).
    #[inline]
    pub const fn is_message(self) -> bool {
        self.0 == Self::MESSAGE_READ.0 || self.0 == Self::MESSAGE_WRITE.0
    }

    /// Returns `true` if this mode reads or writes per-entity component data (`READ`, `WRITE`,
    /// `OPTIONAL_READ`, `OPTIONAL_WRITE`, `CHANGED`, or `ADDED`).
    #[inline]
    pub const fn has_entity_column_data(self) -> bool {
        matches!(self.0, 0 | 1 | 4 | 5 | 6 | 7)
    }
}

/// FFI-safe Bevy schedule identifier (`typedef uint32_t BamScheduleId;`).
///
/// Built-in Bevy schedules occupy `0..=5`, values `6..1024` are reserved for future built-in
/// Bevy schedules, and game-defined custom schedules use IDs `>= ScheduleId::CUSTOM_BASE` (`1024`).
#[repr(transparent)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, Default)]
pub struct ScheduleId(pub u32);

impl ScheduleId {
    /// Main per-frame `Update` schedule (`0`, default on zero-extension; C `BAM_SCHEDULE_UPDATE`).
    pub const UPDATE: Self = Self(0);
    /// One-shot `Startup` schedule (`1`, C `BAM_SCHEDULE_STARTUP`).
    pub const STARTUP: Self = Self(1);
    /// Per-frame `PreUpdate` schedule (`2`, C `BAM_SCHEDULE_PRE_UPDATE`).
    pub const PRE_UPDATE: Self = Self(2);
    /// Per-frame `PostUpdate` schedule (`3`, C `BAM_SCHEDULE_POST_UPDATE`).
    pub const POST_UPDATE: Self = Self(3);
    /// Fixed-timestep `FixedUpdate` schedule (`4`, C `BAM_SCHEDULE_FIXED_UPDATE`).
    pub const FIXED_UPDATE: Self = Self(4);
    /// End-of-frame `Last` schedule (`5`, C `BAM_SCHEDULE_LAST`).
    pub const LAST: Self = Self(5);
    /// First schedule ID available for game-registered custom schedules (`1024`, C `BAM_SCHEDULE_CUSTOM_BASE`).
    pub const CUSTOM_BASE: u32 = 1024;

    /// Constructs a custom game schedule identifier (`id` must be `>= ScheduleId::CUSTOM_BASE`).
    #[inline]
    pub const fn custom(id: u32) -> Self {
        Self(id)
    }

    /// Returns `true` if this ID names one of the six built-in Bevy schedules (`0..=5`).
    #[inline]
    pub const fn is_builtin(self) -> bool {
        self.0 <= Self::LAST.0
    }

    /// Returns `true` if this ID falls in the game-defined custom schedule range (`>= 1024`).
    #[inline]
    pub const fn is_custom(self) -> bool {
        self.0 >= Self::CUSTOM_BASE
    }

    /// Returns `true` if this schedule ID is either a built-in schedule (`0..=5`) or a custom
    /// game schedule (`>= 1024`), rejecting the reserved `6..1024` range.
    #[inline]
    pub const fn is_valid(self) -> bool {
        self.is_builtin() || self.is_custom()
    }
}

/// Mask of recognized [`BamTermDesc::flags`] bits in ABI `0.2` (`0`).
pub const BAM_TERM_KNOWN_FLAGS: u32 = 0;

/// Handle-based query or resource term declaration inside [`crate::BamQueryDesc`].
///
/// Layout: `12` bytes, `4`-byte alignment, zero implicit padding on both 64-bit and 32-bit targets.
#[repr(C)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct BamTermDesc {
    /// Pre-resolved non-zero [`BamComponent`] handle (`0` is rejected with [`FfiResult::INVALID_COMPONENT`]).
    pub component: BamComponent,
    /// Access mode (`READ`..=`RESOURCE_WRITE`).
    pub mode: AccessMode,
    /// Term bitflags (`0` in ABI `0.2`; unknown bits are rejected with [`FfiResult::UNSUPPORTED`]).
    pub flags: u32,
}

impl BamTermDesc {
    /// Constructs a term descriptor with `flags = 0`.
    #[inline]
    pub const fn new(component: BamComponent, mode: AccessMode) -> Self {
        Self {
            component,
            mode,
            flags: 0,
        }
    }

    /// Attaches explicit term flags.
    #[inline]
    pub const fn with_flags(mut self, flags: u32) -> Self {
        self.flags = flags;
        self
    }

    /// Creates a shared read-only (`&T`) term descriptor.
    #[inline]
    pub const fn read(component: BamComponent) -> Self {
        Self::new(component, AccessMode::READ)
    }

    /// Creates an exclusive mutable (`&mut T`) term descriptor.
    #[inline]
    pub const fn write(component: BamComponent) -> Self {
        Self::new(component, AccessMode::WRITE)
    }

    /// Creates an archetype presence filter (`With<T>`) term descriptor.
    #[inline]
    pub const fn filter_only(component: BamComponent) -> Self {
        Self::new(component, AccessMode::FILTER_ONLY)
    }

    /// Creates an archetype absence filter (`Without<T>`) term descriptor.
    #[inline]
    pub const fn without(component: BamComponent) -> Self {
        Self::new(component, AccessMode::WITHOUT)
    }

    /// Creates an optional shared read (`Option<&T>`) term descriptor.
    #[inline]
    pub const fn optional_read(component: BamComponent) -> Self {
        Self::new(component, AccessMode::OPTIONAL_READ)
    }

    /// Creates an optional exclusive write (`Option<&mut T>`) term descriptor.
    #[inline]
    pub const fn optional_write(component: BamComponent) -> Self {
        Self::new(component, AccessMode::OPTIONAL_WRITE)
    }

    /// Creates a row-level change-tick filter (`Changed<T> + &T`) term descriptor.
    #[inline]
    pub const fn changed(component: BamComponent) -> Self {
        Self::new(component, AccessMode::CHANGED)
    }

    /// Creates a row-level added-tick filter (`Added<T> + &T`) term descriptor.
    #[inline]
    pub const fn added(component: BamComponent) -> Self {
        Self::new(component, AccessMode::ADDED)
    }

    /// Creates a shared singleton resource read (`Res<R>`) term descriptor.
    #[inline]
    pub const fn resource_read(component: BamComponent) -> Self {
        Self::new(component, AccessMode::RESOURCE_READ)
    }

    /// Creates an exclusive singleton resource write (`ResMut<R>`) term descriptor.
    #[inline]
    pub const fn resource_write(component: BamComponent) -> Self {
        Self::new(component, AccessMode::RESOURCE_WRITE)
    }

    /// Creates a shared buffered message reader (`MessageReader<M>`) term descriptor.
    #[inline]
    pub const fn message_read(component: BamComponent) -> Self {
        Self::new(component, AccessMode::MESSAGE_READ)
    }

    /// Creates an exclusive buffered message writer (`MessageWriter<M>`) term descriptor.
    #[inline]
    pub const fn message_write(component: BamComponent) -> Self {
        Self::new(component, AccessMode::MESSAGE_WRITE)
    }

    /// Validates `flags`, `component != INVALID`, and `mode` discriminant range.
    #[inline]
    pub const fn validate(&self) -> FfiResult {
        if (self.flags & !BAM_TERM_KNOWN_FLAGS) != 0 {
            return FfiResult::UNSUPPORTED;
        }
        if !self.component.is_valid() {
            return FfiResult::INVALID_COMPONENT;
        }
        if !self.mode.is_valid() {
            return FfiResult::INVALID_ARGUMENT;
        }
        FfiResult::OK
    }
}

const _: () = {
    assert!(core::mem::size_of::<AccessMode>() == 4);
    assert!(core::mem::align_of::<AccessMode>() == 4);
    assert!(core::mem::size_of::<ScheduleId>() == 4);
    assert!(core::mem::align_of::<ScheduleId>() == 4);
    assert!(core::mem::size_of::<BamTermDesc>() == 12);
    assert!(core::mem::align_of::<BamTermDesc>() == 4);
    assert!(core::mem::offset_of!(BamTermDesc, component) == 0);
    assert!(core::mem::offset_of!(BamTermDesc, mode) == 4);
    assert!(core::mem::offset_of!(BamTermDesc, flags) == 8);
};
