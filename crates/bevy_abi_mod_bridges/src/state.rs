//! `bevy_state` (`0.20`) bridges for `State<S>`, `ModNextState<S>` (`NextState::set_if_different`),
//! `SubStates`, read-only `ComputedStates`, dynamic generation-indexed [`ModStateSlab`] storage,
//! zero-copy [`ModStateEntry`] snapshots, [`ModStateTransitionRequest`] messages, deterministic
//! `OnEnter`/`OnExit`/`OnTransition` schedules, `in_state` visible system sets, and unbounded
//! [`ModDespawnOnExit`] / [`ModDespawnOnEnter`] state-scoped despawn markers.

use bevy_abi_mod_host::ModWorldExt;
use bevy_abi_mod_host_sys::{
    ComponentCollisionError, ComponentOptions, HostWireBridge, ModRegistry, ModScheduleMap,
    ModVisibleSets,
};
use bevy_abi_mod_macros::{AbiPod, ModComponent};
use bevy_abi_mod_sys::{BamComponent, FfiResult, ScheduleId};
use bevy_app::{App, FixedUpdate, PostUpdate, PreUpdate, Update};
use bevy_ecs::entity_disabling::Disabled;
use bevy_ecs::lifecycle::Insert;
use bevy_ecs::message::{Message, MessageCursor, Messages};
use bevy_ecs::observer::On;
use bevy_ecs::prelude::{Commands, Component, Entity, Local, Query, Res, ResMut, Resource};
use bevy_ecs::query::Allow;
use bevy_ecs::schedule::{IntoScheduleConfigs, ScheduleLabel, Schedules, SystemSet};
use bevy_ecs::world::World;
use bevy_platform::collections::{HashMap, HashSet};
use bevy_state::app::{AppExtStates, StatesPlugin};
use bevy_state::condition::in_state;
use bevy_state::state::{
    ComputedStates, FreelyMutableState, NextState, OnEnter, OnExit, OnTransition, State,
    StateTransition, StateTransitionSystems, States, SubStates,
};
use bevy_state::state_scoped::{DespawnOnEnter, DespawnOnExit};
use core::any::{Any, TypeId};
use core::fmt;
use core::marker::PhantomData;
use std::sync::{Arc, OnceLock, RwLock};

/// Wire discriminant sentinel indicating that a conditional state ([`SubStates`] or
/// [`ComputedStates`]) does not currently exist in the `World`.
pub const STATE_ABSENT_DISCRIMINANT: u32 = u32::MAX;

/// Canonical ABI name for the dynamic read-only [`ModStateEntry`] snapshot component.
pub const CANONICAL_MOD_STATE_ENTRY_NAME: &str = "bevy_state::ModStateEntry";

/// Canonical ABI name for the dynamic [`ModStateTransitionRequest`] message stream.
pub const CANONICAL_MOD_STATE_TRANSITION_REQUEST_NAME: &str =
    "bevy_state::ModStateTransitionRequest";

/// Canonical ABI name for the dynamic [`ModDespawnOnExit`] component.
pub const CANONICAL_MOD_DESPAWN_ON_EXIT_NAME: &str = "bevy_state::ModDespawnOnExit";

/// Canonical ABI name for the dynamic [`ModDespawnOnEnter`] component.
pub const CANONICAL_MOD_DESPAWN_ON_ENTER_NAME: &str = "bevy_state::ModDespawnOnEnter";

/// [`ModStateEntry::flags`] bit: transitions may be requested (`FreelyMutable` or `SubState`).
pub const MOD_STATE_FLAG_MUTABLE: u32 = 1 << 0;

/// [`ModStateEntry::flags`] bit: `State<S>` resource currently exists in the `World`.
pub const MOD_STATE_FLAG_ACTIVE: u32 = 1 << 1;

/// [`ModStateEntry::flags`] bit: state is a conditional [`SubStates`].
pub const MOD_STATE_FLAG_SUB_STATE: u32 = 1 << 2;

/// [`ModStateEntry::flags`] bit: state is a read-only [`ComputedStates`].
pub const MOD_STATE_FLAG_COMPUTED: u32 = 1 << 3;

/// Number of primary slot-0 legacy default-insertable marker aliases registered for backwards
/// compatibility with `GuestStateHandles::resolve_primary`.
///
/// This is **not** a limit on exposed states or variants: all states (`24+`, unbounded) and
/// arbitrary variant counts (`64+`, unbounded) are backed dynamically by [`ModStateSlab`],
/// [`ModStateEntry`], [`ModStateTransitionRequest`], [`ModDespawnOnExit`], and [`ModDespawnOnEnter`].
const LEGACY_PRIMARY_MARKER_SLOTS: usize = 32;

/// Errors returned by [`ModStateExt::expose_mod_state`], [`ModStateExt::register_mod_sub_state`],
/// and [`ModStateExt::register_mod_computed_state`].
#[derive(Debug)]
pub enum ModStateError {
    /// `base_schedule_id` must be a custom schedule (`>= ScheduleId::CUSTOM_BASE` = `1024`).
    InvalidBaseScheduleId(ScheduleId),
    /// `variants` slice was empty.
    EmptyVariants,
    /// Duplicate `u32` discriminant in `variants`.
    DuplicateDiscriminant(u32),
    /// Reserved sentinel discriminant [`STATE_ABSENT_DISCRIMINANT`] (`u32::MAX`) was supplied.
    ReservedDiscriminant(u32),
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
            Self::DuplicateDiscriminant(d) => write!(f, "duplicate state discriminant {d}"),
            Self::ReservedDiscriminant(d) => {
                write!(f, "discriminant {d} is reserved for absent states")
            }
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

/// Classification of an exposed Bevy state inside [`ModStateSlab`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ModStateKind {
    /// Top-level [`FreelyMutableState`] (`State<S>` + writable transitions).
    FreelyMutable,
    /// Conditional [`SubStates`] (exists only when parent source state matches; writable while active).
    SubState,
    /// Derived read-only [`ComputedStates`] (no `NextState`; transition writes rejected).
    Computed,
}

impl ModStateKind {
    /// Returns `true` if guests may request transitions on this state.
    #[inline]
    pub const fn is_mutable(self) -> bool {
        matches!(self, Self::FreelyMutable | Self::SubState)
    }

    #[inline]
    const fn base_flags(self) -> u32 {
        match self {
            Self::FreelyMutable => MOD_STATE_FLAG_MUTABLE,
            Self::SubState => MOD_STATE_FLAG_MUTABLE | MOD_STATE_FLAG_SUB_STATE,
            Self::Computed => MOD_STATE_FLAG_COMPUTED,
        }
    }
}

/// Generation-indexed handle `(slot_index, generation)` identifying an exposed state entry inside
/// [`ModStateSlab`].
///
/// Releasing a slot bumps its generation counter so stale handles cannot alias a newly exposed
/// state reusing the same `slot_index`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, AbiPod)]
pub struct ModStateHandle {
    /// Zero-based index inside [`ModStateSlab`].
    pub slot_index: u32,
    /// Non-zero generation counter invalidating stale handles after slot release/reuse.
    pub generation: u32,
}

impl ModStateHandle {
    /// Sentinel invalid handle (`slot_index = u32::MAX`, `generation = 0`).
    pub const INVALID: Self = Self {
        slot_index: u32::MAX,
        generation: 0,
    };

    /// Constructs a handle from raw `(slot_index, generation)`.
    #[inline]
    pub const fn new(slot_index: u32, generation: u32) -> Self {
        Self {
            slot_index,
            generation,
        }
    }

    /// Returns `true` if this handle carries a non-zero generation and valid slot index.
    #[inline]
    pub const fn is_valid(self) -> bool {
        self.generation != 0 && self.slot_index != u32::MAX
    }
}

/// Read-only zero-copy `#[repr(C)]` [`ModComponent`] entity mirroring the live state of any
/// exposed [`States`], [`SubStates`], or [`ComputedStates`] slot inside [`ModStateSlab`].
///
/// One host entity carrying [`ModStateEntry`] is maintained per active slab slot, allowing guests
/// in any language (Rust, C99, C++17, Zig) to inspect an unbounded number of states with zero
/// fixed slot limits.
#[repr(C)]
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, Eq, Hash, ModComponent)]
#[mod_component(name = "bevy_state::ModStateEntry")]
pub struct ModStateEntry {
    /// Zero-based slot index inside [`ModStateSlab`].
    pub state_slot: u32,
    /// Non-zero generation counter of the exposed state slot.
    pub state_generation: u32,
    /// Active state discriminant, or [`STATE_ABSENT_DISCRIMINANT`] (`u32::MAX`) when absent.
    pub current_discriminant: u32,
    /// Bitmask of `MOD_STATE_FLAG_*` (`MUTABLE`, `ACTIVE`, `SUB_STATE`, `COMPUTED`).
    pub flags: u32,
}

impl ModStateEntry {
    /// Constructs a [`ModStateEntry`] snapshot for `(state, kind, current_discriminant)`.
    #[inline]
    pub const fn new(
        state: ModStateHandle,
        kind: ModStateKind,
        current_discriminant: Option<u32>,
    ) -> Self {
        let active_flag = if current_discriminant.is_some() {
            MOD_STATE_FLAG_ACTIVE
        } else {
            0
        };
        let disc = match current_discriminant {
            Some(d) => d,
            None => STATE_ABSENT_DISCRIMINANT,
        };
        Self {
            state_slot: state.slot_index,
            state_generation: state.generation,
            current_discriminant: disc,
            flags: kind.base_flags() | active_flag,
        }
    }

    /// Returns the generation-indexed [`ModStateHandle`] for this entry.
    #[inline]
    pub const fn state_handle(self) -> ModStateHandle {
        ModStateHandle::new(self.state_slot, self.state_generation)
    }

    /// Returns `true` if `State<S>` currently exists in the `World`.
    #[inline]
    pub const fn is_active(self) -> bool {
        (self.flags & MOD_STATE_FLAG_ACTIVE) != 0
    }

    /// Returns `true` if transitions are permitted on this state (`false` for [`ComputedStates`]).
    #[inline]
    pub const fn is_mutable(self) -> bool {
        (self.flags & MOD_STATE_FLAG_MUTABLE) != 0
    }

    /// Returns the active discriminant, or `None` when inactive.
    #[inline]
    pub const fn active_discriminant(self) -> Option<u32> {
        if self.is_active() && self.current_discriminant != STATE_ABSENT_DISCRIMINANT {
            Some(self.current_discriminant)
        } else {
            None
        }
    }
}

/// Zero-padding `#[repr(C)]` [`Message`] + [`ModComponent`] allowing guests to request transitions
/// on **any** mutable state (`FreelyMutableState` or `SubStates`) in [`ModStateSlab`] by
/// generation-indexed [`ModStateHandle`] without fixed slot limits.
///
/// Requests targeting stale handles or read-only [`ComputedStates`] are cleanly ignored/rejected.
#[repr(C)]
#[derive(Message, Clone, Copy, Debug, Default, PartialEq, Eq, Hash, ModComponent)]
#[mod_component(name = "bevy_state::ModStateTransitionRequest")]
pub struct ModStateTransitionRequest {
    /// Zero-based slot index inside [`ModStateSlab`].
    pub state_slot: u32,
    /// Non-zero generation counter of the target state slot.
    pub state_generation: u32,
    /// Target variant discriminant to transition to via `NextState::set_if_different`.
    pub target_discriminant: u32,
    /// Explicit zero-reserved alignment bytes (`0`).
    pub reserved0: u32,
}

impl ModStateTransitionRequest {
    /// Constructs a transition request for `(state, target_discriminant)`.
    #[inline]
    pub const fn new(state: ModStateHandle, target_discriminant: u32) -> Self {
        Self {
            state_slot: state.slot_index,
            state_generation: state.generation,
            target_discriminant,
            reserved0: 0,
        }
    }

    /// Returns the generation-indexed [`ModStateHandle`] targeted by this request.
    #[inline]
    pub const fn state_handle(self) -> ModStateHandle {
        ModStateHandle::new(self.state_slot, self.state_generation)
    }
}

/// Dynamic, zero-padding `#[repr(C)]` [`ModComponent`] attaching Bevy's
/// `DespawnOnExit<S>(variant)` for **any** state in [`ModStateSlab`] and **any** variant
/// discriminant without fixed slot or variant limits.
#[repr(C)]
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, Eq, Hash, ModComponent)]
#[mod_component(name = "bevy_state::ModDespawnOnExit")]
pub struct ModDespawnOnExit {
    /// Zero-based slot index inside [`ModStateSlab`].
    pub state_slot: u32,
    /// Non-zero generation counter of the target state slot.
    pub state_generation: u32,
    /// Variant discriminant on whose exit this entity is despawned.
    pub discriminant: u32,
    /// Explicit zero-reserved alignment bytes (`0`).
    pub reserved0: u32,
}

impl ModDespawnOnExit {
    /// Constructs a [`ModDespawnOnExit`] marker for `(state, discriminant)`.
    #[inline]
    pub const fn new(state: ModStateHandle, discriminant: u32) -> Self {
        Self {
            state_slot: state.slot_index,
            state_generation: state.generation,
            discriminant,
            reserved0: 0,
        }
    }

    /// Returns the generation-indexed [`ModStateHandle`] encoded in this marker.
    #[inline]
    pub const fn state_handle(self) -> ModStateHandle {
        ModStateHandle::new(self.state_slot, self.state_generation)
    }
}

/// Dynamic, zero-padding `#[repr(C)]` [`ModComponent`] attaching Bevy's
/// `DespawnOnEnter<S>(variant)` for **any** state in [`ModStateSlab`] and **any** variant
/// discriminant without fixed slot or variant limits.
#[repr(C)]
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, Eq, Hash, ModComponent)]
#[mod_component(name = "bevy_state::ModDespawnOnEnter")]
pub struct ModDespawnOnEnter {
    /// Zero-based slot index inside [`ModStateSlab`].
    pub state_slot: u32,
    /// Non-zero generation counter of the target state slot.
    pub state_generation: u32,
    /// Variant discriminant on whose entry this entity is despawned.
    pub discriminant: u32,
    /// Explicit zero-reserved alignment bytes (`0`).
    pub reserved0: u32,
}

impl ModDespawnOnEnter {
    /// Constructs a [`ModDespawnOnEnter`] marker for `(state, discriminant)`.
    #[inline]
    pub const fn new(state: ModStateHandle, discriminant: u32) -> Self {
        Self {
            state_slot: state.slot_index,
            state_generation: state.generation,
            discriminant,
            reserved0: 0,
        }
    }

    /// Returns the generation-indexed [`ModStateHandle`] encoded in this marker.
    #[inline]
    pub const fn state_handle(self) -> ModStateHandle {
        ModStateHandle::new(self.state_slot, self.state_generation)
    }
}

const _: () = {
    assert!(core::mem::size_of::<ModStateHandle>() == 8);
    assert!(core::mem::align_of::<ModStateHandle>() == 4);
    assert!(core::mem::size_of::<ModStateEntry>() == 16);
    assert!(core::mem::align_of::<ModStateEntry>() == 4);
    assert!(core::mem::size_of::<ModStateTransitionRequest>() == 16);
    assert!(core::mem::align_of::<ModStateTransitionRequest>() == 4);
    assert!(core::mem::size_of::<ModDespawnOnExit>() == 16);
    assert!(core::mem::align_of::<ModDespawnOnExit>() == 4);
    assert!(core::mem::size_of::<ModDespawnOnEnter>() == 16);
    assert!(core::mem::align_of::<ModDespawnOnEnter>() == 4);
};

type StateVariantList<S> = Arc<Vec<(u32, String, S)>>;

static STATE_VARIANT_TABLES: OnceLock<RwLock<HashMap<TypeId, Arc<dyn Any + Send + Sync>>>> =
    OnceLock::new();

fn state_tables() -> &'static RwLock<HashMap<TypeId, Arc<dyn Any + Send + Sync>>> {
    STATE_VARIANT_TABLES.get_or_init(|| RwLock::new(HashMap::default()))
}

fn store_state_variants<S: States + Copy>(variants: Vec<(u32, String, S)>) {
    let arc: StateVariantList<S> = Arc::new(variants);
    if let Ok(mut map) = state_tables().write() {
        map.insert(TypeId::of::<S>(), arc);
    }
}

fn lookup_discriminant_for<S: States + Copy>(value: &S) -> Option<u32> {
    let map = state_tables().read().ok()?;
    let entry = map.get(&TypeId::of::<S>())?;
    let list = entry.downcast_ref::<Vec<(u32, String, S)>>()?;
    list.iter()
        .find(|(_, _, variant)| variant == value)
        .map(|(disc, _, _)| *disc)
}

fn lookup_variant_by_slot<S: States + Copy>(slot: usize) -> Option<S> {
    let map = state_tables().read().ok()?;
    let entry = map.get(&TypeId::of::<S>())?;
    let list = entry.downcast_ref::<Vec<(u32, String, S)>>()?;
    list.get(slot).map(|(_, _, variant)| *variant)
}

type AttachDespawnFn = Arc<dyn Fn(&mut World, Entity, u32) -> bool + Send + Sync>;
type RequestTransitionFn = Arc<dyn Fn(&mut World, u32) -> FfiResult + Send + Sync>;

struct StateSlabEntry {
    type_id: TypeId,
    type_name: &'static str,
    kind: ModStateKind,
    base_schedule_id: ScheduleId,
    state_handle: BamComponent,
    state_resource_name: &'static str,
    next_state_handle: Option<BamComponent>,
    next_state_resource_name: Option<&'static str>,
    entry_entity: Entity,
    current_discriminant: Option<u32>,
    variants: Vec<(u32, String)>,
    attach_despawn_on_exit: AttachDespawnFn,
    attach_despawn_on_enter: AttachDespawnFn,
    request_transition: Option<RequestTransitionFn>,
}

struct SlabSlot {
    generation: u32,
    entry: Option<StateSlabEntry>,
}

/// Dynamic, generation-indexed slab storing metadata, live discriminants, transition dispatchers,
/// [`ModStateEntry`] entities, and state-scoped despawn hooks for an unbounded number of exposed
/// Bevy [`States`], [`SubStates`], and [`ComputedStates`].
#[derive(Resource, Default)]
pub struct ModStateSlab {
    slots: Vec<SlabSlot>,
    free_list: Vec<usize>,
    by_type: HashMap<TypeId, ModStateHandle>,
}

impl ModStateSlab {
    /// Creates an empty [`ModStateSlab`].
    #[inline]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the number of currently active exposed state entries.
    #[inline]
    pub fn len(&self) -> usize {
        self.by_type.len()
    }

    /// Returns `true` if no states are currently exposed.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.by_type.is_empty()
    }

    /// Returns the total allocated slot capacity (`Vec` length).
    #[inline]
    pub fn slot_capacity(&self) -> usize {
        self.slots.len()
    }

    /// Looks up the active [`ModStateHandle`] for state type `S`, if registered.
    #[inline]
    pub fn handle_of<S: 'static>(&self) -> Option<ModStateHandle> {
        self.by_type.get(&TypeId::of::<S>()).copied()
    }

    /// Returns `true` if `handle` refers to a live slot whose generation matches.
    #[inline]
    pub fn is_alive(&self, handle: ModStateHandle) -> bool {
        self.get_entry(handle).is_some()
    }

    fn get_entry(&self, handle: ModStateHandle) -> Option<&StateSlabEntry> {
        if !handle.is_valid() {
            return None;
        }
        let slot = self.slots.get(handle.slot_index as usize)?;
        if slot.generation != handle.generation {
            return None;
        }
        slot.entry.as_ref()
    }

    fn get_entry_mut(&mut self, handle: ModStateHandle) -> Option<&mut StateSlabEntry> {
        if !handle.is_valid() {
            return None;
        }
        let slot = self.slots.get_mut(handle.slot_index as usize)?;
        if slot.generation != handle.generation {
            return None;
        }
        slot.entry.as_mut()
    }

    fn allocate_slot_for<S: 'static>(&mut self) -> ModStateHandle {
        if let Some(&existing) = self.by_type.get(&TypeId::of::<S>()) {
            return existing;
        }
        if let Some(idx) = self.free_list.pop() {
            let gen_val = self.slots[idx].generation;
            let handle = ModStateHandle::new(idx as u32, gen_val);
            self.by_type.insert(TypeId::of::<S>(), handle);
            handle
        } else {
            let idx = u32::try_from(self.slots.len()).expect("ModStateSlab exceeded u32::MAX");
            self.slots.push(SlabSlot {
                generation: 1,
                entry: None,
            });
            let handle = ModStateHandle::new(idx, 1);
            self.by_type.insert(TypeId::of::<S>(), handle);
            handle
        }
    }

    fn install_entry(&mut self, handle: ModStateHandle, entry: StateSlabEntry) -> Option<Entity> {
        if let Some(slot) = self.slots.get_mut(handle.slot_index as usize)
            && slot.generation == handle.generation
        {
            let prev_entity = slot.entry.as_ref().map(|e| e.entry_entity);
            slot.entry = Some(entry);
            return prev_entity;
        }
        None
    }

    fn update_current_discriminant(
        &mut self,
        handle: ModStateHandle,
        current: Option<u32>,
    ) -> Option<(Entity, ModStateKind)> {
        let entry = self.get_entry_mut(handle)?;
        entry.current_discriminant = current;
        Some((entry.entry_entity, entry.kind))
    }

    /// Releases `handle`'s slot in this slab, advancing its generation counter so any stale copies
    /// of `handle` are rejected and returning the slot index to the dynamic free list for reuse.
    ///
    /// Returns the despawnable [`ModStateEntry`] entity (if `handle` was live), or `None` if
    /// already stale/invalid. Prefer [`Self::release_in_world`] when you have `&mut World` access.
    pub fn release(&mut self, handle: ModStateHandle) -> Option<Entity> {
        if !handle.is_valid() {
            return None;
        }
        let idx = handle.slot_index as usize;
        let slot = self.slots.get_mut(idx)?;
        if slot.generation != handle.generation {
            return None;
        }
        let removed = slot.entry.take()?;
        if self.by_type.get(&removed.type_id) == Some(&handle) {
            self.by_type.remove(&removed.type_id);
        }
        slot.generation = slot.generation.wrapping_add(1).max(1);
        self.free_list.push(idx);
        Some(removed.entry_entity)
    }

    /// Releases `handle` inside `world`, bumping its slab generation and despawning its
    /// [`ModStateEntry`] mirror entity.
    pub fn release_in_world(world: &mut World, handle: ModStateHandle) -> bool {
        let entity = world
            .get_resource_mut::<Self>()
            .and_then(|mut slab| slab.release(handle));
        if let Some(ent) = entity {
            if let Ok(ent_mut) = world.get_entity_mut(ent) {
                ent_mut.despawn();
            }
            true
        } else {
            false
        }
    }

    /// Returns the [`ModStateKind`] for a live `handle`.
    #[inline]
    pub fn kind(&self, handle: ModStateHandle) -> Option<ModStateKind> {
        self.get_entry(handle).map(|e| e.kind)
    }

    /// Returns the Rust `core::any::type_name::<S>()` recorded for a live `handle`.
    #[inline]
    pub fn type_name(&self, handle: ModStateHandle) -> Option<&'static str> {
        self.get_entry(handle).map(|e| e.type_name)
    }

    /// Returns the base custom [`ScheduleId`] for a live `handle`.
    #[inline]
    pub fn base_schedule_id(&self, handle: ModStateHandle) -> Option<ScheduleId> {
        self.get_entry(handle).map(|e| e.base_schedule_id)
    }

    /// Returns the [`Entity`] holding the live read-only [`ModStateEntry`] for `handle`.
    #[inline]
    pub fn entry_entity(&self, handle: ModStateHandle) -> Option<Entity> {
        self.get_entry(handle).map(|e| e.entry_entity)
    }

    /// Returns the [`BamComponent`] handle for reading this state (`State#0` on primary slot 0,
    /// or [`ModStateEntry`] on dynamic slots).
    #[inline]
    pub fn state_handle(&self, handle: ModStateHandle) -> Option<BamComponent> {
        self.get_entry(handle).map(|e| e.state_handle)
    }

    /// Returns the canonical registry name for reading this state.
    #[inline]
    pub fn state_resource_name(&self, handle: ModStateHandle) -> Option<&'static str> {
        self.get_entry(handle).map(|e| e.state_resource_name)
    }

    /// Returns the [`BamComponent`] handle for requesting transitions, or `None` for read-only
    /// [`ComputedStates`] or stale handles.
    #[inline]
    pub fn next_state_handle(&self, handle: ModStateHandle) -> Option<BamComponent> {
        self.get_entry(handle).and_then(|e| e.next_state_handle)
    }

    /// Returns the canonical registry name for requesting transitions, or `None` for read-only
    /// [`ComputedStates`] or stale handles.
    #[inline]
    pub fn next_state_resource_name(&self, handle: ModStateHandle) -> Option<&'static str> {
        self.get_entry(handle)
            .and_then(|e| e.next_state_resource_name)
    }

    /// Returns the most recently synced discriminant for a live `handle`:
    /// - `None` if `handle` is invalid or stale,
    /// - `Some(None)` if `handle` is a live [`SubStates`] or [`ComputedStates`] that is currently inactive,
    /// - `Some(Some(disc))` if the state currently holds variant `disc`.
    #[inline]
    pub fn current_discriminant(&self, handle: ModStateHandle) -> Option<Option<u32>> {
        self.get_entry(handle).map(|e| e.current_discriminant)
    }

    /// Returns the registered `(discriminant, variant_name)` list for a live `handle`.
    #[inline]
    pub fn variants(&self, handle: ModStateHandle) -> Option<&[(u32, String)]> {
        self.get_entry(handle).map(|e| e.variants.as_slice())
    }

    /// Returns the number of registered variants for a live `handle`.
    #[inline]
    pub fn variant_count(&self, handle: ModStateHandle) -> Option<usize> {
        self.get_entry(handle).map(|e| e.variants.len())
    }

    fn despawn_on_exit_fn(&self, handle: ModStateHandle) -> Option<AttachDespawnFn> {
        self.get_entry(handle)
            .map(|e| Arc::clone(&e.attach_despawn_on_exit))
    }

    fn despawn_on_enter_fn(&self, handle: ModStateHandle) -> Option<AttachDespawnFn> {
        self.get_entry(handle)
            .map(|e| Arc::clone(&e.attach_despawn_on_enter))
    }

    /// Attaches Bevy's `DespawnOnExit(variant)` to `entity` for `(handle, discriminant)` inside `world`.
    pub fn attach_despawn_on_exit(
        world: &mut World,
        handle: ModStateHandle,
        entity: Entity,
        discriminant: u32,
    ) -> bool {
        let attach = world
            .get_resource::<Self>()
            .and_then(|slab| slab.despawn_on_exit_fn(handle));
        attach.is_some_and(|f| f(world, entity, discriminant))
    }

    /// Attaches Bevy's `DespawnOnEnter(variant)` to `entity` for `(handle, discriminant)` inside `world`.
    pub fn attach_despawn_on_enter(
        world: &mut World,
        handle: ModStateHandle,
        entity: Entity,
        discriminant: u32,
    ) -> bool {
        let attach = world
            .get_resource::<Self>()
            .and_then(|slab| slab.despawn_on_enter_fn(handle));
        attach.is_some_and(|f| f(world, entity, discriminant))
    }

    /// Requests a state transition on `handle` toward `discriminant` inside `world`.
    ///
    /// - Returns [`FfiResult::NOT_FOUND`] if `handle` is invalid or stale.
    /// - Returns [`FfiResult::PERMISSION_DENIED`] if `handle` refers to a read-only [`ComputedStates`].
    /// - Returns [`FfiResult::INVALID_ARGUMENT`] if `discriminant` is not registered for that state.
    /// - Returns [`FfiResult::OK`] once queued into [`ModNextState<S>`].
    pub fn request_transition(
        world: &mut World,
        handle: ModStateHandle,
        discriminant: u32,
    ) -> FfiResult {
        let action = {
            let Some(slab) = world.get_resource::<Self>() else {
                return FfiResult::NOT_FOUND;
            };
            let Some(entry) = slab.get_entry(handle) else {
                return FfiResult::NOT_FOUND;
            };
            match &entry.request_transition {
                Some(f) => Ok(Arc::clone(f)),
                None => Err(FfiResult::PERMISSION_DENIED),
            }
        };
        match action {
            Ok(f) => f(world, discriminant),
            Err(err) => err,
        }
    }
}

/// Host resource bridging guest `u32` state transition requests into Bevy's
/// [`NextState::<S>::set_if_different`].
#[derive(Resource, Debug, Clone)]
pub struct ModNextState<S: FreelyMutableState + Copy> {
    current: Option<u32>,
    pending: Option<u32>,
    variants: Vec<(u32, S)>,
}

impl<S: FreelyMutableState + Copy> ModNextState<S> {
    fn new(initial: Option<u32>, variants: Vec<(u32, S)>) -> Self {
        Self {
            current: initial,
            pending: None,
            variants,
        }
    }

    /// Returns the most recently synced state discriminant, or [`STATE_ABSENT_DISCRIMINANT`]
    /// (`u32::MAX`) if a [`SubStates`] is currently inactive.
    #[inline]
    pub const fn current_discriminant(&self) -> u32 {
        match self.current {
            Some(d) => d,
            None => STATE_ABSENT_DISCRIMINANT,
        }
    }

    /// Returns the active state discriminant if `State<S>` currently exists in the `World`.
    #[inline]
    pub const fn active_discriminant(&self) -> Option<u32> {
        self.current
    }

    /// Returns the pending requested discriminant, if a mod queued a transition this tick.
    #[inline]
    pub const fn pending_discriminant(&self) -> Option<u32> {
        self.pending
    }

    /// Queues a transition request toward `discriminant`, validating membership in `variants`.
    #[inline]
    pub fn queue_discriminant(&mut self, discriminant: u32) -> FfiResult {
        if self.variants.iter().any(|(d, _)| *d == discriminant) {
            self.pending = Some(discriminant);
            FfiResult::OK
        } else {
            FfiResult::INVALID_ARGUMENT
        }
    }
}

#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct ModStateSyncSet;

fn drain_mod_state_transition_requests(
    world: &mut World,
    mut cursor: Local<MessageCursor<ModStateTransitionRequest>>,
) {
    let requests: Vec<ModStateTransitionRequest> = {
        let Some(messages) = world.get_resource::<Messages<ModStateTransitionRequest>>() else {
            return;
        };
        cursor.read(messages).copied().collect()
    };
    for req in requests {
        let _ =
            ModStateSlab::request_transition(world, req.state_handle(), req.target_discriminant);
    }
}

fn update_entry_entity(
    entries: &mut Query<&mut ModStateEntry>,
    entity: Entity,
    handle: ModStateHandle,
    kind: ModStateKind,
    active_disc: Option<u32>,
) {
    if let Ok(mut entry) = entries.get_mut(entity) {
        let desired = ModStateEntry::new(handle, kind, active_disc);
        if *entry != desired {
            *entry = desired;
        }
    }
}

fn sync_mod_next_state<S: FreelyMutableState + Copy>(
    state: Option<Res<State<S>>>,
    mut mod_next: ResMut<ModNextState<S>>,
    mut next_state: Option<ResMut<NextState<S>>>,
    mut slab: Option<ResMut<ModStateSlab>>,
    mut entries: Query<&mut ModStateEntry>,
) {
    let active_disc = state.as_deref().and_then(|s| {
        mod_next
            .variants
            .iter()
            .find(|(_, v)| v == s.get())
            .map(|(d, _)| *d)
    });
    mod_next.current = active_disc;
    if let Some(slab) = slab.as_deref_mut()
        && let Some(handle) = slab.handle_of::<S>()
        && let Some((entity, kind)) = slab.update_current_discriminant(handle, active_disc)
    {
        update_entry_entity(&mut entries, entity, handle, kind, active_disc);
    }
    if let Some(next_state) = next_state.as_deref_mut()
        && let Some(disc) = mod_next.pending.take()
        && let Some((_, variant)) = mod_next.variants.iter().find(|(d, _)| *d == disc)
    {
        next_state.set_if_different(*variant);
    }
}

fn sync_mod_state_slab<S: States + Copy>(
    state: Option<Res<State<S>>>,
    mut slab: Option<ResMut<ModStateSlab>>,
    mut entries: Query<&mut ModStateEntry>,
) {
    let active_disc = state
        .as_deref()
        .and_then(|s| lookup_discriminant_for(s.get()));
    if let Some(slab) = slab.as_deref_mut()
        && let Some(handle) = slab.handle_of::<S>()
        && let Some((entity, kind)) = slab.update_current_discriminant(handle, active_disc)
    {
        update_entry_entity(&mut entries, entity, handle, kind, active_disc);
    }
}

fn sync_mutable_state_post_transition<S: FreelyMutableState + Copy>(
    state: Option<Res<State<S>>>,
    mut mod_next: Option<ResMut<ModNextState<S>>>,
    mut slab: Option<ResMut<ModStateSlab>>,
    mut entries: Query<&mut ModStateEntry>,
) {
    let active_disc = state
        .as_deref()
        .and_then(|s| lookup_discriminant_for(s.get()));
    if let Some(mod_next) = mod_next.as_deref_mut() {
        mod_next.current = active_disc;
    }
    if let Some(slab) = slab.as_deref_mut()
        && let Some(handle) = slab.handle_of::<S>()
        && let Some((entity, kind)) = slab.update_current_discriminant(handle, active_disc)
    {
        update_entry_entity(&mut entries, entity, handle, kind, active_disc);
    }
}

#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct ModInStateSet<S: States + Copy> {
    discriminant: u32,
    _marker: PhantomData<fn() -> S>,
}

impl<S: States + Copy> ModInStateSet<S> {
    const fn new(discriminant: u32) -> Self {
        Self {
            discriminant,
            _marker: PhantomData,
        }
    }
}

struct PrimaryStateWireBridge<S>(PhantomData<fn() -> S>);
struct PrimaryNextStateWireBridge<S>(PhantomData<fn() -> S>);

impl<S: States + Copy> HostWireBridge<State<S>> for PrimaryStateWireBridge<S> {
    type Wire = u32;
    const CANONICAL_NAME: &'static str = "bevy_state::State#0";
    const ALIAS: Option<&'static str> = Some("State");
    const SUPPORTS_SET: bool = false;

    #[inline]
    fn to_wire(value: &State<S>) -> u32 {
        lookup_discriminant_for(value.get()).unwrap_or(STATE_ABSENT_DISCRIMINANT)
    }
}

impl<S: FreelyMutableState + Copy> HostWireBridge<ModNextState<S>>
    for PrimaryNextStateWireBridge<S>
{
    type Wire = u32;
    const CANONICAL_NAME: &'static str = "bevy_abi_mod::ModNextState#0";
    const ALIAS: Option<&'static str> = Some("ModNextState");
    const SUPPORTS_SET: bool = true;

    #[inline]
    fn to_wire(value: &ModNextState<S>) -> u32 {
        value
            .pending
            .or(value.current)
            .unwrap_or(STATE_ABSENT_DISCRIMINANT)
    }

    #[inline]
    fn apply_wire(value: &mut ModNextState<S>, wire: u32) -> FfiResult {
        value.queue_discriminant(wire)
    }
}

fn register_readonly_state_binding<S: States + Copy>(
    world: &mut World,
    handle: ModStateHandle,
    dynamic_entry_handle: BamComponent,
) -> Result<(BamComponent, &'static str), ComponentCollisionError> {
    let state_comp_id = world.register_component::<State<S>>();
    let mut reg = world.resource_mut::<ModRegistry>();
    let use_primary = handle.slot_index == 0
        && handle.generation == 1
        && reg
            .get_by_name(PrimaryStateWireBridge::<S>::CANONICAL_NAME)
            .is_none_or(|e| e.component_id == state_comp_id);
    if use_primary {
        let sh =
            reg.register_bridged_resource::<State<S>, PrimaryStateWireBridge<S>>(state_comp_id)?;
        Ok((sh, PrimaryStateWireBridge::<S>::CANONICAL_NAME))
    } else {
        Ok((dynamic_entry_handle, CANONICAL_MOD_STATE_ENTRY_NAME))
    }
}

fn register_mutable_state_bindings<S: FreelyMutableState + Copy>(
    world: &mut World,
    handle: ModStateHandle,
    dynamic_entry_handle: BamComponent,
    dynamic_request_handle: BamComponent,
) -> Result<(BamComponent, &'static str, BamComponent, &'static str), ComponentCollisionError> {
    let state_comp_id = world.register_component::<State<S>>();
    let next_comp_id = world.register_component::<ModNextState<S>>();
    let mut reg = world.resource_mut::<ModRegistry>();
    let use_primary = handle.slot_index == 0
        && handle.generation == 1
        && reg
            .get_by_name(PrimaryStateWireBridge::<S>::CANONICAL_NAME)
            .is_none_or(|e| e.component_id == state_comp_id)
        && reg
            .get_by_name(PrimaryNextStateWireBridge::<S>::CANONICAL_NAME)
            .is_none_or(|e| e.component_id == next_comp_id);
    if use_primary {
        let sh =
            reg.register_bridged_resource::<State<S>, PrimaryStateWireBridge<S>>(state_comp_id)?;
        let nh = reg.register_bridged_resource::<ModNextState<S>, PrimaryNextStateWireBridge<S>>(
            next_comp_id,
        )?;
        Ok((
            sh,
            PrimaryStateWireBridge::<S>::CANONICAL_NAME,
            nh,
            PrimaryNextStateWireBridge::<S>::CANONICAL_NAME,
        ))
    } else {
        Ok((
            dynamic_entry_handle,
            CANONICAL_MOD_STATE_ENTRY_NAME,
            dynamic_request_handle,
            CANONICAL_MOD_STATE_TRANSITION_REQUEST_NAME,
        ))
    }
}

// ============================================================================
// Primary Slot-0 Backwards-Compatibility Markers (`GuestStateHandles::resolve_primary`)
// ============================================================================

#[derive(Component, Default)]
struct PrimaryDespawnOnExitSlot<S: States + Copy, const VAR_SLOT: usize>(PhantomData<fn() -> S>);

#[derive(Component, Default)]
struct PrimaryDespawnOnEnterSlot<S: States + Copy, const VAR_SLOT: usize>(PhantomData<fn() -> S>);

fn insert_primary_despawn_on_exit_slot<S: States + Copy, const VAR_SLOT: usize>(
    world: &mut World,
    entity: Entity,
) {
    if let Some(variant) = lookup_variant_by_slot::<S>(VAR_SLOT)
        && let Ok(mut e) = world.get_entity_mut(entity)
    {
        e.insert(DespawnOnExit(variant));
    }
}

fn insert_primary_despawn_on_enter_slot<S: States + Copy, const VAR_SLOT: usize>(
    world: &mut World,
    entity: Entity,
) {
    if let Some(variant) = lookup_variant_by_slot::<S>(VAR_SLOT)
        && let Ok(mut e) = world.get_entity_mut(entity)
    {
        e.insert(DespawnOnEnter(variant));
    }
}

struct PrimaryExitMarkerBridge<S, const VAR_SLOT: usize>(PhantomData<fn() -> S>);
struct PrimaryEnterMarkerBridge<S, const VAR_SLOT: usize>(PhantomData<fn() -> S>);

macro_rules! impl_primary_legacy_markers {
    ($($v:literal => ($exit_name:literal, $enter_name:literal)),+ $(,)?) => {
        $(
            impl<S: States + Copy> HostWireBridge<PrimaryDespawnOnExitSlot<S, $v>>
                for PrimaryExitMarkerBridge<S, $v>
            {
                type Wire = u8;
                const CANONICAL_NAME: &'static str = $exit_name;
                const SUPPORTS_SET: bool = false;

                fn to_wire(_value: &PrimaryDespawnOnExitSlot<S, $v>) -> u8 {
                    0
                }
            }

            impl<S: States + Copy> HostWireBridge<PrimaryDespawnOnEnterSlot<S, $v>>
                for PrimaryEnterMarkerBridge<S, $v>
            {
                type Wire = u8;
                const CANONICAL_NAME: &'static str = $enter_name;
                const SUPPORTS_SET: bool = false;

                fn to_wire(_value: &PrimaryDespawnOnEnterSlot<S, $v>) -> u8 {
                    0
                }
            }
        )+

        fn register_primary_exit_marker<S: States + Copy>(
            world: &mut World,
            var_slot: usize,
        ) -> Result<Option<(BamComponent, &'static str)>, ComponentCollisionError> {
            match var_slot {
                $(
                    $v => {
                        let comp_id = world.register_component::<PrimaryDespawnOnExitSlot<S, $v>>();
                        let mut reg = world.resource_mut::<ModRegistry>();
                        if reg
                            .get_by_name($exit_name)
                            .is_some_and(|e| e.component_id != comp_id)
                        {
                            return Ok(None);
                        }
                        let handle = reg.register_bridged_component::<
                            PrimaryDespawnOnExitSlot<S, $v>,
                            PrimaryExitMarkerBridge<S, $v>,
                        >(
                            comp_id,
                            0,
                            Some(insert_primary_despawn_on_exit_slot::<S, $v>),
                        )?;
                        Ok(Some((handle, $exit_name)))
                    }
                )+
                _ => Ok(None),
            }
        }

        fn register_primary_enter_marker<S: States + Copy>(
            world: &mut World,
            var_slot: usize,
        ) -> Result<Option<(BamComponent, &'static str)>, ComponentCollisionError> {
            match var_slot {
                $(
                    $v => {
                        let comp_id = world.register_component::<PrimaryDespawnOnEnterSlot<S, $v>>();
                        let mut reg = world.resource_mut::<ModRegistry>();
                        if reg
                            .get_by_name($enter_name)
                            .is_some_and(|e| e.component_id != comp_id)
                        {
                            return Ok(None);
                        }
                        let handle = reg.register_bridged_component::<
                            PrimaryDespawnOnEnterSlot<S, $v>,
                            PrimaryEnterMarkerBridge<S, $v>,
                        >(
                            comp_id,
                            0,
                            Some(insert_primary_despawn_on_enter_slot::<S, $v>),
                        )?;
                        Ok(Some((handle, $enter_name)))
                    }
                )+
                _ => Ok(None),
            }
        }
    };
}

impl_primary_legacy_markers!(
    0 => ("bevy_state::DespawnOnExit#0::0", "bevy_state::DespawnOnEnter#0::0"),
    1 => ("bevy_state::DespawnOnExit#0::1", "bevy_state::DespawnOnEnter#0::1"),
    2 => ("bevy_state::DespawnOnExit#0::2", "bevy_state::DespawnOnEnter#0::2"),
    3 => ("bevy_state::DespawnOnExit#0::3", "bevy_state::DespawnOnEnter#0::3"),
    4 => ("bevy_state::DespawnOnExit#0::4", "bevy_state::DespawnOnEnter#0::4"),
    5 => ("bevy_state::DespawnOnExit#0::5", "bevy_state::DespawnOnEnter#0::5"),
    6 => ("bevy_state::DespawnOnExit#0::6", "bevy_state::DespawnOnEnter#0::6"),
    7 => ("bevy_state::DespawnOnExit#0::7", "bevy_state::DespawnOnEnter#0::7"),
    8 => ("bevy_state::DespawnOnExit#0::8", "bevy_state::DespawnOnEnter#0::8"),
    9 => ("bevy_state::DespawnOnExit#0::9", "bevy_state::DespawnOnEnter#0::9"),
    10 => ("bevy_state::DespawnOnExit#0::10", "bevy_state::DespawnOnEnter#0::10"),
    11 => ("bevy_state::DespawnOnExit#0::11", "bevy_state::DespawnOnEnter#0::11"),
    12 => ("bevy_state::DespawnOnExit#0::12", "bevy_state::DespawnOnEnter#0::12"),
    13 => ("bevy_state::DespawnOnExit#0::13", "bevy_state::DespawnOnEnter#0::13"),
    14 => ("bevy_state::DespawnOnExit#0::14", "bevy_state::DespawnOnEnter#0::14"),
    15 => ("bevy_state::DespawnOnExit#0::15", "bevy_state::DespawnOnEnter#0::15"),
    16 => ("bevy_state::DespawnOnExit#0::16", "bevy_state::DespawnOnEnter#0::16"),
    17 => ("bevy_state::DespawnOnExit#0::17", "bevy_state::DespawnOnEnter#0::17"),
    18 => ("bevy_state::DespawnOnExit#0::18", "bevy_state::DespawnOnEnter#0::18"),
    19 => ("bevy_state::DespawnOnExit#0::19", "bevy_state::DespawnOnEnter#0::19"),
    20 => ("bevy_state::DespawnOnExit#0::20", "bevy_state::DespawnOnEnter#0::20"),
    21 => ("bevy_state::DespawnOnExit#0::21", "bevy_state::DespawnOnEnter#0::21"),
    22 => ("bevy_state::DespawnOnExit#0::22", "bevy_state::DespawnOnEnter#0::22"),
    23 => ("bevy_state::DespawnOnExit#0::23", "bevy_state::DespawnOnEnter#0::23"),
    24 => ("bevy_state::DespawnOnExit#0::24", "bevy_state::DespawnOnEnter#0::24"),
    25 => ("bevy_state::DespawnOnExit#0::25", "bevy_state::DespawnOnEnter#0::25"),
    26 => ("bevy_state::DespawnOnExit#0::26", "bevy_state::DespawnOnEnter#0::26"),
    27 => ("bevy_state::DespawnOnExit#0::27", "bevy_state::DespawnOnEnter#0::27"),
    28 => ("bevy_state::DespawnOnExit#0::28", "bevy_state::DespawnOnEnter#0::28"),
    29 => ("bevy_state::DespawnOnExit#0::29", "bevy_state::DespawnOnEnter#0::29"),
    30 => ("bevy_state::DespawnOnExit#0::30", "bevy_state::DespawnOnEnter#0::30"),
    31 => ("bevy_state::DespawnOnExit#0::31", "bevy_state::DespawnOnEnter#0::31"),
);

// ============================================================================
// Dynamic State Component/Message/Observer Infrastructure
// ============================================================================

#[derive(Resource, Clone, Copy)]
struct StateDynamicBridgesInstalled {
    entry_handle: BamComponent,
    request_handle: BamComponent,
    exit_handle: BamComponent,
    enter_handle: BamComponent,
}

fn on_insert_mod_despawn_on_exit(
    trigger: On<Insert<ModDespawnOnExit>>,
    query: Query<&ModDespawnOnExit, Allow<Disabled>>,
    mut commands: Commands,
) {
    let entity = trigger.entity;
    if let Ok(&marker) = query.get(entity) {
        commands.queue(move |world: &mut World| {
            let _ = ModStateSlab::attach_despawn_on_exit(
                world,
                marker.state_handle(),
                entity,
                marker.discriminant,
            );
        });
    }
}

fn on_insert_mod_despawn_on_enter(
    trigger: On<Insert<ModDespawnOnEnter>>,
    query: Query<&ModDespawnOnEnter, Allow<Disabled>>,
    mut commands: Commands,
) {
    let entity = trigger.entity;
    if let Ok(&marker) = query.get(entity) {
        commands.queue(move |world: &mut World| {
            let _ = ModStateSlab::attach_despawn_on_enter(
                world,
                marker.state_handle(),
                entity,
                marker.discriminant,
            );
        });
    }
}

fn ensure_dynamic_state_bridges(
    app: &mut App,
) -> Result<StateDynamicBridgesInstalled, ComponentCollisionError> {
    if let Some(&installed) = app.world().get_resource::<StateDynamicBridgesInstalled>() {
        return Ok(installed);
    }
    let entry_handle = app
        .world_mut()
        .register_mod_component::<ModStateEntry>(ComponentOptions::new().immutable())?;
    let request_handle = app
        .world_mut()
        .register_mod_message::<ModStateTransitionRequest>()?;
    let exit_handle = app.world_mut().register_mod_component::<ModDespawnOnExit>(
        ComponentOptions::new().with_raw_insert().with_default(),
    )?;
    let enter_handle = app
        .world_mut()
        .register_mod_component::<ModDespawnOnEnter>(
            ComponentOptions::new().with_raw_insert().with_default(),
        )?;
    app.add_observer(on_insert_mod_despawn_on_exit);
    app.add_observer(on_insert_mod_despawn_on_enter);
    app.add_systems(
        PreUpdate,
        drain_mod_state_transition_requests.before(ModStateSyncSet),
    );
    app.add_systems(
        PostUpdate,
        drain_mod_state_transition_requests.before(ModStateSyncSet),
    );
    let installed = StateDynamicBridgesInstalled {
        entry_handle,
        request_handle,
        exit_handle,
        enter_handle,
    };
    app.insert_resource(installed);
    Ok(installed)
}

/// Metadata descriptor for an exposed Bevy state enum `S` (`FreelyMutableState`, `SubStates`, or
/// `ComputedStates`), stored as a [`Resource`] and returned by [`ModStateExt`].
#[derive(Resource, Clone, Debug)]
pub struct ModStateSpec<S: States + Copy> {
    handle: ModStateHandle,
    kind: ModStateKind,
    base_schedule_id: ScheduleId,
    state_handle: BamComponent,
    state_resource_name: &'static str,
    next_state_handle: Option<BamComponent>,
    next_state_resource_name: Option<&'static str>,
    state_entry_entity: Entity,
    mod_state_entry_handle: BamComponent,
    mod_transition_request_handle: Option<BamComponent>,
    mod_despawn_on_exit_handle: BamComponent,
    mod_despawn_on_enter_handle: BamComponent,
    variants: Vec<(u32, String, S)>,
    legacy_despawn_on_exit: Vec<Option<(BamComponent, &'static str)>>,
    legacy_despawn_on_enter: Vec<Option<(BamComponent, &'static str)>>,
}

impl<S: States + Copy> ModStateSpec<S> {
    /// Returns the generation-indexed [`ModStateHandle`] (`(slot_index, generation)`) assigned to
    /// `S` in [`ModStateSlab`].
    #[inline]
    pub const fn handle(&self) -> ModStateHandle {
        self.handle
    }

    /// Returns the deterministic 0-based slab slot index assigned to `S` in this `World`.
    #[inline]
    pub const fn state_slot(&self) -> usize {
        self.handle.slot_index as usize
    }

    /// Returns the non-zero slab generation counter for this exposed state handle.
    #[inline]
    pub const fn generation(&self) -> u32 {
        self.handle.generation
    }

    /// Returns the [`ModStateKind`] (`FreelyMutable`, `SubState`, or `Computed`).
    #[inline]
    pub const fn kind(&self) -> ModStateKind {
        self.kind
    }

    /// Returns `true` if mods may request transitions on this state (`false` for [`ComputedStates`]).
    #[inline]
    pub const fn is_mutable(&self) -> bool {
        self.kind.is_mutable()
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

    /// Returns the [`Entity`] holding this state's live read-only [`ModStateEntry`] component.
    #[inline]
    pub const fn state_entry_entity(&self) -> Entity {
        self.state_entry_entity
    }

    /// Returns the [`BamComponent`] handle for the universal read-only [`ModStateEntry`] component.
    #[inline]
    pub const fn mod_state_entry_handle(&self) -> BamComponent {
        self.mod_state_entry_handle
    }

    /// Returns the [`BamComponent`] handle for the universal [`ModStateTransitionRequest`] message
    /// stream, or `None` for read-only [`ComputedStates`].
    #[inline]
    pub const fn mod_transition_request_handle(&self) -> Option<BamComponent> {
        self.mod_transition_request_handle
    }

    /// Returns the [`BamComponent`] handle for reading this state (`State#0` on primary slot 0,
    /// or [`ModStateEntry`] on dynamic slots).
    #[inline]
    pub const fn state_handle(&self) -> BamComponent {
        self.state_handle
    }

    /// Returns the canonical registry name for reading this state (e.g. `"bevy_state::State#0"`
    /// on primary slot 0, or [`CANONICAL_MOD_STATE_ENTRY_NAME`] on dynamic slots).
    #[inline]
    pub const fn state_resource_name(&self) -> &'static str {
        self.state_resource_name
    }

    /// Returns the [`BamComponent`] handle for requesting transitions (`ModNextState#0` on primary
    /// slot 0, or [`ModStateTransitionRequest`] on dynamic slots), or `None` for read-only
    /// [`ComputedStates`].
    #[inline]
    pub const fn next_state_handle(&self) -> Option<BamComponent> {
        self.next_state_handle
    }

    /// Returns the canonical registry name for requesting transitions, or `None` for read-only
    /// [`ComputedStates`].
    #[inline]
    pub const fn next_state_resource_name(&self) -> Option<&'static str> {
        self.next_state_resource_name
    }

    /// Returns the [`BamComponent`] handle for the universal dynamic [`ModDespawnOnExit`] component.
    #[inline]
    pub const fn mod_despawn_on_exit_handle(&self) -> BamComponent {
        self.mod_despawn_on_exit_handle
    }

    /// Returns the [`BamComponent`] handle for the universal dynamic [`ModDespawnOnEnter`] component.
    #[inline]
    pub const fn mod_despawn_on_enter_handle(&self) -> BamComponent {
        self.mod_despawn_on_enter_handle
    }

    /// Builds a dynamic [`ModDespawnOnExit`] marker component for `discriminant`, if registered.
    pub fn despawn_on_exit_marker(&self, discriminant: u32) -> Option<ModDespawnOnExit> {
        self.variants.iter().find(|(d, _, _)| *d == discriminant)?;
        Some(ModDespawnOnExit::new(self.handle, discriminant))
    }

    /// Builds a dynamic [`ModDespawnOnEnter`] marker component for `discriminant`, if registered.
    pub fn despawn_on_enter_marker(&self, discriminant: u32) -> Option<ModDespawnOnEnter> {
        self.variants.iter().find(|(d, _, _)| *d == discriminant)?;
        Some(ModDespawnOnEnter::new(self.handle, discriminant))
    }

    /// Builds a dynamic [`ModStateTransitionRequest`] targeting `discriminant`, or `None` if this
    /// state is read-only ([`ComputedStates`]) or `discriminant` is not registered.
    pub fn transition_request(&self, discriminant: u32) -> Option<ModStateTransitionRequest> {
        if !self.is_mutable() {
            return None;
        }
        self.variants.iter().find(|(d, _, _)| *d == discriminant)?;
        Some(ModStateTransitionRequest::new(self.handle, discriminant))
    }

    /// Directly attaches Bevy's `DespawnOnExit(variant)` to `entity` for `discriminant`.
    pub fn attach_despawn_on_exit(
        &self,
        world: &mut World,
        entity: Entity,
        discriminant: u32,
    ) -> bool {
        let Some((_, _, variant)) = self.variants.iter().find(|(d, _, _)| *d == discriminant)
        else {
            return false;
        };
        if let Ok(mut ent) = world.get_entity_mut(entity) {
            ent.insert(DespawnOnExit(*variant));
            true
        } else {
            false
        }
    }

    /// Directly attaches Bevy's `DespawnOnEnter(variant)` to `entity` for `discriminant`.
    pub fn attach_despawn_on_enter(
        &self,
        world: &mut World,
        entity: Entity,
        discriminant: u32,
    ) -> bool {
        let Some((_, _, variant)) = self.variants.iter().find(|(d, _, _)| *d == discriminant)
        else {
            return false;
        };
        if let Ok(mut ent) = world.get_entity_mut(entity) {
            ent.insert(DespawnOnEnter(*variant));
            true
        } else {
            false
        }
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

    /// Returns the primary-slot legacy default-insertable `DespawnOnExit` marker [`BamComponent`]
    /// handle for `discriminant` (when registered on primary slot 0); use
    /// [`Self::mod_despawn_on_exit_handle`] + [`Self::despawn_on_exit_marker`] for arbitrary
    /// dynamic states and variant counts.
    pub fn despawn_on_exit_handle(&self, discriminant: u32) -> Option<BamComponent> {
        let idx = self
            .variants
            .iter()
            .position(|(d, _, _)| *d == discriminant)?;
        self.legacy_despawn_on_exit
            .get(idx)
            .and_then(|opt| opt.map(|(h, _)| h))
    }

    /// Returns the primary-slot legacy canonical name for the `DespawnOnExit` marker of
    /// `discriminant` (when registered on primary slot 0).
    pub fn despawn_on_exit_name(&self, discriminant: u32) -> Option<&'static str> {
        let idx = self
            .variants
            .iter()
            .position(|(d, _, _)| *d == discriminant)?;
        self.legacy_despawn_on_exit
            .get(idx)
            .and_then(|opt| opt.map(|(_, n)| n))
    }

    /// Returns the primary-slot legacy default-insertable `DespawnOnEnter` marker [`BamComponent`]
    /// handle for `discriminant` (when registered on primary slot 0); use
    /// [`Self::mod_despawn_on_enter_handle`] + [`Self::despawn_on_enter_marker`] for arbitrary
    /// dynamic states and variant counts.
    pub fn despawn_on_enter_handle(&self, discriminant: u32) -> Option<BamComponent> {
        let idx = self
            .variants
            .iter()
            .position(|(d, _, _)| *d == discriminant)?;
        self.legacy_despawn_on_enter
            .get(idx)
            .and_then(|opt| opt.map(|(h, _)| h))
    }

    /// Returns the primary-slot legacy canonical name for the `DespawnOnEnter` marker of
    /// `discriminant` (when registered on primary slot 0).
    pub fn despawn_on_enter_name(&self, discriminant: u32) -> Option<&'static str> {
        let idx = self
            .variants
            .iter()
            .position(|(d, _, _)| *d == discriminant)?;
        self.legacy_despawn_on_enter
            .get(idx)
            .and_then(|opt| opt.map(|(_, n)| n))
    }
}

fn short_type_name<S>() -> &'static str {
    let full = core::any::type_name::<S>();
    full.rsplit("::").next().unwrap_or(full)
}

fn register_schedule_label<L: ScheduleLabel>(
    world: &mut World,
    id: ScheduleId,
    label: L,
    eager_create: bool,
) {
    let interned = label.intern();
    if eager_create {
        if !world.contains_resource::<Schedules>() {
            world.insert_resource(Schedules::new());
        }
        if let Some(mut schedules) = world.get_resource_mut::<Schedules>() {
            let _ = schedules.entry(interned);
        }
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

fn validate_state_variants<S: States + Copy>(
    base_schedule_id: ScheduleId,
    variants: &[(u32, &str, S)],
) -> Result<Vec<(u32, String, S)>, ModStateError> {
    if !base_schedule_id.is_custom() {
        return Err(ModStateError::InvalidBaseScheduleId(base_schedule_id));
    }
    if variants.is_empty() {
        return Err(ModStateError::EmptyVariants);
    }

    let n = u32::try_from(variants.len()).map_err(|_| ModStateError::ScheduleIdOverflow)?;
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
        if *disc == STATE_ABSENT_DISCRIMINANT {
            return Err(ModStateError::ReservedDiscriminant(*disc));
        }
        if !seen_discs.insert(*disc) {
            return Err(ModStateError::DuplicateDiscriminant(*disc));
        }
        if name.is_empty() || name.contains("::") || !seen_names.insert(*name) {
            return Err(ModStateError::InvalidVariantName((*name).to_string()));
        }
        owned_variants.push((*disc, (*name).to_string(), *val));
    }
    Ok(owned_variants)
}

#[allow(clippy::too_many_arguments)]
fn finalize_state_exposure<S: States + Copy>(
    app: &mut App,
    handle: ModStateHandle,
    kind: ModStateKind,
    base_schedule_id: ScheduleId,
    owned_variants: Vec<(u32, String, S)>,
    bridges: StateDynamicBridgesInstalled,
    state_handle: BamComponent,
    state_resource_name: &'static str,
    next_state_handle: Option<BamComponent>,
    next_state_resource_name: Option<&'static str>,
    request_transition: Option<RequestTransitionFn>,
) -> Result<ModStateSpec<S>, ModStateError> {
    let n = owned_variants.len() as u32;
    let short = short_type_name::<S>();
    let full_sanitized = core::any::type_name::<S>().replace("::", ".");
    let register_legacy_primary = handle.slot_index == 0 && handle.generation == 1;

    let mut legacy_despawn_on_exit = Vec::with_capacity(owned_variants.len());
    let mut legacy_despawn_on_enter = Vec::with_capacity(owned_variants.len());

    for (i, (disc, var_name, variant)) in owned_variants.iter().enumerate() {
        let enter_id = ScheduleId::custom(base_schedule_id.0 + i as u32);
        let exit_id = ScheduleId::custom(base_schedule_id.0 + n + i as u32);
        register_schedule_label(app.world_mut(), enter_id, OnEnter(*variant), true);
        register_schedule_label(app.world_mut(), exit_id, OnExit(*variant), true);

        for (j, (_, _, entered)) in owned_variants.iter().enumerate() {
            let trans_id =
                ScheduleId::custom(base_schedule_id.0 + 2 * n + (i as u32) * n + (j as u32));
            register_schedule_label(
                app.world_mut(),
                trans_id,
                OnTransition {
                    exited: *variant,
                    entered: *entered,
                },
                false,
            );
        }

        let set = ModInStateSet::<S>::new(*disc);
        app.configure_sets(PreUpdate, set.run_if(in_state(*variant)));
        app.configure_sets(Update, set.run_if(in_state(*variant)));
        app.configure_sets(PostUpdate, set.run_if(in_state(*variant)));
        app.configure_sets(FixedUpdate, set.run_if(in_state(*variant)));

        register_visible_set(app.world_mut(), &format!("state.{short}.{disc}"), set);
        register_visible_set(app.world_mut(), &format!("state.{short}.{var_name}"), set);
        if full_sanitized != short {
            register_visible_set(
                app.world_mut(),
                &format!("state.{full_sanitized}.{disc}"),
                set,
            );
            register_visible_set(
                app.world_mut(),
                &format!("state.{full_sanitized}.{var_name}"),
                set,
            );
        }

        if register_legacy_primary && i < LEGACY_PRIMARY_MARKER_SLOTS {
            legacy_despawn_on_exit.push(register_primary_exit_marker::<S>(app.world_mut(), i)?);
            legacy_despawn_on_enter.push(register_primary_enter_marker::<S>(app.world_mut(), i)?);
        } else {
            legacy_despawn_on_exit.push(None);
            legacy_despawn_on_enter.push(None);
        }
    }

    let variants_for_exit: Arc<Vec<(u32, S)>> =
        Arc::new(owned_variants.iter().map(|(d, _, v)| (*d, *v)).collect());
    let variants_for_enter = Arc::clone(&variants_for_exit);

    let attach_despawn_on_exit: AttachDespawnFn =
        Arc::new(move |world: &mut World, entity: Entity, disc: u32| {
            let Some((_, variant)) = variants_for_exit.iter().find(|(d, _)| *d == disc) else {
                return false;
            };
            if let Ok(mut ent) = world.get_entity_mut(entity) {
                ent.insert(DespawnOnExit(*variant));
                true
            } else {
                false
            }
        });

    let attach_despawn_on_enter: AttachDespawnFn =
        Arc::new(move |world: &mut World, entity: Entity, disc: u32| {
            let Some((_, variant)) = variants_for_enter.iter().find(|(d, _)| *d == disc) else {
                return false;
            };
            if let Ok(mut ent) = world.get_entity_mut(entity) {
                ent.insert(DespawnOnEnter(*variant));
                true
            } else {
                false
            }
        });

    let current_discriminant = app
        .world()
        .get_resource::<State<S>>()
        .and_then(|s| lookup_discriminant_for(s.get()));

    let state_entry_entity = app
        .world_mut()
        .spawn(ModStateEntry::new(handle, kind, current_discriminant))
        .id();

    let slab_variants: Vec<(u32, String)> = owned_variants
        .iter()
        .map(|(d, name, _)| (*d, name.clone()))
        .collect();

    let prev_entity = app
        .world_mut()
        .resource_mut::<ModStateSlab>()
        .install_entry(
            handle,
            StateSlabEntry {
                type_id: TypeId::of::<S>(),
                type_name: core::any::type_name::<S>(),
                kind,
                base_schedule_id,
                state_handle,
                state_resource_name,
                next_state_handle,
                next_state_resource_name,
                entry_entity: state_entry_entity,
                current_discriminant,
                variants: slab_variants,
                attach_despawn_on_exit,
                attach_despawn_on_enter,
                request_transition,
            },
        );
    if let Some(old_ent) = prev_entity
        && let Ok(ent_mut) = app.world_mut().get_entity_mut(old_ent)
    {
        ent_mut.despawn();
    }

    let spec = ModStateSpec {
        handle,
        kind,
        base_schedule_id,
        state_handle,
        state_resource_name,
        next_state_handle,
        next_state_resource_name,
        state_entry_entity,
        mod_state_entry_handle: bridges.entry_handle,
        mod_transition_request_handle: kind.is_mutable().then_some(bridges.request_handle),
        mod_despawn_on_exit_handle: bridges.exit_handle,
        mod_despawn_on_enter_handle: bridges.enter_handle,
        variants: owned_variants,
        legacy_despawn_on_exit,
        legacy_despawn_on_enter,
    };
    app.insert_resource(spec.clone());
    Ok(spec)
}

fn ensure_state_infra(app: &mut App) {
    if !app.is_plugin_added::<StatesPlugin>() {
        app.add_plugins(StatesPlugin);
    }
    if !app.world().contains_resource::<ModRegistry>() {
        app.insert_resource(ModRegistry::new());
    }
    if !app.world().contains_resource::<ModStateSlab>() {
        app.insert_resource(ModStateSlab::new());
    }
}

/// Extension trait on [`App`] for exposing Bevy [`FreelyMutableState`], [`SubStates`], and
/// read-only [`ComputedStates`] enums to native mods via dynamic generation-indexed [`ModStateSlab`]
/// storage.
pub trait ModStateExt {
    /// Exposes a top-level [`FreelyMutableState`] `S` across the C ABI:
    /// - Read-only `State<S>` / [`ModStateEntry`] mirror and writable transitions via
    ///   [`ModNextState<S>`] / [`ModStateTransitionRequest`] (`NextState::set_if_different`),
    /// - Deterministic `OnEnter(s)` (`base + i`), `OnExit(s)` (`base + N + i`), and
    ///   `OnTransition { exited, entered }` (`base + 2*N + i*N + j`) schedules in [`ModScheduleMap`],
    /// - Host-visible `in_state` system sets (`"host::state.<ShortS>.<disc|Name>"`), and
    /// - Dynamic unbounded [`ModDespawnOnExit`] / [`ModDespawnOnEnter`] markers backed by [`ModStateSlab`].
    fn expose_mod_state<S: FreelyMutableState + Copy>(
        &mut self,
        base_schedule_id: ScheduleId,
        variants: &[(u32, &str, S)],
    ) -> Result<ModStateSpec<S>, ModStateError>;

    /// Exposes a Bevy [`SubStates`] `S` across the C ABI:
    /// - Installs `App::add_sub_state::<S>()` (preserving conditional existence governed by
    ///   `S::should_exist(sources)`),
    /// - Exposes read-only [`ModStateEntry`] / `State<S>` and writable transitions via
    ///   [`ModNextState<S>`] / [`ModStateTransitionRequest`] (safely tolerating frames where
    ///   `State<S>` is absent and syncing [`ModStateSlab::current_discriminant`] to `None`),
    /// - Registers deterministic `OnEnter` / `OnExit` / `OnTransition` schedules, `in_state`
    ///   visible sets, and dynamic [`ModDespawnOnExit`] / [`ModDespawnOnEnter`] hooks.
    fn register_mod_sub_state<S: SubStates + Copy>(
        &mut self,
        base_schedule_id: ScheduleId,
        variants: &[(u32, &str, S)],
    ) -> Result<ModStateSpec<S>, ModStateError>;

    /// Exposes a read-only Bevy [`ComputedStates`] `S` across the C ABI:
    /// - Installs `App::add_computed_state::<S>()`,
    /// - Exposes **read-only** [`ModStateEntry`] / `State<S>` (no writable next-state binding;
    ///   transition attempts via [`ModStateSlab::request_transition`] fail with
    ///   [`FfiResult::PERMISSION_DENIED`]),
    /// - Registers deterministic `OnEnter` / `OnExit` / `OnTransition` schedules, `in_state`
    ///   visible sets, and dynamic [`ModDespawnOnExit`] / [`ModDespawnOnEnter`] hooks.
    fn register_mod_computed_state<S: ComputedStates + Copy>(
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
        let owned_variants = validate_state_variants(base_schedule_id, variants)?;
        ensure_state_infra(self);
        let bridges = ensure_dynamic_state_bridges(self)?;

        if !self.world().contains_resource::<State<S>>() {
            self.insert_state(owned_variants[0].2);
        }

        let handle = self
            .world_mut()
            .resource_mut::<ModStateSlab>()
            .allocate_slot_for::<S>();

        store_state_variants(owned_variants.clone());

        let initial_disc = self
            .world()
            .get_resource::<State<S>>()
            .and_then(|s| lookup_discriminant_for(s.get()))
            .or(Some(owned_variants[0].0));
        let pair_list: Vec<(u32, S)> = owned_variants.iter().map(|(d, _, v)| (*d, *v)).collect();
        self.insert_resource(ModNextState::new(initial_disc, pair_list));

        let (state_handle, state_resource_name, next_state_handle, next_state_resource_name) =
            register_mutable_state_bindings::<S>(
                self.world_mut(),
                handle,
                bridges.entry_handle,
                bridges.request_handle,
            )?;

        self.add_systems(PreUpdate, sync_mod_next_state::<S>.in_set(ModStateSyncSet));
        self.add_systems(PostUpdate, sync_mod_next_state::<S>.in_set(ModStateSyncSet));
        self.add_systems(
            StateTransition,
            sync_mutable_state_post_transition::<S>.after(StateTransitionSystems::EnterSchedules),
        );

        let request_transition: RequestTransitionFn = Arc::new(|world: &mut World, disc: u32| {
            if let Some(mut mod_next) = world.get_resource_mut::<ModNextState<S>>() {
                mod_next.queue_discriminant(disc)
            } else {
                FfiResult::INVALID_STATE
            }
        });

        finalize_state_exposure(
            self,
            handle,
            ModStateKind::FreelyMutable,
            base_schedule_id,
            owned_variants,
            bridges,
            state_handle,
            state_resource_name,
            Some(next_state_handle),
            Some(next_state_resource_name),
            Some(request_transition),
        )
    }

    fn register_mod_sub_state<S: SubStates + Copy>(
        &mut self,
        base_schedule_id: ScheduleId,
        variants: &[(u32, &str, S)],
    ) -> Result<ModStateSpec<S>, ModStateError> {
        let owned_variants = validate_state_variants(base_schedule_id, variants)?;
        ensure_state_infra(self);
        let bridges = ensure_dynamic_state_bridges(self)?;

        self.add_sub_state::<S>();

        let handle = self
            .world_mut()
            .resource_mut::<ModStateSlab>()
            .allocate_slot_for::<S>();

        store_state_variants(owned_variants.clone());

        let initial_disc = self
            .world()
            .get_resource::<State<S>>()
            .and_then(|s| lookup_discriminant_for(s.get()));
        let pair_list: Vec<(u32, S)> = owned_variants.iter().map(|(d, _, v)| (*d, *v)).collect();
        self.insert_resource(ModNextState::new(initial_disc, pair_list));

        let (state_handle, state_resource_name, next_state_handle, next_state_resource_name) =
            register_mutable_state_bindings::<S>(
                self.world_mut(),
                handle,
                bridges.entry_handle,
                bridges.request_handle,
            )?;

        self.add_systems(PreUpdate, sync_mod_next_state::<S>.in_set(ModStateSyncSet));
        self.add_systems(PostUpdate, sync_mod_next_state::<S>.in_set(ModStateSyncSet));
        self.add_systems(
            StateTransition,
            sync_mutable_state_post_transition::<S>.after(StateTransitionSystems::EnterSchedules),
        );

        let request_transition: RequestTransitionFn = Arc::new(|world: &mut World, disc: u32| {
            if let Some(mut mod_next) = world.get_resource_mut::<ModNextState<S>>() {
                mod_next.queue_discriminant(disc)
            } else {
                FfiResult::INVALID_STATE
            }
        });

        finalize_state_exposure(
            self,
            handle,
            ModStateKind::SubState,
            base_schedule_id,
            owned_variants,
            bridges,
            state_handle,
            state_resource_name,
            Some(next_state_handle),
            Some(next_state_resource_name),
            Some(request_transition),
        )
    }

    fn register_mod_computed_state<S: ComputedStates + Copy>(
        &mut self,
        base_schedule_id: ScheduleId,
        variants: &[(u32, &str, S)],
    ) -> Result<ModStateSpec<S>, ModStateError> {
        let owned_variants = validate_state_variants(base_schedule_id, variants)?;
        ensure_state_infra(self);
        let bridges = ensure_dynamic_state_bridges(self)?;

        self.add_computed_state::<S>();

        let handle = self
            .world_mut()
            .resource_mut::<ModStateSlab>()
            .allocate_slot_for::<S>();

        store_state_variants(owned_variants.clone());

        let (state_handle, state_resource_name) =
            register_readonly_state_binding::<S>(self.world_mut(), handle, bridges.entry_handle)?;

        self.add_systems(PreUpdate, sync_mod_state_slab::<S>.in_set(ModStateSyncSet));
        self.add_systems(PostUpdate, sync_mod_state_slab::<S>.in_set(ModStateSyncSet));
        self.add_systems(
            StateTransition,
            sync_mod_state_slab::<S>.after(StateTransitionSystems::EnterSchedules),
        );

        finalize_state_exposure(
            self,
            handle,
            ModStateKind::Computed,
            base_schedule_id,
            owned_variants,
            bridges,
            state_handle,
            state_resource_name,
            None,
            None,
            None,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy_abi_mod_host::{AbiModPlugin, ModVisibleSets};
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
        assert_eq!(spec.generation(), 1);
        assert_eq!(spec.kind(), ModStateKind::FreelyMutable);
        assert!(spec.is_mutable());
        assert_eq!(spec.state_resource_name(), "bevy_state::State#0");
        assert_eq!(
            spec.next_state_resource_name(),
            Some("bevy_abi_mod::ModNextState#0")
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

        // Verify `State<GamePhase>` reads `0` (`Menu`) via `HostWireBridge` and `ModStateEntry`.
        assert_eq!(
            PrimaryStateWireBridge::<GamePhase>::to_wire(
                app.world().resource::<State<GamePhase>>()
            ),
            0
        );
        let entry = *app
            .world()
            .get::<ModStateEntry>(spec.state_entry_entity())
            .unwrap();
        assert_eq!(entry.active_discriminant(), Some(0));
        assert!(entry.is_mutable());

        // Queue transition Menu (0) -> Playing (1) via `PrimaryNextStateWireBridge::apply_wire`.
        assert_eq!(
            PrimaryNextStateWireBridge::<GamePhase>::apply_wire(
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
            PrimaryNextStateWireBridge::<GamePhase>::apply_wire(
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
            PrimaryNextStateWireBridge::<GamePhase>::apply_wire(
                app.world_mut()
                    .resource_mut::<ModNextState<GamePhase>>()
                    .into_inner(),
                99
            ),
            FfiResult::INVALID_ARGUMENT
        );

        // Spawn two entities and attach `DespawnOnExit(Playing)` and `DespawnOnEnter(Paused)` via
        // the registered primary legacy default-insertable marker bridges.
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

        // Also spawn two entities using the dynamic `ModDespawnOnExit` / `ModDespawnOnEnter` components.
        let e_dyn_exit = app
            .world_mut()
            .spawn((PhaseScopedTag, spec.despawn_on_exit_marker(1).unwrap()))
            .id();
        let e_dyn_enter = app
            .world_mut()
            .spawn((PhaseScopedTag, spec.despawn_on_enter_marker(2).unwrap()))
            .id();

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
        assert!(
            app.world()
                .get::<DespawnOnExit<GamePhase>>(e_dyn_exit)
                .is_some()
        );
        assert!(
            app.world()
                .get::<DespawnOnEnter<GamePhase>>(e_dyn_enter)
                .is_some()
        );

        // Transition Playing (1) -> Paused (2) via `ModStateTransitionRequest` message -> all 4 despawn!
        app.world_mut()
            .resource_mut::<Messages<ModStateTransitionRequest>>()
            .write(spec.transition_request(2).unwrap());
        app.update();
        assert_eq!(
            *app.world().resource::<State<GamePhase>>().get(),
            GamePhase::Paused
        );
        assert!(app.world().get_entity(e_exit).is_err());
        assert!(app.world().get_entity(e_enter).is_err());
        assert!(app.world().get_entity(e_dyn_exit).is_err());
        assert!(app.world().get_entity(e_dyn_enter).is_err());
    }
}
