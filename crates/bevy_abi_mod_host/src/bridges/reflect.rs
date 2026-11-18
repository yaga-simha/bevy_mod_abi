//! Safe (`#![forbid(unsafe_code)]`) dynamic `bevy_reflect`, Bevy Remote Protocol (`bevy_remote`),
//! and `bevy_settings` bridges for `bevy_abi_mod` (`feature = "bridge-reflect"`).

use bevy_abi_mod_host_sys::loader::ExportedFieldRecord;
use bevy_abi_mod_host_sys::{
    ComponentCollisionError, ComponentOptions, ModRegistry, WitnessFieldDescriptor,
    apply_entity_component_fields, apply_resource_fields, export_entity_component_fields,
    export_resource_fields, insert_or_apply_entity_component_fields,
    inspect_component_witness_fields, inspect_component_witness_fields_in_registry,
    reflect_entity_component, reflect_entity_component_mut,
};
use bevy_abi_mod_sys::{BamComponent, FfiResult, ModComponent};
use bevy_app::{App, Plugin};
use bevy_ecs::change_detection::Mut;
use bevy_ecs::component::Component;
use bevy_ecs::entity::Entity;
use bevy_ecs::prelude::{In, Resource};
use bevy_ecs::reflect::{AppTypeRegistry, ReflectComponent, ReflectResource};
use bevy_ecs::world::World;
use bevy_platform::collections::HashMap;
use bevy_reflect::serde::{ReflectSerializer, TypedReflectDeserializer, TypedReflectSerializer};
use bevy_reflect::structs::{DynamicStruct, Struct};
use bevy_reflect::{
    FromReflect, GetTypeRegistration, PartialReflect, Reflect, ReflectCloneError, ReflectFromPtr,
    TypePath, TypeRegistry,
};
use bevy_remote::{
    BrpError, BrpRequest, BrpResult, RemoteMethodSystemId, RemoteMethods, RemotePlugin, error_codes,
};
use bevy_settings::{ReflectSettingsGroup, SettingsGroup};
use core::any::TypeId;
use core::fmt;
use serde::de::DeserializeSeed;
use serde_json::{Map, Value};
use std::path::Path;

/// BRP method path listing mod-registered components, resources, and witness schemas.
pub const BRP_METHOD_LIST_COMPONENTS: &str = "bevy_abi_mod/list_components";
/// BRP method path inspecting mod-registered dynamic or reflected components on an entity.
pub const BRP_METHOD_GET_COMPONENTS: &str = "bevy_abi_mod/get_components";
/// BRP method path mutating mod-registered dynamic or reflected components on an entity.
pub const BRP_METHOD_MUTATE_COMPONENTS: &str = "bevy_abi_mod/mutate_components";
/// BRP method path inserting or updating a witness-backed or reflected component on an entity.
pub const BRP_METHOD_INSERT_COMPONENTS: &str = "bevy_abi_mod/insert_components";
/// BRP method path inspecting a mod-registered singleton resource.
pub const BRP_METHOD_GET_RESOURCES: &str = "bevy_abi_mod/get_resources";
/// BRP method path mutating a mod-registered singleton resource.
pub const BRP_METHOD_MUTATE_RESOURCES: &str = "bevy_abi_mod/mutate_resources";

/// Errors returned by dynamic reflection and `PartialReflect::to_dynamic` helpers.
#[derive(Debug)]
pub enum ModReflectError {
    /// Underlying FFI / registry failure.
    Ffi(FfiResult),
    /// Bevy `PartialReflect::to_dynamic` clone failure (`ReflectCloneError`).
    Clone(ReflectCloneError),
    /// Type is not registered in `AppTypeRegistry` or lacks `ReflectFromPtr`.
    MissingTypeRegistration(String),
}

impl fmt::Display for ModReflectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ffi(err) => write!(f, "FFI error {err:?}"),
            Self::Clone(err) => write!(f, "reflect clone error: {err}"),
            Self::MissingTypeRegistration(name) => {
                write!(f, "missing AppTypeRegistry registration for '{name}'")
            }
        }
    }
}

impl std::error::Error for ModReflectError {}

impl From<FfiResult> for ModReflectError {
    fn from(err: FfiResult) -> Self {
        Self::Ffi(err)
    }
}

impl From<ReflectCloneError> for ModReflectError {
    fn from(err: ReflectCloneError) -> Self {
        Self::Clone(err)
    }
}

/// Errors returned by `bevy_settings` TOML bridge operations.
#[derive(Debug)]
pub enum ModSettingsError {
    /// Underlying FFI / witness export or import error.
    Ffi(FfiResult),
    /// TOML serialization error.
    TomlSerialize(toml::ser::Error),
    /// TOML deserialization error.
    TomlDeserialize(toml::de::Error),
    /// Filesystem I/O error.
    Io(std::io::Error),
}

impl fmt::Display for ModSettingsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ffi(err) => write!(f, "settings FFI error {err:?}"),
            Self::TomlSerialize(err) => write!(f, "TOML serialize error: {err}"),
            Self::TomlDeserialize(err) => write!(f, "TOML deserialize error: {err}"),
            Self::Io(err) => write!(f, "settings I/O error: {err}"),
        }
    }
}

impl std::error::Error for ModSettingsError {}

impl From<FfiResult> for ModSettingsError {
    fn from(err: FfiResult) -> Self {
        Self::Ffi(err)
    }
}

impl From<toml::ser::Error> for ModSettingsError {
    fn from(err: toml::ser::Error) -> Self {
        Self::TomlSerialize(err)
    }
}

impl From<toml::de::Error> for ModSettingsError {
    fn from(err: toml::de::Error) -> Self {
        Self::TomlDeserialize(err)
    }
}

impl From<std::io::Error> for ModSettingsError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}

/// Cached schema metadata synthesized from `"bam.v0.<name>.field_info"` witnesses for one
/// mod-registered component or resource.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WitnessReflectSchema {
    /// Handle in [`ModRegistry`].
    pub component: BamComponent,
    /// Canonical component/resource name.
    pub canonical_name: String,
    /// `true` when this handle refers to a singleton `Resource`.
    pub is_resource: bool,
    /// `true` when mutations are disallowed (`BAM_STORAGE_FLAG_IMMUTABLE`).
    pub is_immutable: bool,
    /// Witness field descriptors in declaration order.
    pub fields: Vec<WitnessFieldDescriptor>,
}

/// Typed `Reflect` bridge descriptor recorded for host-registered `Reflect` components/resources.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TypedReflectBridgeEntry {
    /// Handle in [`ModRegistry`].
    pub component: BamComponent,
    /// Canonical name in [`ModRegistry`].
    pub canonical_name: String,
    /// Rust [`TypeId`] registered in [`AppTypeRegistry`].
    pub type_id: TypeId,
    /// Full Bevy `TypePath` string.
    pub type_path: String,
    /// `true` when this handle refers to a singleton `Resource`.
    pub is_resource: bool,
}

/// Host resource caching synthesized witness schemas and typed `Reflect` bridge entries.
#[derive(Resource, Default, Clone)]
pub struct ModReflectRegistry {
    witness_schemas: HashMap<BamComponent, WitnessReflectSchema>,
    typed_entries: HashMap<BamComponent, TypedReflectBridgeEntry>,
}

impl ModReflectRegistry {
    /// Returns the synthesized witness schema for `component`, if registered.
    #[inline]
    pub fn witness_schema(&self, component: BamComponent) -> Option<&WitnessReflectSchema> {
        self.witness_schemas.get(&component)
    }

    /// Returns the typed `Reflect` bridge entry for `component`, if registered.
    #[inline]
    pub fn typed_entry(&self, component: BamComponent) -> Option<&TypedReflectBridgeEntry> {
        self.typed_entries.get(&component)
    }

    /// Iterates over all cached witness schemas.
    #[inline]
    pub fn witness_schemas(&self) -> impl Iterator<Item = &WitnessReflectSchema> {
        self.witness_schemas.values()
    }

    /// Iterates over all registered typed `Reflect` bridge entries.
    #[inline]
    pub fn typed_entries(&self) -> impl Iterator<Item = &TypedReflectBridgeEntry> {
        self.typed_entries.values()
    }
}

/// Target binding inside [`ModSettingsRegistry`].
#[derive(Clone, Debug)]
enum SettingsBindingKind {
    /// Witness-backed mod resource (`export_resource_fields` / `apply_resource_fields`).
    WitnessResource(BamComponent),
    /// Native Bevy `SettingsGroup` + `Reflect` resource registered in `AppTypeRegistry`.
    ReflectedSettingsGroup(TypeId),
}

/// Entry mapping a TOML `[group]` table name to a live host/mod resource.
#[derive(Clone, Debug)]
struct ModSettingsBinding {
    group_name: String,
    kind: SettingsBindingKind,
}

/// Host resource mapping TOML `[group]` sections to mod-accessible resources (both witness-backed
/// dynamic resources and typed `SettingsGroup` resources).
#[derive(Resource, Default, Clone)]
pub struct ModSettingsRegistry {
    bindings: Vec<ModSettingsBinding>,
}

impl ModSettingsRegistry {
    /// Returns the number of registered settings group bindings.
    #[inline]
    pub fn len(&self) -> usize {
        self.bindings.len()
    }

    /// Returns `true` if no settings groups are registered.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.bindings.is_empty()
    }
}

/// Bevy [`Plugin`] enabling dynamic `bevy_reflect` schema synthesis, `bevy_remote` BRP endpoints,
/// and `bevy_settings` TOML synchronization for `ModRegistry` components and resources.
#[derive(Default, Debug, Clone, Copy)]
pub struct ModReflectPlugin;

impl Plugin for ModReflectPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<AppTypeRegistry>();
        app.init_resource::<ModReflectRegistry>();
        app.init_resource::<ModSettingsRegistry>();
        register_standard_witness_types(&app.world().resource::<AppTypeRegistry>().clone());
    }

    fn finish(&self, app: &mut App) {
        if app.world().contains_resource::<RemoteMethods>() {
            install_mod_brp_methods(app.world_mut());
        }
        let _ = sync_witness_reflection_schemas(app.world_mut());
    }
}

fn register_standard_witness_types(app_types: &AppTypeRegistry) {
    let mut reg = app_types.write();
    reg.register::<f32>();
    reg.register::<f64>();
    reg.register::<i8>();
    reg.register::<i16>();
    reg.register::<i32>();
    reg.register::<i64>();
    reg.register::<i128>();
    reg.register::<u8>();
    reg.register::<u16>();
    reg.register::<u32>();
    reg.register::<u64>();
    reg.register::<u128>();
    reg.register::<bool>();
    reg.register::<String>();
    reg.register::<Vec<u8>>();
    reg.register::<Vec<u32>>();
    reg.register::<Vec<i32>>();
    reg.register::<Vec<f32>>();
    reg.register::<Option<u32>>();
    reg.register::<Option<i32>>();
    reg.register::<Option<f32>>();
    reg.register::<Option<String>>();
}

/// Extension trait on Bevy [`RemotePlugin`] attaching `bevy_abi_mod/*` BRP inspection & mutation
/// methods before adding [`RemotePlugin`] to an [`App`].
pub trait ModReflectRemoteExt {
    /// Registers all `bevy_abi_mod/*` dynamic reflection BRP endpoints onto `self`.
    fn with_mod_reflect_methods(self) -> Self;
}

impl ModReflectRemoteExt for RemotePlugin {
    fn with_mod_reflect_methods(self) -> Self {
        self.with_method_main(BRP_METHOD_LIST_COMPONENTS, brp_list_components_system)
            .with_method_main(BRP_METHOD_GET_COMPONENTS, brp_get_components_system)
            .with_method_main(BRP_METHOD_MUTATE_COMPONENTS, brp_mutate_components_system)
            .with_method_main(BRP_METHOD_INSERT_COMPONENTS, brp_insert_components_system)
            .with_method_main(BRP_METHOD_GET_RESOURCES, brp_get_resources_system)
            .with_method_main(BRP_METHOD_MUTATE_RESOURCES, brp_mutate_resources_system)
    }
}

/// Installs all `bevy_abi_mod/*` BRP handlers into an already-built `World`'s [`RemoteMethods`]
/// resource (no-op if [`RemoteMethods`] is absent).
pub fn install_mod_brp_methods(world: &mut World) {
    if !world.contains_resource::<RemoteMethods>() {
        return;
    }
    let pairs: [(&str, RemoteMethodSystemId); 6] = [
        (
            BRP_METHOD_LIST_COMPONENTS,
            RemoteMethodSystemId::Instant(world.register_system(brp_list_components_system)),
        ),
        (
            BRP_METHOD_GET_COMPONENTS,
            RemoteMethodSystemId::Instant(world.register_system(brp_get_components_system)),
        ),
        (
            BRP_METHOD_MUTATE_COMPONENTS,
            RemoteMethodSystemId::Instant(world.register_system(brp_mutate_components_system)),
        ),
        (
            BRP_METHOD_INSERT_COMPONENTS,
            RemoteMethodSystemId::Instant(world.register_system(brp_insert_components_system)),
        ),
        (
            BRP_METHOD_GET_RESOURCES,
            RemoteMethodSystemId::Instant(world.register_system(brp_get_resources_system)),
        ),
        (
            BRP_METHOD_MUTATE_RESOURCES,
            RemoteMethodSystemId::Instant(world.register_system(brp_mutate_resources_system)),
        ),
    ];
    let mut remote = world.resource_mut::<RemoteMethods>();
    for (name, id) in pairs {
        remote.insert(name, id);
    }
}

/// Synchronizes all mod-registered components & resources that emit `"bam.v0.<name>.field_info"`
/// witnesses into [`ModReflectRegistry`], returning the count of discovered schemas.
pub fn sync_witness_reflection_schemas(world: &mut World) -> usize {
    if !world.contains_resource::<ModReflectRegistry>() {
        world.insert_resource(ModReflectRegistry::default());
    }
    let candidates: Vec<(BamComponent, String, bool, bool)> = {
        let Some(reg) = world.get_resource::<ModRegistry>() else {
            return 0;
        };
        reg.snapshot()
            .entries()
            .iter()
            .filter(|e| !e.is_message())
            .map(|e| {
                (
                    e.handle,
                    e.name.to_string(),
                    e.is_resource(),
                    e.is_immutable(),
                )
            })
            .collect()
    };

    let mut discovered = Vec::new();
    for (handle, canonical_name, is_resource, is_immutable) in candidates {
        let fields = inspect_component_witness_fields(world, handle).or_else(|| {
            let reg = world.get_resource::<ModRegistry>()?;
            inspect_component_witness_fields_in_registry(reg, handle)
        });
        if let Some(fields) = fields {
            discovered.push((
                handle,
                WitnessReflectSchema {
                    component: handle,
                    canonical_name,
                    is_resource,
                    is_immutable,
                    fields,
                },
            ));
        }
    }

    let count = discovered.len();
    let mut reflect_reg = world.resource_mut::<ModReflectRegistry>();
    for (handle, schema) in discovered {
        reflect_reg.witness_schemas.insert(handle, schema);
    }
    count
}

/// Safe [`World`] extension methods for `bridge-reflect`, witness `DynamicStruct` round-trips,
/// `bevy_remote` BRP dispatch, and `bevy_settings` TOML sync (`#![forbid(unsafe_code)]`).
pub trait ModReflectWorldExt {
    /// Registers host component `T` in both [`ModRegistry`] and [`AppTypeRegistry`] (recording
    /// `ReflectComponent` and `ReflectFromPtr` metadata).
    fn register_reflected_component<
        T: Component + ModComponent + Reflect + FromReflect + TypePath + GetTypeRegistration,
    >(
        &mut self,
        options: ComponentOptions<T>,
    ) -> Result<BamComponent, ComponentCollisionError>;

    /// Registers host resource `R` in both [`ModRegistry`] and [`AppTypeRegistry`].
    fn register_reflected_resource<
        R: Resource + ModComponent + Reflect + FromReflect + TypePath + GetTypeRegistration,
    >(
        &mut self,
    ) -> Result<BamComponent, ComponentCollisionError>;

    /// Registers host `SettingsGroup` resource `S` in [`ModRegistry`], [`AppTypeRegistry`]
    /// (with `ReflectSettingsGroup`, `ReflectDefault`, `ReflectResource`, `ReflectComponent`), and
    /// [`ModSettingsRegistry`].
    fn expose_mod_settings_group<
        S: SettingsGroup
            + Resource
            + ModComponent
            + Reflect
            + FromReflect
            + TypePath
            + GetTypeRegistration,
    >(
        &mut self,
    ) -> Result<BamComponent, ComponentCollisionError>;

    /// Binds a witness-backed mod singleton resource `resource` to TOML section `[group_name]` in
    /// [`ModSettingsRegistry`].
    fn register_mod_settings_resource(
        &mut self,
        resource: BamComponent,
        group_name: impl Into<String>,
    ) -> Result<(), FfiResult>;

    /// Synthesizes and caches witness schema metadata for `component` in [`ModReflectRegistry`].
    fn register_witness_reflection(
        &mut self,
        component: BamComponent,
    ) -> Result<WitnessReflectSchema, FfiResult>;

    /// Borrows a typed `Reflect` component from `entity` via [`ReflectFromPtr`] (`wave3_helpers`).
    fn reflect_mod_component(
        &self,
        entity: Entity,
        component: BamComponent,
    ) -> Result<&dyn Reflect, FfiResult>;

    /// Mutably visits a typed `Reflect` component on `entity` inside `f` via [`ReflectFromPtr`]
    /// (`wave3_helpers::reflect_entity_component_mut`).
    fn with_reflect_mod_component_mut<R>(
        &mut self,
        entity: Entity,
        component: BamComponent,
        f: impl FnOnce(Mut<'_, dyn Reflect>) -> R,
    ) -> Result<R, FfiResult>;

    /// Clones a mod component on `entity` into a boxed dynamic representation using Bevy `0.20`'s
    /// fallible [`PartialReflect::to_dynamic`] (for typed `Reflect` types) or witness
    /// `DynamicStruct` synthesis (for dynamic C-ABI components).
    fn clone_mod_component_dynamic(
        &self,
        entity: Entity,
        component: BamComponent,
    ) -> Result<Box<dyn PartialReflect>, ModReflectError>;

    /// Exports an entity's witness-backed component into a synthesized [`DynamicStruct`].
    fn export_witness_dynamic_struct(
        &self,
        entity: Entity,
        component: BamComponent,
    ) -> Result<DynamicStruct, FfiResult>;

    /// Applies fields from `dyn_struct` back onto an existing entity component via
    /// `"bam.v0.<name>.import_field"`.
    fn apply_witness_dynamic_struct(
        &mut self,
        entity: Entity,
        component: BamComponent,
        dyn_struct: &DynamicStruct,
    ) -> Result<(), FfiResult>;

    /// Exports a witness-backed singleton resource into a synthesized [`DynamicStruct`].
    fn export_witness_resource_dynamic_struct(
        &self,
        resource: BamComponent,
    ) -> Result<DynamicStruct, FfiResult>;

    /// Applies fields from `dyn_struct` back onto an existing singleton resource via
    /// `"bam.v0.<name>.import_field"`.
    fn apply_witness_resource_dynamic_struct(
        &mut self,
        resource: BamComponent,
        dyn_struct: &DynamicStruct,
    ) -> Result<(), FfiResult>;

    /// Executes a Bevy Remote Protocol [`BrpRequest`] synchronously against `self`.
    fn dispatch_mod_brp_request(&mut self, request: BrpRequest) -> BrpResult;

    /// Serializes all registered mod settings groups (`SettingsGroup` + witness resources) to TOML.
    fn export_mod_settings_toml(&self) -> Result<String, ModSettingsError>;

    /// Deserializes TOML settings sections and applies matching values into live world resources.
    fn import_mod_settings_toml(&mut self, toml_str: &str) -> Result<usize, ModSettingsError>;

    /// Exports TOML settings directly to `path`.
    fn save_mod_settings_file(&self, path: impl AsRef<Path>) -> Result<(), ModSettingsError>;

    /// Reads TOML settings from `path` and applies them to live world resources.
    fn load_mod_settings_file(&mut self, path: impl AsRef<Path>)
    -> Result<usize, ModSettingsError>;
}

impl ModReflectWorldExt for World {
    fn register_reflected_component<
        T: Component + ModComponent + Reflect + FromReflect + TypePath + GetTypeRegistration,
    >(
        &mut self,
        options: ComponentOptions<T>,
    ) -> Result<BamComponent, ComponentCollisionError> {
        if !self.contains_resource::<ModRegistry>() {
            self.insert_resource(ModRegistry::new());
        }
        if !self.contains_resource::<AppTypeRegistry>() {
            self.insert_resource(AppTypeRegistry::default());
        }
        if !self.contains_resource::<ModReflectRegistry>() {
            self.insert_resource(ModReflectRegistry::default());
        }

        let comp_id = self.register_component::<T>();
        let handle = self
            .resource_mut::<ModRegistry>()
            .register_component::<T>(comp_id, options)?;

        {
            let app_types = self.resource::<AppTypeRegistry>().clone();
            let mut types = app_types.write();
            types.register::<T>();
            types.register_type_data::<T, ReflectComponent>();
            types.register_type_data::<T, ReflectFromPtr>();
        }

        self.resource_mut::<ModReflectRegistry>()
            .typed_entries
            .insert(
                handle,
                TypedReflectBridgeEntry {
                    component: handle,
                    canonical_name: T::CANONICAL_NAME.to_owned(),
                    type_id: TypeId::of::<T>(),
                    type_path: T::type_path().to_owned(),
                    is_resource: false,
                },
            );
        let _ = self.register_witness_reflection(handle);
        Ok(handle)
    }

    fn register_reflected_resource<
        R: Resource + ModComponent + Reflect + FromReflect + TypePath + GetTypeRegistration,
    >(
        &mut self,
    ) -> Result<BamComponent, ComponentCollisionError> {
        if !self.contains_resource::<ModRegistry>() {
            self.insert_resource(ModRegistry::new());
        }
        if !self.contains_resource::<AppTypeRegistry>() {
            self.insert_resource(AppTypeRegistry::default());
        }
        if !self.contains_resource::<ModReflectRegistry>() {
            self.insert_resource(ModReflectRegistry::default());
        }

        let comp_id = self.register_component::<R>();
        let handle = self
            .resource_mut::<ModRegistry>()
            .register_resource::<R>(comp_id)?;

        {
            let app_types = self.resource::<AppTypeRegistry>().clone();
            let mut types = app_types.write();
            types.register::<R>();
            types.register_type_data::<R, ReflectResource>();
            types.register_type_data::<R, ReflectComponent>();
            types.register_type_data::<R, ReflectFromPtr>();
        }

        self.resource_mut::<ModReflectRegistry>()
            .typed_entries
            .insert(
                handle,
                TypedReflectBridgeEntry {
                    component: handle,
                    canonical_name: R::CANONICAL_NAME.to_owned(),
                    type_id: TypeId::of::<R>(),
                    type_path: R::type_path().to_owned(),
                    is_resource: true,
                },
            );
        let _ = self.register_witness_reflection(handle);
        Ok(handle)
    }

    fn expose_mod_settings_group<
        S: SettingsGroup
            + Resource
            + ModComponent
            + Reflect
            + FromReflect
            + TypePath
            + GetTypeRegistration,
    >(
        &mut self,
    ) -> Result<BamComponent, ComponentCollisionError> {
        if !self.contains_resource::<S>() {
            self.insert_resource(S::default());
        }
        let handle = self.register_reflected_resource::<S>()?;
        {
            let app_types = self.resource::<AppTypeRegistry>().clone();
            let mut types = app_types.write();
            types.register_type_data::<S, ReflectSettingsGroup>();
            types.register_type_data::<S, bevy_reflect::prelude::ReflectDefault>();
        }
        if !self.contains_resource::<ModSettingsRegistry>() {
            self.insert_resource(ModSettingsRegistry::default());
        }
        let mut settings_reg = self.resource_mut::<ModSettingsRegistry>();
        let group_name = S::settings_group_name().to_owned();
        if !settings_reg
            .bindings
            .iter()
            .any(|b| b.group_name == group_name)
        {
            settings_reg.bindings.push(ModSettingsBinding {
                group_name,
                kind: SettingsBindingKind::ReflectedSettingsGroup(TypeId::of::<S>()),
            });
        }
        Ok(handle)
    }

    fn register_mod_settings_resource(
        &mut self,
        resource: BamComponent,
        group_name: impl Into<String>,
    ) -> Result<(), FfiResult> {
        let entry = self
            .get_resource::<ModRegistry>()
            .and_then(|r| r.get_by_handle(resource))
            .ok_or(FfiResult::INVALID_COMPONENT)?;
        if !entry.is_resource() {
            return Err(FfiResult::INVALID_COMPONENT);
        }
        let _ = self.register_witness_reflection(resource)?;
        if !self.contains_resource::<ModSettingsRegistry>() {
            self.insert_resource(ModSettingsRegistry::default());
        }
        let group_name = group_name.into();
        if group_name.is_empty() {
            return Err(FfiResult::INVALID_ARGUMENT);
        }
        let mut settings_reg = self.resource_mut::<ModSettingsRegistry>();
        settings_reg.bindings.retain(|b| b.group_name != group_name);
        settings_reg.bindings.push(ModSettingsBinding {
            group_name,
            kind: SettingsBindingKind::WitnessResource(resource),
        });
        Ok(())
    }

    fn register_witness_reflection(
        &mut self,
        component: BamComponent,
    ) -> Result<WitnessReflectSchema, FfiResult> {
        let (canonical_name, is_resource, is_immutable) = {
            let reg = self
                .get_resource::<ModRegistry>()
                .ok_or(FfiResult::INVALID_STATE)?;
            let entry = reg
                .get_by_handle(component)
                .ok_or(FfiResult::INVALID_COMPONENT)?;
            if entry.is_message() {
                return Err(FfiResult::INVALID_COMPONENT);
            }
            (
                entry.name.to_string(),
                entry.is_resource(),
                entry.is_immutable(),
            )
        };
        let fields = inspect_component_witness_fields(self, component)
            .or_else(|| {
                let reg = self.get_resource::<ModRegistry>()?;
                inspect_component_witness_fields_in_registry(reg, component)
            })
            .ok_or(FfiResult::UNSUPPORTED)?;
        let schema = WitnessReflectSchema {
            component,
            canonical_name,
            is_resource,
            is_immutable,
            fields,
        };
        if !self.contains_resource::<ModReflectRegistry>() {
            self.insert_resource(ModReflectRegistry::default());
        }
        self.resource_mut::<ModReflectRegistry>()
            .witness_schemas
            .insert(component, schema.clone());
        Ok(schema)
    }

    fn reflect_mod_component(
        &self,
        entity: Entity,
        component: BamComponent,
    ) -> Result<&dyn Reflect, FfiResult> {
        let reg = self
            .get_resource::<ModRegistry>()
            .ok_or(FfiResult::INVALID_STATE)?;
        let entry = reg
            .get_by_handle(component)
            .ok_or(FfiResult::INVALID_COMPONENT)?;
        let entity_ref = self
            .get_entity(entity)
            .map_err(|_| FfiResult::INVALID_ENTITY)?;
        let info = self
            .components()
            .get_info(entry.component_id)
            .ok_or(FfiResult::INVALID_COMPONENT)?;
        let type_id = info.type_id().ok_or(FfiResult::UNSUPPORTED)?;
        let app_types = self
            .get_resource::<AppTypeRegistry>()
            .ok_or(FfiResult::INVALID_STATE)?;
        let types = app_types.read();
        let registration = types.get(type_id).ok_or(FfiResult::NOT_FOUND)?;
        let reflect_from_ptr = registration
            .data::<ReflectFromPtr>()
            .ok_or(FfiResult::UNSUPPORTED)?;
        reflect_entity_component(self, entity_ref, entry.component_id, reflect_from_ptr)
            .ok_or(FfiResult::NOT_FOUND)
    }

    fn with_reflect_mod_component_mut<R>(
        &mut self,
        entity: Entity,
        component: BamComponent,
        f: impl FnOnce(Mut<'_, dyn Reflect>) -> R,
    ) -> Result<R, FfiResult> {
        with_reflect_mod_component_mut(self, entity, component, f)
    }

    fn clone_mod_component_dynamic(
        &self,
        entity: Entity,
        component: BamComponent,
    ) -> Result<Box<dyn PartialReflect>, ModReflectError> {
        if let Ok(typed) = self.reflect_mod_component(entity, component) {
            return typed.to_dynamic().map_err(ModReflectError::from);
        }
        let dyn_struct = self.export_witness_dynamic_struct(entity, component)?;
        Ok(Box::new(dyn_struct))
    }

    fn export_witness_dynamic_struct(
        &self,
        entity: Entity,
        component: BamComponent,
    ) -> Result<DynamicStruct, FfiResult> {
        let records = export_entity_component_fields(self, entity, component)?;
        Ok(records_to_dynamic_struct(&records))
    }

    fn apply_witness_dynamic_struct(
        &mut self,
        entity: Entity,
        component: BamComponent,
        dyn_struct: &DynamicStruct,
    ) -> Result<(), FfiResult> {
        let schema = self
            .get_resource::<ModReflectRegistry>()
            .and_then(|r| r.witness_schema(component).cloned())
            .or_else(|| {
                let fields = inspect_component_witness_fields(self, component)?;
                let entry = self
                    .get_resource::<ModRegistry>()?
                    .get_by_handle(component)?;
                Some(WitnessReflectSchema {
                    component,
                    canonical_name: entry.name.to_string(),
                    is_resource: entry.is_resource(),
                    is_immutable: entry.is_immutable(),
                    fields,
                })
            })
            .ok_or(FfiResult::UNSUPPORTED)?;

        let records = dynamic_struct_to_records(dyn_struct, &schema.fields)?;
        apply_entity_component_fields(self, entity, component, &records)
    }

    fn export_witness_resource_dynamic_struct(
        &self,
        resource: BamComponent,
    ) -> Result<DynamicStruct, FfiResult> {
        let records = export_resource_fields(self, resource)?;
        Ok(records_to_dynamic_struct(&records))
    }

    fn apply_witness_resource_dynamic_struct(
        &mut self,
        resource: BamComponent,
        dyn_struct: &DynamicStruct,
    ) -> Result<(), FfiResult> {
        let schema = self
            .get_resource::<ModReflectRegistry>()
            .and_then(|r| r.witness_schema(resource).cloned())
            .or_else(|| {
                let fields = inspect_component_witness_fields(self, resource)?;
                let entry = self
                    .get_resource::<ModRegistry>()?
                    .get_by_handle(resource)?;
                Some(WitnessReflectSchema {
                    component: resource,
                    canonical_name: entry.name.to_string(),
                    is_resource: entry.is_resource(),
                    is_immutable: entry.is_immutable(),
                    fields,
                })
            })
            .ok_or(FfiResult::UNSUPPORTED)?;

        let records = dynamic_struct_to_records(dyn_struct, &schema.fields)?;
        apply_resource_fields(self, resource, &records)
    }

    fn dispatch_mod_brp_request(&mut self, request: BrpRequest) -> BrpResult {
        handle_mod_brp_request(self, &request)
    }

    fn export_mod_settings_toml(&self) -> Result<String, ModSettingsError> {
        export_mod_settings_toml_from_world(self)
    }

    fn import_mod_settings_toml(&mut self, toml_str: &str) -> Result<usize, ModSettingsError> {
        import_mod_settings_toml_into_world(self, toml_str)
    }

    fn save_mod_settings_file(&self, path: impl AsRef<Path>) -> Result<(), ModSettingsError> {
        let content = self.export_mod_settings_toml()?;
        if let Some(parent) = path.as_ref().parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, content)?;
        Ok(())
    }

    fn load_mod_settings_file(
        &mut self,
        path: impl AsRef<Path>,
    ) -> Result<usize, ModSettingsError> {
        let content = std::fs::read_to_string(path)?;
        self.import_mod_settings_toml(&content)
    }
}

/// Mutably visits a typed `Reflect` component on `entity` inside `f` via [`ReflectFromPtr`]
/// (`wave3_helpers::reflect_entity_component_mut`).
pub fn with_reflect_mod_component_mut<R>(
    world: &mut World,
    entity: Entity,
    component: BamComponent,
    f: impl FnOnce(Mut<'_, dyn Reflect>) -> R,
) -> Result<R, FfiResult> {
    let (comp_id, is_immutable) = {
        let reg = world
            .get_resource::<ModRegistry>()
            .ok_or(FfiResult::INVALID_STATE)?;
        let entry = reg
            .get_by_handle(component)
            .ok_or(FfiResult::INVALID_COMPONENT)?;
        (entry.component_id, entry.is_immutable())
    };
    if is_immutable {
        return Err(FfiResult::PERMISSION_DENIED);
    }
    let info = world
        .components()
        .get_info(comp_id)
        .ok_or(FfiResult::INVALID_COMPONENT)?;
    let type_id = info.type_id().ok_or(FfiResult::UNSUPPORTED)?;
    let reflect_from_ptr = {
        let app_types = world
            .get_resource::<AppTypeRegistry>()
            .ok_or(FfiResult::INVALID_STATE)?;
        let types = app_types.read();
        let registration = types.get(type_id).ok_or(FfiResult::NOT_FOUND)?;
        registration
            .data::<ReflectFromPtr>()
            .ok_or(FfiResult::UNSUPPORTED)?
            .clone()
    };
    let mut ent_mut = world
        .get_entity_mut(entity)
        .map_err(|_| FfiResult::INVALID_ENTITY)?;
    let reflected = reflect_entity_component_mut(&mut ent_mut, comp_id, &reflect_from_ptr)
        .ok_or(FfiResult::NOT_FOUND)?;
    Ok(f(reflected))
}

/// Safe [`App`] extension methods mirroring [`ModReflectWorldExt`] registration helpers.
pub trait ModReflectAppExt {
    /// Registers host component `T` in [`ModRegistry`] and [`AppTypeRegistry`].
    fn register_reflected_component<
        T: Component + ModComponent + Reflect + FromReflect + TypePath + GetTypeRegistration,
    >(
        &mut self,
        options: ComponentOptions<T>,
    ) -> Result<BamComponent, ComponentCollisionError>;

    /// Registers host resource `R` in [`ModRegistry`] and [`AppTypeRegistry`].
    fn register_reflected_resource<
        R: Resource + ModComponent + Reflect + FromReflect + TypePath + GetTypeRegistration,
    >(
        &mut self,
    ) -> Result<BamComponent, ComponentCollisionError>;

    /// Exposes a Bevy `SettingsGroup` resource `S` in [`ModRegistry`], [`AppTypeRegistry`], and
    /// [`ModSettingsRegistry`].
    fn expose_mod_settings_group<
        S: SettingsGroup
            + Resource
            + ModComponent
            + Reflect
            + FromReflect
            + TypePath
            + GetTypeRegistration,
    >(
        &mut self,
    ) -> Result<BamComponent, ComponentCollisionError>;

    /// Binds a witness-backed mod singleton resource `resource` to TOML section `[group_name]`.
    fn register_mod_settings_resource(
        &mut self,
        resource: BamComponent,
        group_name: impl Into<String>,
    ) -> Result<(), FfiResult>;
}

impl ModReflectAppExt for App {
    #[inline]
    fn register_reflected_component<
        T: Component + ModComponent + Reflect + FromReflect + TypePath + GetTypeRegistration,
    >(
        &mut self,
        options: ComponentOptions<T>,
    ) -> Result<BamComponent, ComponentCollisionError> {
        self.world_mut().register_reflected_component::<T>(options)
    }

    #[inline]
    fn register_reflected_resource<
        R: Resource + ModComponent + Reflect + FromReflect + TypePath + GetTypeRegistration,
    >(
        &mut self,
    ) -> Result<BamComponent, ComponentCollisionError> {
        self.world_mut().register_reflected_resource::<R>()
    }

    #[inline]
    fn expose_mod_settings_group<
        S: SettingsGroup
            + Resource
            + ModComponent
            + Reflect
            + FromReflect
            + TypePath
            + GetTypeRegistration,
    >(
        &mut self,
    ) -> Result<BamComponent, ComponentCollisionError> {
        self.world_mut().expose_mod_settings_group::<S>()
    }

    #[inline]
    fn register_mod_settings_resource(
        &mut self,
        resource: BamComponent,
        group_name: impl Into<String>,
    ) -> Result<(), FfiResult> {
        self.world_mut()
            .register_mod_settings_resource(resource, group_name)
    }
}

// ============================================================================
// Witness <-> DynamicStruct / JSON / TOML Codec
// ============================================================================

/// Converts exported witness field records into a synthesized Bevy [`DynamicStruct`].
pub fn records_to_dynamic_struct(records: &[ExportedFieldRecord]) -> DynamicStruct {
    let mut out = DynamicStruct::default();
    for rec in records {
        if let Some(boxed) = decode_field_to_reflect(&rec.type_tag, &rec.bytes) {
            out.insert_boxed(rec.field_name.clone(), boxed);
        }
    }
    out
}

/// Encodes fields stored in `dyn_struct` into [`ExportedFieldRecord`]s matching `schema_fields`.
pub fn dynamic_struct_to_records(
    dyn_struct: &DynamicStruct,
    schema_fields: &[WitnessFieldDescriptor],
) -> Result<Vec<ExportedFieldRecord>, FfiResult> {
    let mut out = Vec::new();
    for (name, value) in dyn_struct.iter_fields() {
        let tag = schema_fields
            .iter()
            .find(|f| f.name == name)
            .map(|f| f.type_tag.as_str())
            .or_else(|| infer_tag_from_reflect(value))
            .ok_or(FfiResult::NOT_FOUND)?;
        let bytes = encode_reflect_to_bytes(tag, value)?;
        out.push(ExportedFieldRecord {
            field_name: name.to_owned(),
            type_tag: tag.to_owned(),
            bytes,
        });
    }
    Ok(out)
}

fn infer_tag_from_reflect(value: &dyn PartialReflect) -> Option<&'static str> {
    let path = value.reflect_type_path();
    match path {
        "f32" => Some("f32"),
        "f64" => Some("f64"),
        "i8" => Some("i8"),
        "i16" => Some("i16"),
        "i32" => Some("i32"),
        "i64" => Some("i64"),
        "i128" => Some("i128"),
        "u8" => Some("u8"),
        "u16" => Some("u16"),
        "u32" => Some("u32"),
        "u64" => Some("u64"),
        "u128" => Some("u128"),
        "bool" => Some("bool"),
        "alloc::string::String" | "std::string::String" => Some("String"),
        "alloc::vec::Vec<u8>" => Some("Vec<u8>"),
        "alloc::vec::Vec<u32>" => Some("Vec<u32>"),
        "alloc::vec::Vec<i32>" => Some("Vec<i32>"),
        "alloc::vec::Vec<f32>" => Some("Vec<f32>"),
        "core::option::Option<u32>" => Some("FfiOption<u32>"),
        "core::option::Option<i32>" => Some("FfiOption<i32>"),
        "core::option::Option<f32>" => Some("FfiOption<f32>"),
        _ => None,
    }
}

fn decode_field_to_reflect(type_tag: &str, bytes: &[u8]) -> Option<Box<dyn PartialReflect>> {
    match type_tag {
        "f32" => Some(Box::new(f32::from_le_bytes(bytes.try_into().ok()?))),
        "f64" => Some(Box::new(f64::from_le_bytes(bytes.try_into().ok()?))),
        "i8" => Some(Box::new(i8::from_le_bytes(bytes.try_into().ok()?))),
        "i16" => Some(Box::new(i16::from_le_bytes(bytes.try_into().ok()?))),
        "i32" => Some(Box::new(i32::from_le_bytes(bytes.try_into().ok()?))),
        "i64" => Some(Box::new(i64::from_le_bytes(bytes.try_into().ok()?))),
        "i128" => Some(Box::new(i128::from_le_bytes(bytes.try_into().ok()?))),
        "u8" => Some(Box::new(u8::from_le_bytes(bytes.try_into().ok()?))),
        "u16" => Some(Box::new(u16::from_le_bytes(bytes.try_into().ok()?))),
        "u32" => Some(Box::new(u32::from_le_bytes(bytes.try_into().ok()?))),
        "u64" => Some(Box::new(u64::from_le_bytes(bytes.try_into().ok()?))),
        "u128" => Some(Box::new(u128::from_le_bytes(bytes.try_into().ok()?))),
        "bool" => Some(Box::new(*bytes.first()? != 0)),
        "String" | "__variant" => Some(Box::new(String::from_utf8(bytes.to_vec()).ok()?)),
        "Vec<u8>" => Some(Box::new(bytes.to_vec())),
        "Vec<u32>" => {
            let (chunks, rem) = bytes.as_chunks::<4>();
            if !rem.is_empty() {
                return None;
            }
            let vec: Vec<u32> = chunks.iter().copied().map(u32::from_le_bytes).collect();
            Some(Box::new(vec))
        }
        "Vec<i32>" => {
            let (chunks, rem) = bytes.as_chunks::<4>();
            if !rem.is_empty() {
                return None;
            }
            let vec: Vec<i32> = chunks.iter().copied().map(i32::from_le_bytes).collect();
            Some(Box::new(vec))
        }
        "Vec<f32>" => {
            let (chunks, rem) = bytes.as_chunks::<4>();
            if !rem.is_empty() {
                return None;
            }
            let vec: Vec<f32> = chunks.iter().copied().map(f32::from_le_bytes).collect();
            Some(Box::new(vec))
        }
        "FfiOption<u32>" => match *bytes.first()? {
            0 => Some(Box::new(None::<u32>)),
            _ => Some(Box::new(Some(u32::from_le_bytes(
                bytes.get(1..5)?.try_into().ok()?,
            )))),
        },
        "FfiOption<i32>" => match *bytes.first()? {
            0 => Some(Box::new(None::<i32>)),
            _ => Some(Box::new(Some(i32::from_le_bytes(
                bytes.get(1..5)?.try_into().ok()?,
            )))),
        },
        "FfiOption<f32>" => match *bytes.first()? {
            0 => Some(Box::new(None::<f32>)),
            _ => Some(Box::new(Some(f32::from_le_bytes(
                bytes.get(1..5)?.try_into().ok()?,
            )))),
        },
        _ => None,
    }
}

fn encode_reflect_to_bytes(
    type_tag: &str,
    value: &dyn PartialReflect,
) -> Result<Vec<u8>, FfiResult> {
    if let Some(v) = value.try_downcast_ref::<f32>() {
        return encode_number_f64(type_tag, f64::from(*v));
    }
    if let Some(v) = value.try_downcast_ref::<f64>() {
        return encode_number_f64(type_tag, *v);
    }
    if let Some(v) = value.try_downcast_ref::<i8>() {
        return encode_signed_i128(type_tag, i128::from(*v));
    }
    if let Some(v) = value.try_downcast_ref::<i16>() {
        return encode_signed_i128(type_tag, i128::from(*v));
    }
    if let Some(v) = value.try_downcast_ref::<i32>() {
        return encode_signed_i128(type_tag, i128::from(*v));
    }
    if let Some(v) = value.try_downcast_ref::<i64>() {
        return encode_signed_i128(type_tag, i128::from(*v));
    }
    if let Some(v) = value.try_downcast_ref::<i128>() {
        return encode_signed_i128(type_tag, *v);
    }
    if let Some(v) = value.try_downcast_ref::<u8>() {
        return encode_unsigned_u128(type_tag, u128::from(*v));
    }
    if let Some(v) = value.try_downcast_ref::<u16>() {
        return encode_unsigned_u128(type_tag, u128::from(*v));
    }
    if let Some(v) = value.try_downcast_ref::<u32>() {
        return encode_unsigned_u128(type_tag, u128::from(*v));
    }
    if let Some(v) = value.try_downcast_ref::<u64>() {
        return encode_unsigned_u128(type_tag, u128::from(*v));
    }
    if let Some(v) = value.try_downcast_ref::<u128>() {
        return encode_unsigned_u128(type_tag, *v);
    }
    if let Some(v) = value.try_downcast_ref::<bool>() {
        if type_tag == "bool" {
            return Ok(vec![u8::from(*v)]);
        }
        return Err(FfiResult::TYPE_MISMATCH);
    }
    if let Some(v) = value.try_downcast_ref::<String>() {
        if type_tag == "String" || type_tag == "__variant" {
            return Ok(v.as_bytes().to_vec());
        }
        return Err(FfiResult::TYPE_MISMATCH);
    }
    if let Some(v) = value.try_downcast_ref::<Vec<u8>>() {
        if type_tag == "Vec<u8>" {
            return Ok(v.clone());
        }
        return Err(FfiResult::TYPE_MISMATCH);
    }
    if let Some(v) = value.try_downcast_ref::<Vec<u32>>() {
        if type_tag == "Vec<u32>" {
            let mut bytes = Vec::with_capacity(v.len() * 4);
            for elem in v {
                bytes.extend_from_slice(&elem.to_le_bytes());
            }
            return Ok(bytes);
        }
        return Err(FfiResult::TYPE_MISMATCH);
    }
    if let Some(v) = value.try_downcast_ref::<Vec<i32>>() {
        if type_tag == "Vec<i32>" {
            let mut bytes = Vec::with_capacity(v.len() * 4);
            for elem in v {
                bytes.extend_from_slice(&elem.to_le_bytes());
            }
            return Ok(bytes);
        }
        return Err(FfiResult::TYPE_MISMATCH);
    }
    if let Some(v) = value.try_downcast_ref::<Vec<f32>>() {
        if type_tag == "Vec<f32>" {
            let mut bytes = Vec::with_capacity(v.len() * 4);
            for elem in v {
                bytes.extend_from_slice(&elem.to_le_bytes());
            }
            return Ok(bytes);
        }
        return Err(FfiResult::TYPE_MISMATCH);
    }
    if let Some(opt) = value.try_downcast_ref::<Option<u32>>() {
        if type_tag == "FfiOption<u32>" {
            return Ok(match opt {
                None => vec![0u8],
                Some(n) => {
                    let mut b = vec![1u8];
                    b.extend_from_slice(&n.to_le_bytes());
                    b
                }
            });
        }
        return Err(FfiResult::TYPE_MISMATCH);
    }
    if let Some(opt) = value.try_downcast_ref::<Option<i32>>() {
        if type_tag == "FfiOption<i32>" {
            return Ok(match opt {
                None => vec![0u8],
                Some(n) => {
                    let mut b = vec![1u8];
                    b.extend_from_slice(&n.to_le_bytes());
                    b
                }
            });
        }
        return Err(FfiResult::TYPE_MISMATCH);
    }
    if let Some(opt) = value.try_downcast_ref::<Option<f32>>() {
        if type_tag == "FfiOption<f32>" {
            return Ok(match opt {
                None => vec![0u8],
                Some(n) => {
                    let mut b = vec![1u8];
                    b.extend_from_slice(&n.to_le_bytes());
                    b
                }
            });
        }
        return Err(FfiResult::TYPE_MISMATCH);
    }
    Err(FfiResult::TYPE_MISMATCH)
}

fn encode_number_f64(type_tag: &str, v: f64) -> Result<Vec<u8>, FfiResult> {
    match type_tag {
        "f32" => Ok((v as f32).to_le_bytes().to_vec()),
        "f64" => Ok(v.to_le_bytes().to_vec()),
        _ => Err(FfiResult::TYPE_MISMATCH),
    }
}

fn encode_signed_i128(type_tag: &str, v: i128) -> Result<Vec<u8>, FfiResult> {
    match type_tag {
        "i8" => i8::try_from(v)
            .map(|x| x.to_le_bytes().to_vec())
            .map_err(|_| FfiResult::INVALID_ARGUMENT),
        "i16" => i16::try_from(v)
            .map(|x| x.to_le_bytes().to_vec())
            .map_err(|_| FfiResult::INVALID_ARGUMENT),
        "i32" => i32::try_from(v)
            .map(|x| x.to_le_bytes().to_vec())
            .map_err(|_| FfiResult::INVALID_ARGUMENT),
        "i64" => i64::try_from(v)
            .map(|x| x.to_le_bytes().to_vec())
            .map_err(|_| FfiResult::INVALID_ARGUMENT),
        "i128" => Ok(v.to_le_bytes().to_vec()),
        "f32" => Ok((v as f32).to_le_bytes().to_vec()),
        "f64" => Ok((v as f64).to_le_bytes().to_vec()),
        _ if v >= 0 => encode_unsigned_u128(type_tag, v as u128),
        _ => Err(FfiResult::TYPE_MISMATCH),
    }
}

fn encode_unsigned_u128(type_tag: &str, v: u128) -> Result<Vec<u8>, FfiResult> {
    match type_tag {
        "u8" => u8::try_from(v)
            .map(|x| x.to_le_bytes().to_vec())
            .map_err(|_| FfiResult::INVALID_ARGUMENT),
        "u16" => u16::try_from(v)
            .map(|x| x.to_le_bytes().to_vec())
            .map_err(|_| FfiResult::INVALID_ARGUMENT),
        "u32" => u32::try_from(v)
            .map(|x| x.to_le_bytes().to_vec())
            .map_err(|_| FfiResult::INVALID_ARGUMENT),
        "u64" => u64::try_from(v)
            .map(|x| x.to_le_bytes().to_vec())
            .map_err(|_| FfiResult::INVALID_ARGUMENT),
        "u128" => Ok(v.to_le_bytes().to_vec()),
        "i8" => i8::try_from(v)
            .map(|x| x.to_le_bytes().to_vec())
            .map_err(|_| FfiResult::INVALID_ARGUMENT),
        "i16" => i16::try_from(v)
            .map(|x| x.to_le_bytes().to_vec())
            .map_err(|_| FfiResult::INVALID_ARGUMENT),
        "i32" => i32::try_from(v)
            .map(|x| x.to_le_bytes().to_vec())
            .map_err(|_| FfiResult::INVALID_ARGUMENT),
        "i64" => i64::try_from(v)
            .map(|x| x.to_le_bytes().to_vec())
            .map_err(|_| FfiResult::INVALID_ARGUMENT),
        "i128" => i128::try_from(v)
            .map(|x| x.to_le_bytes().to_vec())
            .map_err(|_| FfiResult::INVALID_ARGUMENT),
        "f32" => Ok((v as f32).to_le_bytes().to_vec()),
        "f64" => Ok((v as f64).to_le_bytes().to_vec()),
        _ => Err(FfiResult::TYPE_MISMATCH),
    }
}

fn dynamic_struct_to_json(dyn_struct: &DynamicStruct) -> Value {
    let mut map = Map::new();
    for (name, field) in dyn_struct.iter_fields() {
        if let Some(val) = reflect_value_to_json(field) {
            map.insert(name.to_owned(), val);
        }
    }
    Value::Object(map)
}

fn reflect_value_to_json(value: &dyn PartialReflect) -> Option<Value> {
    if let Some(v) = value.try_downcast_ref::<f32>() {
        return serde_json::Number::from_f64(f64::from(*v)).map(Value::Number);
    }
    if let Some(v) = value.try_downcast_ref::<f64>() {
        return serde_json::Number::from_f64(*v).map(Value::Number);
    }
    if let Some(v) = value.try_downcast_ref::<i8>() {
        return Some(Value::from(*v));
    }
    if let Some(v) = value.try_downcast_ref::<i16>() {
        return Some(Value::from(*v));
    }
    if let Some(v) = value.try_downcast_ref::<i32>() {
        return Some(Value::from(*v));
    }
    if let Some(v) = value.try_downcast_ref::<i64>() {
        return Some(Value::from(*v));
    }
    if let Some(v) = value.try_downcast_ref::<u8>() {
        return Some(Value::from(*v));
    }
    if let Some(v) = value.try_downcast_ref::<u16>() {
        return Some(Value::from(*v));
    }
    if let Some(v) = value.try_downcast_ref::<u32>() {
        return Some(Value::from(*v));
    }
    if let Some(v) = value.try_downcast_ref::<u64>() {
        return Some(Value::from(*v));
    }
    if let Some(v) = value.try_downcast_ref::<bool>() {
        return Some(Value::Bool(*v));
    }
    if let Some(v) = value.try_downcast_ref::<String>() {
        return Some(Value::String(v.clone()));
    }
    if let Some(v) = value.try_downcast_ref::<Vec<u8>>() {
        return Some(Value::Array(v.iter().map(|b| Value::from(*b)).collect()));
    }
    if let Some(v) = value.try_downcast_ref::<Vec<u32>>() {
        return Some(Value::Array(v.iter().map(|x| Value::from(*x)).collect()));
    }
    if let Some(v) = value.try_downcast_ref::<Vec<i32>>() {
        return Some(Value::Array(v.iter().map(|x| Value::from(*x)).collect()));
    }
    if let Some(v) = value.try_downcast_ref::<Vec<f32>>() {
        return Some(Value::Array(
            v.iter()
                .filter_map(|x| serde_json::Number::from_f64(f64::from(*x)).map(Value::Number))
                .collect(),
        ));
    }
    if let Some(opt) = value.try_downcast_ref::<Option<u32>>() {
        return Some(match opt {
            Some(n) => Value::from(*n),
            None => Value::Null,
        });
    }
    if let Some(opt) = value.try_downcast_ref::<Option<i32>>() {
        return Some(match opt {
            Some(n) => Value::from(*n),
            None => Value::Null,
        });
    }
    if let Some(opt) = value.try_downcast_ref::<Option<f32>>() {
        return Some(match opt {
            Some(n) => serde_json::Number::from_f64(f64::from(*n))
                .map(Value::Number)
                .unwrap_or(Value::Null),
            None => Value::Null,
        });
    }
    None
}

fn json_to_record(
    field_name: &str,
    type_tag: &str,
    val: &Value,
) -> Result<ExportedFieldRecord, String> {
    let bytes = match (type_tag, val) {
        ("f32", Value::Number(n)) => {
            let f = n
                .as_f64()
                .ok_or_else(|| format!("field '{field_name}' expected finite f32"))?;
            (f as f32).to_le_bytes().to_vec()
        }
        ("f64", Value::Number(n)) => {
            let f = n
                .as_f64()
                .ok_or_else(|| format!("field '{field_name}' expected finite f64"))?;
            f.to_le_bytes().to_vec()
        }
        ("i8" | "i16" | "i32" | "i64" | "i128", Value::Number(n)) => {
            let i = n
                .as_i64()
                .ok_or_else(|| format!("field '{field_name}' expected signed integer"))?;
            encode_signed_i128(type_tag, i128::from(i))
                .map_err(|e| format!("field '{field_name}' range/type error: {e:?}"))?
        }
        ("u8" | "u16" | "u32" | "u64" | "u128", Value::Number(n)) => {
            let u = n
                .as_u64()
                .ok_or_else(|| format!("field '{field_name}' expected unsigned integer"))?;
            encode_unsigned_u128(type_tag, u128::from(u))
                .map_err(|e| format!("field '{field_name}' range/type error: {e:?}"))?
        }
        ("bool", Value::Bool(b)) => vec![u8::from(*b)],
        ("String" | "__variant", Value::String(s)) => s.as_bytes().to_vec(),
        ("Vec<u8>", Value::Array(arr)) => {
            let mut out = Vec::with_capacity(arr.len());
            for item in arr {
                let b = item
                    .as_u64()
                    .and_then(|u| u8::try_from(u).ok())
                    .ok_or_else(|| format!("field '{field_name}' expected u8 array"))?;
                out.push(b);
            }
            out
        }
        ("Vec<u32>", Value::Array(arr)) => {
            let mut out = Vec::with_capacity(arr.len() * 4);
            for item in arr {
                let u = item
                    .as_u64()
                    .and_then(|n| u32::try_from(n).ok())
                    .ok_or_else(|| format!("field '{field_name}' expected u32 array"))?;
                out.extend_from_slice(&u.to_le_bytes());
            }
            out
        }
        ("Vec<i32>", Value::Array(arr)) => {
            let mut out = Vec::with_capacity(arr.len() * 4);
            for item in arr {
                let i = item
                    .as_i64()
                    .and_then(|n| i32::try_from(n).ok())
                    .ok_or_else(|| format!("field '{field_name}' expected i32 array"))?;
                out.extend_from_slice(&i.to_le_bytes());
            }
            out
        }
        ("Vec<f32>", Value::Array(arr)) => {
            let mut out = Vec::with_capacity(arr.len() * 4);
            for item in arr {
                let f = item
                    .as_f64()
                    .ok_or_else(|| format!("field '{field_name}' expected f32 array"))?;
                out.extend_from_slice(&(f as f32).to_le_bytes());
            }
            out
        }
        ("FfiOption<u32>", Value::Null)
        | ("FfiOption<i32>", Value::Null)
        | ("FfiOption<f32>", Value::Null) => vec![0u8],
        ("FfiOption<u32>", Value::Number(n)) => {
            let u = n
                .as_u64()
                .and_then(|x| u32::try_from(x).ok())
                .ok_or_else(|| format!("field '{field_name}' expected Option<u32>"))?;
            let mut b = vec![1u8];
            b.extend_from_slice(&u.to_le_bytes());
            b
        }
        ("FfiOption<i32>", Value::Number(n)) => {
            let i = n
                .as_i64()
                .and_then(|x| i32::try_from(x).ok())
                .ok_or_else(|| format!("field '{field_name}' expected Option<i32>"))?;
            let mut b = vec![1u8];
            b.extend_from_slice(&i.to_le_bytes());
            b
        }
        ("FfiOption<f32>", Value::Number(n)) => {
            let f = n
                .as_f64()
                .ok_or_else(|| format!("field '{field_name}' expected Option<f32>"))?;
            let mut b = vec![1u8];
            b.extend_from_slice(&(f as f32).to_le_bytes());
            b
        }
        _ => {
            return Err(format!(
                "incompatible JSON value for field '{field_name}' (type_tag '{type_tag}')"
            ));
        }
    };
    Ok(ExportedFieldRecord {
        field_name: field_name.to_owned(),
        type_tag: type_tag.to_owned(),
        bytes,
    })
}

fn json_map_to_records(
    obj: &Map<String, Value>,
    fields: &[WitnessFieldDescriptor],
) -> Result<Vec<ExportedFieldRecord>, String> {
    let mut out = Vec::with_capacity(obj.len());
    for (k, v) in obj {
        let desc = fields
            .iter()
            .find(|f| f.name == *k)
            .ok_or_else(|| format!("unknown field '{k}'"))?;
        out.push(json_to_record(k, &desc.type_tag, v)?);
    }
    Ok(out)
}

// ============================================================================
// Bevy Remote Protocol (`bevy_remote`) Mod Inspection & Mutation Handlers
// ============================================================================

fn brp_err(code: i16, message: impl Into<String>) -> BrpError {
    BrpError {
        code,
        message: message.into(),
        data: None,
    }
}

fn parse_entity_param(params: Option<&Value>) -> Result<Option<Entity>, BrpError> {
    let Some(val) = params else {
        return Ok(None);
    };
    let Some(ent_val) = val.get("entity") else {
        return Ok(None);
    };
    let bits = ent_val
        .as_u64()
        .ok_or_else(|| brp_err(error_codes::INVALID_PARAMS, "`entity` must be a u64"))?;
    let entity = Entity::try_from_bits(bits)
        .ok_or_else(|| brp_err(error_codes::INVALID_PARAMS, "invalid entity bits"))?;
    Ok(Some(entity))
}

fn resolve_mod_handle_by_name(world: &World, name: &str) -> Result<BamComponent, BrpError> {
    let reg = world
        .get_resource::<ModRegistry>()
        .ok_or_else(|| brp_err(error_codes::INTERNAL_ERROR, "ModRegistry missing"))?;
    reg.get_by_name(name).map(|e| e.handle).ok_or_else(|| {
        brp_err(
            error_codes::COMPONENT_ERROR,
            format!("unknown mod component '{name}'"),
        )
    })
}

fn serialize_typed_reflect_to_json(
    world: &World,
    entity: Entity,
    component: BamComponent,
) -> Option<Value> {
    let reflect = world.reflect_mod_component(entity, component).ok()?;
    let dyn_val = reflect.to_dynamic().ok()?;
    let app_types = world.get_resource::<AppTypeRegistry>()?;
    let types = app_types.read();
    let ser = ReflectSerializer::new(dyn_val.as_partial_reflect(), &types);
    serde_json::to_value(&ser).ok()
}

/// Dispatches a Bevy Remote Protocol [`BrpRequest`] against `world` for `bevy_abi_mod/*` methods.
pub fn handle_mod_brp_request(world: &mut World, request: &BrpRequest) -> BrpResult {
    match request.method.as_str() {
        BRP_METHOD_LIST_COMPONENTS => brp_list_components_impl(world, request.params.as_ref()),
        BRP_METHOD_GET_COMPONENTS => brp_get_components_impl(world, request.params.as_ref()),
        BRP_METHOD_MUTATE_COMPONENTS => brp_mutate_components_impl(world, request.params.as_ref()),
        BRP_METHOD_INSERT_COMPONENTS => brp_insert_components_impl(world, request.params.as_ref()),
        BRP_METHOD_GET_RESOURCES => brp_get_resources_impl(world, request.params.as_ref()),
        BRP_METHOD_MUTATE_RESOURCES => brp_mutate_resources_impl(world, request.params.as_ref()),
        other => Err(brp_err(
            error_codes::METHOD_NOT_FOUND,
            format!("unknown mod BRP method '{other}'"),
        )),
    }
}

fn brp_list_components_system(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    brp_list_components_impl(world, params.as_ref())
}

fn brp_get_components_system(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    brp_get_components_impl(world, params.as_ref())
}

fn brp_mutate_components_system(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    brp_mutate_components_impl(world, params.as_ref())
}

fn brp_insert_components_system(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    brp_insert_components_impl(world, params.as_ref())
}

fn brp_get_resources_system(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    brp_get_resources_impl(world, params.as_ref())
}

fn brp_mutate_resources_system(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    brp_mutate_resources_impl(world, params.as_ref())
}

fn brp_list_components_impl(world: &mut World, params: Option<&Value>) -> BrpResult {
    let _ = sync_witness_reflection_schemas(world);
    let entity_opt = parse_entity_param(params)?;
    let entity_ref = match entity_opt {
        Some(e) => Some(world.get_entity(e).map_err(|_| {
            brp_err(
                error_codes::ENTITY_NOT_FOUND,
                format!("entity {e:?} not found"),
            )
        })?),
        None => None,
    };

    let reg = world
        .get_resource::<ModRegistry>()
        .ok_or_else(|| brp_err(error_codes::INTERNAL_ERROR, "ModRegistry missing"))?;
    let reflect_reg = world.get_resource::<ModReflectRegistry>();

    let mut items = Vec::new();
    for entry in reg.snapshot().entries() {
        if entry.is_message() {
            continue;
        }
        if let Some(eref) = entity_ref
            && (entry.is_resource() || !eref.contains_id(entry.component_id))
        {
            continue;
        }
        let fields_json: Vec<Value> = reflect_reg
            .and_then(|r| r.witness_schema(entry.handle))
            .map(|schema| {
                schema
                    .fields
                    .iter()
                    .map(|f| {
                        serde_json::json!({
                            "name": f.name,
                            "type_tag": f.type_tag,
                            "offset": f.offset,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        items.push(serde_json::json!({
            "name": entry.name.as_ref(),
            "handle": entry.handle.0,
            "is_resource": entry.is_resource(),
            "is_dynamic": entry.is_dynamic,
            "is_immutable": entry.is_immutable(),
            "fields": fields_json,
        }));
    }
    Ok(Value::Array(items))
}

fn brp_get_components_impl(world: &mut World, params: Option<&Value>) -> BrpResult {
    let _ = sync_witness_reflection_schemas(world);
    let params = params.ok_or_else(|| brp_err(error_codes::INVALID_PARAMS, "missing params"))?;
    let entity = parse_entity_param(Some(params))?
        .ok_or_else(|| brp_err(error_codes::INVALID_PARAMS, "missing `entity` parameter"))?;
    if world.get_entity(entity).is_err() {
        return Err(brp_err(
            error_codes::ENTITY_NOT_FOUND,
            format!("entity {entity:?} not found"),
        ));
    }
    let names = params
        .get("components")
        .and_then(Value::as_array)
        .ok_or_else(|| brp_err(error_codes::INVALID_PARAMS, "missing `components` array"))?;

    let mut components_map = Map::new();
    let mut errors_map = Map::new();
    for item in names {
        let Some(name) = item.as_str() else {
            return Err(brp_err(
                error_codes::INVALID_PARAMS,
                "component entries must be strings",
            ));
        };
        let Ok(handle) = resolve_mod_handle_by_name(world, name) else {
            errors_map.insert(
                name.to_owned(),
                serde_json::json!({ "code": error_codes::COMPONENT_ERROR, "message": "unknown component" }),
            );
            continue;
        };
        if let Ok(records) = export_entity_component_fields(world, entity, handle) {
            let dyn_struct = records_to_dynamic_struct(&records);
            components_map.insert(name.to_owned(), dynamic_struct_to_json(&dyn_struct));
        } else if let Some(typed_json) = serialize_typed_reflect_to_json(world, entity, handle) {
            components_map.insert(name.to_owned(), typed_json);
        } else {
            errors_map.insert(
                name.to_owned(),
                serde_json::json!({ "code": error_codes::COMPONENT_NOT_PRESENT, "message": "component not present or lacks reflection" }),
            );
        }
    }

    Ok(serde_json::json!({
        "components": components_map,
        "errors": errors_map,
    }))
}

fn apply_typed_reflect_json(
    world: &mut World,
    entity: Entity,
    component: BamComponent,
    value_json: &Value,
) -> Result<(), BrpError> {
    let (comp_id, type_id, canonical) = {
        let reg = world
            .get_resource::<ModRegistry>()
            .ok_or_else(|| brp_err(error_codes::INTERNAL_ERROR, "ModRegistry missing"))?;
        let entry = reg
            .get_by_handle(component)
            .ok_or_else(|| brp_err(error_codes::COMPONENT_ERROR, "invalid handle"))?;
        let tid = world
            .components()
            .get_info(entry.component_id)
            .and_then(|i| i.type_id())
            .ok_or_else(|| brp_err(error_codes::COMPONENT_ERROR, "component has no TypeId"))?;
        (entry.component_id, tid, entry.name.to_string())
    };
    let app_types = world
        .get_resource::<AppTypeRegistry>()
        .ok_or_else(|| brp_err(error_codes::INTERNAL_ERROR, "AppTypeRegistry missing"))?
        .clone();
    let deserialized = {
        let types = app_types.read();
        let reg = types.get(type_id).ok_or_else(|| {
            brp_err(
                error_codes::COMPONENT_ERROR,
                format!("type '{canonical}' not registered in AppTypeRegistry"),
            )
        })?;
        let de = TypedReflectDeserializer::new(reg, &types);
        de.deserialize(value_json.clone()).map_err(|e| {
            brp_err(
                error_codes::INVALID_PARAMS,
                format!("deserialize failed: {e}"),
            )
        })?
    };
    let _ = comp_id;
    with_reflect_mod_component_mut(world, entity, component, |mut target| {
        target.apply(deserialized.as_partial_reflect());
    })
    .map_err(|e| brp_err(error_codes::COMPONENT_ERROR, format!("apply failed: {e:?}")))
}

fn brp_mutate_components_impl(world: &mut World, params: Option<&Value>) -> BrpResult {
    let _ = sync_witness_reflection_schemas(world);
    let params = params.ok_or_else(|| brp_err(error_codes::INVALID_PARAMS, "missing params"))?;
    let entity = parse_entity_param(Some(params))?
        .ok_or_else(|| brp_err(error_codes::INVALID_PARAMS, "missing `entity` parameter"))?;
    if world.get_entity(entity).is_err() {
        return Err(brp_err(
            error_codes::ENTITY_NOT_FOUND,
            format!("entity {entity:?} not found"),
        ));
    }

    if let (Some(comp_name), Some(val_obj)) = (
        params.get("component").and_then(Value::as_str),
        params.get("value"),
    ) {
        let handle = resolve_mod_handle_by_name(world, comp_name)?;
        if let Some(fields_schema) = world
            .get_resource::<ModReflectRegistry>()
            .and_then(|r| r.witness_schema(handle).map(|s| s.fields.clone()))
        {
            let map = val_obj
                .as_object()
                .ok_or_else(|| brp_err(error_codes::INVALID_PARAMS, "`value` must be an object"))?;
            let records = json_map_to_records(map, &fields_schema)
                .map_err(|m| brp_err(error_codes::INVALID_PARAMS, m))?;
            apply_entity_component_fields(world, entity, handle, &records).map_err(|e| {
                brp_err(
                    error_codes::COMPONENT_ERROR,
                    format!("mutate failed: {e:?}"),
                )
            })?;
        } else {
            apply_typed_reflect_json(world, entity, handle, val_obj)?;
        }
        return Ok(Value::Null);
    }

    if let Some(components_obj) = params.get("components").and_then(Value::as_object) {
        for (comp_name, val_obj) in components_obj {
            let handle = resolve_mod_handle_by_name(world, comp_name)?;
            if let Some(fields_schema) = world
                .get_resource::<ModReflectRegistry>()
                .and_then(|r| r.witness_schema(handle).map(|s| s.fields.clone()))
            {
                let map = val_obj.as_object().ok_or_else(|| {
                    brp_err(
                        error_codes::INVALID_PARAMS,
                        "component value must be object",
                    )
                })?;
                let records = json_map_to_records(map, &fields_schema)
                    .map_err(|m| brp_err(error_codes::INVALID_PARAMS, m))?;
                apply_entity_component_fields(world, entity, handle, &records).map_err(|e| {
                    brp_err(
                        error_codes::COMPONENT_ERROR,
                        format!("mutate failed: {e:?}"),
                    )
                })?;
            } else {
                apply_typed_reflect_json(world, entity, handle, val_obj)?;
            }
        }
        return Ok(Value::Null);
    }

    Err(brp_err(
        error_codes::INVALID_PARAMS,
        "expected (`component`, `value`) or `components` object",
    ))
}

fn brp_insert_components_impl(world: &mut World, params: Option<&Value>) -> BrpResult {
    let _ = sync_witness_reflection_schemas(world);
    let params = params.ok_or_else(|| brp_err(error_codes::INVALID_PARAMS, "missing params"))?;
    let entity = parse_entity_param(Some(params))?
        .ok_or_else(|| brp_err(error_codes::INVALID_PARAMS, "missing `entity` parameter"))?;
    if world.get_entity(entity).is_err() {
        return Err(brp_err(
            error_codes::ENTITY_NOT_FOUND,
            format!("entity {entity:?} not found"),
        ));
    }
    let components_obj = params
        .get("components")
        .and_then(Value::as_object)
        .ok_or_else(|| brp_err(error_codes::INVALID_PARAMS, "missing `components` object"))?;

    for (comp_name, val_obj) in components_obj {
        let handle = resolve_mod_handle_by_name(world, comp_name)?;
        if let Some(fields_schema) = world
            .get_resource::<ModReflectRegistry>()
            .and_then(|r| r.witness_schema(handle).map(|s| s.fields.clone()))
        {
            let map = val_obj.as_object().ok_or_else(|| {
                brp_err(
                    error_codes::INVALID_PARAMS,
                    "component value must be object",
                )
            })?;
            let records = json_map_to_records(map, &fields_schema)
                .map_err(|m| brp_err(error_codes::INVALID_PARAMS, m))?;
            insert_or_apply_entity_component_fields(world, entity, handle, &records).map_err(
                |e| {
                    brp_err(
                        error_codes::COMPONENT_ERROR,
                        format!("insert failed: {e:?}"),
                    )
                },
            )?;
        } else {
            return Err(brp_err(
                error_codes::COMPONENT_ERROR,
                format!("component '{comp_name}' has no witness schema for insert"),
            ));
        }
    }
    Ok(Value::Null)
}

fn brp_get_resources_impl(world: &mut World, params: Option<&Value>) -> BrpResult {
    let _ = sync_witness_reflection_schemas(world);
    let params = params.ok_or_else(|| brp_err(error_codes::INVALID_PARAMS, "missing params"))?;
    let names = params
        .get("resources")
        .and_then(Value::as_array)
        .ok_or_else(|| brp_err(error_codes::INVALID_PARAMS, "missing `resources` array"))?;

    let mut out = Map::new();
    for item in names {
        let Some(name) = item.as_str() else {
            return Err(brp_err(
                error_codes::INVALID_PARAMS,
                "resource entries must be strings",
            ));
        };
        let handle = resolve_mod_handle_by_name(world, name)
            .map_err(|e| brp_err(error_codes::RESOURCE_ERROR, e.message))?;
        let records = export_resource_fields(world, handle).map_err(|e| {
            brp_err(
                error_codes::RESOURCE_NOT_PRESENT,
                format!("failed exporting resource '{name}': {e:?}"),
            )
        })?;
        let dyn_struct = records_to_dynamic_struct(&records);
        out.insert(name.to_owned(), dynamic_struct_to_json(&dyn_struct));
    }
    Ok(Value::Object(out))
}

fn brp_mutate_resources_impl(world: &mut World, params: Option<&Value>) -> BrpResult {
    let _ = sync_witness_reflection_schemas(world);
    let params = params.ok_or_else(|| brp_err(error_codes::INVALID_PARAMS, "missing params"))?;
    let name = params
        .get("resource")
        .and_then(Value::as_str)
        .ok_or_else(|| brp_err(error_codes::INVALID_PARAMS, "missing `resource` string"))?;
    let value_map = params
        .get("value")
        .and_then(Value::as_object)
        .ok_or_else(|| brp_err(error_codes::INVALID_PARAMS, "missing `value` object"))?;
    let handle = resolve_mod_handle_by_name(world, name)
        .map_err(|e| brp_err(error_codes::RESOURCE_ERROR, e.message))?;
    let fields_schema = world
        .get_resource::<ModReflectRegistry>()
        .and_then(|r| r.witness_schema(handle).map(|s| s.fields.clone()))
        .ok_or_else(|| {
            brp_err(
                error_codes::RESOURCE_ERROR,
                format!("resource '{name}' has no witness schema"),
            )
        })?;
    let records = json_map_to_records(value_map, &fields_schema)
        .map_err(|m| brp_err(error_codes::INVALID_PARAMS, m))?;
    apply_resource_fields(world, handle, &records).map_err(|e| {
        brp_err(
            error_codes::RESOURCE_ERROR,
            format!("mutate resource failed: {e:?}"),
        )
    })?;
    Ok(Value::Null)
}

// ============================================================================
// `bevy_settings` TOML Serialization / Application Bridge
// ============================================================================

fn json_to_toml_value(val: Value) -> Option<toml::Value> {
    match val {
        Value::Null => None,
        Value::Bool(b) => Some(toml::Value::Boolean(b)),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Some(toml::Value::Integer(i))
            } else {
                n.as_f64().map(toml::Value::Float)
            }
        }
        Value::String(s) => Some(toml::Value::String(s)),
        Value::Array(arr) => Some(toml::Value::Array(
            arr.into_iter().filter_map(json_to_toml_value).collect(),
        )),
        Value::Object(obj) => {
            let mut tbl = toml::Table::new();
            for (k, v) in obj {
                if let Some(tv) = json_to_toml_value(v) {
                    tbl.insert(k, tv);
                }
            }
            Some(toml::Value::Table(tbl))
        }
    }
}

fn toml_to_json_value(val: &toml::Value) -> Value {
    match val {
        toml::Value::String(s) => Value::String(s.clone()),
        toml::Value::Integer(i) => Value::from(*i),
        toml::Value::Float(f) => serde_json::Number::from_f64(*f)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        toml::Value::Boolean(b) => Value::Bool(*b),
        toml::Value::Datetime(dt) => Value::String(dt.to_string()),
        toml::Value::Array(arr) => Value::Array(arr.iter().map(toml_to_json_value).collect()),
        toml::Value::Table(tbl) => {
            let mut map = Map::new();
            for (k, v) in tbl {
                map.insert(k.clone(), toml_to_json_value(v));
            }
            Value::Object(map)
        }
    }
}

fn export_reflected_settings_group_table(
    world: &World,
    types: &TypeRegistry,
    tid: TypeId,
) -> Option<toml::Table> {
    let reg = types.get(tid)?;
    let reflect_comp = reg.data::<ReflectComponent>()?;
    let comp_id = world.components().get_id(tid)?;
    let res_entity = world.resource_entities().get(comp_id)?;
    let entity_ref = world.get_entity(res_entity).ok()?;
    let reflected = reflect_comp.reflect(entity_ref)?;
    let ser = TypedReflectSerializer::new(reflected.as_partial_reflect(), types);
    let val = toml::Value::try_from(ser).ok()?;
    val.as_table().cloned()
}

fn apply_reflected_settings_group_table(
    world: &mut World,
    app_types: &AppTypeRegistry,
    tid: TypeId,
    table: &toml::Table,
) -> bool {
    let deserialized = {
        let types = app_types.read();
        let Some(reg) = types.get(tid) else {
            return false;
        };
        let de = TypedReflectDeserializer::new(reg, &types);
        match de.deserialize(toml::Value::Table(table.clone())) {
            Ok(v) => v,
            Err(_) => return false,
        }
    };
    let Some(comp_id) = world.components().get_id(tid) else {
        return false;
    };
    let Some(res_entity) = world.resource_entities().get(comp_id) else {
        return false;
    };
    let types_guard = app_types.read();
    let Some(reg) = types_guard.get(tid) else {
        return false;
    };
    let Some(reflect_comp) = reg.data::<ReflectComponent>().cloned() else {
        return false;
    };
    drop(types_guard);
    if let Ok(mut ent_mut) = world.get_entity_mut(res_entity) {
        reflect_comp.apply(&mut ent_mut, deserialized.as_partial_reflect());
        return true;
    }
    false
}

fn export_mod_settings_toml_from_world(world: &World) -> Result<String, ModSettingsError> {
    let Some(settings_reg) = world.get_resource::<ModSettingsRegistry>() else {
        return Ok(String::new());
    };
    let mut root = toml::Table::new();
    let app_types = world.get_resource::<AppTypeRegistry>();

    for binding in &settings_reg.bindings {
        match &binding.kind {
            SettingsBindingKind::WitnessResource(handle) => {
                let records = export_resource_fields(world, *handle)?;
                let dyn_struct = records_to_dynamic_struct(&records);
                if let Some(toml::Value::Table(tbl)) =
                    json_to_toml_value(dynamic_struct_to_json(&dyn_struct))
                {
                    root.insert(binding.group_name.clone(), toml::Value::Table(tbl));
                }
            }
            SettingsBindingKind::ReflectedSettingsGroup(tid) => {
                if let Some(app_types) = app_types {
                    let types = app_types.read();
                    if let Some(tbl) = export_reflected_settings_group_table(world, &types, *tid) {
                        root.insert(binding.group_name.clone(), toml::Value::Table(tbl));
                    }
                }
            }
        }
    }

    Ok(toml::to_string_pretty(&root)?)
}

fn import_mod_settings_toml_into_world(
    world: &mut World,
    toml_str: &str,
) -> Result<usize, ModSettingsError> {
    let _ = sync_witness_reflection_schemas(world);
    let parsed: toml::Table = toml::from_str(toml_str)?;
    let bindings = world
        .get_resource::<ModSettingsRegistry>()
        .map(|r| r.bindings.clone())
        .unwrap_or_default();

    let mut applied_groups = 0usize;
    for binding in bindings {
        let Some(toml::Value::Table(group_tbl)) = parsed.get(&binding.group_name) else {
            continue;
        };
        match binding.kind {
            SettingsBindingKind::WitnessResource(handle) => {
                let Some(fields_schema) = world
                    .get_resource::<ModReflectRegistry>()
                    .and_then(|r| r.witness_schema(handle).map(|s| s.fields.clone()))
                else {
                    continue;
                };
                let mut records = Vec::new();
                for (k, v) in group_tbl {
                    if let Some(desc) = fields_schema.iter().find(|f| f.name == *k)
                        && let Ok(rec) = json_to_record(k, &desc.type_tag, &toml_to_json_value(v))
                    {
                        records.push(rec);
                    }
                }
                if !records.is_empty() {
                    apply_resource_fields(world, handle, &records)?;
                    applied_groups += 1;
                }
            }
            SettingsBindingKind::ReflectedSettingsGroup(tid) => {
                if let Some(app_types) = world.get_resource::<AppTypeRegistry>().cloned()
                    && apply_reflected_settings_group_table(world, &app_types, tid, group_tbl)
                {
                    applied_groups += 1;
                }
            }
        }
    }
    Ok(applied_groups)
}
