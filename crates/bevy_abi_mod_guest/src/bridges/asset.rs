//! Guest-side `#![forbid(unsafe_code)]`, zero-Bevy wrappers for isolated `mod://` asset
//! loading (`GuestAssetServer`, `GuestAssetHandle`, `TypedAssetHandle<A>`).

use bevy_abi_mod_guest_sys::Host;
use bevy_abi_mod_sys::{BamComponent, FfiAssetHandle, FfiAssetLoadState, FfiResult};
use core::marker::PhantomData;

use crate::app::{GuestApp, SystemContext};

/// Untyped or `BamComponent`-tagged guest handle to a host-retained Bevy asset.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct GuestAssetHandle {
    raw: FfiAssetHandle,
}

impl GuestAssetHandle {
    /// Canonical disengaged (`None`) handle.
    pub const NONE: Self = Self {
        raw: FfiAssetHandle::NONE,
    };

    /// Wraps a validated [`FfiAssetHandle`] returned by `BamHostApi::asset_load`.
    #[inline]
    pub const fn from_raw(raw: FfiAssetHandle) -> Self {
        Self { raw }
    }

    /// Returns the underlying wire [`FfiAssetHandle`].
    #[inline]
    pub const fn raw(&self) -> FfiAssetHandle {
        self.raw
    }

    /// Returns the host [`BamComponent`] asset type handle recorded in this handle.
    #[inline]
    pub const fn asset_type(&self) -> BamComponent {
        self.raw.asset_type
    }

    /// Returns `true` if this handle is disengaged (`KIND_NONE`).
    #[inline]
    pub const fn is_none(&self) -> bool {
        self.raw.is_none()
    }

    /// Returns `true` if this handle refers to a host asset.
    #[inline]
    pub const fn is_some(&self) -> bool {
        self.raw.is_some()
    }

    /// Queries the current [`FfiAssetLoadState`] from `host`.
    #[inline]
    pub fn load_state(&self, host: &Host) -> Result<FfiAssetLoadState, FfiResult> {
        host.asset_get_load_state(&self.raw)
    }

    /// Returns `true` if the asset has finished loading (`FfiAssetLoadState::LOADED`).
    #[inline]
    pub fn is_loaded(&self, host: &Host) -> Result<bool, FfiResult> {
        Ok(self.load_state(host)?.is_loaded())
    }

    /// Releases the strong handle pin on `host` early (otherwise retained until mod unload).
    #[inline]
    pub fn release(&self, host: &Host) -> FfiResult {
        host.asset_release(&self.raw)
    }
}

impl From<FfiAssetHandle> for GuestAssetHandle {
    #[inline]
    fn from(raw: FfiAssetHandle) -> Self {
        Self::from_raw(raw)
    }
}

impl From<GuestAssetHandle> for FfiAssetHandle {
    #[inline]
    fn from(handle: GuestAssetHandle) -> Self {
        handle.raw
    }
}

/// Phantom-typed guest handle to a host-retained Bevy asset of logical marker type `A`.
#[derive(Debug, PartialEq, Eq, Hash)]
pub struct TypedAssetHandle<A> {
    raw: FfiAssetHandle,
    _marker: PhantomData<fn() -> A>,
}

impl<A> Copy for TypedAssetHandle<A> {}

impl<A> Clone for TypedAssetHandle<A> {
    #[inline]
    fn clone(&self) -> Self {
        *self
    }
}

impl<A> Default for TypedAssetHandle<A> {
    #[inline]
    fn default() -> Self {
        Self::NONE
    }
}

impl<A> TypedAssetHandle<A> {
    /// Canonical disengaged (`None`) typed handle.
    pub const NONE: Self = Self {
        raw: FfiAssetHandle::NONE,
        _marker: PhantomData,
    };

    /// Wraps a raw [`FfiAssetHandle`].
    #[inline]
    pub const fn from_raw(raw: FfiAssetHandle) -> Self {
        Self {
            raw,
            _marker: PhantomData,
        }
    }

    /// Returns the underlying wire [`FfiAssetHandle`].
    #[inline]
    pub const fn raw(&self) -> FfiAssetHandle {
        self.raw
    }

    /// Erases the marker type `A` into a [`GuestAssetHandle`].
    #[inline]
    pub const fn untyped(&self) -> GuestAssetHandle {
        GuestAssetHandle::from_raw(self.raw)
    }

    /// Returns `true` if this handle is disengaged (`KIND_NONE`).
    #[inline]
    pub const fn is_none(&self) -> bool {
        self.raw.is_none()
    }

    /// Returns `true` if this handle refers to a host asset.
    #[inline]
    pub const fn is_some(&self) -> bool {
        self.raw.is_some()
    }

    /// Queries the current [`FfiAssetLoadState`] from `host`.
    #[inline]
    pub fn load_state(&self, host: &Host) -> Result<FfiAssetLoadState, FfiResult> {
        host.asset_get_load_state(&self.raw)
    }

    /// Returns `true` if the asset has finished loading (`FfiAssetLoadState::LOADED`).
    #[inline]
    pub fn is_loaded(&self, host: &Host) -> Result<bool, FfiResult> {
        Ok(self.load_state(host)?.is_loaded())
    }

    /// Releases the strong handle pin on `host`.
    #[inline]
    pub fn release(&self, host: &Host) -> FfiResult {
        host.asset_release(&self.raw)
    }
}

/// Safe guest wrapper around `BamHostApi::asset_load`, `asset_get_load_state`, and
/// `asset_release`.
#[derive(Clone, Copy)]
pub struct GuestAssetServer<'a> {
    host: &'a Host,
}

impl<'a> GuestAssetServer<'a> {
    /// Binds a [`GuestAssetServer`] to `host`.
    #[inline]
    pub const fn new(host: &'a Host) -> Self {
        Self { host }
    }

    /// Returns the underlying [`Host`] reference.
    #[inline]
    pub const fn host(&self) -> &'a Host {
        self.host
    }

    /// Initiates an async load for `path` with pre-resolved `asset_type` handle.
    #[inline]
    pub fn load_handle(
        &self,
        asset_type: BamComponent,
        path: &str,
    ) -> Result<GuestAssetHandle, FfiResult> {
        self.host
            .asset_load(asset_type, path)
            .map(GuestAssetHandle::from_raw)
    }

    /// Initiates a typed async load for `path`, returning a [`TypedAssetHandle<A>`].
    #[inline]
    pub fn load_typed<A>(
        &self,
        asset_type: BamComponent,
        path: &str,
    ) -> Result<TypedAssetHandle<A>, FfiResult> {
        self.host
            .asset_load(asset_type, path)
            .map(TypedAssetHandle::from_raw)
    }

    /// Initiates an untyped async load (`BamComponent::INVALID`) for `path`.
    #[inline]
    pub fn load_untyped(&self, path: &str) -> Result<GuestAssetHandle, FfiResult> {
        self.host
            .asset_load(BamComponent::INVALID, path)
            .map(GuestAssetHandle::from_raw)
    }

    /// Queries the current [`FfiAssetLoadState`] for `handle`.
    #[inline]
    pub fn load_state(&self, handle: &FfiAssetHandle) -> Result<FfiAssetLoadState, FfiResult> {
        self.host.asset_get_load_state(handle)
    }

    /// Returns `true` if `handle` is currently [`FfiAssetLoadState::LOADED`].
    #[inline]
    pub fn is_loaded(&self, handle: &FfiAssetHandle) -> Result<bool, FfiResult> {
        Ok(self.load_state(handle)?.is_loaded())
    }

    /// Releases the host-retained strong handle for `handle`.
    #[inline]
    pub fn release(&self, handle: &FfiAssetHandle) -> FfiResult {
        self.host.asset_release(handle)
    }
}

/// Convenience extension trait providing `.assets()` on [`Host`], [`GuestApp`], and [`SystemContext`].
pub trait GuestAssetHostExt {
    /// Returns a [`GuestAssetServer`] bound to this context's [`Host`].
    fn assets(&self) -> GuestAssetServer<'_>;
}

impl GuestAssetHostExt for Host {
    #[inline]
    fn assets(&self) -> GuestAssetServer<'_> {
        GuestAssetServer::new(self)
    }
}

impl GuestAssetHostExt for GuestApp<'_> {
    #[inline]
    fn assets(&self) -> GuestAssetServer<'_> {
        GuestAssetServer::new(self.host())
    }
}

impl GuestAssetHostExt for SystemContext<'_> {
    #[inline]
    fn assets(&self) -> GuestAssetServer<'_> {
        GuestAssetServer::new(self.host())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_and_untyped_asset_handle_wrappers_preserve_raw_fields() {
        struct FakeTexture;
        let raw = FfiAssetHandle::from_index(BamComponent::from_raw(5), 11, 2);
        let typed = TypedAssetHandle::<FakeTexture>::from_raw(raw);
        assert!(typed.is_some());
        assert!(!typed.is_none());
        assert_eq!(typed.raw(), raw);
        let untyped = typed.untyped();
        assert_eq!(untyped.asset_type(), BamComponent::from_raw(5));
        assert_eq!(FfiAssetHandle::from(untyped), raw);
        assert!(TypedAssetHandle::<FakeTexture>::NONE.is_none());
    }
}
