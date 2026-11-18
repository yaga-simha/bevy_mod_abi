//! Safe `#![forbid(unsafe_code)]` Bevy 0.20 observer, lifecycle, mutable payload,
//! relationship-propagation trigger, and runtime entity-observer SDK extensions.

use core::ffi::CStr;

use bevy_abi_mod_guest_sys::{ValidatedCommands, resolve_component};
use bevy_abi_mod_sys::{
    AbiPod, BAM_OBSERVER_FLAG_PROPAGATE, BAM_TRIGGER_FLAG_PROPAGATE, BamComponent, BamEventKind,
    FfiEntity, FfiResult,
};

use crate::{
    app::builder::GuestApp,
    commands::GuestCommands,
    component::{GuestComponent, resolve_guest_component},
};

const READY_CANONICAL_CSTR: &CStr = c"bevy_scene::Ready";
const READY_ALIAS_CSTR: &CStr = c"Ready";

#[inline]
pub(crate) fn with_guest_commands<R>(
    validated_cmds: &mut ValidatedCommands<'_>,
    f: impl FnOnce(&mut GuestCommands<'_>) -> R,
) -> R {
    let host = validated_cmds.host();
    let mut cmds = GuestCommands::from_validated(ValidatedCommands::empty(host));
    core::mem::swap(&mut cmds.inner, validated_cmds);
    let out = f(&mut cmds);
    core::mem::swap(&mut cmds.inner, validated_cmds);
    out
}

impl<'a> GuestApp<'a> {
    /// Registers a Bevy-idiomatic dynamic observer for `E: GuestComponent` receiving
    /// `(target_entity, &E, &mut GuestCommands<'_>)`.
    #[inline]
    pub fn add_observer<E, F>(
        &mut self,
        name: &str,
        mut callback: F,
    ) -> Result<&mut Self, FfiResult>
    where
        E: GuestComponent,
        F: FnMut(FfiEntity, &E, &mut GuestCommands<'_>) + Send + 'static,
    {
        let event = self.resolve_component::<E>()?;
        self.observe_handle::<E, _>(name, event, FfiEntity::NULL, move |tgt, ev, cmds| {
            callback(tgt, ev, cmds);
        })
    }

    /// Registers a dynamic component-lifecycle or scene `Ready` observer for `watched_components`
    /// (`FfiEntity::NULL` for global, or a specific `target` entity).
    pub fn observe_lifecycle<F>(
        &mut self,
        name: &str,
        event_kind: BamEventKind,
        watched_components: &[BamComponent],
        target: FfiEntity,
        mut callback: F,
    ) -> Result<&mut Self, FfiResult>
    where
        F: FnMut(FfiEntity, &mut GuestCommands<'_>) + Send + 'static,
    {
        let status = self.load_ctx.register_lifecycle_observer(
            self.instance,
            name,
            event_kind,
            watched_components,
            target,
            0,
            BamComponent::INVALID,
            move |tgt, validated_cmds| {
                with_guest_commands(validated_cmds, |cmds| callback(tgt, cmds));
                FfiResult::OK
            },
        );
        if status.is_ok() {
            Ok(self)
        } else {
            Err(status)
        }
    }

    /// Registers a dynamic `Add<C>` lifecycle observer invoked when `C` is newly added to any entity.
    #[inline]
    pub fn observe_add<C, F>(&mut self, name: &str, callback: F) -> Result<&mut Self, FfiResult>
    where
        C: GuestComponent,
        F: FnMut(FfiEntity, &mut GuestCommands<'_>) + Send + 'static,
    {
        let comp = self.resolve_component::<C>()?;
        self.observe_lifecycle(name, BamEventKind::ADD, &[comp], FfiEntity::NULL, callback)
    }

    /// Registers a dynamic `Insert<C>` lifecycle observer invoked whenever `C` is inserted or overwritten.
    #[inline]
    pub fn observe_insert<C, F>(&mut self, name: &str, callback: F) -> Result<&mut Self, FfiResult>
    where
        C: GuestComponent,
        F: FnMut(FfiEntity, &mut GuestCommands<'_>) + Send + 'static,
    {
        let comp = self.resolve_component::<C>()?;
        self.observe_lifecycle(
            name,
            BamEventKind::INSERT,
            &[comp],
            FfiEntity::NULL,
            callback,
        )
    }

    /// Registers a dynamic `Discard<C>` lifecycle observer invoked before `C` is overwritten or removed.
    #[inline]
    pub fn observe_discard<C, F>(&mut self, name: &str, callback: F) -> Result<&mut Self, FfiResult>
    where
        C: GuestComponent,
        F: FnMut(FfiEntity, &mut GuestCommands<'_>) + Send + 'static,
    {
        let comp = self.resolve_component::<C>()?;
        self.observe_lifecycle(
            name,
            BamEventKind::DISCARD,
            &[comp],
            FfiEntity::NULL,
            callback,
        )
    }

    /// Registers a dynamic `Remove<C>` lifecycle observer invoked before `C` is removed from an entity.
    #[inline]
    pub fn observe_remove<C, F>(&mut self, name: &str, callback: F) -> Result<&mut Self, FfiResult>
    where
        C: GuestComponent,
        F: FnMut(FfiEntity, &mut GuestCommands<'_>) + Send + 'static,
    {
        let comp = self.resolve_component::<C>()?;
        self.observe_lifecycle(
            name,
            BamEventKind::REMOVE,
            &[comp],
            FfiEntity::NULL,
            callback,
        )
    }

    /// Registers a dynamic `Despawn<C>` lifecycle observer invoked when an entity with `C` despawns.
    #[inline]
    pub fn observe_despawn<C, F>(&mut self, name: &str, callback: F) -> Result<&mut Self, FfiResult>
    where
        C: GuestComponent,
        F: FnMut(FfiEntity, &mut GuestCommands<'_>) + Send + 'static,
    {
        let comp = self.resolve_component::<C>()?;
        self.observe_lifecycle(
            name,
            BamEventKind::DESPAWN,
            &[comp],
            FfiEntity::NULL,
            callback,
        )
    }

    /// Registers a dynamic scene/hierarchy `Ready` observer (`FfiEntity::NULL` for all scenes, or
    /// scoped to a specific `target` entity).
    #[inline]
    pub fn observe_ready<F>(
        &mut self,
        name: &str,
        target: FfiEntity,
        callback: F,
    ) -> Result<&mut Self, FfiResult>
    where
        F: FnMut(FfiEntity, &mut GuestCommands<'_>) + Send + 'static,
    {
        self.observe_lifecycle(name, BamEventKind::READY, &[], target, callback)
    }

    /// Registers a global dynamic scene/hierarchy `Ready` observer invoked for every entity that
    /// receives `Ready` across the `World`.
    #[inline]
    pub fn observe_ready_global<F>(
        &mut self,
        name: &str,
        callback: F,
    ) -> Result<&mut Self, FfiResult>
    where
        F: FnMut(FfiEntity, &mut GuestCommands<'_>) + Send + 'static,
    {
        self.observe_ready(name, FfiEntity::NULL, callback)
    }

    /// Starts building a configurable `Ready` observer supporting entity scoping and
    /// `ChildOf` / custom-relationship propagation (`ObserverBuilder::with_propagate`).
    #[inline]
    pub fn observe_ready_with(&mut self, name: impl Into<String>) -> ObserverBuilder<'_, 'a> {
        ObserverBuilder::ready(self, name)
    }

    /// Registers a global mutable event observer (`&mut E`, `&mut bool` propagate) for `E: GuestComponent`.
    pub fn observe_mut<E, F>(&mut self, name: &str, mut callback: F) -> Result<&mut Self, FfiResult>
    where
        E: GuestComponent,
        F: FnMut(&mut E, &mut bool, &mut GuestCommands<'_>) + Send + 'static,
    {
        self.observe_entity_mut::<E, _>(name, FfiEntity::NULL, move |_tgt, ev, prop, cmds| {
            callback(ev, prop, cmds);
        })
    }

    /// Registers an entity-scoped mutable event observer (`target`, `&mut E`, `&mut bool` propagate)
    /// for `E: GuestComponent` (`FfiEntity::NULL` observes all targets).
    pub fn observe_entity_mut<E, F>(
        &mut self,
        name: &str,
        target: FfiEntity,
        callback: F,
    ) -> Result<&mut Self, FfiResult>
    where
        E: GuestComponent,
        F: FnMut(FfiEntity, &mut E, &mut bool, &mut GuestCommands<'_>) + Send + 'static,
    {
        let event = self.resolve_component::<E>()?;
        self.observe_mut_handle::<E, _>(name, event, target, 0, BamComponent::INVALID, callback)
    }

    /// Registers a mutable observer for a pre-resolved event handle `event`.
    pub fn observe_mut_handle<E, F>(
        &mut self,
        name: &str,
        event: BamComponent,
        target: FfiEntity,
        flags: u32,
        propagate_relationship: BamComponent,
        mut callback: F,
    ) -> Result<&mut Self, FfiResult>
    where
        E: AbiPod,
        F: FnMut(FfiEntity, &mut E, &mut bool, &mut GuestCommands<'_>) + Send + 'static,
    {
        let status = self.load_ctx.register_observer_mut::<E, _>(
            self.instance,
            name,
            event,
            target,
            flags,
            propagate_relationship,
            move |tgt, ev, prop, validated_cmds| {
                with_guest_commands(validated_cmds, |cmds| callback(tgt, ev, prop, cmds));
                FfiResult::OK
            },
        );
        if status.is_ok() {
            Ok(self)
        } else {
            Err(status)
        }
    }
}

impl<'a> GuestCommands<'a> {
    /// Queues deferred spawning of an entity-scoped runtime observer for `E: GuestComponent` on
    /// `target` (pass `FfiEntity::NULL` for global observation), returning the reserved observer
    /// [`FfiEntity`].
    #[inline]
    pub fn observe_entity<E, F>(
        &mut self,
        target: FfiEntity,
        callback: F,
    ) -> Result<FfiEntity, FfiResult>
    where
        E: GuestComponent,
        F: FnMut(FfiEntity, &E, &mut GuestCommands<'_>) + Send + 'static,
    {
        self.observe_entity_named::<E, _>(None, target, callback)
    }

    /// Named variant of [`Self::observe_entity`].
    pub fn observe_entity_named<E, F>(
        &mut self,
        name: Option<&str>,
        target: FfiEntity,
        callback: F,
    ) -> Result<FfiEntity, FfiResult>
    where
        E: GuestComponent,
        F: FnMut(FfiEntity, &E, &mut GuestCommands<'_>) + Send + 'static,
    {
        let host = self.inner.host();
        let event = resolve_guest_component::<E>(host, host.manifest_name())?;
        self.observe_handle_runtime::<E, _>(name, event, target, 0, BamComponent::INVALID, callback)
    }

    /// Queues deferred spawning of a runtime observer for pre-resolved event handle `event`.
    pub fn observe_handle_runtime<E, F>(
        &mut self,
        name: Option<&str>,
        event: BamComponent,
        target: FfiEntity,
        flags: u32,
        propagate_relationship: BamComponent,
        mut callback: F,
    ) -> Result<FfiEntity, FfiResult>
    where
        E: AbiPod,
        F: FnMut(FfiEntity, &E, &mut GuestCommands<'_>) + Send + 'static,
    {
        self.inner.observe::<E, _>(
            name,
            event,
            target,
            flags,
            propagate_relationship,
            move |tgt, ev, validated_cmds| {
                with_guest_commands(validated_cmds, |cmds| callback(tgt, ev, cmds));
                FfiResult::OK
            },
        )
    }

    /// Queues deferred spawning of a mutable entity-scoped runtime observer for `E: GuestComponent`.
    pub fn observe_entity_mut<E, F>(
        &mut self,
        target: FfiEntity,
        mut callback: F,
    ) -> Result<FfiEntity, FfiResult>
    where
        E: GuestComponent,
        F: FnMut(FfiEntity, &mut E, &mut bool, &mut GuestCommands<'_>) + Send + 'static,
    {
        let host = self.inner.host();
        let event = resolve_guest_component::<E>(host, host.manifest_name())?;
        self.inner.observe_mut::<E, _>(
            None,
            event,
            target,
            0,
            BamComponent::INVALID,
            move |tgt, ev, prop, validated_cmds| {
                with_guest_commands(validated_cmds, |cmds| callback(tgt, ev, prop, cmds));
                FfiResult::OK
            },
        )
    }

    /// Queues deferred spawning of a runtime lifecycle or `Ready` observer.
    pub fn observe_entity_lifecycle<F>(
        &mut self,
        event_kind: BamEventKind,
        watched_components: &[BamComponent],
        target: FfiEntity,
        mut callback: F,
    ) -> Result<FfiEntity, FfiResult>
    where
        F: FnMut(FfiEntity, &mut GuestCommands<'_>) + Send + 'static,
    {
        self.inner.observe_lifecycle(
            None,
            event_kind,
            watched_components,
            target,
            0,
            BamComponent::INVALID,
            move |tgt, validated_cmds| {
                with_guest_commands(validated_cmds, |cmds| callback(tgt, cmds));
                FfiResult::OK
            },
        )
    }

    /// Triggers an `EntityEvent` `E: GuestComponent` on `target` that propagates upward along
    /// Bevy's `ChildOf` hierarchy (`BAM_TRIGGER_FLAG_PROPAGATE`) until an ancestor observer stops
    /// propagation or the root is reached.
    #[inline]
    pub fn trigger_propagated<E: GuestComponent>(
        &mut self,
        target: FfiEntity,
        event: &E,
    ) -> Result<(), FfiResult> {
        let host = self.inner.host();
        let handle = resolve_guest_component::<E>(host, host.manifest_name())?;
        self.trigger_propagated_handle(handle, target, event, BamComponent::INVALID)
    }

    /// Triggers an `EntityEvent` `E` on `target` propagating upward along custom relationship `R`
    /// (`FfiEntity`-backed relationship component).
    #[inline]
    pub fn trigger_propagated_along<E: GuestComponent, R: GuestComponent>(
        &mut self,
        target: FfiEntity,
        event: &E,
    ) -> Result<(), FfiResult> {
        let host = self.inner.host();
        let event_handle = resolve_guest_component::<E>(host, host.manifest_name())?;
        let rel_handle = resolve_guest_component::<R>(host, host.manifest_name())?;
        self.trigger_propagated_handle(event_handle, target, event, rel_handle)
    }

    /// Triggers a pre-resolved event handle `event_handle` on `target` with `BAM_TRIGGER_FLAG_PROPAGATE`
    /// (`propagate_relationship == BamComponent::INVALID` defaults to `ChildOf`).
    #[inline]
    pub fn trigger_propagated_handle<E: AbiPod>(
        &mut self,
        event_handle: BamComponent,
        target: FfiEntity,
        event: &E,
        propagate_relationship: BamComponent,
    ) -> Result<(), FfiResult> {
        let status = self.inner.trigger_ex(
            event_handle,
            target,
            event,
            BAM_TRIGGER_FLAG_PROPAGATE,
            propagate_relationship,
        );
        if status.is_ok() { Ok(()) } else { Err(status) }
    }

    /// Queues a synthetic lifecycle or `Ready` trigger via `BamHostApi::cmd_trigger_ex`.
    #[inline]
    pub fn trigger_lifecycle(
        &mut self,
        kind: BamEventKind,
        target: FfiEntity,
        watched_components: &[BamComponent],
    ) -> Result<(), FfiResult> {
        let status = self
            .inner
            .trigger_lifecycle(kind, target, watched_components);
        if status.is_ok() { Ok(()) } else { Err(status) }
    }

    /// Queues a synthetic scene `Ready` trigger for `target` via `BamHostApi::cmd_trigger_ex`.
    #[inline]
    pub fn trigger_ready(&mut self, target: FfiEntity) -> Result<(), FfiResult> {
        if target.is_null() {
            return Err(FfiResult::INVALID_ENTITY);
        }
        self.trigger_lifecycle(BamEventKind::READY, target, &[])
    }

    /// Queues deferred spawning of an entity-scoped runtime `Ready` observer on `target`,
    /// returning the reserved observer [`FfiEntity`].
    ///
    /// Returns [`FfiResult::INVALID_ENTITY`] when `target` is [`FfiEntity::NULL`]; use
    /// [`GuestApp::observe_ready_global`] for global `Ready` observation.
    #[inline]
    pub fn observe_entity_ready<F>(
        &mut self,
        target: FfiEntity,
        callback: F,
    ) -> Result<FfiEntity, FfiResult>
    where
        F: FnMut(FfiEntity, &mut GuestCommands<'_>) + Send + 'static,
    {
        if target.is_null() {
            return Err(FfiResult::INVALID_ENTITY);
        }
        self.observe_entity_lifecycle(BamEventKind::READY, &[], target, callback)
    }

    /// Queues deferred spawning of an entity-scoped runtime `Ready` observer on `target` with
    /// optional relationship propagation (`BAM_OBSERVER_FLAG_PROPAGATE`).
    pub fn observe_entity_ready_with<F>(
        &mut self,
        target: FfiEntity,
        propagate: bool,
        propagate_relationship: BamComponent,
        mut callback: F,
    ) -> Result<FfiEntity, FfiResult>
    where
        F: FnMut(FfiEntity, &mut GuestCommands<'_>) + Send + 'static,
    {
        if target.is_null() {
            return Err(FfiResult::INVALID_ENTITY);
        }
        let flags = if propagate || propagate_relationship.is_valid() {
            BAM_OBSERVER_FLAG_PROPAGATE
        } else {
            0
        };
        self.inner.observe_lifecycle(
            None,
            BamEventKind::READY,
            &[],
            target,
            flags,
            propagate_relationship,
            move |tgt, validated_cmds| {
                with_guest_commands(validated_cmds, |cmds| callback(tgt, cmds));
                FfiResult::OK
            },
        )
    }

    /// Queues a propagating `Ready` trigger starting at `target` and bubbling upward along Bevy's
    /// `ChildOf` hierarchy (`BAM_TRIGGER_FLAG_PROPAGATE`).
    pub fn trigger_ready_propagated(&mut self, target: FfiEntity) -> Result<(), FfiResult> {
        if target.is_null() {
            return Err(FfiResult::INVALID_ENTITY);
        }
        let ready_handle = self
            .resolve_component(Some(READY_CANONICAL_CSTR), None)
            .or_else(|_| self.resolve_component(Some(READY_ALIAS_CSTR), None))?;
        self.trigger_propagated_handle(ready_handle, target, &target, BamComponent::INVALID)
    }

    /// Queues a propagating `Ready` trigger starting at `target` and bubbling upward along custom
    /// `FfiEntity`-backed relationship component `R`.
    pub fn trigger_ready_propagated_along<R: GuestComponent>(
        &mut self,
        target: FfiEntity,
    ) -> Result<(), FfiResult> {
        if target.is_null() {
            return Err(FfiResult::INVALID_ENTITY);
        }
        let ready_handle = self
            .resolve_component(Some(READY_CANONICAL_CSTR), None)
            .or_else(|_| self.resolve_component(Some(READY_ALIAS_CSTR), None))?;
        let host = self.inner.host();
        let rel_handle = resolve_guest_component::<R>(host, host.manifest_name())?;
        self.trigger_propagated_handle(ready_handle, target, &target, rel_handle)
    }
}

/// Fluent `#![forbid(unsafe_code)]` builder for Bevy 0.20 `Ready` and lifecycle observers,
/// supporting entity scoping (`.with_target(...)`), upward hierarchy propagation
/// (`.with_propagate(true)`), and custom propagation relationships.
pub struct ObserverBuilder<'app, 'a> {
    app: &'app mut GuestApp<'a>,
    name: String,
    event_kind: BamEventKind,
    watched_components: Vec<BamComponent>,
    target: FfiEntity,
    flags: u32,
    propagate_relationship: BamComponent,
    first_error: Option<FfiResult>,
}

impl<'app, 'a> ObserverBuilder<'app, 'a> {
    /// Creates an [`ObserverBuilder`] targeting Bevy `Ready` (`BamEventKind::READY`) events.
    #[inline]
    pub fn ready(app: &'app mut GuestApp<'a>, name: impl Into<String>) -> Self {
        Self::lifecycle(app, name, BamEventKind::READY)
    }

    /// Creates an [`ObserverBuilder`] targeting a lifecycle [`BamEventKind`].
    #[inline]
    pub fn lifecycle(
        app: &'app mut GuestApp<'a>,
        name: impl Into<String>,
        event_kind: BamEventKind,
    ) -> Self {
        Self {
            app,
            name: name.into(),
            event_kind,
            watched_components: Vec::new(),
            target: FfiEntity::NULL,
            flags: 0,
            propagate_relationship: BamComponent::INVALID,
            first_error: None,
        }
    }

    /// Scopes this observer to `target` (`FfiEntity::NULL` observes globally).
    #[inline]
    pub const fn with_target(mut self, target: FfiEntity) -> Self {
        self.target = target;
        self
    }

    /// Enables or disables upward relationship propagation (`BAM_OBSERVER_FLAG_PROPAGATE`).
    #[inline]
    pub const fn with_propagate(mut self, propagate: bool) -> Self {
        if propagate {
            self.flags |= BAM_OBSERVER_FLAG_PROPAGATE;
        } else {
            self.flags &= !BAM_OBSERVER_FLAG_PROPAGATE;
        }
        self
    }

    /// Configures propagation along a pre-resolved `FfiEntity` relationship handle and enables
    /// `BAM_OBSERVER_FLAG_PROPAGATE`.
    #[inline]
    pub fn with_relationship_handle(mut self, relationship: BamComponent) -> Self {
        if !relationship.is_valid() && self.first_error.is_none() {
            self.first_error = Some(FfiResult::INVALID_COMPONENT);
        }
        self.propagate_relationship = relationship;
        self.flags |= BAM_OBSERVER_FLAG_PROPAGATE;
        self
    }

    /// Resolves `R: GuestComponent` as the propagation relationship and enables
    /// `BAM_OBSERVER_FLAG_PROPAGATE`.
    pub fn with_relationship<R: GuestComponent>(mut self) -> Self {
        match self.app.resolve_component::<R>() {
            Ok(rel) => {
                self.propagate_relationship = rel;
                self.flags |= BAM_OBSERVER_FLAG_PROPAGATE;
            }
            Err(err) if self.first_error.is_none() => self.first_error = Some(err),
            Err(_) => {}
        }
        self
    }

    /// Appends pre-resolved watched component handles for lifecycle filtering.
    pub fn with_watched_components(mut self, components: &[BamComponent]) -> Self {
        for &comp in components {
            if !self.watched_components.contains(&comp) {
                self.watched_components.push(comp);
            }
        }
        self
    }

    /// Resolves `C: GuestComponent` and appends it to the watched component filter list.
    pub fn with_watched_component<C: GuestComponent>(mut self) -> Self {
        match self.app.resolve_component::<C>() {
            Ok(comp) => {
                if !self.watched_components.contains(&comp) {
                    self.watched_components.push(comp);
                }
            }
            Err(err) if self.first_error.is_none() => self.first_error = Some(err),
            Err(_) => {}
        }
        self
    }

    /// Registers the configured observer with a read-only `(target, &mut GuestCommands<'_>)`
    /// callback.
    pub fn build<F>(self, mut callback: F) -> Result<&'app mut GuestApp<'a>, FfiResult>
    where
        F: FnMut(FfiEntity, &mut GuestCommands<'_>) + Send + 'static,
    {
        if let Some(err) = self.first_error {
            return Err(err);
        }
        let status = self.app.load_ctx.register_lifecycle_observer(
            self.app.instance,
            &self.name,
            self.event_kind,
            &self.watched_components,
            self.target,
            self.flags,
            self.propagate_relationship,
            move |tgt, validated_cmds| {
                with_guest_commands(validated_cmds, |cmds| callback(tgt, cmds));
                FfiResult::OK
            },
        );
        if status.is_ok() {
            Ok(self.app)
        } else {
            Err(status)
        }
    }

    /// Registers a `Ready` observer with propagation control (`*propagate = false` stops upward
    /// `ChildOf` / custom-relationship bubbling).
    pub fn build_stoppable<F>(self, mut callback: F) -> Result<&'app mut GuestApp<'a>, FfiResult>
    where
        F: FnMut(FfiEntity, &mut bool, &mut GuestCommands<'_>) + Send + 'static,
    {
        if let Some(err) = self.first_error {
            return Err(err);
        }
        if !self.event_kind.is_ready() {
            return Err(FfiResult::INVALID_ARGUMENT);
        }
        let ready_handle = resolve_component(self.app.host(), Some(READY_CANONICAL_CSTR), None)
            .or_else(|_| resolve_component(self.app.host(), Some(READY_ALIAS_CSTR), None))?;
        self.app.observe_mut_handle::<FfiEntity, _>(
            &self.name,
            ready_handle,
            self.target,
            self.flags,
            self.propagate_relationship,
            move |tgt, _ev, prop, cmds| callback(tgt, prop, cmds),
        )?;
        Ok(self.app)
    }
}

#[cfg(test)]
mod tests {
    use crate::app::{GuestApp, GuestComponentOptions, ObserverBuilder};
    use crate::commands::GuestCommands;
    use crate::component::GuestComponentHooks;
    use bevy_abi_mod_sys::{BamComponent, BamEventKind, FfiChildOf, FfiEntity, FfiResult};

    #[allow(dead_code)]
    fn _verify_ready_observer_sdk_signatures<'a>(
        app: &'a mut GuestApp<'a>,
        cmds: &'a mut GuestCommands<'a>,
        entity: FfiEntity,
        rel_h: BamComponent,
    ) {
        let _ = app.observe_ready_global("ready_global", |_tgt, _cmds| {});
        let _ = app
            .observe_ready_with("ready_prop")
            .with_target(entity)
            .with_propagate(true)
            .build(|_tgt, _cmds| {});
        let _ = ObserverBuilder::ready(app, "ready_custom_rel")
            .with_relationship::<FfiChildOf>()
            .build_stoppable(|_tgt, prop, _cmds| {
                *prop = false;
            });
        let _ = ObserverBuilder::lifecycle(app, "life", BamEventKind::ADD)
            .with_watched_component::<FfiChildOf>()
            .with_relationship_handle(rel_h)
            .build(|_tgt, _cmds| {});
        let _: Result<FfiEntity, FfiResult> = cmds.observe_entity_ready(entity, |_tgt, _cmds| {});
        let _: Result<FfiEntity, FfiResult> =
            cmds.observe_entity_ready_with(entity, true, BamComponent::INVALID, |_tgt, _cmds| {});
        let _: Result<(), FfiResult> = cmds.trigger_ready(entity);
        let _: Result<(), FfiResult> = cmds.trigger_ready_propagated(entity);
        let _: Result<(), FfiResult> = cmds.trigger_ready_propagated_along::<FfiChildOf>(entity);
    }

    #[test]
    fn guest_hooks_and_options_builders_preserve_all_flags_hooks_and_requirements() {
        let _sig_check = _verify_ready_observer_sdk_signatures;
        let req_a = BamComponent::from_raw(7);
        let req_b = BamComponent::from_raw(8);
        let hooks = GuestComponentHooks::<u32>::new()
            .on_add(|_: FfiEntity, _| {})
            .on_insert(|_: FfiEntity, _| {})
            .on_discard(|_: FfiEntity, _| {})
            .on_remove(|_: FfiEntity, _| {})
            .on_despawn(|_: FfiEntity, _| {})
            .with_clone(true)
            .with_required_handle(req_a);
        assert!(hooks.inner.on_add.is_some());
        assert!(hooks.inner.on_insert.is_some());
        assert!(hooks.inner.on_discard.is_some());
        assert!(hooks.inner.on_remove.is_some());
        assert!(hooks.inner.on_despawn.is_some());
        assert!(hooks.inner.enable_clone);
        assert_eq!(hooks.inner.required_components, vec![req_a]);

        let opts = GuestComponentOptions::<u32>::new()
            .summary_tick()
            .clone()
            .require_handle(req_b)
            .hooks(hooks)
            .on_insert(|_: FfiEntity, _| {});
        let _ = opts;
    }
}
