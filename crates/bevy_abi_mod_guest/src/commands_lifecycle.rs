//! Extended entity lifecycle, persistent spawn, and safe non-POD [`ModComponent`] guest APIs.

#[doc(hidden)]
pub use bevy_abi_mod_guest_sys::resolve_guest_instance_symbol;
use bevy_abi_mod_guest_sys::{
    SpawnEntry, iter_dynamic_slice, iter_dynamic_slice_mut, iter_entities_and_dynamic_slice_mut,
    resolve_mod_component,
};
use bevy_abi_mod_sys::{
    AccessMode, BamComponent, BamTermDesc, FfiEntity, FfiResult, MAX_TERMS_PER_QUERY, ModComponent,
};

use crate::{
    app::{GuestApp, QueryBuilder},
    commands::GuestCommands,
    iter::{Chunk, ChunkIter},
};

impl<'a> GuestApp<'a> {
    /// Registers a non-POD (or POD) [`ModComponent`] dynamic component `T` on the host `World`
    /// with automatic manifest qualification, panic-guarded guest `ComponentDropFn`,
    /// `T::erased_vtable_ptr()`, `"bam.v0.<name>.init_default"`, and `T::symbols()` stored inside
    /// [`GuestInstance`](crate::raw::GuestInstance) for live schema evolution across `reload_mod`.
    #[inline]
    pub fn register_dynamic_component<T: ModComponent + Default + Send + Sync + 'static>(
        &mut self,
    ) -> Result<BamComponent, FfiResult> {
        self.register_dynamic_component_with_flags::<T>(0)
    }

    /// Registers a non-POD (or POD) [`ModComponent`] dynamic component `T` with additional
    /// `BAM_COMPONENT_FLAG_*` flags merged with `T::COMPONENT_FLAGS`.
    #[inline]
    pub fn register_dynamic_component_with_flags<
        T: ModComponent + Default + Send + Sync + 'static,
    >(
        &mut self,
        flags: u32,
    ) -> Result<BamComponent, FfiResult> {
        self.load_ctx
            .register_dynamic_mod_component::<T>(self.instance, flags)
    }

    /// Resolves a dynamic [`ModComponent`] handle on the host, trying the manifest-qualified name
    /// first when `T::CANONICAL_NAME` is unqualified.
    #[inline]
    pub fn resolve_dynamic_component<T: ModComponent>(&self) -> Result<BamComponent, FfiResult> {
        resolve_mod_component::<T>(self.host())
    }
}

impl<'a> GuestCommands<'a> {
    /// Reserves a fresh empty entity whose lifetime persists across mod unload
    /// (`BamHostApi::cmd_spawn_persistent`), returning its [`FfiEntity`] identifier.
    #[inline]
    pub fn spawn_empty_persistent(&mut self) -> Result<FfiEntity, FfiResult> {
        self.inner.spawn_persistent(&[])
    }

    /// Reserves a fresh entity whose lifetime persists across mod unload
    /// (`BamHostApi::cmd_spawn_persistent`) and batch-inserts `entries` in a single archetype
    /// transition.
    #[inline]
    pub fn spawn_persistent(&mut self, entries: &[SpawnEntry<'_>]) -> Result<FfiEntity, FfiResult> {
        self.inner.spawn_persistent(entries)
    }

    /// Resolves a dynamic [`ModComponent`] handle on the host, trying the manifest-qualified name
    /// first when `T::CANONICAL_NAME` is unqualified.
    #[inline]
    pub fn resolve_dynamic_component<T: ModComponent>(&self) -> Result<BamComponent, FfiResult> {
        resolve_mod_component::<T>(self.inner.host())
    }

    /// Moves an owned non-POD (or POD) [`ModComponent`] `value` onto `entity` using pre-resolved
    /// `component`, dropping `value` on the guest heap if queueing fails.
    #[inline]
    pub fn insert_dynamic<T: ModComponent>(
        &mut self,
        entity: FfiEntity,
        component: BamComponent,
        value: T,
    ) -> FfiResult {
        self.inner.insert_moved(entity, component, value)
    }

    /// Resolves `T: ModComponent` on the host and moves `value` onto `entity`.
    #[inline]
    pub fn insert_dynamic_component<T: ModComponent>(
        &mut self,
        entity: FfiEntity,
        value: T,
    ) -> FfiResult {
        match resolve_mod_component::<T>(self.inner.host()) {
            Ok(comp) => self.inner.insert_moved(entity, comp, value),
            Err(err) => err,
        }
    }

    /// Spawns a mod-scoped entity carrying owned dynamic [`ModComponent`] `value` under pre-resolved
    /// `component`, dropping `value` on the guest heap if spawning fails.
    #[inline]
    pub fn spawn_dynamic<T: ModComponent>(
        &mut self,
        component: BamComponent,
        value: T,
    ) -> Result<FfiEntity, FfiResult> {
        self.inner.spawn_moved(component, value, &[], false)
    }

    /// Resolves `T: ModComponent` on the host and spawns a mod-scoped entity carrying `value`.
    #[inline]
    pub fn spawn_dynamic_component<T: ModComponent>(
        &mut self,
        value: T,
    ) -> Result<FfiEntity, FfiResult> {
        let comp = resolve_mod_component::<T>(self.inner.host())?;
        self.inner.spawn_moved(comp, value, &[], false)
    }

    /// Spawns a persistent entity (surviving mod unload) carrying owned dynamic [`ModComponent`]
    /// `value` under pre-resolved `component`.
    #[inline]
    pub fn spawn_dynamic_persistent<T: ModComponent>(
        &mut self,
        component: BamComponent,
        value: T,
    ) -> Result<FfiEntity, FfiResult> {
        self.inner.spawn_moved(component, value, &[], true)
    }

    /// Resolves `T: ModComponent` on the host and spawns a persistent entity carrying `value`.
    #[inline]
    pub fn spawn_dynamic_component_persistent<T: ModComponent>(
        &mut self,
        value: T,
    ) -> Result<FfiEntity, FfiResult> {
        let comp = resolve_mod_component::<T>(self.inner.host())?;
        self.inner.spawn_moved(comp, value, &[], true)
    }

    /// Spawns a mod-scoped entity carrying owned dynamic [`ModComponent`] `value` alongside
    /// additional `AbiPod` `extra` [`SpawnEntry`] components in one batch.
    #[inline]
    pub fn spawn_with_dynamic<T: ModComponent>(
        &mut self,
        component: BamComponent,
        value: T,
        extra: &[SpawnEntry<'_>],
    ) -> Result<FfiEntity, FfiResult> {
        self.inner.spawn_moved(component, value, extra, false)
    }

    /// Spawns a persistent entity carrying owned dynamic [`ModComponent`] `value` alongside
    /// additional `AbiPod` `extra` [`SpawnEntry`] components in one batch.
    #[inline]
    pub fn spawn_with_dynamic_persistent<T: ModComponent>(
        &mut self,
        component: BamComponent,
        value: T,
        extra: &[SpawnEntry<'_>],
    ) -> Result<FfiEntity, FfiResult> {
        self.inner.spawn_moved(component, value, extra, true)
    }
}

impl<'b> QueryBuilder<'b> {
    #[inline]
    fn push_dynamic_term<T: ModComponent>(&mut self, mode: AccessMode) {
        match resolve_mod_component::<T>(self.host) {
            Ok(component) => {
                if !component.is_valid() {
                    if self.first_error.is_none() {
                        self.first_error = Some(FfiResult::INVALID_COMPONENT);
                    }
                    return;
                }
                if self.terms.len() >= MAX_TERMS_PER_QUERY {
                    if self.first_error.is_none() {
                        self.first_error = Some(FfiResult::CAPACITY_EXCEEDED);
                    }
                    return;
                }
                self.terms.push(BamTermDesc::new(component, mode));
            }
            Err(err) => {
                if self.first_error.is_none() {
                    self.first_error = Some(err);
                }
            }
        }
    }

    /// Appends a shared read-only (`&T`) term for a dynamic [`ModComponent`] (`T`).
    #[inline]
    pub fn read_dynamic<T: ModComponent>(mut self) -> Self {
        self.push_dynamic_term::<T>(AccessMode::READ);
        self
    }

    /// Appends an exclusive mutable (`&mut T`) term for a dynamic [`ModComponent`] (`T`).
    #[inline]
    pub fn write_dynamic<T: ModComponent>(mut self) -> Self {
        self.push_dynamic_term::<T>(AccessMode::WRITE);
        self
    }

    /// Appends a `With<T>` filter term for a dynamic [`ModComponent`] (`T`).
    #[inline]
    pub fn with_dynamic<T: ModComponent>(mut self) -> Self {
        self.push_dynamic_term::<T>(AccessMode::FILTER_ONLY);
        self
    }

    /// Appends a `Without<T>` filter term for a dynamic [`ModComponent`] (`T`).
    #[inline]
    pub fn without_dynamic<T: ModComponent>(mut self) -> Self {
        self.push_dynamic_term::<T>(AccessMode::WITHOUT);
        self
    }

    /// Appends an `Option<&T>` term for a dynamic [`ModComponent`] (`T`).
    #[inline]
    pub fn optional_read_dynamic<T: ModComponent>(mut self) -> Self {
        self.push_dynamic_term::<T>(AccessMode::OPTIONAL_READ);
        self
    }

    /// Appends an `Option<&mut T>` term for a dynamic [`ModComponent`] (`T`).
    #[inline]
    pub fn optional_write_dynamic<T: ModComponent>(mut self) -> Self {
        self.push_dynamic_term::<T>(AccessMode::OPTIONAL_WRITE);
        self
    }
}

impl<'iter, 'ctx> Chunk<'iter, 'ctx> {
    /// Borrows dense column `col` as `&[T]` for a same-mod dynamic [`ModComponent`] (`T`),
    /// verifying layout, [`ComponentUuid`](bevy_abi_mod_sys::ComponentUuid), and active same-mod
    /// VTable provenance.
    #[inline]
    pub fn dynamic_component<T: ModComponent>(&self, col: usize) -> Option<&[T]> {
        iter_dynamic_slice::<T>(self.iter, col)
    }

    /// Mutably borrows dense column `col` as `&mut [T]` for a same-mod dynamic [`ModComponent`]
    /// (`T`), marking chunk rows changed and verifying active same-mod VTable provenance.
    #[inline]
    pub fn dynamic_component_mut<T: ModComponent>(&mut self, col: usize) -> Option<&mut [T]> {
        iter_dynamic_slice_mut::<T>(self.iter, col, true)
    }

    /// Mutably borrows dense column `col` as `&mut [T]` without automatically marking Bevy change
    /// ticks, for a same-mod dynamic [`ModComponent`] (`T`).
    #[inline]
    pub fn dynamic_component_mut_untracked<T: ModComponent>(
        &mut self,
        col: usize,
    ) -> Option<&mut [T]> {
        iter_dynamic_slice_mut::<T>(self.iter, col, false)
    }

    /// Simultaneously borrows `[FfiEntity; len]` and writable dense dynamic [`ModComponent`] column
    /// `write_col` (`&mut [T]`).
    #[inline]
    pub fn entities_and_dynamic_component_mut<T: ModComponent>(
        &mut self,
        write_col: usize,
    ) -> Option<(&[FfiEntity], &mut [T])> {
        iter_entities_and_dynamic_slice_mut::<T>(self.iter, write_col)
    }
}

impl<'a> ChunkIter<'a> {
    /// Resolves `T: ModComponent` dynamically for each chunk via [`Chunk::dynamic_component`].
    #[inline]
    pub fn iter_host(&self) -> &'a bevy_abi_mod_guest_sys::Host {
        self.inner.host()
    }
}
