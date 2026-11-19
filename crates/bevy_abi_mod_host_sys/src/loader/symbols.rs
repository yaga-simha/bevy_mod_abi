//! World-scoped monotonic UTF-8 symbol interner ([`BamSymbolTable`]) and persistent named-object
//! entity registry ([`ModNamedObjects`]) backing ABI `0.3.0` `intern_symbol` and `bind_named_object`.
//!
//! - [`BamSymbolTable`] assigns dense monotonic [`BamSymbol`] handles (`BamSymbol(0)`, `BamSymbol(1)`,
//!   ...) deduplicated across threads via 16 sharded `RwLock` buckets while serving lock-free
//!   [`BamSymbolTable::resolve_symbol_str`] lookups backed by an append-only paged string arena.
//! - [`ModNamedObjects`] binds canonical UTF-8 object/prefab names and deterministic 128-bit UUIDs
//!   ([`ComponentUuid::from_parts(name, 0, 0, 0)`]) to persistent (`!ModScoped`) Bevy [`Entity`]
//!   handles that survive hot-reload and mod unload.

use crate::handle_header::HostContext;
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
        let mut guard = reg.write().unwrap_or_else(|e| e.into_inner());
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
        {
            let read_guard = shard.read().unwrap_or_else(|e| e.into_inner());
            if let Some(&existing) = read_guard.get(symbol) {
                return Ok(existing);
            }
        }

        let mut write_guard = shard.write().unwrap_or_else(|e| e.into_inner());
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
        let shard = &self.inner.shards[shard_index_for(symbol)];
        let read_guard = shard.read().unwrap_or_else(|e| e.into_inner());
        read_guard.get(symbol).copied()
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
        let mut guard = self.inner.state.write().unwrap_or_else(|e| e.into_inner());
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
        let mut guard = self.inner.state.write().unwrap_or_else(|e| e.into_inner());
        let prev = guard.by_name.remove(name);
        guard.by_uuid.remove(&uuid);
        prev
    }

    /// Unbinds an object by its deterministic [`ComponentUuid`], returning the previously bound
    /// [`Entity`] if any.
    pub fn unbind_by_uuid(&self, uuid: ComponentUuid) -> Option<Entity> {
        let mut guard = self.inner.state.write().unwrap_or_else(|e| e.into_inner());
        let (name_arc, prev) = guard.by_uuid.remove(&uuid)?;
        guard.by_name.remove(name_arc.as_ref());
        Some(prev)
    }

    /// Looks up the bound [`Entity`] for `name`.
    pub fn get(&self, name: &str) -> Option<Entity> {
        let guard = self.inner.state.read().unwrap_or_else(|e| e.into_inner());
        guard.by_name.get(name).copied()
    }

    /// Looks up the bound [`FfiEntity`] for `name`.
    #[inline]
    pub fn get_ffi(&self, name: &str) -> Option<FfiEntity> {
        self.get(name).map(|e| FfiEntity::from_bits(e.to_bits()))
    }

    /// Looks up the bound [`Entity`] by its deterministic 128-bit [`ComponentUuid`].
    pub fn get_by_uuid(&self, uuid: ComponentUuid) -> Option<Entity> {
        let guard = self.inner.state.read().unwrap_or_else(|e| e.into_inner());
        guard.by_uuid.get(&uuid).map(|(_, ent)| *ent)
    }

    /// Looks up the canonical UTF-8 name associated with a bound [`ComponentUuid`].
    pub fn get_name_by_uuid(&self, uuid: ComponentUuid) -> Option<Arc<str>> {
        let guard = self.inner.state.read().unwrap_or_else(|e| e.into_inner());
        guard.by_uuid.get(&uuid).map(|(name, _)| Arc::clone(name))
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
        let guard = self.inner.state.read().unwrap_or_else(|e| e.into_inner());
        guard.by_name.contains_key(name)
    }

    /// Returns the number of bound named objects.
    pub fn len(&self) -> usize {
        let guard = self.inner.state.read().unwrap_or_else(|e| e.into_inner());
        guard.by_name.len()
    }

    /// Returns `true` if no named objects are bound.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns a cloned snapshot of all `(name, uuid, entity)` bindings.
    pub fn entries(&self) -> Vec<(Arc<str>, ComponentUuid, Entity)> {
        let guard = self.inner.state.read().unwrap_or_else(|e| e.into_inner());
        guard
            .by_uuid
            .iter()
            .map(|(&uuid, (name, entity))| (Arc::clone(name), uuid, *entity))
            .collect()
    }

    /// Removes any bindings whose [`Entity`] has been despawned in `world`, returning the number
    /// of pruned entries.
    pub fn prune_despawned(&self, world: &World) -> usize {
        let mut guard = self.inner.state.write().unwrap_or_else(|e| e.into_inner());
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
            let guard = objects
                .inner
                .state
                .read()
                .unwrap_or_else(|e| e.into_inner());
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

    if !world.contains_resource::<BamSymbolTable>() {
        world.insert_resource(BamSymbolTable::new());
    }
    if !world.contains_resource::<ModNamedObjects>() {
        world.insert_resource(ModNamedObjects::new());
    }

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

    {
        let mut guard = world_table_registry()
            .write()
            .unwrap_or_else(|e| e.into_inner());
        guard.insert(
            world_id,
            WorldTableEntry {
                symbols: sym_weak,
                named_objects: obj_weak,
            },
        );
    }

    if let Some(mut registry) = world.get_resource_mut::<ModRegistry>() {
        registry.attach_world_tables_id(world_id);
    }

    world_id
}

pub(crate) fn lookup_symbol_table_by_id(world_id: usize) -> Option<BamSymbolTable> {
    if world_id == 0 {
        return None;
    }
    let guard = world_table_registry()
        .read()
        .unwrap_or_else(|e| e.into_inner());
    let inner = guard.get(&world_id)?.symbols.upgrade()?;
    Some(BamSymbolTable { inner })
}

pub(crate) fn lookup_named_objects_by_id(world_id: usize) -> Option<ModNamedObjects> {
    if world_id == 0 {
        return None;
    }
    let guard = world_table_registry()
        .read()
        .unwrap_or_else(|e| e.into_inner());
    let inner = guard.get(&world_id)?.named_objects.upgrade()?;
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
mod tests {
    use super::*;
    use crate::loader::mod_host::ModId;

    #[test]
    fn symbol_table_monotonic_dedup_and_lock_free_resolve() {
        let table = BamSymbolTable::new();
        assert!(table.is_empty());
        assert_eq!(table.intern(""), Err(FfiResult::INVALID_ARGUMENT));
        assert_eq!(table.resolve_symbol_str(BamSymbol::INVALID), None);
        assert_eq!(table.resolve_symbol_str(BamSymbol::from_raw(0)), None);

        let s0 = table.intern("combat::crit").expect("intern s0");
        let s1 = table.intern("combat::burn").expect("intern s1");
        let s0_dup = table.intern("combat::crit").expect("dedup s0");
        assert_eq!(s0, BamSymbol::from_raw(0));
        assert_eq!(s1, BamSymbol::from_raw(1));
        assert_eq!(s0_dup, s0);
        assert_eq!(table.len(), 2);

        // Borrow `&str` directly while concurrently interning additional symbols.
        let borrowed = table.resolve_symbol_str(s0).expect("resolve s0");
        let s2 = table.intern("ai::patrol").expect("intern s2");
        assert_eq!(borrowed, "combat::crit");
        assert_eq!(table.resolve_symbol_str(s1), Some("combat::burn"));
        assert_eq!(table.resolve_symbol_str(s2), Some("ai::patrol"));
        assert_eq!(table.lookup("ai::patrol"), Some(s2));
        assert_eq!(table.lookup("missing"), None);
    }

    #[test]
    fn named_objects_uuid_binding_rebind_and_mod_scoped_stripping() {
        let mut world = World::new();
        let e1 = world.spawn(ModScoped(ModId(7))).id();
        let e2 = world.spawn_empty().id();

        let uuid = ModNamedObjects::bind_persistent(&mut world, "prefab::Hero", e1)
            .expect("bind_persistent succeeds");
        assert_eq!(uuid, ComponentUuid::from_parts("prefab::Hero", 0, 0, 0));
        assert!(
            !world.entity(e1).contains::<ModScoped>(),
            "bind_persistent must strip ModScoped"
        );

        let objs = world.resource::<ModNamedObjects>();
        assert_eq!(objs.get("prefab::Hero"), Some(e1));
        assert_eq!(objs.get_by_uuid(uuid), Some(e1));
        assert_eq!(objs.get_alive(&world, "prefab::Hero"), Some(e1));
        assert_eq!(objs.get_name_by_uuid(uuid).as_deref(), Some("prefab::Hero"));

        // Rebind to e2.
        objs.bind("prefab::Hero", e2);
        assert_eq!(objs.get("prefab::Hero"), Some(e2));
        assert_eq!(objs.get_by_uuid(uuid), Some(e2));

        // Despawning e2 makes `get_alive` return None and `prune_despawned` cleans it up.
        assert!(world.despawn(e2));
        let objs = world.resource::<ModNamedObjects>();
        assert_eq!(objs.get_alive(&world, "prefab::Hero"), None);
        assert_eq!(objs.prune_despawned(&world), 1);
        assert!(objs.is_empty());

        // `get_or_spawn_persistent` materializes a fresh persistent entity.
        let spawned = ModNamedObjects::get_or_spawn_persistent(&mut world, "prefab::Boss");
        assert!(world.entities().contains(spawned));
        assert!(!world.entity(spawned).contains::<ModScoped>());
        assert_eq!(
            ModNamedObjects::get_or_spawn_persistent(&mut world, "prefab::Boss"),
            spawned
        );
    }

    #[test]
    fn world_drop_cleans_up_registry_slot_without_leaks() {
        let world_id = {
            let mut world = World::new();
            world.insert_resource(ModRegistry::new());
            let id = ensure_symbol_tables_in_world(&mut world);
            assert!(lookup_symbol_table_by_id(id).is_some());
            assert!(lookup_named_objects_by_id(id).is_some());
            id
        };
        assert!(lookup_symbol_table_by_id(world_id).is_none());
        assert!(lookup_named_objects_by_id(world_id).is_none());
    }

    #[test]
    fn named_object_survives_full_excise_unload_and_symbols_persist_across_reload() {
        use crate::drop_trampoline::WorldDropGuard;
        use crate::ffi::register::{LoadContext, host_load_host_ctx};
        use crate::ffi::v03_ext::{host_bind_named_object, host_intern_symbol};
        use crate::loader::mod_host::{LoadedMod, ModHost, excise_and_unload_mod_in_world};
        use crate::system::ModQuarantine;
        use bevy_abi_mod_sys::{BamInstance, FfiEntity, StrView};
        use core::sync::atomic::AtomicBool;

        unsafe extern "C" fn noop_shutdown(_inst: *mut BamInstance) {}

        let mut world = World::new();
        world.insert_resource(WorldDropGuard::new());
        world.insert_resource(ModHost::new());
        world.insert_resource(ModRegistry::new());
        world.insert_resource(ModQuarantine::new());

        let active_1 = Arc::new(AtomicBool::new(true));
        let mod_1 = LoadedMod::register_mock_for_test(
            &mut world,
            "prefab_unload_mod",
            Arc::clone(&active_1),
            noop_shutdown,
            None,
            core::ptr::null_mut(),
        );

        let bound_ent = world.spawn(ModScoped(mod_1)).id();
        let unbound_ent = world.spawn(ModScoped(mod_1)).id();

        let mut sym_before = BamSymbol::INVALID;
        {
            let reg = world.resource::<ModRegistry>().clone();
            let world_ptr = &raw mut world;
            let mut load_ctx =
                LoadContext::with_world_and_mod_id(reg, active_1, world_ptr, Some(mod_1));
            // SAFETY: `load_ctx` is live.
            let host_ctx = unsafe { host_load_host_ctx(load_ctx.as_ffi_ptr()) };
            assert_eq!(
                // SAFETY: Valid `host_ctx`, UTF-8 string, and entity bits.
                unsafe {
                    host_bind_named_object(
                        host_ctx,
                        StrView::from_str("world::spawn_point"),
                        FfiEntity::from_bits(bound_ent.to_bits()),
                    )
                },
                FfiResult::OK
            );
            assert_eq!(
                // SAFETY: Valid `host_ctx`, UTF-8 string, and out pointer.
                unsafe {
                    host_intern_symbol(
                        host_ctx,
                        StrView::from_str("event::portal"),
                        &raw mut sym_before,
                    )
                },
                FfiResult::OK
            );
            load_ctx.finish_init();
        }

        // Full unload despawns unbound ModScoped entities, while bound entity is stripped of
        // ModScoped and survives!
        excise_and_unload_mod_in_world(&mut world, mod_1, |_| {}).expect("unload succeeds");
        assert!(world.entities().contains(bound_ent));
        assert!(world.get::<ModScoped>(bound_ent).is_none());
        assert!(!world.entities().contains(unbound_ent));

        // Reloaded mod sees the exact same symbol ID and named entity!
        let active_2 = Arc::new(AtomicBool::new(true));
        let mod_2 = LoadedMod::register_mock_for_test(
            &mut world,
            "prefab_unload_mod",
            Arc::clone(&active_2),
            noop_shutdown,
            None,
            core::ptr::null_mut(),
        );
        let mut sym_after = BamSymbol::INVALID;
        {
            let reg = world.resource::<ModRegistry>().clone();
            let world_ptr = &raw mut world;
            let mut load_ctx =
                LoadContext::with_world_and_mod_id(reg, active_2, world_ptr, Some(mod_2));
            // SAFETY: `load_ctx` is live.
            let host_ctx = unsafe { host_load_host_ctx(load_ctx.as_ffi_ptr()) };
            assert_eq!(
                // SAFETY: Valid `host_ctx`, UTF-8 string, and out pointer.
                unsafe {
                    host_intern_symbol(
                        host_ctx,
                        StrView::from_str("event::portal"),
                        &raw mut sym_after,
                    )
                },
                FfiResult::OK
            );
            load_ctx.finish_init();
        }
        assert_eq!(sym_after, sym_before);
        assert_eq!(
            world
                .resource::<ModNamedObjects>()
                .get_alive(&world, "world::spawn_point"),
            Some(bound_ent)
        );
    }
}
