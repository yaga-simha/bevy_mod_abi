//! Safe deferred command queue wrapper for guest systems.

use core::ffi::CStr;

use bevy_abi_mod_guest_sys::{ValidatedCommandQueue, ValidatedCommandQueueAdapter};
use bevy_abi_mod_sys::{AbiPod, FfiEntity, FfiResult};

use crate::component::GuestComponent;

/// Safe wrapper around the host's deferred command queue handle.
///
/// Obtain a `GuestCommandQueue` safely through [`SystemContext::commands`](crate::SystemContext::commands),
/// [`SystemContext::iter_and_commands`](crate::SystemContext::iter_and_commands), or
/// [`MultiQueryContext::commands`](crate::MultiQueryContext::commands).
///
/// Constructing a `GuestCommandQueue` directly from a raw pointer without `unsafe` is rejected:
///
/// ```compile_fail
/// use bevy_abi_mod_guest::prelude::*;
///
/// let _queue = GuestCommandQueue::new(core::ptr::null_mut(), CommandBridgeVTable::default());
/// ```
///
/// Inserting a type that does not implement [`AbiPod`] is rejected at compile time:
///
/// ```compile_fail
/// use bevy_abi_mod_guest::prelude::*;
///
/// #[repr(C)]
/// #[derive(Clone, Copy)]
/// struct NotPod {
///     x: f32,
/// }
///
/// fn bad_insert(cmds: &mut GuestCommandQueue<'_>, entity: FfiEntity) {
///     let _ = cmds.insert_value(entity, c"NotPod", &NotPod { x: 1.0 });
/// }
/// ```
pub struct GuestCommandQueue<'a> {
    inner: ValidatedCommandQueue<'a>,
}

impl<'a> GuestCommandQueue<'a> {
    #[inline]
    pub(crate) fn from_validated(inner: ValidatedCommandQueue<'a>) -> Self {
        Self { inner }
    }

    /// Returns `true` if the underlying `RawCommandQueue` pointer is null.
    #[inline]
    pub fn is_null(&self) -> bool {
        self.inner.is_null()
    }

    /// Enqueues creation of a new empty entity and returns its allocated [`FfiEntity`].
    #[inline]
    pub fn spawn_empty(&mut self) -> FfiEntity {
        self.inner.spawn_empty()
    }

    /// Enqueues despawning of `entity`.
    #[inline]
    pub fn despawn(&mut self, entity: FfiEntity) -> FfiResult {
        self.inner.despawn(entity)
    }

    /// Safely enqueues insertion of a typed slice of [`AbiPod`] elements onto `entity`.
    #[inline]
    pub fn insert_slice<T: AbiPod>(
        &mut self,
        entity: FfiEntity,
        component_name: &CStr,
        data: &[T],
    ) -> FfiResult {
        self.inner.insert_slice(entity, component_name, data)
    }

    /// Safely enqueues insertion of a single typed [`AbiPod`] component value onto `entity`.
    #[inline]
    pub fn insert_value<T: AbiPod>(
        &mut self,
        entity: FfiEntity,
        component_name: &CStr,
        value: &T,
    ) -> FfiResult {
        self.inner.insert_value(entity, component_name, value)
    }

    /// Safely enqueues deferred insertion of a typed [`GuestComponent`] value onto `entity`.
    #[inline]
    pub fn insert_component<T: GuestComponent>(
        &mut self,
        entity: FfiEntity,
        value: &T,
    ) -> FfiResult {
        self.insert_value(entity, T::NAME, value)
    }

    /// Safely enqueues deferred insertion of a typed runtime dynamic [`AbiPod`] component value onto `entity`.
    #[inline]
    pub fn insert_runtime_component<T: AbiPod>(
        &mut self,
        entity: FfiEntity,
        name: &CStr,
        value: &T,
    ) -> FfiResult {
        self.insert_value(entity, name, value)
    }

    /// Safely enqueues deferred removal of `component_name` from `entity`.
    #[inline]
    pub fn remove_component(&mut self, entity: FfiEntity, component_name: &CStr) -> FfiResult {
        self.inner.remove_component(entity, component_name)
    }

    /// Safely enqueues deferred removal of a typed [`GuestComponent`] from `entity`.
    #[inline]
    pub fn remove_guest_component<T: GuestComponent>(&mut self, entity: FfiEntity) -> FfiResult {
        self.remove_component(entity, T::NAME)
    }

    /// Safely enqueues deferred insertion of a host-constructed default component onto `entity`.
    #[inline]
    pub fn insert_default(&mut self, entity: FfiEntity, component_name: &CStr) -> FfiResult {
        self.inner.insert_default(entity, component_name)
    }

    /// Safely enqueues deferred default insertion of a typed [`GuestComponent`] onto `entity`.
    #[inline]
    pub fn insert_guest_default<T: GuestComponent>(&mut self, entity: FfiEntity) -> FfiResult {
        self.insert_default(entity, T::NAME)
    }
}

impl<'a> ValidatedCommandQueueAdapter<'a> for GuestCommandQueue<'a> {
    #[inline]
    fn from_validated_queue(inner: ValidatedCommandQueue<'a>) -> Self {
        Self { inner }
    }

    #[inline]
    fn as_validated_queue_mut(&mut self) -> &mut ValidatedCommandQueue<'a> {
        &mut self.inner
    }

    #[inline]
    fn into_validated_queue(self) -> ValidatedCommandQueue<'a> {
        self.inner
    }
}
