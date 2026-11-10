#![deny(unsafe_op_in_unsafe_fn)]
#![deny(improper_ctypes_definitions)]

use abi_contract::{ComponentUuid, ExposableComponent};
use bevy_ecs::component::ComponentId;
use bevy_ecs::entity::Entity;
use bevy_ecs::prelude::{Component, Resource};
use bevy_ecs::world::World;
use core::ffi::{c_char, c_void, CStr};
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, RwLock};

pub type InsertRawFn = unsafe fn(world: &mut World, entity: Entity, data: &[u8]);

/// Monomorphized typed component insertion function.
unsafe fn insert_raw_impl<T: Component + ExposableComponent>(
    world: &mut World,
    entity: Entity,
    data: &[u8],
) {
    if data.len() != core::mem::size_of::<T>() {
        return;
    }
    // SAFETY: caller guarantees `data` contains bytes representing a valid instance of `T`.
    // read_unaligned safely handles any memory alignment differences.
    let comp: T = unsafe { core::ptr::read_unaligned(data.as_ptr() as *const T) };
    if let Ok(mut e) = world.get_entity_mut(entity) {
        e.insert(comp);
    }
}

/// Newtype wrapper over `*const c_void` providing thread-safety guarantees for static symbol pointers.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct SymbolAddress(pub *const c_void);

// SAFETY: SymbolAddress wraps static VTable and function pointers with `'static` lifetime.
unsafe impl Send for SymbolAddress {}
unsafe impl Sync for SymbolAddress {}

/// Metadata stored for each component registered with `ModRegistry`.
#[derive(Clone)]
pub struct RegisteredComponent {
    pub name: String,
    pub uuid: ComponentUuid,
    pub component_id: ComponentId,
    pub size: usize,
    pub align: usize,
    pub vtable_ptr: *const c_void,
    pub insert_raw_fn: InsertRawFn,
}

// SAFETY: All fields (pointers to static VTables and function pointers) are Send and Sync.
unsafe impl Send for RegisteredComponent {}
unsafe impl Sync for RegisteredComponent {}

// Thread-local registry reference for isolated symbol resolution during parallel tests.
thread_local! {
    pub static CURRENT_REGISTRY: RefCell<Option<Arc<ModRegistry>>> = const { RefCell::new(None) };
}

/// Global fallback symbol table for multi-threaded resolution.
pub static GLOBAL_SYMBOLS: LazyLock<RwLock<HashMap<String, SymbolAddress>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

/// RAII guard that restores the previous thread-local `CURRENT_REGISTRY` upon drop.
pub struct CurrentRegistryGuard {
    prev: Option<Arc<ModRegistry>>,
}

impl Drop for CurrentRegistryGuard {
    fn drop(&mut self) {
        CURRENT_REGISTRY.with(|cell| {
            *cell.borrow_mut() = self.prev.take();
        });
    }
}

/// Central registry mapping exposed components, VTables, and symbol addresses.
#[derive(Resource, Clone)]
pub struct ModRegistry {
    components_by_id: HashMap<ComponentId, RegisteredComponent>,
    components_by_name: HashMap<String, ComponentId>,
    components_by_uuid: HashMap<[u8; 16], ComponentId>,
    symbols: HashMap<String, SymbolAddress>,
}

// SAFETY: ModRegistry stores only components and symbol addresses with static lifetimes.
unsafe impl Send for ModRegistry {}
unsafe impl Sync for ModRegistry {}

impl Default for ModRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ModRegistry {
    pub fn new() -> Self {
        let mut reg = Self {
            components_by_id: HashMap::new(),
            components_by_name: HashMap::new(),
            components_by_uuid: HashMap::new(),
            symbols: HashMap::new(),
        };
        reg.insert_symbol(
            "cmd_spawn_empty",
            crate::commands::host_cmd_spawn_empty as *const c_void,
        );
        reg.insert_symbol(
            "host_cmd_spawn_empty",
            crate::commands::host_cmd_spawn_empty as *const c_void,
        );
        reg.insert_symbol(
            "cmd_despawn",
            crate::commands::host_cmd_despawn as *const c_void,
        );
        reg.insert_symbol(
            "host_cmd_despawn",
            crate::commands::host_cmd_despawn as *const c_void,
        );
        reg.insert_symbol(
            "cmd_insert_component",
            crate::commands::host_cmd_insert_component as *const c_void,
        );
        reg.insert_symbol(
            "host_cmd_insert_component",
            crate::commands::host_cmd_insert_component as *const c_void,
        );
        reg
    }

    /// Sets this registry as the thread-local active registry, returning an RAII guard.
    pub fn make_current(self: &Arc<Self>) -> CurrentRegistryGuard {
        let prev = CURRENT_REGISTRY.with(|cell| cell.borrow_mut().replace(Arc::clone(self)));
        CurrentRegistryGuard { prev }
    }

    /// Register an exposable component type with its Bevy `ComponentId`.
    pub fn register_component<T: Component + ExposableComponent>(&mut self, comp_id: ComponentId) {
        let name = T::COMPONENT_NAME.to_string();
        let uuid = T::UUID;
        let vtable_ptr = T::erased_vtable_ptr();

        let reg = RegisteredComponent {
            name: name.clone(),
            uuid,
            component_id: comp_id,
            size: T::size(),
            align: T::align(),
            vtable_ptr,
            insert_raw_fn: insert_raw_impl::<T>,
        };

        self.components_by_id.insert(comp_id, reg);
        self.components_by_name.insert(name.clone(), comp_id);
        self.components_by_uuid.insert(uuid.bytes, comp_id);

        let sym_addr = SymbolAddress(vtable_ptr);
        self.symbols.insert(name.clone(), sym_addr);
        self.symbols.insert(format!("{name}_vtable"), sym_addr);
        for sym in T::symbols() {
            self.symbols
                .insert(sym.name.to_string(), SymbolAddress(sym.ptr));
        }

        // Populate global symbols fallback
        if let Ok(mut global) = GLOBAL_SYMBOLS.write() {
            global.insert(name.clone(), sym_addr);
            global.insert(format!("{name}_vtable"), sym_addr);
            for sym in T::symbols() {
                global.insert(sym.name.to_string(), SymbolAddress(sym.ptr));
            }
        }
    }

    pub fn get_by_id(&self, id: ComponentId) -> Option<&RegisteredComponent> {
        self.components_by_id.get(&id)
    }

    pub fn get_by_name(&self, name: &str) -> Option<&RegisteredComponent> {
        self.components_by_name
            .get(name)
            .and_then(|id| self.components_by_id.get(id))
    }

    pub fn get_by_uuid(&self, uuid: &ComponentUuid) -> Option<&RegisteredComponent> {
        self.components_by_uuid
            .get(&uuid.bytes)
            .and_then(|id| self.components_by_id.get(id))
    }

    pub fn resolve_symbol(&self, name: &str) -> Option<*const c_void> {
        self.symbols.get(name).map(|s| s.0)
    }

    pub fn insert_symbol(&mut self, name: impl Into<String>, ptr: *const c_void) {
        let name_str = name.into();
        let sym_addr = SymbolAddress(ptr);
        self.symbols.insert(name_str.clone(), sym_addr);
        if let Ok(mut global) = GLOBAL_SYMBOLS.write() {
            global.insert(name_str, sym_addr);
        }
    }
}

/// C-ABI symbol resolver function (`GetProcAddressFn`).
/// Checks thread-local `CURRENT_REGISTRY` first, falling back to `GLOBAL_SYMBOLS`.
///
/// # Safety
/// `name` must be a valid NUL-terminated C string or null.
pub unsafe extern "C" fn host_get_proc_address(name: *const c_char) -> *const c_void {
    if name.is_null() {
        return core::ptr::null();
    }
    // SAFETY: caller passes a valid NUL-terminated C string.
    let Ok(sym_str) = unsafe { CStr::from_ptr(name) }.to_str() else {
        return core::ptr::null();
    };

    // 1. Thread-local lookup
    let local_res = CURRENT_REGISTRY.try_with(|cell| {
        cell.borrow()
            .as_ref()
            .and_then(|reg| reg.resolve_symbol(sym_str))
    });
    if let Ok(Some(ptr)) = local_res {
        return ptr;
    }

    // 2. Global fallback lookup
    if let Ok(guard) = GLOBAL_SYMBOLS.read() {
        if let Some(&sym_addr) = guard.get(sym_str) {
            return sym_addr.0;
        }
    }

    core::ptr::null()
}
