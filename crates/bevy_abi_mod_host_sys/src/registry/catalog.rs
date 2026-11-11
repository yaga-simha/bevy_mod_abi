//! Component, resource, VTable, and symbol catalog (`ModRegistry`).

use crate::loader::ModId;
use crate::registry::collision::ComponentCollisionError;
use crate::registry::symbols::{
    CURRENT_REGISTRY, CurrentRegistryGuard, GLOBAL_SYMBOLS, SymbolEntry,
};
use crate::sync_ptr::SymbolAddress;
use bevy_abi_mod_sys::{
    ComponentDropFn, ComponentUuid, ExposableComponent, ModComponentDescriptor,
};
use bevy_ecs::component::ComponentId;
use bevy_ecs::entity::Entity;
use bevy_ecs::prelude::{Component, Resource};
use bevy_ecs::world::World;
use core::ffi::c_void;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};

/// Function signature for monomorphized raw component byte insertion into a Bevy [`World`].
///
/// Takes a raw `*const u8` pointer (backed by `MaybeUninit<u8>` storage) rather than `&[u8]`
/// so uninitialized struct padding bytes never form a Rust `[u8]` slice.
pub type InsertRawFn = unsafe fn(world: &mut World, entity: Entity, data: *const u8);

/// Function signature for monomorphized `Default` component insertion into a Bevy [`World`].
pub type InsertDefaultFn = fn(world: &mut World, entity: Entity);

/// Monomorphized typed component insertion function using unaligned memory reading.
///
/// # Safety
/// Caller must ensure `T` has no drop glue or heap pointers and `data` points to `size_of::<T>()`
/// readable bytes representing a valid bit-pattern for `T` (padding bytes may be uninitialized).
unsafe fn insert_raw_impl<T: Component + ExposableComponent>(
    world: &mut World,
    entity: Entity,
    data: *const u8,
) {
    if core::mem::size_of::<T>() > 0 && data.is_null() {
        return;
    }
    // SAFETY: Caller guarantees `data` points to `size_of::<T>()` bytes valid for `T` (or ZST).
    // `read_unaligned` copies `T` directly without forming `&[u8]` over padding bytes.
    let comp: T = if core::mem::size_of::<T>() == 0 {
        // SAFETY: `T` is zero-sized and `T::IS_RAW_INSERTABLE && !needs_drop::<T>()` holds.
        unsafe { core::mem::MaybeUninit::<T>::zeroed().assume_init() }
    } else {
        // SAFETY: `data` is non-null and holds `size_of::<T>()` bytes for `T`.
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

/// Catalog record stored for each component or resource registered with [`ModRegistry`].
#[derive(Clone)]
pub struct RegisteredComponent {
    /// Canonical component or resource name (e.g. `"Position"` or `"WindConfig"`).
    pub name: String,
    /// Deterministic 128-bit compile-time UUID.
    pub uuid: ComponentUuid,
    /// Runtime Bevy [`ComponentId`].
    pub component_id: ComponentId,
    /// Byte size of the type.
    pub size: usize,
    /// Memory alignment of the type.
    pub align: usize,
    /// Address of the C-ABI VTable (null if none or cleared on guest unload).
    pub vtable_ptr: SymbolAddress,
    /// Function pointer for deferred raw byte insertion (`None` for dynamic, resource, or non-POD types).
    pub insert_raw_fn: Option<InsertRawFn>,
    /// Optional host-side `T::default()` constructor for deferred `cmd_insert_default`.
    pub insert_default_fn: Option<InsertDefaultFn>,
    /// Optional foreign lifecycle drop callback (cleared on guest unload).
    pub drop_fn: Option<ComponentDropFn>,
    /// Indicates whether this component was registered dynamically at runtime.
    pub is_dynamic: bool,
    /// Indicates whether this entry represents a singleton Bevy `Resource` rather than an entity `Component`.
    pub is_resource: bool,
    /// Optional [`ModId`] owning a guest-registered dynamic component's VTable and drop callback.
    pub owner: Option<ModId>,
}

/// Central registry mapping exposed components, VTables, and symbol addresses.
#[derive(Resource, Clone)]
pub struct ModRegistry {
    components_by_id: HashMap<ComponentId, RegisteredComponent>,
    components_by_name: HashMap<String, ComponentId>,
    components_by_uuid: HashMap<[u8; 16], ComponentId>,
    symbols: HashMap<String, SymbolEntry>,
    revoked_owners: Arc<RwLock<HashSet<ModId>>>,
}

impl Default for ModRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ModRegistry {
    /// Creates a new mod registry pre-populated with built-in engine bridge symbols.
    pub fn new() -> Self {
        let mut reg = Self {
            components_by_id: HashMap::new(),
            components_by_name: HashMap::new(),
            components_by_uuid: HashMap::new(),
            symbols: HashMap::new(),
            revoked_owners: Arc::new(RwLock::new(HashSet::new())),
        };
        let builtins: &[(&str, *const c_void)] = &[
            (
                "cmd_spawn_empty",
                crate::ffi::commands::host_cmd_spawn_empty as *const c_void,
            ),
            (
                "host_cmd_spawn_empty",
                crate::ffi::commands::host_cmd_spawn_empty as *const c_void,
            ),
            (
                "cmd_despawn",
                crate::ffi::commands::host_cmd_despawn as *const c_void,
            ),
            (
                "host_cmd_despawn",
                crate::ffi::commands::host_cmd_despawn as *const c_void,
            ),
            (
                "cmd_insert_component",
                crate::ffi::commands::host_cmd_insert_component as *const c_void,
            ),
            (
                "host_cmd_insert_component",
                crate::ffi::commands::host_cmd_insert_component as *const c_void,
            ),
            (
                "cmd_remove_component",
                crate::ffi::commands::host_cmd_remove_component as *const c_void,
            ),
            (
                "host_cmd_remove_component",
                crate::ffi::commands::host_cmd_remove_component as *const c_void,
            ),
            (
                "cmd_insert_default",
                crate::ffi::commands::host_cmd_insert_default as *const c_void,
            ),
            (
                "host_cmd_insert_default",
                crate::ffi::commands::host_cmd_insert_default as *const c_void,
            ),
            (
                "iter_reset",
                crate::ffi::iter::host_iter_reset as *const c_void,
            ),
            (
                "host_iter_reset",
                crate::ffi::iter::host_iter_reset as *const c_void,
            ),
            (
                "res_readonly",
                crate::ffi::iter::host_iter_resource_readonly as *const c_void,
            ),
            (
                "host_res_readonly",
                crate::ffi::iter::host_iter_resource_readonly as *const c_void,
            ),
            (
                "iter_resource_readonly",
                crate::ffi::iter::host_iter_resource_readonly as *const c_void,
            ),
            (
                "host_iter_resource_readonly",
                crate::ffi::iter::host_iter_resource_readonly as *const c_void,
            ),
            (
                "res_mutable",
                crate::ffi::iter::host_iter_resource_mutable as *const c_void,
            ),
            (
                "host_res_mutable",
                crate::ffi::iter::host_iter_resource_mutable as *const c_void,
            ),
            (
                "iter_resource_mutable",
                crate::ffi::iter::host_iter_resource_mutable as *const c_void,
            ),
            (
                "host_iter_resource_mutable",
                crate::ffi::iter::host_iter_resource_mutable as *const c_void,
            ),
            (
                "register_multi_query_system",
                crate::ffi::register::host_register_multi_query_system as *const c_void,
            ),
            (
                "host_register_multi_query_system",
                crate::ffi::register::host_register_multi_query_system as *const c_void,
            ),
            (
                "register_conditional_multi_query_system",
                crate::ffi::register::host_register_conditional_multi_query_system as *const c_void,
            ),
            (
                "host_register_conditional_multi_query_system",
                crate::ffi::register::host_register_conditional_multi_query_system as *const c_void,
            ),
            (
                "register_system_in_schedule",
                crate::ffi::register::host_register_system_in_schedule as *const c_void,
            ),
            (
                "host_register_system_in_schedule",
                crate::ffi::register::host_register_system_in_schedule as *const c_void,
            ),
            (
                "register_scheduled_system",
                crate::ffi::register::host_register_scheduled_system as *const c_void,
            ),
            (
                "host_register_scheduled_system",
                crate::ffi::register::host_register_scheduled_system as *const c_void,
            ),
            (
                "register_conditional_system",
                crate::ffi::register::host_register_conditional_system as *const c_void,
            ),
            (
                "host_register_conditional_system",
                crate::ffi::register::host_register_conditional_system as *const c_void,
            ),
            (
                "register_component",
                crate::ffi::register::host_register_component as *const c_void,
            ),
            (
                "host_register_component",
                crate::ffi::register::host_register_component as *const c_void,
            ),
        ];
        for &(name, ptr) in builtins {
            reg.insert_symbol(name, ptr);
        }
        reg
    }

    /// Binds this registry as the thread-local active registry, returning an RAII restoration guard.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn make_current(self: &Arc<Self>) -> CurrentRegistryGuard {
        self.make_current_internal()
    }

    /// Internal thread-local registry binder used during mod load and system execution.
    pub(crate) fn make_current_internal(self: &Arc<Self>) -> CurrentRegistryGuard {
        let prev = CURRENT_REGISTRY.with(|cell| cell.borrow_mut().replace(Arc::clone(self)));
        CurrentRegistryGuard { prev }
    }

    /// Validates that registering `(name, uuid, size, align, is_resource, expected_comp_id)` does
    /// not collide with an incompatible existing component or resource entry.
    pub fn check_registration_collision(
        &self,
        name: &str,
        uuid: ComponentUuid,
        size: usize,
        align: usize,
        is_resource: bool,
        expected_comp_id: Option<ComponentId>,
    ) -> Result<(), ComponentCollisionError> {
        if let Some(existing_by_uuid) = self.get_by_uuid(&uuid)
            && existing_by_uuid.name != name
        {
            return Err(ComponentCollisionError::UuidCollision {
                existing_name: existing_by_uuid.name.clone(),
                new_name: name.to_string(),
                uuid,
            });
        }

        if let Some(existing) = self.get_by_name(name) {
            if existing.uuid != uuid {
                return Err(ComponentCollisionError::MetadataMismatch {
                    name: name.to_string(),
                    reason: format!(
                        "UUID mismatch (existing={:?}, new={:?})",
                        existing.uuid.bytes, uuid.bytes
                    ),
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
            if existing.is_resource != is_resource {
                return Err(ComponentCollisionError::MetadataMismatch {
                    name: name.to_string(),
                    reason: format!(
                        "kind mismatch (existing is_resource={}, new is_resource={})",
                        existing.is_resource, is_resource
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

    /// Registers an exposable component type with its Bevy [`ComponentId`].
    pub fn register_component<T: Component + ExposableComponent>(&mut self, comp_id: ComponentId) {
        if let Err(err) = self.try_register_component::<T>(comp_id) {
            panic!("ModRegistry::register_component failed: {err}");
        }
    }

    /// Fallible variant of [`ModRegistry::register_component`].
    pub fn try_register_component<T: Component + ExposableComponent>(
        &mut self,
        comp_id: ComponentId,
    ) -> Result<(), ComponentCollisionError> {
        let allow_raw_insert = T::IS_RAW_INSERTABLE && !core::mem::needs_drop::<T>();
        self.try_register_entry::<T>(comp_id, allow_raw_insert, None, false)
    }

    /// Registers an exposable component type along with its host-side [`Default`] constructor.
    pub fn register_component_with_default<T: Component + ExposableComponent + Default>(
        &mut self,
        comp_id: ComponentId,
    ) {
        if let Err(err) = self.try_register_component_with_default::<T>(comp_id) {
            panic!("ModRegistry::register_component_with_default failed: {err}");
        }
    }

    /// Fallible variant of [`ModRegistry::register_component_with_default`].
    pub fn try_register_component_with_default<T: Component + ExposableComponent + Default>(
        &mut self,
        comp_id: ComponentId,
    ) -> Result<(), ComponentCollisionError> {
        let allow_raw_insert = T::IS_RAW_INSERTABLE && !core::mem::needs_drop::<T>();
        self.try_register_entry::<T>(
            comp_id,
            allow_raw_insert,
            Some(insert_default_impl::<T>),
            false,
        )
    }

    /// Registers an exposable component type as VTable-only (disallowing raw byte insertion).
    pub fn register_component_opaque<T: Component + ExposableComponent>(
        &mut self,
        comp_id: ComponentId,
    ) {
        if let Err(err) = self.try_register_component_opaque::<T>(comp_id) {
            panic!("ModRegistry::register_component_opaque failed: {err}");
        }
    }

    /// Fallible variant of [`ModRegistry::register_component_opaque`].
    pub fn try_register_component_opaque<T: Component + ExposableComponent>(
        &mut self,
        comp_id: ComponentId,
    ) -> Result<(), ComponentCollisionError> {
        self.try_register_entry::<T>(comp_id, false, None, false)
    }

    /// Registers an exposable VTable-only component type along with its host-side [`Default`] constructor.
    pub fn register_component_opaque_with_default<T: Component + ExposableComponent + Default>(
        &mut self,
        comp_id: ComponentId,
    ) {
        if let Err(err) = self.try_register_component_opaque_with_default::<T>(comp_id) {
            panic!("ModRegistry::register_component_opaque_with_default failed: {err}");
        }
    }

    /// Fallible variant of [`ModRegistry::register_component_opaque_with_default`].
    pub fn try_register_component_opaque_with_default<
        T: Component + ExposableComponent + Default,
    >(
        &mut self,
        comp_id: ComponentId,
    ) -> Result<(), ComponentCollisionError> {
        self.try_register_entry::<T>(comp_id, false, Some(insert_default_impl::<T>), false)
    }

    /// Registers a Bevy singleton [`Resource`] type with its [`ComponentId`].
    pub fn register_resource<R: Resource + ExposableComponent>(&mut self, comp_id: ComponentId) {
        if let Err(err) = self.try_register_resource::<R>(comp_id) {
            panic!("ModRegistry::register_resource failed: {err}");
        }
    }

    /// Fallible variant of [`ModRegistry::register_resource`].
    pub fn try_register_resource<R: Resource + ExposableComponent>(
        &mut self,
        comp_id: ComponentId,
    ) -> Result<(), ComponentCollisionError> {
        self.try_register_entry::<R>(comp_id, false, None, true)
    }

    fn try_register_entry<T: Component + ExposableComponent>(
        &mut self,
        comp_id: ComponentId,
        allow_raw_insert: bool,
        insert_default_fn: Option<InsertDefaultFn>,
        is_resource: bool,
    ) -> Result<(), ComponentCollisionError> {
        let name = T::COMPONENT_NAME.to_string();
        let uuid = T::UUID;
        self.check_registration_collision(
            &name,
            uuid,
            T::size(),
            T::align(),
            is_resource,
            Some(comp_id),
        )?;

        let sym_addr = SymbolAddress::new(T::erased_vtable_ptr());
        let insert_raw_fn: Option<InsertRawFn> =
            if !is_resource && allow_raw_insert && !core::mem::needs_drop::<T>() {
                Some(insert_raw_impl::<T>)
            } else {
                None
            };

        let merged_default_fn = insert_default_fn.or_else(|| {
            self.components_by_id
                .get(&comp_id)
                .and_then(|prev| prev.insert_default_fn)
        });

        let reg = RegisteredComponent {
            name: name.clone(),
            uuid,
            component_id: comp_id,
            size: T::size(),
            align: T::align(),
            vtable_ptr: sym_addr,
            insert_raw_fn,
            insert_default_fn: merged_default_fn,
            drop_fn: None,
            is_dynamic: false,
            is_resource,
            owner: None,
        };

        self.components_by_id.insert(comp_id, reg);
        self.components_by_name.insert(name.clone(), comp_id);
        self.components_by_uuid.insert(uuid.bytes, comp_id);

        let host_entry = SymbolEntry {
            addr: sym_addr,
            owner: None,
        };
        self.symbols.insert(name.clone(), host_entry);
        self.symbols.insert(format!("{name}_vtable"), host_entry);
        for sym in T::symbols() {
            self.symbols.insert(
                sym.name.to_string(),
                SymbolEntry {
                    addr: SymbolAddress::new(sym.ptr),
                    owner: None,
                },
            );
        }

        if let Ok(mut global) = GLOBAL_SYMBOLS.write() {
            global.insert(name.clone(), sym_addr);
            global.insert(format!("{name}_vtable"), sym_addr);
            for sym in T::symbols() {
                global.insert(sym.name.to_string(), SymbolAddress::new(sym.ptr));
            }
        }

        Ok(())
    }

    /// Registers a dynamic component descriptor declared at runtime by a guest mod.
    pub fn register_dynamic_component(
        &mut self,
        comp_id: ComponentId,
        desc: &ModComponentDescriptor,
        name: String,
    ) {
        if let Err(err) = self.try_register_dynamic_component(comp_id, desc, name) {
            panic!("ModRegistry::register_dynamic_component failed: {err}");
        }
    }

    /// Fallible variant of [`ModRegistry::register_dynamic_component`].
    pub fn try_register_dynamic_component(
        &mut self,
        comp_id: ComponentId,
        desc: &ModComponentDescriptor,
        name: String,
    ) -> Result<(), ComponentCollisionError> {
        self.try_register_dynamic_component_owned(comp_id, desc, name, None)
    }

    /// Registers a guest-owned dynamic component, tagging its VTable and symbols with `owner`
    /// without inserting guest pointers into `GLOBAL_SYMBOLS`.
    pub(crate) fn try_register_dynamic_component_owned(
        &mut self,
        comp_id: ComponentId,
        desc: &ModComponentDescriptor,
        name: String,
        owner: Option<ModId>,
    ) -> Result<(), ComponentCollisionError> {
        let uuid = desc.uuid;
        self.check_registration_collision(
            &name,
            uuid,
            desc.size,
            desc.align,
            false,
            Some(comp_id),
        )?;

        if let Some(mod_id) = owner
            && let Ok(mut revoked) = self.revoked_owners.write()
        {
            revoked.remove(&mod_id);
        }

        let sym_addr = SymbolAddress::new(desc.vtable_ptr);
        let reg = RegisteredComponent {
            name: name.clone(),
            uuid,
            component_id: comp_id,
            size: desc.size,
            align: desc.align,
            vtable_ptr: sym_addr,
            insert_raw_fn: None,
            insert_default_fn: None,
            drop_fn: desc.drop_fn,
            is_dynamic: true,
            is_resource: false,
            owner,
        };

        self.components_by_id.insert(comp_id, reg);
        self.components_by_name.insert(name.clone(), comp_id);
        self.components_by_uuid.insert(uuid.bytes, comp_id);

        if !sym_addr.is_null() {
            let entry = SymbolEntry {
                addr: sym_addr,
                owner,
            };
            self.symbols.insert(name.clone(), entry);
            self.symbols.insert(format!("{name}_vtable"), entry);
        } else {
            self.symbols.remove(&name);
            self.symbols.remove(&format!("{name}_vtable"));
        }

        Ok(())
    }

    /// Evicts all guest-owned VTable pointers, drop hooks, and symbol entries owned by `mod_id`
    /// while preserving `ComponentId` bindings for subsequent reloads.
    pub(crate) fn evict_mod(&mut self, mod_id: ModId) {
        if let Ok(mut revoked) = self.revoked_owners.write() {
            revoked.insert(mod_id);
        }
        self.symbols.retain(|_, entry| entry.owner != Some(mod_id));
        for comp in self.components_by_id.values_mut() {
            if comp.owner == Some(mod_id) {
                comp.vtable_ptr = SymbolAddress::NULL;
                comp.drop_fn = None;
                comp.owner = None;
            }
        }
    }

    /// Rolls back a failed or aborted module load (`mod_id`), removing newly registered dynamic
    /// component entries and symbols completely while clearing VTable/drop hooks on any pre-existing ones.
    pub(crate) fn rollback_mod(&mut self, mod_id: ModId, newly_created: &[ComponentId]) {
        self.evict_mod(mod_id);
        for &comp_id in newly_created {
            if let Some(removed) = self.components_by_id.remove(&comp_id) {
                self.components_by_name.remove(&removed.name);
                self.components_by_uuid.remove(&removed.uuid.bytes);
            }
        }
    }

    /// Registers a non-component `ModExposable` helper/nested struct type (e.g. `Vec3f`)
    /// so its static VTable and field accessors are resolvable via `GetProcAddressFn`.
    pub fn register_type<T: ExposableComponent>(&mut self) {
        let name = T::COMPONENT_NAME.to_string();
        let sym_addr = SymbolAddress::new(T::erased_vtable_ptr());
        let host_entry = SymbolEntry {
            addr: sym_addr,
            owner: None,
        };

        self.symbols.insert(name.clone(), host_entry);
        self.symbols.insert(format!("{name}_vtable"), host_entry);
        for sym in T::symbols() {
            self.symbols.insert(
                sym.name.to_string(),
                SymbolEntry {
                    addr: SymbolAddress::new(sym.ptr),
                    owner: None,
                },
            );
        }

        if let Ok(mut global) = GLOBAL_SYMBOLS.write() {
            global.insert(name.clone(), sym_addr);
            global.insert(format!("{name}_vtable"), sym_addr);
            for sym in T::symbols() {
                global.insert(sym.name.to_string(), SymbolAddress::new(sym.ptr));
            }
        }
    }

    /// Registers a custom host symbol scoped strictly to this [`ModRegistry`] instance
    /// (without polluting global symbols).
    ///
    /// # Safety
    /// Caller must ensure `ptr` remains valid for any guest code resolving `name` against this registry.
    pub unsafe fn register_custom_symbol(&mut self, name: impl Into<String>, ptr: *const c_void) {
        self.symbols.insert(
            name.into(),
            SymbolEntry {
                addr: SymbolAddress::new(ptr),
                owner: None,
            },
        );
    }

    /// Registers a custom symbol owned by a specific guest [`ModId`] so it is automatically evicted
    /// when `mod_id` unloads or rolls back.
    ///
    /// # Safety
    /// Caller must ensure `ptr` remains valid until `owner` is evicted from this registry.
    pub unsafe fn register_mod_symbol(
        &mut self,
        name: impl Into<String>,
        ptr: *const c_void,
        owner: ModId,
    ) {
        self.symbols.insert(
            name.into(),
            SymbolEntry {
                addr: SymbolAddress::new(ptr),
                owner: Some(owner),
            },
        );
    }

    /// Looks up registered component metadata by runtime Bevy [`ComponentId`].
    pub fn get_by_id(&self, id: ComponentId) -> Option<&RegisteredComponent> {
        self.components_by_id.get(&id)
    }

    /// Looks up registered component metadata by canonical string name.
    pub fn get_by_name(&self, name: &str) -> Option<&RegisteredComponent> {
        self.components_by_name
            .get(name)
            .and_then(|id| self.components_by_id.get(id))
    }

    /// Looks up registered component metadata by deterministic 128-bit [`ComponentUuid`].
    pub fn get_by_uuid(&self, uuid: &ComponentUuid) -> Option<&RegisteredComponent> {
        self.components_by_uuid
            .get(&uuid.bytes)
            .and_then(|id| self.components_by_id.get(id))
    }

    /// Resolves a symbol address from this registry by name, returning `None` if not present or if
    /// its owning guest module has been unloaded.
    pub fn resolve_symbol(&self, name: &str) -> Option<*const c_void> {
        let entry = self.symbols.get(name)?;
        if let Some(owner) = entry.owner
            && self
                .revoked_owners
                .read()
                .map(|revoked| revoked.contains(&owner))
                .unwrap_or(true)
        {
            return None;
        }
        Some(entry.addr.as_ptr())
    }

    /// Inserts a host-owned `'static` symbol address into this registry and updates the global fallback cache.
    pub fn insert_symbol(&mut self, name: impl Into<String>, ptr: *const c_void) {
        let name_str = name.into();
        let sym_addr = SymbolAddress::new(ptr);
        self.symbols.insert(
            name_str.clone(),
            SymbolEntry {
                addr: sym_addr,
                owner: None,
            },
        );
        if let Ok(mut global) = GLOBAL_SYMBOLS.write() {
            global.insert(name_str, sym_addr);
        }
    }
}
