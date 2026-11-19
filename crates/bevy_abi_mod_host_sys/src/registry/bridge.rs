//! Safe registration adapters on [`ModRegistry`] enabling `#![forbid(unsafe_code)]` host bridges
//! in `bevy_abi_mod_host` to expose external Bevy `Component`, `Resource`, `Asset`, `EntityEvent`,
//! and `RelationshipTarget` types across the C ABI without `ModComponent` orphan-rule limitations.

use core::ffi::c_void;
use std::sync::Arc;

use bevy_abi_mod_sys::{
    AbiPod, BAM_CHILDREN_NAME, BAM_STORAGE_FLAG_ENTITY_EVENT, BAM_STORAGE_FLAG_EVENT,
    BAM_STORAGE_FLAG_IMMUTABLE, BAM_STORAGE_FLAG_RESOURCE, BamComponent, BamFieldDesc,
    ComponentUuid, FfiEntity, FfiResult, MappedStringVTable, MappedWireVTable, StrView,
    VTableHeader,
};
use bevy_ecs::{
    component::ComponentId,
    entity::Entity,
    event::{EntityEvent, Event, EventKey},
    hierarchy::Children,
    prelude::{Component, Resource},
    relationship::RelationshipTarget,
    world::World,
};

use crate::{
    loader::ModId,
    registry::{
        ComponentCollisionError, EntityEventTriggerFn, InsertDefaultFn, InsertRawFn, ModRegistry,
        RegisteredComponent, RegistrySnapshot, SymbolRecord, catalog::OwnedFieldStrings,
    },
    sync_ptr::SymbolAddress,
};

/// Safe compile-time bridge specification mapping an external `#[repr(Rust)]` Bevy `Component` or
/// `Resource` `T` to a `#[repr(C)]` `AbiPod` wire type `Self::Wire` via [`MappedWireVTable`].
pub trait HostWireBridge<T: Send + Sync + 'static>: Send + Sync + 'static {
    /// Fixed-layout C-ABI wire struct (`#[repr(C)]` + `AbiPod`).
    type Wire: AbiPod;

    /// Canonical namespaced type name (e.g. `"bevy_transform::components::transform::Transform"`).
    const CANONICAL_NAME: &'static str;

    /// Optional short alias (e.g. `Some("Transform")`).
    const ALIAS: Option<&'static str> = None;

    /// Whether [`Self::apply_wire`] is supported (`false` for read-only types like `GlobalTransform`).
    const SUPPORTS_SET: bool = true;

    /// Projects `&T` into its wire representation `Self::Wire`.
    fn to_wire(value: &T) -> Self::Wire;

    /// Mutates `&mut T` in place from `wire` when [`Self::SUPPORTS_SET`] is `true`.
    fn apply_wire(_value: &mut T, _wire: Self::Wire) -> FfiResult {
        FfiResult::PERMISSION_DENIED
    }
}

/// Safe compile-time bridge specification mapping an external UTF-8 string `Component` or
/// `Resource` `T` (such as `bevy_ecs::name::Name`) to [`MappedStringVTable`].
pub trait HostStringBridge<T: Send + Sync + 'static>: Send + Sync + 'static {
    /// Canonical namespaced type name (e.g. `"bevy_ecs::name::Name"`).
    const CANONICAL_NAME: &'static str;

    /// Optional short alias (e.g. `Some("Name")`).
    const ALIAS: Option<&'static str> = None;

    /// Whether [`Self::set_str`] is supported.
    const SUPPORTS_SET: bool = true;

    /// Borrows the UTF-8 string slice from `&T`.
    fn as_str(value: &T) -> &str;

    /// Replaces `&mut T`'s string contents in place.
    fn set_str(_value: &mut T, _text: &str) -> FfiResult {
        FfiResult::PERMISSION_DENIED
    }
}

struct WireVTableFactory<T, B>(core::marker::PhantomData<(T, B)>);

impl<T: Send + Sync + 'static, B: HostWireBridge<T>> WireVTableFactory<T, B> {
    const UUID: ComponentUuid = ComponentUuid::from_canonical_name(B::CANONICAL_NAME);

    unsafe extern "C" fn get_shim(ptr: *const c_void, out_wire: *mut B::Wire) -> FfiResult {
        if ptr.is_null() || out_wire.is_null() {
            return FfiResult::NULL_POINTER;
        }
        let host_ptr = ptr.cast::<T>();
        if !host_ptr.is_aligned() || !out_wire.is_aligned() {
            return FfiResult::INVALID_ARGUMENT;
        }
        // SAFETY: Caller guarantees `ptr` points to an initialized `T` and `out_wire` is writable.
        let val = unsafe { &*host_ptr };
        let wire = B::to_wire(val);
        // SAFETY: `out_wire` is non-null, aligned, and writable.
        unsafe {
            *out_wire = wire;
        }
        FfiResult::OK
    }

    unsafe extern "C" fn set_shim(ptr: *mut c_void, in_wire: *const B::Wire) -> FfiResult {
        if ptr.is_null() || in_wire.is_null() {
            return FfiResult::NULL_POINTER;
        }
        let host_ptr = ptr.cast::<T>();
        if !host_ptr.is_aligned() || !in_wire.is_aligned() {
            return FfiResult::INVALID_ARGUMENT;
        }
        // SAFETY: `in_wire` is non-null, aligned, and readable; copy `B::Wire` before mutating `T`.
        let wire = unsafe { *in_wire };
        // SAFETY: Caller guarantees exclusive mutable access to `T` at `ptr`.
        let val = unsafe { &mut *host_ptr };
        B::apply_wire(val, wire)
    }

    const VTABLE: MappedWireVTable<B::Wire> = MappedWireVTable {
        header: VTableHeader::from_raw_parts(
            core::mem::size_of::<MappedWireVTable<B::Wire>>() as u32,
            Self::UUID,
            core::mem::size_of::<T>(),
            core::mem::align_of::<T>(),
        ),
        get: Some(Self::get_shim),
        set: if B::SUPPORTS_SET {
            Some(Self::set_shim)
        } else {
            None
        },
    };
}

struct StringVTableFactory<T, B>(core::marker::PhantomData<(T, B)>);

impl<T: Send + Sync + 'static, B: HostStringBridge<T>> StringVTableFactory<T, B> {
    const UUID: ComponentUuid = ComponentUuid::from_canonical_name(B::CANONICAL_NAME);

    unsafe extern "C" fn get_shim(ptr: *const c_void) -> StrView {
        if ptr.is_null() {
            return StrView::empty();
        }
        let host_ptr = ptr.cast::<T>();
        if !host_ptr.is_aligned() {
            return StrView::empty();
        }
        // SAFETY: Caller guarantees `ptr` points to an initialized `T` valid for the borrow window.
        let s = B::as_str(unsafe { &*host_ptr });
        StrView::from_str(s)
    }

    unsafe extern "C" fn set_shim(ptr: *mut c_void, view: StrView) -> FfiResult {
        if ptr.is_null() {
            return FfiResult::NULL_POINTER;
        }
        let host_ptr = ptr.cast::<T>();
        if !host_ptr.is_aligned() {
            return FfiResult::INVALID_ARGUMENT;
        }
        // SAFETY: `view.try_as_str()` validates alignment, nullness, and UTF-8.
        let text = match unsafe { view.try_as_str() } {
            Ok(s) => s,
            Err(err) => return err,
        };
        // Copy into a host temporary `String` before mutating `T` to prevent self-aliasing overlap
        // if `view` was borrowed directly from `get_shim(ptr)`.
        let owned = String::from(text);
        // SAFETY: Caller guarantees exclusive mutable access to `T` at `ptr`.
        B::set_str(unsafe { &mut *host_ptr }, &owned)
    }

    const VTABLE: MappedStringVTable = MappedStringVTable {
        header: VTableHeader::from_raw_parts(
            core::mem::size_of::<MappedStringVTable>() as u32,
            Self::UUID,
            core::mem::size_of::<T>(),
            core::mem::align_of::<T>(),
        ),
        get: Some(Self::get_shim),
        set: if B::SUPPORTS_SET {
            Some(Self::set_shim)
        } else {
            None
        },
    };
}

fn insert_default_bridged<T: Component + Default>(world: &mut World, entity: Entity) {
    if let Ok(mut e) = world.get_entity_mut(entity) {
        e.insert(T::default());
    }
}

unsafe fn extract_entity_only_target_impl<E: EntityEvent + Copy>(
    data: *const u8,
) -> Option<Entity> {
    let ev: E = if core::mem::size_of::<E>() == 0 {
        // SAFETY: `E` is zero-sized and `Copy`; reading from `NonNull::dangling()` is sound.
        unsafe { core::ptr::read(core::ptr::NonNull::<E>::dangling().as_ptr()) }
    } else {
        if data.is_null() {
            return None;
        }
        // SAFETY: `event_key` was registered via `world.register_event_key::<E>()`, guaranteeing
        // `data` points to an initialized `E` value during observer execution.
        unsafe { core::ptr::read_unaligned(data.cast::<E>()) }
    };
    Some(ev.event_target())
}

const _: () = {
    assert!(core::mem::size_of::<Entity>() == core::mem::size_of::<FfiEntity>());
    assert!(core::mem::align_of::<Entity>() == core::mem::align_of::<FfiEntity>());
};

/// Function signature borrowing a contiguous zero-copy `(*const FfiEntity, len)` slice from a
/// `RelationshipTarget` component pointer (`Children` or slice-backed target).
pub type ExtractRelationshipTargetsFn = unsafe fn(ptr: *const u8) -> (*const FfiEntity, usize);

unsafe fn extract_relationship_targets_impl<RT>(ptr: *const u8) -> (*const FfiEntity, usize)
where
    RT: RelationshipTarget + core::ops::Deref<Target = [Entity]>,
{
    if ptr.is_null() {
        return (core::ptr::null(), 0);
    }
    let host_ptr = ptr.cast::<RT>();
    if !host_ptr.is_aligned() {
        return (core::ptr::null(), 0);
    }
    // SAFETY: Caller verified `ptr` points to an initialized `RT` component with read access.
    let slice: &[Entity] = unsafe { &*host_ptr };
    if slice.is_empty() {
        (core::ptr::null(), 0)
    } else {
        (slice.as_ptr().cast::<FfiEntity>(), slice.len())
    }
}

impl ModRegistry {
    /// Registers an opaque Bevy `Asset` type handle under `canonical_name` (plus optional `alias`),
    /// returning its 1-based [`BamComponent`] handle for use with `BamHostApi::asset_load`.
    pub fn register_asset_type(
        &mut self,
        canonical_name: &str,
        alias: Option<&str>,
        assets_res_id: ComponentId,
    ) -> Result<BamComponent, ComponentCollisionError> {
        let storage_flags = BAM_STORAGE_FLAG_IMMUTABLE;
        let uuid = ComponentUuid::from_canonical_name(canonical_name);
        self.check_registration_collision(
            canonical_name,
            uuid,
            0,
            1,
            storage_flags,
            Some(assets_res_id),
        )?;
        let handle = self.insert_bridged_entry(
            canonical_name,
            alias,
            uuid,
            assets_res_id,
            0,
            1,
            storage_flags,
            false,
            SymbolAddress::NULL,
            None,
            None,
            None,
        );
        Ok(handle)
    }

    /// Registers an external `#[repr(Rust)]` Bevy [`Component`] `T` bridged via [`HostWireBridge`],
    /// exposing `"bam.v0.<CANONICAL_NAME>.vtable"` (`MappedWireVTable<B::Wire>`) and optional
    /// `"bam.v0.<ALIAS>.vtable"`.
    pub fn register_bridged_component<T: Component, B: HostWireBridge<T>>(
        &mut self,
        comp_id: ComponentId,
        extra_storage_flags: u32,
        with_default: Option<InsertDefaultFn>,
    ) -> Result<BamComponent, ComponentCollisionError> {
        let storage_flags = (extra_storage_flags
            | if !B::SUPPORTS_SET {
                BAM_STORAGE_FLAG_IMMUTABLE
            } else {
                0
            })
            & !BAM_STORAGE_FLAG_RESOURCE;
        let uuid = WireVTableFactory::<T, B>::UUID;
        let size = core::mem::size_of::<T>();
        let align = core::mem::align_of::<T>();
        self.check_registration_collision(
            B::CANONICAL_NAME,
            uuid,
            size,
            align,
            storage_flags,
            Some(comp_id),
        )?;
        let vtable_ref: &'static MappedWireVTable<B::Wire> = &WireVTableFactory::<T, B>::VTABLE;
        let vtable_ptr = SymbolAddress::new(core::ptr::from_ref(vtable_ref).cast::<c_void>());
        Ok(self.insert_bridged_entry(
            B::CANONICAL_NAME,
            B::ALIAS,
            uuid,
            comp_id,
            size,
            align,
            storage_flags,
            false,
            vtable_ptr,
            None,
            with_default,
            None,
        ))
    }

    /// Helper returning `Some(insert_default_impl)` for `T: Component + Default` in `#![forbid(unsafe_code)]` callers.
    #[inline]
    pub const fn default_inserter_for<T: Component + Default>() -> InsertDefaultFn {
        insert_default_bridged::<T>
    }

    /// Registers an external `#[repr(Rust)]` Bevy [`Resource`] `R` bridged via [`HostWireBridge`],
    /// exposing `"bam.v0.<CANONICAL_NAME>.vtable"` (`MappedWireVTable<B::Wire>`).
    pub fn register_bridged_resource<R: Resource, B: HostWireBridge<R>>(
        &mut self,
        comp_id: ComponentId,
    ) -> Result<BamComponent, ComponentCollisionError> {
        let storage_flags = BAM_STORAGE_FLAG_RESOURCE
            | if !B::SUPPORTS_SET {
                BAM_STORAGE_FLAG_IMMUTABLE
            } else {
                0
            };
        let uuid = WireVTableFactory::<R, B>::UUID;
        let size = core::mem::size_of::<R>();
        let align = core::mem::align_of::<R>();
        self.check_registration_collision(
            B::CANONICAL_NAME,
            uuid,
            size,
            align,
            storage_flags,
            Some(comp_id),
        )?;
        let vtable_ref: &'static MappedWireVTable<B::Wire> = &WireVTableFactory::<R, B>::VTABLE;
        let vtable_ptr = SymbolAddress::new(core::ptr::from_ref(vtable_ref).cast::<c_void>());
        Ok(self.insert_bridged_entry(
            B::CANONICAL_NAME,
            B::ALIAS,
            uuid,
            comp_id,
            size,
            align,
            storage_flags,
            false,
            vtable_ptr,
            None,
            None,
            None,
        ))
    }

    /// Registers an external UTF-8 string Bevy [`Component`] `T` (such as `Name`) bridged via
    /// [`HostStringBridge`], exposing `"bam.v0.<CANONICAL_NAME>.vtable"` (`MappedStringVTable`).
    pub fn register_bridged_string_component<T: Component, B: HostStringBridge<T>>(
        &mut self,
        comp_id: ComponentId,
        extra_storage_flags: u32,
        with_default: Option<InsertDefaultFn>,
    ) -> Result<BamComponent, ComponentCollisionError> {
        let storage_flags = (extra_storage_flags
            | if !B::SUPPORTS_SET {
                BAM_STORAGE_FLAG_IMMUTABLE
            } else {
                0
            })
            & !BAM_STORAGE_FLAG_RESOURCE;
        let uuid = StringVTableFactory::<T, B>::UUID;
        let size = core::mem::size_of::<T>();
        let align = core::mem::align_of::<T>();
        self.check_registration_collision(
            B::CANONICAL_NAME,
            uuid,
            size,
            align,
            storage_flags,
            Some(comp_id),
        )?;
        let vtable_ref: &'static MappedStringVTable = &StringVTableFactory::<T, B>::VTABLE;
        let vtable_ptr = SymbolAddress::new(core::ptr::from_ref(vtable_ref).cast::<c_void>());
        Ok(self.insert_bridged_entry(
            B::CANONICAL_NAME,
            B::ALIAS,
            uuid,
            comp_id,
            size,
            align,
            storage_flags,
            false,
            vtable_ptr,
            None,
            with_default,
            None,
        ))
    }

    /// Registers an entity-only Bevy [`EntityEvent`] `E` implementing `From<Entity>` in `world`
    /// under `canonical_name` and optional `alias`, synthesizing a guaranteed `#[repr(C)]`
    /// [`FfiEntity`] payload for guest observers regardless of `E`'s Rust layout.
    pub fn register_bridged_entity_event<E>(
        &mut self,
        world: &mut World,
        canonical_name: &str,
        alias: Option<&str>,
    ) -> Result<BamComponent, ComponentCollisionError>
    where
        E: EntityEvent + Event + Copy + From<Entity>,
        for<'a> E::Trigger<'a>: Default,
    {
        self.register_bridged_entity_event_with::<E>(world, canonical_name, alias, |w, entity| {
            w.trigger(E::from(entity));
        })
    }

    /// Registers an entity-only Bevy [`EntityEvent`] `E` (such as `bevy_scene::Ready`, which does
    /// not implement `From<Entity>`) in `world` under `canonical_name` and optional `alias` using
    /// a caller-supplied `trigger_entity_fn`, synthesizing a guaranteed `#[repr(C)]` [`FfiEntity`]
    /// payload for guest observers regardless of `E`'s Rust layout.
    pub fn register_bridged_entity_event_with<E>(
        &mut self,
        world: &mut World,
        canonical_name: &str,
        alias: Option<&str>,
        trigger_entity_fn: EntityEventTriggerFn,
    ) -> Result<BamComponent, ComponentCollisionError>
    where
        E: EntityEvent + Event + Copy,
        for<'a> E::Trigger<'a>: Default,
    {
        let event_key = world.register_event_key::<E>();
        let storage_flags =
            BAM_STORAGE_FLAG_EVENT | BAM_STORAGE_FLAG_ENTITY_EVENT | BAM_STORAGE_FLAG_IMMUTABLE;
        let comp_id = event_key.component_id();
        let uuid = ComponentUuid::from_parts(
            canonical_name,
            core::mem::size_of::<FfiEntity>(),
            core::mem::align_of::<FfiEntity>(),
            0,
        );
        self.check_registration_collision(
            canonical_name,
            uuid,
            core::mem::size_of::<FfiEntity>(),
            core::mem::align_of::<FfiEntity>(),
            storage_flags,
            Some(comp_id),
        )?;
        let handle = self.insert_bridged_entry(
            canonical_name,
            alias,
            uuid,
            comp_id,
            core::mem::size_of::<FfiEntity>(),
            core::mem::align_of::<FfiEntity>(),
            storage_flags,
            true,
            SymbolAddress::NULL,
            None,
            None,
            None,
        );
        {
            let snap = Arc::make_mut(&mut self.snapshot);
            let idx = (handle.0 - 1) as usize;
            snap.entries[idx].synthesizes_ffi_entity_payload = true;
            snap.entries[idx].event_key = Some(event_key);
            snap.entries[idx].entity_event_trigger_fn = Some(trigger_entity_fn);
            snap.entries[idx].trigger_event_fn = None;
            snap.entries[idx].extract_event_target_fn = Some(extract_entity_only_target_impl::<E>);
        }
        self.publish_after_mutation();
        Ok(handle)
    }

    /// Registers Bevy's built-in [`Children`] relationship-target component under [`BAM_CHILDREN_NAME`]
    /// (plus `"Children"` alias) for zero-copy read-only hierarchy inspection via `sys_entity_children`.
    pub fn register_children_hierarchy(
        &mut self,
        children_id: ComponentId,
    ) -> Result<BamComponent, ComponentCollisionError> {
        self.register_relationship_target::<Children>(
            BAM_CHILDREN_NAME,
            Some("Children"),
            children_id,
        )
    }

    /// Registers any slice-backed Bevy [`RelationshipTarget`] component `RT` (`Deref<Target = [Entity]>`)
    /// under `canonical_name` (plus optional `alias`) so guest systems declaring read access to `RT`
    /// can borrow its contiguous target entities via `BamHostApi::sys_entity_children`.
    pub fn register_relationship_target<RT>(
        &mut self,
        canonical_name: &str,
        alias: Option<&str>,
        comp_id: ComponentId,
    ) -> Result<BamComponent, ComponentCollisionError>
    where
        RT: Component + RelationshipTarget + core::ops::Deref<Target = [Entity]>,
    {
        let storage_flags = BAM_STORAGE_FLAG_IMMUTABLE;
        let uuid = ComponentUuid::from_canonical_name(canonical_name);
        let size = core::mem::size_of::<RT>();
        let align = core::mem::align_of::<RT>();
        self.check_registration_collision(
            canonical_name,
            uuid,
            size,
            align,
            storage_flags,
            Some(comp_id),
        )?;
        Ok(self.insert_bridged_entry(
            canonical_name,
            alias,
            uuid,
            comp_id,
            size,
            align,
            storage_flags,
            false,
            SymbolAddress::NULL,
            None,
            None,
            Some(extract_relationship_targets_impl::<RT>),
        ))
    }

    #[allow(clippy::too_many_arguments)]
    fn insert_bridged_entry(
        &mut self,
        canonical_name: &str,
        alias: Option<&str>,
        uuid: ComponentUuid,
        comp_id: ComponentId,
        size: usize,
        align: usize,
        storage_flags: u32,
        is_abi_pod: bool,
        vtable_ptr: SymbolAddress,
        insert_raw_fn: Option<InsertRawFn>,
        insert_default_fn: Option<InsertDefaultFn>,
        extract_relationship_targets_fn: Option<ExtractRelationshipTargetsFn>,
    ) -> BamComponent {
        let handle = {
            let snap = Arc::make_mut(&mut self.snapshot);
            let handle = if let Some(&existing_handle) = snap.by_name.get(canonical_name) {
                let idx = (existing_handle.0 - 1) as usize;
                snap.entries[idx].vtable_ptr = vtable_ptr;
                snap.entries[idx].insert_raw_fn = insert_raw_fn.or(snap.entries[idx].insert_raw_fn);
                snap.entries[idx].insert_default_fn =
                    insert_default_fn.or(snap.entries[idx].insert_default_fn);
                snap.entries[idx].extract_relationship_targets_fn = extract_relationship_targets_fn
                    .or(snap.entries[idx].extract_relationship_targets_fn);
                snap.entries[idx].is_dynamic = false;
                snap.entries[idx].owner = None;
                existing_handle
            } else {
                let next_raw = u32::try_from(snap.entries.len() + 1)
                    .expect("ModRegistry exceeded u32::MAX component handles");
                let handle = BamComponent::from_raw(next_raw);
                let arc_name: Arc<str> = Arc::from(canonical_name);
                let entry = RegisteredComponent {
                    handle,
                    name: Arc::clone(&arc_name),
                    uuid,
                    component_id: comp_id,
                    size,
                    align,
                    schema_fingerprint: 0,
                    fields: Arc::from([]),
                    owned_field_strings: None,
                    storage_flags,
                    is_abi_pod,
                    is_dynamic: false,
                    vtable_ptr,
                    insert_raw_fn,
                    insert_default_fn,
                    drop_fn: None,
                    drop_slot: None,
                    has_on_add_hook: false,
                    has_on_remove_hook: false,
                    create_msg_cursor_fn: None,
                    msg_read_batch_fn: None,
                    msg_send_fn: None,
                    msg_write_batch_fn: None,
                    validates_entity_payload: false,
                    synthesizes_ffi_entity_payload: false,
                    event_key: None,
                    entity_event_trigger_fn: None,
                    trigger_event_fn: None,
                    extract_event_target_fn: None,
                    extract_relationship_targets_fn,
                    owner: None,
                };
                snap.entries.push(entry);
                snap.by_name.insert(arc_name, handle);
                snap.by_uuid.insert(uuid, handle);
                snap.by_component_id.insert(comp_id, handle);
                handle
            };

            if let Some(short) = alias
                && !snap.by_name.contains_key(short)
            {
                snap.by_name.insert(Arc::from(short), handle);
            }

            if !vtable_ptr.is_null() {
                let vtable_sym = format!("bam.v0.{canonical_name}.vtable");
                snap.symbols.insert(
                    Arc::from(vtable_sym.as_str()),
                    SymbolRecord {
                        addr: vtable_ptr,
                        owner: None,
                    },
                );
                if let Some(short) = alias {
                    let alias_sym = format!("bam.v0.{short}.vtable");
                    snap.symbols
                        .entry(Arc::from(alias_sym.as_str()))
                        .or_insert(SymbolRecord {
                            addr: vtable_ptr,
                            owner: None,
                        });
                }
            }
            handle
        };
        self.publish_after_mutation();
        handle
    }

    /// Attaches the internal world-table directory token (`world_id`) to this registry's snapshot
    /// so `HostContext` FFI entrypoints (`host_intern_symbol`, `host_bind_named_object`) resolve
    /// the world's `BamSymbolTable` and `ModNamedObjects` resources in `O(1)` without exposing
    /// any `"bam.v0.*"` guest symbol.
    pub(crate) fn attach_world_tables_id(&mut self, world_id: usize) {
        if world_id == 0 || self.snapshot.world_tables_id() == Some(world_id) {
            return;
        }
        let encoded = SymbolAddress::new(core::ptr::without_provenance::<c_void>(world_id));
        {
            let snap = Arc::make_mut(&mut self.snapshot);
            snap.symbols.insert(
                Arc::from(INTERNAL_WORLD_TABLES_KEY),
                SymbolRecord {
                    addr: encoded,
                    owner: None,
                },
            );
        }
        self.publish_after_mutation();
    }

    /// Atomically remaps an existing dynamic [`BamComponent`] handle to a freshly migrated Bevy
    /// [`ComponentId`], layout `(size, align, storage_flags)`, `schema_fingerprint`, and retained
    /// `BamFieldDesc` reflection metadata, publishing a new CoW [`RegistrySnapshot`].
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn remap_migrated_component(
        &mut self,
        handle: BamComponent,
        new_comp_id: ComponentId,
        new_uuid: ComponentUuid,
        new_size: usize,
        new_align: usize,
        new_storage_flags: u32,
        new_schema_fingerprint: u64,
        new_fields: Arc<[BamFieldDesc]>,
        new_owned_field_strings: Option<OwnedFieldStrings>,
        new_vtable_ptr: *const c_void,
        owner: Option<ModId>,
    ) -> Result<(), ComponentCollisionError> {
        let Some(existing) = self.snapshot.get_by_handle(handle) else {
            return Err(ComponentCollisionError::MetadataMismatch {
                name: format!("BamComponent({})", handle.0),
                reason: "cannot remap unregistered component handle".to_string(),
            });
        };
        if !existing.is_dynamic {
            return Err(ComponentCollisionError::MetadataMismatch {
                name: existing.name.to_string(),
                reason: "cannot remap host-owned static component".to_string(),
            });
        }
        if let Some(other) = self.snapshot.get_by_uuid(&new_uuid)
            && other.handle != handle
        {
            return Err(ComponentCollisionError::UuidCollision {
                existing_name: other.name.to_string(),
                new_name: existing.name.to_string(),
                uuid: new_uuid,
            });
        }

        let sym_addr = SymbolAddress::new(new_vtable_ptr);
        {
            let snap = Arc::make_mut(&mut self.snapshot);
            let idx = (handle.0 - 1) as usize;
            let (old_comp_id, old_uuid, comp_name) = {
                let entry = &snap.entries[idx];
                (entry.component_id, entry.uuid, Arc::clone(&entry.name))
            };

            if old_comp_id != new_comp_id {
                if snap.by_component_id.get(&old_comp_id) == Some(&handle) {
                    snap.by_component_id.remove(&old_comp_id);
                }
                snap.by_component_id.insert(new_comp_id, handle);
            }
            if old_uuid != new_uuid {
                if snap.by_uuid.get(&old_uuid) == Some(&handle) {
                    snap.by_uuid.remove(&old_uuid);
                }
                snap.by_uuid.insert(new_uuid, handle);
            }

            let entry = &mut snap.entries[idx];
            entry.component_id = new_comp_id;
            entry.uuid = new_uuid;
            entry.size = new_size;
            entry.align = new_align;
            entry.storage_flags = new_storage_flags;
            entry.schema_fingerprint = new_schema_fingerprint;
            entry.fields = new_fields;
            entry.owned_field_strings = new_owned_field_strings;
            entry.vtable_ptr = sym_addr;
            // SAFETY: `new_comp_id` was registered in `world` with `(new_size, new_align)`.
            entry.event_key = Some(unsafe { EventKey::new(new_comp_id) });
            if owner.is_some() {
                entry.owner = owner;
            }

            let vtable_key: Arc<str> = Arc::from(format!("bam.v0.{comp_name}.vtable").as_str());
            if !sym_addr.is_null() {
                snap.symbols.insert(
                    vtable_key,
                    SymbolRecord {
                        addr: sym_addr,
                        owner: entry.owner,
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
        self.publish_after_mutation();
        Ok(())
    }
}

pub(crate) const INTERNAL_WORLD_TABLES_KEY: &str = "__bam_internal.world_tables_id";

impl RegistrySnapshot {
    /// Reads the internal world-table directory token (`world_id`) attached to this snapshot,
    /// if any, using strict-provenance `.addr()`.
    #[inline]
    pub(crate) fn world_tables_id(&self) -> Option<usize> {
        let rec = self.symbols.get(INTERNAL_WORLD_TABLES_KEY)?;
        let id = rec.addr.as_ptr().addr();
        (id != 0).then_some(id)
    }
}
