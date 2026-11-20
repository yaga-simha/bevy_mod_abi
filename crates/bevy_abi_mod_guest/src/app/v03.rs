//! Safe `#![forbid(unsafe_code)]` ABI `0.3.0` builders and extensions:
//! - [`RelationDescBuilder`] & [`GuestApp::register_relation`]
//! - [`ScheduleSetBuilder`], [`GuestApp::configure_schedule_set`], & [`GuestApp::mount_system`]
//! - Symbol interning (`intern_symbol`), named object binding (`bind_named_object`),
//!   UUID-only resolution (`resolve_component_by_uuid`), schema/layout verification
//!   (`verify_component_layout`), panic-guarded column migration (`migrate_component_column`),
//!   structured diagnostics (`log_diagnostic`, `report_fault`), and read-only parallel chunk
//!   dispatch (`par_chunks_readonly`).

use std::ffi::CString;

use bevy_abi_mod_guest_sys::{Host, qualify_mod_component_identity, resolve_mod_component};
use bevy_abi_mod_sys::{
    AbiPod, BAM_RELATION_FLAG_ACYCLIC, BAM_RELATION_FLAG_CASCADE_DELETE,
    BAM_RELATION_FLAG_CASCADE_DESPAWN, BAM_RELATION_FLAG_CLEANUP_REMOVE,
    BAM_RELATION_FLAG_EXCLUSIVE, BAM_RELATION_FLAG_SYMMETRIC, BAM_SCHEDULE_SET_FLAG_AFTER_WEAK,
    BAM_SCHEDULE_SET_FLAG_BEFORE_WEAK, BAM_SCHEDULE_SET_FLAG_PARALLEL,
    BAM_SCHEDULE_SET_FLAG_SEQUENCE, BAM_SCHEDULE_SET_FLAG_SEQUENCE_WEAK, BamComponent,
    BamLayoutInfo, BamSymbol, BamSystem, BamSystemSet, ComponentUuid, FfiEntity, FfiResult,
    ModComponent, ScheduleId,
};

use crate::{
    app::{
        builder::{GuestApp, IntoSystemResult},
        context::{RunConditionContext, SystemContext},
    },
    component::{GuestComponent, qualify_component_identity},
    iter::Chunk,
};

const SCHEDULE_SET_EXEC_MODE_MASK: u32 = BAM_SCHEDULE_SET_FLAG_SEQUENCE
    | BAM_SCHEDULE_SET_FLAG_SEQUENCE_WEAK
    | BAM_SCHEDULE_SET_FLAG_PARALLEL;

/// Fluent `#![forbid(unsafe_code)]` builder for ABI `0.3.0` typed entity relationship descriptors
/// ([`BamRelationDesc`](bevy_abi_mod_sys::BamRelationDesc)) passed to
/// [`GuestApp::register_relation`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelationDescBuilder {
    name: String,
    uuid: Option<ComponentUuid>,
    field_schema_hash: u64,
    flags: u32,
    data_size: usize,
    data_align: usize,
    max_degree: u32,
}

impl RelationDescBuilder {
    /// Creates a marker (zero-sized payload) relationship descriptor named `name`.
    ///
    /// Unqualified names (without `"::"`) are automatically prefixed with `"<manifest.name>::"`
    /// when registered via [`GuestApp::register_relation`].
    #[inline]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            uuid: None,
            field_schema_hash: 0,
            flags: 0,
            data_size: 0,
            data_align: 1,
            max_degree: 0,
        }
    }

    /// Creates a relationship descriptor derived from a [`GuestComponent`] type `R`, adopting its
    /// canonical name and payload `(size, align)` layout.
    #[inline]
    pub fn for_component<R: GuestComponent>() -> Self {
        let name = R::NAME.to_str().unwrap_or("").to_owned();
        let default_unqualified = ComponentUuid::from_parts(
            &name,
            core::mem::size_of::<R>(),
            core::mem::align_of::<R>(),
            R::FIELD_SCHEMA_HASH,
        );
        let explicit_uuid = if name.contains("::") || R::UUID != default_unqualified {
            Some(R::UUID)
        } else {
            None
        };
        Self {
            name,
            uuid: explicit_uuid,
            field_schema_hash: R::FIELD_SCHEMA_HASH,
            flags: 0,
            data_size: core::mem::size_of::<R>(),
            data_align: core::mem::align_of::<R>(),
            max_degree: 0,
        }
    }

    /// Overrides the deterministic 128-bit [`ComponentUuid`] for this relationship.
    #[inline]
    pub const fn with_uuid(mut self, uuid: ComponentUuid) -> Self {
        self.uuid = Some(uuid);
        self
    }

    /// Marks this relationship as exclusive (`BAM_RELATION_FLAG_EXCLUSIVE`): attaching a new target
    /// replaces any prior edge of this relation on the source entity.
    #[inline]
    pub const fn exclusive(mut self) -> Self {
        self.flags |= BAM_RELATION_FLAG_EXCLUSIVE;
        self
    }

    /// Marks this relationship as symmetric (`BAM_RELATION_FLAG_SYMMETRIC`): edge `(A -> B)`
    /// implies `(B -> A)`.
    #[inline]
    pub const fn symmetric(mut self) -> Self {
        self.flags |= BAM_RELATION_FLAG_SYMMETRIC;
        self
    }

    /// Marks this relationship as acyclic (`BAM_RELATION_FLAG_ACYCLIC`): inserting an edge that
    /// would form a directed cycle is rejected with [`FfiResult::CYCLE_DETECTED`].
    #[inline]
    pub const fn acyclic(mut self) -> Self {
        self.flags |= BAM_RELATION_FLAG_ACYCLIC;
        self
    }

    /// Enables cascade despawning (`BAM_RELATION_FLAG_CASCADE_DESPAWN`): despawning a target also
    /// despawns dependent source entities.
    #[inline]
    pub const fn cascade_despawn(mut self) -> Self {
        self.flags |= BAM_RELATION_FLAG_CASCADE_DESPAWN;
        self
    }

    /// Alias for [`Self::cascade_despawn`] (`BAM_RELATION_FLAG_CASCADE_DELETE`).
    #[inline]
    pub const fn cascade_delete(mut self) -> Self {
        self.flags |= BAM_RELATION_FLAG_CASCADE_DELETE;
        self
    }

    /// Enables cleanup-remove (`BAM_RELATION_FLAG_CLEANUP_REMOVE`): despawning a target strips
    /// only the relationship edge from dependent source entities rather than despawning them.
    #[inline]
    pub const fn cleanup_remove(mut self) -> Self {
        self.flags |= BAM_RELATION_FLAG_CLEANUP_REMOVE;
        self
    }

    /// ORs explicit `BAM_RELATION_FLAG_*` bitflags onto this relationship descriptor.
    #[inline]
    pub const fn flags(mut self, flags: u32) -> Self {
        self.flags |= flags;
        self
    }

    /// Configures an optional out-degree limit per source entity (`0` = unlimited).
    #[inline]
    pub const fn max_degree(mut self, max_degree: u32) -> Self {
        self.max_degree = max_degree;
        self
    }

    /// Configures edge payload storage layout matching cross-language `T: AbiPod`.
    #[inline]
    pub const fn with_payload<T: AbiPod>(mut self) -> Self {
        self.data_size = core::mem::size_of::<T>();
        self.data_align = core::mem::align_of::<T>();
        self
    }

    /// Explicitly configures raw payload `(data_size, data_align)` byte layout.
    #[inline]
    pub const fn with_payload_layout(mut self, data_size: usize, data_align: usize) -> Self {
        self.data_size = data_size;
        self.data_align = data_align;
        self
    }

    /// Returns the configured relationship name.
    #[inline]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the optional explicit UUID override, if set.
    #[inline]
    pub const fn uuid(&self) -> Option<ComponentUuid> {
        self.uuid
    }

    /// Returns the accumulated `BAM_RELATION_FLAG_*` bitmask.
    #[inline]
    pub const fn raw_flags(&self) -> u32 {
        self.flags
    }

    /// Returns the edge payload byte size (`0` for marker relations).
    #[inline]
    pub const fn data_size(&self) -> usize {
        self.data_size
    }

    /// Returns the edge payload byte alignment.
    #[inline]
    pub const fn data_align(&self) -> usize {
        self.data_align
    }

    /// Returns the configured out-degree limit (`0` = unlimited).
    #[inline]
    pub const fn configured_max_degree(&self) -> u32 {
        self.max_degree
    }
}

/// Fluent `#![forbid(unsafe_code)]` builder for ABI `0.3.0` schedule system-set descriptors
/// ([`BamScheduleSetDesc`](bevy_abi_mod_sys::BamScheduleSetDesc)) passed to
/// [`GuestApp::configure_schedule_set`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScheduleSetBuilder {
    name: String,
    schedule: ScheduleId,
    flags: u32,
    before_sets: Vec<BamSystemSet>,
    after_sets: Vec<BamSystemSet>,
}

impl ScheduleSetBuilder {
    /// Creates a new schedule system-set builder named `name` targeting [`ScheduleId::UPDATE`].
    #[inline]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            schedule: ScheduleId::UPDATE,
            flags: 0,
            before_sets: Vec::new(),
            after_sets: Vec::new(),
        }
    }

    /// Selects the target [`ScheduleId`] for this set (default [`ScheduleId::UPDATE`]).
    #[inline]
    pub const fn schedule(mut self, schedule: ScheduleId) -> Self {
        self.schedule = schedule;
        self
    }

    /// Configures strict sequential execution (`.chain()`, `BAM_SCHEDULE_SET_FLAG_SEQUENCE`),
    /// clearing any previously selected execution mode flag.
    #[inline]
    pub const fn sequence(mut self) -> Self {
        self.flags = (self.flags & !SCHEDULE_SET_EXEC_MODE_MASK) | BAM_SCHEDULE_SET_FLAG_SEQUENCE;
        self
    }

    /// Alias for [`Self::sequence`] (Bevy `.chain()`).
    #[inline]
    pub const fn chain(self) -> Self {
        self.sequence()
    }

    /// Configures sequential weak-edge execution (`.chain_weak()`,
    /// `BAM_SCHEDULE_SET_FLAG_SEQUENCE_WEAK`), clearing any previously selected execution mode flag.
    #[inline]
    pub const fn sequence_weak(mut self) -> Self {
        self.flags =
            (self.flags & !SCHEDULE_SET_EXEC_MODE_MASK) | BAM_SCHEDULE_SET_FLAG_SEQUENCE_WEAK;
        self
    }

    /// Alias for [`Self::sequence_weak`] (Bevy `.chain_weak()`).
    #[inline]
    pub const fn chain_weak(self) -> Self {
        self.sequence_weak()
    }

    /// Configures explicit parallel execution (`BAM_SCHEDULE_SET_FLAG_PARALLEL`), clearing any
    /// previously selected execution mode flag.
    #[inline]
    pub const fn parallel(mut self) -> Self {
        self.flags = (self.flags & !SCHEDULE_SET_EXEC_MODE_MASK) | BAM_SCHEDULE_SET_FLAG_PARALLEL;
        self
    }

    /// Orders this set to execute before `set`.
    #[inline]
    pub fn before(mut self, set: BamSystemSet) -> Self {
        self.before_sets.push(set);
        self
    }

    /// Orders this set to execute after `set`.
    #[inline]
    pub fn after(mut self, set: BamSystemSet) -> Self {
        self.after_sets.push(set);
        self
    }

    /// Weakly orders this set to execute before `set` (sets `BAM_SCHEDULE_SET_FLAG_BEFORE_WEAK`).
    #[inline]
    pub fn before_weak(mut self, set: BamSystemSet) -> Self {
        self.before_sets.push(set);
        self.flags |= BAM_SCHEDULE_SET_FLAG_BEFORE_WEAK;
        self
    }

    /// Weakly orders this set to execute after `set` (sets `BAM_SCHEDULE_SET_FLAG_AFTER_WEAK`).
    #[inline]
    pub fn after_weak(mut self, set: BamSystemSet) -> Self {
        self.after_sets.push(set);
        self.flags |= BAM_SCHEDULE_SET_FLAG_AFTER_WEAK;
        self
    }

    /// ORs explicit `BAM_SCHEDULE_SET_FLAG_*` bits onto this descriptor.
    #[inline]
    pub const fn flags(mut self, flags: u32) -> Self {
        self.flags |= flags;
        self
    }

    /// Returns the configured schedule set name.
    #[inline]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the target [`ScheduleId`].
    #[inline]
    pub const fn configured_schedule(&self) -> ScheduleId {
        self.schedule
    }

    /// Returns the accumulated `BAM_SCHEDULE_SET_FLAG_*` bitmask.
    #[inline]
    pub const fn raw_flags(&self) -> u32 {
        self.flags
    }

    /// Returns the slice of `before` target sets.
    #[inline]
    pub fn before_sets(&self) -> &[BamSystemSet] {
        &self.before_sets
    }

    /// Returns the slice of `after` target sets.
    #[inline]
    pub fn after_sets(&self) -> &[BamSystemSet] {
        &self.after_sets
    }
}

#[inline]
fn verify_layout_for_host<T: ModComponent>(
    host: &Host,
    component: BamComponent,
) -> Result<BamLayoutInfo, FfiResult> {
    let info = host.component_layout_info(component)?;
    if info.size != core::mem::size_of::<T>() || info.align != core::mem::align_of::<T>() {
        return Err(FfiResult::TYPE_MISMATCH);
    }
    if T::SCHEMA_FINGERPRINT != 0 && info.schema_fingerprint != T::SCHEMA_FINGERPRINT {
        return Err(FfiResult::TYPE_MISMATCH);
    }
    Ok(info)
}

#[inline]
fn resolve_and_verify_layout<T: ModComponent>(host: &Host) -> Result<BamLayoutInfo, FfiResult> {
    let (_qualified_name, qualified_uuid) =
        qualify_mod_component_identity::<T>(host.manifest_name())?;
    let handle = match host.resolve_component_by_uuid(qualified_uuid) {
        Ok(h) => h,
        Err(FfiResult::NOT_FOUND) if qualified_uuid != T::UUID => {
            host.resolve_component_by_uuid(T::UUID)?
        }
        Err(FfiResult::UNSUPPORTED) => resolve_mod_component::<T>(host)?,
        Err(err) => return Err(err),
    };
    verify_layout_for_host::<T>(host, handle)
}

impl<'a> GuestApp<'a> {
    /// Registers a typed entity relationship edge described by [`RelationDescBuilder`] via
    /// `BamHostApi::register_relation` (slot 54), returning its 1-based [`BamComponent`] handle.
    pub fn register_relation(
        &mut self,
        desc: RelationDescBuilder,
    ) -> Result<BamComponent, FfiResult> {
        if desc.name.is_empty() {
            return Err(FfiResult::INVALID_ARGUMENT);
        }
        let qualified = if !desc.name.contains("::") && !self.instance.manifest_name().is_empty() {
            format!("{}::{}", self.instance.manifest_name(), desc.name)
        } else {
            desc.name
        };
        let Ok(cstr) = CString::new(qualified.as_bytes()) else {
            return Err(FfiResult::INVALID_ARGUMENT);
        };
        let uuid = desc.uuid.unwrap_or_else(|| {
            ComponentUuid::from_parts(
                &qualified,
                desc.data_size,
                desc.data_align,
                desc.field_schema_hash,
            )
        });
        self.load_ctx.register_relation(
            self.instance,
            cstr.as_c_str(),
            uuid,
            desc.flags,
            desc.data_size,
            desc.data_align,
            desc.max_degree,
        )
    }

    /// Configures a named schedule system set described by [`ScheduleSetBuilder`] via
    /// `BamHostApi::configure_schedule_set` (slot 61), returning its [`BamSystemSet`] handle.
    pub fn configure_schedule_set(
        &mut self,
        desc: ScheduleSetBuilder,
    ) -> Result<BamSystemSet, FfiResult> {
        if desc.name.is_empty() {
            return Err(FfiResult::INVALID_ARGUMENT);
        }
        let Ok(cstr) = CString::new(desc.name.as_bytes()) else {
            return Err(FfiResult::INVALID_ARGUMENT);
        };
        self.load_ctx.configure_schedule_set(
            self.instance,
            cstr.as_c_str(),
            desc.schedule,
            desc.flags,
            &desc.before_sets,
            &desc.after_sets,
        )
    }

    /// Mounts a registered [`BamSystem`] handle into `schedule` under `set`
    /// (`BamSystemSet::INVALID` for ungrouped scheduling) via
    /// `BamHostApi::mount_system_in_schedule` (slot 62).
    #[inline]
    pub fn mount_system(
        &mut self,
        system: BamSystem,
        schedule: ScheduleId,
        set: BamSystemSet,
    ) -> FfiResult {
        self.load_ctx
            .mount_system_in_schedule(system, schedule, set)
    }

    /// Binds a canonical UTF-8 lookup `name` to `entity` in the host symbol registry via
    /// `BamHostApi::bind_named_object` (slot 56).
    #[inline]
    pub fn bind_named_object(&self, name: &str, entity: FfiEntity) -> FfiResult {
        self.host().bind_named_object(name, entity)
    }

    /// Interns a UTF-8 symbol string into an `O(1)` comparable [`BamSymbol`] handle via
    /// `BamHostApi::intern_symbol` (slot 55).
    #[inline]
    pub fn intern_symbol(&self, symbol: &str) -> Result<BamSymbol, FfiResult> {
        self.host().intern_symbol(symbol)
    }

    /// Resolves a registered component or resource strictly by its 128-bit [`ComponentUuid`] via
    /// `BamHostApi::resolve_component_by_uuid` (slot 52).
    #[inline]
    pub fn resolve_component_by_uuid(
        &self,
        uuid: ComponentUuid,
    ) -> Result<BamComponent, FfiResult> {
        self.host().resolve_component_by_uuid(uuid)
    }

    /// Queries ABI `0.3.0` [`BamLayoutInfo`] metadata (`size`, `align`, `schema_fingerprint`,
    /// storage flags, and fields) for `component` via `BamHostApi::component_layout_info` (slot 53).
    #[inline]
    pub fn component_layout_info(
        &self,
        component: BamComponent,
    ) -> Result<BamLayoutInfo, FfiResult> {
        self.host().component_layout_info(component)
    }

    /// Resolves `T: ModComponent` on the host and verifies that the host's registered `size`,
    /// `align`, and `SCHEMA_FINGERPRINT` match `T`, returning [`BamLayoutInfo`] on success or
    /// [`FfiResult::TYPE_MISMATCH`] on layout or schema divergence.
    #[inline]
    pub fn verify_component_layout<T: ModComponent>(&self) -> Result<BamLayoutInfo, FfiResult> {
        resolve_and_verify_layout::<T>(self.host())
    }

    /// Verifies that the host's registered `size`, `align`, and `SCHEMA_FINGERPRINT` for an
    /// already-resolved `component` handle match `T: ModComponent`.
    #[inline]
    pub fn verify_component_layout_handle<T: ModComponent>(
        &self,
        component: BamComponent,
    ) -> Result<BamLayoutInfo, FfiResult> {
        verify_layout_for_host::<T>(self.host(), component)
    }

    /// Migrates live ECS rows of `component` from `Old` (`Old::SCHEMA_FINGERPRINT`) to `New`
    /// (`New::SCHEMA_FINGERPRINT`, `New::BAM_FIELDS`, `New::REQUIRED_COMPONENTS`) via
    /// `BamHostApi::migrate_component_column` (slot 67) using a panic-guarded trampoline.
    #[inline]
    pub fn migrate_component_column<Old, New, F>(
        &mut self,
        component: BamComponent,
        migrate: F,
    ) -> FfiResult
    where
        Old: ModComponent + AbiPod,
        New: ModComponent + AbiPod,
        F: FnMut(&Old) -> Result<New, FfiResult>,
    {
        self.load_ctx.migrate_mod_component_column::<Old, New, F>(
            self.instance,
            component,
            0,
            migrate,
        )
    }

    /// Migrates live ECS rows of `component` from `Old: AbiPod` to `New: GuestComponent` with
    /// explicit schema fingerprints via `BamHostApi::migrate_component_column` (slot 67).
    pub fn migrate_pod_component_column<Old, New, F>(
        &mut self,
        component: BamComponent,
        old_fingerprint: u64,
        new_fingerprint: u64,
        migrate: F,
    ) -> FfiResult
    where
        Old: AbiPod,
        New: GuestComponent,
        F: FnMut(&Old) -> Result<New, FfiResult>,
    {
        let (qualified_name, uuid) =
            match qualify_component_identity::<New>(self.instance.manifest_name()) {
                Ok(id) => id,
                Err(err) => return err,
            };
        self.load_ctx.migrate_pod_component_column::<Old, New, F>(
            self.instance,
            component,
            old_fingerprint,
            qualified_name.as_c_str(),
            uuid,
            New::FLAGS,
            new_fingerprint,
            migrate,
        )
    }

    /// Emits a structured guest diagnostic message via `BamHostApi::log_diagnostic` (slot 50).
    #[inline]
    pub fn log_diagnostic(&self, level: u32, message: &str) -> FfiResult {
        self.host().log_diagnostic(level, message)
    }

    /// Records a guest-side fault message on this world's host context via
    /// `BamHostApi::report_guest_fault` (slot 51).
    #[inline]
    pub fn report_fault(&self, message: &str) -> FfiResult {
        self.host().report_guest_fault(message)
    }
}

macro_rules! impl_v03_host_forwarders {
    ($($ty:ty),+ $(,)?) => {
        $(
            impl<'a> $ty {
                /// Binds a canonical UTF-8 lookup `name` to `entity` in the host symbol registry
                /// via `BamHostApi::bind_named_object` (slot 56).
                #[inline]
                pub fn bind_named_object(&self, name: &str, entity: FfiEntity) -> FfiResult {
                    self.host().bind_named_object(name, entity)
                }

                /// Interns a UTF-8 symbol string into an `O(1)` comparable [`BamSymbol`] handle
                /// via `BamHostApi::intern_symbol` (slot 55).
                #[inline]
                pub fn intern_symbol(&self, symbol: &str) -> Result<BamSymbol, FfiResult> {
                    self.host().intern_symbol(symbol)
                }

                /// Resolves a registered component or resource strictly by its 128-bit
                /// [`ComponentUuid`] via `BamHostApi::resolve_component_by_uuid` (slot 52).
                #[inline]
                pub fn resolve_component_by_uuid(
                    &self,
                    uuid: ComponentUuid,
                ) -> Result<BamComponent, FfiResult> {
                    self.host().resolve_component_by_uuid(uuid)
                }

                /// Queries ABI `0.3.0` [`BamLayoutInfo`] metadata for `component` via
                /// `BamHostApi::component_layout_info` (slot 53).
                #[inline]
                pub fn component_layout_info(
                    &self,
                    component: BamComponent,
                ) -> Result<BamLayoutInfo, FfiResult> {
                    self.host().component_layout_info(component)
                }

                /// Emits a structured guest diagnostic message via `BamHostApi::log_diagnostic`
                /// (slot 50).
                #[inline]
                pub fn log_diagnostic(&self, level: u32, message: &str) -> FfiResult {
                    self.host().log_diagnostic(level, message)
                }

                /// Records a guest-side fault message on this world's host context via
                /// `BamHostApi::report_guest_fault` (slot 51).
                #[inline]
                pub fn report_fault(&self, message: &str) -> FfiResult {
                    self.host().report_guest_fault(message)
                }
            }
        )+
    };
}

impl_v03_host_forwarders!(SystemContext<'a>, RunConditionContext<'a>);

impl<'a> SystemContext<'a> {
    /// Resolves `T: ModComponent` and verifies its registered `size`, `align`, and
    /// `SCHEMA_FINGERPRINT` against the host.
    #[inline]
    pub fn verify_component_layout<T: ModComponent>(&self) -> Result<BamLayoutInfo, FfiResult> {
        resolve_and_verify_layout::<T>(self.host())
    }

    /// Verifies an already-resolved `component` handle's registered `size`, `align`, and
    /// `SCHEMA_FINGERPRINT` against `T: ModComponent`.
    #[inline]
    pub fn verify_component_layout_handle<T: ModComponent>(
        &self,
        component: BamComponent,
    ) -> Result<BamLayoutInfo, FfiResult> {
        verify_layout_for_host::<T>(self.host(), component)
    }

    /// Processes primary query `0` chunks in parallel across the host `ComputeTaskPool` in
    /// strictly read-only mode via `BamHostApi::par_for_each_chunk_readonly` (slot 59).
    ///
    /// Passes `&Chunk<'_, '_>` to `f`, preventing mutable column borrows at compile time, while
    /// the host rejects queries declaring writable terms with [`FfiResult::PERMISSION_DENIED`].
    #[inline]
    pub fn par_chunks_readonly<F>(&mut self, f: F) -> FfiResult
    where
        F: Fn(&Chunk<'_, '_>) + Sync,
    {
        self.iter().par_for_each_chunk_readonly(f)
    }

    /// Fallible variant of [`Self::par_chunks_readonly`] propagating non-`OK`
    /// [`IntoSystemResult`] returns.
    #[inline]
    pub fn par_try_chunks_readonly<F, R>(&mut self, f: F) -> FfiResult
    where
        F: Fn(&Chunk<'_, '_>) -> R + Sync,
        R: IntoSystemResult,
    {
        self.iter().par_try_for_each_chunk_readonly(f)
    }
}
