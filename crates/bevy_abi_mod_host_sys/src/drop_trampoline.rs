//! Lock-free `ArcSwapOption` per-`World` drop trampolines with lazily-allocated paged slabs and
//! generation-stamped slot recycling.
//!
//! Implements the two-condition slot lifecycle:
//! - Hot path (`drop_trampoline::<SLOT>`): wait-free `OnceLock::get()` + `ArcSwapOption::load()`
//!   with zero `Mutex` acquisition and zero leaked `RetiredSnapshotPtr` allocations.
//! - Lazily-allocated paged slab: up to [`MAX_DROP_PAGES`] (`64`) pages of [`DROP_PAGE_SIZE`] (`64`)
//!   slots ([`MAX_DROP_TRAMPOLINES`] = `4,096`), allocated on demand so empty worlds pay zero heap cost.
//! - Generation-stamped per-`World` recycling: when `unload_mod` finishes despawning mod entities and
//!   `live_values == 0`, `WorldDropGuard::recycle_slots_for_guard` immediately recycles the slot
//!   with `generation + 1` back to the global free list without waiting for `World::drop`.
//! - Deferred reclamation (`live_values: AtomicUsize`): for persistent/host entities that outlive
//!   `unload_mod` or `World::drop`, a slot is returned to the global free list only once *both*
//!   its owning guard/world has closed and `live_values` reaches `0`.

use crate::handle_header::lock_unpoison;
use crate::sync_ptr::GuestFnPtr;
use arc_swap::ArcSwapOption;
use bevy_abi_mod_sys::{ComponentDropFn, ComponentUuid};
use bevy_ecs::lifecycle::{ComponentHook, HookContext};
use bevy_ecs::prelude::Resource;
use bevy_ecs::ptr::OwningPtr;
use bevy_ecs::world::DeferredWorld;
use bevy_platform::collections::HashMap;
use core::ffi::c_void;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex, OnceLock};

/// Number of trampoline slots per lazily-allocated slab page.
pub const DROP_PAGE_SIZE: usize = 64;
/// Maximum number of lazily-allocated slab pages.
pub const MAX_DROP_PAGES: usize = 64;
/// Maximum number of distinct `(World, ComponentUuid)` foreign drop trampolines active simultaneously.
pub const MAX_DROP_TRAMPOLINES: usize = DROP_PAGE_SIZE * MAX_DROP_PAGES;

/// Logical slot state: unassigned and present in `FREE_LIST`.
pub const SLOT_FREE: u8 = 0;
/// Logical slot state: owned by a live `WorldDropGuard`.
pub const SLOT_IN_USE: u8 = 1;
/// Logical slot state: `WorldDropGuard` has dropped; awaiting `live_values == 0` before returning to `FREE_LIST`.
pub const SLOT_CLOSING: u8 = 2;

/// High bit set on a slot's packed lifecycle word while in [`SLOT_CLOSING`].
pub const SLOT_CLOSING_BIT: usize = 1usize << (usize::BITS - 1);
/// Second-highest bit set on a slot's packed lifecycle word while in [`SLOT_IN_USE`].
pub const SLOT_IN_USE_BIT: usize = 1usize << (usize::BITS - 2);
/// Mask extracting `live_values` from a slot's packed lifecycle word.
pub const SLOT_LIVE_COUNT_MASK: usize = !(SLOT_CLOSING_BIT | SLOT_IN_USE_BIT);

/// Decodes a packed slot lifecycle word into `(SLOT_*, live_values)`.
#[inline]
pub const fn decode_slot_lifecycle(raw: usize) -> (u8, usize) {
    let state = if (raw & SLOT_CLOSING_BIT) != 0 {
        SLOT_CLOSING
    } else if (raw & SLOT_IN_USE_BIT) != 0 {
        SLOT_IN_USE
    } else {
        SLOT_FREE
    };
    (state, raw & SLOT_LIVE_COUNT_MASK)
}

type DropGuardPair = (GuestFnPtr<ComponentDropFn>, Arc<AtomicBool>);

struct DropSlotSnapshot {
    entries: Box<[DropGuardPair]>,
}

struct DropPage {
    slots: [ArcSwapOption<DropSlotSnapshot>; DROP_PAGE_SIZE],
    lifecycle: [AtomicUsize; DROP_PAGE_SIZE],
    generation: [AtomicU64; DROP_PAGE_SIZE],
}

impl DropPage {
    fn new() -> Self {
        Self {
            slots: core::array::from_fn(|_| ArcSwapOption::const_empty()),
            lifecycle: core::array::from_fn(|_| AtomicUsize::new(0)),
            generation: core::array::from_fn(|_| AtomicU64::new(1)),
        }
    }
}

static DROP_PAGES: [OnceLock<Box<DropPage>>; MAX_DROP_PAGES] =
    [const { OnceLock::new() }; MAX_DROP_PAGES];

#[inline]
fn get_drop_page(slot: usize) -> Option<(&'static DropPage, usize)> {
    if slot >= MAX_DROP_TRAMPOLINES {
        return None;
    }
    let page_idx = slot / DROP_PAGE_SIZE;
    let in_page = slot % DROP_PAGE_SIZE;
    let page = DROP_PAGES[page_idx].get()?;
    Some((page.as_ref(), in_page))
}

struct DropControlPlane {
    allocated_pages: usize,
    free_list: Vec<usize>,
    entries: Vec<Vec<DropGuardPair>>,
}

impl DropControlPlane {
    fn new() -> Self {
        Self {
            allocated_pages: 0,
            free_list: Vec::new(),
            entries: Vec::new(),
        }
    }

    fn allocate_slot(&mut self) -> Option<usize> {
        if self.free_list.is_empty() {
            if self.allocated_pages >= MAX_DROP_PAGES {
                return None;
            }
            let page_idx = self.allocated_pages;
            let _ = DROP_PAGES[page_idx].get_or_init(|| Box::new(DropPage::new()));
            let base = page_idx * DROP_PAGE_SIZE;
            self.entries.resize_with(base + DROP_PAGE_SIZE, Vec::new);
            for slot in (base..(base + DROP_PAGE_SIZE)).rev() {
                self.free_list.push(slot);
            }
            self.allocated_pages += 1;
        }
        self.free_list.pop()
    }

    #[inline]
    fn available_slots(&self) -> usize {
        let unallocated = (MAX_DROP_PAGES - self.allocated_pages) * DROP_PAGE_SIZE;
        self.free_list.len() + unallocated
    }
}

static DROP_CONTROL: LazyLock<Mutex<DropControlPlane>> =
    LazyLock::new(|| Mutex::new(DropControlPlane::new()));

#[inline]
fn reclaim_closed_slot(slot: usize) {
    let Some((page, in_page)) = get_drop_page(slot) else {
        return;
    };
    // Only the single winner transitioning `(SLOT_CLOSING_BIT | 0) -> 0` pushes `slot` onto
    // `free_list`, preventing double-reclamation under all C11 thread interleavings.
    if page.lifecycle[in_page]
        .compare_exchange(SLOT_CLOSING_BIT, 0, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
    {
        page.slots[in_page].store(None);
        page.generation[in_page].fetch_add(1, Ordering::AcqRel);
        let mut ctrl = lock_unpoison(&DROP_CONTROL);
        ctrl.entries[slot].clear();
        ctrl.free_list.push(slot);
    }
}

/// Staged drop-trampoline mutation recorded during `bevy_abi_mod_init` so [`crate::loader::transaction::LoadTransaction`]
/// or [`crate::rollback_unscheduled_mod_in_world`] can atomically undo uncommitted bindings.
#[derive(Clone)]
pub(crate) struct StagedDropBinding {
    uuid: ComponentUuid,
    slot: usize,
    fresh_slot_allocated: bool,
    previous_snapshot: Option<Arc<DropSlotSnapshot>>,
    is_active: Arc<AtomicBool>,
}

/// Invokes the most recent active foreign destructor bound to `slot` on an uncommitted arena
/// payload (`CommandArena`) **without** decrementing `SLOT_LIFECYCLE` (`on_insert` has not fired),
/// iff `slot`'s generation still matches `expected_gen`.
///
/// # Safety
/// `raw_ptr` must point to a valid initialized guest component payload matching `slot`'s layout.
#[inline]
pub(crate) unsafe fn run_uncommitted_slot_drop(slot: usize, expected_gen: u64, raw_ptr: *mut u8) {
    if raw_ptr.is_null() {
        return;
    }
    let Some((page, in_page)) = get_drop_page(slot) else {
        return;
    };
    if page.generation[in_page].load(Ordering::Acquire) != expected_gen {
        return;
    }
    if let Some(snap) = page.slots[in_page].load().as_ref()
        && let Some(&(GuestFnPtr(drop_fn), _)) = snap
            .entries
            .iter()
            .rev()
            .find(|(_, guard)| guard.load(Ordering::Acquire))
    {
        // SAFETY: `raw_ptr` holds an uncommitted guest payload whose owning mod (`guard == true`)
        // remains mapped in memory and `slot`'s generation matches `expected_gen`.
        unsafe { drop_fn(raw_ptr.cast::<c_void>()) };
    }
}

/// Increments the live-instance count for `slot` when a value of a drop-bearing dynamic component
/// is inserted into Bevy ECS storage.
#[inline]
pub(crate) fn increment_drop_slot_live_values(slot: usize) {
    if let Some((page, in_page)) = get_drop_page(slot) {
        page.lifecycle[in_page].fetch_add(1, Ordering::AcqRel);
    }
}

/// Invokes the most recent active foreign drop callback bound to `slot`, if any, and participates
/// in deferred slot reclamation when `WorldDropGuard` has already closed `slot`.
///
/// # Safety
/// `ptr` must point to a valid component allocation owned by Bevy's storage drop hook.
#[inline]
unsafe fn drop_trampoline_inner(slot: usize, ptr: OwningPtr<'_>) {
    let Some((page, in_page)) = get_drop_page(slot) else {
        return;
    };
    if let Some(snap) = page.slots[in_page].load().as_ref()
        && let Some(&(GuestFnPtr(drop_fn), _)) = snap
            .entries
            .iter()
            .rev()
            .find(|(_, guard)| guard.load(Ordering::Acquire))
    {
        let raw = ptr.as_ptr().cast::<c_void>();
        // SAFETY: Foreign drop function is invoked on a valid component instance while its
        // paired mod load session (`guard == true`) remains mapped in memory.
        unsafe { drop_fn(raw) };
    }

    let prev = page.lifecycle[in_page].fetch_sub(1, Ordering::AcqRel);
    if prev == (SLOT_CLOSING_BIT | 1) {
        reclaim_closed_slot(slot);
    }
}

/// Monomorphized Bevy storage destructor trampoline forwarding `SLOT` to [`drop_trampoline_inner`].
///
/// # Safety
/// `ptr` must point to a valid component allocation owned by Bevy's storage drop hook.
unsafe fn drop_trampoline<const SLOT: usize>(ptr: OwningPtr<'_>) {
    // SAFETY: Caller satisfies `drop_trampoline_inner` contract for `SLOT`.
    unsafe { drop_trampoline_inner(SLOT, ptr) };
}

macro_rules! tramp_16 {
    ($b:expr) => {
        [
            drop_trampoline::<{ $b }>,
            drop_trampoline::<{ $b + 1 }>,
            drop_trampoline::<{ $b + 2 }>,
            drop_trampoline::<{ $b + 3 }>,
            drop_trampoline::<{ $b + 4 }>,
            drop_trampoline::<{ $b + 5 }>,
            drop_trampoline::<{ $b + 6 }>,
            drop_trampoline::<{ $b + 7 }>,
            drop_trampoline::<{ $b + 8 }>,
            drop_trampoline::<{ $b + 9 }>,
            drop_trampoline::<{ $b + 10 }>,
            drop_trampoline::<{ $b + 11 }>,
            drop_trampoline::<{ $b + 12 }>,
            drop_trampoline::<{ $b + 13 }>,
            drop_trampoline::<{ $b + 14 }>,
            drop_trampoline::<{ $b + 15 }>,
        ]
    };
}

macro_rules! tramp_256 {
    ($b:expr) => {
        [
            tramp_16!($b),
            tramp_16!($b + 16),
            tramp_16!($b + 32),
            tramp_16!($b + 48),
            tramp_16!($b + 64),
            tramp_16!($b + 80),
            tramp_16!($b + 96),
            tramp_16!($b + 112),
            tramp_16!($b + 128),
            tramp_16!($b + 144),
            tramp_16!($b + 160),
            tramp_16!($b + 176),
            tramp_16!($b + 192),
            tramp_16!($b + 208),
            tramp_16!($b + 224),
            tramp_16!($b + 240),
        ]
    };
}

pub(crate) type DropTrampolineFn = for<'a> unsafe fn(OwningPtr<'a>);
type DropTrampolineBlock = [[DropTrampolineFn; 16]; 16];

static DROP_TRAMPOLINE_BLOCKS: [DropTrampolineBlock; 16] = [
    tramp_256!(0),
    tramp_256!(256),
    tramp_256!(512),
    tramp_256!(768),
    tramp_256!(1024),
    tramp_256!(1280),
    tramp_256!(1536),
    tramp_256!(1792),
    tramp_256!(2048),
    tramp_256!(2304),
    tramp_256!(2560),
    tramp_256!(2816),
    tramp_256!(3072),
    tramp_256!(3328),
    tramp_256!(3584),
    tramp_256!(3840),
];

#[inline]
fn drop_trampoline_fn_for_slot(slot: usize) -> DropTrampolineFn {
    let b256 = slot / 256;
    let rem = slot % 256;
    let b16 = rem / 16;
    let idx = rem % 16;
    DROP_TRAMPOLINE_BLOCKS[b256][b16][idx]
}

fn on_insert_trampoline<const SLOT: usize>(_world: DeferredWorld<'_>, _ctx: HookContext) {
    increment_drop_slot_live_values(SLOT);
}

macro_rules! insert_hook_16 {
    ($b:expr) => {
        [
            on_insert_trampoline::<{ $b }>,
            on_insert_trampoline::<{ $b + 1 }>,
            on_insert_trampoline::<{ $b + 2 }>,
            on_insert_trampoline::<{ $b + 3 }>,
            on_insert_trampoline::<{ $b + 4 }>,
            on_insert_trampoline::<{ $b + 5 }>,
            on_insert_trampoline::<{ $b + 6 }>,
            on_insert_trampoline::<{ $b + 7 }>,
            on_insert_trampoline::<{ $b + 8 }>,
            on_insert_trampoline::<{ $b + 9 }>,
            on_insert_trampoline::<{ $b + 10 }>,
            on_insert_trampoline::<{ $b + 11 }>,
            on_insert_trampoline::<{ $b + 12 }>,
            on_insert_trampoline::<{ $b + 13 }>,
            on_insert_trampoline::<{ $b + 14 }>,
            on_insert_trampoline::<{ $b + 15 }>,
        ]
    };
}

macro_rules! insert_hook_256 {
    ($b:expr) => {
        [
            insert_hook_16!($b),
            insert_hook_16!($b + 16),
            insert_hook_16!($b + 32),
            insert_hook_16!($b + 48),
            insert_hook_16!($b + 64),
            insert_hook_16!($b + 80),
            insert_hook_16!($b + 96),
            insert_hook_16!($b + 112),
            insert_hook_16!($b + 128),
            insert_hook_16!($b + 144),
            insert_hook_16!($b + 160),
            insert_hook_16!($b + 176),
            insert_hook_16!($b + 192),
            insert_hook_16!($b + 208),
            insert_hook_16!($b + 224),
            insert_hook_16!($b + 240),
        ]
    };
}

type InsertHookBlock = [[ComponentHook; 16]; 16];

static INSERT_HOOK_BLOCKS: [InsertHookBlock; 16] = [
    insert_hook_256!(0),
    insert_hook_256!(256),
    insert_hook_256!(512),
    insert_hook_256!(768),
    insert_hook_256!(1024),
    insert_hook_256!(1280),
    insert_hook_256!(1536),
    insert_hook_256!(1792),
    insert_hook_256!(2048),
    insert_hook_256!(2304),
    insert_hook_256!(2560),
    insert_hook_256!(2816),
    insert_hook_256!(3072),
    insert_hook_256!(3328),
    insert_hook_256!(3584),
    insert_hook_256!(3840),
];

/// Returns the monomorphized `ComponentHook` `fn` pointer incrementing `live_values` for `slot`.
#[inline]
pub(crate) fn on_insert_hook_for_slot(slot: usize) -> ComponentHook {
    let b256 = slot / 256;
    let rem = slot % 256;
    let b16 = rem / 16;
    let idx = rem % 16;
    INSERT_HOOK_BLOCKS[b256][b16][idx]
}

struct WorldDropSlot {
    slot: usize,
    generation: u64,
    guards: Vec<Arc<AtomicBool>>,
}

/// Per-`World` RAII resource owning all drop trampoline slots bound to `ComponentDescriptor`s in
/// that `World`.
///
/// On `unload_mod`, `Self::recycle_slots_for_guard` immediately returns any slot owned exclusively
/// by the unloaded mod whose `live_values == 0` (with incremented `generation + 1`) back to the
/// global free list. Slots whose entities persist past mod unload are reclaimed when the `World`
/// drops (or once `live_values` reaches `0`).
#[derive(Resource, Default)]
pub struct WorldDropGuard {
    slots_by_uuid: HashMap<ComponentUuid, WorldDropSlot>,
    guards: Vec<Arc<AtomicBool>>,
}

impl WorldDropGuard {
    /// Creates an empty [`WorldDropGuard`].
    #[inline]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the number of trampoline slots currently bound to this `World`.
    #[inline]
    pub fn len(&self) -> usize {
        self.slots_by_uuid.len()
    }

    /// Returns `true` if this `World` currently owns zero drop trampoline slots.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.slots_by_uuid.is_empty()
    }

    /// Returns the `(slot, generation)` currently bound to `uuid` in this `World`, if any.
    #[inline]
    pub fn binding_for_uuid(&self, uuid: &ComponentUuid) -> Option<(usize, u64)> {
        self.slots_by_uuid.get(uuid).map(|b| (b.slot, b.generation))
    }

    /// Allocates a new drop trampoline slot for `uuid` or updates the existing slot bound to `uuid`
    /// in this `World`, returning `(slot_index, trampoline_fn, staged_binding)` or `None` when all
    /// [`MAX_DROP_TRAMPOLINES`] slots are currently in use (`FfiResult::CAPACITY_EXCEEDED`).
    pub(crate) fn bind_or_update(
        &mut self,
        uuid: ComponentUuid,
        drop_fn: ComponentDropFn,
        is_active: Arc<AtomicBool>,
    ) -> Option<(usize, DropTrampolineFn, StagedDropBinding)> {
        let mut ctrl = lock_unpoison(&DROP_CONTROL);

        let wrapped = GuestFnPtr(drop_fn);

        if let Some(binding) = self.slots_by_uuid.get_mut(&uuid) {
            let slot = binding.slot;
            if let Some((page, in_page)) = get_drop_page(slot)
                && page.generation[in_page].load(Ordering::Acquire) == binding.generation
            {
                let previous_snapshot = page.slots[in_page].load_full();
                let entries = &mut ctrl.entries[slot];
                entries.retain(|(_, g)| g.load(Ordering::Acquire));
                if let Some(existing) = entries.iter_mut().find(|(_, g)| Arc::ptr_eq(g, &is_active))
                {
                    existing.0 = wrapped;
                } else {
                    entries.push((wrapped, Arc::clone(&is_active)));
                }
                let snap = Arc::new(DropSlotSnapshot {
                    entries: entries.clone().into_boxed_slice(),
                });
                page.slots[in_page].store(Some(snap));
                if !binding.guards.iter().any(|g| Arc::ptr_eq(g, &is_active)) {
                    binding.guards.push(Arc::clone(&is_active));
                }
                drop(ctrl);

                if !self.guards.iter().any(|g| Arc::ptr_eq(g, &is_active)) {
                    self.guards.push(Arc::clone(&is_active));
                }
                let staged = StagedDropBinding {
                    uuid,
                    slot,
                    fresh_slot_allocated: false,
                    previous_snapshot,
                    is_active,
                };
                return Some((slot, drop_trampoline_fn_for_slot(slot), staged));
            }
        }

        let slot = ctrl.allocate_slot()?;
        let (page, in_page) = get_drop_page(slot)?;
        let generation = page.generation[in_page].load(Ordering::Acquire);
        page.lifecycle[in_page].store(SLOT_IN_USE_BIT, Ordering::Release);

        let entries = &mut ctrl.entries[slot];
        entries.clear();
        entries.push((wrapped, Arc::clone(&is_active)));
        let snap = Arc::new(DropSlotSnapshot {
            entries: entries.clone().into_boxed_slice(),
        });
        page.slots[in_page].store(Some(snap));
        drop(ctrl);

        self.slots_by_uuid.insert(
            uuid,
            WorldDropSlot {
                slot,
                generation,
                guards: vec![Arc::clone(&is_active)],
            },
        );
        if !self.guards.iter().any(|g| Arc::ptr_eq(g, &is_active)) {
            self.guards.push(Arc::clone(&is_active));
        }
        let staged = StagedDropBinding {
            uuid,
            slot,
            fresh_slot_allocated: true,
            previous_snapshot: None,
            is_active,
        };
        Some((slot, drop_trampoline_fn_for_slot(slot), staged))
    }

    /// Rolls back uncommitted [`StagedDropBinding`]s in reverse registration order:
    /// disarms `is_active`, restores `previous_snapshot` on hot-updated slots, and immediately
    /// returns freshly allocated slots (`live_values == 0`) to `FREE_LIST` with incremented generation.
    pub(crate) fn rollback_staged_bindings(&mut self, bindings: &[StagedDropBinding]) {
        if bindings.is_empty() {
            return;
        }
        for binding in bindings.iter().rev() {
            binding.is_active.store(false, Ordering::Release);
        }

        let mut ctrl = lock_unpoison(&DROP_CONTROL);

        for binding in bindings.iter().rev() {
            let slot = binding.slot;
            let Some((page, in_page)) = get_drop_page(slot) else {
                continue;
            };
            if binding.fresh_slot_allocated {
                self.slots_by_uuid.remove(&binding.uuid);
                page.slots[in_page].store(None);
                page.lifecycle[in_page].store(0, Ordering::Release);
                page.generation[in_page].fetch_add(1, Ordering::AcqRel);
                ctrl.entries[slot].clear();
                if !ctrl.free_list.contains(&slot) {
                    ctrl.free_list.push(slot);
                }
            } else {
                if let Some(slot_entry) = self.slots_by_uuid.get_mut(&binding.uuid) {
                    slot_entry
                        .guards
                        .retain(|g| !Arc::ptr_eq(g, &binding.is_active));
                }
                ctrl.entries[slot].retain(|(_, g)| {
                    !Arc::ptr_eq(g, &binding.is_active) && g.load(Ordering::Acquire)
                });
                page.slots[in_page].store(binding.previous_snapshot.clone());
            }
        }
        drop(ctrl);

        self.guards.retain(|g| g.load(Ordering::Acquire));
    }

    /// Immediately recycles drop slots owned exclusively by `is_active` whose `live_values == 0`
    /// after `unload_mod` finishes despawning mod entities, incrementing each recycled slot's
    /// generation (`gen + 1`) and returning it to the global free list.
    ///
    /// Slots that still have live entities (`live_values > 0`, e.g. `spawn_persistent`) remain
    /// bound until their remaining entities despawn and `World::drop` (or a later unload) runs.
    pub(crate) fn recycle_slots_for_guard(&mut self, is_active: &Arc<AtomicBool>) -> usize {
        let mut ctrl = lock_unpoison(&DROP_CONTROL);

        let mut recycled_slots = Vec::new();
        let mut uuids_to_remove = Vec::new();

        for (&uuid, binding) in &mut self.slots_by_uuid {
            let had_guard = binding.guards.iter().any(|g| Arc::ptr_eq(g, is_active));
            if had_guard {
                binding.guards.retain(|g| !Arc::ptr_eq(g, is_active));
            }
            if !binding.guards.is_empty() {
                continue;
            }
            let slot = binding.slot;
            let Some((page, in_page)) = get_drop_page(slot) else {
                uuids_to_remove.push(uuid);
                continue;
            };
            if page.generation[in_page].load(Ordering::Acquire) != binding.generation {
                uuids_to_remove.push(uuid);
                continue;
            }
            // Only recycle if `live_values == 0` (`SLOT_IN_USE_BIT | 0 -> 0`).
            if page.lifecycle[in_page]
                .compare_exchange(SLOT_IN_USE_BIT, 0, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                page.slots[in_page].store(None);
                page.generation[in_page].fetch_add(1, Ordering::AcqRel);
                ctrl.entries[slot].clear();
                recycled_slots.push(slot);
                uuids_to_remove.push(uuid);
            }
        }

        // Push sorted descending so the lowest slot index is popped first (LIFO) on immediate reload.
        recycled_slots.sort_unstable_by(|a, b| b.cmp(a));
        let count = recycled_slots.len();
        for slot in recycled_slots {
            ctrl.free_list.push(slot);
        }
        drop(ctrl);

        for uuid in uuids_to_remove {
            self.slots_by_uuid.remove(&uuid);
        }
        self.guards
            .retain(|g| !Arc::ptr_eq(g, is_active) && g.load(Ordering::Acquire));

        count
    }
}

impl Drop for WorldDropGuard {
    fn drop(&mut self) {
        for guard in &self.guards {
            guard.store(false, Ordering::Release);
        }
        for binding in self.slots_by_uuid.values() {
            let slot = binding.slot;
            if let Some((page, in_page)) = get_drop_page(slot) {
                page.slots[in_page].store(None);
                let prev = page.lifecycle[in_page]
                    .fetch_xor(SLOT_IN_USE_BIT | SLOT_CLOSING_BIT, Ordering::AcqRel);
                if (prev & SLOT_LIVE_COUNT_MASK) == 0 {
                    reclaim_closed_slot(slot);
                }
            }
        }
    }
}

/// Disarms `is_active` and prunes all drop trampoline entries paired with `is_active` before `dlclose`.
pub(crate) fn prune_drop_trampolines_for_guard(is_active: &Arc<AtomicBool>) {
    is_active.store(false, Ordering::Release);
    let mut ctrl = lock_unpoison(&DROP_CONTROL);
    let allocated_slots = ctrl.entries.len();

    for (slot, entries) in ctrl.entries.iter_mut().enumerate().take(allocated_slots) {
        let Some((page, in_page)) = get_drop_page(slot) else {
            continue;
        };
        let (state, _) = decode_slot_lifecycle(page.lifecycle[in_page].load(Ordering::Acquire));
        if state != SLOT_IN_USE {
            continue;
        }
        let before = entries.len();
        entries.retain(|(_, g)| !Arc::ptr_eq(g, is_active) && g.load(Ordering::Acquire));
        if entries.len() != before {
            if entries.is_empty() {
                page.slots[in_page].store(None);
            } else {
                let snap = Arc::new(DropSlotSnapshot {
                    entries: entries.clone().into_boxed_slice(),
                });
                page.slots[in_page].store(Some(snap));
            }
        }
    }
}

/// Returns the current number of free drop trampoline slots in the global pool (for diagnostics).
pub fn available_drop_trampoline_slots() -> usize {
    lock_unpoison(&DROP_CONTROL).available_slots()
}

/// Returns the current monotonically increasing generation counter for `slot` (for diagnostics).
pub fn drop_slot_generation(slot: usize) -> Option<u64> {
    let (page, in_page) = get_drop_page(slot)?;
    Some(page.generation[in_page].load(Ordering::Acquire))
}

/// Returns the current `live_values` count for `slot` (for diagnostics).
pub fn drop_slot_live_values(slot: usize) -> Option<usize> {
    let (page, in_page) = get_drop_page(slot)?;
    let (_, live) = decode_slot_lifecycle(page.lifecycle[in_page].load(Ordering::Acquire));
    Some(live)
}

/// Returns the number of 64-slot drop pages lazily allocated so far (for diagnostics).
pub fn allocated_drop_pages() -> usize {
    lock_unpoison(&DROP_CONTROL).allocated_pages
}

#[cfg(test)]
pub(crate) static TEST_DROP_SLAB_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
#[path = "../tests/unit/drop_trampoline_tests.rs"]
mod tests;
