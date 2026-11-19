//! Pre-unload snapshot and post-init restoration for live archetype column schema migration
//! across [`reload_mod`](crate::loader::mod_host).
//!
//! Ensures zero cross-allocator crossover:
//! 1. While the old `.dylib` and [`BamInstance`](bevy_abi_mod_sys::BamInstance) are still mapped,
//!    every dynamic component owned by the reloading mod with a `"bam.v0.<name>.export_fields"`
//!    witness exports all live entity values into host-owned `(field_name, type_tag, Vec<u8>)`
//!    records, then removes those components via `remove_by_id` so the old mod's `drop_fn` frees
//!    old guest-heap allocations on the old guest allocator (`live_values` returns to `0`).
//! 2. Retires the migrated dynamic component entries from [`ModRegistry`]'s name/UUID/ComponentId
//!    indexes (preserving `BamComponent` dense indices) so the replacement mod can register an
//!    evolved `(uuid, size, align, drop_fn, hooks)` layout without `TYPE_MISMATCH`.
//! 3. After the new `.dylib` initializes, allocates an aligned host scratch buffer per entity,
//!    initializes it on the new guest via `"bam.v0.<name>.init_default"`, replays saved fields
//!    via `"bam.v0.<name>.import_field"` (ignoring `NOT_FOUND` for removed fields, widening
//!    widened primitives, keeping `Default` for newly added fields, and allocating `String`/`Vec`
//!    on the new guest's heap), and transfers ownership into Bevy ECS via `insert_by_id`.

use crate::ffi::register::LoadContext;
use crate::loader::error::ModLoadError;
use crate::loader::mod_host::{LoadedMod, ModHost, ModId, ModScoped};
use crate::loader::symbols::ModNamedObjects;
use crate::registry::{
    ModRegistry, RegisteredComponent, component_flags_to_storage_flags,
    extract_owned_dynamic_fields,
};
use crate::sync_ptr::SymbolAddress;
use bevy_abi_mod_sys::{
    BAM_STORAGE_FLAG_ENTITY_EVENT, BAM_STORAGE_FLAG_EVENT, BAM_STORAGE_FLAG_MESSAGE,
    BAM_STORAGE_FLAG_RESOURCE, BamComponent, BamRowMigrateFn, ComponentDesc, ComponentDropFn,
    ComponentUuid, FfiResult, FfiSlice, StrView,
};
use bevy_ecs::component::{ComponentCloneBehavior, ComponentDescriptor, ComponentId, StorageType};
use bevy_ecs::entity::Entity;
use bevy_ecs::entity_disabling::Disabled;
use bevy_ecs::ptr::OwningPtr;
use bevy_ecs::query::{Allow, FilteredAccess, QueryBuilder, Without};
use bevy_ecs::resource::IsResource;
use bevy_ecs::storage::TableRow;
use bevy_ecs::world::World;
use core::alloc::Layout;
use core::any::TypeId;
use core::ffi::{CStr, c_void};
use core::ptr::NonNull;
use nonmax::NonMaxU32;
use std::sync::Arc;

type BamInitDefaultFn = unsafe extern "C" fn(dst: *mut c_void) -> FfiResult;
type BamExportFieldsSinkFn = unsafe extern "C" fn(
    ctx: *mut c_void,
    field_name: StrView,
    type_tag: StrView,
    payload: FfiSlice<u8>,
) -> FfiResult;
type BamExportFieldsFn = unsafe extern "C" fn(
    src_ptr: *const c_void,
    sink: Option<BamExportFieldsSinkFn>,
    sink_ctx: *mut c_void,
) -> FfiResult;
type BamImportFieldFn = unsafe extern "C" fn(
    dst_ptr: *mut c_void,
    field_name: StrView,
    type_tag: StrView,
    payload: FfiSlice<u8>,
) -> FfiResult;

struct OwnedMigrationBuffer {
    ptr: *mut u8,
    layout: Layout,
}

impl OwnedMigrationBuffer {
    fn new(layout: Layout) -> Self {
        if layout.size() == 0 {
            return Self {
                ptr: NonNull::<u8>::dangling().as_ptr(),
                layout,
            };
        }
        // SAFETY: `layout.size() > 0` and `layout.align()` is a valid power of two.
        let ptr = unsafe { std::alloc::alloc_zeroed(layout) };
        if ptr.is_null() {
            std::alloc::handle_alloc_error(layout);
        }
        Self { ptr, layout }
    }
}

impl Drop for OwnedMigrationBuffer {
    fn drop(&mut self) {
        if self.layout.size() > 0 {
            // SAFETY: `self.ptr` was allocated with `std::alloc::alloc_zeroed(self.layout)`.
            unsafe {
                std::alloc::dealloc(self.ptr, self.layout);
            }
        }
    }
}

/// Single exported field payload captured from a live component prior to reload teardown.
#[derive(Clone, Debug)]
pub struct ExportedFieldRecord {
    /// Field name (or `"__variant"` / `"<Variant>::<field>"` for enums).
    pub field_name: String,
    /// Canonical witness `type_tag` (e.g. `"i32"`, `"String"`, `"Vec<u32>"`).
    pub type_tag: String,
    /// Raw field bytes owned by the host during reload transition.
    pub bytes: Vec<u8>,
}

/// Snapshot of one live entity's exported component fields.
#[derive(Clone, Debug)]
pub struct SnapshotEntityFields {
    /// Target Bevy [`Entity`] (or [`Entity::PLACEHOLDER`] for singleton resources).
    pub entity: Entity,
    /// Ordered sequence of exported field payloads.
    pub fields: Vec<ExportedFieldRecord>,
}

/// Snapshot of all entities carrying one dynamic component owned by the reloading mod.
#[derive(Clone, Debug)]
pub struct SnapshotComponentColumn {
    /// Canonical component name (e.g. `"unit_mod::UnitState"`).
    pub canonical_name: Arc<str>,
    /// `true` if this entry was registered as a singleton `IsResource`.
    pub is_resource: bool,
    /// Exported per-entity field records.
    pub entities: Vec<SnapshotEntityFields>,
}

/// Metadata for a dynamic POD component column retained zero-copy in place across reload
/// because it carries a non-zero `schema_fingerprint` and has no guest-heap destructor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetainedZeroCopyColumn {
    /// Canonical component name (e.g. `"sim::Position"`).
    pub canonical_name: Arc<str>,
    /// Preserved 1-based [`BamComponent`] handle.
    pub handle: BamComponent,
    /// Retained Bevy [`ComponentId`] whose `BlobVec` / `SparseSet` column was left untouched.
    pub component_id: ComponentId,
    /// Deterministic 64-bit schema fingerprint enabling zero-copy retention.
    pub schema_fingerprint: u64,
    /// Component byte size.
    pub size: usize,
    /// Component byte alignment.
    pub align: usize,
    /// Storage/mutability flags (`BAM_STORAGE_FLAG_*`).
    pub storage_flags: u32,
}

/// Complete pre-unload dynamic component snapshot produced for one reloading [`ModId`].
#[derive(Clone, Debug, Default)]
pub struct ReloadColumnSnapshot {
    /// Snapshotted component columns requiring post-reload restoration, in deterministic registry order.
    pub columns: Vec<SnapshotComponentColumn>,
    /// Dynamic POD component columns retained zero-copy in place across reload (0 bytes copied).
    pub retained_zero_copy: Vec<RetainedZeroCopyColumn>,
    /// Total raw field bytes copied during pre-unload snapshot (`0` when all columns retain zero-copy).
    pub bytes_copied: usize,
}

unsafe extern "C" fn collect_exported_field_sink(
    ctx: *mut c_void,
    field_name: StrView,
    type_tag: StrView,
    payload: FfiSlice<u8>,
) -> FfiResult {
    if ctx.is_null() {
        return FfiResult::NULL_POINTER;
    }
    // SAFETY: `ctx` points to a stack-local `&mut Vec<ExportedFieldRecord>` valid for this synchronous call.
    let out = unsafe { &mut *ctx.cast::<Vec<ExportedFieldRecord>>() };
    // SAFETY: Guest witness symbol guarantees `field_name` points to initialized UTF-8 bytes.
    let Ok(name_str) = (unsafe { field_name.try_as_str() }) else {
        return FfiResult::INVALID_UTF8;
    };
    // SAFETY: Guest witness symbol guarantees `type_tag` points to initialized UTF-8 bytes.
    let Ok(tag_str) = (unsafe { type_tag.try_as_str() }) else {
        return FfiResult::INVALID_UTF8;
    };
    let bytes = if payload.len == 0 {
        Vec::new()
    } else if payload.ptr.is_null() {
        return FfiResult::NULL_POINTER;
    } else {
        // SAFETY: `payload.ptr` is non-null and readable for `payload.len` bytes for this call.
        unsafe { payload.as_slice() }.to_vec()
    };
    out.push(ExportedFieldRecord {
        field_name: name_str.to_owned(),
        type_tag: tag_str.to_owned(),
        bytes,
    });
    FfiResult::OK
}

fn resolve_component_symbol(
    loaded: &LoadedMod,
    registry: &ModRegistry,
    comp_name: &str,
    suffix: &str,
) -> Option<*const c_void> {
    let exact = format!("bam.v0.{comp_name}.{suffix}");
    if let Some(ptr) = loaded.resolve_guest_symbol(&exact) {
        return Some(ptr);
    }
    if let Some((_, short_name)) = comp_name.rsplit_once("::") {
        let short_sym = format!("bam.v0.{short_name}.{suffix}");
        if let Some(ptr) = loaded.resolve_guest_symbol(&short_sym) {
            return Some(ptr);
        }
    }
    registry.resolve_symbol(&exact)
}

struct PendingRemoval {
    name: Arc<str>,
    uuid: ComponentUuid,
    component_id: ComponentId,
    handle: BamComponent,
    is_resource: bool,
    entities: Vec<Entity>,
}

/// Captures field-level snapshots of all dynamic components owned by `mod_id` while its old
/// shared library and `BamInstance` remain mapped, removes those components from live entities
/// (so old `drop_fn`s free guest-heap allocations on the old guest allocator), and retires their
/// [`ModRegistry`] name/UUID/`ComponentId` entries so the replacement mod can register evolved
/// schemas cleanly.
pub fn snapshot_mod_dynamic_columns_for_reload(
    world: &mut World,
    mod_id: ModId,
) -> Result<ReloadColumnSnapshot, ModLoadError> {
    let registry = world.get_resource::<ModRegistry>().cloned();
    let Some(registry) = registry else {
        return Ok(ReloadColumnSnapshot::default());
    };

    let owned_entries: Vec<RegisteredComponent> = registry
        .snapshot()
        .entries()
        .iter()
        .filter(|entry| entry.is_dynamic && entry.owner == Some(mod_id))
        .cloned()
        .collect();

    if owned_entries.is_empty() {
        return Ok(ReloadColumnSnapshot::default());
    }

    // Zero-copy retention fast path: pure `AbiPod` non-resource dynamic components carrying a
    // non-zero `schema_fingerprint` hold zero guest-heap allocations, so their Bevy `ComponentId`
    // and `BlobVec`/`SparseSet` column storage stay untouched in place with 0 bytes copied.
    let mut retained_zero_copy = Vec::new();
    let mut snapshot_candidates = Vec::new();
    for entry in owned_entries {
        if entry.is_abi_pod
            && entry.drop_fn.is_none()
            && entry.drop_slot.is_none()
            && !entry.is_resource()
            && entry.schema_fingerprint != 0
        {
            retained_zero_copy.push(RetainedZeroCopyColumn {
                canonical_name: Arc::clone(&entry.name),
                handle: entry.handle,
                component_id: entry.component_id,
                schema_fingerprint: entry.schema_fingerprint,
                size: entry.size,
                align: entry.align,
                storage_flags: entry.storage_flags,
            });
        } else {
            snapshot_candidates.push(entry);
        }
    }

    if snapshot_candidates.is_empty() {
        return Ok(ReloadColumnSnapshot {
            columns: Vec::new(),
            retained_zero_copy,
            bytes_copied: 0,
        });
    }

    let export_fns: Vec<Option<BamExportFieldsFn>> = {
        let Some(mod_host) = world.get_resource::<ModHost>() else {
            return Err(ModLoadError::ModNotFound { id: mod_id });
        };
        let Some(loaded) = mod_host.get_mod(mod_id) else {
            return Err(ModLoadError::ModNotFound { id: mod_id });
        };
        snapshot_candidates
            .iter()
            .map(|entry| {
                resolve_component_symbol(loaded, &registry, &entry.name, "export_fields").map(
                    |ptr| {
                        // SAFETY: Witness symbol `"bam.v0.<name>.export_fields"` exported by
                        // `#[derive(ModComponent)]` uses the exact `BamExportFieldsFn` C-ABI signature.
                        unsafe { core::mem::transmute::<*const c_void, BamExportFieldsFn>(ptr) }
                    },
                )
            })
            .collect()
    };

    let disabled_id = world.components().get_id(TypeId::of::<Disabled>());
    let is_resource_id = world.components().get_id(TypeId::of::<IsResource>());

    let mut columns = Vec::new();
    let mut removals = Vec::new();
    let mut bytes_copied = 0usize;

    for (entry, export_fn_opt) in snapshot_candidates.into_iter().zip(export_fns) {
        let Some(export_fn) = export_fn_opt else {
            // Pure `AbiPod` components without witness symbols stay on entities if non-resource,
            // while dynamic `IsResource` singletons still retire cleanly across reloads.
            if entry.is_resource() {
                removals.push(PendingRemoval {
                    name: entry.name,
                    uuid: entry.uuid,
                    component_id: entry.component_id,
                    handle: entry.handle,
                    is_resource: true,
                    entities: Vec::new(),
                });
            }
            continue;
        };

        if entry.is_resource() {
            let mut entity_snapshots = Vec::new();
            if let Some(res_ptr) = world.get_resource_by_id(entry.component_id) {
                let src_ptr = if entry.size == 0 {
                    NonNull::<u8>::dangling().as_ptr().cast::<c_void>()
                } else {
                    res_ptr.as_ptr().cast::<c_void>()
                };
                let mut fields: Vec<ExportedFieldRecord> = Vec::new();
                // SAFETY: `entry`'s owning mod is still mapped, `src_ptr` points to a live
                // initialized resource value, and `collect_exported_field_sink` writes into `fields`.
                let rc = unsafe {
                    export_fn(
                        src_ptr,
                        Some(collect_exported_field_sink),
                        (&raw mut fields).cast::<c_void>(),
                    )
                };
                if !rc.is_ok() {
                    return Err(ModLoadError::SchemaMigrationFailed {
                        component_name: entry.name.to_string(),
                        status: rc,
                        detail: "export_fields failed on dynamic resource".to_string(),
                    });
                }
                bytes_copied += fields.iter().map(|f| f.bytes.len()).sum::<usize>();
                entity_snapshots.push(SnapshotEntityFields {
                    entity: Entity::PLACEHOLDER,
                    fields,
                });
            }
            columns.push(SnapshotComponentColumn {
                canonical_name: Arc::clone(&entry.name),
                is_resource: true,
                entities: entity_snapshots,
            });
            removals.push(PendingRemoval {
                name: entry.name,
                uuid: entry.uuid,
                component_id: entry.component_id,
                handle: entry.handle,
                is_resource: true,
                entities: Vec::new(),
            });
            continue;
        }

        let mut builder = QueryBuilder::<Entity>::new(world);
        builder.with_id(entry.component_id);
        if let Some(res_marker_id) = is_resource_id {
            builder.without_id(res_marker_id);
        }
        if let Some(dis_id) = disabled_id {
            let mut acc = FilteredAccess::default();
            acc.access_mut().add_archetypal(dis_id);
            builder.extend_access(acc);
        }
        let matched_entities: Vec<Entity> = builder.build().iter(world).collect();

        let mut entity_snapshots = Vec::with_capacity(matched_entities.len());
        for &entity in &matched_entities {
            let Ok(ent_ref) = world.get_entity(entity) else {
                continue;
            };
            let Ok(comp_ptr) = ent_ref.get_by_id(entry.component_id) else {
                continue;
            };
            let src_ptr = if entry.size == 0 {
                NonNull::<u8>::dangling().as_ptr().cast::<c_void>()
            } else {
                comp_ptr.as_ptr().cast::<c_void>()
            };
            let mut fields: Vec<ExportedFieldRecord> = Vec::new();
            // SAFETY: `entry`'s owning mod is still mapped, `src_ptr` points to the live
            // initialized component on `entity`, and `fields` is stack-pinned for this call.
            let rc = unsafe {
                export_fn(
                    src_ptr,
                    Some(collect_exported_field_sink),
                    (&raw mut fields).cast::<c_void>(),
                )
            };
            if !rc.is_ok() {
                return Err(ModLoadError::SchemaMigrationFailed {
                    component_name: entry.name.to_string(),
                    status: rc,
                    detail: format!("export_fields failed on entity {entity:?}"),
                });
            }
            bytes_copied += fields.iter().map(|f| f.bytes.len()).sum::<usize>();
            entity_snapshots.push(SnapshotEntityFields { entity, fields });
        }

        columns.push(SnapshotComponentColumn {
            canonical_name: Arc::clone(&entry.name),
            is_resource: false,
            entities: entity_snapshots,
        });
        removals.push(PendingRemoval {
            name: entry.name,
            uuid: entry.uuid,
            component_id: entry.component_id,
            handle: entry.handle,
            is_resource: false,
            entities: matched_entities,
        });
    }

    // All exports succeeded cleanly. Now strip old components from entities/resources while the old
    // `.dylib` is still mapped so old `drop_fn`s run on the old allocator (`live_values -> 0`),
    // and retire their registry lookups so the new mod can register evolved layouts.
    for removal in &removals {
        if removal.is_resource {
            world.remove_resource_by_id(removal.component_id);
        } else {
            for &entity in &removal.entities {
                if let Ok(mut ent_mut) = world.get_entity_mut(entity) {
                    ent_mut.remove_by_id(removal.component_id);
                }
            }
        }
    }

    if !removals.is_empty()
        && let Some(mut reg) = world.get_resource_mut::<ModRegistry>()
    {
        let mut snap = (*reg.snapshot_arc()).clone();
        for removal in removals {
            if snap.by_name.get(removal.name.as_ref()) == Some(&removal.handle) {
                snap.by_name.remove(removal.name.as_ref());
            }
            if snap.by_uuid.get(&removal.uuid) == Some(&removal.handle) {
                snap.by_uuid.remove(&removal.uuid);
            }
            if snap.by_component_id.get(&removal.component_id) == Some(&removal.handle) {
                snap.by_component_id.remove(&removal.component_id);
            }
            snap.required_components.remove(&removal.handle);
            snap.extended_hook_flags.remove(&removal.handle);
            if let Some(idx) = removal.handle.0.checked_sub(1).map(|i| i as usize)
                && let Some(entry) = snap.entries.get_mut(idx)
            {
                entry.owner = None;
                entry.drop_fn = None;
                entry.vtable_ptr = SymbolAddress::NULL;
            }
        }
        reg.restore_snapshot(Arc::new(snap));
    }

    Ok(ReloadColumnSnapshot {
        columns,
        retained_zero_copy,
        bytes_copied,
    })
}

/// Reassigns all [`ModScoped(old_id)`](ModScoped) entities (including `Disabled` entities) in
/// `world` to [`ModScoped(new_id)`](ModScoped) after a successful reload in a single zero-allocation
/// contiguous column scan.
pub fn migrate_mod_scoped_entities(world: &mut World, old_id: ModId, new_id: ModId) {
    ModNamedObjects::strip_mod_scoped_in_world(world);
    if world
        .components()
        .get_id(TypeId::of::<ModScoped>())
        .is_none()
    {
        return;
    }
    let mut query = world.query_filtered::<&mut ModScoped, Allow<Disabled>>();
    for mut scoped in query.iter_mut(world) {
        if scoped.0 == old_id {
            scoped.0 = new_id;
        }
    }
}

/// Bulk-despawns any remaining [`ModScoped(old_id)`](ModScoped) entities if a reload fails
/// permanently after `old_id` was already unloaded (promoting persistent named objects first).
pub fn cleanup_orphaned_mod_scoped_entities(world: &mut World, old_id: ModId) {
    ModNamedObjects::strip_mod_scoped_in_world(world);
    if world
        .components()
        .get_id(TypeId::of::<ModScoped>())
        .is_none()
    {
        return;
    }
    world.despawn_all_where::<(&ModScoped,), (Allow<Disabled>, Without<IsResource>)>(|(s,)| {
        s.0 == old_id
    });
}

struct MigratedChunk {
    entities: Vec<Entity>,
    dst_buf: OwnedMigrationBuffer,
}

/// Migrates all live entities carrying dynamic `AbiPod` component `component` from its active
/// schema (`old_fingerprint`) to `new_desc`, transforming each matching archetype table via a
/// single aligned contiguous column buffer (zero per-entity heap allocations) and atomically
/// remapping the existing `BamComponent(u32)` handle in [`ModRegistry`].
///
/// # Safety
/// - `new_desc` must have passed `ComponentDesc::validate()` with valid `name` and optional `fields`.
/// - `migrate_fn` must uphold [`BamRowMigrateFn`]'s nounwind C-ABI safety contract.
pub(crate) unsafe fn migrate_component_column_ffi(
    ctx: &mut LoadContext,
    component: BamComponent,
    old_fingerprint: u64,
    new_desc: &ComponentDesc,
    migrate_fn: BamRowMigrateFn,
    user_data: *mut c_void,
) -> FfiResult {
    let Some(old_entry) = ctx.registry.snapshot().get_by_handle(component).cloned() else {
        ctx.host_ctx.set_last_error(format!(
            "migrate_component_column component handle {} is not registered",
            component.0
        ));
        return FfiResult::INVALID_COMPONENT;
    };

    if !old_entry.is_dynamic
        || old_entry.is_resource()
        || old_entry.is_message()
        || old_entry.is_event()
    {
        ctx.host_ctx.set_last_error(format!(
            "migrate_component_column '{}' requires a dynamic non-resource/non-message/non-event component",
            old_entry.name
        ));
        return FfiResult::PERMISSION_DENIED;
    }

    if let (Some(existing_owner), Some(caller_mod)) = (old_entry.owner, ctx.mod_id)
        && existing_owner != caller_mod
    {
        ctx.host_ctx.set_last_error(format!(
            "migrate_component_column '{}' belongs to a different active mod",
            old_entry.name
        ));
        return FfiResult::PERMISSION_DENIED;
    }

    if old_fingerprint != 0 && old_entry.schema_fingerprint != old_fingerprint {
        ctx.host_ctx.set_last_error(format!(
            "migrate_component_column '{}' old_fingerprint mismatch (expected 0x{:016x}, got 0x{:016x})",
            old_entry.name, old_entry.schema_fingerprint, old_fingerprint
        ));
        return FfiResult::TYPE_MISMATCH;
    }

    // SAFETY: `new_desc.validate()` verified `!new_desc.name.is_null()`.
    let Ok(new_name_str) = (unsafe { CStr::from_ptr(new_desc.name) }).to_str() else {
        ctx.host_ctx
            .set_last_error("migrate_component_column new_desc `name` is not valid UTF-8");
        return FfiResult::INVALID_UTF8;
    };
    if new_name_str.is_empty() {
        ctx.host_ctx
            .set_last_error("migrate_component_column new_desc `name` must not be empty");
        return FfiResult::INVALID_ARGUMENT;
    }

    if new_desc.is_summary_tick() && new_desc.is_sparse_set() {
        ctx.host_ctx.set_last_error(
            "migrate_component_column new_desc cannot combine SUMMARY_TICK with SPARSE_SET storage",
        );
        return FfiResult::INVALID_ARGUMENT;
    }

    let Some(new_layout) = new_desc.layout() else {
        ctx.host_ctx.set_last_error(
            "migrate_component_column new_desc violates C struct size/align invariants",
        );
        return FfiResult::INVALID_ARGUMENT;
    };

    let new_storage_flags = component_flags_to_storage_flags(new_desc.flags);
    if (new_storage_flags
        & (BAM_STORAGE_FLAG_RESOURCE
            | BAM_STORAGE_FLAG_MESSAGE
            | BAM_STORAGE_FLAG_EVENT
            | BAM_STORAGE_FLAG_ENTITY_EVENT))
        != 0
    {
        ctx.host_ctx.set_last_error(
            "migrate_component_column new_desc cannot target resource, message, or event storage",
        );
        return FfiResult::INVALID_ARGUMENT;
    }

    if !old_entry.is_abi_pod
        || old_entry.drop_fn.is_some()
        || old_entry.drop_slot.is_some()
        || !new_desc.is_pod()
    {
        ctx.host_ctx.set_last_error(format!(
            "migrate_component_column '{}' requires AbiPod components without drop_fn",
            old_entry.name
        ));
        return FfiResult::UNSUPPORTED;
    }

    // SAFETY: `new_desc.validate()` verified `!new_desc.fields.is_null()` when `field_count > 0`.
    let (new_schema_fingerprint, new_fields, new_owned_strings) =
        match unsafe { extract_owned_dynamic_fields(new_desc) } {
            Ok(v) => v,
            Err((err, msg)) => {
                ctx.host_ctx.set_last_error(msg);
                return err;
            }
        };

    // Zero-copy fast path: unchanged non-zero fingerprint + identical layout & storage.
    if new_schema_fingerprint != 0
        && new_schema_fingerprint == old_entry.schema_fingerprint
        && new_desc.size == old_entry.size
        && new_desc.align == old_entry.align
        && new_storage_flags == old_entry.storage_flags
    {
        return FfiResult::OK;
    }

    if let Some(other) = ctx.registry.get_by_uuid(&new_desc.uuid)
        && other.handle != component
    {
        ctx.host_ctx.set_last_error(format!(
            "migrate_component_column '{}' new UUID {:?} collides with existing '{}'",
            old_entry.name, new_desc.uuid, other.name
        ));
        return FfiResult::TYPE_MISMATCH;
    }

    let old_comp_id = old_entry.component_id;
    let old_size = old_entry.size;
    let new_size = new_desc.size;
    let new_align = new_desc.align.max(1);
    let old_is_sparse = old_entry.is_sparse_set();

    let new_comp_id = {
        let world = ctx.world_mut();
        let mut chunks: Vec<MigratedChunk> = Vec::new();

        if old_is_sparse {
            let mut sparse_entities: Vec<Entity> = Vec::new();
            for arch in world.archetypes().iter() {
                if !arch.is_empty() && arch.contains(old_comp_id) {
                    sparse_entities.extend(arch.entities().iter().map(|ae| ae.id()));
                }
            }
            if !sparse_entities.is_empty()
                && let Some(sparse_set) = world.storages().sparse_sets.get(old_comp_id)
            {
                let row_count = sparse_entities.len();
                let Some(total_bytes) = row_count.checked_mul(new_size) else {
                    ctx.host_ctx.set_last_error(
                        "migrate_component_column contiguous buffer byte count overflowed usize",
                    );
                    return FfiResult::CAPACITY_EXCEEDED;
                };
                let Ok(dst_col_layout) = Layout::from_size_align(total_bytes, new_align) else {
                    ctx.host_ctx.set_last_error(
                        "migrate_component_column contiguous buffer layout invalid",
                    );
                    return FfiResult::CAPACITY_EXCEEDED;
                };
                let dst_buf = OwnedMigrationBuffer::new(dst_col_layout);
                let dst_base = if new_size == 0 {
                    NonNull::<u8>::dangling().as_ptr()
                } else {
                    dst_buf.ptr
                };

                for (row, &entity) in sparse_entities.iter().enumerate() {
                    let Some(src_ptr) = sparse_set.get(entity) else {
                        continue;
                    };
                    let src_row = if old_size == 0 {
                        NonNull::<u8>::dangling().as_ptr().cast::<c_void>()
                    } else {
                        src_ptr.as_ptr().cast::<c_void>()
                    };
                    let dst_row = if new_size == 0 {
                        dst_base.cast::<c_void>()
                    } else {
                        // SAFETY: `row < row_count` and `dst_buf` holds `row_count * new_size`
                        // writable `new_align`-aligned bytes.
                        unsafe { dst_base.add(row * new_size).cast::<c_void>() }
                    };
                    // SAFETY: `src_row` and `dst_row` match old/new POD layouts and `migrate_fn`
                    // satisfies `BamRowMigrateFn`'s nounwind contract.
                    let rc = unsafe { migrate_fn(src_row, dst_row, user_data) };
                    if !rc.is_ok() {
                        ctx.host_ctx.set_last_error(format!(
                            "migrate_component_column '{}' migrate_fn failed at sparse row {row} ({rc})",
                            old_entry.name
                        ));
                        return rc;
                    }
                }
                chunks.push(MigratedChunk {
                    entities: sparse_entities,
                    dst_buf,
                });
            }
        } else {
            for table in world.storages().tables.iter() {
                let row_count = table.entity_count() as usize;
                if row_count == 0 {
                    continue;
                }
                let Some(src_col) = table.get_column(old_comp_id) else {
                    continue;
                };
                let Some(total_bytes) = row_count.checked_mul(new_size) else {
                    ctx.host_ctx.set_last_error(
                        "migrate_component_column contiguous buffer byte count overflowed usize",
                    );
                    return FfiResult::CAPACITY_EXCEEDED;
                };
                let Ok(dst_col_layout) = Layout::from_size_align(total_bytes, new_align) else {
                    ctx.host_ctx.set_last_error(
                        "migrate_component_column contiguous buffer layout invalid",
                    );
                    return FfiResult::CAPACITY_EXCEEDED;
                };
                let dst_buf = OwnedMigrationBuffer::new(dst_col_layout);
                let src_base = if old_size == 0 {
                    NonNull::<u8>::dangling().as_ptr()
                } else {
                    // SAFETY: `row_count > 0`, so row `0` is in bounds of `src_col`'s contiguous
                    // column allocation with full column provenance.
                    unsafe {
                        src_col
                            .get_data_unchecked(TableRow::new(NonMaxU32::ZERO))
                            .as_ptr()
                    }
                };
                let dst_base = if new_size == 0 {
                    NonNull::<u8>::dangling().as_ptr()
                } else {
                    dst_buf.ptr
                };

                for row in 0..row_count {
                    let src_row = if old_size == 0 {
                        src_base.cast::<c_void>()
                    } else {
                        // SAFETY: `row < row_count == table.entity_count()` and `src_base` points
                        // to `row_count * old_size` contiguous initialized POD bytes.
                        unsafe { src_base.add(row * old_size).cast::<c_void>() }
                    };
                    let dst_row = if new_size == 0 {
                        dst_base.cast::<c_void>()
                    } else {
                        // SAFETY: `row < row_count` and `dst_buf` holds `row_count * new_size`
                        // writable `new_align`-aligned bytes.
                        unsafe { dst_base.add(row * new_size).cast::<c_void>() }
                    };
                    // SAFETY: `src_row` and `dst_row` match old/new POD layouts and `migrate_fn`
                    // satisfies `BamRowMigrateFn`'s nounwind contract.
                    let rc = unsafe { migrate_fn(src_row, dst_row, user_data) };
                    if !rc.is_ok() {
                        ctx.host_ctx.set_last_error(format!(
                            "migrate_component_column '{}' migrate_fn failed at row {row} ({rc})",
                            old_entry.name
                        ));
                        return rc;
                    }
                }

                chunks.push(MigratedChunk {
                    entities: table.entities().to_vec(),
                    dst_buf,
                });
            }
        }

        // All row transformations succeeded without error. Register the fresh Bevy `ComponentId`
        // and commit migrated component columns in reverse table order (avoiding old-table
        // swap-remove churn and intermediate-table growth).
        let new_storage_type = if new_desc.is_sparse_set() {
            StorageType::SparseSet
        } else {
            StorageType::Table
        };
        // SAFETY: `new_layout` was validated by `new_desc.layout()`; POD component has no drop_fn.
        let bevy_desc = unsafe {
            ComponentDescriptor::new_with_layout(
                old_entry.name.to_string(),
                new_storage_type,
                new_layout,
                None,
                !new_desc.is_immutable(),
                new_desc.is_summary_tick(),
                ComponentCloneBehavior::Default,
                None,
            )
        };
        let new_id = world.register_component_with_descriptor(bevy_desc);

        for chunk in chunks {
            for row in (0..chunk.entities.len()).rev() {
                let entity = chunk.entities[row];
                if let Ok(mut ent_mut) = world.get_entity_mut(entity) {
                    ent_mut.remove_by_id(old_comp_id);
                    let row_ptr = if new_size == 0 {
                        NonNull::<u8>::dangling()
                    } else {
                        // SAFETY: `row < chunk.entities.len()` and `dst_buf.ptr` is allocated
                        // for `chunk.entities.len() * new_size` bytes aligned to `new_align`.
                        unsafe { NonNull::new_unchecked(chunk.dst_buf.ptr.add(row * new_size)) }
                    };
                    // SAFETY: `row_ptr` holds `new_size` initialized POD bytes written by
                    // `migrate_fn`; `insert_by_id` copies out without freeing `chunk.dst_buf`.
                    let owning = unsafe { OwningPtr::new(row_ptr) };
                    // SAFETY: `new_id` was registered in `world` with `(new_size, new_align)`.
                    unsafe {
                        ent_mut.insert_by_id(new_id, owning);
                    }
                }
            }
        }

        new_id
    };

    ctx.record_fresh_dynamic_comp_id(new_comp_id);
    if let Err(err) = ctx.registry.remap_migrated_component(
        component,
        new_comp_id,
        new_desc.uuid,
        new_desc.size,
        new_desc.align,
        new_storage_flags,
        new_schema_fingerprint,
        new_fields,
        new_owned_strings,
        new_desc.vtable_ptr,
        ctx.mod_id,
    ) {
        ctx.host_ctx.set_last_error(err.to_string());
        return FfiResult::TYPE_MISMATCH;
    }

    ctx.host_ctx.set_snapshot(ctx.registry.snapshot_arc());
    if !ctx.world_ptr.is_null() {
        let updated_reg = ctx.registry.clone();
        ctx.world_mut().insert_resource(updated_reg);
    }

    FfiResult::OK
}

fn invoke_drop_if_needed(drop_fn: Option<ComponentDropFn>, dst_ptr: *mut c_void) {
    if let Some(df) = drop_fn {
        // SAFETY: Caller guarantees `dst_ptr` holds an initialized value matching `df` before
        // `OwnedMigrationBuffer` frees the backing memory.
        unsafe {
            df(dst_ptr);
        }
    }
}

fn construct_migrated_buffer(
    entry: &RegisteredComponent,
    init_default_fn: Option<BamInitDefaultFn>,
    import_field_fn: Option<BamImportFieldFn>,
    fields: &[ExportedFieldRecord],
) -> Result<OwnedMigrationBuffer, ModLoadError> {
    let layout = Layout::from_size_align(entry.size, entry.align.max(1)).map_err(|err| {
        ModLoadError::SchemaMigrationFailed {
            component_name: entry.name.to_string(),
            status: FfiResult::INVALID_ARGUMENT,
            detail: format!("invalid migrated component layout: {err}"),
        }
    })?;
    let buf = OwnedMigrationBuffer::new(layout);
    let dst_ptr = if entry.size == 0 {
        NonNull::<u8>::dangling().as_ptr().cast::<c_void>()
    } else {
        buf.ptr.cast::<c_void>()
    };

    if let Some(init_fn) = init_default_fn {
        // SAFETY: `dst_ptr` points to `entry.size` uninitialized bytes aligned to `entry.align`.
        let status = unsafe { init_fn(dst_ptr) };
        if !status.is_ok() {
            return Err(ModLoadError::SchemaMigrationFailed {
                component_name: entry.name.to_string(),
                status,
                detail: "init_default failed during post-reload restoration".to_string(),
            });
        }
    } else if !entry.is_abi_pod {
        return Err(ModLoadError::SchemaMigrationFailed {
            component_name: entry.name.to_string(),
            status: FfiResult::UNSUPPORTED,
            detail: "non-POD dynamic component is missing init_default symbol".to_string(),
        });
    }

    if let Some(import_fn) = import_field_fn {
        for field in fields {
            let slice = if field.bytes.is_empty() {
                FfiSlice::empty()
            } else {
                FfiSlice::from_slice(&field.bytes)
            };
            // SAFETY: `dst_ptr` holds an initialized `T`, `StrView`s borrow valid UTF-8, and
            // `slice` borrows `field.bytes` for this synchronous call.
            let rc = unsafe {
                import_fn(
                    dst_ptr,
                    StrView::from_str(&field.field_name),
                    StrView::from_str(&field.type_tag),
                    slice,
                )
            };
            if rc == FfiResult::OK || rc == FfiResult::NOT_FOUND {
                continue;
            }
            invoke_drop_if_needed(entry.drop_fn, dst_ptr);
            return Err(ModLoadError::SchemaMigrationFailed {
                component_name: entry.name.to_string(),
                status: rc,
                detail: format!(
                    "import_field failed for '{}' ({})",
                    field.field_name, field.type_tag
                ),
            });
        }
    }

    Ok(buf)
}

/// Restores snapshotted dynamic component columns onto their live entities (or singleton
/// resources) on `new_id`'s newly initialized allocator via `init_default` + `import_field`.
pub fn restore_mod_dynamic_columns_after_reload(
    world: &mut World,
    new_id: ModId,
    snapshot: ReloadColumnSnapshot,
) -> Result<(), ModLoadError> {
    if snapshot.columns.is_empty() {
        return Ok(());
    }
    let Some(registry) = world.get_resource::<ModRegistry>().cloned() else {
        return Ok(());
    };

    for col in snapshot.columns {
        let Some(new_entry) = registry.get_by_name(&col.canonical_name).cloned() else {
            // Component type was removed in the reloaded mod version; entities stay alive without it.
            continue;
        };
        if new_entry.owner != Some(new_id) {
            continue;
        }

        let (init_default_fn, import_field_fn) = {
            let Some(mod_host) = world.get_resource::<ModHost>() else {
                return Err(ModLoadError::ModNotFound { id: new_id });
            };
            let Some(loaded) = mod_host.get_mod(new_id) else {
                return Err(ModLoadError::ModNotFound { id: new_id });
            };
            let init_opt =
                resolve_component_symbol(loaded, &registry, &col.canonical_name, "init_default")
                    .map(|ptr| {
                        // SAFETY: Symbol `"bam.v0.<name>.init_default"` matches `BamInitDefaultFn`.
                        unsafe { core::mem::transmute::<*const c_void, BamInitDefaultFn>(ptr) }
                    });
            let import_opt =
                resolve_component_symbol(loaded, &registry, &col.canonical_name, "import_field")
                    .map(|ptr| {
                        // SAFETY: Symbol `"bam.v0.<name>.import_field"` matches `BamImportFieldFn`.
                        unsafe { core::mem::transmute::<*const c_void, BamImportFieldFn>(ptr) }
                    });
            (init_opt, import_opt)
        };

        if col.is_resource && new_entry.is_resource() {
            let Some(first) = col.entities.first() else {
                continue;
            };
            let buf = construct_migrated_buffer(
                &new_entry,
                init_default_fn,
                import_field_fn,
                &first.fields,
            )?;
            let dst_ptr = if new_entry.size == 0 {
                NonNull::<u8>::dangling().as_ptr().cast::<c_void>()
            } else {
                buf.ptr.cast::<c_void>()
            };
            if world.contains_resource_by_id(new_entry.component_id) {
                world.remove_resource_by_id(new_entry.component_id);
            }
            let res_ent = world.spawn(IsResource::new(new_entry.component_id)).id();
            if let Ok(mut ent_mut) = world.get_entity_mut(res_ent) {
                // SAFETY: `OwnedMigrationBuffer::new` returns non-null aligned storage (or dangling ZST).
                let nn = unsafe { NonNull::new_unchecked(dst_ptr.cast::<u8>()) };
                // SAFETY: `nn` holds an initialized value matching `new_entry`; `insert_by_id`
                // moves out of `owning` without invoking `OwnedMigrationBuffer` dealloc on it.
                let owning = unsafe { OwningPtr::new(nn) };
                // SAFETY: `new_entry.component_id` was registered in `world` with `(size, align)`.
                unsafe {
                    ent_mut.insert_by_id(new_entry.component_id, owning);
                }
            } else {
                invoke_drop_if_needed(new_entry.drop_fn, dst_ptr);
            }
            drop(buf);
            continue;
        }

        if col.is_resource != new_entry.is_resource() {
            continue;
        }

        for ent_fields in &col.entities {
            if world.get_entity(ent_fields.entity).is_err() {
                continue;
            }
            let buf = construct_migrated_buffer(
                &new_entry,
                init_default_fn,
                import_field_fn,
                &ent_fields.fields,
            )?;
            let dst_ptr = if new_entry.size == 0 {
                NonNull::<u8>::dangling().as_ptr().cast::<c_void>()
            } else {
                buf.ptr.cast::<c_void>()
            };
            if let Ok(mut ent_mut) = world.get_entity_mut(ent_fields.entity) {
                // SAFETY: `dst_ptr` is non-null and aligned for `new_entry`.
                let nn = unsafe { NonNull::new_unchecked(dst_ptr.cast::<u8>()) };
                // SAFETY: `nn` holds an initialized `T` constructed on `new_id`'s allocator;
                // `insert_by_id` takes ownership and Bevy's `on_insert` hook increments `live_values`.
                let owning = unsafe { OwningPtr::new(nn) };
                // SAFETY: `new_entry.component_id` was registered in `world` with `(size, align)`.
                unsafe {
                    ent_mut.insert_by_id(new_entry.component_id, owning);
                }
            } else {
                invoke_drop_if_needed(new_entry.drop_fn, dst_ptr);
            }
            drop(buf);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drop_trampoline::WorldDropGuard;
    use crate::ffi::register::{LoadContext, host_register_component};
    use crate::loader::mod_host::excise_and_unload_mod_for_reload_in_world;
    use crate::system::ModQuarantine;
    use bevy_abi_mod_macros::ModComponent;
    use bevy_abi_mod_sys::{BamInstance, ComponentDesc, ModComponent};
    use core::ffi::{CStr, c_char};
    use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    #[test]
    fn snapshot_migrate_and_restore_v1_to_v2_under_miri() {
        // Serialize against other global `DROP_SLAB` LIFO/free-count assertions.
        let _serial = crate::drop_trampoline::TEST_DROP_SLAB_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        static V1_DROPS: AtomicUsize = AtomicUsize::new(0);
        static V2_DROPS: AtomicUsize = AtomicUsize::new(0);

        #[derive(Clone, Default)]
        struct DropV1;
        impl Drop for DropV1 {
            fn drop(&mut self) {
                V1_DROPS.fetch_add(1, Ordering::SeqCst);
            }
        }

        #[derive(Clone, Default)]
        struct DropV2;
        impl Drop for DropV2 {
            fn drop(&mut self) {
                V2_DROPS.fetch_add(1, Ordering::SeqCst);
            }
        }

        #[derive(Clone, Default, ModComponent)]
        #[mod_component(name = "sim::UnitState")]
        struct UnitV1 {
            hp: i32,
            tag: String,
            scores: Vec<u32>,
            legacy: u64,
            #[mod_field(skip)]
            _probe: DropV1,
        }

        #[derive(Clone, ModComponent)]
        #[mod_component(name = "sim::UnitState")]
        struct UnitV2 {
            shield: f32,
            scores: Vec<u32>,
            hp: i64,
            tag: String,
            #[mod_field(skip)]
            _probe: DropV2,
        }

        impl Default for UnitV2 {
            fn default() -> Self {
                Self {
                    shield: 12.5,
                    scores: Vec::new(),
                    hp: 0,
                    tag: String::new(),
                    _probe: DropV2,
                }
            }
        }

        unsafe extern "C" fn drop_v1(ptr: *mut c_void) {
            // SAFETY: `ptr` points to a live aligned `UnitV1`.
            unsafe { core::ptr::drop_in_place(ptr.cast::<UnitV1>()) };
        }
        unsafe extern "C" fn drop_v2(ptr: *mut c_void) {
            // SAFETY: `ptr` points to a live aligned `UnitV2`.
            unsafe { core::ptr::drop_in_place(ptr.cast::<UnitV2>()) };
        }
        unsafe extern "C" fn init_v2(dst: *mut c_void) -> FfiResult {
            // SAFETY: `dst` points to writable `UnitV2`-aligned storage.
            unsafe { core::ptr::write(dst.cast::<UnitV2>(), UnitV2::default()) };
            FfiResult::OK
        }
        unsafe extern "C" fn noop_shutdown(_inst: *mut BamInstance) {}
        unsafe extern "C" fn resolve_v1(
            _inst: *const BamInstance,
            name: *const c_char,
        ) -> *const c_void {
            // SAFETY: `name` is a NUL-terminated C string.
            let Ok(s) = (unsafe { CStr::from_ptr(name) }).to_str() else {
                return core::ptr::null();
            };
            UnitV1::resolve_symbol(s).unwrap_or(core::ptr::null())
        }
        unsafe extern "C" fn resolve_v2(
            _inst: *const BamInstance,
            name: *const c_char,
        ) -> *const c_void {
            // SAFETY: `name` is a NUL-terminated C string.
            let Ok(s) = (unsafe { CStr::from_ptr(name) }).to_str() else {
                return core::ptr::null();
            };
            if s == "bam.v0.sim::UnitState.init_default" {
                return init_v2 as *const c_void;
            }
            UnitV2::resolve_symbol(s).unwrap_or(core::ptr::null())
        }

        V1_DROPS.store(0, Ordering::SeqCst);
        V2_DROPS.store(0, Ordering::SeqCst);

        let mut world = World::new();
        world.insert_resource(WorldDropGuard::new());
        world.insert_resource(ModHost::new());
        world.insert_resource(ModRegistry::new());
        world.insert_resource(ModQuarantine::new());

        let old_active = Arc::new(AtomicBool::new(true));
        let old_id = LoadedMod::register_mock_for_test(
            &mut world,
            "sim_mod",
            Arc::clone(&old_active),
            noop_shutdown,
            Some(resolve_v1),
            core::ptr::null_mut(),
        );
        let mut old_handle = BamComponent::INVALID;
        {
            let reg = world.resource::<ModRegistry>().clone();
            let world_ptr = &raw mut world;
            let mut load_ctx =
                LoadContext::with_world_and_mod_id(reg, old_active, world_ptr, Some(old_id));
            let desc = ComponentDesc::new(
                UnitV1::COMPONENT_CSTR.as_ptr(),
                UnitV1::UUID,
                core::mem::size_of::<UnitV1>(),
                core::mem::align_of::<UnitV1>(),
            )
            .with_drop(drop_v1)
            .with_vtable(UnitV1::erased_vtable_ptr());
            let rc_v1 =
                // SAFETY: `load_ctx`, `desc`, and `&mut old_handle` are valid for this test registration.
                unsafe {
                    host_register_component(
                        load_ctx.as_ffi_ptr(),
                        &raw const desc,
                        &raw mut old_handle,
                    )
                };
            assert_eq!(rc_v1, FfiResult::OK);
            world.insert_resource(load_ctx.registry.clone());
            load_ctx.finish_init();
        }

        let comp_v1_id = world
            .resource::<ModRegistry>()
            .get_by_handle(old_handle)
            .unwrap()
            .component_id;

        let e_active = world.spawn(ModScoped(old_id)).id();
        let e_disabled = world.spawn((ModScoped(old_id), Disabled)).id();
        // SAFETY: `comp_v1_id` has exact `UnitV1` layout.
        unsafe {
            OwningPtr::make(
                UnitV1 {
                    hp: -7,
                    tag: String::from("knight"),
                    scores: vec![4, 5, 6],
                    legacy: 999,
                    _probe: DropV1,
                },
                |ptr| {
                    world.entity_mut(e_active).insert_by_id(comp_v1_id, ptr);
                },
            );
            OwningPtr::make(
                UnitV1 {
                    hp: 88,
                    tag: String::from("archer"),
                    scores: vec![1, 2],
                    legacy: 111,
                    _probe: DropV1,
                },
                |ptr| {
                    world.entity_mut(e_disabled).insert_by_id(comp_v1_id, ptr);
                },
            );
        }

        // Snapshot, remove V1 (dropping on V1 allocator), and unload for reload preserving entities.
        let snap =
            snapshot_mod_dynamic_columns_for_reload(&mut world, old_id).expect("snapshot succeeds");
        assert_eq!(V1_DROPS.load(Ordering::SeqCst), 2);
        assert!(world.entities().contains(e_active));
        assert!(world.entities().contains(e_disabled));

        excise_and_unload_mod_for_reload_in_world(&mut world, old_id, |_| {})
            .expect("unload for reload preserves ModScoped entities");
        assert!(world.entities().contains(e_active));
        assert!(world.entities().contains(e_disabled));

        // Load V2 with evolved layout & restore columns + migrate ModScoped.
        let new_active = Arc::new(AtomicBool::new(true));
        let new_id = LoadedMod::register_mock_for_test(
            &mut world,
            "sim_mod",
            Arc::clone(&new_active),
            noop_shutdown,
            Some(resolve_v2),
            core::ptr::null_mut(),
        );
        let mut new_handle = BamComponent::INVALID;
        {
            let reg = world.resource::<ModRegistry>().clone();
            let world_ptr = &raw mut world;
            let mut load_ctx =
                LoadContext::with_world_and_mod_id(reg, new_active, world_ptr, Some(new_id));
            let desc = ComponentDesc::new(
                UnitV2::COMPONENT_CSTR.as_ptr(),
                UnitV2::UUID,
                core::mem::size_of::<UnitV2>(),
                core::mem::align_of::<UnitV2>(),
            )
            .with_drop(drop_v2)
            .with_vtable(UnitV2::erased_vtable_ptr());
            let rc_v2 =
                // SAFETY: `load_ctx`, `desc`, and `&mut new_handle` are valid for this test registration.
                unsafe {
                    host_register_component(
                        load_ctx.as_ffi_ptr(),
                        &raw const desc,
                        &raw mut new_handle,
                    )
                };
            assert_eq!(rc_v2, FfiResult::OK);
            world.insert_resource(load_ctx.registry.clone());
            load_ctx.finish_init();
        }

        restore_mod_dynamic_columns_after_reload(&mut world, new_id, snap)
            .expect("restore V2 columns succeeds");
        migrate_mod_scoped_entities(&mut world, old_id, new_id);

        assert_eq!(world.get::<ModScoped>(e_active), Some(&ModScoped(new_id)));
        assert_eq!(world.get::<ModScoped>(e_disabled), Some(&ModScoped(new_id)));

        let comp_v2_id = world
            .resource::<ModRegistry>()
            .get_by_handle(new_handle)
            .unwrap()
            .component_id;
        // SAFETY: `comp_v2_id` holds live `UnitV2` values constructed by `init_v2` + `import_field`.
        unsafe {
            let p1 = world
                .entity(e_active)
                .get_by_id(comp_v2_id)
                .unwrap()
                .deref::<UnitV2>();
            assert_eq!(p1.hp, -7i64);
            assert!((p1.shield - 12.5).abs() < f32::EPSILON);
            assert_eq!(p1.tag, "knight");
            assert_eq!(p1.scores, vec![4, 5, 6]);

            let p2 = world
                .entity(e_disabled)
                .get_by_id(comp_v2_id)
                .unwrap()
                .deref::<UnitV2>();
            assert_eq!(p2.hp, 88i64);
            assert!((p2.shield - 12.5).abs() < f32::EPSILON);
            assert_eq!(p2.tag, "archer");
            assert_eq!(p2.scores, vec![1, 2]);
        }

        // Permanent unload despawns both ModScoped(new_id) entities (including Disabled) and drops V2!
        crate::loader::mod_host::excise_and_unload_mod_in_world(&mut world, new_id, |_| {})
            .expect("permanent unload succeeds");
        assert_eq!(V2_DROPS.load(Ordering::SeqCst), 2);
        assert!(!world.entities().contains(e_active));
        assert!(!world.entities().contains(e_disabled));
    }

    #[test]
    fn zero_copy_fingerprint_retention_reload_under_miri() {
        static EXPORT_CALLS: AtomicUsize = AtomicUsize::new(0);

        #[repr(C)]
        #[derive(Clone, Copy, Debug, PartialEq)]
        struct FastPod {
            x: i32,
            y: i32,
        }

        const FAST_UUID: ComponentUuid = ComponentUuid::from_parts("sim::FastPod", 8, 4, 0);
        const FAST_FP: u64 = 0x51A0_F1A6_0000_0001;
        const CHANGED_FP: u64 = 0x51A0_F1A6_0000_0002;

        unsafe extern "C" fn forbidden_export_fields(
            _src: *const c_void,
            _sink: Option<BamExportFieldsSinkFn>,
            _sink_ctx: *mut c_void,
        ) -> FfiResult {
            EXPORT_CALLS.fetch_add(1, Ordering::SeqCst);
            FfiResult::OK
        }
        unsafe extern "C" fn noop_shutdown(_inst: *mut BamInstance) {}
        unsafe extern "C" fn resolve_sym(
            _inst: *const BamInstance,
            name: *const c_char,
        ) -> *const c_void {
            // SAFETY: `name` is a valid NUL-terminated C string.
            let Ok(s) = (unsafe { CStr::from_ptr(name) }).to_str() else {
                return core::ptr::null();
            };
            if s == "bam.v0.sim::FastPod.export_fields" {
                return forbidden_export_fields as *const c_void;
            }
            core::ptr::null()
        }

        EXPORT_CALLS.store(0, Ordering::SeqCst);

        let mut world = World::new();
        world.insert_resource(WorldDropGuard::new());
        world.insert_resource(ModHost::new());
        world.insert_resource(ModRegistry::new());
        world.insert_resource(ModQuarantine::new());

        let old_active = Arc::new(AtomicBool::new(true));
        let old_id = LoadedMod::register_mock_for_test(
            &mut world,
            "fast_pod_mod",
            Arc::clone(&old_active),
            noop_shutdown,
            Some(resolve_sym),
            core::ptr::null_mut(),
        );

        let mut old_handle = BamComponent::INVALID;
        {
            let reg = world.resource::<ModRegistry>().clone();
            let world_ptr = &raw mut world;
            let mut load_ctx =
                LoadContext::with_world_and_mod_id(reg, old_active, world_ptr, Some(old_id));
            let desc = ComponentDesc::new(
                c"sim::FastPod".as_ptr(),
                FAST_UUID,
                core::mem::size_of::<FastPod>(),
                core::mem::align_of::<FastPod>(),
            )
            .with_schema_fingerprint(FAST_FP);
            // SAFETY: `load_ctx`, `desc`, and `&mut old_handle` are valid for this call.
            let rc = unsafe {
                host_register_component(load_ctx.as_ffi_ptr(), &raw const desc, &raw mut old_handle)
            };
            assert_eq!(rc, FfiResult::OK);
            world.insert_resource(load_ctx.registry.clone());
            load_ctx.finish_init();
        }

        let old_comp_id = world
            .resource::<ModRegistry>()
            .get_by_handle(old_handle)
            .unwrap()
            .component_id;

        let e_active = world.spawn(ModScoped(old_id)).id();
        let e_disabled = world.spawn((ModScoped(old_id), Disabled)).id();
        let e_persistent = world.spawn_empty().id();
        // SAFETY: `old_comp_id` has exact `FastPod` layout.
        unsafe {
            OwningPtr::make(FastPod { x: 10, y: 20 }, |p| {
                world.entity_mut(e_active).insert_by_id(old_comp_id, p);
            });
            OwningPtr::make(FastPod { x: -30, y: 40 }, |p| {
                world.entity_mut(e_disabled).insert_by_id(old_comp_id, p);
            });
            OwningPtr::make(FastPod { x: 77, y: 88 }, |p| {
                world.entity_mut(e_persistent).insert_by_id(old_comp_id, p);
            });
        }

        // Capture raw column data pointer for `e_active`'s table prior to reload.
        let before_table_id = world.entity(e_active).location().table_id;
        let before_col_ptr = {
            let table = world.storages().tables.get(before_table_id).unwrap();
            let col = table.get_column(old_comp_id).unwrap();
            // SAFETY: Table has at least 1 entity so row 0 is in bounds.
            unsafe {
                col.get_data_unchecked(TableRow::new(NonMaxU32::ZERO))
                    .as_ptr() as usize
            }
        };

        // Snapshot: must retain zero-copy without invoking `export_fields` or copying any bytes.
        let snap =
            snapshot_mod_dynamic_columns_for_reload(&mut world, old_id).expect("snapshot succeeds");
        assert_eq!(snap.bytes_copied, 0);
        assert!(snap.columns.is_empty());
        assert_eq!(snap.retained_zero_copy.len(), 1);
        assert_eq!(snap.retained_zero_copy[0].component_id, old_comp_id);
        assert_eq!(snap.retained_zero_copy[0].schema_fingerprint, FAST_FP);
        assert_eq!(EXPORT_CALLS.load(Ordering::SeqCst), 0);

        excise_and_unload_mod_for_reload_in_world(&mut world, old_id, |_| {})
            .expect("unload for reload succeeds");

        let new_active = Arc::new(AtomicBool::new(true));
        let new_id = LoadedMod::register_mock_for_test(
            &mut world,
            "fast_pod_mod",
            Arc::clone(&new_active),
            noop_shutdown,
            Some(resolve_sym),
            core::ptr::null_mut(),
        );

        let mut new_handle = BamComponent::INVALID;
        {
            let reg = world.resource::<ModRegistry>().clone();
            let world_ptr = &raw mut world;
            let mut load_ctx =
                LoadContext::with_world_and_mod_id(reg, new_active, world_ptr, Some(new_id));

            // Mismatched non-zero fingerprint must be rejected with TYPE_MISMATCH.
            let bad_desc = ComponentDesc::new(
                c"sim::FastPod".as_ptr(),
                FAST_UUID,
                core::mem::size_of::<FastPod>(),
                core::mem::align_of::<FastPod>(),
            )
            .with_schema_fingerprint(CHANGED_FP);
            let mut bad_handle = BamComponent::INVALID;
            // SAFETY: Valid test pointers.
            let rc_bad = unsafe {
                host_register_component(
                    load_ctx.as_ffi_ptr(),
                    &raw const bad_desc,
                    &raw mut bad_handle,
                )
            };
            assert_eq!(rc_bad, FfiResult::TYPE_MISMATCH);

            // Matching fingerprint retains exact handle, ComponentId, and BlobVec storage.
            let good_desc = ComponentDesc::new(
                c"sim::FastPod".as_ptr(),
                FAST_UUID,
                core::mem::size_of::<FastPod>(),
                core::mem::align_of::<FastPod>(),
            )
            .with_schema_fingerprint(FAST_FP);
            // SAFETY: Valid test pointers.
            let rc_good = unsafe {
                host_register_component(
                    load_ctx.as_ffi_ptr(),
                    &raw const good_desc,
                    &raw mut new_handle,
                )
            };
            assert_eq!(rc_good, FfiResult::OK);
            assert_eq!(new_handle, old_handle);
            world.insert_resource(load_ctx.registry.clone());
            load_ctx.finish_init();
        }

        restore_mod_dynamic_columns_after_reload(&mut world, new_id, snap)
            .expect("restore succeeds");
        migrate_mod_scoped_entities(&mut world, old_id, new_id);

        let retained_entry = world
            .resource::<ModRegistry>()
            .get_by_handle(new_handle)
            .unwrap();
        assert_eq!(retained_entry.component_id, old_comp_id);
        assert_eq!(retained_entry.schema_fingerprint, FAST_FP);
        assert_eq!(retained_entry.owner, Some(new_id));

        // Exact same Bevy BlobVec backing allocation pointer — 0 bytes copied!
        let after_table_id = world.entity(e_active).location().table_id;
        assert_eq!(after_table_id, before_table_id);
        let after_col_ptr = {
            let table = world.storages().tables.get(after_table_id).unwrap();
            let col = table.get_column(old_comp_id).unwrap();
            // SAFETY: Table still holds `e_active` at row 0.
            unsafe {
                col.get_data_unchecked(TableRow::new(NonMaxU32::ZERO))
                    .as_ptr() as usize
            }
        };
        assert_eq!(after_col_ptr, before_col_ptr);
        assert_eq!(EXPORT_CALLS.load(Ordering::SeqCst), 0);

        // SAFETY: `old_comp_id` holds live `FastPod` values.
        unsafe {
            assert_eq!(
                *world
                    .entity(e_active)
                    .get_by_id(old_comp_id)
                    .unwrap()
                    .deref::<FastPod>(),
                FastPod { x: 10, y: 20 }
            );
            assert_eq!(
                *world
                    .entity(e_disabled)
                    .get_by_id(old_comp_id)
                    .unwrap()
                    .deref::<FastPod>(),
                FastPod { x: -30, y: 40 }
            );
            assert_eq!(
                *world
                    .entity(e_persistent)
                    .get_by_id(old_comp_id)
                    .unwrap()
                    .deref::<FastPod>(),
                FastPod { x: 77, y: 88 }
            );
        }
        assert_eq!(world.get::<ModScoped>(e_active), Some(&ModScoped(new_id)));
        assert_eq!(world.get::<ModScoped>(e_disabled), Some(&ModScoped(new_id)));
    }

    #[test]
    fn migrate_component_column_8b_to_16b_dense_sparse_and_error_rollback_under_miri() {
        use crate::ffi::v03_ext::host_migrate_component_column;
        use bevy_abi_mod_sys::{BAM_COMPONENT_FLAG_SPARSE_SET, BamFieldDesc, BamFieldKind};

        #[repr(C)]
        #[derive(Clone, Copy, Debug, PartialEq)]
        struct PosV1 {
            x: f32,
            y: f32,
        }

        #[repr(C)]
        #[derive(Clone, Copy, Debug, PartialEq)]
        struct PosV2 {
            x: f64,
            y: f64,
        }

        const UUID_V1: ComponentUuid = ComponentUuid::from_parts("sim::PosMig", 8, 4, 1);
        const UUID_V2: ComponentUuid = ComponentUuid::from_parts("sim::PosMig", 16, 8, 2);
        const FP_V1: u64 = 0x1111_2222_3333_4444;
        const FP_V2: u64 = 0x5555_6666_7777_8888;
        const V2_FIELDS: [BamFieldDesc; 2] = [
            BamFieldDesc::new(
                BamFieldKind::F64,
                StrView::from_str("x"),
                StrView::from_str("f64"),
                0,
                8,
                8,
            ),
            BamFieldDesc::new(
                BamFieldKind::F64,
                StrView::from_str("y"),
                StrView::from_str("f64"),
                8,
                8,
                8,
            ),
        ];

        unsafe extern "C" fn widen_pos(
            src_row: *const c_void,
            dst_row: *mut c_void,
            user_data: *mut c_void,
        ) -> FfiResult {
            // SAFETY: `src_row` points to initialized `PosV1`; `dst_row` is aligned `PosV2` storage.
            let old = unsafe { core::ptr::read(src_row.cast::<PosV1>()) };
            let bias = if user_data.is_null() {
                0.0
            } else {
                // SAFETY: `user_data` points to an `f64` stack local when non-null.
                unsafe { *user_data.cast::<f64>() }
            };
            // SAFETY: `dst_row` is valid writable `PosV2` storage.
            unsafe {
                core::ptr::write(
                    dst_row.cast::<PosV2>(),
                    PosV2 {
                        x: f64::from(old.x) + bias,
                        y: f64::from(old.y) - bias,
                    },
                );
            }
            FfiResult::OK
        }

        unsafe extern "C" fn failing_migrate(
            src_row: *const c_void,
            dst_row: *mut c_void,
            counter: *mut c_void,
        ) -> FfiResult {
            // SAFETY: `counter` points to an `AtomicUsize` for this synchronous test call.
            let seen = unsafe { &*counter.cast::<AtomicUsize>() }.fetch_add(1, Ordering::SeqCst);
            if seen == 2 {
                return FfiResult::CAPACITY_EXCEEDED;
            }
            // SAFETY: `widen_pos` handles valid `PosV1 -> PosV2` pointers with null user_data.
            unsafe { widen_pos(src_row, dst_row, core::ptr::null_mut()) }
        }

        let mut world = World::new();
        world.insert_resource(WorldDropGuard::new());
        world.insert_resource(ModRegistry::new());

        let active = Arc::new(AtomicBool::new(true));
        let mut dense_handle = BamComponent::INVALID;
        let mut sparse_handle = BamComponent::INVALID;
        {
            let reg = world.resource::<ModRegistry>().clone();
            let world_ptr = &raw mut world;
            let mut load_ctx =
                LoadContext::with_world_and_mod_id(reg, Arc::clone(&active), world_ptr, None);

            let dense_desc = ComponentDesc::new(
                c"sim::PosMig".as_ptr(),
                UUID_V1,
                core::mem::size_of::<PosV1>(),
                core::mem::align_of::<PosV1>(),
            )
            .with_schema_fingerprint(FP_V1);
            assert_eq!(
                // SAFETY: Valid test pointers.
                unsafe {
                    host_register_component(
                        load_ctx.as_ffi_ptr(),
                        &raw const dense_desc,
                        &raw mut dense_handle,
                    )
                },
                FfiResult::OK
            );

            let sparse_desc = ComponentDesc::new(
                c"sim::SparsePosMig".as_ptr(),
                ComponentUuid::from_parts("sim::SparsePosMig", 8, 4, 1),
                core::mem::size_of::<PosV1>(),
                core::mem::align_of::<PosV1>(),
            )
            .with_flags(BAM_COMPONENT_FLAG_SPARSE_SET)
            .with_schema_fingerprint(FP_V1);
            assert_eq!(
                // SAFETY: Valid test pointers.
                unsafe {
                    host_register_component(
                        load_ctx.as_ffi_ptr(),
                        &raw const sparse_desc,
                        &raw mut sparse_handle,
                    )
                },
                FfiResult::OK
            );

            world.insert_resource(load_ctx.registry.clone());
            load_ctx.finish_init();
        }

        let old_dense_id = world
            .resource::<ModRegistry>()
            .get_by_handle(dense_handle)
            .unwrap()
            .component_id;
        let old_sparse_id = world
            .resource::<ModRegistry>()
            .get_by_handle(sparse_handle)
            .unwrap()
            .component_id;

        let mut dense_entities = Vec::new();
        for i in 0..4u32 {
            let e = if i == 3 {
                world.spawn(Disabled).id()
            } else {
                world.spawn_empty().id()
            };
            // SAFETY: `old_dense_id` has exact `PosV1` layout.
            unsafe {
                OwningPtr::make(
                    PosV1 {
                        x: i as f32 * 2.0,
                        y: i as f32 * -3.0,
                    },
                    |p| {
                        world.entity_mut(e).insert_by_id(old_dense_id, p);
                    },
                );
            }
            dense_entities.push(e);
        }

        let mut sparse_entities = Vec::new();
        for i in 0..3u32 {
            let e = world.spawn_empty().id();
            // SAFETY: `old_sparse_id` has exact `PosV1` layout.
            unsafe {
                OwningPtr::make(
                    PosV1 {
                        x: 10.0 + i as f32,
                        y: 20.0 + i as f32,
                    },
                    |p| {
                        world.entity_mut(e).insert_by_id(old_sparse_id, p);
                    },
                );
            }
            sparse_entities.push(e);
        }

        let v2_dense_desc = ComponentDesc::new(
            c"sim::PosMig".as_ptr(),
            UUID_V2,
            core::mem::size_of::<PosV2>(),
            core::mem::align_of::<PosV2>(),
        )
        .with_schema_fingerprint(FP_V2)
        .with_fields(V2_FIELDS.as_ptr(), V2_FIELDS.len());

        let v1_same_desc = ComponentDesc::new(
            c"sim::PosMig".as_ptr(),
            UUID_V1,
            core::mem::size_of::<PosV1>(),
            core::mem::align_of::<PosV1>(),
        )
        .with_schema_fingerprint(FP_V1);

        {
            let reg = world.resource::<ModRegistry>().clone();
            let world_ptr = &raw mut world;
            let mut load_ctx =
                LoadContext::with_world_and_mod_id(reg, Arc::clone(&active), world_ptr, None);

            // 1. Mismatched `old_fingerprint` -> TYPE_MISMATCH.
            assert_eq!(
                // SAFETY: Valid test pointers.
                unsafe {
                    host_migrate_component_column(
                        load_ctx.as_ffi_ptr(),
                        dense_handle,
                        0xDEAD_BEEF,
                        &raw const v2_dense_desc,
                        Some(widen_pos),
                        core::ptr::null_mut(),
                    )
                },
                FfiResult::TYPE_MISMATCH
            );

            // 2. Unchanged non-zero fingerprint fast-path -> OK with 0 migrate_fn calls.
            let fail_counter = AtomicUsize::new(0);
            assert_eq!(
                // SAFETY: Valid test pointers; fast-path must not invoke `failing_migrate`.
                unsafe {
                    host_migrate_component_column(
                        load_ctx.as_ffi_ptr(),
                        dense_handle,
                        FP_V1,
                        &raw const v1_same_desc,
                        Some(failing_migrate),
                        (&raw const fail_counter).cast_mut().cast::<c_void>(),
                    )
                },
                FfiResult::OK
            );
            assert_eq!(fail_counter.load(Ordering::SeqCst), 0);

            // 3. Error mid-table aborts atomically without modifying registry or entity data.
            assert_eq!(
                // SAFETY: Valid test pointers.
                unsafe {
                    host_migrate_component_column(
                        load_ctx.as_ffi_ptr(),
                        dense_handle,
                        FP_V1,
                        &raw const v2_dense_desc,
                        Some(failing_migrate),
                        (&raw const fail_counter).cast_mut().cast::<c_void>(),
                    )
                },
                FfiResult::CAPACITY_EXCEEDED
            );
            assert_eq!(
                load_ctx
                    .registry
                    .get_by_handle(dense_handle)
                    .unwrap()
                    .component_id,
                old_dense_id
            );

            // 4. Successful dense 8B -> 16B migration across active + Disabled tables.
            let mut bias = 0.5_f64;
            assert_eq!(
                // SAFETY: Valid test pointers; `bias` lives for the call.
                unsafe {
                    host_migrate_component_column(
                        load_ctx.as_ffi_ptr(),
                        dense_handle,
                        FP_V1,
                        &raw const v2_dense_desc,
                        Some(widen_pos),
                        (&raw mut bias).cast::<c_void>(),
                    )
                },
                FfiResult::OK
            );

            // 5. SparseSet 8B -> Table 16B migration.
            let sparse_to_table_desc = ComponentDesc::new(
                c"sim::SparsePosMig".as_ptr(),
                ComponentUuid::from_parts("sim::SparsePosMig", 16, 8, 2),
                core::mem::size_of::<PosV2>(),
                core::mem::align_of::<PosV2>(),
            )
            .with_schema_fingerprint(FP_V2);
            assert_eq!(
                // SAFETY: Valid test pointers.
                unsafe {
                    host_migrate_component_column(
                        load_ctx.as_ffi_ptr(),
                        sparse_handle,
                        FP_V1,
                        &raw const sparse_to_table_desc,
                        Some(widen_pos),
                        core::ptr::null_mut(),
                    )
                },
                FfiResult::OK
            );

            load_ctx.finish_init();
        }

        let reg = world.resource::<ModRegistry>();
        let new_dense_entry = reg.get_by_handle(dense_handle).unwrap().clone();
        assert_ne!(new_dense_entry.component_id, old_dense_id);
        assert_eq!(new_dense_entry.size, 16);
        assert_eq!(new_dense_entry.align, 8);
        assert_eq!(new_dense_entry.schema_fingerprint, FP_V2);
        assert_eq!(new_dense_entry.uuid, UUID_V2);
        assert_eq!(new_dense_entry.fields.len(), 2);
        assert_eq!(reg.get_by_uuid(&UUID_V2).unwrap().handle, dense_handle);
        assert!(reg.get_by_uuid(&UUID_V1).is_none());

        for (i, &e) in dense_entities.iter().enumerate() {
            assert!(world.entity(e).get_by_id(old_dense_id).is_err());
            // SAFETY: `new_dense_entry.component_id` holds initialized `PosV2` values.
            let val = unsafe {
                *world
                    .entity(e)
                    .get_by_id(new_dense_entry.component_id)
                    .unwrap()
                    .deref::<PosV2>()
            };
            assert_eq!(
                val,
                PosV2 {
                    x: (i as f64) * 2.0 + 0.5,
                    y: (i as f64) * -3.0 - 0.5,
                }
            );
        }

        let new_sparse_entry = reg.get_by_handle(sparse_handle).unwrap().clone();
        assert_ne!(new_sparse_entry.component_id, old_sparse_id);
        assert!(!new_sparse_entry.is_sparse_set());
        assert_eq!(new_sparse_entry.size, 16);
        for (i, &e) in sparse_entities.iter().enumerate() {
            assert!(world.entity(e).get_by_id(old_sparse_id).is_err());
            // SAFETY: `new_sparse_entry.component_id` holds initialized `PosV2` values.
            let val = unsafe {
                *world
                    .entity(e)
                    .get_by_id(new_sparse_entry.component_id)
                    .unwrap()
                    .deref::<PosV2>()
            };
            assert_eq!(
                val,
                PosV2 {
                    x: 10.0 + i as f64,
                    y: 20.0 + i as f64,
                }
            );
        }
    }
}
