//! Trait and identity helpers for cross-language guest POD components (`GuestComponent`).

use core::ffi::CStr;
use std::ffi::CString;

use bevy_abi_mod_guest_sys::{Host, resolve_component};
use bevy_abi_mod_sys::{AbiPod, BamComponent, ComponentUuid, FfiResult};

/// Computes a deterministic 128-bit [`ComponentUuid`] for `T: AbiPod` from its canonical
/// namespaced name (`"<namespace>::<Name>"`) and optional `field_schema_hash`.
#[inline]
pub const fn guest_component_uuid<T: AbiPod>(
    canonical_name: &str,
    field_schema_hash: u64,
) -> ComponentUuid {
    ComponentUuid::from_parts(
        canonical_name,
        core::mem::size_of::<T>(),
        core::mem::align_of::<T>(),
        field_schema_hash,
    )
}

/// Cross-language Plain Old Data component or resource usable with typed [`GuestApp`](crate::GuestApp),
/// [`QueryBuilder`](crate::QueryBuilder), [`Chunk`](crate::Chunk), and [`SystemContext`](crate::SystemContext) APIs.
///
/// Requires [`AbiPod`] (`#[repr(C)]` or `#[repr(transparent)]`, `Copy + Send + Sync + 'static`,
/// fixed-width C-compatible fields, and zero heap ownership or invalid bit patterns).
///
/// # Compile-Time Soundness Rejections
///
/// Heap-owning types (`String`, `Vec<T>`, `Box<T>`) are rejected at compile time:
///
/// ```compile_fail
/// use bevy_abi_mod_guest::prelude::*;
///
/// #[repr(C)]
/// struct BadHeapComponent {
///     label: String,
/// }
///
/// impl GuestComponent for BadHeapComponent {
///     const NAME: &'static core::ffi::CStr = c"bad_mod::BadHeapComponent";
/// }
/// ```
///
/// Types with invalid bit patterns (`bool`, `char`) or platform-dependent widths (`usize`, `u128`)
/// are also rejected at compile time:
///
/// ```compile_fail
/// use bevy_abi_mod_sys::AbiPod;
/// use bevy_abi_mod_guest::GuestComponent;
///
/// fn assert_guest<T: GuestComponent>() {}
/// fn check<T: AbiPod>() {}
///
/// check::<bool>();
/// ```
pub trait GuestComponent: AbiPod {
    /// Component or resource name as a NUL-terminated UTF-8 C string literal.
    ///
    /// Prefer canonical namespaced identifiers (`c"my_game::Position"`). When registered via
    /// [`GuestApp::register_component`](crate::GuestApp::register_component) with an unqualified
    /// name (`c"Health"`), the guest SDK automatically prefixes the mod's manifest name
    /// (`"<manifest.name>::Health"`).
    const NAME: &'static CStr;

    /// Optional deterministic field-schema hash mixed into the default [`Self::UUID`] calculation.
    const FIELD_SCHEMA_HASH: u64 = 0;

    /// Optional `BAM_COMPONENT_FLAG_*` registration flags (`0` = dense `Table`, mutable).
    const FLAGS: u32 = 0;

    /// Deterministic 128-bit identity ([`ComponentUuid`]) verified on handle resolution and column
    /// introspection (`C-13`).
    const UUID: ComponentUuid = match Self::NAME.to_str() {
        Ok(name_str) => ComponentUuid::from_parts(
            name_str,
            core::mem::size_of::<Self>(),
            core::mem::align_of::<Self>(),
            Self::FIELD_SCHEMA_HASH,
        ),
        Err(_) => panic!("GuestComponent::NAME must be valid UTF-8"),
    };
}

/// Computes the qualified registration `(CString, ComponentUuid)` for `T: GuestComponent` inside a
/// mod whose manifest name is `manifest_name`.
///
/// If `T::NAME` already contains `"::"` (or `manifest_name` is empty), returns `(T::NAME, T::UUID)`.
/// Otherwise prefixes `"<manifest_name>::<T::NAME>"` and recomputes the namespaced UUID unless `T`
/// supplied an explicit custom `UUID`.
pub(crate) fn qualify_component_identity<T: GuestComponent>(
    manifest_name: &str,
) -> Result<(CString, ComponentUuid), FfiResult> {
    if T::NAME.to_bytes().is_empty() {
        return Err(FfiResult::INVALID_ARGUMENT);
    }
    let Ok(raw_name) = T::NAME.to_str() else {
        return Err(FfiResult::INVALID_UTF8);
    };
    if raw_name.contains("::") || manifest_name.is_empty() {
        return Ok((CString::from(T::NAME), T::UUID));
    }
    let qualified = format!("{manifest_name}::{raw_name}");
    let Ok(qualified_cstr) = CString::new(qualified.as_bytes()) else {
        return Err(FfiResult::INVALID_ARGUMENT);
    };
    let default_unqualified_uuid = ComponentUuid::from_parts(
        raw_name,
        core::mem::size_of::<T>(),
        core::mem::align_of::<T>(),
        T::FIELD_SCHEMA_HASH,
    );
    let uuid = if T::UUID == default_unqualified_uuid {
        ComponentUuid::from_parts(
            &qualified,
            core::mem::size_of::<T>(),
            core::mem::align_of::<T>(),
            T::FIELD_SCHEMA_HASH,
        )
    } else {
        T::UUID
    };
    Ok((qualified_cstr, uuid))
}

/// Resolves `T: GuestComponent` on `host`, trying the manifest-qualified identity first when
/// `T::NAME` is unqualified and falling back to `(T::NAME, T::UUID)` if not found under the mod's
/// own namespace.
pub(crate) fn resolve_guest_component<T: GuestComponent>(
    host: &Host,
    manifest_name: &str,
) -> Result<BamComponent, FfiResult> {
    let (qualified_name, qualified_uuid) = qualify_component_identity::<T>(manifest_name)?;
    match resolve_component(host, Some(qualified_name.as_c_str()), Some(qualified_uuid)) {
        Ok(comp) => Ok(comp),
        Err(FfiResult::NOT_FOUND) if qualified_name.as_c_str() != T::NAME => {
            resolve_component(host, Some(T::NAME), Some(T::UUID))
        }
        Err(err) => Err(err),
    }
}
