//! World-scoped monotonic UTF-8 symbol interner ([`BamSymbolTable`]) and persistent named-object
//! entity registry ([`ModNamedObjects`]) backing ABI `0.3.0` `intern_symbol` and `bind_named_object`.
//!
//! - [`BamSymbolTable`] assigns dense monotonic [`BamSymbol`] handles (`BamSymbol(0)`, `BamSymbol(1)`,
//!   ...) deduplicated across threads via 16 sharded `RwLock` buckets while serving lock-free
//!   [`BamSymbolTable::resolve_symbol_str`] lookups backed by an append-only paged string arena.
//! - [`ModNamedObjects`] binds canonical UTF-8 object/prefab names and deterministic 128-bit UUIDs
//!   ([`ComponentUuid::from_parts(name, 0, 0, 0)`]) to persistent (`!ModScoped`) Bevy [`Entity`]
//!   handles that survive hot-reload and mod unload.

use crate::handle_header::{HostContext, read_unpoison, write_unpoison};
use crate::loader::mod_host::ModScoped;
use crate::registry::ModRegistry;
use bevy_abi_mod_sys::{BamSymbol, ComponentUuid, FfiEntity, FfiResult};
use bevy_ecs::entity::Entity;
use bevy_ecs::prelude::Resource;
use bevy_ecs::world::World;
use bevy_platform::collections::HashMap;
use core::any::TypeId;
use core::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock, RwLock, Weak};

const SYMBOL_SHARD_COUNT: usize = 16;
const SYMBOL_PAGE_SHIFT: usize = 8;
const SYMBOL_PAGE_SIZE: usize = 1 << SYMBOL_PAGE_SHIFT;

#[cfg(miri)]
const SYMBOL_MAX_PAGES: usize = 64;
#[cfg(not(miri))]
const SYMBOL_MAX_PAGES: usize = 4096;

const SYMBOL_MAX_ENTRIES: usize = SYMBOL_MAX_PAGES * SYMBOL_PAGE_SIZE;

type SymbolPage = [OnceLock<Box<str>>; SYMBOL_PAGE_SIZE];

#[inline]
fn fnv1a_hash_str(s: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in s.as_bytes() {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

#[inline]
fn shard_index_for(s: &str) -> usize {
    let h = fnv1a_hash_str(s);
    ((h ^ (h >> 32)) as usize) & (SYMBOL_SHARD_COUNT - 1)
}

struct WorldTableEntry {
    symbols: Weak<BamSymbolTableInner>,
    named_objects: Weak<ModNamedObjectsInner>,
}

static NEXT_WORLD_TABLES_ID: AtomicUsize = AtomicUsize::new(1);
static WORLD_TABLE_REGISTRY: OnceLock<RwLock<HashMap<usize, WorldTableEntry>>> = OnceLock::new();

#[inline]
fn world_table_registry() -> &'static RwLock<HashMap<usize, WorldTableEntry>> {
    WORLD_TABLE_REGISTRY.get_or_init(|| RwLock::new(HashMap::new()))
}

fn cleanup_world_table_slot(world_id: usize) {
    if world_id == 0 {
        return;
    }
    if let Some(reg) = WORLD_TABLE_REGISTRY.get() {
        let mut guard = write_unpoison(reg);
        let should_remove = guard.get(&world_id).is_some_and(|entry| {
            entry.symbols.strong_count() == 0 && entry.named_objects.strong_count() == 0
        });
        if should_remove {
            guard.remove(&world_id);
        }
    }
}

struct BamSymbolTableInner {
    world_id: AtomicUsize,
    next_id: AtomicUsize,
    shards: [RwLock<HashMap<Arc<str>, BamSymbol>>; SYMBOL_SHARD_COUNT],
    pages: Box<[OnceLock<Box<SymbolPage>>]>,
}

impl Drop for BamSymbolTableInner {
    fn drop(&mut self) {
        cleanup_world_table_slot(self.world_id.load(Ordering::Acquire));
    }
}

/// World-scoped sharded monotonic UTF-8 symbol interner backing [`crate::host_intern_symbol`].
///
/// Assigns stable, dense 0-based [`BamSymbol`] handles (`BamSymbol(0)`, `BamSymbol(1)`, ...)
/// that persist unchanged across mod hot-reloads. Lookups via [`Self::resolve_symbol_str`] are
/// wait-free and return `&str` directly from the append-only paged string arena without locking.
#[derive(Resource, Clone)]
pub struct BamSymbolTable {
    inner: Arc<BamSymbolTableInner>,
}

impl Default for BamSymbolTable {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl core::fmt::Debug for BamSymbolTable {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("BamSymbolTable")
            .field("len", &self.len())
            .finish()
    }
}

impl BamSymbolTable {
    /// Creates an empty [`BamSymbolTable`].
    pub fn new() -> Self {
        let shards = core::array::from_fn(|_| RwLock::new(HashMap::new()));
        let mut pages_vec = Vec::with_capacity(SYMBOL_MAX_PAGES);
        pages_vec.resize_with(SYMBOL_MAX_PAGES, OnceLock::new);
        Self {
            inner: Arc::new(BamSymbolTableInner {
                world_id: AtomicUsize::new(0),
                next_id: AtomicUsize::new(0),
                shards,
                pages: pages_vec.into_boxed_slice(),
            }),
        }
    }

    /// Interns a non-empty UTF-8 string into a stable monotonic [`BamSymbol`] handle.
    ///
    /// Identical UTF-8 strings always deduplicate to the same [`BamSymbol`] across threads and
    /// mod reloads. Empty strings are rejected with [`FfiResult::INVALID_ARGUMENT`].
    pub fn intern(&self, symbol: &str) -> Result<BamSymbol, FfiResult> {
        if symbol.is_empty() {
            return Err(FfiResult::INVALID_ARGUMENT);
        }
        let shard = &self.inner.shards[shard_index_for(symbol)];
        if let Some(&existing) = read_unpoison(shard).get(symbol) {
            return Ok(existing);
        }

        let mut write_guard = write_unpoison(shard);
        if let Some(&existing) = write_guard.get(symbol) {
            return Ok(existing);
        }

        let mut cur = self.inner.next_id.load(Ordering::Acquire);
        let idx = loop {
            if cur >= SYMBOL_MAX_ENTRIES || cur >= u32::MAX as usize {
                return Err(FfiResult::CAPACITY_EXCEEDED);
            }
            match self.inner.next_id.compare_exchange_weak(
                cur,
                cur + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(prev) => break prev,
                Err(actual) => cur = actual,
            }
        };

        let page_idx = idx >> SYMBOL_PAGE_SHIFT;
        let slot_idx = idx & (SYMBOL_PAGE_SIZE - 1);
        let page = self.inner.pages[page_idx]
            .get_or_init(|| Box::new(core::array::from_fn(|_| OnceLock::new())));
        let _ = page[slot_idx].set(symbol.to_owned().into_boxed_str());

        let handle = BamSymbol::from_raw(idx as u32);
        write_guard.insert(Arc::from(symbol), handle);
        Ok(handle)
    }

    /// Looks up an already-interned UTF-8 string without allocating a new handle.
    pub fn lookup(&self, symbol: &str) -> Option<BamSymbol> {
        if symbol.is_empty() {
            return None;
        }
        read_unpoison(&self.inner.shards[shard_index_for(symbol)])
            .get(symbol)
            .copied()
    }

    /// Resolves an interned [`BamSymbol`] back to its UTF-8 string slice (lock-free).
    #[inline]
    pub fn resolve_symbol_str(&self, symbol: BamSymbol) -> Option<&str> {
        if !symbol.is_valid() {
            return None;
        }
        let idx = symbol.to_raw() as usize;
        let page_idx = idx >> SYMBOL_PAGE_SHIFT;
        let slot_idx = idx & (SYMBOL_PAGE_SIZE - 1);
        self.inner
            .pages
            .get(page_idx)?
            .get()?
            .get(slot_idx)?
            .get()
            .map(Box::as_ref)
    }

    /// Returns the total number of distinct symbols interned in this table.
    #[inline]
    pub fn len(&self) -> usize {
        self.inner.next_id.load(Ordering::Acquire)
    }

    /// Returns `true` if no symbols have been interned yet.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[derive(Default)]
struct NamedObjectsState {
    by_name: HashMap<Arc<str>, Entity>,
    by_uuid: HashMap<ComponentUuid, (Arc<str>, Entity)>,
}

struct ModNamedObjectsInner {
    world_id: AtomicUsize,
    state: RwLock<NamedObjectsState>,
}

impl Drop for ModNamedObjectsInner {
    fn drop(&mut self) {
        cleanup_world_table_slot(self.world_id.load(Ordering::Acquire));
    }
}

/// World-scoped persistent named-object / prefab entity registry backing
/// [`crate::host_bind_named_object`].
///
/// Entries are indexed by both canonical UTF-8 name and deterministic 128-bit UUID
/// (`ComponentUuid::from_parts(name, 0, 0, 0)`). Bound entities have [`ModScoped`] stripped
/// automatically before mod teardown/unload so they persist across hot-reloads and unloads.
#[derive(Resource, Clone)]
pub struct ModNamedObjects {
    inner: Arc<ModNamedObjectsInner>,
}

impl Default for ModNamedObjects {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl core::fmt::Debug for ModNamedObjects {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ModNamedObjects")
            .field("len", &self.len())
            .finish()
    }
}

impl ModNamedObjects {
    /// Creates an empty [`ModNamedObjects`] registry.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(ModNamedObjectsInner {
                world_id: AtomicUsize::new(0),
                state: RwLock::new(NamedObjectsState::default()),
            }),
        }
    }

    /// Computes the deterministic 128-bit UUID associated with a canonical named object string
    /// (`ComponentUuid::from_parts(name, 0, 0, 0)`).
    #[inline]
    pub const fn uuid_for_name(name: &str) -> ComponentUuid {
        ComponentUuid::from_parts(name, 0, 0, 0)
    }

    /// Binds or rebinds `name` (and its deterministic UUID) to `entity`, returning its UUID.
    pub fn bind(&self, name: &str, entity: Entity) -> ComponentUuid {
        let uuid = Self::uuid_for_name(name);
        let name_arc: Arc<str> = Arc::from(name);
        let mut guard = write_unpoison(&self.inner.state);
        guard.by_name.insert(Arc::clone(&name_arc), entity);
        guard.by_uuid.insert(uuid, (name_arc, entity));
        uuid
    }

    /// Validates that `entity` is alive in `world`, strips any [`ModScoped`] component so it
    /// persists across reloads/unloads, and binds `name` to `entity` in `world`'s [`ModNamedObjects`].
    pub fn bind_persistent(
        world: &mut World,
        name: &str,
        entity: Entity,
    ) -> Result<ComponentUuid, FfiResult> {
        if name.is_empty() {
            return Err(FfiResult::INVALID_ARGUMENT);
        }
        if entity == Entity::PLACEHOLDER || !world.entities().contains(entity) {
            return Err(FfiResult::INVALID_ENTITY);
        }
        if world
            .components()
            .get_id(TypeId::of::<ModScoped>())
            .is_some()
            && let Ok(mut ent_mut) = world.get_entity_mut(entity)
        {
            ent_mut.remove::<ModScoped>();
        }
        ensure_symbol_tables_in_world(world);
        Ok(world.resource::<Self>().bind(name, entity))
    }

    /// Returns the live entity bound to `name`, or spawns a fresh persistent (`!ModScoped`) entity
    /// in `world`, binds it under `name`, and returns it.
    pub fn get_or_spawn_persistent(world: &mut World, name: &str) -> Entity {
        ensure_symbol_tables_in_world(world);
        if let Some(existing) = world.resource::<Self>().get_alive(world, name) {
            if world
                .components()
                .get_id(TypeId::of::<ModScoped>())
                .is_some()
                && let Ok(mut ent_mut) = world.get_entity_mut(existing)
            {
                ent_mut.remove::<ModScoped>();
            }
            return existing;
        }
        let entity = world.spawn_empty().id();
        world.resource::<Self>().bind(name, entity);
        entity
    }

    /// Unbinds `name` and its deterministic UUID, returning the previously bound [`Entity`] if any.
    pub fn unbind(&self, name: &str) -> Option<Entity> {
        let uuid = Self::uuid_for_name(name);
        let mut guard = write_unpoison(&self.inner.state);
        let prev = guard.by_name.remove(name);
        guard.by_uuid.remove(&uuid);
        prev
    }

    /// Unbinds an object by its deterministic [`ComponentUuid`], returning the previously bound
    /// [`Entity`] if any.
    pub fn unbind_by_uuid(&self, uuid: ComponentUuid) -> Option<Entity> {
        let mut guard = write_unpoison(&self.inner.state);
        let (name_arc, prev) = guard.by_uuid.remove(&uuid)?;
        guard.by_name.remove(name_arc.as_ref());
        Some(prev)
    }

    /// Looks up the bound [`Entity`] for `name`.
    pub fn get(&self, name: &str) -> Option<Entity> {
        read_unpoison(&self.inner.state).by_name.get(name).copied()
    }

    /// Looks up the bound [`FfiEntity`] for `name`.
    #[inline]
    pub fn get_ffi(&self, name: &str) -> Option<FfiEntity> {
        self.get(name).map(|e| FfiEntity::from_bits(e.to_bits()))
    }

    /// Looks up the bound [`Entity`] by its deterministic 128-bit [`ComponentUuid`].
    pub fn get_by_uuid(&self, uuid: ComponentUuid) -> Option<Entity> {
        read_unpoison(&self.inner.state)
            .by_uuid
            .get(&uuid)
            .map(|(_, ent)| *ent)
    }

    /// Looks up the canonical UTF-8 name associated with a bound [`ComponentUuid`].
    pub fn get_name_by_uuid(&self, uuid: ComponentUuid) -> Option<Arc<str>> {
        read_unpoison(&self.inner.state)
            .by_uuid
            .get(&uuid)
            .map(|(name, _)| Arc::clone(name))
    }

    /// Looks up `name` and verifies the bound [`Entity`] is still alive in `world`.
    #[inline]
    pub fn get_alive(&self, world: &World, name: &str) -> Option<Entity> {
        self.get(name).filter(|&e| world.entities().contains(e))
    }

    /// Looks up `uuid` and verifies the bound [`Entity`] is still alive in `world`.
    #[inline]
    pub fn get_alive_by_uuid(&self, world: &World, uuid: ComponentUuid) -> Option<Entity> {
        self.get_by_uuid(uuid)
            .filter(|&e| world.entities().contains(e))
    }

    /// Returns `true` if `name` is currently bound.
    pub fn contains_name(&self, name: &str) -> bool {
        read_unpoison(&self.inner.state).by_name.contains_key(name)
    }

    /// Returns the number of bound named objects.
    pub fn len(&self) -> usize {
        read_unpoison(&self.inner.state).by_name.len()
    }

    /// Returns `true` if no named objects are bound.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns a cloned snapshot of all `(name, uuid, entity)` bindings.
    pub fn entries(&self) -> Vec<(Arc<str>, ComponentUuid, Entity)> {
        read_unpoison(&self.inner.state)
            .by_uuid
            .iter()
            .map(|(&uuid, (name, entity))| (Arc::clone(name), uuid, *entity))
            .collect()
    }

    /// Removes any bindings whose [`Entity`] has been despawned in `world`, returning the number
    /// of pruned entries.
    pub fn prune_despawned(&self, world: &World) -> usize {
        let mut guard = write_unpoison(&self.inner.state);
        let before = guard.by_name.len();
        guard
            .by_name
            .retain(|_, &mut e| world.entities().contains(e));
        guard
            .by_uuid
            .retain(|_, (_, e)| world.entities().contains(*e));
        before.saturating_sub(guard.by_name.len())
    }

    /// Strips [`ModScoped`] from every live bound entity in `world` so named objects/prefabs
    /// always survive mod unload and hot-reload teardown, returning how many entities were promoted.
    pub fn strip_mod_scoped_in_world(world: &mut World) -> usize {
        if world
            .components()
            .get_id(TypeId::of::<ModScoped>())
            .is_none()
        {
            return 0;
        }
        let entities: Vec<Entity> = {
            let Some(objects) = world.get_resource::<Self>() else {
                return 0;
            };
            let guard = read_unpoison(&objects.inner.state);
            if guard.by_name.is_empty() {
                return 0;
            }
            guard.by_name.values().copied().collect()
        };
        let mut stripped = 0usize;
        for entity in entities {
            if let Ok(mut ent_mut) = world.get_entity_mut(entity)
                && ent_mut.contains::<ModScoped>()
            {
                ent_mut.remove::<ModScoped>();
                stripped += 1;
            }
        }
        stripped
    }
}

/// Ensures [`BamSymbolTable`] and [`ModNamedObjects`] Bevy resources exist in `world`, links them
/// in the internal world-table directory, and attaches their world table token to `world`'s
/// [`ModRegistry`] (if present) so [`HostContext`] FFI calls (`host_intern_symbol` and
/// `host_bind_named_object`) resolve them in `O(1)`.
pub fn ensure_symbol_tables_in_world(world: &mut World) -> usize {
    let existing_sym_id = world
        .get_resource::<BamSymbolTable>()
        .map(|t| t.inner.world_id.load(Ordering::Acquire))
        .unwrap_or(0);
    let existing_obj_id = world
        .get_resource::<ModNamedObjects>()
        .map(|o| o.inner.world_id.load(Ordering::Acquire))
        .unwrap_or(0);

    let world_id = if existing_sym_id != 0 {
        existing_sym_id
    } else if existing_obj_id != 0 {
        existing_obj_id
    } else {
        loop {
            let candidate = NEXT_WORLD_TABLES_ID.fetch_add(1, Ordering::Relaxed);
            if candidate != 0 {
                break candidate;
            }
        }
    };

    world.init_resource::<BamSymbolTable>();
    world.init_resource::<ModNamedObjects>();

    let sym_weak = {
        let sym = world.resource::<BamSymbolTable>();
        sym.inner.world_id.store(world_id, Ordering::Release);
        Arc::downgrade(&sym.inner)
    };
    let obj_weak = {
        let obj = world.resource::<ModNamedObjects>();
        obj.inner.world_id.store(world_id, Ordering::Release);
        Arc::downgrade(&obj.inner)
    };

    write_unpoison(world_table_registry()).insert(
        world_id,
        WorldTableEntry {
            symbols: sym_weak,
            named_objects: obj_weak,
        },
    );

    if let Some(mut registry) = world.get_resource_mut::<ModRegistry>() {
        registry.attach_world_tables_id(world_id);
    }

    world_id
}

pub(crate) fn lookup_symbol_table_by_id(world_id: usize) -> Option<BamSymbolTable> {
    if world_id == 0 {
        return None;
    }
    let inner = read_unpoison(world_table_registry())
        .get(&world_id)?
        .symbols
        .upgrade()?;
    Some(BamSymbolTable { inner })
}

pub(crate) fn lookup_named_objects_by_id(world_id: usize) -> Option<ModNamedObjects> {
    if world_id == 0 {
        return None;
    }
    let inner = read_unpoison(world_table_registry())
        .get(&world_id)?
        .named_objects
        .upgrade()?;
    Some(ModNamedObjects { inner })
}

pub(crate) fn intern_symbol_for_host_ctx(
    host_ctx: &HostContext,
    symbol: &str,
) -> Result<BamSymbol, FfiResult> {
    let Some(world_id) = host_ctx.snapshot().world_tables_id() else {
        host_ctx
            .set_last_error("intern_symbol: HostContext is not bound to a World BamSymbolTable");
        return Err(FfiResult::INVALID_STATE);
    };
    let Some(table) = lookup_symbol_table_by_id(world_id) else {
        host_ctx.set_last_error("intern_symbol: backing World BamSymbolTable has been dropped");
        return Err(FfiResult::INVALID_STATE);
    };
    table.intern(symbol)
}

pub(crate) fn bind_named_object_for_host_ctx(
    host_ctx: &HostContext,
    name: &str,
    entity: Entity,
) -> Result<(), FfiResult> {
    let Some(world_id) = host_ctx.snapshot().world_tables_id() else {
        host_ctx.set_last_error(
            "bind_named_object: HostContext is not bound to a World ModNamedObjects",
        );
        return Err(FfiResult::INVALID_STATE);
    };
    let Some(objects) = lookup_named_objects_by_id(world_id) else {
        host_ctx
            .set_last_error("bind_named_object: backing World ModNamedObjects has been dropped");
        return Err(FfiResult::INVALID_STATE);
    };
    objects.bind(name, entity);
    Ok(())
}

#[cfg(test)]
#[path = "../../tests/unit/symbols_tests.rs"]
mod tests;
