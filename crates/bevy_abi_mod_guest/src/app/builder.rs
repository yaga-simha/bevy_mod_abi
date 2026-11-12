//! `GuestApp` initialization session and unified `SystemBuilder` / `QueryBuilder` (`G-02`).

use core::ffi::{CStr, c_void};

use bevy_abi_mod_guest_sys::{
    GuestInstance, Host, PreparedRunIf, SystemRegistrationConfig, ValidatedLoadCtx,
    ValidatedSystemInvocation, resolve_component,
};
use bevy_abi_mod_sys::{
    AbiPod, AccessMode, BAM_SYSTEM_FLAG_MAIN_THREAD, BamComponent, BamHostApi, BamLoadCtx,
    BamTermDesc, ComponentUuid, FfiResult, MAX_QUERIES_PER_SYSTEM, MAX_TERMS_PER_QUERY, ScheduleId,
};

use crate::{
    app::context::{RunConditionContext, SystemContext},
    component::{GuestComponent, qualify_component_identity, resolve_guest_component},
};

mod private {
    pub trait Sealed {}
}

/// Return-type conversion trait permitting [`SystemBuilder::build`] closures to return
/// [`FfiResult`], `()`, or `Result<(), FfiResult>`.
pub trait IntoSystemResult: private::Sealed {
    /// Converts the callback return value into an [`FfiResult`].
    fn into_ffi_result(self) -> FfiResult;
}

impl private::Sealed for FfiResult {}
impl IntoSystemResult for FfiResult {
    #[inline]
    fn into_ffi_result(self) -> FfiResult {
        self
    }
}

impl private::Sealed for () {}
impl IntoSystemResult for () {
    #[inline]
    fn into_ffi_result(self) -> FfiResult {
        FfiResult::OK
    }
}

impl private::Sealed for Result<(), FfiResult> {}
impl IntoSystemResult for Result<(), FfiResult> {
    #[inline]
    fn into_ffi_result(self) -> FfiResult {
        match self {
            Ok(()) => FfiResult::OK,
            Err(err) => err,
        }
    }
}

/// Safe per-World mod initialization builder passed to [`GuestPlugin::build`](crate::GuestPlugin::build)
/// and [`export_mod!`](crate::export_mod).
///
/// Constructing `GuestApp` directly from raw C pointers in safe Rust is rejected at compile time:
///
/// ```compile_fail
/// use bevy_abi_mod_guest::GuestApp;
///
/// let _app = GuestApp::from_raw(core::ptr::null(), core::ptr::null_mut());
/// ```
///
/// Registering non-`AbiPod` types (such as `String` or `Vec<u8>`) via `register_component` is
/// rejected at compile time (`G-07`):
///
/// ```compile_fail
/// use bevy_abi_mod_guest::GuestApp;
///
/// fn register_bad(app: &mut GuestApp<'_>) {
///     let _ = app.register_component::<String>();
/// }
/// ```
pub struct GuestApp<'a> {
    instance: &'a mut GuestInstance,
    load_ctx: ValidatedLoadCtx<'a>,
}

impl<'a> GuestApp<'a> {
    /// Wraps a validated [`GuestInstance`] and [`ValidatedLoadCtx`] for the duration of
    /// `bevy_abi_mod_init`.
    #[inline]
    pub fn from_validated(instance: &'a mut GuestInstance, load_ctx: ValidatedLoadCtx<'a>) -> Self {
        Self { instance, load_ctx }
    }

    /// Returns the canonical manifest name of this mod instance.
    #[inline]
    pub fn manifest_name(&self) -> &str {
        self.instance.manifest_name()
    }

    /// Returns the cached [`Host`] table for this mod instance.
    #[inline]
    pub const fn host(&self) -> &Host {
        self.instance.host()
    }

    /// Returns the validated [`BamHostApi`] function table.
    #[inline]
    pub const fn host_api(&self) -> &BamHostApi {
        self.instance.host().api()
    }

    /// Returns the raw `*mut BamLoadCtx` pointer for low-level registration helpers.
    #[inline]
    pub const fn as_raw_load_ctx(&self) -> *mut BamLoadCtx {
        self.load_ctx.as_raw()
    }

    /// Borrows the underlying [`GuestInstance`] mutably (used by low-level `raw` helpers).
    #[inline]
    pub fn instance_mut(&mut self) -> &mut GuestInstance {
        self.instance
    }

    /// Reads the most recent human-readable UTF-8 diagnostic recorded on this world's host context.
    #[inline]
    pub fn last_error(&self) -> String {
        self.instance.host().last_error()
    }

    /// Resolves a custom `"bam.v0.<name>"` host symbol pointer.
    #[inline]
    pub fn resolve_custom_symbol(&self, symbol: &CStr) -> Result<*const c_void, FfiResult> {
        self.instance.host().resolve_custom_symbol(symbol)
    }

    /// Resolves a [`BamComponent`] handle for `T: GuestComponent` (`C-05`).
    ///
    /// Tries the manifest-qualified name first when `T::NAME` is unqualified, falling back to
    /// `(T::NAME, T::UUID)`.
    #[inline]
    pub fn resolve_component<T: GuestComponent>(&self) -> Result<BamComponent, FfiResult> {
        resolve_guest_component::<T>(self.instance.host(), self.instance.manifest_name())
    }

    /// Resolves a [`BamComponent`] handle by explicit `name` and/or `uuid`.
    #[inline]
    pub fn resolve_component_named(
        &self,
        name: Option<&CStr>,
        uuid: Option<ComponentUuid>,
    ) -> Result<BamComponent, FfiResult> {
        resolve_component(self.instance.host(), name, uuid)
    }

    /// Registers a cross-language `T: GuestComponent` (`AbiPod`) dynamic component on the host
    /// `World` (`G-07`, `C-10`), returning its 1-based [`BamComponent`] handle.
    ///
    /// Unqualified `T::NAME` values (without `"::"`) are automatically prefixed with
    /// `"<manifest.name>::"`.
    #[inline]
    pub fn register_component<T: GuestComponent>(&mut self) -> Result<BamComponent, FfiResult> {
        self.register_component_with_flags::<T>(T::FLAGS)
    }

    /// Registers `T: GuestComponent` with explicit `BAM_COMPONENT_FLAG_*` flags (e.g.
    /// `BAM_COMPONENT_FLAG_SPARSE_SET` or `BAM_COMPONENT_FLAG_IMMUTABLE`).
    pub fn register_component_with_flags<T: GuestComponent>(
        &mut self,
        flags: u32,
    ) -> Result<BamComponent, FfiResult> {
        let (qualified_name, uuid) =
            qualify_component_identity::<T>(self.instance.manifest_name())?;
        self.load_ctx.register_pod_component::<T>(
            self.instance,
            qualified_name.as_c_str(),
            uuid,
            flags,
        )
    }

    /// Registers an `AbiPod` component under an explicit `name`, `uuid`, and `flags`.
    #[inline]
    pub fn register_pod_component_named<T: AbiPod>(
        &mut self,
        name: &CStr,
        uuid: ComponentUuid,
        flags: u32,
    ) -> Result<BamComponent, FfiResult> {
        self.load_ctx
            .register_pod_component::<T>(self.instance, name, uuid, flags)
    }

    /// Allocates guest-owned data inside this per-World [`GuestInstance`], returning a stable
    /// `*mut T` valid until `bevy_abi_mod_shutdown` (or freed immediately on init failure).
    #[inline]
    pub fn alloc_user_data<T: Send + 'static>(&mut self, value: T) -> *mut T {
        self.instance.track_allocation(Box::new(value))
    }

    /// Registers a per-instance teardown closure executed during `bevy_abi_mod_shutdown` in LIFO
    /// order before closure allocations are freed.
    #[inline]
    pub fn on_shutdown<F>(&mut self, cleanup: F)
    where
        F: FnOnce() + Send + 'static,
    {
        self.instance.add_shutdown_hook(cleanup);
    }

    /// Starts building a unified guest ECS system named `name` (`G-02`).
    #[inline]
    pub fn system<'b>(&'b mut self, name: &'b CStr) -> SystemBuilder<'b, 'a> {
        SystemBuilder {
            app: self,
            name,
            queries: Vec::new(),
            system_resources: Vec::new(),
            schedule: ScheduleId::UPDATE,
            flags: 0,
            after: Vec::new(),
            before: Vec::new(),
            run_if: None,
            first_error: None,
        }
    }
}

/// Declarative query-clause builder passed to [`SystemBuilder::query`].
pub struct QueryBuilder<'b> {
    host: &'b Host,
    manifest_name: &'b str,
    terms: Vec<BamTermDesc>,
    first_error: Option<FfiResult>,
}

impl<'b> QueryBuilder<'b> {
    #[inline]
    fn record_error(&mut self, err: FfiResult) {
        if self.first_error.is_none() {
            self.first_error = Some(err);
        }
    }

    #[inline]
    fn push_term(&mut self, component: BamComponent, mode: AccessMode) {
        if !component.is_valid() {
            self.record_error(FfiResult::INVALID_COMPONENT);
            return;
        }
        if !mode.is_valid() {
            self.record_error(FfiResult::INVALID_ARGUMENT);
            return;
        }
        if self.terms.len() >= MAX_TERMS_PER_QUERY {
            self.record_error(FfiResult::CAPACITY_EXCEEDED);
            return;
        }
        self.terms.push(BamTermDesc::new(component, mode));
    }

    #[inline]
    fn push_guest_term<T: GuestComponent>(&mut self, mode: AccessMode) {
        match resolve_guest_component::<T>(self.host, self.manifest_name) {
            Ok(comp) => self.push_term(comp, mode),
            Err(err) => self.record_error(err),
        }
    }

    /// Appends a shared read-only (`&T`) entity column term.
    #[inline]
    pub fn read<T: GuestComponent>(mut self) -> Self {
        self.push_guest_term::<T>(AccessMode::READ);
        self
    }

    /// Appends an exclusive mutable (`&mut T`) entity column term.
    #[inline]
    pub fn write<T: GuestComponent>(mut self) -> Self {
        self.push_guest_term::<T>(AccessMode::WRITE);
        self
    }

    /// Appends an archetype presence filter (`With<T>`) term.
    #[inline]
    pub fn with<T: GuestComponent>(mut self) -> Self {
        self.push_guest_term::<T>(AccessMode::FILTER_ONLY);
        self
    }

    /// Appends an archetype absence filter (`Without<T>`) term.
    #[inline]
    pub fn without<T: GuestComponent>(mut self) -> Self {
        self.push_guest_term::<T>(AccessMode::WITHOUT);
        self
    }

    /// Appends an optional shared read-only (`Option<&T>`) column term.
    #[inline]
    pub fn optional_read<T: GuestComponent>(mut self) -> Self {
        self.push_guest_term::<T>(AccessMode::OPTIONAL_READ);
        self
    }

    /// Appends an optional exclusive mutable (`Option<&mut T>`) column term.
    #[inline]
    pub fn optional_write<T: GuestComponent>(mut self) -> Self {
        self.push_guest_term::<T>(AccessMode::OPTIONAL_WRITE);
        self
    }

    /// Appends a row-level change-tick filter with read-only column access (`Changed<T> + &T`).
    #[inline]
    pub fn changed<T: GuestComponent>(mut self) -> Self {
        self.push_guest_term::<T>(AccessMode::CHANGED);
        self
    }

    /// Appends a row-level added-tick filter with read-only column access (`Added<T> + &T`).
    #[inline]
    pub fn added<T: GuestComponent>(mut self) -> Self {
        self.push_guest_term::<T>(AccessMode::ADDED);
        self
    }

    /// Appends a shared singleton resource read (`Res<R>`) term to this query.
    #[inline]
    pub fn resource_read<R: GuestComponent>(mut self) -> Self {
        self.push_guest_term::<R>(AccessMode::RESOURCE_READ);
        self
    }

    /// Appends an exclusive singleton resource write (`ResMut<R>`) term to this query.
    #[inline]
    pub fn resource_write<R: GuestComponent>(mut self) -> Self {
        self.push_guest_term::<R>(AccessMode::RESOURCE_WRITE);
        self
    }

    /// Appends a pre-resolved [`BamComponent`] handle with explicit [`AccessMode`].
    #[inline]
    pub fn term(mut self, component: BamComponent, mode: AccessMode) -> Self {
        self.push_term(component, mode);
        self
    }

    /// Appends a pre-resolved shared read-only (`&T`) handle term.
    #[inline]
    pub fn read_handle(mut self, component: BamComponent) -> Self {
        self.push_term(component, AccessMode::READ);
        self
    }

    /// Appends a pre-resolved exclusive mutable (`&mut T`) handle term.
    #[inline]
    pub fn write_handle(mut self, component: BamComponent) -> Self {
        self.push_term(component, AccessMode::WRITE);
        self
    }

    /// Appends a pre-resolved `With<T>` filter handle term.
    #[inline]
    pub fn with_handle(mut self, component: BamComponent) -> Self {
        self.push_term(component, AccessMode::FILTER_ONLY);
        self
    }

    /// Appends a pre-resolved `Without<T>` filter handle term.
    #[inline]
    pub fn without_handle(mut self, component: BamComponent) -> Self {
        self.push_term(component, AccessMode::WITHOUT);
        self
    }

    /// Appends a pre-resolved `Option<&T>` handle term.
    #[inline]
    pub fn optional_read_handle(mut self, component: BamComponent) -> Self {
        self.push_term(component, AccessMode::OPTIONAL_READ);
        self
    }

    /// Appends a pre-resolved `Option<&mut T>` handle term.
    #[inline]
    pub fn optional_write_handle(mut self, component: BamComponent) -> Self {
        self.push_term(component, AccessMode::OPTIONAL_WRITE);
        self
    }

    /// Appends a pre-resolved `Changed<T>` handle term.
    #[inline]
    pub fn changed_handle(mut self, component: BamComponent) -> Self {
        self.push_term(component, AccessMode::CHANGED);
        self
    }

    /// Appends a pre-resolved `Added<T>` handle term.
    #[inline]
    pub fn added_handle(mut self, component: BamComponent) -> Self {
        self.push_term(component, AccessMode::ADDED);
        self
    }

    /// Appends a pre-resolved `Res<R>` handle term.
    #[inline]
    pub fn resource_read_handle(mut self, component: BamComponent) -> Self {
        self.push_term(component, AccessMode::RESOURCE_READ);
        self
    }

    /// Appends a pre-resolved `ResMut<R>` handle term.
    #[inline]
    pub fn resource_write_handle(mut self, component: BamComponent) -> Self {
        self.push_term(component, AccessMode::RESOURCE_WRITE);
        self
    }

    /// Resolves `name` (with optional `uuid`) on the host and appends a term with `mode`.
    #[inline]
    pub fn term_named(
        mut self,
        name: &CStr,
        uuid: Option<ComponentUuid>,
        mode: AccessMode,
    ) -> Self {
        match resolve_component(self.host, Some(name), uuid) {
            Ok(comp) => self.push_term(comp, mode),
            Err(err) => self.record_error(err),
        }
        self
    }
}

/// Unified fluent builder compiling queries, resources, schedule selection, ordering sets,
/// main-thread pinning, and optional `run_if` predicates into a single [`BamSystemDesc`](bevy_abi_mod_sys::BamSystemDesc) (`G-02`).
pub struct SystemBuilder<'b, 'a> {
    app: &'b mut GuestApp<'a>,
    name: &'b CStr,
    queries: Vec<Vec<BamTermDesc>>,
    system_resources: Vec<BamTermDesc>,
    schedule: ScheduleId,
    flags: u32,
    after: Vec<&'b CStr>,
    before: Vec<&'b CStr>,
    run_if: Option<PreparedRunIf>,
    first_error: Option<FfiResult>,
}

impl<'b, 'a> SystemBuilder<'b, 'a> {
    #[inline]
    fn record_error(&mut self, err: FfiResult) {
        if self.first_error.is_none() {
            self.first_error = Some(err);
        }
    }

    /// Appends a query clause (`0..=MAX_QUERIES_PER_SYSTEM`) configured via [`QueryBuilder`].
    pub fn query<F>(mut self, configure: F) -> Self
    where
        F: for<'q> FnOnce(QueryBuilder<'q>) -> QueryBuilder<'q>,
    {
        if self.queries.len() >= MAX_QUERIES_PER_SYSTEM {
            self.record_error(FfiResult::CAPACITY_EXCEEDED);
            return self;
        }
        let host = *self.app.host();
        let qb = QueryBuilder {
            host: &host,
            manifest_name: host.manifest_name(),
            terms: Vec::new(),
            first_error: None,
        };
        let finished = configure(qb);
        if let Some(err) = finished.first_error {
            self.record_error(err);
        } else {
            self.queries.push(finished.terms);
        }
        self
    }

    #[inline]
    fn push_system_resource_handle(&mut self, component: BamComponent, mode: AccessMode) {
        if !component.is_valid() {
            self.record_error(FfiResult::INVALID_COMPONENT);
            return;
        }
        if self.system_resources.len() >= MAX_TERMS_PER_QUERY {
            self.record_error(FfiResult::CAPACITY_EXCEEDED);
            return;
        }
        self.system_resources
            .push(BamTermDesc::new(component, mode));
    }

    /// Declares a shared singleton resource read (`Res<R>`), accessible via
    /// `ctx.resource::<R>(decl_index)` in declaration order (`0, 1, ...`).
    pub fn resource_read<R: GuestComponent>(mut self) -> Self {
        match self.app.resolve_component::<R>() {
            Ok(comp) => self.push_system_resource_handle(comp, AccessMode::RESOURCE_READ),
            Err(err) => self.record_error(err),
        }
        self
    }

    /// Declares an exclusive singleton resource write (`ResMut<R>`), accessible via
    /// `ctx.resource_mut::<R>(decl_index)` in declaration order (`0, 1, ...`).
    pub fn resource_write<R: GuestComponent>(mut self) -> Self {
        match self.app.resolve_component::<R>() {
            Ok(comp) => self.push_system_resource_handle(comp, AccessMode::RESOURCE_WRITE),
            Err(err) => self.record_error(err),
        }
        self
    }

    /// Declares a shared singleton resource read using a pre-resolved [`BamComponent`] handle.
    #[inline]
    pub fn resource_read_handle(mut self, component: BamComponent) -> Self {
        self.push_system_resource_handle(component, AccessMode::RESOURCE_READ);
        self
    }

    /// Declares an exclusive singleton resource write using a pre-resolved [`BamComponent`] handle.
    #[inline]
    pub fn resource_write_handle(mut self, component: BamComponent) -> Self {
        self.push_system_resource_handle(component, AccessMode::RESOURCE_WRITE);
        self
    }

    /// Selects the target Bevy [`ScheduleId`] (default [`ScheduleId::UPDATE`]).
    #[inline]
    pub fn schedule(mut self, schedule: ScheduleId) -> Self {
        self.schedule = schedule;
        self
    }

    /// Orders this system to run after `target_set_name` (`"system"` within the same mod or
    /// `"other_mod::system"` across mods).
    #[inline]
    pub fn after(mut self, target_set_name: &'b CStr) -> Self {
        self.after.push(target_set_name);
        self
    }

    /// Orders this system to run before `target_set_name`.
    #[inline]
    pub fn before(mut self, target_set_name: &'b CStr) -> Self {
        self.before.push(target_set_name);
        self
    }

    /// Pins this system onto Bevy's main thread (`BAM_SYSTEM_FLAG_MAIN_THREAD`).
    #[inline]
    pub fn main_thread(mut self) -> Self {
        self.flags |= BAM_SYSTEM_FLAG_MAIN_THREAD;
        self
    }

    /// Sets explicit `BAM_SYSTEM_FLAG_*` flags on this system descriptor.
    #[inline]
    pub fn flags(mut self, flags: u32) -> Self {
        self.flags |= flags;
        self
    }

    /// Attaches an optional run-condition predicate `|ctx: &RunConditionContext<'_>| -> bool`.
    ///
    /// For zero-argument predicates or plain `fn() -> bool` functions, use [`Self::run_if_fn`]
    /// or `.run_if(|_| ...)`.
    #[inline]
    pub fn run_if<F>(mut self, mut condition: F) -> Self
    where
        F: FnMut(&RunConditionContext<'_>) -> bool + Send + 'static,
    {
        if self.first_error.is_none() {
            let prepared = PreparedRunIf::new(self.app.instance, move |host| {
                let ctx = RunConditionContext::new(host);
                condition(&ctx)
            });
            self.run_if = Some(prepared);
        }
        self
    }

    /// Attaches a zero-argument run-condition predicate `|| -> bool`.
    #[inline]
    pub fn run_if_fn<F>(mut self, mut condition: F) -> Self
    where
        F: FnMut() -> bool + Send + 'static,
    {
        if self.first_error.is_none() {
            let prepared = PreparedRunIf::new(self.app.instance, move |_host| condition());
            self.run_if = Some(prepared);
        }
        self
    }

    /// Finalizes and registers the system on the host `World`, returning [`FfiResult::OK`] on
    /// success or the first registration/resolution error encountered.
    pub fn build<F, R>(self, mut callback: F) -> FfiResult
    where
        F: FnMut(&mut SystemContext<'_>) -> R + Send + 'static,
        R: IntoSystemResult,
    {
        if let Some(err) = self.first_error {
            return err;
        }
        let query_slices: Vec<&[BamTermDesc]> = self.queries.iter().map(Vec::as_slice).collect();
        let config = SystemRegistrationConfig {
            name: self.name,
            user_queries: &query_slices,
            system_resources: &self.system_resources,
            schedule: self.schedule,
            flags: self.flags,
            after: &self.after,
            before: &self.before,
        };
        self.app.load_ctx.register_system(
            self.app.instance,
            &config,
            self.run_if,
            move |inv: &mut ValidatedSystemInvocation<'_>| {
                let mut ctx = SystemContext::from_invocation(inv);
                callback(&mut ctx).into_ffi_result()
            },
        )
    }
}
