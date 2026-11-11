//! Component access modes, query term declarations, and Bevy schedule identifiers.

use core::ffi::c_char;

/// FFI-safe component access mode declaration (`#[repr(transparent)]` `u32` newtype).
///
/// Using a single integer mode prevents contradictory states (such as simultaneous
/// `is_mutable: true` and `is_filter_only: true`) and avoids Rust `enum`/`bool` UB from C.
///
/// # Example
///
/// ```
/// use bevy_abi_mod_sys::{ComponentAccessMode, ComponentAccessDesc};
///
/// let mode = ComponentAccessMode::READ;
/// assert!(mode.is_valid());
/// assert!(!mode.is_mutable());
///
/// let desc = ComponentAccessDesc::read(c"Position".as_ptr());
/// assert_eq!(desc.mode, ComponentAccessMode::READ);
/// ```
#[repr(transparent)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, Default)]
pub struct ComponentAccessMode(pub u32);

impl ComponentAccessMode {
    /// Read-only column access (`&T`). Multiple systems can access concurrently.
    pub const READ: Self = Self(0);
    /// Exclusive write access (`&mut T`). Triggers Bevy change ticks.
    pub const WRITE: Self = Self(1);
    /// Archetype presence filter (`With<T>`). Iterates entities without reading column memory.
    pub const FILTER_ONLY: Self = Self(2);
    /// Archetype absence filter (`Without<T>`). Proves query disjointness for scheduler concurrency.
    pub const WITHOUT: Self = Self(3);
    /// Optional read-only column access (`Option<&T>`). Matches archetypes with or without `T`;
    /// `iter_column_readonly` returns non-null when present or `null` when absent.
    pub const OPTIONAL_READ: Self = Self(4);
    /// Optional exclusive write access (`Option<&mut T>`). Matches archetypes with or without `T`;
    /// `iter_column_mutable` returns non-null and marks ticks when present or `null` when absent.
    pub const OPTIONAL_WRITE: Self = Self(5);
    /// Row-level change-tick filter (`Changed<T>`) with read-only column access.
    /// Yields contiguous runs of entities whose `T` change tick is newer than the system's `last_run`.
    pub const CHANGED: Self = Self(6);
    /// Row-level added-tick filter (`Added<T>`) with read-only column access.
    /// Yields contiguous runs of entities whose `T` added tick is newer than the system's `last_run`.
    pub const ADDED: Self = Self(7);
    /// Shared read-only access to a Bevy singleton `Resource` (`Res<R>`).
    pub const RESOURCE_READ: Self = Self(8);
    /// Exclusive mutable access to a Bevy singleton `Resource` (`ResMut<R>`).
    pub const RESOURCE_WRITE: Self = Self(9);

    /// Returns `true` if this mode is one of the recognized variants (`0..=9`).
    #[inline]
    pub const fn is_valid(self) -> bool {
        self.0 <= Self::RESOURCE_WRITE.0
    }

    /// Returns `true` if this access mode requires exclusive mutable access (`WRITE`, `OPTIONAL_WRITE`, or `RESOURCE_WRITE`).
    #[inline]
    pub const fn is_mutable(self) -> bool {
        self.0 == Self::WRITE.0
            || self.0 == Self::OPTIONAL_WRITE.0
            || self.0 == Self::RESOURCE_WRITE.0
    }

    /// Returns `true` if this access mode is an archetype presence filter (`With<T>`).
    #[inline]
    pub const fn is_filter_only(self) -> bool {
        self.0 == Self::FILTER_ONLY.0
    }

    /// Returns `true` if this access mode is an archetype absence filter (`Without<T>`).
    #[inline]
    pub const fn is_without(self) -> bool {
        self.0 == Self::WITHOUT.0
    }

    /// Returns `true` if this access mode is optional (`Option<&T>` or `Option<&mut T>`).
    #[inline]
    pub const fn is_optional(self) -> bool {
        self.0 == Self::OPTIONAL_READ.0 || self.0 == Self::OPTIONAL_WRITE.0
    }

    /// Returns `true` if this access mode filters rows by component ticks (`Changed<T>` or `Added<T>`).
    #[inline]
    pub const fn is_tick_filter(self) -> bool {
        self.0 == Self::CHANGED.0 || self.0 == Self::ADDED.0
    }

    /// Returns `true` if this access mode targets a singleton Bevy `Resource` (`Res<R>` or `ResMut<R>`).
    #[inline]
    pub const fn is_resource(self) -> bool {
        self.0 == Self::RESOURCE_READ.0 || self.0 == Self::RESOURCE_WRITE.0
    }
}

/// Component access descriptor passed in `SystemDescriptor::queries`.
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct ComponentAccessDesc {
    /// Null-terminated UTF-8 C string identifying the registered component or resource name.
    pub name: *const c_char,
    /// Access mode (`READ`, `WRITE`, `FILTER_ONLY`, `WITHOUT`, `OPTIONAL_*`, `CHANGED`, `ADDED`, `RESOURCE_*`).
    pub mode: ComponentAccessMode,
}

impl ComponentAccessDesc {
    /// Creates a read-only access descriptor for the specified component name.
    #[inline]
    pub const fn read(name: *const c_char) -> Self {
        Self {
            name,
            mode: ComponentAccessMode::READ,
        }
    }

    /// Creates an exclusive write access descriptor for the specified component name.
    #[inline]
    pub const fn write(name: *const c_char) -> Self {
        Self {
            name,
            mode: ComponentAccessMode::WRITE,
        }
    }

    /// Creates an archetype presence filter (`With<T>`) descriptor.
    #[inline]
    pub const fn filter_only(name: *const c_char) -> Self {
        Self {
            name,
            mode: ComponentAccessMode::FILTER_ONLY,
        }
    }

    /// Creates an archetype absence filter (`Without<T>`) descriptor.
    #[inline]
    pub const fn without(name: *const c_char) -> Self {
        Self {
            name,
            mode: ComponentAccessMode::WITHOUT,
        }
    }

    /// Creates an optional read-only (`Option<&T>`) access descriptor.
    #[inline]
    pub const fn optional_read(name: *const c_char) -> Self {
        Self {
            name,
            mode: ComponentAccessMode::OPTIONAL_READ,
        }
    }

    /// Creates an optional mutable (`Option<&mut T>`) access descriptor.
    #[inline]
    pub const fn optional_write(name: *const c_char) -> Self {
        Self {
            name,
            mode: ComponentAccessMode::OPTIONAL_WRITE,
        }
    }

    /// Creates a per-row change-tick filter (`Changed<T>`) descriptor.
    #[inline]
    pub const fn changed(name: *const c_char) -> Self {
        Self {
            name,
            mode: ComponentAccessMode::CHANGED,
        }
    }

    /// Creates a per-row added-tick filter (`Added<T>`) descriptor.
    #[inline]
    pub const fn added(name: *const c_char) -> Self {
        Self {
            name,
            mode: ComponentAccessMode::ADDED,
        }
    }

    /// Creates a shared resource read (`Res<R>`) descriptor.
    #[inline]
    pub const fn resource_read(name: *const c_char) -> Self {
        Self {
            name,
            mode: ComponentAccessMode::RESOURCE_READ,
        }
    }

    /// Creates an exclusive resource write (`ResMut<R>`) descriptor.
    #[inline]
    pub const fn resource_write(name: *const c_char) -> Self {
        Self {
            name,
            mode: ComponentAccessMode::RESOURCE_WRITE,
        }
    }
}

/// FFI-safe Bevy schedule identifier (`#[repr(transparent)]` `u32` newtype).
///
/// Defaults to [`ScheduleLabelId::UPDATE`] (`0`), so older `SystemDescriptor` instances zero-extended
/// via [`crate::read_extensible_struct`] automatically run in Bevy's `Update` schedule.
#[repr(transparent)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, Default)]
pub struct ScheduleLabelId(pub u32);

impl ScheduleLabelId {
    /// Main per-frame `Update` schedule (`0`, default).
    pub const UPDATE: Self = Self(0);
    /// One-shot `Startup` schedule (`1`).
    pub const STARTUP: Self = Self(1);
    /// Per-frame `PreUpdate` schedule (`2`).
    pub const PRE_UPDATE: Self = Self(2);
    /// Per-frame `PostUpdate` schedule (`3`).
    pub const POST_UPDATE: Self = Self(3);
    /// Fixed-timestep `FixedUpdate` schedule (`4`).
    pub const FIXED_UPDATE: Self = Self(4);
    /// End-of-frame `Last` schedule (`5`).
    pub const LAST: Self = Self(5);

    /// Returns `true` if this schedule label ID is one of the recognized built-in variants (`0..=5`).
    #[inline]
    pub const fn is_valid(self) -> bool {
        self.0 <= Self::LAST.0
    }
}
