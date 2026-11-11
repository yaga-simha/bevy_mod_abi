//! Safe execution context passed to single-query guest systems.

use core::ffi::CStr;

use bevy_abi_mod_sys::{AbiPod, HostApi};

use crate::commands::GuestCommandQueue;
use crate::component::GuestComponent;
use crate::iter::GuestChunkIter;

/// Safe execution context passed to systems registered via [`GuestApp::add_system`](crate::GuestApp::add_system).
///
/// `SystemContext` and [`MultiQueryContext`](crate::MultiQueryContext) are the only safe entry
/// points for obtaining a [`GuestChunkIter`] or [`GuestCommandQueue`] in `bevy_abi_mod_guest`.
///
/// # Compile-Fail Safety Check
///
/// Borrowing a non-[`AbiPod`] reference type as an ECS resource is rejected at compile time:
///
/// ```compile_fail
/// use bevy_abi_mod_guest::SystemContext;
///
/// fn bad_resource(ctx: &SystemContext<'_>) {
///     let _ = ctx.resource::<&'static u32>(c"RefResource");
/// }
/// ```
pub struct SystemContext<'a> {
    pub(crate) iter: GuestChunkIter<'a>,
    pub(crate) commands: GuestCommandQueue<'a>,
    pub(crate) host_api: &'a HostApi,
}

impl<'a> SystemContext<'a> {
    /// Borrows the system's [`GuestChunkIter`] mutably.
    #[inline]
    pub fn iter(&mut self) -> &mut GuestChunkIter<'a> {
        &mut self.iter
    }

    /// Borrows the system's deferred [`GuestCommandQueue`] mutably.
    #[inline]
    pub fn commands(&mut self) -> &mut GuestCommandQueue<'a> {
        &mut self.commands
    }

    /// Simultaneously borrows both the [`GuestChunkIter`] and the deferred [`GuestCommandQueue`].
    #[inline]
    pub fn iter_and_commands(&mut self) -> (&mut GuestChunkIter<'a>, &mut GuestCommandQueue<'a>) {
        (&mut self.iter, &mut self.commands)
    }

    /// Returns a shared reference to the host's [`HostApi`] VTable.
    #[inline]
    pub fn host_api(&self) -> &'a HostApi {
        self.host_api
    }

    /// Obtains a read-only byte pointer to an exposed Bevy `Resource` (`Res<R>`).
    #[inline]
    pub fn resource_ptr(&self, resource_name: &CStr) -> *const u8 {
        self.iter.resource_ptr(resource_name)
    }

    /// Obtains a mutable byte pointer to an exposed Bevy `Resource` (`ResMut<R>`), marking its change tick.
    #[inline]
    pub fn resource_mut_ptr(&mut self, resource_name: &CStr) -> *mut u8 {
        self.iter.resource_mut_ptr(resource_name)
    }

    /// Safely borrows a typed [`AbiPod`] Bevy `Resource` (`Res<R>`).
    #[inline]
    pub fn resource<R: AbiPod>(&self, resource_name: &CStr) -> Option<&R> {
        self.iter.resource::<R>(resource_name)
    }

    /// Safely borrows a typed [`AbiPod`] Bevy `Resource` (`ResMut<R>`) mutably, marking its change tick.
    #[inline]
    pub fn resource_mut<R: AbiPod>(&mut self, resource_name: &CStr) -> Option<&mut R> {
        self.iter.resource_mut::<R>(resource_name)
    }

    /// Safely borrows a typed [`GuestComponent`] resource (`Res<R>`).
    #[inline]
    pub fn guest_resource<R: GuestComponent>(&self) -> Option<&R> {
        self.iter.guest_resource::<R>()
    }

    /// Safely borrows a typed [`GuestComponent`] resource (`ResMut<R>`) mutably.
    #[inline]
    pub fn guest_resource_mut<R: GuestComponent>(&mut self) -> Option<&mut R> {
        self.iter.guest_resource_mut::<R>()
    }
}
