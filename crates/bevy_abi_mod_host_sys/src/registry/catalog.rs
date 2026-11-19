//! Copy-on-Write (`CoW`) component, resource, VTable, and `"bam.v0.*"` symbol catalog.

use crate::ffi::assets_scene::{HostAssetBackend, HostSceneBackend};
use crate::loader::ModId;
use crate::registry::bridge::ExtractRelationshipTargetsFn;
use crate::registry::collision::ComponentCollisionError;
use crate::sync_ptr::SymbolAddress;
use arc_swap::ArcSwap;
use bevy_abi_mod_sys::{
    AbiPod, BAM_CHILD_OF_NAME, BAM_COMPONENT_FLAG_IMMUTABLE, BAM_COMPONENT_FLAG_SPARSE_SET,
    BAM_COMPONENT_FLAG_SUMMARY_TICK, BAM_STORAGE_FLAG_ENTITY_EVENT, BAM_STORAGE_FLAG_EVENT,
    BAM_STORAGE_FLAG_IMMUTABLE, BAM_STORAGE_FLAG_MESSAGE, BAM_STORAGE_FLAG_RESOURCE,
    BAM_STORAGE_FLAG_SPARSE_SET, BAM_STORAGE_FLAG_SUMMARY_TICK, BamComponent, BamFieldDesc,
    ComponentDesc, ComponentDropFn, ComponentUuid, FfiChildOf, FfiEntity, FfiResult, FfiSlice,
    ModComponent, RawInsertable, StrView,
};
use bevy_ecs::change_detection::Tick;
use bevy_ecs::component::ComponentId;
use bevy_ecs::entity::{ContainsEntity, Entity};
use bevy_ecs::event::{EntityEvent, Event, EventKey};
use bevy_ecs::hierarchy::ChildOf;
use bevy_ecs::message::{Message, MessageCursor, Messages};
use bevy_ecs::prelude::{Component, Resource};
use bevy_ecs::world::World;
use bevy_ecs::world::unsafe_world_cell::UnsafeWorldCell;
use bevy_platform::collections::HashMap;
use core::any::Any;
use core::ffi::c_void;
use core::marker::PhantomData;
use core::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Function signature for monomorphized raw component byte insertion into a Bevy [`World`].
///
/// Takes a raw `*const u8` pointer (backed by `MaybeUninit<u8>` arena storage) rather than `&[u8]`
/// so uninitialized struct padding bytes never form a Rust `[u8]` slice.
pub type InsertRawFn = unsafe fn(world: &mut World, entity: Entity, data: *const u8);

/// Function signature for monomorphized `Default` component insertion into a Bevy [`World`].
pub type InsertDefaultFn = fn(world: &mut World, entity: Entity);

/// Monomorphized factory creating a per-system type-erased `MessageCursor<M>`.
pub type CreateMsgCursorFn = fn() -> Box<dyn Any + Send + Sync>;

/// Monomorphized batch reader advancing a per-system `MessageCursor<M>` over `Messages<M>`.
///
/// # Safety
/// - `world` must hold shared read (or write) access to `msg_res_id` (`Messages<M>`).
/// - `cursor` must have been created by the matching [`CreateMsgCursorFn`].
/// - If `max_count > 0`, `out_buf` must be writable for `max_count * size_of::<M>()` bytes.
/// - `out_count` must be non-null and writable.
pub type MsgReadBatchFn = unsafe fn(
    world: UnsafeWorldCell<'_>,
    msg_res_id: ComponentId,
    cursor: &mut dyn Any,
    out_buf: *mut u8,
    max_count: usize,
    out_count: *mut usize,
) -> FfiResult;

/// Monomorphized message writer pushing a single POD `M` into `Messages<M>`.
///
/// # Safety
/// - `world` must hold exclusive write access to `msg_res_id` (`Messages<M>`).
/// - If `data_len > 0`, `data_ptr` must be readable for `data_len` bytes.
pub type MsgSendFn = unsafe fn(
    world: UnsafeWorldCell<'_>,
    last_run: Tick,
    this_run: Tick,
    msg_res_id: ComponentId,
    data_ptr: *const u8,
    data_len: usize,
) -> FfiResult;

/// Function signature for monomorphized batch `Messages<M>` writes across the C ABI.
pub type MsgWriteBatchFn = unsafe fn(
    world: UnsafeWorldCell<'_>,
    last_run: Tick,
    this_run: Tick,
    msg_res_id: ComponentId,
    data_ptr: *const u8,
    count: usize,
) -> FfiResult;

/// Function signature for monomorphized typed `world.trigger(...)` invocation from raw guest bytes.
pub type TriggerEventFn =
    unsafe fn(world: &mut World, target: Option<Entity>, data: *const u8) -> Result<(), FfiResult>;

/// Safe function signature triggering an entity-only host `EntityEvent` (such as `bevy_scene::Ready`)
/// directly from a validated target [`Entity`] without requiring `From<Entity>`.
pub type EntityEventTriggerFn = fn(world: &mut World, entity: Entity);

/// Function signature extracting the target [`Entity`] from a raw `EntityEvent` payload.
pub type ExtractEventTargetFn = unsafe fn(data: *const u8) -> Option<Entity>;

/// ABI-stable `#[repr(transparent)]` entity wrapper implementing Bevy's [`ContainsEntity`] trait
/// for `#[event_target]` fields in `#[derive(EntityEvent, ModComponent)]` host event structs.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TargetEntity(pub FfiEntity);

impl Default for TargetEntity {
    #[inline]
    fn default() -> Self {
        Self(FfiEntity::NULL)
    }
}

impl TargetEntity {
    /// Wraps a Bevy [`Entity`] as a `TargetEntity`.
    #[inline]
    pub const fn from_entity(entity: Entity) -> Self {
        Self(FfiEntity::from_bits(entity.to_bits()))
    }

    /// Converts back to a Bevy [`Entity`], or `Entity::PLACEHOLDER` if [`FfiEntity::NULL`].
    #[inline]
    pub const fn to_entity(self) -> Entity {
        if let Some(e) = Entity::try_from_bits(self.0.to_bits()) {
            e
        } else {
            Entity::PLACEHOLDER
        }
    }
}

impl From<Entity> for TargetEntity {
    #[inline]
    fn from(entity: Entity) -> Self {
        Self::from_entity(entity)
    }
}

impl From<FfiEntity> for TargetEntity {
    #[inline]
    fn from(entity: FfiEntity) -> Self {
        Self(entity)
    }
}

impl From<TargetEntity> for FfiEntity {
    #[inline]
    fn from(entity: TargetEntity) -> Self {
        entity.0
    }
}

impl core::ops::Deref for TargetEntity {
    type Target = FfiEntity;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl ContainsEntity for TargetEntity {
    #[inline]
    fn entity(&self) -> Entity {
        self.to_entity()
    }
}

// SAFETY: `TargetEntity` is `#[repr(transparent)]` over `FfiEntity` (`u64`), has no padding or drop
// glue, and matches `ModComponent` field classification for `TargetEntity`.
unsafe impl RawInsertable for TargetEntity {}

// SAFETY: `TargetEntity` is `#[repr(transparent)]` over `FfiEntity`, which is `AbiPod`.
unsafe impl AbiPod for TargetEntity {}

fn create_msg_cursor_impl<M: Message>() -> Box<dyn Any + Send + Sync> {
    Box::new(MessageCursor::<M>::default())
}

unsafe fn msg_read_batch_impl<M: Message + AbiPod>(
    world: UnsafeWorldCell<'_>,
    msg_res_id: ComponentId,
    cursor: &mut dyn Any,
    out_buf: *mut u8,
    max_count: usize,
    out_count: *mut usize,
) -> FfiResult {
    // SAFETY: Caller guarantees `out_count` is non-null and writable.
    unsafe {
        *out_count = 0;
    }
    let Some(typed_cursor) = cursor.downcast_mut::<MessageCursor<M>>() else {
        return FfiResult::TYPE_MISMATCH;
    };
    if max_count == 0 {
        return FfiResult::OK;
    }
    if core::mem::size_of::<M>() > 0 && out_buf.is_null() {
        return FfiResult::NULL_POINTER;
    }
    // SAFETY: `MESSAGE_READ` or `MESSAGE_WRITE` on `msg_res_id` (`Messages<M>`) was declared in `FilteredAccessSet`.
    let Some(res_ptr) = (unsafe { world.get_resource_by_id(msg_res_id) }) else {
        return FfiResult::OK;
    };
    // SAFETY: `msg_res_id` was registered from `world.register_component::<Messages<M>>()`.
    let messages: &Messages<M> = unsafe { res_ptr.deref::<Messages<M>>() };
    let mut written = 0usize;
    for msg in typed_cursor.read(messages) {
        if written >= max_count {
            break;
        }
        if core::mem::size_of::<M>() > 0 {
            // SAFETY: `out_buf` is writable for `max_count * size_of::<M>()` bytes; `write_unaligned`
            // supports arbitrary guest buffer alignments cleanly.
            unsafe {
                core::ptr::write_unaligned(out_buf.cast::<M>().add(written), *msg);
            }
        }
        written += 1;
    }
    // SAFETY: `out_count` is non-null and writable.
    unsafe {
        *out_count = written;
    }
    FfiResult::OK
}

/// Pushes a constructed `msg: M` into the World's `Messages<M>` resource, stamping `this_run`.
///
/// # Safety
/// Caller must guarantee `MESSAGE_WRITE` on `msg_res_id` was declared in `FilteredAccessSet`.
unsafe fn push_message_to_world<M: Message>(
    world: UnsafeWorldCell<'_>,
    last_run: Tick,
    this_run: Tick,
    msg_res_id: ComponentId,
    msg: M,
) -> FfiResult {
    // SAFETY: `MESSAGE_WRITE` on `msg_res_id` (`Messages<M>`) was declared in `FilteredAccessSet`,
    // granting exclusive mutable access to `Messages<M>` and its resource entity.
    let Some(entity) = (unsafe { world.resource_entities() }).get(msg_res_id) else {
        return FfiResult::NOT_FOUND;
    };
    let Ok(entity_cell) = world.get_entity_with_ticks(entity, last_run, this_run) else {
        return FfiResult::NOT_FOUND;
    };
    // SAFETY: Exclusive mutable access to `Messages<M>` is guaranteed by Bevy's scheduler.
    let Ok(mut_untyped) = (unsafe { entity_cell.get_mut_by_id(msg_res_id) }) else {
        return FfiResult::NOT_FOUND;
    };
    // SAFETY: `msg_res_id` was registered for `Messages<M>`.
    let mut messages = unsafe { mut_untyped.with_type::<Messages<M>>() };
    messages.write(msg);
    FfiResult::OK
}

unsafe fn msg_send_impl<M: Message + AbiPod>(
    world: UnsafeWorldCell<'_>,
    last_run: Tick,
    this_run: Tick,
    msg_res_id: ComponentId,
    data_ptr: *const u8,
    data_len: usize,
) -> FfiResult {
    if data_len != core::mem::size_of::<M>() {
        return FfiResult::INVALID_ARGUMENT;
    }
    if data_len > 0 && data_ptr.is_null() {
        return FfiResult::NULL_POINTER;
    }
    let msg: M = if data_len == 0 {
        // SAFETY: `M: AbiPod` is zero-sized, `Copy`, and valid when zeroed.
        unsafe { core::mem::MaybeUninit::<M>::zeroed().assume_init() }
    } else {
        // SAFETY: `data_ptr` is non-null and readable for `size_of::<M>()` bytes.
        unsafe { core::ptr::read_unaligned(data_ptr.cast::<M>()) }
    };
    // SAFETY: Caller declared `MESSAGE_WRITE` on `msg_res_id`.
    unsafe { push_message_to_world(world, last_run, this_run, msg_res_id, msg) }
}

unsafe fn msg_read_batch_non_pod_impl<M: Message>(
    world: UnsafeWorldCell<'_>,
    msg_res_id: ComponentId,
    cursor: &mut dyn Any,
    out_buf: *mut u8,
    max_count: usize,
    out_count: *mut usize,
) -> FfiResult {
    // SAFETY: Caller guarantees `out_count` is non-null and writable.
    unsafe {
        *out_count = 0;
    }
    let Some(typed_cursor) = cursor.downcast_mut::<MessageCursor<M>>() else {
        return FfiResult::TYPE_MISMATCH;
    };
    if max_count == 0 {
        return FfiResult::OK;
    }
    if out_buf.is_null() {
        return FfiResult::NULL_POINTER;
    }
    // SAFETY: `MESSAGE_READ` or `MESSAGE_WRITE` on `msg_res_id` (`Messages<M>`) was declared in `FilteredAccessSet`.
    let Some(res_ptr) = (unsafe { world.get_resource_by_id(msg_res_id) }) else {
        return FfiResult::OK;
    };
    // SAFETY: `msg_res_id` was registered from `world.register_component::<Messages<M>>()`.
    let messages: &Messages<M> = unsafe { res_ptr.deref::<Messages<M>>() };
    let mut written = 0usize;
    for msg in typed_cursor.read(messages).take(max_count) {
        let msg_ptr = core::ptr::from_ref(msg).cast::<c_void>();
        // SAFETY: `out_buf` is writable for `max_count * size_of::<*const c_void>()` bytes;
        // `write_unaligned` preserves full pointer provenance regardless of `out_buf` alignment.
        #[allow(clippy::cast_ptr_alignment)]
        unsafe {
            core::ptr::write_unaligned(
                out_buf
                    .add(written * core::mem::size_of::<*const c_void>())
                    .cast::<*const c_void>(),
                msg_ptr,
            );
        }
        written += 1;
    }
    // SAFETY: `out_count` is non-null and writable.
    unsafe {
        *out_count = written;
    }
    FfiResult::OK
}

fn read_u32_framed_slice<'a>(bytes: &'a [u8], pos: &mut usize) -> Result<&'a [u8], FfiResult> {
    let end_len = pos.checked_add(4).ok_or(FfiResult::INVALID_ARGUMENT)?;
    let len_hdr = bytes
        .get(*pos..end_len)
        .ok_or(FfiResult::INVALID_ARGUMENT)?;
    let len = u32::from_le_bytes([len_hdr[0], len_hdr[1], len_hdr[2], len_hdr[3]]) as usize;
    *pos = end_len;
    let end_payload = pos.checked_add(len).ok_or(FfiResult::INVALID_ARGUMENT)?;
    let payload = bytes
        .get(*pos..end_payload)
        .ok_or(FfiResult::INVALID_ARGUMENT)?;
    *pos = end_payload;
    Ok(payload)
}

unsafe fn msg_send_non_pod_impl<M: Message + ModComponent + Default>(
    world: UnsafeWorldCell<'_>,
    last_run: Tick,
    this_run: Tick,
    msg_res_id: ComponentId,
    data_ptr: *const u8,
    data_len: usize,
) -> FfiResult {
    if data_len < 4 {
        return FfiResult::INVALID_ARGUMENT;
    }
    if data_ptr.is_null() {
        return FfiResult::NULL_POINTER;
    }
    let Some(sym) = M::symbols().iter().find(|s| {
        s.name
            .strip_prefix("bam.v0.")
            .and_then(|rest| rest.strip_suffix(".import_field"))
            == Some(M::CANONICAL_NAME)
    }) else {
        return FfiResult::UNSUPPORTED;
    };
    if sym.ptr.is_null() {
        return FfiResult::UNSUPPORTED;
    }
    type ImportFieldFn = unsafe extern "C" fn(
        dst_ptr: *mut c_void,
        field_name: StrView,
        type_tag: StrView,
        payload: FfiSlice<u8>,
    ) -> FfiResult;
    // SAFETY: Witness codegen exports `"bam.v0.<M>.import_field"` with `ImportFieldFn` signature.
    let import_fn: ImportFieldFn = unsafe { core::mem::transmute(sym.ptr) };

    // SAFETY: Caller guarantees `data_ptr` is readable for `data_len` bytes (`>= 4`).
    let bytes = unsafe { core::slice::from_raw_parts(data_ptr, data_len) };
    let field_count = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
    let mut pos = 4usize;
    let mut host_msg = M::default();

    for _ in 0..field_count {
        let name_bytes = match read_u32_framed_slice(bytes, &mut pos) {
            Ok(s) => s,
            Err(err) => return err,
        };
        let Ok(name_str) = core::str::from_utf8(name_bytes) else {
            return FfiResult::INVALID_UTF8;
        };
        let tag_bytes = match read_u32_framed_slice(bytes, &mut pos) {
            Ok(s) => s,
            Err(err) => return err,
        };
        let Ok(tag_str) = core::str::from_utf8(tag_bytes) else {
            return FfiResult::INVALID_UTF8;
        };
        let val_bytes = match read_u32_framed_slice(bytes, &mut pos) {
            Ok(s) => s,
            Err(err) => return err,
        };

        // SAFETY: `&raw mut host_msg` points to an initialized `M`, and `StrView` / `FfiSlice`
        // borrow valid UTF-8 / byte slices for the synchronous duration of `import_fn`.
        let status = unsafe {
            import_fn(
                (&raw mut host_msg).cast::<c_void>(),
                StrView::from_str(name_str),
                StrView::from_str(tag_str),
                FfiSlice::from_slice(val_bytes),
            )
        };
        if !status.is_ok() {
            return status;
        }
    }

    if pos != bytes.len() {
        return FfiResult::INVALID_ARGUMENT;
    }
    // SAFETY: Caller declared `MESSAGE_WRITE` on `msg_res_id`.
    unsafe { push_message_to_world(world, last_run, this_run, msg_res_id, host_msg) }
}

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

unsafe fn msg_write_batch_impl<M: Message + AbiPod>(
    world: UnsafeWorldCell<'_>,
    last_run: Tick,
    this_run: Tick,
    msg_res_id: ComponentId,
    data_ptr: *const u8,
    count: usize,
) -> FfiResult {
    if count == 0 {
        return FfiResult::OK;
    }
    if core::mem::size_of::<M>() > 0 && data_ptr.is_null() {
        return FfiResult::NULL_POINTER;
    }
    // SAFETY: `MESSAGE_WRITE` on `msg_res_id` (`Messages<M>`) was declared in `FilteredAccessSet`.
    let Some(entity) = (unsafe { world.resource_entities() }).get(msg_res_id) else {
        return FfiResult::NOT_FOUND;
    };
    let Ok(entity_cell) = world.get_entity_with_ticks(entity, last_run, this_run) else {
        return FfiResult::NOT_FOUND;
    };
    // SAFETY: Exclusive mutable access to `Messages<M>` is guaranteed by Bevy's scheduler.
    let Ok(mut_untyped) = (unsafe { entity_cell.get_mut_by_id(msg_res_id) }) else {
        return FfiResult::NOT_FOUND;
    };
    // SAFETY: `msg_res_id` was registered for `Messages<M>`.
    let mut messages = unsafe { mut_untyped.with_type::<Messages<M>>() };
    let msg_size = core::mem::size_of::<M>();
    if msg_size == 0 {
        for _ in 0..count {
            // SAFETY: `M: AbiPod` is zero-sized, `Copy`, and valid when zeroed.
            let zst = unsafe { core::mem::MaybeUninit::<M>::zeroed().assume_init() };
            messages.write(zst);
        }
    } else {
        for i in 0..count {
            // SAFETY: Caller verified `data_ptr` is readable for `count * size_of::<M>()` bytes.
            let msg = unsafe { core::ptr::read_unaligned(data_ptr.cast::<M>().add(i)) };
            messages.write(msg);
        }
    }
    FfiResult::OK
}

unsafe fn insert_child_of_raw(world: &mut World, entity: Entity, data: *const u8) {
    if data.is_null() {
        return;
    }
    // SAFETY: `data` points to an 8-byte `FfiChildOf` payload (`FfiEntity` parent handle).
    let raw_bits = unsafe { core::ptr::read_unaligned(data.cast::<u64>()) };
    let Some(parent) = Entity::try_from_bits(raw_bits) else {
        return;
    };
    if parent == entity || world.get_entity(parent).is_err() {
        return;
    }
    if let Ok(mut e) = world.get_entity_mut(entity) {
        e.insert(ChildOf(parent));
    }
}

unsafe fn trigger_event_impl<E>(
    world: &mut World,
    _target: Option<Entity>,
    data: *const u8,
) -> Result<(), FfiResult>
where
    E: Event<Trigger<'static>: Default> + ModComponent + Copy,
{
    if core::mem::size_of::<E>() > 0 && data.is_null() {
        return Err(FfiResult::NULL_POINTER);
    }
    let event: E = if core::mem::size_of::<E>() == 0 {
        // SAFETY: `E` is zero-sized and valid as POD event data.
        unsafe { core::mem::MaybeUninit::<E>::zeroed().assume_init() }
    } else {
        // SAFETY: `data` points to `size_of::<E>()` initialized bytes.
        unsafe { core::ptr::read_unaligned(data.cast::<E>()) }
    };
    world.trigger(event);
    Ok(())
}

unsafe fn trigger_entity_event_impl<E>(
    world: &mut World,
    target: Option<Entity>,
    data: *const u8,
) -> Result<(), FfiResult>
where
    E: EntityEvent + Event + ModComponent + Copy,
    for<'a> E::Trigger<'a>: Default,
{
    if core::mem::size_of::<E>() > 0 && data.is_null() {
        return Err(FfiResult::NULL_POINTER);
    }
    // SAFETY: `data` points to `size_of::<E>()` initialized bytes.
    let event: E = unsafe { core::ptr::read_unaligned(data.cast::<E>()) };
    let target_entity = target.unwrap_or_else(|| event.event_target());
    if !world.entities().contains(target_entity) {
        return Err(FfiResult::INVALID_ENTITY);
    }
    world.trigger(event);
    Ok(())
}

unsafe fn extract_entity_event_target_impl<E: EntityEvent + Copy>(
    data: *const u8,
) -> Option<Entity> {
    if core::mem::size_of::<E>() > 0 && data.is_null() {
        return None;
    }
    // SAFETY: `data` points to an initialized `E` value during observer execution.
    let event: E = unsafe { core::ptr::read_unaligned(data.cast::<E>()) };
    Some(event.event_target())
}

/// Monomorphized host-side `T::default()` component constructor and inserter.
fn insert_default_impl<T: Component + Default>(world: &mut World, entity: Entity) {
    if let Ok(mut e) = world.get_entity_mut(entity) {
        e.insert(T::default());
    }
}

/// Fluent registration options configuring how a host Rust [`Component`] is exposed to guest mods.
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

    /// Marks this component registration as carrying a Bevy 0.20 per-table summary tick
    /// (`BAM_STORAGE_FLAG_SUMMARY_TICK`).
    #[inline]
    pub const fn summary_tick(mut self) -> Self {
        self.extra_storage_flags |= BAM_STORAGE_FLAG_SUMMARY_TICK;
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
    if (component_flags & BAM_COMPONENT_FLAG_SUMMARY_TICK) != 0 {
        out |= BAM_STORAGE_FLAG_SUMMARY_TICK;
    }
    out
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct SymbolRecord {
    pub(crate) addr: SymbolAddress,
    pub(crate) owner: Option<ModId>,
}

/// Retained heap strings backing dynamic [`BamFieldDesc::name`] and [`BamFieldDesc::type_tag`].
pub type OwnedFieldStrings = Arc<[(Arc<str>, Arc<str>)]>;

/// Extracts ABI `0.3` `schema_fingerprint` and host-retained [`BamFieldDesc`] metadata from a
/// dynamic [`ComponentDesc`] when covered by `desc.struct_size`.
///
/// # Safety
/// - If `desc` covers `field_count` and `desc.field_count > 0`, `desc.fields` must point to
///   `desc.field_count` readable [`BamFieldDesc`] entries whose non-empty [`StrView`]s point to
///   initialized bytes.
pub(crate) unsafe fn extract_owned_dynamic_fields(
    desc: &ComponentDesc,
) -> Result<(u64, Arc<[BamFieldDesc]>, Option<OwnedFieldStrings>), (FfiResult, String)> {
    let schema_fingerprint = if desc.covers_slot(
        core::mem::offset_of!(ComponentDesc, schema_fingerprint),
        core::mem::size_of::<u64>(),
    ) {
        desc.schema_fingerprint
    } else {
        0
    };

    let has_fields = desc.covers_slot(
        core::mem::offset_of!(ComponentDesc, field_count),
        core::mem::size_of::<usize>(),
    ) && desc.field_count > 0;
    if !has_fields {
        return Ok((schema_fingerprint, Arc::from([]), None));
    }
    if desc.fields.is_null() {
        return Err((
            FfiResult::NULL_POINTER,
            "ComponentDesc `fields` pointer is null with non-zero `field_count`".to_string(),
        ));
    }

    // SAFETY: Caller guarantees `desc.fields` is readable for `desc.field_count` entries.
    let raw_fields = unsafe { core::slice::from_raw_parts(desc.fields, desc.field_count) };
    let mut string_pairs: Vec<(Arc<str>, Arc<str>)> = Vec::with_capacity(raw_fields.len());
    let mut field_descs: Vec<BamFieldDesc> = Vec::with_capacity(raw_fields.len());

    for (idx, field) in raw_fields.iter().enumerate() {
        if (field.struct_size as usize) < BamFieldDesc::MIN_V0_3_SIZE {
            return Err((
                FfiResult::INVALID_VERSION,
                format!(
                    "BamFieldDesc[{idx}] struct_size ({}) < MIN_V0_3_SIZE ({})",
                    field.struct_size,
                    BamFieldDesc::MIN_V0_3_SIZE
                ),
            ));
        }
        // SAFETY: Caller guarantees each field `name` points to readable bytes when `len > 0`.
        let name_str = unsafe { field.name.try_as_str() }.map_err(|err| {
            (
                err,
                format!("BamFieldDesc[{idx}] `name` StrView validation failed ({err})"),
            )
        })?;
        // SAFETY: Caller guarantees each field `type_tag` points to readable bytes when `len > 0`.
        let tag_str = unsafe { field.type_tag.try_as_str() }.map_err(|err| {
            (
                err,
                format!("BamFieldDesc[{idx}] `type_tag` StrView validation failed ({err})"),
            )
        })?;
        string_pairs.push((Arc::from(name_str), Arc::from(tag_str)));
    }

    let owned_strings: OwnedFieldStrings = Arc::from(string_pairs);
    for (field, (name_arc, tag_arc)) in raw_fields.iter().zip(owned_strings.iter()) {
        field_descs.push(BamFieldDesc::new(
            field.kind,
            StrView::from_str(name_arc),
            StrView::from_str(tag_arc),
            field.offset,
            field.size,
            field.align,
        ));
    }
    Ok((
        schema_fingerprint,
        Arc::from(field_descs),
        Some(owned_strings),
    ))
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
    /// Deterministic 64-bit FNV-1a schema fingerprint (`ModComponent::SCHEMA_FINGERPRINT`).
    pub schema_fingerprint: u64,
    /// Host-retained ABI `0.3` compile-time field reflection metadata (`ModComponent::BAM_FIELDS`).
    pub fields: Arc<[BamFieldDesc]>,
    /// Owned string backing for dynamic guest [`Self::fields`] entries (`None` for `'static` host entries).
    pub owned_field_strings: Option<OwnedFieldStrings>,
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
    /// Whether `on_add` hook trampoline was registered on Bevy's `ComponentHooks`.
    pub has_on_add_hook: bool,
    /// Whether `on_remove` hook trampoline was registered on Bevy's `ComponentHooks`.
    pub has_on_remove_hook: bool,
    /// Optional [`ModId`] owning a dynamic component's VTable and drop callback.
    pub create_msg_cursor_fn: Option<CreateMsgCursorFn>,
    /// Optional monomorphized batch reader for `Messages<M>`.
    pub msg_read_batch_fn: Option<MsgReadBatchFn>,
    /// Optional monomorphized writer for `Messages<M>`.
    pub msg_send_fn: Option<MsgSendFn>,
    /// Optional monomorphized batch writer for `Messages<M>`.
    pub msg_write_batch_fn: Option<MsgWriteBatchFn>,
    /// Whether raw 8-byte entity payloads (such as `ChildOf`) must be validated before insert.
    pub validates_entity_payload: bool,
    /// Whether `guest_observer_runner` synthesizes an aligned stack [`FfiEntity`] wire payload
    /// instead of forwarding raw `&E` bytes (used for bridged `#[repr(Rust)]` entity events like `bevy_scene::Ready`).
    pub synthesizes_ffi_entity_payload: bool,
    /// Bevy [`EventKey`] when registered as a triggerable host [`Event`] / [`EntityEvent`] or dynamic event.
    pub event_key: Option<EventKey>,
    /// Optional safe entity-only trigger constructor (`fn(&mut World, Entity)`) for bridged
    /// `EntityEvent` types such as `bevy_scene::Ready` that do not implement `From<Entity>`.
    pub entity_event_trigger_fn: Option<EntityEventTriggerFn>,
    /// Monomorphized typed `world.trigger(...)` dispatcher for host-registered events.
    pub trigger_event_fn: Option<TriggerEventFn>,
    /// Extractor returning the target entity from raw `EntityEvent` payload bytes.
    pub extract_event_target_fn: Option<ExtractEventTargetFn>,
    /// Optional extractor reading `RelationshipTarget` entities (such as `Children`) into an [`FfiEntity`] slice.
    pub extract_relationship_targets_fn: Option<ExtractRelationshipTargetsFn>,
    /// Optional [`ModId`] owning a dynamic component's VTable and drop callback.
    pub owner: Option<ModId>,
}

impl RegisteredComponent {
    /// Returns `true` if this entry represents a singleton Bevy `Resource`.
    #[inline]
    pub const fn is_resource(&self) -> bool {
        (self.storage_flags & BAM_STORAGE_FLAG_RESOURCE) != 0
    }

    /// Returns `true` if this entry represents a buffered Bevy `Messages<M>` stream.
    #[inline]
    pub const fn is_message(&self) -> bool {
        (self.storage_flags & BAM_STORAGE_FLAG_MESSAGE) != 0
    }

    /// Returns `true` if this entry represents a registered triggerable Bevy `Event`.
    #[inline]
    pub const fn is_event(&self) -> bool {
        (self.storage_flags & BAM_STORAGE_FLAG_EVENT) != 0
    }

    /// Returns `true` if this entry represents an entity-targeted Bevy `EntityEvent`.
    #[inline]
    pub const fn is_entity_event(&self) -> bool {
        (self.storage_flags & BAM_STORAGE_FLAG_ENTITY_EVENT) != 0
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

    /// Returns `true` if `BAM_STORAGE_FLAG_SUMMARY_TICK` is set.
    #[inline]
    pub const fn is_summary_tick(&self) -> bool {
        (self.storage_flags & BAM_STORAGE_FLAG_SUMMARY_TICK) != 0
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ExtendedDynamicFlags {
    pub(crate) has_on_add_effective: bool,
    pub(crate) has_on_insert: bool,
    pub(crate) has_on_discard: bool,
    pub(crate) has_on_remove: bool,
    pub(crate) has_on_despawn: bool,
    pub(crate) has_clone_fn: bool,
}

/// Immutable dense snapshot shared via `Arc` across `ModRegistry`, `HostContext`, and `GuestSystem`.
#[derive(Clone, Default)]
pub struct RegistrySnapshot {
    pub(crate) entries: Vec<RegisteredComponent>,
    pub(crate) by_name: HashMap<Arc<str>, BamComponent>,
    pub(crate) by_uuid: HashMap<ComponentUuid, BamComponent>,
    pub(crate) by_component_id: HashMap<ComponentId, BamComponent>,
    pub(crate) symbols: HashMap<Arc<str>, SymbolRecord>,
    pub(crate) required_components: HashMap<BamComponent, Box<[BamComponent]>>,
    pub(crate) extended_hook_flags: HashMap<BamComponent, ExtendedDynamicFlags>,
    asset_backend: Option<Arc<dyn HostAssetBackend>>,
    scene_backend: Option<Arc<dyn HostSceneBackend>>,
}

impl RegistrySnapshot {
    /// Looks up a component or resource in `O(1)` by its 1-based [`BamComponent`] handle.
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

    /// Resolves a `"bam.v0.*"` symbol or VTable address in this snapshot.
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

    /// Borrows the optional [`HostAssetBackend`] registered on this snapshot.
    #[inline]
    pub fn asset_backend(&self) -> Option<&Arc<dyn HostAssetBackend>> {
        self.asset_backend.as_ref()
    }

    /// Borrows the optional [`HostSceneBackend`] registered on this snapshot.
    #[inline]
    pub fn scene_backend(&self) -> Option<&Arc<dyn HostSceneBackend>> {
        self.scene_backend.as_ref()
    }

    /// Returns the required [`BamComponent`] handles registered for `handle` (empty if none).
    #[inline]
    pub fn required_components(&self, handle: BamComponent) -> &[BamComponent] {
        self.required_components
            .get(&handle)
            .map_or(&[], |boxed| boxed.as_ref())
    }
}

struct RegistryTracker {
    latest: ArcSwap<RegistrySnapshot>,
    generation: AtomicU64,
}

/// World-scoped Copy-on-Write (`CoW`) component, resource, and `"bam.v0.*"` symbol registry.
///
/// Cloning `ModRegistry` is an `O(1)` `Arc` bump. Mutations run on cold setup/load/unload paths via
/// `Arc::make_mut` and increment `generation` so `GuestSystem`s refresh their cached snapshot only
/// when the registry actually changes.
#[derive(Resource, Clone)]
pub struct ModRegistry {
    pub(super) snapshot: Arc<RegistrySnapshot>,
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
    /// Creates an empty [`ModRegistry`] with **zero** built-in function symbols.
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
    pub(crate) fn publish_after_mutation(&mut self) {
        self.tracker.latest.store(Arc::clone(&self.snapshot));
        let next_gen = self
            .tracker
            .generation
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1);
        self.generation = next_gen;
    }

    /// Installs or replaces the world-scoped [`HostAssetBackend`] servicing `asset_*` FFI slots.
    pub fn set_asset_backend(&mut self, backend: Arc<dyn HostAssetBackend>) {
        let snap = Arc::make_mut(&mut self.snapshot);
        snap.asset_backend = Some(backend);
        self.publish_after_mutation();
    }

    /// Borrows the installed [`HostAssetBackend`], if any.
    #[inline]
    pub fn asset_backend(&self) -> Option<&Arc<dyn HostAssetBackend>> {
        self.snapshot.asset_backend()
    }

    /// Installs or replaces the world-scoped [`HostSceneBackend`] servicing `cmd_spawn_scene`.
    pub fn set_scene_backend(&mut self, backend: Arc<dyn HostSceneBackend>) {
        let snap = Arc::make_mut(&mut self.snapshot);
        snap.scene_backend = Some(backend);
        self.publish_after_mutation();
    }

    /// Borrows the installed [`HostSceneBackend`], if any.
    #[inline]
    pub fn scene_backend(&self) -> Option<&Arc<dyn HostSceneBackend>> {
        self.snapshot.scene_backend()
    }

    /// Validates that registering `(name, uuid, size, align, storage_flags, expected_comp_id)` does
    /// not collide with an incompatible existing component or resource entry.
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

    /// Validates that dynamically re-registering `name` with `(has_on_add, has_on_remove)` matches
    /// the lifecycle hooks permanently wired into Bevy's `ComponentHooks`.
    pub fn check_dynamic_hook_compatibility(
        &self,
        name: &str,
        has_on_add: bool,
        has_on_remove: bool,
    ) -> Result<(), ComponentCollisionError> {
        if let Some(existing) = self.snapshot.get_by_name(name) {
            if !existing.is_dynamic && (has_on_add || has_on_remove) {
                return Err(ComponentCollisionError::MetadataMismatch {
                    name: name.to_string(),
                    reason: "cannot attach foreign on_add/on_remove hooks to host-owned component"
                        .to_string(),
                });
            }
            if existing.has_on_add_hook != has_on_add
                || existing.has_on_remove_hook != has_on_remove
            {
                return Err(ComponentCollisionError::MetadataMismatch {
                    name: name.to_string(),
                    reason: format!(
                        "hook presence mismatch on re-registration (existing=({}, {}), new=({has_on_add}, {has_on_remove}))",
                        existing.has_on_add_hook, existing.has_on_remove_hook
                    ),
                });
            }
        }
        Ok(())
    }

    /// Validates that dynamically re-registering `name` matches all extended lifecycle hooks,
    /// `clone_fn`, and `required_components` presence permanently wired into Bevy's `ComponentInfo`.
    pub fn check_dynamic_extended_hook_compatibility(
        &self,
        name: &str,
        desc: &ComponentDesc,
    ) -> Result<(), ComponentCollisionError> {
        let new_flags = ExtendedDynamicFlags {
            has_on_add_effective: desc.on_add.is_some() || desc.required_component_count > 0,
            has_on_insert: desc.on_insert.is_some(),
            has_on_discard: desc.on_discard.is_some(),
            has_on_remove: desc.on_remove.is_some(),
            has_on_despawn: desc.on_despawn.is_some(),
            has_clone_fn: desc.clone_fn.is_some(),
        };
        if let Some(existing) = self.snapshot.get_by_name(name) {
            if !existing.is_dynamic
                && (new_flags.has_on_add_effective
                    || new_flags.has_on_insert
                    || new_flags.has_on_discard
                    || new_flags.has_on_remove
                    || new_flags.has_on_despawn
                    || new_flags.has_clone_fn)
            {
                return Err(ComponentCollisionError::MetadataMismatch {
                    name: name.to_string(),
                    reason: "cannot attach foreign hooks/clone/required_components to host-owned component"
                        .to_string(),
                });
            }
            let existing_flags = self
                .snapshot
                .extended_hook_flags
                .get(&existing.handle)
                .copied()
                .unwrap_or(ExtendedDynamicFlags {
                    has_on_add_effective: existing.has_on_add_hook,
                    has_on_remove: existing.has_on_remove_hook,
                    ..ExtendedDynamicFlags::default()
                });
            if existing_flags != new_flags {
                return Err(ComponentCollisionError::MetadataMismatch {
                    name: name.to_string(),
                    reason: format!(
                        "extended hook/clone presence mismatch on re-registration (existing={existing_flags:?}, new={new_flags:?})"
                    ),
                });
            }
        }
        Ok(())
    }

    /// Registers a host [`Component`] implementing [`ModComponent`] with [`ComponentOptions`],
    /// returning its 1-based [`BamComponent`] handle.
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
    /// 1-based [`BamComponent`] handle.
    pub fn register_resource<R: Resource + ModComponent>(
        &mut self,
        comp_id: ComponentId,
    ) -> Result<BamComponent, ComponentCollisionError> {
        let storage_flags =
            component_flags_to_storage_flags(R::COMPONENT_FLAGS) | BAM_STORAGE_FLAG_RESOURCE;
        self.insert_host_entry::<R>(comp_id, storage_flags, None, None)
    }

    /// Registers a buffered POD Bevy [`Message`] type (`Messages<M>`) implementing [`ModComponent`]
    /// and [`AbiPod`], returning its 1-based [`BamComponent`] handle.
    pub fn register_message<M: Message + ModComponent + AbiPod>(
        &mut self,
        messages_res_id: ComponentId,
    ) -> Result<BamComponent, ComponentCollisionError> {
        self.register_message_inner::<M>(
            messages_res_id,
            true,
            Some(msg_read_batch_impl::<M>),
            Some(msg_send_impl::<M>),
            Some(msg_write_batch_impl::<M>),
        )
    }

    /// Registers a VTable-backed non-POD Bevy [`Message`] type (`Messages<M>`) implementing
    /// [`ModComponent`] + [`Default`] (e.g. containing `String` or `Vec<T>` fields), returning its
    /// 1-based [`BamComponent`] handle.
    ///
    /// - `msg_read_batch` writes `*const c_void` (`usize`) pointers to host `&M` items inside
    ///   `Messages<M>`, which guests inspect through the exported `"bam.v0.<M>.export_fields"`
    ///   witness symbol.
    /// - `msg_send` accepts field-witness packets `[u32 field_count; (name, tag, payload)*]` and
    ///   reconstructs `M` on the host allocator via `"bam.v0.<M>.import_field"`.
    pub fn register_non_pod_message<M: Message + ModComponent + Default>(
        &mut self,
        messages_res_id: ComponentId,
    ) -> Result<BamComponent, ComponentCollisionError> {
        self.register_message_inner::<M>(
            messages_res_id,
            false,
            Some(msg_read_batch_non_pod_impl::<M>),
            Some(msg_send_non_pod_impl::<M>),
            None,
        )
    }

    fn register_message_inner<M: Message + ModComponent>(
        &mut self,
        messages_res_id: ComponentId,
        is_abi_pod: bool,
        msg_read_batch_fn: Option<MsgReadBatchFn>,
        msg_send_fn: Option<MsgSendFn>,
        msg_write_batch_fn: Option<MsgWriteBatchFn>,
    ) -> Result<BamComponent, ComponentCollisionError> {
        let storage_flags =
            component_flags_to_storage_flags(M::COMPONENT_FLAGS) | BAM_STORAGE_FLAG_MESSAGE;
        let name_str = M::CANONICAL_NAME;
        let uuid = M::UUID;
        let (wire_size, wire_align) = if is_abi_pod {
            (M::size(), M::align())
        } else {
            (
                core::mem::size_of::<*const c_void>(),
                core::mem::align_of::<*const c_void>(),
            )
        };
        self.check_registration_collision(
            name_str,
            uuid,
            wire_size,
            wire_align,
            storage_flags,
            Some(messages_res_id),
        )?;

        let sym_addr = SymbolAddress::new(M::erased_vtable_ptr());
        let handle = {
            let snap = Arc::make_mut(&mut self.snapshot);
            let handle = if let Some(&existing_handle) = snap.by_name.get(name_str) {
                let idx = (existing_handle.0 - 1) as usize;
                snap.entries[idx].vtable_ptr = sym_addr;
                snap.entries[idx].size = wire_size;
                snap.entries[idx].align = wire_align;
                snap.entries[idx].schema_fingerprint = M::SCHEMA_FINGERPRINT;
                snap.entries[idx].fields = Arc::from(M::BAM_FIELDS);
                snap.entries[idx].owned_field_strings = None;
                snap.entries[idx].is_abi_pod = is_abi_pod;
                snap.entries[idx].create_msg_cursor_fn = Some(create_msg_cursor_impl::<M>);
                snap.entries[idx].msg_read_batch_fn = msg_read_batch_fn;
                snap.entries[idx].msg_send_fn = msg_send_fn;
                snap.entries[idx].msg_write_batch_fn = msg_write_batch_fn;
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
                    component_id: messages_res_id,
                    size: wire_size,
                    align: wire_align,
                    schema_fingerprint: M::SCHEMA_FINGERPRINT,
                    fields: Arc::from(M::BAM_FIELDS),
                    owned_field_strings: None,
                    storage_flags,
                    is_abi_pod,
                    is_dynamic: false,
                    vtable_ptr: sym_addr,
                    insert_raw_fn: None,
                    insert_default_fn: None,
                    drop_fn: None,
                    drop_slot: None,
                    has_on_add_hook: false,
                    has_on_remove_hook: false,
                    create_msg_cursor_fn: Some(create_msg_cursor_impl::<M>),
                    msg_read_batch_fn,
                    msg_send_fn,
                    msg_write_batch_fn,
                    validates_entity_payload: false,
                    synthesizes_ffi_entity_payload: false,
                    event_key: None,
                    entity_event_trigger_fn: None,
                    trigger_event_fn: None,
                    extract_event_target_fn: None,
                    extract_relationship_targets_fn: None,
                    owner: None,
                };
                snap.entries.push(entry);
                snap.by_name.insert(arc_name, handle);
                snap.by_uuid.insert(uuid, handle);
                snap.by_component_id.insert(messages_res_id, handle);
                handle
            };
            Self::insert_mod_component_symbols::<M>(snap);
            if !is_abi_pod {
                let marker_sym = format!("bam.v0.{}.non_pod_message", M::CANONICAL_NAME);
                snap.symbols.insert(
                    Arc::from(marker_sym.as_str()),
                    SymbolRecord {
                        addr: SymbolAddress::new(msg_read_batch_non_pod_impl::<M> as *const c_void),
                        owner: None,
                    },
                );
            }
            handle
        };
        self.publish_after_mutation();
        Ok(handle)
    }

    /// Registers Bevy's built-in [`ChildOf`] relationship component under [`BAM_CHILD_OF_NAME`]
    /// (plus `"ChildOf"` alias if unclaimed) with [`FfiChildOf`] wire layout (`validates_entity_payload = true`).
    pub fn register_hierarchy(
        &mut self,
        child_of_id: ComponentId,
        is_immutable: bool,
    ) -> Result<BamComponent, ComponentCollisionError> {
        let storage_flags = if is_immutable {
            BAM_STORAGE_FLAG_IMMUTABLE
        } else {
            0
        };
        let uuid = FfiChildOf::UUID;
        self.check_registration_collision(
            BAM_CHILD_OF_NAME,
            uuid,
            core::mem::size_of::<FfiChildOf>(),
            core::mem::align_of::<FfiChildOf>(),
            storage_flags,
            Some(child_of_id),
        )?;

        let handle = {
            let snap = Arc::make_mut(&mut self.snapshot);
            let handle = if let Some(&existing_handle) = snap.by_name.get(BAM_CHILD_OF_NAME) {
                let idx = (existing_handle.0 - 1) as usize;
                snap.entries[idx].insert_raw_fn = Some(insert_child_of_raw);
                snap.entries[idx].validates_entity_payload = true;
                snap.entries[idx].is_dynamic = false;
                snap.entries[idx].owner = None;
                existing_handle
            } else {
                let next_raw = u32::try_from(snap.entries.len() + 1)
                    .expect("ModRegistry exceeded u32::MAX component handles");
                let handle = BamComponent::from_raw(next_raw);
                let arc_name: Arc<str> = Arc::from(BAM_CHILD_OF_NAME);
                let entry = RegisteredComponent {
                    handle,
                    name: Arc::clone(&arc_name),
                    uuid,
                    component_id: child_of_id,
                    size: core::mem::size_of::<FfiChildOf>(),
                    align: core::mem::align_of::<FfiChildOf>(),
                    schema_fingerprint: 0,
                    fields: Arc::from([]),
                    owned_field_strings: None,
                    storage_flags,
                    is_abi_pod: true,
                    is_dynamic: false,
                    vtable_ptr: SymbolAddress::NULL,
                    insert_raw_fn: Some(insert_child_of_raw),
                    insert_default_fn: None,
                    drop_fn: None,
                    drop_slot: None,
                    has_on_add_hook: false,
                    has_on_remove_hook: false,
                    create_msg_cursor_fn: None,
                    msg_read_batch_fn: None,
                    msg_send_fn: None,
                    msg_write_batch_fn: None,
                    validates_entity_payload: true,
                    synthesizes_ffi_entity_payload: false,
                    event_key: None,
                    entity_event_trigger_fn: None,
                    trigger_event_fn: None,
                    extract_event_target_fn: None,
                    extract_relationship_targets_fn: None,
                    owner: None,
                };
                snap.entries.push(entry);
                snap.by_name.insert(arc_name, handle);
                snap.by_uuid.insert(uuid, handle);
                snap.by_component_id.insert(child_of_id, handle);
                handle
            };
            if !snap.by_name.contains_key("ChildOf") {
                snap.by_name.insert(Arc::from("ChildOf"), handle);
            }
            handle
        };
        self.publish_after_mutation();
        Ok(handle)
    }

    /// Registers a triggerable host Bevy [`Event`] implementing [`ModComponent`] + [`Copy`],
    /// returning its 1-based [`BamComponent`] handle.
    pub fn register_event<E>(
        &mut self,
        event_key: EventKey,
    ) -> Result<BamComponent, ComponentCollisionError>
    where
        E: Event<Trigger<'static>: Default> + ModComponent + Copy,
    {
        self.register_event_internal::<E>(
            event_key,
            BAM_STORAGE_FLAG_EVENT,
            Some(trigger_event_impl::<E>),
            None,
        )
    }

    /// Registers a triggerable entity-targeted Bevy [`EntityEvent`] implementing [`ModComponent`] + [`Copy`],
    /// returning its 1-based [`BamComponent`] handle.
    pub fn register_entity_event<E>(
        &mut self,
        event_key: EventKey,
    ) -> Result<BamComponent, ComponentCollisionError>
    where
        E: EntityEvent + Event + ModComponent + Copy,
        for<'a> E::Trigger<'a>: Default,
    {
        self.register_event_internal::<E>(
            event_key,
            BAM_STORAGE_FLAG_EVENT | BAM_STORAGE_FLAG_ENTITY_EVENT,
            Some(trigger_entity_event_impl::<E>),
            Some(extract_entity_event_target_impl::<E>),
        )
    }

    fn register_event_internal<E: ModComponent + Copy>(
        &mut self,
        event_key: EventKey,
        extra_flags: u32,
        trigger_event_fn: Option<TriggerEventFn>,
        extract_event_target_fn: Option<ExtractEventTargetFn>,
    ) -> Result<BamComponent, ComponentCollisionError> {
        let storage_flags = component_flags_to_storage_flags(E::COMPONENT_FLAGS) | extra_flags;
        let comp_id = event_key.component_id();
        let name_str = E::CANONICAL_NAME;
        let uuid = E::UUID;
        self.check_registration_collision(
            name_str,
            uuid,
            E::size(),
            E::align(),
            storage_flags,
            Some(comp_id),
        )?;

        let sym_addr = SymbolAddress::new(E::erased_vtable_ptr());
        let handle = {
            let snap = Arc::make_mut(&mut self.snapshot);
            let handle = if let Some(&existing_handle) = snap.by_name.get(name_str) {
                let idx = (existing_handle.0 - 1) as usize;
                snap.entries[idx].vtable_ptr = sym_addr;
                snap.entries[idx].schema_fingerprint = E::SCHEMA_FINGERPRINT;
                snap.entries[idx].fields = Arc::from(E::BAM_FIELDS);
                snap.entries[idx].owned_field_strings = None;
                snap.entries[idx].event_key = Some(event_key);
                snap.entries[idx].entity_event_trigger_fn = None;
                snap.entries[idx].trigger_event_fn = trigger_event_fn;
                snap.entries[idx].extract_event_target_fn = extract_event_target_fn;
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
                    size: E::size(),
                    align: E::align(),
                    schema_fingerprint: E::SCHEMA_FINGERPRINT,
                    fields: Arc::from(E::BAM_FIELDS),
                    owned_field_strings: None,
                    storage_flags,
                    is_abi_pod: E::IS_ABI_POD,
                    is_dynamic: false,
                    vtable_ptr: sym_addr,
                    insert_raw_fn: None,
                    insert_default_fn: None,
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
                    event_key: Some(event_key),
                    entity_event_trigger_fn: None,
                    trigger_event_fn,
                    extract_event_target_fn,
                    extract_relationship_targets_fn: None,
                    owner: None,
                };
                snap.entries.push(entry);
                snap.by_name.insert(arc_name, handle);
                snap.by_uuid.insert(uuid, handle);
                snap.by_component_id.insert(comp_id, handle);
                handle
            };
            Self::insert_mod_component_symbols::<E>(snap);
            handle
        };
        self.publish_after_mutation();
        Ok(handle)
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
                snap.entries[idx].schema_fingerprint = T::SCHEMA_FINGERPRINT;
                snap.entries[idx].fields = Arc::from(T::BAM_FIELDS);
                snap.entries[idx].owned_field_strings = None;
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
                    schema_fingerprint: T::SCHEMA_FINGERPRINT,
                    fields: Arc::from(T::BAM_FIELDS),
                    owned_field_strings: None,
                    storage_flags,
                    is_abi_pod: T::IS_ABI_POD,
                    is_dynamic: false,
                    vtable_ptr: sym_addr,
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
                    extract_relationship_targets_fn: None,
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
    #[allow(dead_code)]
    pub(crate) fn register_dynamic_component_owned(
        &mut self,
        comp_id: ComponentId,
        desc: &ComponentDesc,
        name: &str,
        owner: Option<ModId>,
        drop_slot: Option<usize>,
    ) -> Result<BamComponent, ComponentCollisionError> {
        self.register_dynamic_component_owned_with_required(
            comp_id,
            desc,
            name,
            owner,
            drop_slot,
            Box::new([]),
        )
    }

    /// Registers a guest-owned dynamic component with optional validated `required_components`,
    /// returning its 1-based [`BamComponent`] handle.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn register_dynamic_component_owned_with_required(
        &mut self,
        comp_id: ComponentId,
        desc: &ComponentDesc,
        name: &str,
        owner: Option<ModId>,
        drop_slot: Option<usize>,
        required_components: Box<[BamComponent]>,
    ) -> Result<BamComponent, ComponentCollisionError> {
        let fallback_fp = if desc.covers_slot(
            core::mem::offset_of!(ComponentDesc, schema_fingerprint),
            core::mem::size_of::<u64>(),
        ) {
            desc.schema_fingerprint
        } else {
            0
        };
        // SAFETY: Validated `ComponentDesc` passed from host helpers or FFI boundary.
        let (schema_fingerprint, fields, owned_field_strings) =
            unsafe { extract_owned_dynamic_fields(desc) }
                .unwrap_or_else(|_| (fallback_fp, Arc::from([]), None));
        self.register_dynamic_component_owned_with_schema(
            comp_id,
            desc,
            name,
            owner,
            drop_slot,
            required_components,
            schema_fingerprint,
            fields,
            owned_field_strings,
        )
    }

    /// Registers a guest-owned dynamic component with pre-extracted `schema_fingerprint` and
    /// host-retained `fields`, returning its 1-based [`BamComponent`] handle.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn register_dynamic_component_owned_with_schema(
        &mut self,
        comp_id: ComponentId,
        desc: &ComponentDesc,
        name: &str,
        owner: Option<ModId>,
        drop_slot: Option<usize>,
        required_components: Box<[BamComponent]>,
        schema_fingerprint: u64,
        fields: Arc<[BamFieldDesc]>,
        owned_field_strings: Option<OwnedFieldStrings>,
    ) -> Result<BamComponent, ComponentCollisionError> {
        let storage_flags = component_flags_to_storage_flags(desc.flags);
        let expected_comp_id = self
            .snapshot
            .get_by_name(name)
            .filter(|entry| entry.is_dynamic)
            .map_or(comp_id, |entry| entry.component_id);
        self.check_registration_collision(
            name,
            desc.uuid,
            desc.size,
            desc.align,
            storage_flags,
            Some(expected_comp_id),
        )?;
        self.check_dynamic_drop_compatibility(name, desc.drop_fn.is_some())?;
        self.check_dynamic_extended_hook_compatibility(name, desc)?;

        let ext_flags = ExtendedDynamicFlags {
            has_on_add_effective: desc.on_add.is_some() || !required_components.is_empty(),
            has_on_insert: desc.on_insert.is_some(),
            has_on_discard: desc.on_discard.is_some(),
            has_on_remove: desc.on_remove.is_some(),
            has_on_despawn: desc.on_despawn.is_some(),
            has_clone_fn: desc.clone_fn.is_some(),
        };

        let sym_addr = SymbolAddress::new(desc.vtable_ptr);
        let handle = {
            let snap = Arc::make_mut(&mut self.snapshot);
            let mut is_dynamic_entry = true;
            let mut replaced_comp_id: Option<(ComponentId, ComponentId)> = None;
            let handle = if let Some(&existing_handle) = snap.by_name.get(name) {
                let idx = (existing_handle.0 - 1) as usize;
                let entry = &mut snap.entries[idx];
                is_dynamic_entry = entry.is_dynamic;
                if entry.is_dynamic {
                    if entry.component_id != comp_id {
                        replaced_comp_id = Some((entry.component_id, comp_id));
                    }
                    entry.component_id = comp_id;
                    entry.schema_fingerprint = schema_fingerprint;
                    entry.fields = Arc::clone(&fields);
                    entry.owned_field_strings = owned_field_strings;
                    entry.vtable_ptr = sym_addr;
                    entry.drop_fn = desc.drop_fn;
                    entry.drop_slot = drop_slot;
                    // SAFETY: `comp_id` was registered in `world` with `(desc.size, desc.align)`
                    // and is safe for dynamic event dispatch.
                    entry.event_key = Some(unsafe { EventKey::new(comp_id) });
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
                    schema_fingerprint,
                    fields,
                    owned_field_strings,
                    storage_flags,
                    is_abi_pod: desc.is_pod(),
                    is_dynamic: true,
                    vtable_ptr: sym_addr,
                    insert_raw_fn: None,
                    insert_default_fn: None,
                    drop_fn: desc.drop_fn,
                    drop_slot,
                    has_on_add_hook: ext_flags.has_on_add_effective,
                    has_on_remove_hook: ext_flags.has_on_remove,
                    create_msg_cursor_fn: None,
                    msg_read_batch_fn: None,
                    msg_send_fn: None,
                    msg_write_batch_fn: None,
                    validates_entity_payload: false,
                    synthesizes_ffi_entity_payload: false,
                    // SAFETY: `comp_id` was registered via `world.register_component_with_descriptor`
                    // with layout `(desc.size, desc.align)` and is safe for dynamic event dispatch.
                    event_key: Some(unsafe { EventKey::new(comp_id) }),
                    entity_event_trigger_fn: None,
                    trigger_event_fn: None,
                    extract_event_target_fn: None,
                    extract_relationship_targets_fn: None,
                    owner,
                };
                snap.entries.push(entry);
                snap.by_name.insert(arc_name, handle);
                snap.by_uuid.insert(desc.uuid, handle);
                snap.by_component_id.insert(comp_id, handle);
                handle
            };

            if let Some((old_comp_id, new_comp_id)) = replaced_comp_id {
                snap.by_component_id.remove(&old_comp_id);
                snap.by_component_id.insert(new_comp_id, handle);
            }

            if is_dynamic_entry {
                snap.extended_hook_flags.insert(handle, ext_flags);
                if required_components.is_empty() {
                    snap.required_components.remove(&handle);
                } else {
                    snap.required_components.insert(handle, required_components);
                }
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

    /// Returns the required [`BamComponent`] handles registered for `handle` (empty if none).
    #[inline]
    pub fn required_components(&self, handle: BamComponent) -> &[BamComponent] {
        self.snapshot.required_components(handle)
    }
}
