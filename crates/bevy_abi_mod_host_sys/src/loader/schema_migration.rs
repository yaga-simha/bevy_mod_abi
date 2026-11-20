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
use crate::ffi::validation::{
    BamExportFieldsFn, BamImportFieldFn, BamInitDefaultFn,
    collect_witness_export_sink as collect_exported_field_sink,
};
use crate::handle_header::{AlignedBuf, validate_non_empty_cstr};
use crate::loader::error::ModLoadError;
use crate::loader::mod_host::{LoadedMod, ModHost, ModId, ModScoped};
use crate::loader::symbols::ModNamedObjects;
use crate::registry::{
    MigratedComponentSpec, ModRegistry, RegisteredComponent, component_flags_to_storage_flags,
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
use core::ffi::c_void;
use core::ptr::NonNull;
use nonmax::NonMaxU32;
use std::sync::Arc;

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
    dst_buf: AlignedBuf,
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
    if let Err(err) = unsafe { validate_non_empty_cstr(new_desc.name) } {
        ctx.host_ctx
            .set_last_error("migrate_component_column new_desc `name` is empty or invalid UTF-8");
        return err;
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
                let dst_buf = AlignedBuf::from_layout_zeroed(dst_col_layout);
                let dst_base = dst_buf.as_mut_ptr();

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
                let dst_buf = AlignedBuf::from_layout_zeroed(dst_col_layout);
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
                let dst_base = dst_buf.as_mut_ptr();

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
                        chunk.dst_buf.non_null()
                    } else {
                        // SAFETY: `row < chunk.entities.len()` and `dst_buf` is allocated
                        // for `chunk.entities.len() * new_size` bytes aligned to `new_align`.
                        unsafe {
                            NonNull::new_unchecked(chunk.dst_buf.as_mut_ptr().add(row * new_size))
                        }
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
    if let Err(err) = ctx
        .registry
        .remap_migrated_component(MigratedComponentSpec {
            handle: component,
            new_comp_id,
            new_uuid: new_desc.uuid,
            new_size: new_desc.size,
            new_align: new_desc.align,
            new_storage_flags,
            new_schema_fingerprint,
            new_fields,
            new_owned_field_strings: new_owned_strings,
            new_vtable_ptr: new_desc.vtable_ptr,
            owner: ctx.mod_id,
        })
    {
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
        // `AlignedBuf` frees the backing memory.
        unsafe {
            df(dst_ptr);
        }
    }
}

fn insert_migrated_buf_into_entity(
    world: &mut World,
    entity: Entity,
    entry: &RegisteredComponent,
    buf: AlignedBuf,
) {
    if let Ok(mut ent_mut) = world.get_entity_mut(entity) {
        // SAFETY: `buf.non_null()` holds an initialized `T` constructed on `new_id`'s allocator;
        // `insert_by_id` moves out of `owning` without invoking `AlignedBuf` dealloc on it.
        let owning = unsafe { OwningPtr::new(buf.non_null()) };
        // SAFETY: `entry.component_id` was registered in `world` with `(size, align)`.
        unsafe {
            ent_mut.insert_by_id(entry.component_id, owning);
        }
    } else {
        invoke_drop_if_needed(entry.drop_fn, buf.as_mut_c_void());
    }
    drop(buf);
}

fn construct_migrated_buffer(
    entry: &RegisteredComponent,
    init_default_fn: Option<BamInitDefaultFn>,
    import_field_fn: Option<BamImportFieldFn>,
    fields: &[ExportedFieldRecord],
) -> Result<AlignedBuf, ModLoadError> {
    let buf = AlignedBuf::new_zeroed(entry.size, entry.align).map_err(|status| {
        ModLoadError::SchemaMigrationFailed {
            component_name: entry.name.to_string(),
            status,
            detail: "invalid migrated component layout".to_string(),
        }
    })?;
    let dst_ptr = buf.as_mut_c_void();

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
            if world.contains_resource_by_id(new_entry.component_id) {
                world.remove_resource_by_id(new_entry.component_id);
            }
            let res_ent = world.spawn(IsResource::new(new_entry.component_id)).id();
            insert_migrated_buf_into_entity(world, res_ent, &new_entry, buf);
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
            insert_migrated_buf_into_entity(world, ent_fields.entity, &new_entry, buf);
        }
    }

    Ok(())
}

#[cfg(test)]
#[path = "../../tests/unit/schema_migration_tests.rs"]
mod tests;
