//! Copy-on-Write (`CoW`) component, resource, VTable, and `"bam.v0.*"` symbol catalog
//! (`C-03..C-05`, `L-07`, `P-08`, `B-01`, `B-08`).

use crate::loader::ModId;
use crate::registry::collision::ComponentCollisionError;
use crate::sync_ptr::SymbolAddress;
use arc_swap::ArcSwap;
use bevy_abi_mod_sys::{
    BAM_COMPONENT_FLAG_IMMUTABLE, BAM_COMPONENT_FLAG_SPARSE_SET, BAM_STORAGE_FLAG_IMMUTABLE,
    BAM_STORAGE_FLAG_RESOURCE, BAM_STORAGE_FLAG_SPARSE_SET, BamComponent, ComponentDesc,
    ComponentDropFn, ComponentUuid, ModComponent, RawInsertable,
};
use bevy_ecs::component::ComponentId;
use bevy_ecs::entity::Entity;
use bevy_ecs::prelude::{Component, Resource};
use bevy_ecs::world::World;
use bevy_platform::collections::HashMap;
use core::ffi::c_void;
use core::marker::PhantomData;
use core::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Function signature for monomorphized raw component byte insertion into a Bevy [`World`].
///
/// Takes a raw `*const u8` pointer (backed by `MaybeUninit<u8>` arena storage) rather than `&[u8]`
/// so uninitialized struct padding bytes never form a Rust `[u8]` slice (`S-04`).
pub type InsertRawFn = unsafe fn(world: &mut World, entity: Entity, data: *const u8);

/// Function signature for monomorphized `Default` component insertion into a Bevy [`World`].
pub type InsertDefaultFn = fn(world: &mut World, entity: Entity);

/// Monomorphized typed component insertion function using unaligned memory reading.
///
/// # Safety
/// Caller must ensure `T` has no drop glue or heap pointers and `data` points to `size_of::<T>()`
/// readable bytes representing a valid bit-pattern for `T` (padding bytes may be uninitialized).
unsafe fn insert_raw_impl<T: Component + ModComponent>(
    world: &mut World,
    entity: Entity,
    data: *const u8,
) {
    if core::mem::size_of::<T>() > 0 && data.is_null() {
        return;
    }
    let comp: T = if core::mem::size_of::<T>() == 0 {
        // SAFETY: `T` is zero-sized and `T::IS_RAW_INSERTABLE && !needs_drop::<T>()` holds.
        unsafe { core::mem::MaybeUninit::<T>::zeroed().assume_init() }
    } else {
        // SAFETY: `data` is non-null and holds `size_of::<T>()` bytes for `T`; `read_unaligned`
        // copies `T` directly without forming `&[u8]` over padding bytes.
        unsafe { core::ptr::read_unaligned(data.cast::<T>()) }
    };
    if let Ok(mut e) = world.get_entity_mut(entity) {
        e.insert(comp);
    }
}

/// Monomorphized host-side `T::default()` component constructor and inserter.
fn insert_default_impl<T: Component + Default>(world: &mut World, entity: Entity) {
    if let Ok(mut e) = world.get_entity_mut(entity) {
        e.insert(T::default());
    }
}

/// Fluent registration options configuring how a host Rust [`Component`] is exposed to guest mods
/// (`B-01`, ADR 0006).
///
/// By default (`ComponentOptions::default()`), raw byte insertion (`cmd_insert` / `cmd_spawn`) is
/// enabled automatically when `T::IS_RAW_INSERTABLE && !needs_drop::<T>()`, and disabled otherwise.
/// Call [`.opaque()`](Self::opaque) to force VTable-only access even for `RawInsertable` types, or
/// [`.with_default()`](Self::with_default) to expose host-constructed `T::default()` via `cmd_insert_default`.
pub struct ComponentOptions<T> {
    allow_raw_insert: bool,
    insert_default_fn: Option<InsertDefaultFn>,
    extra_storage_flags: u32,
    _marker: PhantomData<fn() -> T>,
}

impl<T> Clone for ComponentOptions<T> {
    #[inline]
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for ComponentOptions<T> {}

impl<T> core::fmt::Debug for ComponentOptions<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ComponentOptions")
            .field("allow_raw_insert", &self.allow_raw_insert)
            .field("has_default", &self.insert_default_fn.is_some())
            .field("extra_storage_flags", &self.extra_storage_flags)
            .finish()
    }
}

impl<T: Component + ModComponent> Default for ComponentOptions<T> {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Component + ModComponent> ComponentOptions<T> {
    /// Creates default options for `T`, automatically enabling raw byte insertion iff
    /// `T::IS_RAW_INSERTABLE && !core::mem::needs_drop::<T>()`.
    #[inline]
    pub const fn new() -> Self {
        Self {
            allow_raw_insert: T::IS_RAW_INSERTABLE && !core::mem::needs_drop::<T>(),
            insert_default_fn: None,
            extra_storage_flags: component_flags_to_storage_flags(T::COMPONENT_FLAGS),
            _marker: PhantomData,
        }
    }

    /// Disables raw byte insertion (`cmd_insert` and `cmd_spawn` will return
    /// [`bevy_abi_mod_sys::FfiResult::PERMISSION_DENIED`]), restricting guest mutations to VTable
    /// methods or [`Self::with_default`].
    #[inline]
    pub const fn opaque(mut self) -> Self {
        self.allow_raw_insert = false;
        self
    }

    /// Explicitly enables raw byte insertion for `T: RawInsertable`.
    #[inline]
    pub const fn with_raw_insert(mut self) -> Self
    where
        T: RawInsertable,
    {
        self.allow_raw_insert = !core::mem::needs_drop::<T>();
        self
    }

    /// Exposes host-side `T::default()` construction for `cmd_insert_default`.
    #[inline]
    pub fn with_default(mut self) -> Self
    where
        T: Default,
    {
        self.insert_default_fn = Some(insert_default_impl::<T>);
        self
    }

    /// Marks this component registration as immutable (`BAM_STORAGE_FLAG_IMMUTABLE`), rejecting
    /// guest `WRITE` and `OPTIONAL_WRITE` query access with `PERMISSION_DENIED`.
    #[inline]
    pub const fn immutable(mut self) -> Self {
        self.extra_storage_flags |= BAM_STORAGE_FLAG_IMMUTABLE;
        self
    }

    /// Marks this component registration as sparse-set stored (`BAM_STORAGE_FLAG_SPARSE_SET`).
    #[inline]
    pub const fn sparse_set(mut self) -> Self {
        self.extra_storage_flags |= BAM_STORAGE_FLAG_SPARSE_SET;
        self
    }

    /// Returns additional `BAM_STORAGE_FLAG_*` bits configured on these options.
    #[inline]
    pub const fn extra_storage_flags(&self) -> u32 {
        self.extra_storage_flags
    }
}

#[inline]
pub(crate) const fn component_flags_to_storage_flags(component_flags: u32) -> u32 {
    let mut out = 0u32;
    if (component_flags & BAM_COMPONENT_FLAG_SPARSE_SET) != 0 {
        out |= BAM_STORAGE_FLAG_SPARSE_SET;
    }
    if (component_flags & BAM_COMPONENT_FLAG_IMMUTABLE) != 0 {
        out |= BAM_STORAGE_FLAG_IMMUTABLE;
    }
    out
}

#[derive(Clone, Copy, Debug)]
struct SymbolRecord {
    addr: SymbolAddress,
    owner: Option<ModId>,
}

/// Catalog record stored for each component or singleton resource in [`RegistrySnapshot`].
#[derive(Clone, Debug)]
pub struct RegisteredComponent {
    /// 1-based [`BamComponent`] handle (`index == (handle.0 - 1) as usize`).
    pub handle: BamComponent,
    /// Canonical namespaced component or resource name (e.g. `"my_game::Position"`).
    pub name: Arc<str>,
    /// Deterministic 128-bit UUID.
    pub uuid: ComponentUuid,
    /// Runtime Bevy [`ComponentId`].
    pub component_id: ComponentId,
    /// Byte size (`size_of::<T>()`).
    pub size: usize,
    /// Byte alignment (`align_of::<T>()`).
    pub align: usize,
    /// Bitmask of `BAM_STORAGE_FLAG_*` (`SPARSE_SET`, `IMMUTABLE`, `RESOURCE`).
    pub storage_flags: u32,
    /// Whether this type satisfies cross-language `AbiPod`.
    pub is_abi_pod: bool,
    /// Whether this component was registered dynamically by a guest mod at runtime.
    pub is_dynamic: bool,
    /// Address of the C-ABI VTable (`SymbolAddress::NULL` if none or evicted on unload).
    pub vtable_ptr: SymbolAddress,
    /// Monomorphized raw byte inserter (`None` for dynamic, opaque, or resource entries).
    pub insert_raw_fn: Option<InsertRawFn>,
    /// Optional host-side `T::default()` constructor for `cmd_insert_default`.
    pub insert_default_fn: Option<InsertDefaultFn>,
    /// Optional foreign drop callback (cleared on guest unload).
    pub drop_fn: Option<ComponentDropFn>,
    /// Optional drop trampoline slot index assigned in [`crate::WorldDropGuard`].
    pub drop_slot: Option<usize>,
    /// Optional [`ModId`] owning a dynamic component's VTable and drop callback.
    pub owner: Option<ModId>,
}

impl RegisteredComponent {
    /// Returns `true` if this entry represents a singleton Bevy `Resource`.
    #[inline]
    pub const fn is_resource(&self) -> bool {
        (self.storage_flags & BAM_STORAGE_FLAG_RESOURCE) != 0
    }

    /// Returns `true` if this entry uses `StorageType::SparseSet`.
    #[inline]
    pub const fn is_sparse_set(&self) -> bool {
        (self.storage_flags & BAM_STORAGE_FLAG_SPARSE_SET) != 0
    }

    /// Returns `true` if mutable guest access (`WRITE`, `OPTIONAL_WRITE`, `RESOURCE_WRITE`) is forbidden.
    #[inline]
    pub const fn is_immutable(&self) -> bool {
        (self.storage_flags & BAM_STORAGE_FLAG_IMMUTABLE) != 0
    }
}

/// Immutable dense snapshot shared via `Arc` across `ModRegistry`, `HostContext`, and `GuestSystem`
/// (`L-07`, `P-08`).
#[derive(Clone, Default)]
pub struct RegistrySnapshot {
    entries: Vec<RegisteredComponent>,
    by_name: HashMap<Arc<str>, BamComponent>,
    by_uuid: HashMap<ComponentUuid, BamComponent>,
    by_component_id: HashMap<ComponentId, BamComponent>,
    symbols: HashMap<Arc<str>, SymbolRecord>,
}

impl RegistrySnapshot {
    /// Looks up a component or resource in `O(1)` by its 1-based [`BamComponent`] handle (`C-05`).
    #[inline]
    pub fn get_by_handle(&self, handle: BamComponent) -> Option<&RegisteredComponent> {
        let idx = handle.0.checked_sub(1)? as usize;
        self.entries.get(idx)
    }

    /// Looks up a component or resource by its runtime Bevy [`ComponentId`].
    #[inline]
    pub fn get_by_id(&self, id: ComponentId) -> Option<&RegisteredComponent> {
        let &handle = self.by_component_id.get(&id)?;
        self.get_by_handle(handle)
    }

    /// Looks up a component or resource by canonical string name.
    #[inline]
    pub fn get_by_name(&self, name: &str) -> Option<&RegisteredComponent> {
        let &handle = self.by_name.get(name)?;
        self.get_by_handle(handle)
    }

    /// Looks up a component or resource by 128-bit [`ComponentUuid`].
    #[inline]
    pub fn get_by_uuid(&self, uuid: &ComponentUuid) -> Option<&RegisteredComponent> {
        let &handle = self.by_uuid.get(uuid)?;
        self.get_by_handle(handle)
    }

    /// Resolves a `"bam.v0.*"` symbol or VTable address in this snapshot (`C-04`).
    #[inline]
    pub fn resolve_symbol(&self, name: &str) -> Option<*const c_void> {
        if !name.starts_with("bam.v0.") {
            return None;
        }
        let rec = self.symbols.get(name)?;
        if rec.addr.is_null() {
            None
        } else {
            Some(rec.addr.as_ptr())
        }
    }

    /// Returns the total number of registered components and resources.
    #[inline]
    pub fn component_count(&self) -> usize {
        self.entries.len()
    }

    /// Returns the total number of registered `"bam.v0.*"` symbols in this snapshot.
    #[inline]
    pub fn symbol_count(&self) -> usize {
        self.symbols.len()
    }

    /// Returns the dense slice of all registered component/resource entries.
    #[inline]
    pub fn entries(&self) -> &[RegisteredComponent] {
        &self.entries
    }
}

struct RegistryTracker {
    latest: ArcSwap<RegistrySnapshot>,
    generation: AtomicU64,
}

/// World-scoped Copy-on-Write (`CoW`) component, resource, and `"bam.v0.*"` symbol registry (`L-07`).
///
/// Cloning `ModRegistry` is an `O(1)` `Arc` bump. Mutations run on cold setup/load/unload paths via
/// `Arc::make_mut` and increment `generation` so `GuestSystem`s refresh their cached snapshot only
/// when the registry actually changes.
#[derive(Resource, Clone)]
pub struct ModRegistry {
    snapshot: Arc<RegistrySnapshot>,
    generation: u64,
    tracker: Arc<RegistryTracker>,
}

impl Default for ModRegistry {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl ModRegistry {
    /// Creates an empty [`ModRegistry`] with **zero** built-in function symbols (`C-03`).
    #[inline]
    pub fn new() -> Self {
        let initial = Arc::new(RegistrySnapshot::default());
        let tracker = Arc::new(RegistryTracker {
            latest: ArcSwap::from(Arc::clone(&initial)),
            generation: AtomicU64::new(1),
        });
        Self {
            snapshot: initial,
            generation: 1,
            tracker,
        }
    }

    /// Returns the current monotonic mutation generation of this registry (`>= 1`).
    #[inline]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// Reads the latest published generation across this registry's shared `World` lineage (`O(1)`).
    #[inline]
    pub fn latest_generation(&self) -> u64 {
        self.tracker.generation.load(Ordering::Acquire)
    }

    /// Loads the latest published `Arc<RegistrySnapshot>` across this registry's `World` lineage.
    #[inline]
    pub fn latest_snapshot_arc(&self) -> Arc<RegistrySnapshot> {
        self.tracker.latest.load_full()
    }

    /// Borrows the current [`RegistrySnapshot`].
    #[inline]
    pub fn snapshot(&self) -> &RegistrySnapshot {
        &self.snapshot
    }

    /// Clones the `Arc<RegistrySnapshot>` handle in `O(1)`.
    #[inline]
    pub fn snapshot_arc(&self) -> Arc<RegistrySnapshot> {
        Arc::clone(&self.snapshot)
    }

    /// Restores a previously captured `Arc<RegistrySnapshot>` (used by transactional load rollback)
    /// and advances `generation`.
    pub(crate) fn restore_snapshot(&mut self, snapshot: Arc<RegistrySnapshot>) {
        self.snapshot = Arc::clone(&snapshot);
        let next_gen = self
            .tracker
            .generation
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1);
        self.generation = next_gen;
        self.tracker.latest.store(snapshot);
    }

    #[inline]
    fn publish_after_mutation(&mut self) {
        self.tracker.latest.store(Arc::clone(&self.snapshot));
        let next_gen = self
            .tracker
            .generation
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1);
        self.generation = next_gen;
    }

    /// Validates that registering `(name, uuid, size, align, storage_flags, expected_comp_id)` does
    /// not collide with an incompatible existing component or resource entry (`C-10`).
    pub fn check_registration_collision(
        &self,
        name: &str,
        uuid: ComponentUuid,
        size: usize,
        align: usize,
        storage_flags: u32,
        expected_comp_id: Option<ComponentId>,
    ) -> Result<(), ComponentCollisionError> {
        if let Some(existing_by_uuid) = self.snapshot.get_by_uuid(&uuid)
            && existing_by_uuid.name.as_ref() != name
        {
            return Err(ComponentCollisionError::UuidCollision {
                existing_name: existing_by_uuid.name.to_string(),
                new_name: name.to_string(),
                uuid,
            });
        }

        if let Some(existing) = self.snapshot.get_by_name(name) {
            if existing.uuid != uuid {
                return Err(ComponentCollisionError::MetadataMismatch {
                    name: name.to_string(),
                    reason: format!("UUID mismatch (existing={:?}, new={uuid:?})", existing.uuid),
                });
            }
            if existing.size != size || existing.align != align {
                return Err(ComponentCollisionError::MetadataMismatch {
                    name: name.to_string(),
                    reason: format!(
                        "layout mismatch (existing=({}, {}), new=({}, {}))",
                        existing.size, existing.align, size, align
                    ),
                });
            }
            if existing.storage_flags != storage_flags {
                return Err(ComponentCollisionError::MetadataMismatch {
                    name: name.to_string(),
                    reason: format!(
                        "storage_flags mismatch (existing=0x{:x}, new=0x{:x})",
                        existing.storage_flags, storage_flags
                    ),
                });
            }
            if let Some(comp_id) = expected_comp_id
                && existing.component_id != comp_id
            {
                return Err(ComponentCollisionError::MetadataMismatch {
                    name: name.to_string(),
                    reason: format!(
                        "ComponentId mismatch (existing={:?}, new={:?})",
                        existing.component_id, comp_id
                    ),
                });
            }
        }

        Ok(())
    }

    /// Validates that dynamically re-registering `name` with `has_drop_fn` is compatible with the
    /// destructor configuration permanently bound in Bevy's `ComponentInfo`.
    pub fn check_dynamic_drop_compatibility(
        &self,
        name: &str,
        has_drop_fn: bool,
    ) -> Result<(), ComponentCollisionError> {
        if let Some(existing) = self.snapshot.get_by_name(name) {
            if !existing.is_dynamic && has_drop_fn {
                return Err(ComponentCollisionError::MetadataMismatch {
                    name: name.to_string(),
                    reason: "cannot attach foreign drop_fn to host-owned component".to_string(),
                });
            }
            if existing.drop_slot.is_some() != has_drop_fn {
                return Err(ComponentCollisionError::MetadataMismatch {
                    name: name.to_string(),
                    reason: format!(
                        "drop_fn presence mismatch on re-registration (existing_has_drop={}, new_has_drop={has_drop_fn})",
                        existing.drop_slot.is_some()
                    ),
                });
            }
        }
        Ok(())
    }

    /// Registers a host [`Component`] implementing [`ModComponent`] with [`ComponentOptions`],
    /// returning its 1-based [`BamComponent`] handle (`B-01`, `C-05`).
    pub fn register_component<T: Component + ModComponent>(
        &mut self,
        comp_id: ComponentId,
        options: ComponentOptions<T>,
    ) -> Result<BamComponent, ComponentCollisionError> {
        self.register_component_with_world_flags::<T>(comp_id, options, 0)
    }

    /// Internal helper merging Bevy's `ComponentInfo` storage/mutability flags (`SPARSE_SET`, `IMMUTABLE`)
    /// with `ComponentOptions<T>`.
    pub fn register_component_with_world_flags<T: Component + ModComponent>(
        &mut self,
        comp_id: ComponentId,
        options: ComponentOptions<T>,
        inferred_storage_flags: u32,
    ) -> Result<BamComponent, ComponentCollisionError> {
        let storage_flags = (options.extra_storage_flags
            | inferred_storage_flags
            | component_flags_to_storage_flags(T::COMPONENT_FLAGS))
            & !BAM_STORAGE_FLAG_RESOURCE;
        let insert_raw_fn: Option<InsertRawFn> =
            if options.allow_raw_insert && !core::mem::needs_drop::<T>() {
                Some(insert_raw_impl::<T>)
            } else {
                None
            };
        self.insert_host_entry::<T>(
            comp_id,
            storage_flags,
            insert_raw_fn,
            options.insert_default_fn,
        )
    }

    /// Registers a singleton Bevy [`Resource`] implementing [`ModComponent`], returning its
    /// 1-based [`BamComponent`] handle (`B-01`, `C-05`).
    pub fn register_resource<R: Resource + ModComponent>(
        &mut self,
        comp_id: ComponentId,
    ) -> Result<BamComponent, ComponentCollisionError> {
        let storage_flags =
            component_flags_to_storage_flags(R::COMPONENT_FLAGS) | BAM_STORAGE_FLAG_RESOURCE;
        self.insert_host_entry::<R>(comp_id, storage_flags, None, None)
    }

    fn insert_host_entry<T: Component + ModComponent>(
        &mut self,
        comp_id: ComponentId,
        storage_flags: u32,
        insert_raw_fn: Option<InsertRawFn>,
        insert_default_fn: Option<InsertDefaultFn>,
    ) -> Result<BamComponent, ComponentCollisionError> {
        let name_str = T::CANONICAL_NAME;
        let uuid = T::UUID;
        self.check_registration_collision(
            name_str,
            uuid,
            T::size(),
            T::align(),
            storage_flags,
            Some(comp_id),
        )?;

        let sym_addr = SymbolAddress::new(T::erased_vtable_ptr());
        let handle = {
            let snap = Arc::make_mut(&mut self.snapshot);
            let handle = if let Some(&existing_handle) = snap.by_name.get(name_str) {
                let idx = (existing_handle.0 - 1) as usize;
                let merged_default = insert_default_fn.or(snap.entries[idx].insert_default_fn);
                let merged_raw = insert_raw_fn.or(snap.entries[idx].insert_raw_fn);
                snap.entries[idx].vtable_ptr = sym_addr;
                snap.entries[idx].insert_raw_fn = merged_raw;
                snap.entries[idx].insert_default_fn = merged_default;
                snap.entries[idx].is_dynamic = false;
                snap.entries[idx].owner = None;
                existing_handle
            } else {
                let next_raw = u32::try_from(snap.entries.len() + 1)
                    .expect("ModRegistry exceeded u32::MAX component handles");
                let handle = BamComponent::from_raw(next_raw);
                let arc_name: Arc<str> = Arc::from(name_str);
                let entry = RegisteredComponent {
                    handle,
                    name: Arc::clone(&arc_name),
                    uuid,
                    component_id: comp_id,
                    size: T::size(),
                    align: T::align(),
                    storage_flags,
                    is_abi_pod: T::IS_ABI_POD,
                    is_dynamic: false,
                    vtable_ptr: sym_addr,
                    insert_raw_fn,
                    insert_default_fn,
                    drop_fn: None,
                    drop_slot: None,
                    owner: None,
                };
                snap.entries.push(entry);
                snap.by_name.insert(arc_name, handle);
                snap.by_uuid.insert(uuid, handle);
                snap.by_component_id.insert(comp_id, handle);
                handle
            };

            Self::insert_mod_component_symbols::<T>(snap);
            handle
        };
        self.publish_after_mutation();
        Ok(handle)
    }

    fn insert_mod_component_symbols<T: ModComponent>(snap: &mut RegistrySnapshot) {
        let vtable_sym = format!("bam.v0.{}.vtable", T::CANONICAL_NAME);
        let vtable_addr = SymbolAddress::new(T::erased_vtable_ptr());
        if !vtable_addr.is_null() {
            snap.symbols.insert(
                Arc::from(vtable_sym.as_str()),
                SymbolRecord {
                    addr: vtable_addr,
                    owner: None,
                },
            );
        }
        for sym in T::symbols() {
            if sym.name.starts_with("bam.v0.") && !sym.ptr.is_null() {
                snap.symbols.insert(
                    Arc::from(sym.name),
                    SymbolRecord {
                        addr: SymbolAddress::new(sym.ptr),
                        owner: None,
                    },
                );
            }
        }
    }

    /// Registers a non-component [`ModComponent`] helper/nested struct type (e.g. `Vec3f`)
    /// so its static `"bam.v0.<name>.vtable"` and accessor symbols are resolvable via `get_proc_address`.
    pub fn register_type<T: ModComponent>(&mut self) {
        let snap = Arc::make_mut(&mut self.snapshot);
        Self::insert_mod_component_symbols::<T>(snap);
        self.publish_after_mutation();
    }

    /// Registers a guest-owned dynamic component from a validated [`ComponentDesc`], returning its
    /// 1-based [`BamComponent`] handle.
    pub(crate) fn register_dynamic_component_owned(
        &mut self,
        comp_id: ComponentId,
        desc: &ComponentDesc,
        name: &str,
        owner: Option<ModId>,
        drop_slot: Option<usize>,
    ) -> Result<BamComponent, ComponentCollisionError> {
        let storage_flags = component_flags_to_storage_flags(desc.flags);
        self.check_registration_collision(
            name,
            desc.uuid,
            desc.size,
            desc.align,
            storage_flags,
            Some(comp_id),
        )?;
        self.check_dynamic_drop_compatibility(name, desc.drop_fn.is_some())?;

        let sym_addr = SymbolAddress::new(desc.vtable_ptr);
        let handle = {
            let snap = Arc::make_mut(&mut self.snapshot);
            let mut is_dynamic_entry = true;
            let handle = if let Some(&existing_handle) = snap.by_name.get(name) {
                let idx = (existing_handle.0 - 1) as usize;
                let entry = &mut snap.entries[idx];
                is_dynamic_entry = entry.is_dynamic;
                if entry.is_dynamic {
                    entry.vtable_ptr = sym_addr;
                    entry.drop_fn = desc.drop_fn;
                    if drop_slot.is_some() {
                        entry.drop_slot = drop_slot;
                    }
                    entry.owner = owner;
                }
                existing_handle
            } else {
                let next_raw = u32::try_from(snap.entries.len() + 1)
                    .expect("ModRegistry exceeded u32::MAX component handles");
                let handle = BamComponent::from_raw(next_raw);
                let arc_name: Arc<str> = Arc::from(name);
                let entry = RegisteredComponent {
                    handle,
                    name: Arc::clone(&arc_name),
                    uuid: desc.uuid,
                    component_id: comp_id,
                    size: desc.size,
                    align: desc.align,
                    storage_flags,
                    is_abi_pod: desc.is_pod(),
                    is_dynamic: true,
                    vtable_ptr: sym_addr,
                    insert_raw_fn: None,
                    insert_default_fn: None,
                    drop_fn: desc.drop_fn,
                    drop_slot,
                    owner,
                };
                snap.entries.push(entry);
                snap.by_name.insert(arc_name, handle);
                snap.by_uuid.insert(desc.uuid, handle);
                snap.by_component_id.insert(comp_id, handle);
                handle
            };

            if is_dynamic_entry {
                let vtable_key: Arc<str> = Arc::from(format!("bam.v0.{name}.vtable").as_str());
                if !sym_addr.is_null() {
                    snap.symbols.insert(
                        vtable_key,
                        SymbolRecord {
                            addr: sym_addr,
                            owner,
                        },
                    );
                } else if snap
                    .symbols
                    .get(vtable_key.as_ref())
                    .is_some_and(|r| r.owner.is_some())
                {
                    snap.symbols.remove(vtable_key.as_ref());
                }
            }
            handle
        };
        self.publish_after_mutation();

        Ok(handle)
    }

    /// Registers a custom `"bam.v0.<name>"` host symbol scoped strictly to this [`ModRegistry`].
    ///
    /// If `name` does not already begin with `"bam.v0."`, it is automatically prefixed with `"bam.v0."`.
    ///
    /// # Safety
    /// Caller must ensure `ptr` remains valid for any guest code resolving `name` against this registry.
    pub unsafe fn register_custom_symbol(&mut self, name: &str, ptr: *const c_void) {
        let full_name: Arc<str> = if name.starts_with("bam.v0.") {
            Arc::from(name)
        } else {
            Arc::from(format!("bam.v0.{name}").as_str())
        };
        let snap = Arc::make_mut(&mut self.snapshot);
        snap.symbols.insert(
            full_name,
            SymbolRecord {
                addr: SymbolAddress::new(ptr),
                owner: None,
            },
        );
        self.publish_after_mutation();
    }

    /// Registers a custom `"bam.v0.<name>"` symbol owned by `owner` (`ModId`) so it is automatically
    /// evicted when `owner` unloads.
    ///
    /// # Safety
    /// Caller must ensure `ptr` remains valid until `owner` is evicted from this registry.
    pub unsafe fn register_mod_symbol(&mut self, name: &str, ptr: *const c_void, owner: ModId) {
        let full_name: Arc<str> = if name.starts_with("bam.v0.") {
            Arc::from(name)
        } else {
            Arc::from(format!("bam.v0.{name}").as_str())
        };
        let snap = Arc::make_mut(&mut self.snapshot);
        snap.symbols.insert(
            full_name,
            SymbolRecord {
                addr: SymbolAddress::new(ptr),
                owner: Some(owner),
            },
        );
        self.publish_after_mutation();
    }

    /// Evicts all guest-owned VTable pointers, drop callbacks, and custom symbols owned by `mod_id`
    /// while preserving 1-based [`BamComponent`] handles and Bevy [`ComponentId`]s across reloads.
    pub(crate) fn evict_mod(&mut self, mod_id: ModId) {
        let snap = Arc::make_mut(&mut self.snapshot);
        snap.symbols.retain(|_, rec| rec.owner != Some(mod_id));
        for comp in &mut snap.entries {
            if comp.owner == Some(mod_id) {
                comp.vtable_ptr = SymbolAddress::NULL;
                comp.drop_fn = None;
                comp.owner = None;
            }
        }
        self.publish_after_mutation();
    }

    /// Looks up registered component metadata in `O(1)` by 1-based [`BamComponent`] handle.
    #[inline]
    pub fn get_by_handle(&self, handle: BamComponent) -> Option<&RegisteredComponent> {
        self.snapshot.get_by_handle(handle)
    }

    /// Looks up registered component metadata by runtime Bevy [`ComponentId`].
    #[inline]
    pub fn get_by_id(&self, id: ComponentId) -> Option<&RegisteredComponent> {
        self.snapshot.get_by_id(id)
    }

    /// Looks up registered component metadata by canonical string name.
    #[inline]
    pub fn get_by_name(&self, name: &str) -> Option<&RegisteredComponent> {
        self.snapshot.get_by_name(name)
    }

    /// Looks up registered component metadata by deterministic 128-bit [`ComponentUuid`].
    #[inline]
    pub fn get_by_uuid(&self, uuid: &ComponentUuid) -> Option<&RegisteredComponent> {
        self.snapshot.get_by_uuid(uuid)
    }

    /// Resolves a `"bam.v0.*"` symbol address from this registry.
    #[inline]
    pub fn resolve_symbol(&self, name: &str) -> Option<*const c_void> {
        self.snapshot.resolve_symbol(name)
    }
}
