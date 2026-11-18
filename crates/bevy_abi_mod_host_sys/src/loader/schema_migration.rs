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

use crate::loader::error::ModLoadError;
use crate::loader::mod_host::{LoadedMod, ModHost, ModId, ModScoped};
use crate::registry::{ModRegistry, RegisteredComponent};
use crate::sync_ptr::SymbolAddress;
use bevy_abi_mod_sys::{
    BamComponent, ComponentDropFn, ComponentUuid, FfiResult, FfiSlice, StrView,
};
use bevy_ecs::component::ComponentId;
use bevy_ecs::entity::Entity;
use bevy_ecs::entity_disabling::Disabled;
use bevy_ecs::ptr::OwningPtr;
use bevy_ecs::query::{Allow, FilteredAccess, QueryBuilder, Without};
use bevy_ecs::resource::IsResource;
use bevy_ecs::world::World;
use core::alloc::Layout;
use core::any::TypeId;
use core::ffi::c_void;
use core::ptr::NonNull;
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

/// Complete pre-unload dynamic component snapshot produced for one reloading [`ModId`].
#[derive(Clone, Debug, Default)]
pub struct ReloadColumnSnapshot {
    /// Snapshotted component columns in deterministic registry order.
    pub columns: Vec<SnapshotComponentColumn>,
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

    let export_fns: Vec<Option<BamExportFieldsFn>> = {
        let Some(mod_host) = world.get_resource::<ModHost>() else {
            return Err(ModLoadError::ModNotFound { id: mod_id });
        };
        let Some(loaded) = mod_host.get_mod(mod_id) else {
            return Err(ModLoadError::ModNotFound { id: mod_id });
        };
        owned_entries
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

    for (entry, export_fn_opt) in owned_entries.into_iter().zip(export_fns) {
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
                let mut fields = Vec::new();
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
            let mut fields = Vec::new();
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

    Ok(ReloadColumnSnapshot { columns })
}

/// Reassigns all [`ModScoped(old_id)`](ModScoped) entities (including `Disabled` entities) in
/// `world` to [`ModScoped(new_id)`](ModScoped) after a successful reload.
pub fn migrate_mod_scoped_entities(world: &mut World, old_id: ModId, new_id: ModId) {
    let Some(scoped_id) = world.components().get_id(TypeId::of::<ModScoped>()) else {
        return;
    };
    let dis_id_opt = world.components().get_id(TypeId::of::<Disabled>());
    let mut builder = QueryBuilder::<Entity>::new(world);
    builder.with_id(scoped_id);
    if let Some(dis_id) = dis_id_opt {
        let mut acc = FilteredAccess::default();
        acc.access_mut().add_archetypal(dis_id);
        builder.extend_access(acc);
    }
    let candidates: Vec<Entity> = builder.build().iter(world).collect();
    for entity in candidates {
        if let Some(mut scoped) = world.get_mut::<ModScoped>(entity)
            && scoped.0 == old_id
        {
            scoped.0 = new_id;
        }
    }
}

/// Bulk-despawns any remaining [`ModScoped(old_id)`](ModScoped) entities if a reload fails
/// permanently after `old_id` was already unloaded.
pub fn cleanup_orphaned_mod_scoped_entities(world: &mut World, old_id: ModId) {
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
}
