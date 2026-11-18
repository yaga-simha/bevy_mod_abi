//! Guest-side `#![forbid(unsafe_code)]`, zero-Bevy helpers for bridged Bevy `0.20` states
//! (`GuestStateBridge`, deterministic `OnEnter`/`OnExit`/`OnTransition` schedule IDs,
//! `.run_if_in_state(...)`, `set_state_if_different`, and `DespawnOnExit`/`DespawnOnEnter`).

use bevy_abi_mod_guest_sys::{Host, ValidatedSystemRef, ValidatedWireVTable, resolve_component};
use bevy_abi_mod_sys::{
    AccessMode, BamChangeTicks, BamComponent, ComponentUuid, FfiEntity, FfiResult, ScheduleId,
};
use core::ffi::CStr;
use std::ffi::CString;

use crate::app::{QueryBuilder, SystemBuilder, SystemContext};
use crate::commands::GuestCommands;

/// Computes the deterministic `OnEnter(variant)` custom [`ScheduleId`] for `variant_index`
/// (`0..variant_count`) under `base`.
#[inline]
pub const fn on_enter_schedule(base: ScheduleId, variant_index: u32) -> ScheduleId {
    ScheduleId::custom(base.0 + variant_index)
}

/// Computes the deterministic `OnExit(variant)` custom [`ScheduleId`] for `variant_index`
/// (`0..variant_count`) under `base`.
#[inline]
pub const fn on_exit_schedule(
    base: ScheduleId,
    variant_count: u32,
    variant_index: u32,
) -> ScheduleId {
    ScheduleId::custom(base.0 + variant_count + variant_index)
}

/// Computes the deterministic `OnTransition { exited, entered }` custom [`ScheduleId`] under `base`.
#[inline]
pub const fn on_transition_schedule(
    base: ScheduleId,
    variant_count: u32,
    exited_index: u32,
    entered_index: u32,
) -> ScheduleId {
    ScheduleId::custom(base.0 + 2 * variant_count + exited_index * variant_count + entered_index)
}

/// Safe guest bridge for reading `State<S>` (`u32` discriminant) and writing `ModNextState<S>`
/// (`NextState::set_if_different`) on a host-exposed Bevy state machine.
#[derive(Clone, Copy)]
pub struct GuestStateBridge {
    state_slot: usize,
    state_handle: BamComponent,
    next_state_handle: BamComponent,
    state_vtable: ValidatedWireVTable<u32>,
    next_state_vtable: ValidatedWireVTable<u32>,
}

impl GuestStateBridge {
    /// Resolves the primary exposed state (`slot = 0`, alias `"State"` / `"ModNextState"`).
    #[inline]
    pub fn resolve_primary(host: &Host) -> Result<Self, FfiResult> {
        Self::resolve_slot(host, 0)
    }

    /// Resolves exposed state slot `slot` (`"bevy_state::State#<slot>"` and
    /// `"bevy_abi_mod::ModNextState#<slot>"`).
    pub fn resolve_slot(host: &Host, slot: usize) -> Result<Self, FfiResult> {
        let state_canonical = format!("bevy_state::State#{slot}");
        let next_canonical = format!("bevy_abi_mod::ModNextState#{slot}");
        let state_comp_cstr =
            CString::new(state_canonical.as_str()).map_err(|_| FfiResult::INVALID_ARGUMENT)?;
        let next_comp_cstr =
            CString::new(next_canonical.as_str()).map_err(|_| FfiResult::INVALID_ARGUMENT)?;
        let state_sym = CString::new(format!("bam.v0.{state_canonical}.vtable"))
            .map_err(|_| FfiResult::INVALID_ARGUMENT)?;
        let next_sym = CString::new(format!("bam.v0.{next_canonical}.vtable"))
            .map_err(|_| FfiResult::INVALID_ARGUMENT)?;

        let state_uuid = ComponentUuid::from_canonical_name(&state_canonical);
        let next_uuid = ComponentUuid::from_canonical_name(&next_canonical);

        let state_handle = resolve_component(host, Some(&state_comp_cstr), Some(state_uuid))?;
        let next_state_handle = resolve_component(host, Some(&next_comp_cstr), Some(next_uuid))?;

        let state_vtable = host
            .resolve_mapped_wire_vtable::<u32>(&state_sym, state_uuid)
            .or_else(|err| {
                if slot == 0 {
                    host.resolve_mapped_wire_vtable::<u32>(c"bam.v0.State.vtable", state_uuid)
                } else {
                    Err(err)
                }
            })?;
        let next_state_vtable = host
            .resolve_mapped_wire_vtable::<u32>(&next_sym, next_uuid)
            .or_else(|err| {
                if slot == 0 {
                    host.resolve_mapped_wire_vtable::<u32>(c"bam.v0.ModNextState.vtable", next_uuid)
                } else {
                    Err(err)
                }
            })?;

        Ok(Self {
            state_slot: slot,
            state_handle,
            next_state_handle,
            state_vtable,
            next_state_vtable,
        })
    }

    /// Returns the 0-based state slot index bound to this bridge.
    #[inline]
    pub const fn state_slot(&self) -> usize {
        self.state_slot
    }

    /// Returns the resolved [`BamComponent`] resource handle for `State<S>` in this slot.
    #[inline]
    pub const fn state_handle(&self) -> BamComponent {
        self.state_handle
    }

    /// Returns the resolved [`BamComponent`] resource handle for `ModNextState<S>` in this slot.
    #[inline]
    pub const fn next_state_handle(&self) -> BamComponent {
        self.next_state_handle
    }

    /// Reads the current `State<S>` `u32` discriminant declared in query `0` at `term_index`.
    #[inline]
    pub fn current(
        &self,
        ctx: &mut SystemContext<'_>,
        term_index: usize,
    ) -> Result<u32, FfiResult> {
        ctx.iter()
            .inner
            .read_mapped_wire_resource(term_index, &self.state_vtable)
    }

    /// Reads the current `State<S>` `u32` discriminant declared in `query_index` at `term_index`.
    #[inline]
    pub fn current_in_query(
        &self,
        ctx: &mut SystemContext<'_>,
        query_index: usize,
        term_index: usize,
    ) -> Result<u32, FfiResult> {
        let iter = ctx.query_mut(query_index).ok_or(FfiResult::OUT_OF_RANGE)?;
        iter.inner
            .read_mapped_wire_resource(term_index, &self.state_vtable)
    }

    /// Returns `true` if `State<S>` declared in query `0` at `term_index` equals `discriminant`.
    #[inline]
    pub fn is_in_state(
        &self,
        ctx: &mut SystemContext<'_>,
        term_index: usize,
        discriminant: u32,
    ) -> Result<bool, FfiResult> {
        Ok(self.current(ctx, term_index)? == discriminant)
    }

    /// Queues a state transition via `ModNextState<S>` (`NextState::set_if_different`) declared in
    /// query `0` at `next_term_index`.
    #[inline]
    pub fn set_state_if_different(
        &self,
        ctx: &mut SystemContext<'_>,
        next_term_index: usize,
        discriminant: u32,
    ) -> FfiResult {
        ctx.iter().inner.write_mapped_wire_resource(
            next_term_index,
            &self.next_state_vtable,
            &discriminant,
        )
    }

    /// Queues a state transition via `ModNextState<S>` declared in `query_index` at `next_term_index`.
    #[inline]
    pub fn set_state_if_different_in_query(
        &self,
        ctx: &mut SystemContext<'_>,
        query_index: usize,
        next_term_index: usize,
        discriminant: u32,
    ) -> FfiResult {
        let Some(iter) = ctx.query_mut(query_index) else {
            return FfiResult::OUT_OF_RANGE;
        };
        iter.inner.write_mapped_wire_resource(
            next_term_index,
            &self.next_state_vtable,
            &discriminant,
        )
    }

    /// Queues a state transition via `ModNextState<S>` handle on a [`ValidatedSystemRef`].
    #[inline]
    pub fn set_state_if_different_by_handle(
        &self,
        sys_ref: &mut ValidatedSystemRef<'_>,
        next_handle: BamComponent,
        discriminant: u32,
    ) -> Result<BamChangeTicks, FfiResult> {
        sys_ref.write_mapped_wire_resource_by_handle(
            next_handle,
            &self.next_state_vtable,
            &discriminant,
        )
    }

    /// Resolves the `DespawnOnExit` marker component handle for `variant_slot` (`0..32`).
    pub fn resolve_despawn_on_exit(
        &self,
        host: &Host,
        variant_slot: usize,
    ) -> Result<BamComponent, FfiResult> {
        let name = format!(
            "bevy_state::DespawnOnExit#{}::{variant_slot}",
            self.state_slot
        );
        let cstr = CString::new(name.as_str()).map_err(|_| FfiResult::INVALID_ARGUMENT)?;
        let uuid = ComponentUuid::from_canonical_name(&name);
        bevy_abi_mod_guest_sys::resolve_component(host, Some(&cstr), Some(uuid))
    }

    /// Resolves the `DespawnOnEnter` marker component handle for `variant_slot` (`0..32`).
    pub fn resolve_despawn_on_enter(
        &self,
        host: &Host,
        variant_slot: usize,
    ) -> Result<BamComponent, FfiResult> {
        let name = format!(
            "bevy_state::DespawnOnEnter#{}::{variant_slot}",
            self.state_slot
        );
        let cstr = CString::new(name.as_str()).map_err(|_| FfiResult::INVALID_ARGUMENT)?;
        let uuid = ComponentUuid::from_canonical_name(&name);
        bevy_abi_mod_guest_sys::resolve_component(host, Some(&cstr), Some(uuid))
    }
}

/// Extension trait on [`SystemBuilder`] attaching a host-exposed `in_state` system set
/// (`"host::state.<StateName>.<Variant|disc>"`).
pub trait GuestStateSystemBuilderExt<'b> {
    /// Schedules this system inside `host_state_set` (which runs on the host iff `in_state(S::V)`
    /// holds).
    fn run_if_in_state(self, host_state_set: &'b CStr) -> Self;
}

impl<'b, 'a> GuestStateSystemBuilderExt<'b> for SystemBuilder<'b, 'a> {
    #[inline]
    fn run_if_in_state(self, host_state_set: &'b CStr) -> Self {
        self.in_set(host_state_set)
    }
}

/// Extension trait on [`GuestCommands`] for attaching `DespawnOnExit` / `DespawnOnEnter`
/// state-scoped lifetime markers to spawned entities.
pub trait GuestCommandsStateExt {
    /// Attaches a pre-resolved `DespawnOnExit` marker component (`exit_marker`) onto `entity`.
    fn despawn_on_exit(&mut self, entity: FfiEntity, exit_marker: BamComponent) -> FfiResult;

    /// Attaches a pre-resolved `DespawnOnEnter` marker component (`enter_marker`) onto `entity`.
    fn despawn_on_enter(&mut self, entity: FfiEntity, enter_marker: BamComponent) -> FfiResult;
}

impl GuestCommandsStateExt for GuestCommands<'_> {
    #[inline]
    fn despawn_on_exit(&mut self, entity: FfiEntity, exit_marker: BamComponent) -> FfiResult {
        self.insert_default(entity, exit_marker)
    }

    #[inline]
    fn despawn_on_enter(&mut self, entity: FfiEntity, enter_marker: BamComponent) -> FfiResult {
        self.insert_default(entity, enter_marker)
    }
}

/// Extension trait on [`QueryBuilder`] adding bridged `State<S>` and `ModNextState<S>` resource
/// terms.
pub trait GuestStateQueryBuilderExt {
    /// Appends a shared resource read (`Res<State<S>>`) term using `bridge`.
    fn read_state(self, bridge: &GuestStateBridge) -> Self;
    /// Appends an exclusive resource write (`ResMut<ModNextState<S>>`) term using `bridge`.
    fn write_next_state(self, bridge: &GuestStateBridge) -> Self;
    /// Appends a shared resource read (`Res<State<S>>`) term for primary state slot `0`.
    fn read_primary_state(self) -> Self;
    /// Appends an exclusive resource write (`ResMut<ModNextState<S>>`) term for primary slot `0`.
    fn write_primary_next_state(self) -> Self;
}

impl GuestStateQueryBuilderExt for QueryBuilder<'_> {
    #[inline]
    fn read_state(self, bridge: &GuestStateBridge) -> Self {
        self.resource_read_handle(bridge.state_handle())
    }

    #[inline]
    fn write_next_state(self, bridge: &GuestStateBridge) -> Self {
        self.resource_write_handle(bridge.next_state_handle())
    }

    #[inline]
    fn read_primary_state(self) -> Self {
        self.term_named(
            c"bevy_state::State#0",
            Some(ComponentUuid::from_canonical_name("bevy_state::State#0")),
            AccessMode::RESOURCE_READ,
        )
    }

    #[inline]
    fn write_primary_next_state(self) -> Self {
        self.term_named(
            c"bevy_abi_mod::ModNextState#0",
            Some(ComponentUuid::from_canonical_name(
                "bevy_abi_mod::ModNextState#0",
            )),
            AccessMode::RESOURCE_WRITE,
        )
    }
}

/// Bundled state bridge + pre-resolved `DespawnOnExit` / `DespawnOnEnter` marker handles
/// (`0..variant_count`) for one host-exposed state slot.
#[derive(Clone)]
pub struct GuestStateHandles {
    /// Resolved state VTable and resource handle bridge.
    pub bridge: GuestStateBridge,
    /// `DespawnOnExit(variant)` marker handles indexed by `variant_slot`.
    pub despawn_on_exit: Vec<BamComponent>,
    /// `DespawnOnEnter(variant)` marker handles indexed by `variant_slot`.
    pub despawn_on_enter: Vec<BamComponent>,
}

impl GuestStateHandles {
    /// Resolves primary state slot `0` and all `variant_count` `DespawnOnExit` /
    /// `DespawnOnEnter` handles.
    pub fn resolve_primary(host: &Host, variant_count: usize) -> Result<Self, FfiResult> {
        Self::resolve_slot(host, 0, variant_count)
    }

    /// Resolves `slot` and all `variant_count` `DespawnOnExit` / `DespawnOnEnter` handles.
    pub fn resolve_slot(host: &Host, slot: usize, variant_count: usize) -> Result<Self, FfiResult> {
        let bridge = GuestStateBridge::resolve_slot(host, slot)?;
        let mut despawn_on_exit = Vec::with_capacity(variant_count);
        let mut despawn_on_enter = Vec::with_capacity(variant_count);
        for idx in 0..variant_count {
            despawn_on_exit.push(bridge.resolve_despawn_on_exit(host, idx)?);
            despawn_on_enter.push(bridge.resolve_despawn_on_enter(host, idx)?);
        }
        Ok(Self {
            bridge,
            despawn_on_exit,
            despawn_on_enter,
        })
    }

    /// Returns the `DespawnOnExit` handle for `variant_slot`, if present.
    #[inline]
    pub fn exit_marker(&self, variant_slot: usize) -> Option<BamComponent> {
        self.despawn_on_exit.get(variant_slot).copied()
    }

    /// Returns the `DespawnOnEnter` handle for `variant_slot`, if present.
    #[inline]
    pub fn enter_marker(&self, variant_slot: usize) -> Option<BamComponent> {
        self.despawn_on_enter.get(variant_slot).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_state_schedule_ids_match_host_layout() {
        let base = ScheduleId::custom(2000);
        assert_eq!(on_enter_schedule(base, 0), ScheduleId::custom(2000));
        assert_eq!(on_enter_schedule(base, 1), ScheduleId::custom(2001));
        assert_eq!(on_exit_schedule(base, 3, 0), ScheduleId::custom(2003));
        assert_eq!(on_exit_schedule(base, 3, 2), ScheduleId::custom(2005));
        assert_eq!(
            on_transition_schedule(base, 3, 0, 1),
            ScheduleId::custom(2007)
        );
        assert_eq!(
            on_transition_schedule(base, 3, 2, 1),
            ScheduleId::custom(2013)
        );
    }
}
