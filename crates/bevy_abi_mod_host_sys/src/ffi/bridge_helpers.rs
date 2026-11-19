//! Safe host-side bridge helpers supporting `bridge-reflect`, `bridge-bsn`, and
//! `sub-app-extract` so `#![forbid(unsafe_code)]` crates (`bevy_abi_mod_host` /
//! `bevy_abi_mod`) never require `unsafe` blocks.
//!
//! Provides:
//! 1. **Witness Introspection & Live World Read/Write**:
//!    - [`inspect_component_witness_fields`] / [`inspect_component_witness_fields_in_registry`]
//!    - [`export_entity_component_fields`], [`export_resource_fields`]
//!    - [`apply_entity_component_fields`], [`apply_resource_fields`]
//!    - [`insert_entity_component_from_fields`], [`insert_or_apply_entity_component_fields`]
//!    - [`read_entity_pod_bytes`], [`write_entity_pod_bytes`]
//!    - Optional `bridge-reflect` helpers: `reflect_entity_component`,
//!      `reflect_entity_component_mut`
//! 2. **Cross-`World` (`SubApp`) Component & Resource Extraction**:
//!    - [`ensure_component_in_target_world`]
//!    - [`extract_component_between_worlds`]
//!    - [`extract_resource_between_worlds`]

use core::{alloc::Layout, ffi::c_void, mem::MaybeUninit, ptr::NonNull};

use bevy_abi_mod_sys::{
    BamComponent, ComponentCloneFn, ComponentDropFn, FfiBool, FfiResult, FfiSlice, StrView,
};
#[cfg(feature = "bridge-reflect")]
use bevy_ecs::{
    change_detection::Mut,
    world::{EntityRef, EntityWorldMut},
};
use bevy_ecs::{
    component::{ComponentCloneBehavior, ComponentDescriptor, ComponentId, StorageType},
    entity::Entity,
    ptr::OwningPtr,
    resource::IsResource,
    world::World,
};
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

/// Safely borrows `&dyn Reflect` from `entity` for `component_id` using `reflect_from_ptr`,
/// verifying that `component_id`'s runtime `TypeId` in `entity`'s `World` matches
/// `reflect_from_ptr.type_id()`.
#[cfg(feature = "bridge-reflect")]
pub fn reflect_entity_component<'w>(
    world: &'w World,
    entity: EntityRef<'w>,
    component_id: ComponentId,
    reflect_from_ptr: &ReflectFromPtr,
) -> Option<&'w dyn Reflect> {
    let info = world.components().get_info(component_id)?;
    if info.type_id()? != reflect_from_ptr.type_id() {
        return None;
    }
    let ptr = entity.get_by_id(component_id).ok()?;
    // SAFETY: Verified `info.type_id() == Some(reflect_from_ptr.type_id())`, and `ptr` is valid,
    // aligned, and borrowed for `'w`.
    Some(unsafe { reflect_from_ptr.ptr_as_reflect(ptr) })
}

/// Safely borrows `Mut<'w, dyn Reflect>` from `entity` for `component_id` using `reflect_from_ptr`,
/// verifying that `component_id`'s runtime `TypeId` matches `reflect_from_ptr.type_id()`.
#[cfg(feature = "bridge-reflect")]
pub fn reflect_entity_component_mut<'w>(
    entity: &'w mut EntityWorldMut<'_>,
    component_id: ComponentId,
    reflect_from_ptr: &ReflectFromPtr,
) -> Option<Mut<'w, dyn Reflect>> {
    let expected_tid = entity
        .world()
        .components()
        .get_info(component_id)?
        .type_id()?;
    if expected_tid != reflect_from_ptr.type_id() {
        return None;
    }
    let mut_untyped = entity.get_mut_by_id(component_id).ok()?;
    // SAFETY: Verified `component_id`'s `TypeId` equals `reflect_from_ptr.type_id()`, and
    // `EntityWorldMut` holds exclusive mutable access for `'w`.
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
}
