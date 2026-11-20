#![forbid(unsafe_code)]
#![warn(missing_docs)]

//! Safe (`#![forbid(unsafe_code)]`) dynamic `bevy_reflect`, Bevy Remote Protocol (`bevy_remote`
//! BRP), `bevy_settings` TOML, versioned save-game persistence (`ModSaveGamePlugin`), and live
//! `bql_lsp` / BRP telemetry (`ModTelemetryPlugin`) bridges for `bevy_abi_mod` (`ABI 0.3.0`).
//!
//! Exposes both witness-backed and declarative ABI `0.3` `BamFieldDesc` components and singleton
//! resources to Bevy's reflection registry (`AppTypeRegistry` / `ReflectFromPtr`), JSON-RPC
//! remote inspection/mutation (`bevy_abi_mod/*`, `bam/*`, and standard `world.*` BRP methods),
//! persistent TOML settings files, persistent (`!ModScoped`) entity save archives with recovery
//! and schema migration policies, and non-blocking 64-byte binary telemetry streaming.

pub(crate) mod codec;
pub mod reflect;
pub mod savegame;
pub mod telemetry;

pub use bevy_abi_mod_host_sys::{
    reflect_entity_component, reflect_entity_component_mut, reflect_resource_component,
    reflect_resource_component_mut,
};
pub use reflect::{
    BRP_METHOD_GET_COMPONENTS, BRP_METHOD_GET_RESOURCES, BRP_METHOD_INSERT_COMPONENTS,
    BRP_METHOD_LIST_COMPONENTS, BRP_METHOD_MUTATE_COMPONENTS, BRP_METHOD_MUTATE_RESOURCES,
    ModReflectAppExt, ModReflectError, ModReflectPlugin, ModReflectRegistry, ModReflectRemoteExt,
    ModReflectWorldExt, ModSettingsError, ModSettingsRegistry, TypedReflectBridgeEntry,
    WitnessReflectSchema, handle_mod_brp_request, install_mod_brp_methods,
};
pub use savegame::{
    ComponentMigrationContext, EphemeralHandleBehavior, MOD_SAVE_ARCHIVE_VERSION,
    MOD_SAVE_BINARY_MAGIC, MigratedComponentPayload, ModComponentMigrator, ModSaveAppExt,
    ModSaveArchive, ModSaveFormatPreference, ModSaveGamePlugin, ModSaveLoadError,
    ModSaveLoadPolicy, ModSaveLoadReport, ModSaveOptions, ModSaveRegistry, ModSaveWorldExt,
    SavedComponentEntry, SavedComponentFormat, SavedComponentHeader, SavedComponentPayload,
    SavedEntityRecord, SavedFieldHeader, SavedModMetadata,
};
pub use telemetry::{
    BRP_METHOD_BAM_COMPONENT_SCHEMAS, BRP_METHOD_BAM_HOT_RELOAD, BRP_METHOD_BAM_LIST_MODS,
    BRP_METHOD_BAM_SYSTEM_METRICS, CustomHotReloadHandler, HotReloadOutcome, HotReloadRequest,
    ModHotReloadQueue, ModTelemetryEndpoint, ModTelemetryPlugin, ModTelemetryRecorder,
    ModTelemetryRemoteExt, ModTelemetryTransport, ModTelemetryWorldExt,
    SYSTEM_TELEMETRY_DIGEST_SIZE, SYSTEM_TELEMETRY_DIGEST_VERSION, SYSTEM_TELEMETRY_MAGIC,
    SummaryTickTableStats, SystemExecutionSample, SystemMetricSnapshot, SystemTelemetryDigest64,
    SystemTelemetryMetrics, TELEMETRY_FLAG_BUDGET_BREACHED, TELEMETRY_FLAG_FAULTED,
    TELEMETRY_FLAG_QUARANTINED, TELEMETRY_FLAG_SUMMARY_TICK_ACTIVE, TelemetryFrameError,
    flush_mod_telemetry_system, fnv1a_64_hash_system, handle_telemetry_brp_request,
    install_bam_telemetry_brp_methods, probe_summary_tick_tables, process_pending_hot_reloads,
};

/// Convenient prelude re-exporting reflection, BRP, settings, save-game persistence, and live
/// telemetry plugins, traits, policies, digest structs, and error types.
pub mod prelude {
    pub use crate::reflect::{
        ModReflectAppExt, ModReflectError, ModReflectPlugin, ModReflectRegistry,
        ModReflectRemoteExt, ModReflectWorldExt, ModSettingsError, ModSettingsRegistry,
    };
    pub use crate::savegame::{
        ComponentMigrationContext, EphemeralHandleBehavior, MigratedComponentPayload,
        ModSaveAppExt, ModSaveArchive, ModSaveFormatPreference, ModSaveGamePlugin,
        ModSaveLoadError, ModSaveLoadPolicy, ModSaveLoadReport, ModSaveOptions, ModSaveRegistry,
        ModSaveWorldExt, SavedComponentFormat, SavedComponentHeader, SavedComponentPayload,
    };
    pub use crate::telemetry::{
        HotReloadOutcome, HotReloadRequest, ModHotReloadQueue, ModTelemetryEndpoint,
        ModTelemetryPlugin, ModTelemetryRecorder, ModTelemetryRemoteExt, ModTelemetryTransport,
        ModTelemetryWorldExt, SummaryTickTableStats, SystemExecutionSample, SystemMetricSnapshot,
        SystemTelemetryDigest64, SystemTelemetryMetrics, TelemetryFrameError,
    };
}
