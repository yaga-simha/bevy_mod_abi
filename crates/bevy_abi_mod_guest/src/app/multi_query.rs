//! Safe execution context passed to multi-query guest systems.

use core::ffi::CStr;

use bevy_abi_mod_guest_sys::{MAX_GUEST_MULTI_QUERIES, ResourceBridgeVTable};
use bevy_abi_mod_sys::{AbiPod, HostApi};

use crate::commands::GuestCommandQueue;
use crate::component::GuestComponent;
use crate::iter::GuestChunkIter;

/// Safe execution context passed to multi-query systems registered via
/// [`GuestApp::add_multi_query_system`](crate::GuestApp::add_multi_query_system).
pub struct MultiQueryContext<'a> {
    pub(crate) iters: [Option<GuestChunkIter<'a>>; MAX_GUEST_MULTI_QUERIES],
    pub(crate) iter_count: usize,
    pub(crate) commands: GuestCommandQueue<'a>,
    pub(crate) host_api: &'a HostApi,
    pub(crate) res_vtable: ResourceBridgeVTable,
}

impl<'a> MultiQueryContext<'a> {
    /// Number of query iterators passed to this multi-query system.
    #[inline]
    pub fn iter_count(&self) -> usize {
        self.iter_count
    }

    /// Borrows query iterator `index` mutably.
    #[inline]
    pub fn iter_mut(&mut self, index: usize) -> Option<&mut GuestChunkIter<'a>> {
        if index < self.iter_count {
            self.iters[index].as_mut()
        } else {
            None
        }
    }

    /// Simultaneously borrows two disjoint query iterators `(index_a, index_b)` for nested query loops.
    #[inline]
    pub fn iters_2_mut(
        &mut self,
        index_a: usize,
        index_b: usize,
    ) -> Option<(&mut GuestChunkIter<'a>, &mut GuestChunkIter<'a>)> {
        if index_a == index_b || index_a >= self.iter_count || index_b >= self.iter_count {
            return None;
        }
        if index_a < index_b {
            let (left, right) = self.iters.split_at_mut(index_b);
            Some((left[index_a].as_mut()?, right[0].as_mut()?))
        } else {
            let (left, right) = self.iters.split_at_mut(index_a);
            Some((right[0].as_mut()?, left[index_b].as_mut()?))
        }
    }

    /// Borrows the deferred [`GuestCommandQueue`] mutably.
    #[inline]
    pub fn commands(&mut self) -> &mut GuestCommandQueue<'a> {
        &mut self.commands
    }

    /// Simultaneously borrows two disjoint query iterators and the deferred [`GuestCommandQueue`].
    #[inline]
    pub fn iters_2_and_commands(
        &mut self,
        index_a: usize,
        index_b: usize,
    ) -> Option<(
        &mut GuestChunkIter<'a>,
        &mut GuestChunkIter<'a>,
        &mut GuestCommandQueue<'a>,
    )> {
        if index_a == index_b || index_a >= self.iter_count || index_b >= self.iter_count {
            return None;
        }
        let commands = &mut self.commands;
        if index_a < index_b {
            let (left, right) = self.iters.split_at_mut(index_b);
            Some((left[index_a].as_mut()?, right[0].as_mut()?, commands))
        } else {
            let (left, right) = self.iters.split_at_mut(index_a);
            Some((right[0].as_mut()?, left[index_b].as_mut()?, commands))
        }
    }

    /// Returns a shared reference to the host's [`HostApi`] VTable.
    #[inline]
    pub fn host_api(&self) -> &'a HostApi {
        self.host_api
    }

    /// Obtains a read-only byte pointer to an exposed Bevy `Resource` (`Res<R>`) declared in any
    /// query spec of this multi-query system.
    #[inline]
    pub fn resource_ptr(&self, resource_name: &CStr) -> *const u8 {
        for slot in self.iters.iter().take(self.iter_count).flatten() {
            let ptr = slot
                .as_validated()
                .multi_query_resource_readonly(&self.res_vtable, resource_name);
            if !ptr.is_null() {
                return ptr;
            }
        }
        core::ptr::null()
    }

    /// Obtains a mutable byte pointer to an exposed Bevy `Resource` (`ResMut<R>`) declared with
    /// `RESOURCE_WRITE` in any query spec of this multi-query system, marking its change tick.
    #[inline]
    pub fn resource_mut_ptr(&mut self, resource_name: &CStr) -> *mut u8 {
        for slot in self.iters.iter_mut().take(self.iter_count).flatten() {
            let ptr = slot
                .as_validated_mut()
                .multi_query_resource_mutable(&self.res_vtable, resource_name);
            if !ptr.is_null() {
                return ptr;
            }
        }
        core::ptr::null_mut()
    }

    /// Safely borrows a typed [`AbiPod`] Bevy `Resource` (`Res<R>`) across any query spec.
    #[inline]
    pub fn resource<R: AbiPod>(&self, resource_name: &CStr) -> Option<&R> {
        for slot in self.iters.iter().take(self.iter_count).flatten() {
            if let Some(res) = slot
                .as_validated()
                .multi_query_resource::<R>(&self.res_vtable, resource_name)
            {
                return Some(res);
            }
        }
        None
    }

    /// Safely borrows a typed [`AbiPod`] Bevy `Resource` (`ResMut<R>`) mutably across any query spec.
    #[inline]
    pub fn resource_mut<R: AbiPod>(&mut self, resource_name: &CStr) -> Option<&mut R> {
        for slot in self.iters.iter_mut().take(self.iter_count).flatten() {
            if let Some(res) = slot
                .as_validated_mut()
                .multi_query_resource_mut::<R>(&self.res_vtable, resource_name)
            {
                return Some(res);
            }
        }
        None
    }

    /// Safely borrows a typed [`GuestComponent`] resource (`Res<R>`) using `R::NAME`.
    #[inline]
    pub fn guest_resource<R: GuestComponent>(&self) -> Option<&R> {
        self.resource::<R>(R::NAME)
    }

    /// Safely borrows a typed [`GuestComponent`] resource (`ResMut<R>`) mutably using `R::NAME`.
    #[inline]
    pub fn guest_resource_mut<R: GuestComponent>(&mut self) -> Option<&mut R> {
        self.resource_mut::<R>(R::NAME)
    }
}
