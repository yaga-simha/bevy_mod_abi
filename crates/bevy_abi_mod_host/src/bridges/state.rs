//! `bevy_state` (`0.20`) bridges for `State<S>`, `ModNextState<S>` (`NextState::set_if_different`),
//! deterministic `OnEnter`/`OnExit`/`OnTransition` schedules, `in_state` visible system sets,
//! and per-variant `DespawnOnExit` / `DespawnOnEnter` default-insertable marker bridges.

use bevy_abi_mod_host_sys::{
    ComponentCollisionError, HostWireBridge, ModRegistry, ModScheduleMap, ModVisibleSets,
};
use bevy_abi_mod_sys::{BamComponent, FfiResult, ScheduleId};
use bevy_app::{App, FixedUpdate, PostUpdate, PreUpdate, Update};
use bevy_ecs::prelude::{Component, Entity, Res, ResMut, Resource};
use bevy_ecs::schedule::{IntoScheduleConfigs, ScheduleLabel, Schedules, SystemSet};
use bevy_ecs::world::World;
use bevy_platform::collections::{HashMap, HashSet};
use bevy_state::app::{AppExtStates, StatesPlugin};
use bevy_state::condition::in_state;
use bevy_state::state::{FreelyMutableState, NextState, OnEnter, OnExit, OnTransition, State};
use bevy_state::state_scoped::{DespawnOnEnter, DespawnOnExit};
use core::any::{Any, TypeId};
use core::fmt;
use core::marker::PhantomData;
use std::sync::{Arc, OnceLock, RwLock};

/// Maximum number of exposed Bevy state enums supported per `World`.
pub const MAX_EXPOSED_MOD_STATES: usize = 8;

/// Maximum number of variants supported per exposed `ModStateSpec<S>`.
pub const MAX_MOD_STATE_VARIANTS: usize = 32;

/// Errors returned by [`ModStateExt::expose_mod_state`].
#[derive(Debug)]
pub enum ModStateError {
    /// `base_schedule_id` must be a custom schedule (`>= ScheduleId::CUSTOM_BASE` = `1024`).
    InvalidBaseScheduleId(ScheduleId),
    /// `variants` slice was empty.
    EmptyVariants,
    /// `variants` exceeded [`MAX_MOD_STATE_VARIANTS`].
    TooManyVariants {
        /// Number of variants passed.
        count: usize,
        /// Maximum supported (`32`).
        max: usize,
    },
    /// More than [`MAX_EXPOSED_MOD_STATES`] distinct state enums were exposed in one `World`.
    TooManyExposedStates {
        /// Maximum supported (`8`).
        max: usize,
    },
    /// Duplicate `u32` discriminant in `variants`.
    DuplicateDiscriminant(u32),
    /// Empty or duplicate variant name in `variants`.
    InvalidVariantName(String),
    /// Deterministic `ScheduleId` arithmetic overflowed `u32::MAX`.
    ScheduleIdOverflow,
    /// Component or resource registration collided in [`ModRegistry`].
    ComponentCollision(ComponentCollisionError),
}

impl fmt::Display for ModStateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidBaseScheduleId(id) => {
                write!(f, "base schedule id {:?} must be >= 1024", id)
            }
            Self::EmptyVariants => write!(f, "state variants slice must not be empty"),
            Self::TooManyVariants { count, max } => {
                write!(f, "too many state variants ({count} > max {max})")
            }
            Self::TooManyExposedStates { max } => {
                write!(f, "exceeded maximum exposed state slots ({max})")
            }
            Self::DuplicateDiscriminant(d) => write!(f, "duplicate state discriminant {d}"),
            Self::InvalidVariantName(name) => {
                write!(f, "invalid or duplicate variant name '{name}'")
            }
            Self::ScheduleIdOverflow => write!(f, "state ScheduleId range overflowed u32::MAX"),
            Self::ComponentCollision(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for ModStateError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::ComponentCollision(err) => Some(err),
            _ => None,
        }
    }
}

impl From<ComponentCollisionError> for ModStateError {
    fn from(err: ComponentCollisionError) -> Self {
        Self::ComponentCollision(err)
    }
}

type StateVariantList<S> = Arc<Vec<(u32, String, S)>>;

static STATE_VARIANT_TABLES: OnceLock<RwLock<HashMap<TypeId, Arc<dyn Any + Send + Sync>>>> =
    OnceLock::new();

fn state_tables() -> &'static RwLock<HashMap<TypeId, Arc<dyn Any + Send + Sync>>> {
    STATE_VARIANT_TABLES.get_or_init(|| RwLock::new(HashMap::default()))
}

fn store_state_variants<S: FreelyMutableState + Copy>(variants: Vec<(u32, String, S)>) {
    let arc: StateVariantList<S> = Arc::new(variants);
    if let Ok(mut map) = state_tables().write() {
        map.insert(TypeId::of::<S>(), arc);
    }
}

fn lookup_discriminant_for<S: FreelyMutableState + Copy>(value: &S) -> Option<u32> {
    let map = state_tables().read().ok()?;
    let entry = map.get(&TypeId::of::<S>())?;
    let list = entry.downcast_ref::<Vec<(u32, String, S)>>()?;
    list.iter()
        .find(|(_, _, variant)| variant == value)
        .map(|(disc, _, _)| *disc)
}

fn lookup_variant_by_slot<S: FreelyMutableState + Copy>(slot: usize) -> Option<S> {
    let map = state_tables().read().ok()?;
    let entry = map.get(&TypeId::of::<S>())?;
    let list = entry.downcast_ref::<Vec<(u32, String, S)>>()?;
    list.get(slot).map(|(_, _, variant)| *variant)
}

#[derive(Resource, Default)]
struct ExposedStateSlotAllocator {
    assigned: HashMap<TypeId, usize>,
    next_slot: usize,
}

impl ExposedStateSlotAllocator {
    fn allocate<S: 'static>(&mut self) -> Result<usize, ModStateError> {
        if let Some(&slot) = self.assigned.get(&TypeId::of::<S>()) {
            return Ok(slot);
        }
        if self.next_slot >= MAX_EXPOSED_MOD_STATES {
            return Err(ModStateError::TooManyExposedStates {
                max: MAX_EXPOSED_MOD_STATES,
            });
        }
        let slot = self.next_slot;
        self.next_slot += 1;
        self.assigned.insert(TypeId::of::<S>(), slot);
        Ok(slot)
    }
}

/// Host resource bridging guest `u32` state transition requests into Bevy's
/// [`NextState::<S>::set_if_different`].
#[derive(Resource, Debug, Clone)]
pub struct ModNextState<S: FreelyMutableState + Copy> {
    current: u32,
    pending: Option<u32>,
    variants: Vec<(u32, S)>,
}

impl<S: FreelyMutableState + Copy> ModNextState<S> {
    fn new(initial: u32, variants: Vec<(u32, S)>) -> Self {
        Self {
            current: initial,
            pending: None,
            variants,
        }
    }

    /// Returns the most recently synced state discriminant.
    #[inline]
    pub const fn current_discriminant(&self) -> u32 {
        self.current
    }

    /// Returns the pending requested discriminant, if a mod queued a transition this tick.
    #[inline]
    pub const fn pending_discriminant(&self) -> Option<u32> {
        self.pending
    }
}

fn sync_mod_next_state<S: FreelyMutableState + Copy>(
    state: Res<State<S>>,
    mut mod_next: ResMut<ModNextState<S>>,
    mut next_state: ResMut<NextState<S>>,
) {
    if let Some(current) = mod_next
        .variants
        .iter()
        .find(|(_, v)| v == state.get())
        .map(|(d, _)| *d)
    {
        mod_next.current = current;
    }
    if let Some(disc) = mod_next.pending.take()
        && let Some((_, variant)) = mod_next.variants.iter().find(|(d, _)| *d == disc)
    {
        next_state.set_if_different(*variant);
    }
}

#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct ModInStateSet<S: FreelyMutableState + Copy> {
    discriminant: u32,
    _marker: PhantomData<fn() -> S>,
}

impl<S: FreelyMutableState + Copy> ModInStateSet<S> {
    const fn new(discriminant: u32) -> Self {
        Self {
            discriminant,
            _marker: PhantomData,
        }
    }
}

#[derive(Component, Default)]
struct ModDespawnOnExitSlot<
    S: FreelyMutableState + Copy,
    const STATE_SLOT: usize,
    const VAR_SLOT: usize,
>(PhantomData<fn() -> S>);

#[derive(Component, Default)]
struct ModDespawnOnEnterSlot<
    S: FreelyMutableState + Copy,
    const STATE_SLOT: usize,
    const VAR_SLOT: usize,
>(PhantomData<fn() -> S>);

fn insert_despawn_on_exit_slot<S: FreelyMutableState + Copy, const VAR_SLOT: usize>(
    world: &mut World,
    entity: Entity,
) {
    if let Some(variant) = lookup_variant_by_slot::<S>(VAR_SLOT)
        && let Ok(mut e) = world.get_entity_mut(entity)
    {
        e.insert(DespawnOnExit(variant));
    }
}

fn insert_despawn_on_enter_slot<S: FreelyMutableState + Copy, const VAR_SLOT: usize>(
    world: &mut World,
    entity: Entity,
) {
    if let Some(variant) = lookup_variant_by_slot::<S>(VAR_SLOT)
        && let Ok(mut e) = world.get_entity_mut(entity)
    {
        e.insert(DespawnOnEnter(variant));
    }
}

struct StateWireBridge<S, const STATE_SLOT: usize>(PhantomData<fn() -> S>);
struct NextStateWireBridge<S, const STATE_SLOT: usize>(PhantomData<fn() -> S>);
struct ExitMarkerBridge<S, const STATE_SLOT: usize, const VAR_SLOT: usize>(PhantomData<fn() -> S>);
struct EnterMarkerBridge<S, const STATE_SLOT: usize, const VAR_SLOT: usize>(PhantomData<fn() -> S>);

macro_rules! impl_state_slot_bridges {
    ($($slot:literal => ($state_name:literal, $state_alias:expr, $next_name:literal, $next_alias:expr)),+ $(,)?) => {
        $(
            impl<S: FreelyMutableState + Copy> HostWireBridge<State<S>> for StateWireBridge<S, $slot> {
                type Wire = u32;
                const CANONICAL_NAME: &'static str = $state_name;
                const ALIAS: Option<&'static str> = $state_alias;
                const SUPPORTS_SET: bool = false;

                fn to_wire(value: &State<S>) -> u32 {
                    lookup_discriminant_for(value.get()).unwrap_or(u32::MAX)
                }
            }

            impl<S: FreelyMutableState + Copy> HostWireBridge<ModNextState<S>>
                for NextStateWireBridge<S, $slot>
            {
                type Wire = u32;
                const CANONICAL_NAME: &'static str = $next_name;
                const ALIAS: Option<&'static str> = $next_alias;
                const SUPPORTS_SET: bool = true;

                fn to_wire(value: &ModNextState<S>) -> u32 {
                    value.pending.unwrap_or(value.current)
                }

                fn apply_wire(value: &mut ModNextState<S>, wire: u32) -> FfiResult {
                    if value.variants.iter().any(|(d, _)| *d == wire) {
                        value.pending = Some(wire);
                        FfiResult::OK
                    } else {
                        FfiResult::INVALID_ARGUMENT
                    }
                }
            }
        )+

        fn register_state_resources_for_slot<S: FreelyMutableState + Copy>(
            world: &mut World,
            state_slot: usize,
        ) -> Result<(BamComponent, &'static str, BamComponent, &'static str), ComponentCollisionError> {
            let state_comp_id = world.register_component::<State<S>>();
            let next_comp_id = world.register_component::<ModNextState<S>>();
            let mut reg = world.resource_mut::<ModRegistry>();
            match state_slot {
                $(
                    $slot => {
                        let sh = reg.register_bridged_resource::<State<S>, StateWireBridge<S, $slot>>(
                            state_comp_id,
                        )?;
                        let nh = reg.register_bridged_resource::<ModNextState<S>, NextStateWireBridge<S, $slot>>(
                            next_comp_id,
                        )?;
                        Ok((sh, $state_name, nh, $next_name))
                    }
                )+
                _ => unreachable!("state_slot < MAX_EXPOSED_MOD_STATES"),
            }
        }
    };
}

impl_state_slot_bridges!(
    0 => ("bevy_state::State#0", Some("State"), "bevy_abi_mod::ModNextState#0", Some("ModNextState")),
    1 => ("bevy_state::State#1", None, "bevy_abi_mod::ModNextState#1", None),
    2 => ("bevy_state::State#2", None, "bevy_abi_mod::ModNextState#2", None),
    3 => ("bevy_state::State#3", None, "bevy_abi_mod::ModNextState#3", None),
    4 => ("bevy_state::State#4", None, "bevy_abi_mod::ModNextState#4", None),
    5 => ("bevy_state::State#5", None, "bevy_abi_mod::ModNextState#5", None),
    6 => ("bevy_state::State#6", None, "bevy_abi_mod::ModNextState#6", None),
    7 => ("bevy_state::State#7", None, "bevy_abi_mod::ModNextState#7", None),
);

const EXIT_MARKER_NAMES: [[&str; MAX_MOD_STATE_VARIANTS]; MAX_EXPOSED_MOD_STATES] = [
    [
        "bevy_state::DespawnOnExit#0::0",
        "bevy_state::DespawnOnExit#0::1",
        "bevy_state::DespawnOnExit#0::2",
        "bevy_state::DespawnOnExit#0::3",
        "bevy_state::DespawnOnExit#0::4",
        "bevy_state::DespawnOnExit#0::5",
        "bevy_state::DespawnOnExit#0::6",
        "bevy_state::DespawnOnExit#0::7",
        "bevy_state::DespawnOnExit#0::8",
        "bevy_state::DespawnOnExit#0::9",
        "bevy_state::DespawnOnExit#0::10",
        "bevy_state::DespawnOnExit#0::11",
        "bevy_state::DespawnOnExit#0::12",
        "bevy_state::DespawnOnExit#0::13",
        "bevy_state::DespawnOnExit#0::14",
        "bevy_state::DespawnOnExit#0::15",
        "bevy_state::DespawnOnExit#0::16",
        "bevy_state::DespawnOnExit#0::17",
        "bevy_state::DespawnOnExit#0::18",
        "bevy_state::DespawnOnExit#0::19",
        "bevy_state::DespawnOnExit#0::20",
        "bevy_state::DespawnOnExit#0::21",
        "bevy_state::DespawnOnExit#0::22",
        "bevy_state::DespawnOnExit#0::23",
        "bevy_state::DespawnOnExit#0::24",
        "bevy_state::DespawnOnExit#0::25",
        "bevy_state::DespawnOnExit#0::26",
        "bevy_state::DespawnOnExit#0::27",
        "bevy_state::DespawnOnExit#0::28",
        "bevy_state::DespawnOnExit#0::29",
        "bevy_state::DespawnOnExit#0::30",
        "bevy_state::DespawnOnExit#0::31",
    ],
    [
        "bevy_state::DespawnOnExit#1::0",
        "bevy_state::DespawnOnExit#1::1",
        "bevy_state::DespawnOnExit#1::2",
        "bevy_state::DespawnOnExit#1::3",
        "bevy_state::DespawnOnExit#1::4",
        "bevy_state::DespawnOnExit#1::5",
        "bevy_state::DespawnOnExit#1::6",
        "bevy_state::DespawnOnExit#1::7",
        "bevy_state::DespawnOnExit#1::8",
        "bevy_state::DespawnOnExit#1::9",
        "bevy_state::DespawnOnExit#1::10",
        "bevy_state::DespawnOnExit#1::11",
        "bevy_state::DespawnOnExit#1::12",
        "bevy_state::DespawnOnExit#1::13",
        "bevy_state::DespawnOnExit#1::14",
        "bevy_state::DespawnOnExit#1::15",
        "bevy_state::DespawnOnExit#1::16",
        "bevy_state::DespawnOnExit#1::17",
        "bevy_state::DespawnOnExit#1::18",
        "bevy_state::DespawnOnExit#1::19",
        "bevy_state::DespawnOnExit#1::20",
        "bevy_state::DespawnOnExit#1::21",
        "bevy_state::DespawnOnExit#1::22",
        "bevy_state::DespawnOnExit#1::23",
        "bevy_state::DespawnOnExit#1::24",
        "bevy_state::DespawnOnExit#1::25",
        "bevy_state::DespawnOnExit#1::26",
        "bevy_state::DespawnOnExit#1::27",
        "bevy_state::DespawnOnExit#1::28",
        "bevy_state::DespawnOnExit#1::29",
        "bevy_state::DespawnOnExit#1::30",
        "bevy_state::DespawnOnExit#1::31",
    ],
    [
        "bevy_state::DespawnOnExit#2::0",
        "bevy_state::DespawnOnExit#2::1",
        "bevy_state::DespawnOnExit#2::2",
        "bevy_state::DespawnOnExit#2::3",
        "bevy_state::DespawnOnExit#2::4",
        "bevy_state::DespawnOnExit#2::5",
        "bevy_state::DespawnOnExit#2::6",
        "bevy_state::DespawnOnExit#2::7",
        "bevy_state::DespawnOnExit#2::8",
        "bevy_state::DespawnOnExit#2::9",
        "bevy_state::DespawnOnExit#2::10",
        "bevy_state::DespawnOnExit#2::11",
        "bevy_state::DespawnOnExit#2::12",
        "bevy_state::DespawnOnExit#2::13",
        "bevy_state::DespawnOnExit#2::14",
        "bevy_state::DespawnOnExit#2::15",
        "bevy_state::DespawnOnExit#2::16",
        "bevy_state::DespawnOnExit#2::17",
        "bevy_state::DespawnOnExit#2::18",
        "bevy_state::DespawnOnExit#2::19",
        "bevy_state::DespawnOnExit#2::20",
        "bevy_state::DespawnOnExit#2::21",
        "bevy_state::DespawnOnExit#2::22",
        "bevy_state::DespawnOnExit#2::23",
        "bevy_state::DespawnOnExit#2::24",
        "bevy_state::DespawnOnExit#2::25",
        "bevy_state::DespawnOnExit#2::26",
        "bevy_state::DespawnOnExit#2::27",
        "bevy_state::DespawnOnExit#2::28",
        "bevy_state::DespawnOnExit#2::29",
        "bevy_state::DespawnOnExit#2::30",
        "bevy_state::DespawnOnExit#2::31",
    ],
    [
        "bevy_state::DespawnOnExit#3::0",
        "bevy_state::DespawnOnExit#3::1",
        "bevy_state::DespawnOnExit#3::2",
        "bevy_state::DespawnOnExit#3::3",
        "bevy_state::DespawnOnExit#3::4",
        "bevy_state::DespawnOnExit#3::5",
        "bevy_state::DespawnOnExit#3::6",
        "bevy_state::DespawnOnExit#3::7",
        "bevy_state::DespawnOnExit#3::8",
        "bevy_state::DespawnOnExit#3::9",
        "bevy_state::DespawnOnExit#3::10",
        "bevy_state::DespawnOnExit#3::11",
        "bevy_state::DespawnOnExit#3::12",
        "bevy_state::DespawnOnExit#3::13",
        "bevy_state::DespawnOnExit#3::14",
        "bevy_state::DespawnOnExit#3::15",
        "bevy_state::DespawnOnExit#3::16",
        "bevy_state::DespawnOnExit#3::17",
        "bevy_state::DespawnOnExit#3::18",
        "bevy_state::DespawnOnExit#3::19",
        "bevy_state::DespawnOnExit#3::20",
        "bevy_state::DespawnOnExit#3::21",
        "bevy_state::DespawnOnExit#3::22",
        "bevy_state::DespawnOnExit#3::23",
        "bevy_state::DespawnOnExit#3::24",
        "bevy_state::DespawnOnExit#3::25",
        "bevy_state::DespawnOnExit#3::26",
        "bevy_state::DespawnOnExit#3::27",
        "bevy_state::DespawnOnExit#3::28",
        "bevy_state::DespawnOnExit#3::29",
        "bevy_state::DespawnOnExit#3::30",
        "bevy_state::DespawnOnExit#3::31",
    ],
    [
        "bevy_state::DespawnOnExit#4::0",
        "bevy_state::DespawnOnExit#4::1",
        "bevy_state::DespawnOnExit#4::2",
        "bevy_state::DespawnOnExit#4::3",
        "bevy_state::DespawnOnExit#4::4",
        "bevy_state::DespawnOnExit#4::5",
        "bevy_state::DespawnOnExit#4::6",
        "bevy_state::DespawnOnExit#4::7",
        "bevy_state::DespawnOnExit#4::8",
        "bevy_state::DespawnOnExit#4::9",
        "bevy_state::DespawnOnExit#4::10",
        "bevy_state::DespawnOnExit#4::11",
        "bevy_state::DespawnOnExit#4::12",
        "bevy_state::DespawnOnExit#4::13",
        "bevy_state::DespawnOnExit#4::14",
        "bevy_state::DespawnOnExit#4::15",
        "bevy_state::DespawnOnExit#4::16",
        "bevy_state::DespawnOnExit#4::17",
        "bevy_state::DespawnOnExit#4::18",
        "bevy_state::DespawnOnExit#4::19",
        "bevy_state::DespawnOnExit#4::20",
        "bevy_state::DespawnOnExit#4::21",
        "bevy_state::DespawnOnExit#4::22",
        "bevy_state::DespawnOnExit#4::23",
        "bevy_state::DespawnOnExit#4::24",
        "bevy_state::DespawnOnExit#4::25",
        "bevy_state::DespawnOnExit#4::26",
        "bevy_state::DespawnOnExit#4::27",
        "bevy_state::DespawnOnExit#4::28",
        "bevy_state::DespawnOnExit#4::29",
        "bevy_state::DespawnOnExit#4::30",
        "bevy_state::DespawnOnExit#4::31",
    ],
    [
        "bevy_state::DespawnOnExit#5::0",
        "bevy_state::DespawnOnExit#5::1",
        "bevy_state::DespawnOnExit#5::2",
        "bevy_state::DespawnOnExit#5::3",
        "bevy_state::DespawnOnExit#5::4",
        "bevy_state::DespawnOnExit#5::5",
        "bevy_state::DespawnOnExit#5::6",
        "bevy_state::DespawnOnExit#5::7",
        "bevy_state::DespawnOnExit#5::8",
        "bevy_state::DespawnOnExit#5::9",
        "bevy_state::DespawnOnExit#5::10",
        "bevy_state::DespawnOnExit#5::11",
        "bevy_state::DespawnOnExit#5::12",
        "bevy_state::DespawnOnExit#5::13",
        "bevy_state::DespawnOnExit#5::14",
        "bevy_state::DespawnOnExit#5::15",
        "bevy_state::DespawnOnExit#5::16",
        "bevy_state::DespawnOnExit#5::17",
        "bevy_state::DespawnOnExit#5::18",
        "bevy_state::DespawnOnExit#5::19",
        "bevy_state::DespawnOnExit#5::20",
        "bevy_state::DespawnOnExit#5::21",
        "bevy_state::DespawnOnExit#5::22",
        "bevy_state::DespawnOnExit#5::23",
        "bevy_state::DespawnOnExit#5::24",
        "bevy_state::DespawnOnExit#5::25",
        "bevy_state::DespawnOnExit#5::26",
        "bevy_state::DespawnOnExit#5::27",
        "bevy_state::DespawnOnExit#5::28",
        "bevy_state::DespawnOnExit#5::29",
        "bevy_state::DespawnOnExit#5::30",
        "bevy_state::DespawnOnExit#5::31",
    ],
    [
        "bevy_state::DespawnOnExit#6::0",
        "bevy_state::DespawnOnExit#6::1",
        "bevy_state::DespawnOnExit#6::2",
        "bevy_state::DespawnOnExit#6::3",
        "bevy_state::DespawnOnExit#6::4",
        "bevy_state::DespawnOnExit#6::5",
        "bevy_state::DespawnOnExit#6::6",
        "bevy_state::DespawnOnExit#6::7",
        "bevy_state::DespawnOnExit#6::8",
        "bevy_state::DespawnOnExit#6::9",
        "bevy_state::DespawnOnExit#6::10",
        "bevy_state::DespawnOnExit#6::11",
        "bevy_state::DespawnOnExit#6::12",
        "bevy_state::DespawnOnExit#6::13",
        "bevy_state::DespawnOnExit#6::14",
        "bevy_state::DespawnOnExit#6::15",
        "bevy_state::DespawnOnExit#6::16",
        "bevy_state::DespawnOnExit#6::17",
        "bevy_state::DespawnOnExit#6::18",
        "bevy_state::DespawnOnExit#6::19",
        "bevy_state::DespawnOnExit#6::20",
        "bevy_state::DespawnOnExit#6::21",
        "bevy_state::DespawnOnExit#6::22",
        "bevy_state::DespawnOnExit#6::23",
        "bevy_state::DespawnOnExit#6::24",
        "bevy_state::DespawnOnExit#6::25",
        "bevy_state::DespawnOnExit#6::26",
        "bevy_state::DespawnOnExit#6::27",
        "bevy_state::DespawnOnExit#6::28",
        "bevy_state::DespawnOnExit#6::29",
        "bevy_state::DespawnOnExit#6::30",
        "bevy_state::DespawnOnExit#6::31",
    ],
    [
        "bevy_state::DespawnOnExit#7::0",
        "bevy_state::DespawnOnExit#7::1",
        "bevy_state::DespawnOnExit#7::2",
        "bevy_state::DespawnOnExit#7::3",
        "bevy_state::DespawnOnExit#7::4",
        "bevy_state::DespawnOnExit#7::5",
        "bevy_state::DespawnOnExit#7::6",
        "bevy_state::DespawnOnExit#7::7",
        "bevy_state::DespawnOnExit#7::8",
        "bevy_state::DespawnOnExit#7::9",
        "bevy_state::DespawnOnExit#7::10",
        "bevy_state::DespawnOnExit#7::11",
        "bevy_state::DespawnOnExit#7::12",
        "bevy_state::DespawnOnExit#7::13",
        "bevy_state::DespawnOnExit#7::14",
        "bevy_state::DespawnOnExit#7::15",
        "bevy_state::DespawnOnExit#7::16",
        "bevy_state::DespawnOnExit#7::17",
        "bevy_state::DespawnOnExit#7::18",
        "bevy_state::DespawnOnExit#7::19",
        "bevy_state::DespawnOnExit#7::20",
        "bevy_state::DespawnOnExit#7::21",
        "bevy_state::DespawnOnExit#7::22",
        "bevy_state::DespawnOnExit#7::23",
        "bevy_state::DespawnOnExit#7::24",
        "bevy_state::DespawnOnExit#7::25",
        "bevy_state::DespawnOnExit#7::26",
        "bevy_state::DespawnOnExit#7::27",
        "bevy_state::DespawnOnExit#7::28",
        "bevy_state::DespawnOnExit#7::29",
        "bevy_state::DespawnOnExit#7::30",
        "bevy_state::DespawnOnExit#7::31",
    ],
];

const ENTER_MARKER_NAMES: [[&str; MAX_MOD_STATE_VARIANTS]; MAX_EXPOSED_MOD_STATES] = [
    [
        "bevy_state::DespawnOnEnter#0::0",
        "bevy_state::DespawnOnEnter#0::1",
        "bevy_state::DespawnOnEnter#0::2",
        "bevy_state::DespawnOnEnter#0::3",
        "bevy_state::DespawnOnEnter#0::4",
        "bevy_state::DespawnOnEnter#0::5",
        "bevy_state::DespawnOnEnter#0::6",
        "bevy_state::DespawnOnEnter#0::7",
        "bevy_state::DespawnOnEnter#0::8",
        "bevy_state::DespawnOnEnter#0::9",
        "bevy_state::DespawnOnEnter#0::10",
        "bevy_state::DespawnOnEnter#0::11",
        "bevy_state::DespawnOnEnter#0::12",
        "bevy_state::DespawnOnEnter#0::13",
        "bevy_state::DespawnOnEnter#0::14",
        "bevy_state::DespawnOnEnter#0::15",
        "bevy_state::DespawnOnEnter#0::16",
        "bevy_state::DespawnOnEnter#0::17",
        "bevy_state::DespawnOnEnter#0::18",
        "bevy_state::DespawnOnEnter#0::19",
        "bevy_state::DespawnOnEnter#0::20",
        "bevy_state::DespawnOnEnter#0::21",
        "bevy_state::DespawnOnEnter#0::22",
        "bevy_state::DespawnOnEnter#0::23",
        "bevy_state::DespawnOnEnter#0::24",
        "bevy_state::DespawnOnEnter#0::25",
        "bevy_state::DespawnOnEnter#0::26",
        "bevy_state::DespawnOnEnter#0::27",
        "bevy_state::DespawnOnEnter#0::28",
        "bevy_state::DespawnOnEnter#0::29",
        "bevy_state::DespawnOnEnter#0::30",
        "bevy_state::DespawnOnEnter#0::31",
    ],
    [
        "bevy_state::DespawnOnEnter#1::0",
        "bevy_state::DespawnOnEnter#1::1",
        "bevy_state::DespawnOnEnter#1::2",
        "bevy_state::DespawnOnEnter#1::3",
        "bevy_state::DespawnOnEnter#1::4",
        "bevy_state::DespawnOnEnter#1::5",
        "bevy_state::DespawnOnEnter#1::6",
        "bevy_state::DespawnOnEnter#1::7",
        "bevy_state::DespawnOnEnter#1::8",
        "bevy_state::DespawnOnEnter#1::9",
        "bevy_state::DespawnOnEnter#1::10",
        "bevy_state::DespawnOnEnter#1::11",
        "bevy_state::DespawnOnEnter#1::12",
        "bevy_state::DespawnOnEnter#1::13",
        "bevy_state::DespawnOnEnter#1::14",
        "bevy_state::DespawnOnEnter#1::15",
        "bevy_state::DespawnOnEnter#1::16",
        "bevy_state::DespawnOnEnter#1::17",
        "bevy_state::DespawnOnEnter#1::18",
        "bevy_state::DespawnOnEnter#1::19",
        "bevy_state::DespawnOnEnter#1::20",
        "bevy_state::DespawnOnEnter#1::21",
        "bevy_state::DespawnOnEnter#1::22",
        "bevy_state::DespawnOnEnter#1::23",
        "bevy_state::DespawnOnEnter#1::24",
        "bevy_state::DespawnOnEnter#1::25",
        "bevy_state::DespawnOnEnter#1::26",
        "bevy_state::DespawnOnEnter#1::27",
        "bevy_state::DespawnOnEnter#1::28",
        "bevy_state::DespawnOnEnter#1::29",
        "bevy_state::DespawnOnEnter#1::30",
        "bevy_state::DespawnOnEnter#1::31",
    ],
    [
        "bevy_state::DespawnOnEnter#2::0",
        "bevy_state::DespawnOnEnter#2::1",
        "bevy_state::DespawnOnEnter#2::2",
        "bevy_state::DespawnOnEnter#2::3",
        "bevy_state::DespawnOnEnter#2::4",
        "bevy_state::DespawnOnEnter#2::5",
        "bevy_state::DespawnOnEnter#2::6",
        "bevy_state::DespawnOnEnter#2::7",
        "bevy_state::DespawnOnEnter#2::8",
        "bevy_state::DespawnOnEnter#2::9",
        "bevy_state::DespawnOnEnter#2::10",
        "bevy_state::DespawnOnEnter#2::11",
        "bevy_state::DespawnOnEnter#2::12",
        "bevy_state::DespawnOnEnter#2::13",
        "bevy_state::DespawnOnEnter#2::14",
        "bevy_state::DespawnOnEnter#2::15",
        "bevy_state::DespawnOnEnter#2::16",
        "bevy_state::DespawnOnEnter#2::17",
        "bevy_state::DespawnOnEnter#2::18",
        "bevy_state::DespawnOnEnter#2::19",
        "bevy_state::DespawnOnEnter#2::20",
        "bevy_state::DespawnOnEnter#2::21",
        "bevy_state::DespawnOnEnter#2::22",
        "bevy_state::DespawnOnEnter#2::23",
        "bevy_state::DespawnOnEnter#2::24",
        "bevy_state::DespawnOnEnter#2::25",
        "bevy_state::DespawnOnEnter#2::26",
        "bevy_state::DespawnOnEnter#2::27",
        "bevy_state::DespawnOnEnter#2::28",
        "bevy_state::DespawnOnEnter#2::29",
        "bevy_state::DespawnOnEnter#2::30",
        "bevy_state::DespawnOnEnter#2::31",
    ],
    [
        "bevy_state::DespawnOnEnter#3::0",
        "bevy_state::DespawnOnEnter#3::1",
        "bevy_state::DespawnOnEnter#3::2",
        "bevy_state::DespawnOnEnter#3::3",
        "bevy_state::DespawnOnEnter#3::4",
        "bevy_state::DespawnOnEnter#3::5",
        "bevy_state::DespawnOnEnter#3::6",
        "bevy_state::DespawnOnEnter#3::7",
        "bevy_state::DespawnOnEnter#3::8",
        "bevy_state::DespawnOnEnter#3::9",
        "bevy_state::DespawnOnEnter#3::10",
        "bevy_state::DespawnOnEnter#3::11",
        "bevy_state::DespawnOnEnter#3::12",
        "bevy_state::DespawnOnEnter#3::13",
        "bevy_state::DespawnOnEnter#3::14",
        "bevy_state::DespawnOnEnter#3::15",
        "bevy_state::DespawnOnEnter#3::16",
        "bevy_state::DespawnOnEnter#3::17",
        "bevy_state::DespawnOnEnter#3::18",
        "bevy_state::DespawnOnEnter#3::19",
        "bevy_state::DespawnOnEnter#3::20",
        "bevy_state::DespawnOnEnter#3::21",
        "bevy_state::DespawnOnEnter#3::22",
        "bevy_state::DespawnOnEnter#3::23",
        "bevy_state::DespawnOnEnter#3::24",
        "bevy_state::DespawnOnEnter#3::25",
        "bevy_state::DespawnOnEnter#3::26",
        "bevy_state::DespawnOnEnter#3::27",
        "bevy_state::DespawnOnEnter#3::28",
        "bevy_state::DespawnOnEnter#3::29",
        "bevy_state::DespawnOnEnter#3::30",
        "bevy_state::DespawnOnEnter#3::31",
    ],
    [
        "bevy_state::DespawnOnEnter#4::0",
        "bevy_state::DespawnOnEnter#4::1",
        "bevy_state::DespawnOnEnter#4::2",
        "bevy_state::DespawnOnEnter#4::3",
        "bevy_state::DespawnOnEnter#4::4",
        "bevy_state::DespawnOnEnter#4::5",
        "bevy_state::DespawnOnEnter#4::6",
        "bevy_state::DespawnOnEnter#4::7",
        "bevy_state::DespawnOnEnter#4::8",
        "bevy_state::DespawnOnEnter#4::9",
        "bevy_state::DespawnOnEnter#4::10",
        "bevy_state::DespawnOnEnter#4::11",
        "bevy_state::DespawnOnEnter#4::12",
        "bevy_state::DespawnOnEnter#4::13",
        "bevy_state::DespawnOnEnter#4::14",
        "bevy_state::DespawnOnEnter#4::15",
        "bevy_state::DespawnOnEnter#4::16",
        "bevy_state::DespawnOnEnter#4::17",
        "bevy_state::DespawnOnEnter#4::18",
        "bevy_state::DespawnOnEnter#4::19",
        "bevy_state::DespawnOnEnter#4::20",
        "bevy_state::DespawnOnEnter#4::21",
        "bevy_state::DespawnOnEnter#4::22",
        "bevy_state::DespawnOnEnter#4::23",
        "bevy_state::DespawnOnEnter#4::24",
        "bevy_state::DespawnOnEnter#4::25",
        "bevy_state::DespawnOnEnter#4::26",
        "bevy_state::DespawnOnEnter#4::27",
        "bevy_state::DespawnOnEnter#4::28",
        "bevy_state::DespawnOnEnter#4::29",
        "bevy_state::DespawnOnEnter#4::30",
        "bevy_state::DespawnOnEnter#4::31",
    ],
    [
        "bevy_state::DespawnOnEnter#5::0",
        "bevy_state::DespawnOnEnter#5::1",
        "bevy_state::DespawnOnEnter#5::2",
        "bevy_state::DespawnOnEnter#5::3",
        "bevy_state::DespawnOnEnter#5::4",
        "bevy_state::DespawnOnEnter#5::5",
        "bevy_state::DespawnOnEnter#5::6",
        "bevy_state::DespawnOnEnter#5::7",
        "bevy_state::DespawnOnEnter#5::8",
        "bevy_state::DespawnOnEnter#5::9",
        "bevy_state::DespawnOnEnter#5::10",
        "bevy_state::DespawnOnEnter#5::11",
        "bevy_state::DespawnOnEnter#5::12",
        "bevy_state::DespawnOnEnter#5::13",
        "bevy_state::DespawnOnEnter#5::14",
        "bevy_state::DespawnOnEnter#5::15",
        "bevy_state::DespawnOnEnter#5::16",
        "bevy_state::DespawnOnEnter#5::17",
        "bevy_state::DespawnOnEnter#5::18",
        "bevy_state::DespawnOnEnter#5::19",
        "bevy_state::DespawnOnEnter#5::20",
        "bevy_state::DespawnOnEnter#5::21",
        "bevy_state::DespawnOnEnter#5::22",
        "bevy_state::DespawnOnEnter#5::23",
        "bevy_state::DespawnOnEnter#5::24",
        "bevy_state::DespawnOnEnter#5::25",
        "bevy_state::DespawnOnEnter#5::26",
        "bevy_state::DespawnOnEnter#5::27",
        "bevy_state::DespawnOnEnter#5::28",
        "bevy_state::DespawnOnEnter#5::29",
        "bevy_state::DespawnOnEnter#5::30",
        "bevy_state::DespawnOnEnter#5::31",
    ],
    [
        "bevy_state::DespawnOnEnter#6::0",
        "bevy_state::DespawnOnEnter#6::1",
        "bevy_state::DespawnOnEnter#6::2",
        "bevy_state::DespawnOnEnter#6::3",
        "bevy_state::DespawnOnEnter#6::4",
        "bevy_state::DespawnOnEnter#6::5",
        "bevy_state::DespawnOnEnter#6::6",
        "bevy_state::DespawnOnEnter#6::7",
        "bevy_state::DespawnOnEnter#6::8",
        "bevy_state::DespawnOnEnter#6::9",
        "bevy_state::DespawnOnEnter#6::10",
        "bevy_state::DespawnOnEnter#6::11",
        "bevy_state::DespawnOnEnter#6::12",
        "bevy_state::DespawnOnEnter#6::13",
        "bevy_state::DespawnOnEnter#6::14",
        "bevy_state::DespawnOnEnter#6::15",
        "bevy_state::DespawnOnEnter#6::16",
        "bevy_state::DespawnOnEnter#6::17",
        "bevy_state::DespawnOnEnter#6::18",
        "bevy_state::DespawnOnEnter#6::19",
        "bevy_state::DespawnOnEnter#6::20",
        "bevy_state::DespawnOnEnter#6::21",
        "bevy_state::DespawnOnEnter#6::22",
        "bevy_state::DespawnOnEnter#6::23",
        "bevy_state::DespawnOnEnter#6::24",
        "bevy_state::DespawnOnEnter#6::25",
        "bevy_state::DespawnOnEnter#6::26",
        "bevy_state::DespawnOnEnter#6::27",
        "bevy_state::DespawnOnEnter#6::28",
        "bevy_state::DespawnOnEnter#6::29",
        "bevy_state::DespawnOnEnter#6::30",
        "bevy_state::DespawnOnEnter#6::31",
    ],
    [
        "bevy_state::DespawnOnEnter#7::0",
        "bevy_state::DespawnOnEnter#7::1",
        "bevy_state::DespawnOnEnter#7::2",
        "bevy_state::DespawnOnEnter#7::3",
        "bevy_state::DespawnOnEnter#7::4",
        "bevy_state::DespawnOnEnter#7::5",
        "bevy_state::DespawnOnEnter#7::6",
        "bevy_state::DespawnOnEnter#7::7",
        "bevy_state::DespawnOnEnter#7::8",
        "bevy_state::DespawnOnEnter#7::9",
        "bevy_state::DespawnOnEnter#7::10",
        "bevy_state::DespawnOnEnter#7::11",
        "bevy_state::DespawnOnEnter#7::12",
        "bevy_state::DespawnOnEnter#7::13",
        "bevy_state::DespawnOnEnter#7::14",
        "bevy_state::DespawnOnEnter#7::15",
        "bevy_state::DespawnOnEnter#7::16",
        "bevy_state::DespawnOnEnter#7::17",
        "bevy_state::DespawnOnEnter#7::18",
        "bevy_state::DespawnOnEnter#7::19",
        "bevy_state::DespawnOnEnter#7::20",
        "bevy_state::DespawnOnEnter#7::21",
        "bevy_state::DespawnOnEnter#7::22",
        "bevy_state::DespawnOnEnter#7::23",
        "bevy_state::DespawnOnEnter#7::24",
        "bevy_state::DespawnOnEnter#7::25",
        "bevy_state::DespawnOnEnter#7::26",
        "bevy_state::DespawnOnEnter#7::27",
        "bevy_state::DespawnOnEnter#7::28",
        "bevy_state::DespawnOnEnter#7::29",
        "bevy_state::DespawnOnEnter#7::30",
        "bevy_state::DespawnOnEnter#7::31",
    ],
];

impl<S: FreelyMutableState + Copy, const STATE_SLOT: usize, const VAR_SLOT: usize>
    HostWireBridge<ModDespawnOnExitSlot<S, STATE_SLOT, VAR_SLOT>>
    for ExitMarkerBridge<S, STATE_SLOT, VAR_SLOT>
{
    type Wire = u8;
    const CANONICAL_NAME: &'static str = EXIT_MARKER_NAMES[STATE_SLOT][VAR_SLOT];
    const SUPPORTS_SET: bool = false;

    fn to_wire(_value: &ModDespawnOnExitSlot<S, STATE_SLOT, VAR_SLOT>) -> u8 {
        0
    }
}

impl<S: FreelyMutableState + Copy, const STATE_SLOT: usize, const VAR_SLOT: usize>
    HostWireBridge<ModDespawnOnEnterSlot<S, STATE_SLOT, VAR_SLOT>>
    for EnterMarkerBridge<S, STATE_SLOT, VAR_SLOT>
{
    type Wire = u8;
    const CANONICAL_NAME: &'static str = ENTER_MARKER_NAMES[STATE_SLOT][VAR_SLOT];
    const SUPPORTS_SET: bool = false;

    fn to_wire(_value: &ModDespawnOnEnterSlot<S, STATE_SLOT, VAR_SLOT>) -> u8 {
        0
    }
}

fn register_exit_const<
    S: FreelyMutableState + Copy,
    const STATE_SLOT: usize,
    const VAR_SLOT: usize,
>(
    world: &mut World,
) -> Result<(BamComponent, &'static str), ComponentCollisionError> {
    let comp_id = world.register_component::<ModDespawnOnExitSlot<S, STATE_SLOT, VAR_SLOT>>();
    let handle = world
        .resource_mut::<ModRegistry>()
        .register_bridged_component::<
            ModDespawnOnExitSlot<S, STATE_SLOT, VAR_SLOT>,
            ExitMarkerBridge<S, STATE_SLOT, VAR_SLOT>,
        >(comp_id, 0, Some(insert_despawn_on_exit_slot::<S, VAR_SLOT>))?;
    Ok((handle, EXIT_MARKER_NAMES[STATE_SLOT][VAR_SLOT]))
}

fn register_enter_const<
    S: FreelyMutableState + Copy,
    const STATE_SLOT: usize,
    const VAR_SLOT: usize,
>(
    world: &mut World,
) -> Result<(BamComponent, &'static str), ComponentCollisionError> {
    let comp_id = world.register_component::<ModDespawnOnEnterSlot<S, STATE_SLOT, VAR_SLOT>>();
    let handle = world
        .resource_mut::<ModRegistry>()
        .register_bridged_component::<
            ModDespawnOnEnterSlot<S, STATE_SLOT, VAR_SLOT>,
            EnterMarkerBridge<S, STATE_SLOT, VAR_SLOT>,
        >(comp_id, 0, Some(insert_despawn_on_enter_slot::<S, VAR_SLOT>))?;
    Ok((handle, ENTER_MARKER_NAMES[STATE_SLOT][VAR_SLOT]))
}

macro_rules! dispatch_var_slot {
    ($fn_name:ident, $state_slot:literal, $var_slot:expr, $world:expr, $($v:literal),+ $(,)?) => {
        match $var_slot {
            $(
                $v => $fn_name::<S, $state_slot, $v>($world),
            )+
            _ => unreachable!("var_slot < MAX_MOD_STATE_VARIANTS"),
        }
    };
}

macro_rules! dispatch_state_and_var_slot {
    ($fn_name:ident, $state_slot:expr, $var_slot:expr, $world:expr, $($s:literal),+ $(,)?) => {
        match $state_slot {
            $(
                $s => dispatch_var_slot!(
                    $fn_name,
                    $s,
                    $var_slot,
                    $world,
                    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15,
                    16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31
                ),
            )+
            _ => unreachable!("state_slot < MAX_EXPOSED_MOD_STATES"),
        }
    };
}

fn register_despawn_on_exit_slot<S: FreelyMutableState + Copy>(
    world: &mut World,
    state_slot: usize,
    var_slot: usize,
) -> Result<(BamComponent, &'static str), ComponentCollisionError> {
    dispatch_state_and_var_slot!(
        register_exit_const,
        state_slot,
        var_slot,
        world,
        0,
        1,
        2,
        3,
        4,
        5,
        6,
        7
    )
}

fn register_despawn_on_enter_slot<S: FreelyMutableState + Copy>(
    world: &mut World,
    state_slot: usize,
    var_slot: usize,
) -> Result<(BamComponent, &'static str), ComponentCollisionError> {
    dispatch_state_and_var_slot!(
        register_enter_const,
        state_slot,
        var_slot,
        world,
        0,
        1,
        2,
        3,
        4,
        5,
        6,
        7
    )
}

/// Metadata descriptor for an exposed Bevy state enum `S`, stored as a [`Resource`] and returned
/// by [`ModStateExt::expose_mod_state`].
#[derive(Resource, Clone, Debug)]
pub struct ModStateSpec<S: FreelyMutableState + Copy> {
    state_slot: usize,
    base_schedule_id: ScheduleId,
    state_handle: BamComponent,
    state_resource_name: &'static str,
    next_state_handle: BamComponent,
    next_state_resource_name: &'static str,
    variants: Vec<(u32, String, S)>,
    despawn_on_exit: Vec<(BamComponent, &'static str)>,
    despawn_on_enter: Vec<(BamComponent, &'static str)>,
}

impl<S: FreelyMutableState + Copy> ModStateSpec<S> {
    /// Returns the deterministic 0-based state slot assigned to `S` in this `World`.
    #[inline]
    pub const fn state_slot(&self) -> usize {
        self.state_slot
    }

    /// Returns the base custom [`ScheduleId`] from which `OnEnter`, `OnExit`, and `OnTransition`
    /// schedule IDs are derived.
    #[inline]
    pub const fn base_schedule_id(&self) -> ScheduleId {
        self.base_schedule_id
    }

    /// Returns the number of exposed variants.
    #[inline]
    pub fn variant_count(&self) -> usize {
        self.variants.len()
    }

    /// Returns the [`BamComponent`] handle for read-only `State<S>`.
    #[inline]
    pub const fn state_handle(&self) -> BamComponent {
        self.state_handle
    }

    /// Returns the canonical registry name for `State<S>` (e.g. `"bevy_state::State#0"`).
    #[inline]
    pub const fn state_resource_name(&self) -> &'static str {
        self.state_resource_name
    }

    /// Returns the [`BamComponent`] handle for writable [`ModNextState<S>`].
    #[inline]
    pub const fn next_state_handle(&self) -> BamComponent {
        self.next_state_handle
    }

    /// Returns the canonical registry name for [`ModNextState<S>`] (e.g. `"bevy_abi_mod::ModNextState#0"`).
    #[inline]
    pub const fn next_state_resource_name(&self) -> &'static str {
        self.next_state_resource_name
    }

    /// Returns the `OnEnter(variant)` custom [`ScheduleId`] for `discriminant`.
    pub fn on_enter_schedule(&self, discriminant: u32) -> Option<ScheduleId> {
        let idx = self
            .variants
            .iter()
            .position(|(d, _, _)| *d == discriminant)?;
        Some(ScheduleId::custom(self.base_schedule_id.0 + idx as u32))
    }

    /// Returns the `OnExit(variant)` custom [`ScheduleId`] for `discriminant`.
    pub fn on_exit_schedule(&self, discriminant: u32) -> Option<ScheduleId> {
        let idx = self
            .variants
            .iter()
            .position(|(d, _, _)| *d == discriminant)?;
        let n = self.variants.len() as u32;
        Some(ScheduleId::custom(self.base_schedule_id.0 + n + idx as u32))
    }

    /// Returns the `OnTransition { exited, entered }` custom [`ScheduleId`].
    pub fn on_transition_schedule(
        &self,
        exited_discriminant: u32,
        entered_discriminant: u32,
    ) -> Option<ScheduleId> {
        let i = self
            .variants
            .iter()
            .position(|(d, _, _)| *d == exited_discriminant)?;
        let j = self
            .variants
            .iter()
            .position(|(d, _, _)| *d == entered_discriminant)?;
        let n = self.variants.len() as u32;
        Some(ScheduleId::custom(
            self.base_schedule_id.0 + 2 * n + (i as u32) * n + (j as u32),
        ))
    }

    /// Returns the host-visible `"host::state.<ShortS>.<discriminant>"` system set name.
    pub fn in_state_set_name(&self, discriminant: u32) -> Option<String> {
        self.variants.iter().find(|(d, _, _)| *d == discriminant)?;
        let short = short_type_name::<S>();
        Some(format!("host::state.{short}.{discriminant}"))
    }

    /// Returns the host-visible `"host::state.<ShortS>.<variant_name>"` system set name.
    pub fn in_state_set_name_by_variant(&self, variant_name: &str) -> Option<String> {
        self.variants
            .iter()
            .find(|(_, name, _)| name == variant_name)?;
        let short = short_type_name::<S>();
        Some(format!("host::state.{short}.{variant_name}"))
    }

    /// Returns the [`BamComponent`] handle for the `DespawnOnExit` marker of `discriminant`.
    pub fn despawn_on_exit_handle(&self, discriminant: u32) -> Option<BamComponent> {
        let idx = self
            .variants
            .iter()
            .position(|(d, _, _)| *d == discriminant)?;
        self.despawn_on_exit.get(idx).map(|(h, _)| *h)
    }

    /// Returns the canonical registry name for the `DespawnOnExit` marker of `discriminant`.
    pub fn despawn_on_exit_name(&self, discriminant: u32) -> Option<&'static str> {
        let idx = self
            .variants
            .iter()
            .position(|(d, _, _)| *d == discriminant)?;
        self.despawn_on_exit.get(idx).map(|(_, n)| *n)
    }

    /// Returns the [`BamComponent`] handle for the `DespawnOnEnter` marker of `discriminant`.
    pub fn despawn_on_enter_handle(&self, discriminant: u32) -> Option<BamComponent> {
        let idx = self
            .variants
            .iter()
            .position(|(d, _, _)| *d == discriminant)?;
        self.despawn_on_enter.get(idx).map(|(h, _)| *h)
    }

    /// Returns the canonical registry name for the `DespawnOnEnter` marker of `discriminant`.
    pub fn despawn_on_enter_name(&self, discriminant: u32) -> Option<&'static str> {
        let idx = self
            .variants
            .iter()
            .position(|(d, _, _)| *d == discriminant)?;
        self.despawn_on_enter.get(idx).map(|(_, n)| *n)
    }
}

fn short_type_name<S>() -> &'static str {
    let full = core::any::type_name::<S>();
    full.rsplit("::").next().unwrap_or(full)
}

fn register_schedule_label<L: ScheduleLabel>(world: &mut World, id: ScheduleId, label: L) {
    let interned = label.intern();
    if !world.contains_resource::<Schedules>() {
        world.insert_resource(Schedules::new());
    }
    if let Some(mut schedules) = world.get_resource_mut::<Schedules>() {
        let _ = schedules.entry(interned);
    }
    if !world.contains_resource::<ModScheduleMap>() {
        world.insert_resource(ModScheduleMap::new());
    }
    if let Some(mut map) = world.get_resource_mut::<ModScheduleMap>() {
        let ok = map.register(id, interned);
        debug_assert!(ok);
    }
}

fn register_visible_set<Set: SystemSet + Clone>(world: &mut World, name: &str, set: Set) {
    if !world.contains_resource::<ModVisibleSets>() {
        world.insert_resource(ModVisibleSets::new());
    }
    if let Some(mut sets) = world.get_resource_mut::<ModVisibleSets>() {
        sets.register(name, set);
    }
}

/// Extension trait on [`App`] for exposing Bevy [`FreelyMutableState`] enums to native mods.
pub trait ModStateExt {
    /// Exposes state `S` across the C ABI:
    /// - Read-only `State<S>` (`u32` discriminant wire bridge) and writable [`ModNextState<S>`]
    ///   (flushing into `NextState::<S>::set_if_different` in `PreUpdate` and `PostUpdate`),
    /// - Deterministic `OnEnter(s)` (`base + i`), `OnExit(s)` (`base + N + i`), and
    ///   `OnTransition { exited, entered }` (`base + 2*N + i*N + j`) schedules in [`ModScheduleMap`],
    /// - Host-visible `in_state` system sets (`"host::state.<ShortS>.<disc|Name>"`), and
    /// - Per-variant `DespawnOnExit(s)` / `DespawnOnEnter(s)` default-insertable marker bridges.
    fn expose_mod_state<S: FreelyMutableState + Copy>(
        &mut self,
        base_schedule_id: ScheduleId,
        variants: &[(u32, &str, S)],
    ) -> Result<ModStateSpec<S>, ModStateError>;
}

impl ModStateExt for App {
    fn expose_mod_state<S: FreelyMutableState + Copy>(
        &mut self,
        base_schedule_id: ScheduleId,
        variants: &[(u32, &str, S)],
    ) -> Result<ModStateSpec<S>, ModStateError> {
        if !base_schedule_id.is_custom() {
            return Err(ModStateError::InvalidBaseScheduleId(base_schedule_id));
        }
        if variants.is_empty() {
            return Err(ModStateError::EmptyVariants);
        }
        if variants.len() > MAX_MOD_STATE_VARIANTS {
            return Err(ModStateError::TooManyVariants {
                count: variants.len(),
                max: MAX_MOD_STATE_VARIANTS,
            });
        }

        let n = variants.len() as u32;
        let total_schedules = 2u32
            .checked_mul(n)
            .and_then(|v| v.checked_add(n.checked_mul(n)?))
            .ok_or(ModStateError::ScheduleIdOverflow)?;
        if base_schedule_id.0.checked_add(total_schedules).is_none() {
            return Err(ModStateError::ScheduleIdOverflow);
        }

        let mut seen_discs: HashSet<u32> = HashSet::default();
        let mut seen_names: HashSet<&str> = HashSet::default();
        let mut owned_variants = Vec::with_capacity(variants.len());
        for (disc, name, val) in variants {
            if !seen_discs.insert(*disc) {
                return Err(ModStateError::DuplicateDiscriminant(*disc));
            }
            if name.is_empty() || name.contains("::") || !seen_names.insert(*name) {
                return Err(ModStateError::InvalidVariantName((*name).to_string()));
            }
            owned_variants.push((*disc, (*name).to_string(), *val));
        }

        if !self.is_plugin_added::<StatesPlugin>() {
            self.add_plugins(StatesPlugin);
        }
        if !self.world().contains_resource::<State<S>>() {
            self.insert_state(owned_variants[0].2);
        }
        if !self.world().contains_resource::<ModRegistry>() {
            self.insert_resource(ModRegistry::new());
        }
        if !self
            .world()
            .contains_resource::<ExposedStateSlotAllocator>()
        {
            self.insert_resource(ExposedStateSlotAllocator::default());
        }
        let state_slot = self
            .world_mut()
            .resource_mut::<ExposedStateSlotAllocator>()
            .allocate::<S>()?;

        store_state_variants(owned_variants.clone());

        let initial_disc = self
            .world()
            .get_resource::<State<S>>()
            .and_then(|s| lookup_discriminant_for(s.get()))
            .unwrap_or(owned_variants[0].0);
        let pair_list: Vec<(u32, S)> = owned_variants.iter().map(|(d, _, v)| (*d, *v)).collect();
        self.insert_resource(ModNextState::new(initial_disc, pair_list));

        let (state_handle, state_resource_name, next_state_handle, next_state_resource_name) =
            register_state_resources_for_slot::<S>(self.world_mut(), state_slot)?;

        self.add_systems(PreUpdate, sync_mod_next_state::<S>);
        self.add_systems(PostUpdate, sync_mod_next_state::<S>);

        let short = short_type_name::<S>();
        let full_sanitized = core::any::type_name::<S>().replace("::", ".");

        let mut despawn_on_exit = Vec::with_capacity(owned_variants.len());
        let mut despawn_on_enter = Vec::with_capacity(owned_variants.len());

        for (i, (disc, var_name, variant)) in owned_variants.iter().enumerate() {
            let enter_id = ScheduleId::custom(base_schedule_id.0 + i as u32);
            let exit_id = ScheduleId::custom(base_schedule_id.0 + n + i as u32);
            register_schedule_label(self.world_mut(), enter_id, OnEnter(*variant));
            register_schedule_label(self.world_mut(), exit_id, OnExit(*variant));

            for (j, (_, _, entered)) in owned_variants.iter().enumerate() {
                let trans_id =
                    ScheduleId::custom(base_schedule_id.0 + 2 * n + (i as u32) * n + (j as u32));
                register_schedule_label(
                    self.world_mut(),
                    trans_id,
                    OnTransition {
                        exited: *variant,
                        entered: *entered,
                    },
                );
            }

            let set = ModInStateSet::<S>::new(*disc);
            self.configure_sets(PreUpdate, set.run_if(in_state(*variant)));
            self.configure_sets(Update, set.run_if(in_state(*variant)));
            self.configure_sets(PostUpdate, set.run_if(in_state(*variant)));
            self.configure_sets(FixedUpdate, set.run_if(in_state(*variant)));

            register_visible_set(self.world_mut(), &format!("state.{short}.{disc}"), set);
            register_visible_set(self.world_mut(), &format!("state.{short}.{var_name}"), set);
            if full_sanitized != short {
                register_visible_set(
                    self.world_mut(),
                    &format!("state.{full_sanitized}.{disc}"),
                    set,
                );
                register_visible_set(
                    self.world_mut(),
                    &format!("state.{full_sanitized}.{var_name}"),
                    set,
                );
            }

            despawn_on_exit.push(register_despawn_on_exit_slot::<S>(
                self.world_mut(),
                state_slot,
                i,
            )?);
            despawn_on_enter.push(register_despawn_on_enter_slot::<S>(
                self.world_mut(),
                state_slot,
                i,
            )?);
        }

        let spec = ModStateSpec {
            state_slot,
            base_schedule_id,
            state_handle,
            state_resource_name,
            next_state_handle,
            next_state_resource_name,
            variants: owned_variants,
            despawn_on_exit,
            despawn_on_enter,
        };
        self.insert_resource(spec.clone());
        Ok(spec)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AbiModPlugin, ModVisibleSets};
    use bevy_abi_mod_host_sys::GuestSystem;
    use bevy_abi_mod_sys::{BamSystemCtx, ComponentUuid};
    use bevy_state::state::States;
    use core::ffi::c_void;
    use core::sync::atomic::{AtomicUsize, Ordering};

    #[derive(States, Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
    enum GamePhase {
        #[default]
        Menu,
        Playing,
        Paused,
    }

    #[derive(Component)]
    struct PhaseScopedTag;

    #[test]
    fn expose_mod_state_schedules_set_if_different_visible_sets_and_despawn_markers() {
        static ENTER_PLAYING_COUNT: AtomicUsize = AtomicUsize::new(0);
        static EXIT_MENU_COUNT: AtomicUsize = AtomicUsize::new(0);
        static TRANSITION_M_TO_P_COUNT: AtomicUsize = AtomicUsize::new(0);
        static IN_PLAYING_TICK_COUNT: AtomicUsize = AtomicUsize::new(0);

        ENTER_PLAYING_COUNT.store(0, Ordering::SeqCst);
        EXIT_MENU_COUNT.store(0, Ordering::SeqCst);
        TRANSITION_M_TO_P_COUNT.store(0, Ordering::SeqCst);
        IN_PLAYING_TICK_COUNT.store(0, Ordering::SeqCst);

        extern "C" fn on_enter_playing(_ctx: *mut BamSystemCtx, _ud: *mut c_void) -> FfiResult {
            ENTER_PLAYING_COUNT.fetch_add(1, Ordering::SeqCst);
            FfiResult::OK
        }
        extern "C" fn on_exit_menu(_ctx: *mut BamSystemCtx, _ud: *mut c_void) -> FfiResult {
            EXIT_MENU_COUNT.fetch_add(1, Ordering::SeqCst);
            FfiResult::OK
        }
        extern "C" fn on_transition_m_to_p(_ctx: *mut BamSystemCtx, _ud: *mut c_void) -> FfiResult {
            TRANSITION_M_TO_P_COUNT.fetch_add(1, Ordering::SeqCst);
            FfiResult::OK
        }
        extern "C" fn while_playing(_ctx: *mut BamSystemCtx, _ud: *mut c_void) -> FfiResult {
            IN_PLAYING_TICK_COUNT.fetch_add(1, Ordering::SeqCst);
            FfiResult::OK
        }

        let mut app = App::new();
        app.add_plugins((AbiModPlugin, StatesPlugin));
        let spec = app
            .expose_mod_state::<GamePhase>(
                ScheduleId::custom(2000),
                &[
                    (0, "Menu", GamePhase::Menu),
                    (1, "Playing", GamePhase::Playing),
                    (2, "Paused", GamePhase::Paused),
                ],
            )
            .unwrap();

        assert_eq!(spec.state_slot(), 0);
        assert_eq!(spec.state_resource_name(), "bevy_state::State#0");
        assert_eq!(
            spec.next_state_resource_name(),
            "bevy_abi_mod::ModNextState#0"
        );
        assert_eq!(spec.on_enter_schedule(0), Some(ScheduleId::custom(2000)));
        assert_eq!(spec.on_enter_schedule(1), Some(ScheduleId::custom(2001)));
        assert_eq!(spec.on_exit_schedule(0), Some(ScheduleId::custom(2003)));
        assert_eq!(
            spec.on_transition_schedule(0, 1),
            Some(ScheduleId::custom(2007))
        );

        let reg = app.world().resource::<ModRegistry>().clone();
        assert_eq!(
            reg.get_by_name("State").unwrap().uuid,
            ComponentUuid::from_canonical_name("bevy_state::State#0")
        );
        assert!(reg.resolve_symbol("bam.v0.State.vtable").is_some());
        assert!(reg.resolve_symbol("bam.v0.ModNextState.vtable").is_some());

        let enter_playing_sys = GuestSystem::new(
            "enter_playing",
            Vec::new(),
            on_enter_playing,
            core::ptr::null_mut(),
            reg.clone(),
        )
        .with_schedule(spec.on_enter_schedule(1).unwrap());
        let exit_menu_sys = GuestSystem::new(
            "exit_menu",
            Vec::new(),
            on_exit_menu,
            core::ptr::null_mut(),
            reg.clone(),
        )
        .with_schedule(spec.on_exit_schedule(0).unwrap());
        let trans_sys = GuestSystem::new(
            "trans_m_to_p",
            Vec::new(),
            on_transition_m_to_p,
            core::ptr::null_mut(),
            reg.clone(),
        )
        .with_schedule(spec.on_transition_schedule(0, 1).unwrap());
        let in_playing_sys = GuestSystem::new(
            "while_playing",
            Vec::new(),
            while_playing,
            core::ptr::null_mut(),
            reg,
        );

        let playing_set = app
            .world()
            .resource::<ModVisibleSets>()
            .resolve("state.GamePhase.Playing")
            .unwrap();

        app.add_systems(OnEnter(GamePhase::Playing), enter_playing_sys);
        app.add_systems(OnExit(GamePhase::Menu), exit_menu_sys);
        app.add_systems(
            OnTransition {
                exited: GamePhase::Menu,
                entered: GamePhase::Playing,
            },
            trans_sys,
        );
        app.add_systems(Update, in_playing_sys.in_set(playing_set));

        // Frame 1: in `Menu` -> `while_playing` does not run.
        app.update();
        assert_eq!(IN_PLAYING_TICK_COUNT.load(Ordering::SeqCst), 0);

        // Verify `State<GamePhase>` reads `0` (`Menu`) via `HostWireBridge`.
        assert_eq!(
            StateWireBridge::<GamePhase, 0>::to_wire(app.world().resource::<State<GamePhase>>()),
            0
        );

        // Queue transition Menu (0) -> Playing (1) via `NextStateWireBridge::apply_wire`.
        assert_eq!(
            NextStateWireBridge::<GamePhase, 0>::apply_wire(
                app.world_mut()
                    .resource_mut::<ModNextState<GamePhase>>()
                    .into_inner(),
                1
            ),
            FfiResult::OK
        );

        // Frame 2: `PreUpdate` syncs `NextState::set_if_different(Playing)` -> `StateTransition` fires
        // `OnExit(Menu)`, `OnTransition`, `OnEnter(Playing)`, and `Update` runs `while_playing`.
        app.update();
        assert_eq!(
            *app.world().resource::<State<GamePhase>>().get(),
            GamePhase::Playing
        );
        assert_eq!(EXIT_MENU_COUNT.load(Ordering::SeqCst), 1);
        assert_eq!(TRANSITION_M_TO_P_COUNT.load(Ordering::SeqCst), 1);
        assert_eq!(ENTER_PLAYING_COUNT.load(Ordering::SeqCst), 1);
        assert_eq!(IN_PLAYING_TICK_COUNT.load(Ordering::SeqCst), 1);

        // Requesting `Playing` (1) again uses `set_if_different` -> does NOT re-trigger `OnEnter(Playing)`.
        assert_eq!(
            NextStateWireBridge::<GamePhase, 0>::apply_wire(
                app.world_mut()
                    .resource_mut::<ModNextState<GamePhase>>()
                    .into_inner(),
                1
            ),
            FfiResult::OK
        );
        app.update();
        assert_eq!(ENTER_PLAYING_COUNT.load(Ordering::SeqCst), 1);
        assert_eq!(IN_PLAYING_TICK_COUNT.load(Ordering::SeqCst), 2);

        // Invalid discriminant is rejected cleanly.
        assert_eq!(
            NextStateWireBridge::<GamePhase, 0>::apply_wire(
                app.world_mut()
                    .resource_mut::<ModNextState<GamePhase>>()
                    .into_inner(),
                99
            ),
            FfiResult::INVALID_ARGUMENT
        );

        // Spawn two entities and attach `DespawnOnExit(Playing)` and `DespawnOnEnter(Paused)` via
        // the registered default-insertable marker bridges.
        let e_exit = app.world_mut().spawn(PhaseScopedTag).id();
        let e_enter = app.world_mut().spawn(PhaseScopedTag).id();
        let exit_name = spec.despawn_on_exit_name(1).unwrap();
        let enter_name = spec.despawn_on_enter_name(2).unwrap();
        let (exit_fn, enter_fn) = {
            let reg = app.world().resource::<ModRegistry>();
            (
                reg.get_by_name(exit_name)
                    .unwrap()
                    .insert_default_fn
                    .unwrap(),
                reg.get_by_name(enter_name)
                    .unwrap()
                    .insert_default_fn
                    .unwrap(),
            )
        };
        exit_fn(app.world_mut(), e_exit);
        enter_fn(app.world_mut(), e_enter);

        assert!(
            app.world()
                .get::<DespawnOnExit<GamePhase>>(e_exit)
                .is_some()
        );
        assert!(
            app.world()
                .get::<DespawnOnEnter<GamePhase>>(e_enter)
                .is_some()
        );

        // Transition Playing (1) -> Paused (2) -> both scoped entities are despawned!
        assert_eq!(
            NextStateWireBridge::<GamePhase, 0>::apply_wire(
                app.world_mut()
                    .resource_mut::<ModNextState<GamePhase>>()
                    .into_inner(),
                2
            ),
            FfiResult::OK
        );
        app.update();
        assert_eq!(
            *app.world().resource::<State<GamePhase>>().get(),
            GamePhase::Paused
        );
        assert!(app.world().get_entity(e_exit).is_err());
        assert!(app.world().get_entity(e_enter).is_err());
        assert_eq!(IN_PLAYING_TICK_COUNT.load(Ordering::SeqCst), 2);
    }
}
