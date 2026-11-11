//! Typed plain-old-data component and resource descriptor trait for guest mods.

use core::ffi::CStr;

use bevy_abi_mod_sys::{AbiPod, ComponentAccessDesc};

/// Trait for `#[repr(C)]` plain-old-data component and resource structs defined or mirrored by guest mods.
///
/// Layout and bit-validity safety invariants are carried by the [`AbiPod`] supertrait
/// (derivable via `#[derive(AbiPod)]` when the `derive` feature is enabled).
///
/// # Compile-Fail Safety Check
///
/// Implementing `GuestComponent` for a type that does not implement [`AbiPod`] is rejected at
/// compile time:
///
/// ```compile_fail
/// use core::ffi::CStr;
/// use bevy_abi_mod_guest::GuestComponent;
///
/// #[repr(C)]
/// #[derive(Clone, Copy)]
/// struct NotAbiPod {
///     flag: bool,
/// }
///
/// impl GuestComponent for NotAbiPod {
///     const NAME: &'static CStr = c"NotAbiPod";
/// }
/// ```
pub trait GuestComponent: AbiPod {
    /// Registered C-ABI component or resource name (for example `c"Position"`).
    const NAME: &'static CStr;

    /// Builds a read-only (`&T`) query access descriptor for this component.
    #[inline]
    fn read_desc() -> ComponentAccessDesc {
        ComponentAccessDesc::read(Self::NAME.as_ptr())
    }

    /// Builds a mutable (`&mut T`) query access descriptor for this component.
    #[inline]
    fn write_desc() -> ComponentAccessDesc {
        ComponentAccessDesc::write(Self::NAME.as_ptr())
    }

    /// Builds a presence filter (`With<T>`) query access descriptor for this component.
    #[inline]
    fn with_desc() -> ComponentAccessDesc {
        ComponentAccessDesc::filter_only(Self::NAME.as_ptr())
    }

    /// Builds an absence filter (`Without<T>`) query access descriptor for this component.
    #[inline]
    fn without_desc() -> ComponentAccessDesc {
        ComponentAccessDesc::without(Self::NAME.as_ptr())
    }

    /// Builds an optional read-only (`Option<&T>`) query access descriptor for this component.
    #[inline]
    fn optional_read_desc() -> ComponentAccessDesc {
        ComponentAccessDesc::optional_read(Self::NAME.as_ptr())
    }

    /// Builds an optional mutable (`Option<&mut T>`) query access descriptor for this component.
    #[inline]
    fn optional_write_desc() -> ComponentAccessDesc {
        ComponentAccessDesc::optional_write(Self::NAME.as_ptr())
    }

    /// Builds a row-level change filter (`Changed<T>`) query access descriptor for this component.
    #[inline]
    fn changed_desc() -> ComponentAccessDesc {
        ComponentAccessDesc::changed(Self::NAME.as_ptr())
    }

    /// Builds a row-level addition filter (`Added<T>`) query access descriptor for this component.
    #[inline]
    fn added_desc() -> ComponentAccessDesc {
        ComponentAccessDesc::added(Self::NAME.as_ptr())
    }

    /// Builds a read-only resource (`Res<T>`) access descriptor for this type.
    #[inline]
    fn resource_read_desc() -> ComponentAccessDesc {
        ComponentAccessDesc::resource_read(Self::NAME.as_ptr())
    }

    /// Builds a mutable resource (`ResMut<T>`) access descriptor for this type.
    #[inline]
    fn resource_write_desc() -> ComponentAccessDesc {
        ComponentAccessDesc::resource_write(Self::NAME.as_ptr())
    }
}
