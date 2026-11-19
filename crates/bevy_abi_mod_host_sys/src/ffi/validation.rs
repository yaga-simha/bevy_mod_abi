//! Safe host-side bridge helpers supporting `bridge-reflect`, `bridge-bsn`, and
//! `sub-app-extract` so `#![forbid(unsafe_code)]` crates (`bevy_abi_mod_host` /
//! `bevy_abi_mod`) never require `unsafe` blocks.
//!
//! Provides:
//! 1. **Witness & Declarative `BamFieldDesc` Introspection & Live World Read/Write**:
//!    - [`inspect_component_witness_fields`] / [`inspect_component_witness_fields_in_registry`]
//!    - [`inspect_component_bam_fields`] / [`inspect_component_bam_fields_in_registry`]
//!    - [`export_entity_component_fields`], [`export_resource_fields`]
//!    - [`apply_entity_component_fields`], [`apply_resource_fields`]
//!    - [`insert_entity_component_from_fields`], [`insert_or_apply_entity_component_fields`]
//!    - [`read_entity_pod_bytes`], [`write_entity_pod_bytes`]
//!    - [`read_entity_bam_field_bytes`], [`write_entity_bam_field_bytes`],
//!      [`insert_entity_pod_with_field_patches`], [`read_resource_bam_field_bytes`],
//!      [`write_resource_bam_field_bytes`]
//!    - Optional `bridge-reflect` helpers: `reflect_entity_component`,
//!      `reflect_entity_component_mut`, `reflect_resource_component`,
//!      `reflect_resource_component_mut`, `register_bam_field_reflect_type`
//! 2. **Cross-`World` (`SubApp`) Component & Resource Extraction**:
//!    - [`ensure_component_in_target_world`]
//!    - [`extract_component_between_worlds`]
//!    - [`extract_resource_between_worlds`]

use core::{alloc::Layout, ffi::c_void, mem::MaybeUninit, ptr::NonNull};

use bevy_abi_mod_sys::{
    BamComponent, BamFieldKind, ComponentCloneFn, ComponentDropFn, FfiBool, FfiResult, FfiSlice,
    StrView,
};
#[cfg(feature = "bridge-reflect")]
use bevy_ecs::{
    change_detection::Mut,
    component::ComponentInfo,
    reflect::{AppTypeRegistry, ReflectComponent, ReflectComponentFns, ReflectResource},
    world::{EntityMut, EntityRef, EntityWorldMut, FilteredEntityMut, FilteredEntityRef},
};
use bevy_ecs::{
    component::{ComponentCloneBehavior, ComponentDescriptor, ComponentId, StorageType},
    entity::Entity,
    ptr::OwningPtr,
    resource::IsResource,
    world::World,
};
use bevy_platform::collections::HashSet;
#[cfg(feature = "bridge-reflect")]
use bevy_reflect::{Reflect, ReflectFromPtr};

use crate::{
    ffi::register::DynamicComponentHookMap,
    loader::{ExportedFieldRecord, ModHost},
    registry::{ModRegistry, RegisteredComponent},
};

type BamFieldInfoFn = unsafe extern "C" fn(
    index: usize,
    out_name: *mut StrView,
    out_type_tag: *mut StrView,
    out_offset: *mut usize,
) -> FfiBool;
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

/// Metadata for a single non-skipped field reported by `"bam.v0.<name>.field_info"`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WitnessFieldDescriptor {
    /// Zero-based witness field ordinal.
    pub index: usize,
    /// Field name (or variant-qualified name for tagged enums).
    pub name: String,
    /// Canonical `ModComponent` witness `type_tag` (e.g. `"f32"`, `"bool"`, `"String"`, `"Vec<u32>"`).
    pub type_tag: String,
    /// Byte offset within the enclosing struct (`0` for enum descriptors).
    pub offset: usize,
}

struct AlignedScratchBuffer {
    ptr: *mut u8,
    layout: Layout,
}

impl AlignedScratchBuffer {
    fn new_zeroed(size: usize, align: usize) -> Result<Self, FfiResult> {
        let layout =
            Layout::from_size_align(size, align.max(1)).map_err(|_| FfiResult::INVALID_ARGUMENT)?;
        if layout.size() == 0 {
            return Ok(Self {
                ptr: NonNull::<u8>::dangling().as_ptr(),
                layout,
            });
        }
        // SAFETY: `layout.size() > 0` and `layout.align()` is a valid power of two.
        let ptr = unsafe { std::alloc::alloc_zeroed(layout) };
        if ptr.is_null() {
            std::alloc::handle_alloc_error(layout);
        }
        Ok(Self { ptr, layout })
    }

    #[inline]
    fn as_mut_c_void(&self) -> *mut c_void {
        if self.layout.size() == 0 {
            NonNull::<u8>::dangling().as_ptr().cast::<c_void>()
        } else {
            self.ptr.cast::<c_void>()
        }
    }

    #[inline]
    fn non_null_u8(&self) -> NonNull<u8> {
        if self.layout.size() == 0 {
            NonNull::<u8>::dangling()
        } else {
            // SAFETY: `self.ptr` was checked non-null on allocation.
            unsafe { NonNull::new_unchecked(self.ptr) }
        }
    }
}

impl Drop for AlignedScratchBuffer {
    fn drop(&mut self) {
        if self.layout.size() > 0 {
            // SAFETY: `self.ptr` was allocated with `std::alloc::alloc_zeroed(self.layout)`.
            unsafe {
                std::alloc::dealloc(self.ptr, self.layout);
            }
        }
    }
}

fn resolve_witness_symbol_in_world(
    world: Option<&World>,
    registry: &ModRegistry,
    entry: &RegisteredComponent,
    suffix: &str,
) -> Option<*const c_void> {
    let exact = format!("bam.v0.{}.{suffix}", entry.name);
    if let Some(ptr) = registry.resolve_symbol(&exact) {
        return Some(ptr);
    }
    if let (Some(w), Some(owner_id)) = (world, entry.owner)
        && let Some(mod_host) = w.get_resource::<ModHost>()
        && let Some(loaded) = mod_host.get_mod(owner_id)
        && loaded.is_active()
    {
        if let Some(ptr) = loaded.resolve_guest_symbol(&exact) {
            return Some(ptr);
        }
        if let Some((_, short_name)) = entry.name.rsplit_once("::") {
            let short_sym = format!("bam.v0.{short_name}.{suffix}");
            if let Some(ptr) = loaded.resolve_guest_symbol(&short_sym) {
                return Some(ptr);
            }
        }
    }
    None
}

unsafe extern "C" fn collect_witness_export_sink(
    ctx: *mut c_void,
    field_name: StrView,
    type_tag: StrView,
    payload: FfiSlice<u8>,
) -> FfiResult {
    if ctx.is_null() {
        return FfiResult::NULL_POINTER;
    }
    // SAFETY: `ctx` points to a stack-local `&mut Vec<ExportedFieldRecord>` valid for this call.
    let out = unsafe { &mut *ctx.cast::<Vec<ExportedFieldRecord>>() };
    // SAFETY: Witness guarantees `field_name` points to initialized UTF-8 bytes.
    let Ok(name_str) = (unsafe { field_name.try_as_str() }) else {
        return FfiResult::INVALID_UTF8;
    };
    // SAFETY: Witness guarantees `type_tag` points to initialized UTF-8 bytes.
    let Ok(tag_str) = (unsafe { type_tag.try_as_str() }) else {
        return FfiResult::INVALID_UTF8;
    };
    let bytes = if payload.len == 0 {
        Vec::new()
    } else if payload.ptr.is_null() {
        return FfiResult::NULL_POINTER;
    } else {
        // SAFETY: `payload` is non-null and readable for `payload.len` bytes for this call.
        unsafe { payload.as_slice() }.to_vec()
    };
    out.push(ExportedFieldRecord {
        field_name: name_str.to_owned(),
        type_tag: tag_str.to_owned(),
        bytes,
    });
    FfiResult::OK
}

fn export_fields_from_raw_ptr(
    export_fn: BamExportFieldsFn,
    src_ptr: *const c_void,
) -> Result<Vec<ExportedFieldRecord>, FfiResult> {
    let mut fields = Vec::new();
    // SAFETY: Caller guarantees `src_ptr` points to a valid initialized component/resource value
    // matching `export_fn`, and `fields` outlives this synchronous callback.
    let status = unsafe {
        export_fn(
            src_ptr,
            Some(collect_witness_export_sink),
            (&raw mut fields).cast::<c_void>(),
        )
    };
    if status.is_ok() {
        Ok(fields)
    } else {
        Err(status)
    }
}

fn apply_fields_to_raw_ptr(
    import_fn: BamImportFieldFn,
    dst_ptr: *mut c_void,
    fields: &[ExportedFieldRecord],
    ignore_not_found: bool,
) -> Result<(), FfiResult> {
    for field in fields {
        let slice = if field.bytes.is_empty() {
            FfiSlice::empty()
        } else {
            FfiSlice::from_slice(&field.bytes)
        };
        // SAFETY: Caller guarantees `dst_ptr` points to a valid initialized mutable value matching
        // `import_fn`; `StrView`s and `FfiSlice` borrow valid stack/heap memory for this call.
        let status = unsafe {
            import_fn(
                dst_ptr,
                StrView::from_str(&field.field_name),
                StrView::from_str(&field.type_tag),
                slice,
            )
        };
        if status.is_ok() || (ignore_not_found && status == FfiResult::NOT_FOUND) {
            continue;
        }
        return Err(status);
    }
    Ok(())
}

fn drop_raw_if_needed(drop_fn: Option<ComponentDropFn>, ptr: *mut c_void) {
    if let Some(df) = drop_fn {
        // SAFETY: Caller guarantees `ptr` holds an initialized uncommitted value matching `df`.
        unsafe {
            df(ptr);
        }
    }
}

fn query_field_info_from_fn(field_info_fn: BamFieldInfoFn) -> Vec<WitnessFieldDescriptor> {
    let mut out = Vec::new();
    let mut index = 0usize;
    loop {
        let mut raw_name = StrView::empty();
        let mut raw_tag = StrView::empty();
        let mut offset = 0usize;
        // SAFETY: Output pointers are non-null, aligned stack locals.
        let ok =
            unsafe { field_info_fn(index, &raw mut raw_name, &raw mut raw_tag, &raw mut offset) };
        if !ok.get() {
            break;
        }
        // SAFETY: Generated `field_info` witness writes static valid UTF-8 `StrView`s when returning `TRUE`.
        let Ok(name) = (unsafe { raw_name.try_as_str() }) else {
            break;
        };
        // SAFETY: Same guarantee for `raw_tag`.
        let Ok(type_tag) = (unsafe { raw_tag.try_as_str() }) else {
            break;
        };
        out.push(WitnessFieldDescriptor {
            index,
            name: name.to_owned(),
            type_tag: type_tag.to_owned(),
            offset,
        });
        index += 1;
    }
    out
}

/// Queries the `"bam.v0.<name>.field_info"` witness registered in `registry` (without needing a
/// `World`, suitable for host-registered `#[derive(ModComponent)]` types).
pub fn inspect_component_witness_fields_in_registry(
    registry: &ModRegistry,
    component: BamComponent,
) -> Option<Vec<WitnessFieldDescriptor>> {
    let entry = registry.get_by_handle(component)?;
    let ptr = resolve_witness_symbol_in_world(None, registry, entry, "field_info")?;
    // SAFETY: `"bam.v0.<name>.field_info"` emitted by `#[derive(ModComponent)]` uses `BamFieldInfoFn`.
    let field_info_fn: BamFieldInfoFn =
        unsafe { core::mem::transmute::<*const c_void, BamFieldInfoFn>(ptr) };
    Some(query_field_info_from_fn(field_info_fn))
}

/// Queries the `"bam.v0.<name>.field_info"` witness for `component` across both `ModRegistry` and
/// any active owning guest `LoadedMod` in `world`.
pub fn inspect_component_witness_fields(
    world: &World,
    component: BamComponent,
) -> Option<Vec<WitnessFieldDescriptor>> {
    let registry = world.get_resource::<ModRegistry>()?;
    let entry = registry.get_by_handle(component)?;
    let ptr = resolve_witness_symbol_in_world(Some(world), registry, entry, "field_info")?;
    // SAFETY: `"bam.v0.<name>.field_info"` emitted by `#[derive(ModComponent)]` uses `BamFieldInfoFn`.
    let field_info_fn: BamFieldInfoFn =
        unsafe { core::mem::transmute::<*const c_void, BamFieldInfoFn>(ptr) };
    Some(query_field_info_from_fn(field_info_fn))
}

/// Exports all witness fields of `component` on `entity` in `world` into host-owned
/// [`ExportedFieldRecord`]s with zero allocator crossover.
pub fn export_entity_component_fields(
    world: &World,
    entity: Entity,
    component: BamComponent,
) -> Result<Vec<ExportedFieldRecord>, FfiResult> {
    let registry = world
        .get_resource::<ModRegistry>()
        .ok_or(FfiResult::INVALID_STATE)?;
    let entry = registry
        .get_by_handle(component)
        .ok_or(FfiResult::INVALID_COMPONENT)?;
    if entry.is_resource() || entry.is_message() {
        return Err(FfiResult::INVALID_COMPONENT);
    }
    let fn_ptr = resolve_witness_symbol_in_world(Some(world), registry, entry, "export_fields")
        .ok_or(FfiResult::UNSUPPORTED)?;
    // SAFETY: `"bam.v0.<name>.export_fields"` uses `BamExportFieldsFn`.
    let export_fn: BamExportFieldsFn =
        unsafe { core::mem::transmute::<*const c_void, BamExportFieldsFn>(fn_ptr) };

    let entity_ref = world
        .get_entity(entity)
        .map_err(|_| FfiResult::INVALID_ENTITY)?;
    let comp_ptr = entity_ref
        .get_by_id(entry.component_id)
        .map_err(|_| FfiResult::NOT_FOUND)?;
    let src_ptr = if entry.size == 0 {
        NonNull::<u8>::dangling()
            .as_ptr()
            .cast::<c_void>()
            .cast_const()
    } else {
        comp_ptr.as_ptr().cast::<c_void>()
    };
    export_fields_from_raw_ptr(export_fn, src_ptr)
}

/// Exports all witness fields of singleton resource `resource` in `world` into host-owned
/// [`ExportedFieldRecord`]s.
pub fn export_resource_fields(
    world: &World,
    resource: BamComponent,
) -> Result<Vec<ExportedFieldRecord>, FfiResult> {
    let registry = world
        .get_resource::<ModRegistry>()
        .ok_or(FfiResult::INVALID_STATE)?;
    let entry = registry
        .get_by_handle(resource)
        .ok_or(FfiResult::INVALID_COMPONENT)?;
    if !entry.is_resource() {
        return Err(FfiResult::INVALID_COMPONENT);
    }
    let fn_ptr = resolve_witness_symbol_in_world(Some(world), registry, entry, "export_fields")
        .ok_or(FfiResult::UNSUPPORTED)?;
    // SAFETY: `"bam.v0.<name>.export_fields"` uses `BamExportFieldsFn`.
    let export_fn: BamExportFieldsFn =
        unsafe { core::mem::transmute::<*const c_void, BamExportFieldsFn>(fn_ptr) };

    let res_ptr = world
        .get_resource_by_id(entry.component_id)
        .ok_or(FfiResult::NOT_FOUND)?;
    let src_ptr = if entry.size == 0 {
        NonNull::<u8>::dangling()
            .as_ptr()
            .cast::<c_void>()
            .cast_const()
    } else {
        res_ptr.as_ptr().cast::<c_void>()
    };
    export_fields_from_raw_ptr(export_fn, src_ptr)
}

/// Mutates an existing `component` on `entity` in `world` via `"bam.v0.<name>.import_field"`,
/// marking Bevy change ticks automatically.
pub fn apply_entity_component_fields(
    world: &mut World,
    entity: Entity,
    component: BamComponent,
    fields: &[ExportedFieldRecord],
) -> Result<(), FfiResult> {
    let (comp_id, size, is_immutable, import_fn) = {
        let registry = world
            .get_resource::<ModRegistry>()
            .ok_or(FfiResult::INVALID_STATE)?;
        let entry = registry
            .get_by_handle(component)
            .ok_or(FfiResult::INVALID_COMPONENT)?;
        if entry.is_resource() || entry.is_message() {
            return Err(FfiResult::INVALID_COMPONENT);
        }
        if entry.is_immutable() {
            return Err(FfiResult::PERMISSION_DENIED);
        }
        let fn_ptr = resolve_witness_symbol_in_world(Some(world), registry, entry, "import_field")
            .ok_or(FfiResult::UNSUPPORTED)?;
        // SAFETY: `"bam.v0.<name>.import_field"` uses `BamImportFieldFn`.
        let import_fn: BamImportFieldFn =
            unsafe { core::mem::transmute::<*const c_void, BamImportFieldFn>(fn_ptr) };
        (
            entry.component_id,
            entry.size,
            entry.is_immutable(),
            import_fn,
        )
    };
    if is_immutable {
        return Err(FfiResult::PERMISSION_DENIED);
    }
    let mut ent_mut = world
        .get_entity_mut(entity)
        .map_err(|_| FfiResult::INVALID_ENTITY)?;
    let mut mut_untyped = ent_mut
        .get_mut_by_id(comp_id)
        .map_err(|_| FfiResult::NOT_FOUND)?;
    let ptr_mut = mut_untyped.as_mut();
    let dst_ptr = if size == 0 {
        NonNull::<u8>::dangling().as_ptr().cast::<c_void>()
    } else {
        ptr_mut.as_ptr().cast::<c_void>()
    };
    apply_fields_to_raw_ptr(import_fn, dst_ptr, fields, false)
}

/// Mutates an existing singleton `resource` in `world` via `"bam.v0.<name>.import_field"`,
/// marking Bevy resource change ticks automatically.
pub fn apply_resource_fields(
    world: &mut World,
    resource: BamComponent,
    fields: &[ExportedFieldRecord],
) -> Result<(), FfiResult> {
    let (comp_id, size, import_fn) = {
        let registry = world
            .get_resource::<ModRegistry>()
            .ok_or(FfiResult::INVALID_STATE)?;
        let entry = registry
            .get_by_handle(resource)
            .ok_or(FfiResult::INVALID_COMPONENT)?;
        if !entry.is_resource() {
            return Err(FfiResult::INVALID_COMPONENT);
        }
        if entry.is_immutable() {
            return Err(FfiResult::PERMISSION_DENIED);
        }
        let fn_ptr = resolve_witness_symbol_in_world(Some(world), registry, entry, "import_field")
            .ok_or(FfiResult::UNSUPPORTED)?;
        // SAFETY: `"bam.v0.<name>.import_field"` uses `BamImportFieldFn`.
        let import_fn: BamImportFieldFn =
            unsafe { core::mem::transmute::<*const c_void, BamImportFieldFn>(fn_ptr) };
        (entry.component_id, entry.size, import_fn)
    };
    let mut mut_untyped = world
        .get_resource_mut_by_id(comp_id)
        .ok_or(FfiResult::NOT_FOUND)?;
    let ptr_mut = mut_untyped.as_mut();
    let dst_ptr = if size == 0 {
        NonNull::<u8>::dangling().as_ptr().cast::<c_void>()
    } else {
        ptr_mut.as_ptr().cast::<c_void>()
    };
    apply_fields_to_raw_ptr(import_fn, dst_ptr, fields, false)
}

/// Constructs a fresh `component` value (via `"bam.v0.<name>.init_default"`, `insert_default_fn`,
/// or zeroed `AbiPod`), applies `fields` via `"bam.v0.<name>.import_field"`, and inserts it onto
/// `entity` in `world`.
pub fn insert_entity_component_from_fields(
    world: &mut World,
    entity: Entity,
    component: BamComponent,
    fields: &[ExportedFieldRecord],
) -> Result<(), FfiResult> {
    if world.get_entity(entity).is_err() {
        return Err(FfiResult::INVALID_ENTITY);
    }
    let (entry, init_default_fn, import_fn) = {
        let registry = world
            .get_resource::<ModRegistry>()
            .ok_or(FfiResult::INVALID_STATE)?;
        let entry = registry
            .get_by_handle(component)
            .cloned()
            .ok_or(FfiResult::INVALID_COMPONENT)?;
        if entry.is_resource() || entry.is_message() {
            return Err(FfiResult::INVALID_COMPONENT);
        }
        let init_fn =
            resolve_witness_symbol_in_world(Some(world), registry, &entry, "init_default").map(
                |ptr| {
                    // SAFETY: `"bam.v0.<name>.init_default"` uses `BamInitDefaultFn`.
                    unsafe { core::mem::transmute::<*const c_void, BamInitDefaultFn>(ptr) }
                },
            );
        let imp_fn = resolve_witness_symbol_in_world(Some(world), registry, &entry, "import_field")
            .map(|ptr| {
                // SAFETY: `"bam.v0.<name>.import_field"` uses `BamImportFieldFn`.
                unsafe { core::mem::transmute::<*const c_void, BamImportFieldFn>(ptr) }
            });
        (entry, init_fn, imp_fn)
    };

    // Host component with `insert_default_fn` (when `init_default` C symbol is absent): insert
    // default on `entity`, then apply `fields` in place.
    if init_default_fn.is_none()
        && let Some(host_default_fn) = entry.insert_default_fn
    {
        host_default_fn(world, entity);
        if fields.is_empty() {
            return Ok(());
        }
        return apply_entity_component_fields(world, entity, component, fields);
    }

    let buf = AlignedScratchBuffer::new_zeroed(entry.size, entry.align)?;
    let dst_ptr = buf.as_mut_c_void();
    if let Some(init_fn) = init_default_fn {
        // SAFETY: `dst_ptr` points to `entry.size` aligned bytes in `buf`.
        let st = unsafe { init_fn(dst_ptr) };
        if st.is_err() {
            return Err(st);
        }
    } else if !entry.is_abi_pod {
        return Err(FfiResult::UNSUPPORTED);
    }

    if !fields.is_empty() {
        let Some(imp_fn) = import_fn else {
            drop_raw_if_needed(entry.drop_fn, dst_ptr);
            return Err(FfiResult::UNSUPPORTED);
        };
        if let Err(err) = apply_fields_to_raw_ptr(imp_fn, dst_ptr, fields, false) {
            drop_raw_if_needed(entry.drop_fn, dst_ptr);
            return Err(err);
        }
    }

    if !entry.is_dynamic {
        if let Some(insert_raw) = entry.insert_raw_fn {
            // SAFETY: `dst_ptr` points to `entry.size` aligned initialized bytes for `T`.
            unsafe {
                insert_raw(world, entity, dst_ptr.cast::<u8>());
            }
            return Ok(());
        }
        drop_raw_if_needed(entry.drop_fn, dst_ptr);
        return Err(FfiResult::UNSUPPORTED);
    }

    if let Ok(mut ent_mut) = world.get_entity_mut(entity) {
        let nn = buf.non_null_u8();
        // SAFETY: `nn` holds an initialized value matching `entry.component_id`; `insert_by_id`
        // moves the value into Bevy ECS storage without deallocating `buf`'s backing heap block.
        let owning = unsafe { OwningPtr::new(nn) };
        // SAFETY: `entry.component_id` was registered in `world` with `(entry.size, entry.align)`.
        unsafe {
            ent_mut.insert_by_id(entry.component_id, owning);
        }
        Ok(())
    } else {
        drop_raw_if_needed(entry.drop_fn, dst_ptr);
        Err(FfiResult::INVALID_ENTITY)
    }
}

/// Applies `fields` in place if `entity` already carries `component` (and is mutable), or inserts
/// a newly constructed `component` populated with `fields` otherwise.
pub fn insert_or_apply_entity_component_fields(
    world: &mut World,
    entity: Entity,
    component: BamComponent,
    fields: &[ExportedFieldRecord],
) -> Result<(), FfiResult> {
    let has_mutable = {
        let registry = world
            .get_resource::<ModRegistry>()
            .ok_or(FfiResult::INVALID_STATE)?;
        let entry = registry
            .get_by_handle(component)
            .ok_or(FfiResult::INVALID_COMPONENT)?;
        let ent_ref = world
            .get_entity(entity)
            .map_err(|_| FfiResult::INVALID_ENTITY)?;
        ent_ref.contains_id(entry.component_id) && !entry.is_immutable()
    };
    if has_mutable {
        apply_entity_component_fields(world, entity, component, fields)
    } else {
        insert_entity_component_from_fields(world, entity, component, fields)
    }
}

/// Reads the raw initialized bytes of an `AbiPod` component on `entity` (for pure C/POD dynamic
/// components that have no field-witness symbols).
pub fn read_entity_pod_bytes(
    world: &World,
    entity: Entity,
    component: BamComponent,
) -> Result<Vec<u8>, FfiResult> {
    let registry = world
        .get_resource::<ModRegistry>()
        .ok_or(FfiResult::INVALID_STATE)?;
    let entry = registry
        .get_by_handle(component)
        .ok_or(FfiResult::INVALID_COMPONENT)?;
    if !entry.is_abi_pod || entry.is_resource() || entry.is_message() {
        return Err(FfiResult::TYPE_MISMATCH);
    }
    let ent_ref = world
        .get_entity(entity)
        .map_err(|_| FfiResult::INVALID_ENTITY)?;
    let ptr = ent_ref
        .get_by_id(entry.component_id)
        .map_err(|_| FfiResult::NOT_FOUND)?;
    if entry.size == 0 {
        return Ok(Vec::new());
    }
    let mut out = vec![0u8; entry.size];
    // SAFETY: `entry.is_abi_pod` guarantees zero uninitialized padding and valid byte patterns
    // across all `entry.size` bytes; `ptr` and `out` are non-overlapping.
    unsafe {
        core::ptr::copy_nonoverlapping(ptr.as_ptr(), out.as_mut_ptr(), entry.size);
    }
    Ok(out)
}

/// Overwrites or inserts raw bytes of an `AbiPod` component onto `entity` in `world`.
pub fn write_entity_pod_bytes(
    world: &mut World,
    entity: Entity,
    component: BamComponent,
    bytes: &[u8],
) -> Result<(), FfiResult> {
    let entry = {
        let registry = world
            .get_resource::<ModRegistry>()
            .ok_or(FfiResult::INVALID_STATE)?;
        let entry = registry
            .get_by_handle(component)
            .cloned()
            .ok_or(FfiResult::INVALID_COMPONENT)?;
        if !entry.is_abi_pod || entry.drop_fn.is_some() || entry.is_resource() || entry.is_message()
        {
            return Err(FfiResult::TYPE_MISMATCH);
        }
        if bytes.len() != entry.size {
            return Err(FfiResult::INVALID_ARGUMENT);
        }
        entry
    };

    let buf = AlignedScratchBuffer::new_zeroed(entry.size, entry.align)?;
    if entry.size > 0 {
        // SAFETY: `bytes.len() == entry.size` and `buf.ptr` has `entry.size` writable bytes.
        unsafe {
            core::ptr::copy_nonoverlapping(bytes.as_ptr(), buf.ptr, entry.size);
        }
    }

    if !entry.is_dynamic {
        if let Some(insert_raw) = entry.insert_raw_fn {
            // SAFETY: `buf` holds `entry.size` aligned bytes for `RawInsertable` `T`.
            unsafe {
                insert_raw(world, entity, buf.non_null_u8().as_ptr());
            }
            return Ok(());
        }
        return Err(FfiResult::PERMISSION_DENIED);
    }

    let mut ent_mut = world
        .get_entity_mut(entity)
        .map_err(|_| FfiResult::INVALID_ENTITY)?;
    let nn = buf.non_null_u8();
    // SAFETY: `nn` holds `entry.size` aligned POD bytes matching `entry.component_id`.
    let owning = unsafe { OwningPtr::new(nn) };
    // SAFETY: `entry.component_id` was registered in `world` with `(entry.size, entry.align)`.
    unsafe {
        ent_mut.insert_by_id(entry.component_id, owning);
    }
    Ok(())
}

// ============================================================================
// Declarative ABI 0.3 `BamFieldDesc` Validation & Raw Byte-Offset POD Access
// ============================================================================

/// Resolved POD reflection classification for a declarative [`bevy_abi_mod_sys::BamFieldDesc`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BamFieldPodKind {
    /// `bool` / `FfiBool` (1 byte, align 1).
    Bool,
    /// `u8` (1 byte, align 1).
    U8,
    /// `i8` (1 byte, align 1).
    I8,
    /// `u16` (2 bytes, align 2).
    U16,
    /// `i16` (2 bytes, align 2).
    I16,
    /// `u32` (4 bytes, align 4).
    U32,
    /// `i32` (4 bytes, align 4).
    I32,
    /// `u64` (8 bytes, align 8).
    U64,
    /// `i64` (8 bytes, align 8).
    I64,
    /// `f32` (4 bytes, align 4).
    F32,
    /// `f64` (8 bytes, align 8).
    F64,
    /// `usize` (pointer-sized unsigned integer).
    Usize,
    /// `isize` (pointer-sized signed integer).
    Isize,
    /// `FfiEntity` / `Entity` handle stored as 8-byte `u64` bits (size 8, align 8).
    Entity,
    /// `BamComponent` handle stored as 4-byte `u32` raw ID (size 4, align 4).
    Component,
    /// 2-element `f32` vector / array (`Vec2`, `[f32; 2]`, size 8, align 4).
    Vec2,
    /// 3-element `f32` vector / array (`Vec3`, `[f32; 3]`, size 12, align 4).
    Vec3,
    /// 4-element `f32` vector / quaternion (`Vec4`, `Quat`, `[f32; 4]`, size 16, align 4).
    Vec4,
    /// 2-element `f64` vector / array (`DVec2`, `[f64; 2]`, size 16, align 8).
    DVec2,
    /// 3-element `f64` vector / array (`DVec3`, `[f64; 3]`, size 24, align 8).
    DVec3,
    /// 4-element `f64` vector / array (`DVec4`, `[f64; 4]`, size 32, align 8).
    DVec4,
    /// 2-element `u32` vector / array (`UVec2`, `[u32; 2]`, size 8, align 4).
    UVec2,
    /// 3-element `u32` vector / array (`UVec3`, `[u32; 3]`, size 12, align 4).
    UVec3,
    /// 4-element `u32` vector / array (`UVec4`, `[u32; 4]`, size 16, align 4).
    UVec4,
    /// 2-element `i32` vector / array (`IVec2`, `[i32; 2]`, size 8, align 4).
    IVec2,
    /// 3-element `i32` vector / array (`IVec3`, `[i32; 3]`, size 12, align 4).
    IVec3,
    /// 4-element `i32` vector / array (`IVec4`, `[i32; 4]`, size 16, align 4).
    IVec4,
    /// Fixed 2-byte array `[u8; 2]` (size 2, align 1).
    Bytes2,
    /// Fixed 3-byte array `[u8; 3]` (size 3, align 1).
    Bytes3,
    /// Fixed 4-byte array `[u8; 4]` (size 4, align 1).
    Bytes4,
    /// Fixed 8-byte array `[u8; 8]` (size 8, align 1).
    Bytes8,
    /// Fixed 16-byte array `[u8; 16]` (size 16, align 1).
    Bytes16,
}

impl BamFieldPodKind {
    /// Classifies a `(BamFieldKind, type_tag, size)` tuple into a validated POD reflection kind.
    pub fn classify(kind: BamFieldKind, type_tag: &str, size: usize) -> Option<Self> {
        let compact: String = type_tag.chars().filter(|c| !c.is_whitespace()).collect();
        let tag = compact.as_str();

        // Semantic handle and vector/array tags take priority so e.g. `U64 + "Entity"` or
        // `NESTED + "Vec3"` resolve to their rich reflected representations.
        match tag {
            "Entity" | "FfiEntity" | "BamEntity" | "TargetEntity" | "bevy_ecs::entity::Entity"
                if size == core::mem::size_of::<u64>() =>
            {
                return Some(Self::Entity);
            }
            "BamComponent" | "Component" if size == core::mem::size_of::<u32>() => {
                return Some(Self::Component);
            }
            "Vec2" | "FfiVec2" | "BamVec2" | "[f32;2]" | "glam::Vec2" | "bevy_math::Vec2"
                if size == core::mem::size_of::<[f32; 2]>() =>
            {
                return Some(Self::Vec2);
            }
            "Vec3" | "Vec3A" | "FfiVec3" | "BamVec3" | "[f32;3]" | "glam::Vec3"
            | "bevy_math::Vec3"
                if size == core::mem::size_of::<[f32; 3]>() =>
            {
                return Some(Self::Vec3);
            }
            "Vec4" | "Quat" | "FfiVec4" | "BamVec4" | "FfiQuat" | "[f32;4]" | "glam::Vec4"
            | "glam::Quat" | "bevy_math::Vec4" | "bevy_math::Quat"
                if size == core::mem::size_of::<[f32; 4]>() =>
            {
                return Some(Self::Vec4);
            }
            "DVec2" | "[f64;2]" if size == core::mem::size_of::<[f64; 2]>() => {
                return Some(Self::DVec2);
            }
            "DVec3" | "[f64;3]" if size == core::mem::size_of::<[f64; 3]>() => {
                return Some(Self::DVec3);
            }
            "DVec4" | "DQuat" | "[f64;4]" if size == core::mem::size_of::<[f64; 4]>() => {
                return Some(Self::DVec4);
            }
            "UVec2" | "[u32;2]" if size == core::mem::size_of::<[u32; 2]>() => {
                return Some(Self::UVec2);
            }
            "UVec3" | "[u32;3]" if size == core::mem::size_of::<[u32; 3]>() => {
                return Some(Self::UVec3);
            }
            "UVec4" | "[u32;4]" if size == core::mem::size_of::<[u32; 4]>() => {
                return Some(Self::UVec4);
            }
            "IVec2" | "[i32;2]" if size == core::mem::size_of::<[i32; 2]>() => {
                return Some(Self::IVec2);
            }
            "IVec3" | "[i32;3]" if size == core::mem::size_of::<[i32; 3]>() => {
                return Some(Self::IVec3);
            }
            "IVec4" | "[i32;4]" if size == core::mem::size_of::<[i32; 4]>() => {
                return Some(Self::IVec4);
            }
            "[u8;2]" if size == 2 => return Some(Self::Bytes2),
            "[u8;3]" if size == 3 => return Some(Self::Bytes3),
            "[u8;4]" if size == 4 => return Some(Self::Bytes4),
            "[u8;8]" if size == 8 => return Some(Self::Bytes8),
            "[u8;16]" | "ComponentUuid" | "BamUuid" if size == 16 => return Some(Self::Bytes16),
            _ => {}
        }

        match kind {
            BamFieldKind::BOOL if size == 1 => Some(Self::Bool),
            BamFieldKind::U8 if size == 1 => Some(Self::U8),
            BamFieldKind::I8 if size == 1 => Some(Self::I8),
            BamFieldKind::U16 if size == 2 => Some(Self::U16),
            BamFieldKind::I16 if size == 2 => Some(Self::I16),
            BamFieldKind::U32 if size == 4 => Some(Self::U32),
            BamFieldKind::I32 if size == 4 => Some(Self::I32),
            BamFieldKind::U64 if size == 8 => Some(Self::U64),
            BamFieldKind::I64 if size == 8 => Some(Self::I64),
            BamFieldKind::F32 if size == 4 => Some(Self::F32),
            BamFieldKind::F64 if size == 8 => Some(Self::F64),
            BamFieldKind::USIZE if size == core::mem::size_of::<usize>() => Some(Self::Usize),
            BamFieldKind::ISIZE if size == core::mem::size_of::<isize>() => Some(Self::Isize),
            BamFieldKind::UNKNOWN => match tag {
                "bool" | "FfiBool" | "BamBool" if size == 1 => Some(Self::Bool),
                "u8" if size == 1 => Some(Self::U8),
                "i8" if size == 1 => Some(Self::I8),
                "u16" if size == 2 => Some(Self::U16),
                "i16" if size == 2 => Some(Self::I16),
                "u32" if size == 4 => Some(Self::U32),
                "i32" if size == 4 => Some(Self::I32),
                "u64" if size == 8 => Some(Self::U64),
                "i64" if size == 8 => Some(Self::I64),
                "f32" if size == 4 => Some(Self::F32),
                "f64" if size == 8 => Some(Self::F64),
                "usize" if size == core::mem::size_of::<usize>() => Some(Self::Usize),
                "isize" if size == core::mem::size_of::<isize>() => Some(Self::Isize),
                _ => None,
            },
            _ => None,
        }
    }

    /// Exact byte size required for this POD kind.
    #[inline]
    pub const fn byte_size(self) -> usize {
        match self {
            Self::Bool | Self::U8 | Self::I8 => 1,
            Self::U16 | Self::I16 | Self::Bytes2 => 2,
            Self::Bytes3 => 3,
            Self::U32 | Self::I32 | Self::F32 | Self::Component | Self::Bytes4 => 4,
            Self::U64
            | Self::I64
            | Self::F64
            | Self::Entity
            | Self::Vec2
            | Self::UVec2
            | Self::IVec2
            | Self::Bytes8 => 8,
            Self::Usize => core::mem::size_of::<usize>(),
            Self::Isize => core::mem::size_of::<isize>(),
            Self::Vec3 | Self::UVec3 | Self::IVec3 => 12,
            Self::Vec4 | Self::DVec2 | Self::UVec4 | Self::IVec4 | Self::Bytes16 => 16,
            Self::DVec3 => 24,
            Self::DVec4 => 32,
        }
    }

    /// Rust reference alignment required for direct in-place `&T` / `&mut T` borrowing.
    #[inline]
    pub const fn rust_align(self) -> usize {
        match self {
            Self::Bool
            | Self::U8
            | Self::I8
            | Self::Bytes2
            | Self::Bytes3
            | Self::Bytes4
            | Self::Bytes8
            | Self::Bytes16 => 1,
            Self::U16 | Self::I16 => core::mem::align_of::<u16>(),
            Self::U32
            | Self::I32
            | Self::F32
            | Self::Component
            | Self::Vec2
            | Self::Vec3
            | Self::Vec4
            | Self::UVec2
            | Self::UVec3
            | Self::UVec4
            | Self::IVec2
            | Self::IVec3
            | Self::IVec4 => core::mem::align_of::<u32>(),
            Self::U64
            | Self::I64
            | Self::F64
            | Self::Entity
            | Self::DVec2
            | Self::DVec3
            | Self::DVec4 => core::mem::align_of::<u64>(),
            Self::Usize => core::mem::align_of::<usize>(),
            Self::Isize => core::mem::align_of::<isize>(),
        }
    }
}

/// Host-owned, bounds- and alignment-validated field descriptor extracted from a component's
/// ABI `0.3` [`bevy_abi_mod_sys::BamFieldDesc`] slice.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct BamFieldDescriptor {
    /// Zero-based field ordinal.
    pub index: usize,
    /// Original wire classification tag.
    pub kind: BamFieldKind,
    /// Resolved POD reflection kind (`None` for non-POD / opaque fields).
    pub pod_kind: Option<BamFieldPodKind>,
    /// UTF-8 field identifier.
    pub name: String,
    /// Normalized Rust/ABI type tag.
    pub type_tag: String,
    /// Byte offset inside the enclosing component or resource.
    pub offset: usize,
    /// Field byte size.
    pub size: usize,
    /// Declared field byte alignment.
    pub align: usize,
    /// `true` when this POD field satisfies in-place Rust reference alignment inside the
    /// enclosing component's layout (`component_align >= pod_kind.rust_align()` and
    /// `offset % pod_kind.rust_align() == 0`).
    pub is_inplace_reflectable: bool,
}

/// Single byte-range mutation patch targeting one validated field inside a component or resource.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BamFieldBytePatch {
    /// Byte offset inside the enclosing component or resource.
    pub offset: usize,
    /// Replacement byte payload (must match the declared field size).
    pub bytes: Vec<u8>,
}

fn is_explicit_primitive_kind(kind: BamFieldKind) -> bool {
    matches!(
        kind,
        BamFieldKind::BOOL
            | BamFieldKind::U8
            | BamFieldKind::I8
            | BamFieldKind::U16
            | BamFieldKind::I16
            | BamFieldKind::U32
            | BamFieldKind::I32
            | BamFieldKind::U64
            | BamFieldKind::I64
            | BamFieldKind::F32
            | BamFieldKind::F64
            | BamFieldKind::USIZE
            | BamFieldKind::ISIZE
    )
}

fn extract_validated_bam_fields(
    entry: &RegisteredComponent,
) -> Result<Vec<BamFieldDescriptor>, FfiResult> {
    if entry.fields.is_empty() {
        return Ok(Vec::new());
    }
    let mut seen_names = HashSet::with_capacity(entry.fields.len());
    let mut out = Vec::with_capacity(entry.fields.len());
    for (index, field) in entry.fields.iter().enumerate() {
        // SAFETY: `entry` retains `'static` or `entry.owned_field_strings` backing every `StrView`
        // in `entry.fields` for the full borrow lifetime of `entry`.
        let name = unsafe { field.name.try_as_str() }?;
        // SAFETY: Same lifetime guarantee for `field.type_tag`.
        let type_tag = unsafe { field.type_tag.try_as_str() }?;
        if name.is_empty() || !seen_names.insert(name) {
            return Err(FfiResult::INVALID_ARGUMENT);
        }
        if !field.align.is_power_of_two()
            || field.align > entry.align.max(1)
            || !field.offset.is_multiple_of(field.align)
        {
            return Err(FfiResult::INVALID_ARGUMENT);
        }
        let Some(end) = field.offset.checked_add(field.size) else {
            return Err(FfiResult::OUT_OF_RANGE);
        };
        if end > entry.size {
            return Err(FfiResult::OUT_OF_RANGE);
        }
        let pod_kind = BamFieldPodKind::classify(field.kind, type_tag, field.size);
        if is_explicit_primitive_kind(field.kind) && pod_kind.is_none() {
            return Err(FfiResult::INVALID_ARGUMENT);
        }
        let is_inplace_reflectable = pod_kind.is_some_and(|pk| {
            entry.align >= pk.rust_align() && field.offset.is_multiple_of(pk.rust_align())
        });
        out.push(BamFieldDescriptor {
            index,
            kind: field.kind,
            pod_kind,
            name: name.to_owned(),
            type_tag: type_tag.to_owned(),
            offset: field.offset,
            size: field.size,
            align: field.align,
            is_inplace_reflectable,
        });
    }
    Ok(out)
}

/// Validates and extracts host-owned [`BamFieldDescriptor`]s for `component` directly from
/// `registry`.
///
/// Returns `Ok(None)` when `component` has no declarative `BamFieldDesc` metadata (`field_count == 0`),
/// or `Err(FfiResult)` when `component` is invalid or any field descriptor violates bounds,
/// alignment, naming, or primitive size invariants.
pub fn inspect_component_bam_fields_in_registry(
    registry: &ModRegistry,
    component: BamComponent,
) -> Result<Option<Vec<BamFieldDescriptor>>, FfiResult> {
    let entry = registry
        .get_by_handle(component)
        .ok_or(FfiResult::INVALID_COMPONENT)?;
    if entry.fields.is_empty() {
        return Ok(None);
    }
    extract_validated_bam_fields(entry).map(Some)
}

/// Validates and extracts host-owned [`BamFieldDescriptor`]s for `component` from `world`'s
/// [`ModRegistry`].
pub fn inspect_component_bam_fields(
    world: &World,
    component: BamComponent,
) -> Result<Option<Vec<BamFieldDescriptor>>, FfiResult> {
    let registry = world
        .get_resource::<ModRegistry>()
        .ok_or(FfiResult::INVALID_STATE)?;
    inspect_component_bam_fields_in_registry(registry, component)
}

/// Reads `size` bytes at `offset` from `component` on `entity` with strict bounds validation.
pub fn read_entity_bam_field_bytes(
    world: &World,
    entity: Entity,
    component: BamComponent,
    offset: usize,
    size: usize,
) -> Result<Vec<u8>, FfiResult> {
    let registry = world
        .get_resource::<ModRegistry>()
        .ok_or(FfiResult::INVALID_STATE)?;
    let entry = registry
        .get_by_handle(component)
        .ok_or(FfiResult::INVALID_COMPONENT)?;
    if entry.is_resource() || entry.is_message() {
        return Err(FfiResult::INVALID_COMPONENT);
    }
    let Some(end) = offset.checked_add(size) else {
        return Err(FfiResult::OUT_OF_RANGE);
    };
    if end > entry.size {
        return Err(FfiResult::OUT_OF_RANGE);
    }
    let ent_ref = world
        .get_entity(entity)
        .map_err(|_| FfiResult::INVALID_ENTITY)?;
    let ptr = ent_ref
        .get_by_id(entry.component_id)
        .map_err(|_| FfiResult::NOT_FOUND)?;
    if size == 0 {
        return Ok(Vec::new());
    }
    let mut out = vec![0u8; size];
    // SAFETY: `offset + size <= entry.size` was verified against `entry.component_id`'s layout;
    // `ptr` points to live component storage and `out` is a disjoint heap buffer of `size` bytes.
    unsafe {
        core::ptr::copy_nonoverlapping(ptr.as_ptr().add(offset), out.as_mut_ptr(), size);
    }
    Ok(out)
}

/// Applies validated byte-offset field patches onto an existing mutable `component` on `entity`,
/// marking Bevy change ticks automatically.
pub fn write_entity_bam_field_bytes(
    world: &mut World,
    entity: Entity,
    component: BamComponent,
    patches: &[BamFieldBytePatch],
) -> Result<(), FfiResult> {
    let (comp_id, comp_size) = {
        let registry = world
            .get_resource::<ModRegistry>()
            .ok_or(FfiResult::INVALID_STATE)?;
        let entry = registry
            .get_by_handle(component)
            .ok_or(FfiResult::INVALID_COMPONENT)?;
        if entry.is_resource() || entry.is_message() {
            return Err(FfiResult::INVALID_COMPONENT);
        }
        if entry.is_immutable() {
            return Err(FfiResult::PERMISSION_DENIED);
        }
        for patch in patches {
            let Some(end) = patch.offset.checked_add(patch.bytes.len()) else {
                return Err(FfiResult::OUT_OF_RANGE);
            };
            if end > entry.size {
                return Err(FfiResult::OUT_OF_RANGE);
            }
        }
        (entry.component_id, entry.size)
    };
    let mut ent_mut = world
        .get_entity_mut(entity)
        .map_err(|_| FfiResult::INVALID_ENTITY)?;
    let mut mut_untyped = ent_mut
        .get_mut_by_id(comp_id)
        .map_err(|_| FfiResult::NOT_FOUND)?;
    let dst_base = mut_untyped.as_mut().as_ptr();
    for patch in patches {
        if !patch.bytes.is_empty() && comp_size > 0 {
            // SAFETY: Every patch verified `patch.offset + patch.bytes.len() <= comp_size` above;
            // `dst_base` points to exclusively borrowed live component storage disjoint from `patch.bytes`.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    patch.bytes.as_ptr(),
                    dst_base.add(patch.offset),
                    patch.bytes.len(),
                );
            }
        }
    }
    Ok(())
}

/// Zero-initializes a fresh POD `component`, writes `patches` at validated byte offsets, and
/// inserts the resulting component onto `entity`.
pub fn insert_entity_pod_with_field_patches(
    world: &mut World,
    entity: Entity,
    component: BamComponent,
    patches: &[BamFieldBytePatch],
) -> Result<(), FfiResult> {
    let entry = {
        let registry = world
            .get_resource::<ModRegistry>()
            .ok_or(FfiResult::INVALID_STATE)?;
        let entry = registry
            .get_by_handle(component)
            .cloned()
            .ok_or(FfiResult::INVALID_COMPONENT)?;
        if !entry.is_abi_pod || entry.drop_fn.is_some() || entry.is_resource() || entry.is_message()
        {
            return Err(FfiResult::TYPE_MISMATCH);
        }
        for patch in patches {
            let Some(end) = patch.offset.checked_add(patch.bytes.len()) else {
                return Err(FfiResult::OUT_OF_RANGE);
            };
            if end > entry.size {
                return Err(FfiResult::OUT_OF_RANGE);
            }
        }
        entry
    };

    let buf = AlignedScratchBuffer::new_zeroed(entry.size, entry.align)?;
    for patch in patches {
        if !patch.bytes.is_empty() && entry.size > 0 {
            // SAFETY: Verified `patch.offset + patch.bytes.len() <= entry.size` above; `buf.ptr`
            // has `entry.size` writable bytes disjoint from `patch.bytes`.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    patch.bytes.as_ptr(),
                    buf.ptr.add(patch.offset),
                    patch.bytes.len(),
                );
            }
        }
    }

    if !entry.is_dynamic {
        if let Some(insert_raw) = entry.insert_raw_fn {
            // SAFETY: `buf` holds `entry.size` aligned initialized POD bytes for `T`.
            unsafe {
                insert_raw(world, entity, buf.non_null_u8().as_ptr());
            }
            return Ok(());
        }
        return Err(FfiResult::PERMISSION_DENIED);
    }

    let mut ent_mut = world
        .get_entity_mut(entity)
        .map_err(|_| FfiResult::INVALID_ENTITY)?;
    let nn = buf.non_null_u8();
    // SAFETY: `nn` holds `entry.size` aligned initialized POD bytes matching `entry.component_id`.
    let owning = unsafe { OwningPtr::new(nn) };
    // SAFETY: `entry.component_id` was registered in `world` with `(entry.size, entry.align)`.
    unsafe {
        ent_mut.insert_by_id(entry.component_id, owning);
    }
    Ok(())
}

/// Reads `size` bytes at `offset` from singleton `resource` with strict bounds validation.
pub fn read_resource_bam_field_bytes(
    world: &World,
    resource: BamComponent,
    offset: usize,
    size: usize,
) -> Result<Vec<u8>, FfiResult> {
    let registry = world
        .get_resource::<ModRegistry>()
        .ok_or(FfiResult::INVALID_STATE)?;
    let entry = registry
        .get_by_handle(resource)
        .ok_or(FfiResult::INVALID_COMPONENT)?;
    if !entry.is_resource() {
        return Err(FfiResult::INVALID_COMPONENT);
    }
    let Some(end) = offset.checked_add(size) else {
        return Err(FfiResult::OUT_OF_RANGE);
    };
    if end > entry.size {
        return Err(FfiResult::OUT_OF_RANGE);
    }
    let ptr = world
        .get_resource_by_id(entry.component_id)
        .ok_or(FfiResult::NOT_FOUND)?;
    if size == 0 {
        return Ok(Vec::new());
    }
    let mut out = vec![0u8; size];
    // SAFETY: `offset + size <= entry.size` was verified against `entry.component_id`'s layout;
    // `ptr` points to live resource storage disjoint from `out`.
    unsafe {
        core::ptr::copy_nonoverlapping(ptr.as_ptr().add(offset), out.as_mut_ptr(), size);
    }
    Ok(out)
}

/// Applies validated byte-offset field patches onto an existing mutable singleton `resource`,
/// marking Bevy resource change ticks automatically.
pub fn write_resource_bam_field_bytes(
    world: &mut World,
    resource: BamComponent,
    patches: &[BamFieldBytePatch],
) -> Result<(), FfiResult> {
    let (comp_id, comp_size) = {
        let registry = world
            .get_resource::<ModRegistry>()
            .ok_or(FfiResult::INVALID_STATE)?;
        let entry = registry
            .get_by_handle(resource)
            .ok_or(FfiResult::INVALID_COMPONENT)?;
        if !entry.is_resource() {
            return Err(FfiResult::INVALID_COMPONENT);
        }
        if entry.is_immutable() {
            return Err(FfiResult::PERMISSION_DENIED);
        }
        for patch in patches {
            let Some(end) = patch.offset.checked_add(patch.bytes.len()) else {
                return Err(FfiResult::OUT_OF_RANGE);
            };
            if end > entry.size {
                return Err(FfiResult::OUT_OF_RANGE);
            }
        }
        (entry.component_id, entry.size)
    };
    let mut mut_untyped = world
        .get_resource_mut_by_id(comp_id)
        .ok_or(FfiResult::NOT_FOUND)?;
    let dst_base = mut_untyped.as_mut().as_ptr();
    for patch in patches {
        if !patch.bytes.is_empty() && comp_size > 0 {
            // SAFETY: Every patch verified `patch.offset + patch.bytes.len() <= comp_size` above;
            // `dst_base` points to exclusively borrowed live resource storage disjoint from `patch.bytes`.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    patch.bytes.as_ptr(),
                    dst_base.add(patch.offset),
                    patch.bytes.len(),
                );
            }
        }
    }
    Ok(())
}

// ============================================================================
// Dynamic `StructInfo` + `ReflectFromPtr` + `ReflectComponent` Adapter Pool
// ============================================================================

#[cfg(feature = "bridge-reflect")]
mod bam_field_reflect_pool {
    use super::*;
    use bevy_ecs::entity::EntityMapper;
    use bevy_ecs::relationship::RelationshipHookMode;
    use bevy_ecs::world::unsafe_world_cell::UnsafeEntityCell;
    use bevy_platform::collections::HashMap;
    use bevy_reflect::structs::{
        FieldIter, Struct, StructInfo, struct_debug, struct_partial_cmp, struct_partial_eq,
    };
    use bevy_reflect::{
        ApplyError, CreateTypeData, DynamicTyped, NamedField, PartialReflect, ReflectCloneError,
        ReflectKind, ReflectMut, ReflectOwned, ReflectRef, TypeInfo, TypePath, TypeRegistration,
        Typed,
    };
    use core::any::{Any, TypeId};
    use core::cmp::Ordering;
    use core::fmt::{self, Formatter};
    use std::sync::{Mutex, OnceLock};

    #[derive(Clone, Debug)]
    struct BamReflectSlotField {
        name: &'static str,
        pod_kind: BamFieldPodKind,
        offset: usize,
        size: usize,
    }

    pub(super) struct BamReflectSlotCore {
        pub(super) type_path: &'static str,
        pub(super) short_path: &'static str,
        pub(super) crate_name: Option<&'static str>,
        pub(super) module_path: Option<&'static str>,
        pub(super) expected_component_name: Box<str>,
        pub(super) component_id: ComponentId,
        pub(super) component_size: usize,
        pub(super) component_align: usize,
        pub(super) slot_size: usize,
        pub(super) is_immutable: bool,
        pub(super) is_resource: bool,
        pub(super) is_abi_pod: bool,
        pub(super) has_drop: bool,
        fields: Box<[BamReflectSlotField]>,
        field_indices: HashMap<&'static str, usize>,
    }

    struct SlotCell {
        core: OnceLock<BamReflectSlotCore>,
        type_info: OnceLock<&'static TypeInfo>,
    }

    impl SlotCell {
        const fn new() -> Self {
            Self {
                core: OnceLock::new(),
                type_info: OnceLock::new(),
            }
        }
    }

    #[derive(Clone, Debug, PartialEq, Eq, Hash)]
    struct BamReflectSlotKey {
        component_id: ComponentId,
        type_path: String,
        expected_component_name: String,
        component_size: usize,
        component_align: usize,
        is_immutable: bool,
        is_resource: bool,
        is_abi_pod: bool,
        has_drop: bool,
        fields: Vec<(String, BamFieldPodKind, usize, usize)>,
    }

    #[derive(Default)]
    struct SlotAllocator {
        by_key: HashMap<BamReflectSlotKey, usize>,
        by_type_id: HashMap<TypeId, usize>,
        used: Vec<bool>,
    }

    struct SlotDescriptor {
        slot_index: usize,
        slot_size: usize,
        type_id: fn() -> TypeId,
        init_type_info: fn(&[NamedField]) -> &'static TypeInfo,
        build_registration: fn(bool) -> TypeRegistration,
    }

    /// Size-matched `#[repr(C)]` raw-storage adapter providing zero-copy, Miri-sound
    /// `ReflectFromPtr::ptr_as_reflect` / `ptr_as_reflect_mut`, `Struct`, and `StructInfo`
    /// reflection directly over Bevy ECS storage for foreign `BamFieldDesc` components/resources.
    #[repr(C)]
    #[derive(Copy, Clone)]
    struct BamFieldReflectSlot<const SLOT: usize, const SIZE: usize>([MaybeUninit<u8>; SIZE]);

    #[inline]
    fn slot_core(slot: usize) -> Option<&'static BamReflectSlotCore> {
        SLOT_CELLS.get(slot)?.core.get()
    }

    #[inline]
    fn slot_core_expect(slot: usize) -> &'static BamReflectSlotCore {
        SLOT_CELLS[slot]
            .core
            .get()
            .expect("BamFieldReflectSlot core metadata must be initialized before use")
    }

    #[inline]
    fn slot_type_info_expect(slot: usize) -> &'static TypeInfo {
        SLOT_CELLS[slot]
            .type_info
            .get()
            .expect("BamFieldReflectSlot TypeInfo must be initialized before use")
    }

    fn parse_type_path_parts(
        type_path: &'static str,
    ) -> (&'static str, Option<&'static str>, Option<&'static str>) {
        if let Some((module_path, short_path)) = type_path.rsplit_once("::") {
            let crate_name = module_path
                .split_once("::")
                .map_or(module_path, |(first, _)| first);
            (short_path, Some(crate_name), Some(module_path))
        } else {
            (type_path, None, None)
        }
    }

    fn make_named_field(name: &'static str, kind: BamFieldPodKind) -> NamedField {
        match kind {
            BamFieldPodKind::Bool => NamedField::new::<bool>(name),
            BamFieldPodKind::U8 => NamedField::new::<u8>(name),
            BamFieldPodKind::I8 => NamedField::new::<i8>(name),
            BamFieldPodKind::U16 => NamedField::new::<u16>(name),
            BamFieldPodKind::I16 => NamedField::new::<i16>(name),
            BamFieldPodKind::U32 | BamFieldPodKind::Component => NamedField::new::<u32>(name),
            BamFieldPodKind::I32 => NamedField::new::<i32>(name),
            BamFieldPodKind::U64 | BamFieldPodKind::Entity => NamedField::new::<u64>(name),
            BamFieldPodKind::I64 => NamedField::new::<i64>(name),
            BamFieldPodKind::F32 => NamedField::new::<f32>(name),
            BamFieldPodKind::F64 => NamedField::new::<f64>(name),
            BamFieldPodKind::Usize => NamedField::new::<usize>(name),
            BamFieldPodKind::Isize => NamedField::new::<isize>(name),
            BamFieldPodKind::Vec2 => NamedField::new::<[f32; 2]>(name),
            BamFieldPodKind::Vec3 => NamedField::new::<[f32; 3]>(name),
            BamFieldPodKind::Vec4 => NamedField::new::<[f32; 4]>(name),
            BamFieldPodKind::DVec2 => NamedField::new::<[f64; 2]>(name),
            BamFieldPodKind::DVec3 => NamedField::new::<[f64; 3]>(name),
            BamFieldPodKind::DVec4 => NamedField::new::<[f64; 4]>(name),
            BamFieldPodKind::UVec2 => NamedField::new::<[u32; 2]>(name),
            BamFieldPodKind::UVec3 => NamedField::new::<[u32; 3]>(name),
            BamFieldPodKind::UVec4 => NamedField::new::<[u32; 4]>(name),
            BamFieldPodKind::IVec2 => NamedField::new::<[i32; 2]>(name),
            BamFieldPodKind::IVec3 => NamedField::new::<[i32; 3]>(name),
            BamFieldPodKind::IVec4 => NamedField::new::<[i32; 4]>(name),
            BamFieldPodKind::Bytes2 => NamedField::new::<[u8; 2]>(name),
            BamFieldPodKind::Bytes3 => NamedField::new::<[u8; 3]>(name),
            BamFieldPodKind::Bytes4 => NamedField::new::<[u8; 4]>(name),
            BamFieldPodKind::Bytes8 => NamedField::new::<[u8; 8]>(name),
            BamFieldPodKind::Bytes16 => NamedField::new::<[u8; 16]>(name),
        }
    }

    /// Borrows a single validated POD field at `field.offset` from `base[0..slot_size]`.
    ///
    /// # Safety
    /// - `base` must point to a live `[MaybeUninit<u8>; slot_size]` buffer borrowed for `'a`.
    /// - Bytes in `field.offset..field.offset + field.size` must be initialized for the live component.
    #[allow(clippy::cast_ptr_alignment)]
    unsafe fn field_ref_from_raw<'a>(
        base: *const MaybeUninit<u8>,
        slot_size: usize,
        field: &BamReflectSlotField,
    ) -> Option<&'a dyn PartialReflect> {
        let end = field.offset.checked_add(field.size)?;
        if end > slot_size || field.size != field.pod_kind.byte_size() {
            return None;
        }
        // SAFETY: `field.offset + field.size <= slot_size` within the borrowed buffer.
        let raw = unsafe { base.add(field.offset) }.cast::<u8>();
        let align = field.pod_kind.rust_align();
        if !(raw as usize).is_multiple_of(align) {
            return None;
        }
        if field.pod_kind == BamFieldPodKind::Bool {
            // SAFETY: 1-byte `u8` read at validated in-bounds offset; validate `<= 1` before `&bool`.
            let byte = unsafe { *raw };
            return if byte <= 1 {
                // SAFETY: Verified `byte` is `0` or `1`, valid bit pattern for Rust `bool`.
                Some(unsafe { &*raw.cast::<bool>() })
            } else {
                static TRUE_VAL: bool = true;
                Some(&TRUE_VAL)
            };
        }
        // SAFETY: Every POD primitive/array arm below verified exact `size == pod_kind.byte_size()`,
        // bounds `field.offset + field.size <= slot_size`, and runtime alignment
        // `(raw as usize).is_multiple_of(align)` for the live `'a` component borrow.
        let r: &'a dyn PartialReflect = unsafe {
            match field.pod_kind {
                BamFieldPodKind::Bool => unreachable!(),
                BamFieldPodKind::U8 => &*raw,
                BamFieldPodKind::I8 => &*raw.cast::<i8>(),
                BamFieldPodKind::U16 => &*raw.cast::<u16>(),
                BamFieldPodKind::I16 => &*raw.cast::<i16>(),
                BamFieldPodKind::U32 | BamFieldPodKind::Component => &*raw.cast::<u32>(),
                BamFieldPodKind::I32 => &*raw.cast::<i32>(),
                BamFieldPodKind::U64 | BamFieldPodKind::Entity => &*raw.cast::<u64>(),
                BamFieldPodKind::I64 => &*raw.cast::<i64>(),
                BamFieldPodKind::F32 => &*raw.cast::<f32>(),
                BamFieldPodKind::F64 => &*raw.cast::<f64>(),
                BamFieldPodKind::Usize => &*raw.cast::<usize>(),
                BamFieldPodKind::Isize => &*raw.cast::<isize>(),
                BamFieldPodKind::Vec2 => &*raw.cast::<[f32; 2]>(),
                BamFieldPodKind::Vec3 => &*raw.cast::<[f32; 3]>(),
                BamFieldPodKind::Vec4 => &*raw.cast::<[f32; 4]>(),
                BamFieldPodKind::DVec2 => &*raw.cast::<[f64; 2]>(),
                BamFieldPodKind::DVec3 => &*raw.cast::<[f64; 3]>(),
                BamFieldPodKind::DVec4 => &*raw.cast::<[f64; 4]>(),
                BamFieldPodKind::UVec2 => &*raw.cast::<[u32; 2]>(),
                BamFieldPodKind::UVec3 => &*raw.cast::<[u32; 3]>(),
                BamFieldPodKind::UVec4 => &*raw.cast::<[u32; 4]>(),
                BamFieldPodKind::IVec2 => &*raw.cast::<[i32; 2]>(),
                BamFieldPodKind::IVec3 => &*raw.cast::<[i32; 3]>(),
                BamFieldPodKind::IVec4 => &*raw.cast::<[i32; 4]>(),
                BamFieldPodKind::Bytes2 => &*raw.cast::<[u8; 2]>(),
                BamFieldPodKind::Bytes3 => &*raw.cast::<[u8; 3]>(),
                BamFieldPodKind::Bytes4 => &*raw.cast::<[u8; 4]>(),
                BamFieldPodKind::Bytes8 => &*raw.cast::<[u8; 8]>(),
                BamFieldPodKind::Bytes16 => &*raw.cast::<[u8; 16]>(),
            }
        };
        Some(r)
    }

    /// Mutably borrows a single validated POD field at `field.offset` from `base[0..slot_size]`.
    ///
    /// # Safety
    /// - `base` must point to an exclusively borrowed `[MaybeUninit<u8>; slot_size]` buffer for `'a`.
    /// - Bytes in `field.offset..field.offset + field.size` must be initialized for the live component.
    #[allow(clippy::cast_ptr_alignment)]
    unsafe fn field_mut_from_raw<'a>(
        base: *mut MaybeUninit<u8>,
        slot_size: usize,
        field: &BamReflectSlotField,
    ) -> Option<&'a mut dyn PartialReflect> {
        let end = field.offset.checked_add(field.size)?;
        if end > slot_size || field.size != field.pod_kind.byte_size() {
            return None;
        }
        // SAFETY: `field.offset + field.size <= slot_size` within the exclusively borrowed buffer.
        let raw = unsafe { base.add(field.offset) }.cast::<u8>();
        let align = field.pod_kind.rust_align();
        if !(raw as usize).is_multiple_of(align) {
            return None;
        }
        if field.pod_kind == BamFieldPodKind::Bool {
            // SAFETY: Normalize any foreign non-0/1 truthy byte to `1` before creating `&mut bool`.
            return Some(unsafe {
                if *raw > 1 {
                    *raw = 1;
                }
                &mut *raw.cast::<bool>()
            });
        }
        // SAFETY: Every POD primitive/array arm below verified exact `size == pod_kind.byte_size()`,
        // bounds `field.offset + field.size <= slot_size`, and runtime alignment
        // `(raw as usize).is_multiple_of(align)` for the exclusively borrowed `'a` component buffer.
        let r: &'a mut dyn PartialReflect = unsafe {
            match field.pod_kind {
                BamFieldPodKind::Bool => unreachable!(),
                BamFieldPodKind::U8 => &mut *raw,
                BamFieldPodKind::I8 => &mut *raw.cast::<i8>(),
                BamFieldPodKind::U16 => &mut *raw.cast::<u16>(),
                BamFieldPodKind::I16 => &mut *raw.cast::<i16>(),
                BamFieldPodKind::U32 | BamFieldPodKind::Component => &mut *raw.cast::<u32>(),
                BamFieldPodKind::I32 => &mut *raw.cast::<i32>(),
                BamFieldPodKind::U64 | BamFieldPodKind::Entity => &mut *raw.cast::<u64>(),
                BamFieldPodKind::I64 => &mut *raw.cast::<i64>(),
                BamFieldPodKind::F32 => &mut *raw.cast::<f32>(),
                BamFieldPodKind::F64 => &mut *raw.cast::<f64>(),
                BamFieldPodKind::Usize => &mut *raw.cast::<usize>(),
                BamFieldPodKind::Isize => &mut *raw.cast::<isize>(),
                BamFieldPodKind::Vec2 => &mut *raw.cast::<[f32; 2]>(),
                BamFieldPodKind::Vec3 => &mut *raw.cast::<[f32; 3]>(),
                BamFieldPodKind::Vec4 => &mut *raw.cast::<[f32; 4]>(),
                BamFieldPodKind::DVec2 => &mut *raw.cast::<[f64; 2]>(),
                BamFieldPodKind::DVec3 => &mut *raw.cast::<[f64; 3]>(),
                BamFieldPodKind::DVec4 => &mut *raw.cast::<[f64; 4]>(),
                BamFieldPodKind::UVec2 => &mut *raw.cast::<[u32; 2]>(),
                BamFieldPodKind::UVec3 => &mut *raw.cast::<[u32; 3]>(),
                BamFieldPodKind::UVec4 => &mut *raw.cast::<[u32; 4]>(),
                BamFieldPodKind::IVec2 => &mut *raw.cast::<[i32; 2]>(),
                BamFieldPodKind::IVec3 => &mut *raw.cast::<[i32; 3]>(),
                BamFieldPodKind::IVec4 => &mut *raw.cast::<[i32; 4]>(),
                BamFieldPodKind::Bytes2 => &mut *raw.cast::<[u8; 2]>(),
                BamFieldPodKind::Bytes3 => &mut *raw.cast::<[u8; 3]>(),
                BamFieldPodKind::Bytes4 => &mut *raw.cast::<[u8; 4]>(),
                BamFieldPodKind::Bytes8 => &mut *raw.cast::<[u8; 8]>(),
                BamFieldPodKind::Bytes16 => &mut *raw.cast::<[u8; 16]>(),
            }
        };
        Some(r)
    }

    fn apply_struct_to_slot(
        slot: usize,
        base: *mut MaybeUninit<u8>,
        slot_size: usize,
        value: &dyn PartialReflect,
    ) -> Result<(), ApplyError> {
        let src_struct = value.reflect_ref().as_struct()?;
        let meta = slot_core_expect(slot);
        for (name, field_val) in src_struct.iter_fields() {
            let Some(&idx) = meta.field_indices.get(name) else {
                continue;
            };
            let field_meta = &meta.fields[idx];
            // SAFETY: Caller passed `self.0.as_mut_ptr()` from `&mut self` with length `slot_size`.
            if let Some(target_field) = unsafe { field_mut_from_raw(base, slot_size, field_meta) } {
                if field_meta.pod_kind == BamFieldPodKind::Entity
                    && let Some(target_bits) = target_field.try_downcast_mut::<u64>()
                    && let Some(ent) = field_val.try_downcast_ref::<Entity>()
                {
                    *target_bits = ent.to_bits();
                    continue;
                }
                target_field.try_apply(field_val)?;
            }
        }
        Ok(())
    }

    impl<const SLOT: usize, const SIZE: usize> TypePath for BamFieldReflectSlot<SLOT, SIZE> {
        fn type_path() -> &'static str {
            slot_core_expect(SLOT).type_path
        }

        fn short_type_path() -> &'static str {
            slot_core_expect(SLOT).short_path
        }

        fn type_ident() -> Option<&'static str> {
            Some(slot_core_expect(SLOT).short_path)
        }

        fn crate_name() -> Option<&'static str> {
            slot_core_expect(SLOT).crate_name
        }

        fn module_path() -> Option<&'static str> {
            slot_core_expect(SLOT).module_path
        }
    }

    impl<const SLOT: usize, const SIZE: usize> Typed for BamFieldReflectSlot<SLOT, SIZE> {
        fn type_info() -> &'static TypeInfo {
            slot_type_info_expect(SLOT)
        }
    }

    impl<const SLOT: usize, const SIZE: usize> Struct for BamFieldReflectSlot<SLOT, SIZE> {
        fn field(&self, name: &str) -> Option<&dyn PartialReflect> {
            let idx = slot_core_expect(SLOT).field_indices.get(name).copied()?;
            self.field_at(idx)
        }

        fn field_mut(&mut self, name: &str) -> Option<&mut dyn PartialReflect> {
            let idx = slot_core_expect(SLOT).field_indices.get(name).copied()?;
            self.field_at_mut(idx)
        }

        fn field_at(&self, index: usize) -> Option<&dyn PartialReflect> {
            let field = slot_core_expect(SLOT).fields.get(index)?;
            // SAFETY: `self.0` is a live `[MaybeUninit<u8>; SIZE]` borrowed for `'_`.
            unsafe { field_ref_from_raw(self.0.as_ptr(), SIZE, field) }
        }

        fn field_at_mut(&mut self, index: usize) -> Option<&mut dyn PartialReflect> {
            let field = slot_core_expect(SLOT).fields.get(index)?;
            // SAFETY: `self.0` is an exclusively borrowed `[MaybeUninit<u8>; SIZE]` for `'_`.
            unsafe { field_mut_from_raw(self.0.as_mut_ptr(), SIZE, field) }
        }

        fn name_at(&self, index: usize) -> Option<&str> {
            slot_core_expect(SLOT).fields.get(index).map(|f| f.name)
        }

        fn index_of_name(&self, name: &str) -> Option<usize> {
            slot_core_expect(SLOT).field_indices.get(name).copied()
        }

        fn field_len(&self) -> usize {
            slot_core_expect(SLOT).fields.len()
        }

        fn iter_fields(&self) -> FieldIter<'_> {
            FieldIter::new(self)
        }
    }

    impl<const SLOT: usize, const SIZE: usize> PartialReflect for BamFieldReflectSlot<SLOT, SIZE> {
        #[inline]
        fn get_represented_type_info(&self) -> Option<&'static TypeInfo> {
            Some(Self::type_info())
        }

        #[inline]
        fn into_partial_reflect(self: Box<Self>) -> Box<dyn PartialReflect> {
            self
        }

        #[inline]
        fn as_partial_reflect(&self) -> &dyn PartialReflect {
            self
        }

        #[inline]
        fn as_partial_reflect_mut(&mut self) -> &mut dyn PartialReflect {
            self
        }

        #[inline]
        fn try_into_reflect(self: Box<Self>) -> Result<Box<dyn Reflect>, Box<dyn PartialReflect>> {
            Ok(self)
        }

        #[inline]
        fn try_as_reflect(&self) -> Option<&dyn Reflect> {
            Some(self)
        }

        #[inline]
        fn try_as_reflect_mut(&mut self) -> Option<&mut dyn Reflect> {
            Some(self)
        }

        fn try_apply(&mut self, value: &dyn PartialReflect) -> Result<(), ApplyError> {
            apply_struct_to_slot(SLOT, self.0.as_mut_ptr(), SIZE, value)
        }

        #[inline]
        fn reflect_kind(&self) -> ReflectKind {
            ReflectKind::Struct
        }

        #[inline]
        fn reflect_ref(&self) -> ReflectRef<'_> {
            ReflectRef::Struct(self)
        }

        #[inline]
        fn reflect_mut(&mut self) -> ReflectMut<'_> {
            ReflectMut::Struct(self)
        }

        #[inline]
        fn reflect_owned(self: Box<Self>) -> ReflectOwned {
            ReflectOwned::Struct(self)
        }

        fn reflect_clone(&self) -> Result<Box<dyn Reflect>, ReflectCloneError> {
            let meta = slot_core_expect(SLOT);
            if meta.is_abi_pod && !meta.has_drop {
                // SAFETY: `self.0` holds `SIZE` live `MaybeUninit<u8>` bytes for `SLOT`.
                let owned =
                    unsafe { OwnedBamFieldStruct::from_raw_bytes(SLOT, self.0.as_ptr(), SIZE) };
                Ok(Box::new(owned))
            } else {
                Err(ReflectCloneError::NotCloneable {
                    type_path: meta.type_path.into(),
                })
            }
        }

        fn reflect_partial_eq(&self, value: &dyn PartialReflect) -> Option<bool> {
            struct_partial_eq(self, value)
        }

        fn reflect_partial_cmp(&self, value: &dyn PartialReflect) -> Option<Ordering> {
            struct_partial_cmp(self, value)
        }

        fn debug(&self, f: &mut Formatter<'_>) -> fmt::Result {
            struct_debug(self, f)
        }
    }

    impl<const SLOT: usize, const SIZE: usize> Reflect for BamFieldReflectSlot<SLOT, SIZE> {
        #[inline]
        fn into_any(self: Box<Self>) -> Box<dyn Any> {
            self
        }

        #[inline]
        fn as_any(&self) -> &dyn Any {
            self
        }

        #[inline]
        fn as_any_mut(&mut self) -> &mut dyn Any {
            self
        }

        #[inline]
        fn into_reflect(self: Box<Self>) -> Box<dyn Reflect> {
            self
        }

        #[inline]
        fn as_reflect(&self) -> &dyn Reflect {
            self
        }

        #[inline]
        fn as_reflect_mut(&mut self) -> &mut dyn Reflect {
            self
        }

        fn set(&mut self, value: Box<dyn Reflect>) -> Result<(), Box<dyn Reflect>> {
            match value.take::<Self>() {
                Ok(concrete) => {
                    *self = concrete;
                    Ok(())
                }
                Err(other) => {
                    if self.try_apply(other.as_partial_reflect()).is_ok() {
                        Ok(())
                    } else {
                        Err(other)
                    }
                }
            }
        }
    }

    /// Heap-owned, 8-byte-aligned reflected copy of a [`BamFieldReflectSlot`] component returned
    /// by [`PartialReflect::reflect_clone`] and [`ReflectComponent::take`].
    ///
    /// Backing the byte storage with `Box<[MaybeUninit<u64>]>` guarantees 8-byte alignment for all
    /// in-place POD field borrows even under allocators (such as Miri) that align 1-byte types on
    /// odd addresses, while preserving uninitialized interior padding bytes soundly.
    struct OwnedBamFieldStruct {
        slot: usize,
        byte_len: usize,
        storage: Box<[MaybeUninit<u64>]>,
    }

    impl OwnedBamFieldStruct {
        /// # Safety
        /// `src` must point to at least `byte_len` readable `MaybeUninit<u8>` bytes matching `slot`.
        unsafe fn from_raw_bytes(
            slot: usize,
            src: *const MaybeUninit<u8>,
            byte_len: usize,
        ) -> Self {
            let words = byte_len.div_ceil(core::mem::size_of::<u64>());
            let mut storage = vec![MaybeUninit::<u64>::uninit(); words].into_boxed_slice();
            if byte_len > 0 {
                // SAFETY: `storage` holds `words * 8 >= byte_len` writable bytes disjoint from `src`.
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        src,
                        storage.as_mut_ptr().cast::<MaybeUninit<u8>>(),
                        byte_len,
                    );
                }
            }
            Self {
                slot,
                byte_len,
                storage,
            }
        }

        fn clone_aligned(&self) -> Self {
            let mut storage =
                vec![MaybeUninit::<u64>::uninit(); self.storage.len()].into_boxed_slice();
            storage.copy_from_slice(&self.storage);
            Self {
                slot: self.slot,
                byte_len: self.byte_len,
                storage,
            }
        }
    }

    impl TypePath for OwnedBamFieldStruct {
        fn type_path() -> &'static str {
            "bevy_abi_mod_host_sys::OwnedBamFieldStruct"
        }

        fn short_type_path() -> &'static str {
            "OwnedBamFieldStruct"
        }

        fn type_ident() -> Option<&'static str> {
            Some("OwnedBamFieldStruct")
        }

        fn crate_name() -> Option<&'static str> {
            Some("bevy_abi_mod_host_sys")
        }

        fn module_path() -> Option<&'static str> {
            None
        }
    }

    impl Struct for OwnedBamFieldStruct {
        fn field(&self, name: &str) -> Option<&dyn PartialReflect> {
            let idx = slot_core_expect(self.slot)
                .field_indices
                .get(name)
                .copied()?;
            self.field_at(idx)
        }

        fn field_mut(&mut self, name: &str) -> Option<&mut dyn PartialReflect> {
            let idx = slot_core_expect(self.slot)
                .field_indices
                .get(name)
                .copied()?;
            self.field_at_mut(idx)
        }

        fn field_at(&self, index: usize) -> Option<&dyn PartialReflect> {
            let field = slot_core_expect(self.slot).fields.get(index)?;
            // SAFETY: `self.storage` is 8-byte aligned and owns at least `self.byte_len` bytes.
            unsafe {
                field_ref_from_raw(
                    self.storage.as_ptr().cast::<MaybeUninit<u8>>(),
                    self.byte_len,
                    field,
                )
            }
        }

        fn field_at_mut(&mut self, index: usize) -> Option<&mut dyn PartialReflect> {
            let field = slot_core_expect(self.slot).fields.get(index)?;
            // SAFETY: `self.storage` is 8-byte aligned and exclusively borrowed for `'_`.
            unsafe {
                field_mut_from_raw(
                    self.storage.as_mut_ptr().cast::<MaybeUninit<u8>>(),
                    self.byte_len,
                    field,
                )
            }
        }

        fn name_at(&self, index: usize) -> Option<&str> {
            slot_core_expect(self.slot)
                .fields
                .get(index)
                .map(|f| f.name)
        }

        fn index_of_name(&self, name: &str) -> Option<usize> {
            slot_core_expect(self.slot).field_indices.get(name).copied()
        }

        fn field_len(&self) -> usize {
            slot_core_expect(self.slot).fields.len()
        }

        fn iter_fields(&self) -> FieldIter<'_> {
            FieldIter::new(self)
        }
    }

    impl PartialReflect for OwnedBamFieldStruct {
        #[inline]
        fn get_represented_type_info(&self) -> Option<&'static TypeInfo> {
            SLOT_CELLS[self.slot].type_info.get().copied()
        }

        #[inline]
        fn into_partial_reflect(self: Box<Self>) -> Box<dyn PartialReflect> {
            self
        }

        #[inline]
        fn as_partial_reflect(&self) -> &dyn PartialReflect {
            self
        }

        #[inline]
        fn as_partial_reflect_mut(&mut self) -> &mut dyn PartialReflect {
            self
        }

        #[inline]
        fn try_into_reflect(self: Box<Self>) -> Result<Box<dyn Reflect>, Box<dyn PartialReflect>> {
            Ok(self)
        }

        #[inline]
        fn try_as_reflect(&self) -> Option<&dyn Reflect> {
            Some(self)
        }

        #[inline]
        fn try_as_reflect_mut(&mut self) -> Option<&mut dyn Reflect> {
            Some(self)
        }

        fn try_apply(&mut self, value: &dyn PartialReflect) -> Result<(), ApplyError> {
            apply_struct_to_slot(
                self.slot,
                self.storage.as_mut_ptr().cast::<MaybeUninit<u8>>(),
                self.byte_len,
                value,
            )
        }

        #[inline]
        fn reflect_kind(&self) -> ReflectKind {
            ReflectKind::Struct
        }

        #[inline]
        fn reflect_ref(&self) -> ReflectRef<'_> {
            ReflectRef::Struct(self)
        }

        #[inline]
        fn reflect_mut(&mut self) -> ReflectMut<'_> {
            ReflectMut::Struct(self)
        }

        #[inline]
        fn reflect_owned(self: Box<Self>) -> ReflectOwned {
            ReflectOwned::Struct(self)
        }

        fn reflect_clone(&self) -> Result<Box<dyn Reflect>, ReflectCloneError> {
            Ok(Box::new(self.clone_aligned()))
        }

        fn reflect_partial_eq(&self, value: &dyn PartialReflect) -> Option<bool> {
            struct_partial_eq(self, value)
        }

        fn reflect_partial_cmp(&self, value: &dyn PartialReflect) -> Option<Ordering> {
            struct_partial_cmp(self, value)
        }

        fn debug(&self, f: &mut Formatter<'_>) -> fmt::Result {
            struct_debug(self, f)
        }
    }

    impl DynamicTyped for OwnedBamFieldStruct {
        #[inline]
        fn reflect_type_info(&self) -> &'static TypeInfo {
            SLOT_CELLS[self.slot]
                .type_info
                .get()
                .expect("OwnedBamFieldStruct slot TypeInfo must be initialized")
        }
    }

    impl Reflect for OwnedBamFieldStruct {
        #[inline]
        fn into_any(self: Box<Self>) -> Box<dyn Any> {
            self
        }

        #[inline]
        fn as_any(&self) -> &dyn Any {
            self
        }

        #[inline]
        fn as_any_mut(&mut self) -> &mut dyn Any {
            self
        }

        #[inline]
        fn into_reflect(self: Box<Self>) -> Box<dyn Reflect> {
            self
        }

        #[inline]
        fn as_reflect(&self) -> &dyn Reflect {
            self
        }

        #[inline]
        fn as_reflect_mut(&mut self) -> &mut dyn Reflect {
            self
        }

        fn set(&mut self, value: Box<dyn Reflect>) -> Result<(), Box<dyn Reflect>> {
            match value.take::<Self>() {
                Ok(concrete)
                    if concrete.slot == self.slot && concrete.byte_len == self.byte_len =>
                {
                    *self = concrete;
                    Ok(())
                }
                Ok(other) => Err(Box::new(other)),
                Err(other) => {
                    if self.try_apply(other.as_partial_reflect()).is_ok() {
                        Ok(())
                    } else {
                        Err(other)
                    }
                }
            }
        }
    }

    fn verify_world_component_info(world: &World, meta: &BamReflectSlotCore) -> bool {
        let Some(info) = world.components().get_info(meta.component_id) else {
            return false;
        };
        info.layout().size() == meta.component_size
            && info.layout().align() == meta.component_align
            && info.name().to_string() == meta.expected_component_name.as_ref()
    }

    fn insert_slot_component_on_entity(
        slot: usize,
        slot_size: usize,
        entity: &mut EntityWorldMut,
        reflected: &dyn PartialReflect,
    ) {
        let Some(meta) = slot_core(slot) else {
            return;
        };
        if !meta.is_abi_pod || meta.has_drop || meta.is_resource {
            return;
        }
        if !verify_world_component_info(entity.world(), meta) {
            return;
        }
        let Ok(buf) = AlignedScratchBuffer::new_zeroed(meta.component_size, meta.component_align)
        else {
            return;
        };
        if apply_struct_to_slot(
            slot,
            buf.ptr.cast::<MaybeUninit<u8>>(),
            slot_size,
            reflected,
        )
        .is_err()
        {
            return;
        }
        let nn = buf.non_null_u8();
        // SAFETY: `nn` holds `meta.component_size` aligned initialized POD bytes matching `meta.component_id`.
        let owning = unsafe { OwningPtr::new(nn) };
        // SAFETY: Verified `meta.component_id` layout & name match `entity.world()` above.
        unsafe {
            entity.insert_by_id(meta.component_id, owning);
        }
    }

    fn copy_slot_component_between_worlds(
        slot: usize,
        src_world: &World,
        dst_world: &mut World,
        src_ent: Entity,
        dst_ent: Entity,
    ) {
        let Some(meta) = slot_core(slot) else {
            return;
        };
        if !meta.is_abi_pod || meta.has_drop || meta.is_resource {
            return;
        }
        if !verify_world_component_info(src_world, meta)
            || !verify_world_component_info(dst_world, meta)
        {
            return;
        }
        let Ok(src_ref) = src_world.get_entity(src_ent) else {
            return;
        };
        let Ok(src_ptr) = src_ref.get_by_id(meta.component_id) else {
            return;
        };
        let Ok(buf) = AlignedScratchBuffer::new_zeroed(meta.component_size, meta.component_align)
        else {
            return;
        };
        if meta.component_size > 0 {
            // SAFETY: Both `src_ptr` and `buf.ptr` hold `meta.component_size` bytes as `MaybeUninit<u8>`.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    src_ptr.as_ptr().cast::<MaybeUninit<u8>>(),
                    buf.ptr.cast::<MaybeUninit<u8>>(),
                    meta.component_size,
                );
            }
        }
        if let Ok(mut dst_mut) = dst_world.get_entity_mut(dst_ent) {
            let nn = buf.non_null_u8();
            // SAFETY: `nn` holds `meta.component_size` aligned POD bytes matching `meta.component_id`.
            let owning = unsafe { OwningPtr::new(nn) };
            // SAFETY: Verified `meta.component_id` layout matches `dst_world` above.
            unsafe {
                dst_mut.insert_by_id(meta.component_id, owning);
            }
        }
    }

    fn build_slot_reflect_component<const SLOT: usize, const SIZE: usize>() -> ReflectComponent {
        ReflectComponent::new(ReflectComponentFns {
            insert: |entity, reflected, _registry| {
                insert_slot_component_on_entity(SLOT, SIZE, entity, reflected);
            },
            apply: |mut entity: EntityMut, reflected| {
                let Some(meta) = slot_core(SLOT) else {
                    return;
                };
                if meta.is_immutable {
                    return;
                }
                if let Ok(mut mut_untyped) = entity.get_mut_by_id(meta.component_id) {
                    let rfp = <ReflectFromPtr as CreateTypeData<
                        BamFieldReflectSlot<SLOT, SIZE>,
                    >>::create_type_data(());
                    // SAFETY: `meta.component_id` has `component_size >= SIZE`, `align >= 1`, and
                    // `mut_untyped` holds exclusive mutable access to the live component.
                    let target = unsafe { rfp.ptr_as_reflect_mut(mut_untyped.as_mut()) };
                    let _ = target.try_apply(reflected);
                }
            },
            apply_or_insert_mapped:
                |entity,
                 reflected,
                 _registry,
                 _mapper: &mut dyn EntityMapper,
                 _mode: RelationshipHookMode| {
                    let Some(meta) = slot_core(SLOT) else {
                        return;
                    };
                    if !verify_world_component_info(entity.world(), meta) {
                        return;
                    }
                    if !meta.is_immutable && entity.contains_id(meta.component_id) {
                        if let Ok(mut mut_untyped) = entity.get_mut_by_id(meta.component_id) {
                            let rfp = <ReflectFromPtr as CreateTypeData<
                                BamFieldReflectSlot<SLOT, SIZE>,
                            >>::create_type_data(());
                            // SAFETY: Verified `meta.component_id` layout and exclusive mutable borrow.
                            let target = unsafe { rfp.ptr_as_reflect_mut(mut_untyped.as_mut()) };
                            let _ = target.try_apply(reflected);
                        }
                    } else {
                        insert_slot_component_on_entity(SLOT, SIZE, entity, reflected);
                    }
                },
            remove: |entity| {
                if let Some(meta) = slot_core(SLOT)
                    && verify_world_component_info(entity.world(), meta)
                {
                    entity.remove_by_id(meta.component_id);
                }
            },
            take: |entity| {
                let meta = slot_core(SLOT)?;
                if !meta.is_abi_pod
                    || meta.has_drop
                    || meta.is_resource
                    || !verify_world_component_info(entity.world(), meta)
                {
                    return None;
                }
                let src_ptr = entity.get_by_id(meta.component_id).ok()?;
                // SAFETY: `meta.component_size >= SIZE`; copying as `MaybeUninit<u8>` into an
                // 8-byte-aligned `OwnedBamFieldStruct` preserves padding and satisfies field alignment.
                let owned = unsafe {
                    OwnedBamFieldStruct::from_raw_bytes(
                        SLOT,
                        src_ptr.as_ptr().cast::<MaybeUninit<u8>>(),
                        SIZE,
                    )
                };
                entity.remove_by_id(meta.component_id);
                Some(Box::new(owned))
            },
            contains: |entity: FilteredEntityRef| {
                slot_core(SLOT).is_some_and(|meta| entity.contains_id(meta.component_id))
            },
            reflect: |entity: FilteredEntityRef<'_, '_>| {
                let meta = slot_core(SLOT)?;
                let ptr = entity.get_by_id(meta.component_id)?;
                let rfp = <ReflectFromPtr as CreateTypeData<
                    BamFieldReflectSlot<SLOT, SIZE>,
                >>::create_type_data(());
                // SAFETY: `meta.component_id` was validated for `SLOT` with `component_size >= SIZE`
                // and `align >= 1`; `ptr` borrows live component storage for the entity's lifetime.
                Some(unsafe { rfp.ptr_as_reflect(ptr) })
            },
            reflect_mut: |entity: FilteredEntityMut<'_, '_>| {
                let meta = slot_core(SLOT)?;
                if meta.is_immutable {
                    return None;
                }
                let mut_untyped = entity.into_mut_by_id(meta.component_id)?;
                let rfp = <ReflectFromPtr as CreateTypeData<
                    BamFieldReflectSlot<SLOT, SIZE>,
                >>::create_type_data(());
                // SAFETY: `meta.component_id` was validated for `SLOT` with `component_size >= SIZE`,
                // `!meta.is_immutable`, and `into_mut_by_id` grants exclusive mutable access.
                Some(mut_untyped.map_unchanged(|ptr| unsafe { rfp.ptr_as_reflect_mut(ptr) }))
            },
            map_entities: |_reflect, _mapper| {},
            reflect_unchecked_mut: |entity: UnsafeEntityCell<'_>| {
                let meta = slot_core(SLOT)?;
                if meta.is_immutable {
                    return None;
                }
                // SAFETY: Caller of `reflect_unchecked_mut` guarantees exclusive mutable access.
                let mut_untyped = unsafe { entity.get_mut_by_id(meta.component_id).ok()? };
                let rfp = <ReflectFromPtr as CreateTypeData<
                    BamFieldReflectSlot<SLOT, SIZE>,
                >>::create_type_data(());
                // SAFETY: `meta.component_id` layout verified for `SLOT`; exclusive access guaranteed by caller.
                Some(mut_untyped.map_unchanged(|ptr| unsafe { rfp.ptr_as_reflect_mut(ptr) }))
            },
            copy: |src_world, dst_world, src_ent, dst_ent, _registry| {
                copy_slot_component_between_worlds(SLOT, src_world, dst_world, src_ent, dst_ent);
            },
            register_component: |_world| slot_core_expect(SLOT).component_id,
        })
    }

    fn init_slot_type_info<const SLOT: usize, const SIZE: usize>(
        named_fields: &[NamedField],
    ) -> &'static TypeInfo {
        let info = StructInfo::new::<BamFieldReflectSlot<SLOT, SIZE>>(named_fields);
        Box::leak(Box::new(TypeInfo::Struct(info)))
    }

    fn build_slot_registration<const SLOT: usize, const SIZE: usize>(
        is_resource: bool,
    ) -> TypeRegistration {
        let mut reg = TypeRegistration::of::<BamFieldReflectSlot<SLOT, SIZE>>();
        reg.register_type_data::<ReflectFromPtr, BamFieldReflectSlot<SLOT, SIZE>>();
        reg.insert(build_slot_reflect_component::<SLOT, SIZE>());
        if is_resource {
            reg.insert(ReflectResource);
        }
        reg
    }

    const fn make_slot_descriptor<const SLOT: usize, const SIZE: usize>() -> SlotDescriptor {
        SlotDescriptor {
            slot_index: SLOT,
            slot_size: SIZE,
            type_id: TypeId::of::<BamFieldReflectSlot<SLOT, SIZE>>,
            init_type_info: init_slot_type_info::<SLOT, SIZE>,
            build_registration: build_slot_registration::<SLOT, SIZE>,
        }
    }

    macro_rules! define_slot_descriptors {
        (
            fours: [ $( ($b4:expr, $s4:expr) ),* $(,)? ],
            eights: [ $( ($b8:expr, $s8:expr) ),* $(,)? ],
            twos: [ $( ($b2:expr, $s2:expr) ),* $(,)? ] $(,)?
        ) => {
            static SLOT_DESCRIPTORS: [SlotDescriptor; TOTAL_SLOTS] = [
                $(
                    make_slot_descriptor::<{ $b4 }, $s4>(),
                    make_slot_descriptor::<{ $b4 + 1 }, $s4>(),
                    make_slot_descriptor::<{ $b4 + 2 }, $s4>(),
                    make_slot_descriptor::<{ $b4 + 3 }, $s4>(),
                )*
                $(
                    make_slot_descriptor::<{ $b8 }, $s8>(),
                    make_slot_descriptor::<{ $b8 + 1 }, $s8>(),
                    make_slot_descriptor::<{ $b8 + 2 }, $s8>(),
                    make_slot_descriptor::<{ $b8 + 3 }, $s8>(),
                    make_slot_descriptor::<{ $b8 + 4 }, $s8>(),
                    make_slot_descriptor::<{ $b8 + 5 }, $s8>(),
                    make_slot_descriptor::<{ $b8 + 6 }, $s8>(),
                    make_slot_descriptor::<{ $b8 + 7 }, $s8>(),
                )*
                $(
                    make_slot_descriptor::<{ $b2 }, $s2>(),
                    make_slot_descriptor::<{ $b2 + 1 }, $s2>(),
                )*
            ];
        };
    }

    const TOTAL_SLOTS: usize = 304;

    static SLOT_CELLS: [SlotCell; TOTAL_SLOTS] = [const { SlotCell::new() }; TOTAL_SLOTS];

    define_slot_descriptors! {
        // Tiny sizes 0..=3 (0..16) + extended 4-byte multiples 68..=128 (16..80) -> 80 slots
        fours: [
            (0, 0),
            (4, 1),
            (8, 2),
            (12, 3),
            (16, 68),
            (20, 72),
            (24, 76),
            (28, 80),
            (32, 84),
            (36, 88),
            (40, 92),
            (44, 96),
            (48, 100),
            (52, 104),
            (56, 108),
            (60, 112),
            (64, 116),
            (68, 120),
            (72, 124),
            (76, 128),
        ],
        // Common 4-byte multiples 4..=64 (8 slots each -> 80..208)
        eights: [
            (80, 4),
            (88, 8),
            (96, 12),
            (104, 16),
            (112, 20),
            (120, 24),
            (128, 28),
            (136, 32),
            (144, 36),
            (152, 40),
            (160, 44),
            (168, 48),
            (176, 52),
            (184, 56),
            (192, 60),
            (200, 64),
        ],
        // Larger 8-byte/64-byte multiples 136..=512 (208..234) + non-multiple-of-4 small sizes (234..292)
        twos: [
            (208, 136),
            (210, 144),
            (212, 152),
            (214, 160),
            (216, 176),
            (218, 192),
            (220, 208),
            (222, 224),
            (224, 240),
            (226, 256),
            (228, 320),
            (230, 384),
            (232, 512),
            (234, 5),
            (236, 6),
            (238, 7),
            (240, 9),
            (242, 10),
            (244, 11),
            (246, 13),
            (248, 14),
            (250, 15),
            (252, 17),
            (254, 18),
            (256, 19),
            (258, 21),
            (260, 22),
            (262, 23),
            (264, 25),
            (266, 26),
            (268, 27),
            (270, 29),
            (272, 30),
            (274, 31),
            (276, 34),
            (278, 38),
            (280, 42),
            (282, 46),
            (284, 50),
            (286, 54),
            (288, 58),
            (290, 62),
            (292, 168),
            (294, 184),
            (296, 200),
            (298, 216),
            (300, 232),
            (302, 248),
        ],
    }

    const _: () = {
        let mut i = 0;
        while i < TOTAL_SLOTS {
            assert!(SLOT_DESCRIPTORS[i].slot_index == i);
            i += 1;
        }
    };

    static SLOT_ALLOCATOR: OnceLock<Mutex<SlotAllocator>> = OnceLock::new();

    fn allocator() -> &'static Mutex<SlotAllocator> {
        SLOT_ALLOCATOR.get_or_init(|| {
            Mutex::new(SlotAllocator {
                by_key: HashMap::default(),
                by_type_id: HashMap::default(),
                used: vec![false; TOTAL_SLOTS],
            })
        })
    }

    pub(super) fn slot_core_for_type_id(type_id: TypeId) -> Option<&'static BamReflectSlotCore> {
        let idx = {
            let guard = allocator()
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            guard.by_type_id.get(&type_id).copied()?
        };
        SLOT_CELLS.get(idx)?.core.get()
    }

    pub(super) fn slot_matches_component_info(
        type_id: TypeId,
        component_id: ComponentId,
        info: &ComponentInfo,
        require_mutable: bool,
    ) -> bool {
        let Some(meta) = slot_core_for_type_id(type_id) else {
            return false;
        };
        if meta.component_id != component_id {
            return false;
        }
        if require_mutable && (meta.is_immutable || !info.mutable()) {
            return false;
        }
        info.layout().size() == meta.component_size
            && info.layout().align() == meta.component_align
            && info.layout().size() >= meta.slot_size
            && info.name().to_string() == meta.expected_component_name.as_ref()
    }

    /// Allocates (or reuses) a size-matched [`BamFieldReflectSlot`] carrying dynamic `StructInfo`,
    /// `ReflectFromPtr`, `ReflectComponent`, and (for resources) `ReflectResource`, and registers
    /// it into `world`'s [`AppTypeRegistry`].
    pub(super) fn register_type_in_world(
        world: &mut World,
        entry: &RegisteredComponent,
        validated_fields: &[BamFieldDescriptor],
    ) -> Result<Option<TypeId>, FfiResult> {
        let inplace_fields: Vec<&BamFieldDescriptor> = validated_fields
            .iter()
            .filter(|f| f.pod_kind.is_some())
            .collect();
        if inplace_fields.is_empty() || inplace_fields.iter().any(|f| !f.is_inplace_reflectable) {
            return Ok(None);
        }

        let Some(comp_info) = world.components().get_info(entry.component_id) else {
            return Err(FfiResult::INVALID_COMPONENT);
        };
        if comp_info.layout().size() != entry.size || comp_info.layout().align() != entry.align {
            return Err(FfiResult::TYPE_MISMATCH);
        }
        let expected_component_name = comp_info.name().to_string();

        let max_field_end = inplace_fields
            .iter()
            .filter_map(|f| f.offset.checked_add(f.size))
            .max()
            .unwrap_or(0);
        if max_field_end > entry.size {
            return Err(FfiResult::OUT_OF_RANGE);
        }

        let key = BamReflectSlotKey {
            component_id: entry.component_id,
            type_path: entry.name.to_string(),
            expected_component_name: expected_component_name.clone(),
            component_size: entry.size,
            component_align: entry.align,
            is_immutable: entry.is_immutable(),
            is_resource: entry.is_resource(),
            is_abi_pod: entry.is_abi_pod,
            has_drop: entry.drop_fn.is_some() || entry.drop_slot.is_some(),
            fields: inplace_fields
                .iter()
                .map(|f| (f.name.clone(), f.pod_kind.unwrap(), f.offset, f.size))
                .collect(),
        };

        let slot_idx = {
            let mut guard = allocator()
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(&existing) = guard.by_key.get(&key) {
                Some(existing)
            } else {
                // Prefer exact size match, otherwise the largest unused slot covering `[0..max_field_end)`
                // while staying `<= entry.size` so references never outrun component storage.
                let exact_match = SLOT_DESCRIPTORS
                    .iter()
                    .enumerate()
                    .find(|(idx, d)| !guard.used[*idx] && d.slot_size == entry.size)
                    .map(|(idx, _)| idx);
                let chosen = exact_match.or_else(|| {
                    SLOT_DESCRIPTORS
                        .iter()
                        .enumerate()
                        .filter(|(idx, d)| {
                            !guard.used[*idx]
                                && d.slot_size >= max_field_end
                                && d.slot_size <= entry.size
                        })
                        .max_by_key(|(_, d)| d.slot_size)
                        .map(|(idx, _)| idx)
                });
                if let Some(idx) = chosen {
                    let desc = &SLOT_DESCRIPTORS[idx];
                    debug_assert_eq!(desc.slot_index, idx);
                    let type_path_static: &'static str =
                        Box::leak(entry.name.to_string().into_boxed_str());
                    let (short_path, crate_name, module_path) =
                        parse_type_path_parts(type_path_static);

                    let mut slot_fields = Vec::with_capacity(inplace_fields.len());
                    let mut field_indices = HashMap::with_capacity(inplace_fields.len());
                    let mut named_fields = Vec::with_capacity(inplace_fields.len());
                    for (field_ord, f) in inplace_fields.iter().enumerate() {
                        let name_static: &'static str = Box::leak(f.name.clone().into_boxed_str());
                        let pk = f.pod_kind.unwrap();
                        slot_fields.push(BamReflectSlotField {
                            name: name_static,
                            pod_kind: pk,
                            offset: f.offset,
                            size: f.size,
                        });
                        field_indices.insert(name_static, field_ord);
                        named_fields.push(make_named_field(name_static, pk));
                    }

                    let core_meta = BamReflectSlotCore {
                        type_path: type_path_static,
                        short_path,
                        crate_name,
                        module_path,
                        expected_component_name: expected_component_name.into_boxed_str(),
                        component_id: entry.component_id,
                        component_size: entry.size,
                        component_align: entry.align,
                        slot_size: desc.slot_size,
                        is_immutable: entry.is_immutable(),
                        is_resource: entry.is_resource(),
                        is_abi_pod: entry.is_abi_pod,
                        has_drop: entry.drop_fn.is_some() || entry.drop_slot.is_some(),
                        fields: slot_fields.into_boxed_slice(),
                        field_indices,
                    };
                    let _ = SLOT_CELLS[idx].core.set(core_meta);
                    let ti = (desc.init_type_info)(&named_fields);
                    let _ = SLOT_CELLS[idx].type_info.set(ti);

                    guard.used[idx] = true;
                    guard.by_type_id.insert((desc.type_id)(), idx);
                    guard.by_key.insert(key, idx);
                }
                chosen
            }
        };

        let Some(idx) = slot_idx else {
            return Ok(None);
        };
        let desc = &SLOT_DESCRIPTORS[idx];
        let type_id = (desc.type_id)();

        if !world.contains_resource::<AppTypeRegistry>() {
            world.insert_resource(AppTypeRegistry::default());
        }
        let app_types = world.resource::<AppTypeRegistry>().clone();
        let mut types = app_types.write();
        if !types.contains(type_id) {
            let registration = (desc.build_registration)(entry.is_resource());
            if types.get_with_type_path(entry.name.as_ref()).is_some() {
                types.overwrite_registration(registration);
            } else {
                types.add_registration(registration);
            }
        }
        Ok(Some(type_id))
    }
}

/// Registers a dynamic `StructInfo`, [`ReflectFromPtr`], [`ReflectComponent`], and (when
/// `entry.is_resource()`) [`ReflectResource`] adapter into `world`'s [`AppTypeRegistry`] for any
/// component or resource carrying validated POD [`bevy_abi_mod_sys::BamFieldDesc`] metadata.
///
/// Returns `Ok(Some(type_id))` when an in-place zero-copy `ReflectFromPtr` adapter was registered,
/// `Ok(None)` when the component has no in-place-aligned POD fields or exceeds the inline slot pool
/// (in which case callers can still use [`read_entity_bam_field_bytes`] /
/// [`write_entity_bam_field_bytes`]), or `Err(FfiResult)` when descriptors are invalid.
#[cfg(feature = "bridge-reflect")]
pub fn register_bam_field_reflect_type(
    world: &mut World,
    component: BamComponent,
) -> Result<Option<core::any::TypeId>, FfiResult> {
    let entry = {
        let registry = world
            .get_resource::<ModRegistry>()
            .ok_or(FfiResult::INVALID_STATE)?;
        registry
            .get_by_handle(component)
            .cloned()
            .ok_or(FfiResult::INVALID_COMPONENT)?
    };
    if entry.is_message() || entry.fields.is_empty() {
        return Ok(None);
    }
    let validated = extract_validated_bam_fields(&entry)?;
    bam_field_reflect_pool::register_type_in_world(world, &entry, &validated)
}

/// Safely borrows `&dyn Reflect` from `entity` for `component_id` using `reflect_from_ptr`,
/// verifying that `component_id`'s runtime `TypeId` (or bound declarative `BamFieldDesc` reflect
/// slot layout) in `world` matches `reflect_from_ptr.type_id()`.
#[cfg(feature = "bridge-reflect")]
pub fn reflect_entity_component<'w>(
    world: &'w World,
    entity: EntityRef<'w>,
    component_id: ComponentId,
    reflect_from_ptr: &ReflectFromPtr,
) -> Option<&'w dyn Reflect> {
    let info = world.components().get_info(component_id)?;
    let matches_native = info.type_id() == Some(reflect_from_ptr.type_id());
    let matches_dynamic = !matches_native
        && bam_field_reflect_pool::slot_matches_component_info(
            reflect_from_ptr.type_id(),
            component_id,
            info,
            false,
        );
    if !matches_native && !matches_dynamic {
        return None;
    }
    let ptr = entity.get_by_id(component_id).ok()?;
    // SAFETY: Either `info.type_id() == Some(reflect_from_ptr.type_id())`, or `reflect_from_ptr`
    // belongs to a size/alignment-validated `BamFieldReflectSlot` bound to `component_id`;
    // `ptr` is valid, aligned, and borrowed for `'w`.
    Some(unsafe { reflect_from_ptr.ptr_as_reflect(ptr) })
}

/// Safely borrows `Mut<'w, dyn Reflect>` from `entity` for `component_id` using `reflect_from_ptr`,
/// verifying that `component_id`'s runtime `TypeId` (or bound declarative `BamFieldDesc` reflect
/// slot layout & mutability) matches `reflect_from_ptr.type_id()`.
#[cfg(feature = "bridge-reflect")]
pub fn reflect_entity_component_mut<'w>(
    entity: &'w mut EntityWorldMut<'_>,
    component_id: ComponentId,
    reflect_from_ptr: &ReflectFromPtr,
) -> Option<Mut<'w, dyn Reflect>> {
    let info = entity.world().components().get_info(component_id)?;
    if !info.mutable() {
        return None;
    }
    let matches_native = info.type_id() == Some(reflect_from_ptr.type_id());
    let matches_dynamic = !matches_native
        && bam_field_reflect_pool::slot_matches_component_info(
            reflect_from_ptr.type_id(),
            component_id,
            info,
            true,
        );
    if !matches_native && !matches_dynamic {
        return None;
    }
    let mut_untyped = entity.get_mut_by_id(component_id).ok()?;
    // SAFETY: Verified `component_id` matches `reflect_from_ptr` (native `TypeId` or validated
    // mutable `BamFieldReflectSlot`), and `EntityWorldMut` holds exclusive mutable access for `'w`.
    Some(mut_untyped.map_unchanged(|ptr| unsafe { reflect_from_ptr.ptr_as_reflect_mut(ptr) }))
}

/// Safely borrows `&dyn Reflect` from a singleton resource `component_id` in `world` using
/// `reflect_from_ptr` (supporting both native Rust `Reflect` resources and declarative
/// `BamFieldDesc`-described resources).
#[cfg(feature = "bridge-reflect")]
pub fn reflect_resource_component<'w>(
    world: &'w World,
    component_id: ComponentId,
    reflect_from_ptr: &ReflectFromPtr,
) -> Option<&'w dyn Reflect> {
    let info = world.components().get_info(component_id)?;
    let matches_native = info.type_id() == Some(reflect_from_ptr.type_id());
    let matches_dynamic = !matches_native
        && bam_field_reflect_pool::slot_matches_component_info(
            reflect_from_ptr.type_id(),
            component_id,
            info,
            false,
        );
    if !matches_native && !matches_dynamic {
        return None;
    }
    let ptr = world.get_resource_by_id(component_id)?;
    // SAFETY: Verified `component_id` matches `reflect_from_ptr` and `ptr` borrows live resource
    // storage for `'w`.
    Some(unsafe { reflect_from_ptr.ptr_as_reflect(ptr) })
}

/// Safely borrows `Mut<'w, dyn Reflect>` from a mutable singleton resource `component_id` in
/// `world` using `reflect_from_ptr`.
#[cfg(feature = "bridge-reflect")]
pub fn reflect_resource_component_mut<'w>(
    world: &'w mut World,
    component_id: ComponentId,
    reflect_from_ptr: &ReflectFromPtr,
) -> Option<Mut<'w, dyn Reflect>> {
    let info = world.components().get_info(component_id)?;
    if !info.mutable() {
        return None;
    }
    let matches_native = info.type_id() == Some(reflect_from_ptr.type_id());
    let matches_dynamic = !matches_native
        && bam_field_reflect_pool::slot_matches_component_info(
            reflect_from_ptr.type_id(),
            component_id,
            info,
            true,
        );
    if !matches_native && !matches_dynamic {
        return None;
    }
    let mut_untyped = world.get_resource_mut_by_id(component_id)?;
    // SAFETY: Verified `component_id` matches `reflect_from_ptr` (mutable), and `&'w mut World`
    // holds exclusive mutable access to the resource for `'w`.
    Some(mut_untyped.map_unchanged(|ptr| unsafe { reflect_from_ptr.ptr_as_reflect_mut(ptr) }))
}

/// Ensures `target_world` (e.g. a Bevy `SubApp` / `RenderApp` world) has a registered
/// [`ComponentId`] matching `component` from `source_world`'s [`ModRegistry`], returning the
/// target `ComponentId`.
///
/// For dynamic POD components (or components cloned into the sub-app without foreign `drop_fn`
/// ownership), registers a matching POD `ComponentDescriptor` in `target_world` and records it in
/// `target_world`'s [`ModRegistry`] if absent.
pub fn ensure_component_in_target_world(
    source_world: &World,
    target_world: &mut World,
    component: BamComponent,
) -> Result<ComponentId, FfiResult> {
    let src_reg = source_world
        .get_resource::<ModRegistry>()
        .ok_or(FfiResult::INVALID_STATE)?;
    let entry = src_reg
        .get_by_handle(component)
        .cloned()
        .ok_or(FfiResult::INVALID_COMPONENT)?;

    if !target_world.contains_resource::<ModRegistry>() {
        target_world.insert_resource(ModRegistry::new());
    }
    if let Some(existing) = target_world
        .resource::<ModRegistry>()
        .get_by_uuid(&entry.uuid)
    {
        return Ok(existing.component_id);
    }
    if let Some(existing) = target_world
        .resource::<ModRegistry>()
        .get_by_name(&entry.name)
    {
        if existing.size == entry.size && existing.align == entry.align {
            return Ok(existing.component_id);
        }
        return Err(FfiResult::TYPE_MISMATCH);
    }

    // If `source_world` registered a native Rust type (`TypeId` present on `ComponentInfo`) that
    // was already registered in `target_world`, reuse that exact `ComponentId`.
    if let Some(src_info) = source_world.components().get_info(entry.component_id)
        && let Some(tid) = src_info.type_id()
        && let Some(existing_tid_cid) = target_world.components().get_id(tid)
    {
        return Ok(existing_tid_cid);
    }

    // Refuse registering foreign `drop_fn`-bearing components directly in a `SubApp` without a
    // dedicated `WorldDropGuard` binding; POD / cloneable extracted representations have no `drop_fn`.
    if entry.drop_fn.is_some() {
        return Err(FfiResult::UNSUPPORTED);
    }

    let layout = Layout::from_size_align(entry.size, entry.align.max(1))
        .map_err(|_| FfiResult::INVALID_ARGUMENT)?;
    let storage_type = if entry.is_sparse_set() {
        StorageType::SparseSet
    } else {
        StorageType::Table
    };
    // SAFETY: `layout` is valid and `drop` is `None` (verified `entry.drop_fn.is_none()` above).
    let bevy_desc = unsafe {
        ComponentDescriptor::new_with_layout(
            entry.name.to_string(),
            storage_type,
            layout,
            None,
            !entry.is_immutable(),
            entry.is_summary_tick(),
            ComponentCloneBehavior::Default,
            None,
        )
    };
    let target_cid = target_world.register_component_with_descriptor(bevy_desc);
    let desc = bevy_abi_mod_sys::ComponentDesc::new(
        core::ptr::null(),
        entry.uuid,
        entry.size,
        entry.align,
    );
    let _ = target_world
        .resource_mut::<ModRegistry>()
        .register_dynamic_component_owned(target_cid, &desc, &entry.name, entry.owner, None);
    Ok(target_cid)
}

/// Safely extracts (copies or clones via `ComponentCloneFn`) `component` from `source_entity` in
/// `source_world` onto `target_entity` in `target_world` (`SubApp` / `RenderApp` world) without
/// any `unsafe` in the caller.
///
/// Returns `Ok(true)` if `source_entity` carried `component` and it was written onto
/// `target_entity`, or `Ok(false)` if `source_entity` did not carry `component`.
pub fn extract_component_between_worlds(
    source_world: &World,
    target_world: &mut World,
    source_entity: Entity,
    target_entity: Entity,
    component: BamComponent,
) -> Result<bool, FfiResult> {
    let src_reg = source_world
        .get_resource::<ModRegistry>()
        .ok_or(FfiResult::INVALID_STATE)?;
    let entry = src_reg
        .get_by_handle(component)
        .cloned()
        .ok_or(FfiResult::INVALID_COMPONENT)?;
    if entry.is_resource() || entry.is_message() {
        return Err(FfiResult::INVALID_COMPONENT);
    }

    let Ok(src_ent) = source_world.get_entity(source_entity) else {
        return Ok(false);
    };
    let Ok(src_ptr) = src_ent.get_by_id(entry.component_id) else {
        return Ok(false);
    };
    if target_world.get_entity(target_entity).is_err() {
        return Err(FfiResult::INVALID_ENTITY);
    }

    // Host typed `RawInsertable` fast path when `target_world` uses the native Rust component `T`.
    if !entry.is_dynamic
        && entry.drop_fn.is_none()
        && let Some(insert_raw) = entry.insert_raw_fn
    {
        let buf = AlignedScratchBuffer::new_zeroed(entry.size, entry.align)?;
        if entry.size > 0 {
            // SAFETY: Copying as `MaybeUninit<u8>` preserves any struct padding bytes without UB.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    src_ptr.as_ptr().cast::<MaybeUninit<u8>>(),
                    buf.ptr.cast::<MaybeUninit<u8>>(),
                    entry.size,
                );
            }
        }
        // SAFETY: `buf` holds `entry.size` aligned bytes copied from a live `T` in `source_world`.
        unsafe {
            insert_raw(target_world, target_entity, buf.non_null_u8().as_ptr());
        }
        return Ok(true);
    }

    let target_cid = ensure_component_in_target_world(source_world, target_world, component)?;
    let layout = Layout::from_size_align(entry.size, entry.align.max(1))
        .map_err(|_| FfiResult::INVALID_ARGUMENT)?;
    let buf = AlignedScratchBuffer::new_zeroed(entry.size, entry.align)?;

    let active_clone: Option<(ComponentCloneFn, *mut c_void)> = source_world
        .get_resource::<DynamicComponentHookMap>()
        .and_then(|m| m.active_clone_binding(entry.component_id, layout));

    if let Some((clone_fn, user_data)) = active_clone {
        let s_void = if entry.size == 0 {
            NonNull::<u8>::dangling()
                .as_ptr()
                .cast::<c_void>()
                .cast_const()
        } else {
            src_ptr.as_ptr().cast::<c_void>()
        };
        // SAFETY: `s_void` points to a live source component and `buf` holds `entry.size` aligned
        // writable bytes; `active_clone_binding` verified `is_active` and matching `layout`.
        let st = unsafe { clone_fn(s_void, buf.as_mut_c_void(), user_data) };
        if st.is_err() {
            return Err(st);
        }
    } else if entry.is_abi_pod && entry.drop_fn.is_none() {
        if entry.size > 0 {
            // SAFETY: `entry.is_abi_pod` with no `drop_fn` is bitwise copyable across `entry.size` bytes.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    src_ptr.as_ptr().cast::<MaybeUninit<u8>>(),
                    buf.ptr.cast::<MaybeUninit<u8>>(),
                    entry.size,
                );
            }
        }
    } else {
        return Err(FfiResult::UNSUPPORTED);
    }

    let mut dst_ent = target_world
        .get_entity_mut(target_entity)
        .map_err(|_| FfiResult::INVALID_ENTITY)?;
    let nn = buf.non_null_u8();
    // SAFETY: `nn` holds `entry.size` aligned initialized bytes matching `target_cid`.
    let owning = unsafe { OwningPtr::new(nn) };
    // SAFETY: `target_cid` was registered in `target_world` with `(entry.size, entry.align)`.
    unsafe {
        dst_ent.insert_by_id(target_cid, owning);
    }
    Ok(true)
}

/// Safely extracts a POD singleton `resource` (`IsResource`) from `source_world` into
/// `target_world` (`SubApp` / `RenderApp` world).
///
/// Returns `Ok(true)` when `resource` existed in `source_world` and was written into
/// `target_world`, or `Ok(false)` when `source_world` did not contain `resource`.
pub fn extract_resource_between_worlds(
    source_world: &World,
    target_world: &mut World,
    resource: BamComponent,
) -> Result<bool, FfiResult> {
    let src_reg = source_world
        .get_resource::<ModRegistry>()
        .ok_or(FfiResult::INVALID_STATE)?;
    let entry = src_reg
        .get_by_handle(resource)
        .cloned()
        .ok_or(FfiResult::INVALID_COMPONENT)?;
    if !entry.is_resource() {
        return Err(FfiResult::INVALID_COMPONENT);
    }
    if !entry.is_abi_pod || entry.drop_fn.is_some() {
        return Err(FfiResult::UNSUPPORTED);
    }
    let Some(src_ptr) = source_world.get_resource_by_id(entry.component_id) else {
        return Ok(false);
    };

    let target_cid = ensure_component_in_target_world(source_world, target_world, resource)?;
    let buf = AlignedScratchBuffer::new_zeroed(entry.size, entry.align)?;
    if entry.size > 0 {
        // SAFETY: `entry.is_abi_pod` with no `drop_fn` is bitwise copyable across `entry.size` bytes.
        unsafe {
            core::ptr::copy_nonoverlapping(
                src_ptr.as_ptr().cast::<MaybeUninit<u8>>(),
                buf.ptr.cast::<MaybeUninit<u8>>(),
                entry.size,
            );
        }
    }

    if let Some(mut dst_mut) = target_world.get_resource_mut_by_id(target_cid) {
        if entry.size > 0 {
            let ptr_mut = dst_mut.as_mut();
            // SAFETY: Both buffers have `entry.size` aligned POD bytes and do not overlap.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    buf.ptr.cast::<MaybeUninit<u8>>(),
                    ptr_mut.as_ptr().cast::<MaybeUninit<u8>>(),
                    entry.size,
                );
            }
        }
        return Ok(true);
    }

    let res_ent = target_world.spawn(IsResource::new(target_cid)).id();
    let mut ent_mut = target_world
        .get_entity_mut(res_ent)
        .map_err(|_| FfiResult::INVALID_STATE)?;
    let nn = buf.non_null_u8();
    // SAFETY: `nn` holds `entry.size` aligned POD bytes matching `target_cid`.
    let owning = unsafe { OwningPtr::new(nn) };
    // SAFETY: `target_cid` was registered in `target_world` with `(entry.size, entry.align)`.
    unsafe {
        ent_mut.insert_by_id(target_cid, owning);
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::ComponentOptions;
    use bevy_abi_mod_macros::ModComponent;
    use bevy_abi_mod_sys::{ComponentDesc, ComponentUuid};
    use bevy_ecs::component::Component;

    #[derive(Component, Default, Debug, PartialEq, ModComponent)]
    #[mod_component(name = "bridge_test::BioCard")]
    struct BioCard {
        title: String,
        level: u32,
        active: bool,
    }

    #[repr(C)]
    #[derive(Component, Clone, Copy, Default, Debug, PartialEq, ModComponent)]
    #[mod_component(name = "bridge_test::TintUniform")]
    struct TintUniform {
        rgba: [f32; 4],
    }

    #[test]
    fn witness_field_introspection_export_apply_and_insert_work_under_miri() {
        let mut world = World::new();
        let cid = world.register_component::<BioCard>();
        let mut reg = ModRegistry::new();
        let handle = reg
            .register_component::<BioCard>(cid, ComponentOptions::new().with_default())
            .unwrap();
        world.insert_resource(reg);

        let fields_meta = inspect_component_witness_fields(&world, handle).unwrap();
        assert_eq!(fields_meta.len(), 3);
        assert_eq!(fields_meta[0].name, "title");
        assert_eq!(fields_meta[0].type_tag, "String");
        assert_eq!(fields_meta[1].name, "level");
        assert_eq!(fields_meta[1].type_tag, "u32");
        assert_eq!(fields_meta[2].name, "active");
        assert_eq!(fields_meta[2].type_tag, "bool");

        let ent = world.spawn_empty().id();
        let init_fields = vec![
            ExportedFieldRecord {
                field_name: "title".to_owned(),
                type_tag: "String".to_owned(),
                bytes: b"Alchemist".to_vec(),
            },
            ExportedFieldRecord {
                field_name: "level".to_owned(),
                type_tag: "u32".to_owned(),
                bytes: 42u32.to_ne_bytes().to_vec(),
            },
            ExportedFieldRecord {
                field_name: "active".to_owned(),
                type_tag: "bool".to_owned(),
                bytes: vec![1u8],
            },
        ];
        insert_or_apply_entity_component_fields(&mut world, ent, handle, &init_fields).unwrap();
        assert_eq!(
            world.entity(ent).get::<BioCard>().unwrap(),
            &BioCard {
                title: "Alchemist".to_owned(),
                level: 42,
                active: true,
            }
        );

        let exported = export_entity_component_fields(&world, ent, handle).unwrap();
        assert_eq!(exported.len(), 3);
        assert_eq!(exported[0].bytes, b"Alchemist");

        let patch = vec![ExportedFieldRecord {
            field_name: "level".to_owned(),
            type_tag: "u16".to_owned(), // lossless numeric widening u16 -> u32
            bytes: 99u16.to_ne_bytes().to_vec(),
        }];
        apply_entity_component_fields(&mut world, ent, handle, &patch).unwrap();
        assert_eq!(world.entity(ent).get::<BioCard>().unwrap().level, 99);
    }

    #[test]
    fn cross_world_extract_copies_host_and_dynamic_pod_components_and_resources() {
        let mut main_world = World::new();
        let mut sub_world = World::new();

        let tint_cid = main_world.register_component::<TintUniform>();
        let mut reg = ModRegistry::new();
        let tint_h = reg
            .register_component::<TintUniform>(tint_cid, ComponentOptions::new().with_raw_insert())
            .unwrap();

        // Register a dynamic POD component and dynamic POD resource in `main_world`.
        let dyn_layout = Layout::from_size_align(8, 4).unwrap();
        // SAFETY: Valid POD layout with `drop = None`.
        let dyn_cid = main_world.register_component_with_descriptor(unsafe {
            ComponentDescriptor::new_with_layout(
                "bridge_test::DynUvScale".to_owned(),
                StorageType::Table,
                dyn_layout,
                None,
                true,
                false,
                ComponentCloneBehavior::Default,
                None,
            )
        });
        let dyn_desc = ComponentDesc::new(
            c"bridge_test::DynUvScale".as_ptr(),
            ComponentUuid::from_parts("bridge_test::DynUvScale", 8, 4, 0),
            8,
            4,
        );
        let dyn_h = reg
            .register_dynamic_component_owned(
                dyn_cid,
                &dyn_desc,
                "bridge_test::DynUvScale",
                None,
                None,
            )
            .unwrap();

        main_world.insert_resource(reg);

        let src_e = main_world
            .spawn(TintUniform {
                rgba: [0.25, 0.5, 0.75, 1.0],
            })
            .id();
        write_entity_pod_bytes(&mut main_world, src_e, dyn_h, &[1, 2, 3, 4, 5, 6, 7, 8]).unwrap();

        let dst_e = sub_world.spawn_empty().id();
        assert!(
            extract_component_between_worlds(&main_world, &mut sub_world, src_e, dst_e, tint_h)
                .unwrap()
        );
        assert!(
            extract_component_between_worlds(&main_world, &mut sub_world, src_e, dst_e, dyn_h)
                .unwrap()
        );

        assert_eq!(
            sub_world.entity(dst_e).get::<TintUniform>().unwrap().rgba,
            [0.25, 0.5, 0.75, 1.0]
        );
        let sub_dyn_h = sub_world
            .resource::<ModRegistry>()
            .get_by_name("bridge_test::DynUvScale")
            .unwrap()
            .handle;
        assert_eq!(
            read_entity_pod_bytes(&sub_world, dst_e, sub_dyn_h).unwrap(),
            vec![1, 2, 3, 4, 5, 6, 7, 8]
        );
    }

    #[test]
    fn bam_field_desc_validation_rejects_oob_misaligned_duplicate_and_size_mismatch() {
        use bevy_abi_mod_sys::{BamFieldDesc, BamFieldKind, StrView};

        fn register_with_fields(
            world: &mut World,
            reg: &mut ModRegistry,
            name: &'static str,
            c_name: &'static core::ffi::CStr,
            size: usize,
            align: usize,
            fields: &[BamFieldDesc],
        ) -> BamComponent {
            let layout = Layout::from_size_align(size, align).unwrap();
            // SAFETY: Valid POD layout with `drop = None`.
            let cid = world.register_component_with_descriptor(unsafe {
                ComponentDescriptor::new_with_layout(
                    name.to_owned(),
                    StorageType::Table,
                    layout,
                    None,
                    true,
                    false,
                    ComponentCloneBehavior::Default,
                    None,
                )
            });
            let desc = ComponentDesc::new(
                c_name.as_ptr(),
                ComponentUuid::from_parts(name, size, align, 0),
                size,
                align,
            )
            .with_fields(fields.as_ptr(), fields.len());
            reg.register_dynamic_component_owned(cid, &desc, name, None, None)
                .unwrap()
        }

        let mut world = World::new();
        let mut reg = ModRegistry::new();

        // 1. Out-of-bounds offset (`offset + size > entry.size`)
        let oob_fields = [BamFieldDesc::new(
            BamFieldKind::U32,
            StrView::from_str("oob"),
            StrView::from_str("u32"),
            8,
            4,
            4,
        )];
        let h_oob = register_with_fields(
            &mut world,
            &mut reg,
            "val_test::Oob",
            c"val_test::Oob",
            8,
            4,
            &oob_fields,
        );
        assert_eq!(
            inspect_component_bam_fields_in_registry(&reg, h_oob),
            Err(FfiResult::OUT_OF_RANGE)
        );

        // 2. Misaligned offset (`offset % align != 0`)
        let misalign_fields = [BamFieldDesc::new(
            BamFieldKind::U32,
            StrView::from_str("bad_off"),
            StrView::from_str("u32"),
            2,
            4,
            4,
        )];
        let h_misaligned = register_with_fields(
            &mut world,
            &mut reg,
            "val_test::Misaligned",
            c"val_test::Misaligned",
            8,
            4,
            &misalign_fields,
        );
        assert_eq!(
            inspect_component_bam_fields_in_registry(&reg, h_misaligned),
            Err(FfiResult::INVALID_ARGUMENT)
        );

        // 3. Primitive size mismatch (`U32` with size 2)
        let bad_size_fields = [BamFieldDesc::new(
            BamFieldKind::U32,
            StrView::from_str("short_u32"),
            StrView::from_str("u32"),
            0,
            2,
            2,
        )];
        let h_sz = register_with_fields(
            &mut world,
            &mut reg,
            "val_test::BadSize",
            c"val_test::BadSize",
            8,
            4,
            &bad_size_fields,
        );
        assert_eq!(
            inspect_component_bam_fields_in_registry(&reg, h_sz),
            Err(FfiResult::INVALID_ARGUMENT)
        );

        // 4. Duplicate field names
        let dup_fields = [
            BamFieldDesc::new(
                BamFieldKind::F32,
                StrView::from_str("x"),
                StrView::from_str("f32"),
                0,
                4,
                4,
            ),
            BamFieldDesc::new(
                BamFieldKind::F32,
                StrView::from_str("x"),
                StrView::from_str("f32"),
                4,
                4,
                4,
            ),
        ];
        let h_dup = register_with_fields(
            &mut world,
            &mut reg,
            "val_test::Dup",
            c"val_test::Dup",
            8,
            4,
            &dup_fields,
        );
        assert_eq!(
            inspect_component_bam_fields_in_registry(&reg, h_dup),
            Err(FfiResult::INVALID_ARGUMENT)
        );

        // 5. Valid descriptor with padding + Vec3 + Entity + Bool + OPAQUE
        let valid_fields = [
            BamFieldDesc::new(
                BamFieldKind::U8,
                StrView::from_str("tag"),
                StrView::from_str("u8"),
                0,
                1,
                1,
            ),
            BamFieldDesc::new(
                BamFieldKind::F32,
                StrView::from_str("score"),
                StrView::from_str("f32"),
                4,
                4,
                4,
            ),
            BamFieldDesc::new(
                BamFieldKind::BOOL,
                StrView::from_str("active"),
                StrView::from_str("bool"),
                8,
                1,
                1,
            ),
            BamFieldDesc::new(
                BamFieldKind::NESTED,
                StrView::from_str("vel"),
                StrView::from_str("Vec3"),
                12,
                12,
                4,
            ),
            BamFieldDesc::new(
                BamFieldKind::U64,
                StrView::from_str("target"),
                StrView::from_str("Entity"),
                24,
                8,
                8,
            ),
            BamFieldDesc::new(
                BamFieldKind::OPAQUE,
                StrView::from_str("reserved"),
                StrView::from_str("Opaque"),
                32,
                8,
                8,
            ),
        ];
        let h_ok = register_with_fields(
            &mut world,
            &mut reg,
            "val_test::Valid",
            c"val_test::Valid",
            40,
            8,
            &valid_fields,
        );
        let parsed = inspect_component_bam_fields_in_registry(&reg, h_ok)
            .unwrap()
            .unwrap();
        assert_eq!(parsed.len(), 6);
        assert_eq!(parsed[3].pod_kind, Some(BamFieldPodKind::Vec3));
        assert!(parsed[3].is_inplace_reflectable);
        assert_eq!(parsed[4].pod_kind, Some(BamFieldPodKind::Entity));
        assert_eq!(parsed[5].pod_kind, None);
    }

    #[cfg(feature = "bridge-reflect")]
    #[test]
    fn bam_field_reflect_from_ptr_and_reflect_component_are_sound_under_miri() {
        use bevy_abi_mod_sys::{BamFieldDesc, BamFieldKind, ModComponent, StrView};
        use bevy_ecs::prelude::Resource;
        use bevy_ecs::reflect::{AppTypeRegistry, ReflectComponent, ReflectResource};
        use bevy_reflect::structs::DynamicStruct;
        use bevy_reflect::{GetPath, ReflectFromPtr, TypeInfo};

        #[repr(C)]
        struct ForeignBody {
            tag: u8,
            // 3 bytes implicit padding!
            score: f32,
            active: u8, // raw C99 bool byte (test foreign value `2` -> truthy!)
            // 3 bytes implicit padding!
            vel: [f32; 3],
            target_bits: u64,
        }
        assert_eq!(core::mem::size_of::<ForeignBody>(), 32);
        assert_eq!(core::mem::align_of::<ForeignBody>(), 8);

        #[repr(C)]
        #[derive(Resource)]
        struct MiriSimClock {
            dt: f32,
            tick: u32,
        }

        const CLOCK_FIELDS: &[BamFieldDesc] = &[
            BamFieldDesc::new(
                BamFieldKind::F32,
                StrView::from_str("dt"),
                StrView::from_str("f32"),
                core::mem::offset_of!(MiriSimClock, dt),
                4,
                4,
            ),
            BamFieldDesc::new(
                BamFieldKind::U32,
                StrView::from_str("tick"),
                StrView::from_str("u32"),
                core::mem::offset_of!(MiriSimClock, tick),
                4,
                4,
            ),
        ];

        impl ModComponent for MiriSimClock {
            const CANONICAL_NAME: &'static str = "miri_c99::SimClock";
            const COMPONENT_CSTR: &'static core::ffi::CStr = c"miri_c99::SimClock";
            const UUID: ComponentUuid = ComponentUuid::from_parts("miri_c99::SimClock", 8, 4, 0);
            const IS_ABI_POD: bool = true;
            const BAM_FIELDS: &'static [BamFieldDesc] = CLOCK_FIELDS;
            type VTable = ();
            fn vtable() -> &'static Self::VTable {
                &()
            }
            fn symbols() -> &'static [bevy_abi_mod_sys::SymbolEntry] {
                &[]
            }
        }

        let fields = [
            BamFieldDesc::new(
                BamFieldKind::U8,
                StrView::from_str("tag"),
                StrView::from_str("u8"),
                core::mem::offset_of!(ForeignBody, tag),
                1,
                1,
            ),
            BamFieldDesc::new(
                BamFieldKind::F32,
                StrView::from_str("score"),
                StrView::from_str("f32"),
                core::mem::offset_of!(ForeignBody, score),
                4,
                4,
            ),
            BamFieldDesc::new(
                BamFieldKind::BOOL,
                StrView::from_str("active"),
                StrView::from_str("bool"),
                core::mem::offset_of!(ForeignBody, active),
                1,
                1,
            ),
            BamFieldDesc::new(
                BamFieldKind::NESTED,
                StrView::from_str("vel"),
                StrView::from_str("Vec3"),
                core::mem::offset_of!(ForeignBody, vel),
                12,
                4,
            ),
            BamFieldDesc::new(
                BamFieldKind::U64,
                StrView::from_str("target_bits"),
                StrView::from_str("Entity"),
                core::mem::offset_of!(ForeignBody, target_bits),
                8,
                8,
            ),
        ];

        let mut world = World::new();
        world.insert_resource(AppTypeRegistry::default());
        let mut reg = ModRegistry::new();

        // Register dynamic C99-style component (no Rust `ModComponent` / no witness VTable!)
        let layout = Layout::from_size_align(32, 8).unwrap();
        // SAFETY: Valid POD layout with `drop = None`.
        let comp_id = world.register_component_with_descriptor(unsafe {
            ComponentDescriptor::new_with_layout(
                "miri_c99::ForeignBody".to_owned(),
                StorageType::Table,
                layout,
                None,
                true,
                false,
                ComponentCloneBehavior::Default,
                None,
            )
        });
        let desc = ComponentDesc::new(
            c"miri_c99::ForeignBody".as_ptr(),
            ComponentUuid::from_parts("miri_c99::ForeignBody", 32, 8, 0),
            32,
            8,
        )
        .with_fields(fields.as_ptr(), fields.len());
        let handle = reg
            .register_dynamic_component_owned(comp_id, &desc, "miri_c99::ForeignBody", None, None)
            .unwrap();

        // Also register a POD singleton resource carrying ONLY `BAM_FIELDS` (no `#[derive(Reflect)]`,
        // no witness VTable!).
        let res_cid = world.register_component::<MiriSimClock>();
        let res_handle = reg.register_resource::<MiriSimClock>(res_cid).unwrap();
        world.insert_resource(MiriSimClock {
            dt: 0.016,
            tick: 12,
        });

        world.insert_resource(reg);

        // Register dynamic `StructInfo` + `ReflectFromPtr` + `ReflectComponent`/`ReflectResource`
        let tid = register_bam_field_reflect_type(&mut world, handle)
            .unwrap()
            .expect("aligned POD struct must bind in-place ReflectFromPtr slot");
        let res_tid = register_bam_field_reflect_type(&mut world, res_handle)
            .unwrap()
            .expect("aligned POD resource must bind in-place ReflectFromPtr slot");

        // Verify `StructInfo`, `ReflectFromPtr`, `ReflectComponent`, `ReflectResource`
        let (rfp, reflect_comp) = {
            let app_types = world.resource::<AppTypeRegistry>().read();
            let type_reg = app_types.get(tid).unwrap();
            let TypeInfo::Struct(info) = type_reg.type_info() else {
                panic!("expected TypeInfo::Struct");
            };
            assert_eq!(info.type_path(), "miri_c99::ForeignBody");
            assert_eq!(info.type_path_table().ident(), Some("ForeignBody"));
            assert_eq!(info.field_len(), 5);
            assert!(type_reg.data::<ReflectResource>().is_none());

            let res_reg = app_types.get(res_tid).unwrap();
            assert!(res_reg.data::<ReflectResource>().is_some());
            (
                type_reg.data::<ReflectFromPtr>().unwrap().clone(),
                type_reg.data::<ReflectComponent>().unwrap().clone(),
            )
        };

        // Insert component with interior padding set to 0xAA and C99 truthy bool = 2
        let ent = world.spawn_empty().id();
        let mut raw_bytes = [0xAAu8; 32];
        raw_bytes[0] = 7;
        raw_bytes[4..8].copy_from_slice(&42.5f32.to_ne_bytes());
        raw_bytes[8] = 2; // non-canonical C99 true!
        raw_bytes[12..16].copy_from_slice(&1.0f32.to_ne_bytes());
        raw_bytes[16..20].copy_from_slice(&2.0f32.to_ne_bytes());
        raw_bytes[20..24].copy_from_slice(&3.0f32.to_ne_bytes());
        raw_bytes[24..32].copy_from_slice(&0x0001_0002_0003_0004u64.to_ne_bytes());
        write_entity_pod_bytes(&mut world, ent, handle, &raw_bytes).unwrap();

        // 1. Read-only `ReflectFromPtr` borrow via `reflect_entity_component`
        {
            let ent_ref = world.entity(ent);
            let r = reflect_entity_component(&world, ent_ref, comp_id, &rfp).unwrap();
            let s = r.reflect_ref().as_struct().unwrap();
            assert_eq!(
                *s.field("tag").unwrap().try_downcast_ref::<u8>().unwrap(),
                7
            );
            assert_eq!(
                *s.field("score").unwrap().try_downcast_ref::<f32>().unwrap(),
                42.5
            );
            assert!(
                *s.field("active")
                    .unwrap()
                    .try_downcast_ref::<bool>()
                    .unwrap()
            );
            assert_eq!(
                *s.field("vel")
                    .unwrap()
                    .try_downcast_ref::<[f32; 3]>()
                    .unwrap(),
                [1.0, 2.0, 3.0]
            );
            assert_eq!(
                *s.field("target_bits")
                    .unwrap()
                    .try_downcast_ref::<u64>()
                    .unwrap(),
                0x0001_0002_0003_0004u64
            );
            let dyn_val = r.to_dynamic().unwrap();
            assert_eq!(
                dyn_val.get_represented_type_info().unwrap().type_path(),
                "miri_c99::ForeignBody"
            );
        }

        // 2. Mutable `ReflectFromPtr` borrow via `reflect_entity_component_mut`
        {
            let mut ent_mut = world.entity_mut(ent);
            let mut r = reflect_entity_component_mut(&mut ent_mut, comp_id, &rfp).unwrap();
            *r.reflect_path_mut("score")
                .unwrap()
                .try_downcast_mut::<f32>()
                .unwrap() = 99.25;
            *r.reflect_path_mut("active")
                .unwrap()
                .try_downcast_mut::<bool>()
                .unwrap() = false;
            *r.reflect_path_mut("vel[1]")
                .unwrap()
                .try_downcast_mut::<f32>()
                .unwrap() = -7.5;
        }

        // Verify byte-exact mutation in raw ECS storage & untouched padding bytes
        let mutated_bytes = read_entity_pod_bytes(&world, ent, handle).unwrap();
        assert_eq!(mutated_bytes[0], 7);
        assert_eq!(&mutated_bytes[1..4], &[0xAA, 0xAA, 0xAA]);
        assert_eq!(
            f32::from_ne_bytes(mutated_bytes[4..8].try_into().unwrap()),
            99.25
        );
        assert_eq!(mutated_bytes[8], 0);
        assert_eq!(
            f32::from_ne_bytes(mutated_bytes[16..20].try_into().unwrap()),
            -7.5
        );

        // 3. `ReflectComponent::insert`, `reflect`, `take`, and `remove`
        let ent2 = world.spawn_empty().id();
        let mut init_dyn = DynamicStruct::default();
        init_dyn.insert("tag", 11u8);
        init_dyn.insert("score", 3.5f32);
        init_dyn.insert("active", true);
        init_dyn.insert("vel", [4.0f32, 5.0, 6.0]);
        init_dyn.insert("target_bits", 77u64);
        {
            let app_types = world.resource::<AppTypeRegistry>().clone();
            let types_guard = app_types.read();
            let mut e2 = world.entity_mut(ent2);
            reflect_comp.insert(&mut e2, &init_dyn, &types_guard);
        }
        let taken = {
            let mut e2 = world.entity_mut(ent2);
            reflect_comp
                .take(&mut e2)
                .expect("take should return boxed slot")
        };
        assert!(!world.entity(ent2).contains_id(comp_id));
        let taken_s = taken.reflect_ref().as_struct().unwrap();
        assert_eq!(
            *taken_s
                .field("score")
                .unwrap()
                .try_downcast_ref::<f32>()
                .unwrap(),
            3.5
        );

        // 4. Declarative `BamFieldDesc` resource reflection via `reflect_resource_component` / `_mut`
        let res_rfp = world
            .resource::<AppTypeRegistry>()
            .read()
            .get(res_tid)
            .unwrap()
            .data::<ReflectFromPtr>()
            .unwrap()
            .clone();
        {
            let r = reflect_resource_component(&world, res_cid, &res_rfp).unwrap();
            assert_eq!(
                *r.reflect_path("tick")
                    .unwrap()
                    .try_downcast_ref::<u32>()
                    .unwrap(),
                12
            );
        }
        {
            let mut r = reflect_resource_component_mut(&mut world, res_cid, &res_rfp).unwrap();
            *r.reflect_path_mut("tick")
                .unwrap()
                .try_downcast_mut::<u32>()
                .unwrap() = 55;
        }
        assert_eq!(world.resource::<MiriSimClock>().tick, 55);
        let tick_bytes = read_resource_bam_field_bytes(&world, res_handle, 4, 4).unwrap();
        assert_eq!(u32::from_ne_bytes(tick_bytes.try_into().unwrap()), 55);
    }
}
