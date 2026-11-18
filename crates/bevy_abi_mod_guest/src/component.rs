//! Trait and identity helpers for cross-language guest POD components (`GuestComponent`).

use core::ffi::CStr;
use std::ffi::CString;

use bevy_abi_mod_guest_sys::{Host, PodComponentHooks, UuidFilter, resolve_component};
use bevy_abi_mod_sys::{AbiPod, BamComponent, ComponentUuid, FfiEntity, FfiResult};

/// Safe builder for optional dynamic component lifecycle hooks (`on_add`, `on_insert`,
/// `on_discard`, `on_remove`, `on_despawn`), `EntityCloner` support, and required components.
pub struct GuestComponentHooks<T: AbiPod> {
    pub(crate) inner: PodComponentHooks<T>,
}

impl<T: AbiPod> Default for GuestComponentHooks<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: AbiPod> GuestComponentHooks<T> {
    /// Creates an empty hook configuration with no lifecycle hooks, clone, or requirements set.
    #[inline]
    pub fn new() -> Self {
        Self {
            inner: PodComponentHooks::default(),
        }
    }

    /// Attaches an `on_add` hook invoked whenever `T` is newly added to an entity.
    #[inline]
    pub fn on_add<F>(mut self, callback: F) -> Self
    where
        F: FnMut(FfiEntity, &T) + Send + 'static,
    {
        self.inner.on_add = Some(Box::new(callback));
        self
    }

    /// Attaches an `on_insert` hook invoked whenever `T` is inserted onto an entity (add or overwrite).
    #[inline]
    pub fn on_insert<F>(mut self, callback: F) -> Self
    where
        F: FnMut(FfiEntity, &T) + Send + 'static,
    {
        self.inner.on_insert = Some(Box::new(callback));
        self
    }

    /// Attaches an `on_discard` hook invoked immediately before `T` is overwritten or removed.
    #[inline]
    pub fn on_discard<F>(mut self, callback: F) -> Self
    where
        F: FnMut(FfiEntity, &T) + Send + 'static,
    {
        self.inner.on_discard = Some(Box::new(callback));
        self
    }

    /// Attaches an `on_remove` hook invoked immediately before `T` is removed from an entity.
    #[inline]
    pub fn on_remove<F>(mut self, callback: F) -> Self
    where
        F: FnMut(FfiEntity, &T) + Send + 'static,
    {
        self.inner.on_remove = Some(Box::new(callback));
        self
    }

    /// Attaches an `on_despawn` hook invoked when an entity carrying `T` is despawned.
    #[inline]
    pub fn on_despawn<F>(mut self, callback: F) -> Self
    where
        F: FnMut(FfiEntity, &T) + Send + 'static,
    {
        self.inner.on_despawn = Some(Box::new(callback));
        self
    }

    /// Enables or disables bitwise `ComponentCloneFn` registration for Bevy's `EntityCloner`.
    #[inline]
    pub fn with_clone(mut self, enabled: bool) -> Self {
        self.inner.enable_clone = enabled;
        self
    }

    /// Appends a pre-resolved required [`BamComponent`] handle (`#[require(...)]`).
    #[inline]
    pub fn with_required_handle(mut self, component: BamComponent) -> Self {
        self.inner.required_components.push(component);
        self
    }
}

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
    /// introspection.
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

impl GuestComponent for bevy_abi_mod_sys::FfiChildOf {
    const NAME: &'static CStr = c"bevy_ecs::hierarchy::ChildOf";
    const UUID: ComponentUuid = bevy_abi_mod_sys::FfiChildOf::UUID;
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

/// Computes the expected [`UuidFilter`] for runtime column and resource validation of
/// `T: GuestComponent` inside a mod whose manifest name is `manifest_name`.
///
/// When `T::NAME` is already namespaced (`contains("::")`), `manifest_name` is empty, or `T`
/// overrides `UUID` explicitly, matches `T::UUID` exactly. When `T::NAME` is unqualified, matches
/// the auto-prefixed `"<manifest_name>::<T::NAME>"` UUID (without heap allocation for names up to
/// 256 bytes) or `T::UUID` (matching [`resolve_guest_component`]'s fallback behavior).
#[inline]
pub(crate) fn guest_uuid_filter<T: GuestComponent>(manifest_name: &str) -> UuidFilter {
    let Ok(raw_name) = T::NAME.to_str() else {
        return UuidFilter::exact(T::UUID);
    };
    if raw_name.is_empty() || raw_name.contains("::") || manifest_name.is_empty() {
        return UuidFilter::exact(T::UUID);
    }
    let default_unqualified_uuid = ComponentUuid::from_parts(
        raw_name,
        core::mem::size_of::<T>(),
        core::mem::align_of::<T>(),
        T::FIELD_SCHEMA_HASH,
    );
    if T::UUID != default_unqualified_uuid {
        return UuidFilter::exact(T::UUID);
    }
    let total_len = manifest_name.len() + 2 + raw_name.len();
    let qualified_uuid = if total_len <= 256 {
        let mut stack_buf = [0u8; 256];
        let m_len = manifest_name.len();
        stack_buf[..m_len].copy_from_slice(manifest_name.as_bytes());
        stack_buf[m_len..m_len + 2].copy_from_slice(b"::");
        stack_buf[m_len + 2..total_len].copy_from_slice(raw_name.as_bytes());
        let Ok(qualified_str) = core::str::from_utf8(&stack_buf[..total_len]) else {
            return UuidFilter::exact(T::UUID);
        };
        ComponentUuid::from_parts(
            qualified_str,
            core::mem::size_of::<T>(),
            core::mem::align_of::<T>(),
            T::FIELD_SCHEMA_HASH,
        )
    } else {
        let qualified = format!("{manifest_name}::{raw_name}");
        ComponentUuid::from_parts(
            &qualified,
            core::mem::size_of::<T>(),
            core::mem::align_of::<T>(),
            T::FIELD_SCHEMA_HASH,
        )
    };
    UuidFilter::with_fallback(qualified_uuid, T::UUID)
}
