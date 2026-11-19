//! Isolated `mod://<mod_name>/...` asset source, opt-in load-order [`AssetOverlayPolicy`]
//! (`mod://overlay/...`), traversal-safe `AssetReader`, and [`HostAssetBackend`] implementation
//! for `bevy_asset` (`0.20`).
//!
//! Enforces `#![forbid(unsafe_code)]`, per-mod directory sandboxing (`..` and symlink escape
//! rejection), owner-scoped namespace isolation, opt-in deterministic topological/priority asset
//! overlays with automatic revert on mod unload, typed (`LoadBuilder::load_erased`) and untyped
//! (`LoadBuilder::load_untyped`) async loading, and automatic handle release on mod unload.

use bevy_abi_mod_host::{ModDependencyGraph, register_mod_unload_hook_in_world};
use bevy_abi_mod_host_sys::{
    ComponentCollisionError, HostAssetBackend, ModHost, ModId, ModRegistry, RegistrySnapshot,
};
use bevy_abi_mod_sys::{BamComponent, ComponentUuid, FfiAssetHandle, FfiAssetLoadState, FfiResult};
use bevy_app::{App, Plugin, PreUpdate};
use bevy_asset::io::file::FileAssetReader;
use bevy_asset::io::{
    AssetReader, AssetReaderError, AssetSourceBuilder, PathStream, Reader, VecReader,
};
use bevy_asset::uuid::Uuid;
use bevy_asset::{
    Asset, AssetApp, AssetIndex, AssetServer, Assets, LoadState, LoadedUntypedAsset,
    UntypedAssetId, UntypedHandle,
};
use bevy_ecs::change_detection::DetectChanges;
use bevy_ecs::prelude::{Res, Resource};
use bevy_ecs::world::World;
use bevy_platform::collections::{HashMap, HashSet};
use core::any::TypeId;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

/// Reserved `mod://` namespace used for opt-in load-order overlay resolution
/// (`mod://overlay/<rel_path>`).
pub const OVERLAY_NAMESPACE: &str = "overlay";

const MOD_ASSET_UNLOAD_HOOK_KEY: &str = "bevy_abi_mod_bridges::asset_overlay_unload";

/// Opt-in policy governing how `mod://overlay/<rel_path>` asset paths resolve across active mods.
///
/// Defaults to [`Self::Disabled`], preserving strict per-mod namespace isolation.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum AssetOverlayPolicy {
    /// Strict per-mod namespace isolation (default): `mod://overlay/...` requests are rejected.
    #[default]
    Disabled,
    /// Resolves `mod://overlay/<rel_path>` by active mount/load order (most recently mounted mod
    /// has highest priority; earlier mounted mods and optional base directory serve as fallbacks).
    LoadOrderPriority,
    /// Resolves `mod://overlay/<rel_path>` using Kahn's deterministic topological order computed
    /// from [`ModDependencyGraph`] (dependents override their dependencies with lexicographical
    /// tie-breaking; earlier dependencies and optional base directory serve as fallbacks).
    TopologicalOverlay,
    /// Resolves `mod://overlay/<rel_path>` using an explicit caller-supplied priority list of mod
    /// namespaces ordered **highest-priority first** (only currently mounted mods participate,
    /// falling back to the optional base directory if configured).
    ExplicitPriority(Vec<String>),
}

impl AssetOverlayPolicy {
    /// Returns `true` if overlay resolution (`mod://overlay/...`) is enabled.
    #[inline]
    pub const fn is_enabled(&self) -> bool {
        !matches!(self, Self::Disabled)
    }

    /// Constructs [`Self::ExplicitPriority`] from an iterator of mod names (highest-priority first).
    pub fn explicit_priority(mods: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self::ExplicitPriority(mods.into_iter().map(Into::into).collect())
    }
}

#[derive(Default)]
struct OverlayState {
    policy: AssetOverlayPolicy,
    /// Active mounted mod names in mount insertion order (oldest first -> newest last).
    mount_order: Vec<String>,
    /// Cached topological load order (dependencies first -> dependents last).
    topo_load_order: Vec<String>,
    /// Optional canonicalized base/fallback asset directory consulted after all active mod layers.
    base_dir: Option<PathBuf>,
    /// Mounted `mod_name -> ModId` bindings for DAG & unload-hook tracking.
    mod_ids_by_name: HashMap<String, ModId>,
    /// Reverse `ModId -> mod_name` bindings for unload-hook lookup.
    mod_names_by_id: HashMap<ModId, String>,
}

#[derive(Default)]
struct ModAssetInner {
    dirs: RwLock<HashMap<String, PathBuf>>,
    overlay: RwLock<OverlayState>,
    asset_server: RwLock<Option<AssetServer>>,
    bam_to_type_id: RwLock<HashMap<BamComponent, TypeId>>,
    untyped_handle_type: RwLock<Option<BamComponent>>,
    retained: Mutex<HashMap<Option<String>, HashMap<FfiAssetHandle, (UntypedHandle, usize)>>>,
    tracked_overlay_paths: Mutex<HashSet<String>>,
    last_effective_snapshot: Mutex<(Vec<String>, Option<PathBuf>)>,
}

impl ModAssetInner {
    fn is_overlay_enabled(&self) -> bool {
        self.overlay.read().is_ok_and(|ov| ov.policy.is_enabled())
    }

    fn record_overlay_path(&self, normalized_uri: &str) {
        if let Ok(mut set) = self.tracked_overlay_paths.lock() {
            set.insert(normalized_uri.to_string());
        }
    }

    fn effective_overlay_order_locked(
        dirs: &HashMap<String, PathBuf>,
        ov: &OverlayState,
    ) -> Vec<String> {
        match &ov.policy {
            AssetOverlayPolicy::Disabled => Vec::new(),
            AssetOverlayPolicy::LoadOrderPriority => ov
                .mount_order
                .iter()
                .rev()
                .filter(|name| dirs.contains_key(name.as_str()))
                .cloned()
                .collect(),
            AssetOverlayPolicy::TopologicalOverlay => {
                let mut out = Vec::with_capacity(dirs.len());
                let mut seen: HashSet<&str> = HashSet::default();
                for name in ov.topo_load_order.iter().rev() {
                    if dirs.contains_key(name.as_str()) && seen.insert(name.as_str()) {
                        out.push(name.clone());
                    }
                }
                for name in ov.mount_order.iter().rev() {
                    if dirs.contains_key(name.as_str()) && seen.insert(name.as_str()) {
                        out.push(name.clone());
                    }
                }
                out
            }
            AssetOverlayPolicy::ExplicitPriority(priority) => {
                let mut out = Vec::with_capacity(priority.len());
                let mut seen: HashSet<&str> = HashSet::default();
                for name in priority {
                    if dirs.contains_key(name.as_str()) && seen.insert(name.as_str()) {
                        out.push(name.clone());
                    }
                }
                out
            }
        }
    }

    fn active_overlay_layers(&self) -> (AssetOverlayPolicy, Vec<(Option<String>, PathBuf)>) {
        let Ok(dirs) = self.dirs.read() else {
            return (AssetOverlayPolicy::Disabled, Vec::new());
        };
        let Ok(ov) = self.overlay.read() else {
            return (AssetOverlayPolicy::Disabled, Vec::new());
        };
        let policy = ov.policy.clone();
        if !policy.is_enabled() {
            return (policy, Vec::new());
        }
        let ordered = Self::effective_overlay_order_locked(&dirs, &ov);
        let mut layers: Vec<(Option<String>, PathBuf)> = Vec::with_capacity(ordered.len() + 1);
        for name in ordered {
            if let Some(root) = dirs.get(&name) {
                layers.push((Some(name), root.clone()));
            }
        }
        if let Some(base) = &ov.base_dir {
            layers.push((None, base.clone()));
        }
        (policy, layers)
    }

    fn active_overlay_order(&self) -> Vec<String> {
        let Ok(dirs) = self.dirs.read() else {
            return Vec::new();
        };
        let Ok(ov) = self.overlay.read() else {
            return Vec::new();
        };
        Self::effective_overlay_order_locked(&dirs, &ov)
    }

    fn reload_tracked_overlay_paths(&self, force: bool) {
        let current_snapshot = {
            let Ok(dirs) = self.dirs.read() else {
                return;
            };
            let Ok(ov) = self.overlay.read() else {
                return;
            };
            let order = Self::effective_overlay_order_locked(&dirs, &ov);
            (order, ov.base_dir.clone())
        };

        let should_reload = {
            let Ok(mut last) = self.last_effective_snapshot.lock() else {
                return;
            };
            let changed = *last != current_snapshot;
            if changed {
                *last = current_snapshot;
            }
            force || changed
        };

        if !should_reload {
            return;
        }

        let server = self.asset_server.read().ok().and_then(|g| g.clone());
        let Some(server) = server else {
            return;
        };
        let paths: Vec<String> = self
            .tracked_overlay_paths
            .lock()
            .map(|set| set.iter().cloned().collect())
            .unwrap_or_default();
        for path in paths {
            server.reload(path);
        }
    }

    fn recompute_topo_order(
        &self,
        dag: &ModDependencyGraph,
        host: Option<&ModHost>,
        excluding_mod_id: Option<ModId>,
        excluding_mod_name: Option<&str>,
    ) {
        let mounted_names: BTreeSet<String> = self
            .dirs
            .read()
            .map(|dirs| {
                dirs.keys()
                    .filter(|n| excluding_mod_name.is_none_or(|ex| ex != n.as_str()))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();

        let mut active_ids: BTreeSet<ModId> = BTreeSet::new();
        if let Ok(ov) = self.overlay.read() {
            for &id in ov.mod_ids_by_name.values() {
                if excluding_mod_id != Some(id) {
                    active_ids.insert(id);
                }
            }
        }
        if let Some(host) = host {
            for loaded in host.loaded_mods() {
                if loaded.is_active() && excluding_mod_id != Some(loaded.id()) {
                    active_ids.insert(loaded.id());
                }
            }
        }

        // Build deterministic Kahn topological graph over active DAG nodes + mounted mods.
        // Including asset-less intermediary DAG nodes preserves transitive dependencies
        // (`A <- M (no assets) <- B` => `B` still overrides `A`).
        let mut vertices: BTreeSet<String> = mounted_names;
        let mut raw_deps: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for id in active_ids {
            if let Some(node) = dag.get_node(id)
                && excluding_mod_name.is_none_or(|ex| ex != node.name.as_str())
            {
                vertices.insert(node.name.clone());
                raw_deps.insert(node.name.clone(), node.dependencies.clone());
            }
        }

        let mut in_degree: BTreeMap<String, usize> =
            vertices.iter().map(|v| (v.clone(), 0usize)).collect();
        let mut dependents: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();

        for (mod_name, deps) in &raw_deps {
            let mut unique_deps: BTreeSet<&str> = BTreeSet::new();
            for dep in deps {
                if dep != mod_name && vertices.contains(dep) {
                    unique_deps.insert(dep.as_str());
                }
            }
            for dep in unique_deps {
                dependents
                    .entry(dep.to_string())
                    .or_default()
                    .insert(mod_name.clone());
                if let Some(deg) = in_degree.get_mut(mod_name) {
                    *deg += 1;
                }
            }
        }

        let mut ready: BTreeSet<String> = in_degree
            .iter()
            .filter(|&(_, &deg)| deg == 0)
            .map(|(k, _)| k.clone())
            .collect();

        let mut topo_load_order = Vec::with_capacity(vertices.len());
        while let Some(name) = ready.pop_first() {
            topo_load_order.push(name.clone());
            if let Some(children) = dependents.get(&name) {
                for child in children {
                    if let Some(deg) = in_degree.get_mut(child) {
                        *deg = deg.saturating_sub(1);
                        if *deg == 0 {
                            ready.insert(child.clone());
                        }
                    }
                }
            }
        }

        // Deterministic fallback if any external cyclic edge was manually injected into the DAG.
        for (name, deg) in in_degree {
            if deg > 0 {
                topo_load_order.push(name);
            }
        }

        if let Ok(mut ov) = self.overlay.write() {
            ov.topo_load_order = topo_load_order;
        }
    }
}

/// Resource tracking per-mod asset root directories, opt-in [`AssetOverlayPolicy`] resolution,
/// registered `BamComponent -> TypeId` asset mappings, and host-retained strong [`UntypedHandle`]
/// pins.
#[derive(Resource, Clone, Default)]
pub struct ModAssetRegistry {
    inner: Arc<ModAssetInner>,
}

impl ModAssetRegistry {
    /// Creates an empty [`ModAssetRegistry`] with [`AssetOverlayPolicy::Disabled`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Mounts `root_dir` as the isolated filesystem root for `mod_name` (`mod://<mod_name>/...`).
    ///
    /// Canonicalizes `root_dir` immediately so subsequent symlink-escape checks work reliably
    /// across platforms (including macOS `/var` -> `/private/var`).
    pub fn mount_mod_asset_dir(
        &self,
        mod_name: &str,
        root_dir: impl AsRef<Path>,
    ) -> std::io::Result<PathBuf> {
        if mod_name.is_empty()
            || mod_name == OVERLAY_NAMESPACE
            || mod_name == "."
            || mod_name == ".."
            || mod_name.contains('/')
            || mod_name.contains('\\')
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "invalid or reserved mod asset namespace",
            ));
        }
        let canonical = std::fs::canonicalize(root_dir.as_ref())?;
        if !canonical.is_dir() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "mod asset root must be a directory",
            ));
        }
        if let Ok(mut dirs) = self.inner.dirs.write() {
            dirs.insert(mod_name.to_string(), canonical.clone());
        }
        if let Ok(mut ov) = self.inner.overlay.write() {
            if let Some(pos) = ov.mount_order.iter().position(|n| n == mod_name) {
                ov.mount_order.remove(pos);
            }
            ov.mount_order.push(mod_name.to_string());
        }
        if self.inner.is_overlay_enabled() {
            self.inner.reload_tracked_overlay_paths(true);
        }
        Ok(canonical)
    }

    /// Mounts `root_dir` for `mod_name` and binds it to `mod_id` so [`ModDependencyGraph`]
    /// topological ordering and automatic [`ModUnloadHooks`](bevy_abi_mod_host::ModUnloadHooks)
    /// teardown track this mod automatically.
    pub fn mount_mod_asset_dir_for_mod(
        &self,
        mod_id: ModId,
        mod_name: &str,
        root_dir: impl AsRef<Path>,
    ) -> std::io::Result<PathBuf> {
        let canonical = self.mount_mod_asset_dir(mod_name, root_dir)?;
        self.bind_mod_id(mod_name, mod_id);
        Ok(canonical)
    }

    /// Associates a mounted `mod_name` namespace with its runtime [`ModId`].
    pub fn bind_mod_id(&self, mod_name: &str, mod_id: ModId) {
        if let Ok(mut ov) = self.inner.overlay.write() {
            if let Some(old_id) = ov.mod_ids_by_name.insert(mod_name.to_string(), mod_id)
                && old_id != mod_id
            {
                ov.mod_names_by_id.remove(&old_id);
            }
            ov.mod_names_by_id.insert(mod_id, mod_name.to_string());
        }
    }

    /// Removes the runtime [`ModId`] binding, returning the associated mod namespace if present.
    pub fn unbind_mod_id(&self, mod_id: ModId) -> Option<String> {
        let mut ov = self.inner.overlay.write().ok()?;
        let name = ov.mod_names_by_id.remove(&mod_id)?;
        if ov.mod_ids_by_name.get(&name) == Some(&mod_id) {
            ov.mod_ids_by_name.remove(&name);
        }
        Some(name)
    }

    /// Returns the mod namespace bound to `mod_id`, if any.
    pub fn mod_name_for_id(&self, mod_id: ModId) -> Option<String> {
        self.inner
            .overlay
            .read()
            .ok()
            .and_then(|ov| ov.mod_names_by_id.get(&mod_id).cloned())
    }

    /// Unmounts `mod_name`'s asset directory, returning `true` if it was mounted.
    ///
    /// When an [`AssetOverlayPolicy`] is enabled, any live `mod://overlay/...` assets are
    /// automatically reloaded on [`AssetServer`] so overrides revert to lower-priority layers.
    pub fn unmount_mod_asset_dir(&self, mod_name: &str) -> bool {
        let removed = self
            .inner
            .dirs
            .write()
            .is_ok_and(|mut dirs| dirs.remove(mod_name).is_some());
        if let Ok(mut ov) = self.inner.overlay.write() {
            ov.mount_order.retain(|n| n != mod_name);
            ov.topo_load_order.retain(|n| n != mod_name);
            if let Some(id) = ov.mod_ids_by_name.remove(mod_name) {
                ov.mod_names_by_id.remove(&id);
            }
        }
        if removed && self.inner.is_overlay_enabled() {
            self.inner.reload_tracked_overlay_paths(true);
        }
        removed
    }

    /// Returns `true` if `mod_name` currently has a mounted asset directory.
    pub fn has_mod_asset_dir(&self, mod_name: &str) -> bool {
        self.inner
            .dirs
            .read()
            .is_ok_and(|dirs| dirs.contains_key(mod_name))
    }

    /// Configures the opt-in [`AssetOverlayPolicy`] used for `mod://overlay/<rel_path>` resolution,
    /// reloading any tracked overlay assets if the active layer stack changed.
    pub fn set_overlay_policy(&self, policy: AssetOverlayPolicy) {
        if let Ok(mut ov) = self.inner.overlay.write() {
            ov.policy = policy;
        }
        self.inner.reload_tracked_overlay_paths(true);
    }

    /// Returns the currently configured [`AssetOverlayPolicy`].
    pub fn overlay_policy(&self) -> AssetOverlayPolicy {
        self.inner
            .overlay
            .read()
            .map(|ov| ov.policy.clone())
            .unwrap_or_default()
    }

    /// Sets a canonicalized base/fallback asset directory consulted after all active mod overlay
    /// layers when resolving `mod://overlay/<rel_path>`.
    pub fn set_base_overlay_dir(&self, root_dir: impl AsRef<Path>) -> std::io::Result<PathBuf> {
        let canonical = std::fs::canonicalize(root_dir.as_ref())?;
        if !canonical.is_dir() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "base overlay asset root must be a directory",
            ));
        }
        if let Ok(mut ov) = self.inner.overlay.write() {
            ov.base_dir = Some(canonical.clone());
        }
        if self.inner.is_overlay_enabled() {
            self.inner.reload_tracked_overlay_paths(true);
        }
        Ok(canonical)
    }

    /// Clears the configured base/fallback overlay directory.
    pub fn clear_base_overlay_dir(&self) {
        if let Ok(mut ov) = self.inner.overlay.write() {
            ov.base_dir = None;
        }
        if self.inner.is_overlay_enabled() {
            self.inner.reload_tracked_overlay_paths(true);
        }
    }

    /// Returns the active mod namespaces participating in `mod://overlay/...` resolution ordered
    /// **highest-priority first** (empty when [`AssetOverlayPolicy::Disabled`]).
    pub fn active_overlay_order(&self) -> Vec<String> {
        self.inner.active_overlay_order()
    }

    /// Synchronizes topological overlay priority from `dag` (and optional `host`), reloading
    /// tracked `mod://overlay/...` assets on [`AssetServer`] if the effective priority order changed.
    pub fn sync_overlay_from_dependency_graph(
        &self,
        dag: &ModDependencyGraph,
        host: Option<&ModHost>,
    ) {
        self.inner.recompute_topo_order(dag, host, None, None);
        if self.inner.is_overlay_enabled() {
            self.inner.reload_tracked_overlay_paths(false);
        }
    }

    /// Resolves `rel_path` against the active overlay stack, returning
    /// `(Some(winning_mod_name), canonical_file_path)` for a mod layer or
    /// `(None, canonical_file_path)` when resolved from the base fallback directory.
    pub fn resolve_overlay_file(
        &self,
        rel_path: impl AsRef<Path>,
    ) -> Result<(Option<String>, PathBuf), AssetReaderError> {
        let joined = Path::new(OVERLAY_NAMESPACE).join(rel_path.as_ref());
        let reader = ModAssetReader::new(Arc::clone(&self.inner));
        let (owner, _, _, resolved) = reader.resolve_overlay_existing(&joined, false)?;
        Ok((owner, resolved))
    }

    /// Handles teardown for an unloading `mod_id`: unmounts its asset directory, releases all
    /// host-retained handles owned by its namespace, recomputes topological order excluding
    /// `mod_id`, and reloads tracked `mod://overlay/...` assets so overrides revert automatically.
    pub fn handle_mod_unloaded(
        &self,
        mod_id: ModId,
        fallback_mod_name: Option<&str>,
        dag: Option<&ModDependencyGraph>,
        host: Option<&ModHost>,
    ) -> Option<String> {
        let mod_name = self
            .unbind_mod_id(mod_id)
            .or_else(|| fallback_mod_name.map(str::to_string));

        if let Some(ref name) = mod_name {
            if let Ok(mut dirs) = self.inner.dirs.write() {
                dirs.remove(name.as_str());
            }
            if let Ok(mut ov) = self.inner.overlay.write() {
                ov.mount_order.retain(|n| n != name);
                ov.topo_load_order.retain(|n| n != name);
                ov.mod_ids_by_name.remove(name.as_str());
            }
            if let Ok(mut retained) = self.inner.retained.lock() {
                retained.remove(&Some(name.clone()));
            }
        }

        if let Some(dag) = dag {
            self.inner
                .recompute_topo_order(dag, host, Some(mod_id), mod_name.as_deref());
        }
        if self.inner.is_overlay_enabled() {
            self.inner.reload_tracked_overlay_paths(true);
        }
        mod_name
    }

    /// Returns the number of strong asset handles currently retained for `owner_namespace`.
    pub fn retained_count_for_owner(&self, owner_namespace: Option<&str>) -> usize {
        self.inner
            .retained
            .lock()
            .ok()
            .and_then(|map| {
                map.get(&owner_namespace.map(str::to_string))
                    .map(|inner| inner.values().map(|(_, count)| *count).sum())
            })
            .unwrap_or(0)
    }

    fn bind_asset_server(&self, server: AssetServer) {
        if let Ok(mut guard) = self.inner.asset_server.write() {
            *guard = Some(server);
        }
    }

    fn record_asset_type(&self, handle: BamComponent, type_id: TypeId) {
        if let Ok(mut map) = self.inner.bam_to_type_id.write() {
            map.insert(handle, type_id);
        }
        if type_id == TypeId::of::<LoadedUntypedAsset>()
            && let Ok(mut untyped) = self.inner.untyped_handle_type.write()
        {
            *untyped = Some(handle);
        }
    }
}

fn on_mod_unloaded_asset_cleanup(world: &mut World, mod_id: ModId) {
    let Some(asset_reg) = world.get_resource::<ModAssetRegistry>().cloned() else {
        return;
    };
    let fallback_name = world
        .get_resource::<ModDependencyGraph>()
        .and_then(|dag| dag.get_node(mod_id))
        .map(|node| node.name.clone())
        .or_else(|| {
            world
                .get_resource::<ModHost>()
                .and_then(|host| host.get_mod(mod_id))
                .map(|m| m.name().to_string())
        });
    let dag = world.get_resource::<ModDependencyGraph>();
    let host = world.get_resource::<ModHost>();
    asset_reg.handle_mod_unloaded(mod_id, fallback_name.as_deref(), dag, host);
}

fn sync_mod_asset_overlay_system(
    dag: Option<Res<ModDependencyGraph>>,
    host: Option<Res<ModHost>>,
    asset_reg: Res<ModAssetRegistry>,
) {
    let dag_changed = dag.as_ref().is_some_and(|d| d.is_changed());
    let host_changed = host.as_ref().is_some_and(|h| h.is_changed());
    if (dag_changed || host_changed)
        && let Some(dag) = dag.as_deref()
    {
        asset_reg.sync_overlay_from_dependency_graph(dag, host.as_deref());
    }
}

fn build_mod_asset_plugin(
    app: &mut App,
    initial_policy: Option<AssetOverlayPolicy>,
    initial_base_dir: Option<&Path>,
) {
    let asset_reg = if let Some(existing) = app.world().get_resource::<ModAssetRegistry>() {
        existing.clone()
    } else {
        let reg = ModAssetRegistry::new();
        app.insert_resource(reg.clone());
        reg
    };

    if let Some(policy) = initial_policy {
        asset_reg.set_overlay_policy(policy);
    }
    if let Some(base_dir) = initial_base_dir {
        let _ = asset_reg.set_base_overlay_dir(base_dir);
    }

    let reader_inner = Arc::clone(&asset_reg.inner);
    app.register_asset_source(
        "mod",
        AssetSourceBuilder::new(move || Box::new(ModAssetReader::new(Arc::clone(&reader_inner)))),
    );

    if !app.world().contains_resource::<ModRegistry>() {
        app.insert_resource(ModRegistry::new());
    }
    let backend: Arc<dyn HostAssetBackend> = Arc::new(BevyHostAssetBackend {
        inner: Arc::clone(&asset_reg.inner),
    });
    app.world_mut()
        .resource_mut::<ModRegistry>()
        .set_asset_backend(backend);

    register_mod_unload_hook_in_world(
        app.world_mut(),
        MOD_ASSET_UNLOAD_HOOK_KEY,
        on_mod_unloaded_asset_cleanup,
    );
    app.add_systems(PreUpdate, sync_mod_asset_overlay_system);

    if let Some(server) = app.world().get_resource::<AssetServer>().cloned() {
        asset_reg.bind_asset_server(server);
    }
}

fn finish_mod_asset_plugin(app: &mut App) {
    let asset_reg = app
        .world()
        .get_resource::<ModAssetRegistry>()
        .cloned()
        .unwrap_or_default();
    if let Some(server) = app.world().get_resource::<AssetServer>().cloned() {
        asset_reg.bind_asset_server(server);
    }
    let _ = app.world_mut().register_mod_asset::<LoadedUntypedAsset>();
}

/// Bevy [`Plugin`] mounting the isolated `"mod"` [`AssetSourceBuilder`] (`mod://<mod_name>/...`),
/// installing [`ModAssetRegistry`] (with [`AssetOverlayPolicy::Disabled`] by default) and a
/// [`HostAssetBackend`] on [`ModRegistry`].
///
/// Use [`ModAssetPlugin::with_overlay_policy`] to opt into load-order or topological
/// `mod://overlay/...` resolution.
///
/// Must be added **before** Bevy's [`AssetPlugin`](bevy_asset::AssetPlugin) so the `"mod"` asset
/// source is registered into [`AssetSourceBuilders`](bevy_asset::io::AssetSourceBuilders).
#[derive(Default, Debug, Clone, Copy)]
pub struct ModAssetPlugin;

impl ModAssetPlugin {
    /// Creates a [`ConfiguredModAssetPlugin`] installing the `"mod"` asset source with an explicit
    /// opt-in [`AssetOverlayPolicy`].
    #[inline]
    pub const fn with_overlay_policy(policy: AssetOverlayPolicy) -> ConfiguredModAssetPlugin {
        ConfiguredModAssetPlugin {
            policy,
            base_dir: None,
        }
    }
}

impl Plugin for ModAssetPlugin {
    fn build(&self, app: &mut App) {
        build_mod_asset_plugin(app, None, None);
    }

    fn finish(&self, app: &mut App) {
        finish_mod_asset_plugin(app);
    }
}

/// Configured variant of [`ModAssetPlugin`] created by [`ModAssetPlugin::with_overlay_policy`].
#[derive(Debug, Clone)]
pub struct ConfiguredModAssetPlugin {
    policy: AssetOverlayPolicy,
    base_dir: Option<PathBuf>,
}

impl ConfiguredModAssetPlugin {
    /// Sets a base/fallback asset directory consulted after all active mod overlay layers.
    #[inline]
    pub fn with_base_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.base_dir = Some(dir.into());
        self
    }
}

impl Plugin for ConfiguredModAssetPlugin {
    fn build(&self, app: &mut App) {
        build_mod_asset_plugin(app, Some(self.policy.clone()), self.base_dir.as_deref());
    }

    fn finish(&self, app: &mut App) {
        finish_mod_asset_plugin(app);
    }
}

/// Extension trait on [`World`] for mounting mod asset directories, configuring opt-in
/// [`AssetOverlayPolicy`], and exposing [`Asset`] types across the C ABI.
pub trait ModAssetWorldExt {
    /// Mounts `root_dir` as the isolated filesystem root for `mod_name` (`mod://<mod_name>/...`).
    fn mount_mod_asset_dir(
        &mut self,
        mod_name: &str,
        root_dir: impl AsRef<Path>,
    ) -> std::io::Result<PathBuf>;

    /// Mounts `root_dir` for `mod_name` and associates it with `mod_id` for topological overlay
    /// resolution and automatic unload cleanup.
    fn mount_mod_asset_dir_for_mod(
        &mut self,
        mod_id: ModId,
        mod_name: &str,
        root_dir: impl AsRef<Path>,
    ) -> std::io::Result<PathBuf>;

    /// Unmounts `mod_name`'s asset directory.
    fn unmount_mod_asset_dir(&mut self, mod_name: &str) -> bool;

    /// Configures the opt-in [`AssetOverlayPolicy`] on [`ModAssetRegistry`].
    fn set_asset_overlay_policy(&mut self, policy: AssetOverlayPolicy);

    /// Returns the currently active [`AssetOverlayPolicy`].
    fn asset_overlay_policy(&self) -> AssetOverlayPolicy;

    /// Configures a base/fallback directory consulted after all active mod overlay layers.
    fn set_asset_overlay_base_dir(
        &mut self,
        root_dir: impl AsRef<Path>,
    ) -> std::io::Result<PathBuf>;

    /// Clears the configured base/fallback overlay directory.
    fn clear_asset_overlay_base_dir(&mut self);

    /// Synchronizes [`ModAssetRegistry`]'s topological overlay order immediately from
    /// [`ModDependencyGraph`] and [`ModHost`].
    fn sync_asset_overlay_order(&mut self);

    /// Exposes Bevy asset type `A` under `core::any::type_name::<A>()` (with optional short alias),
    /// returning its [`BamComponent`] handle for `asset_load`.
    fn register_mod_asset<A: Asset>(&mut self) -> Result<BamComponent, ComponentCollisionError>;

    /// Exposes Bevy asset type `A` with an explicit optional short alias (e.g. `Some("Image")`).
    fn register_mod_asset_with_alias<A: Asset>(
        &mut self,
        alias: Option<&str>,
    ) -> Result<BamComponent, ComponentCollisionError>;
}

impl ModAssetWorldExt for World {
    fn mount_mod_asset_dir(
        &mut self,
        mod_name: &str,
        root_dir: impl AsRef<Path>,
    ) -> std::io::Result<PathBuf> {
        if !self.contains_resource::<ModAssetRegistry>() {
            self.insert_resource(ModAssetRegistry::new());
        }
        self.resource::<ModAssetRegistry>()
            .mount_mod_asset_dir(mod_name, root_dir)
    }

    fn mount_mod_asset_dir_for_mod(
        &mut self,
        mod_id: ModId,
        mod_name: &str,
        root_dir: impl AsRef<Path>,
    ) -> std::io::Result<PathBuf> {
        if !self.contains_resource::<ModAssetRegistry>() {
            self.insert_resource(ModAssetRegistry::new());
        }
        self.resource::<ModAssetRegistry>()
            .mount_mod_asset_dir_for_mod(mod_id, mod_name, root_dir)
    }

    fn unmount_mod_asset_dir(&mut self, mod_name: &str) -> bool {
        self.get_resource::<ModAssetRegistry>()
            .is_some_and(|reg| reg.unmount_mod_asset_dir(mod_name))
    }

    fn set_asset_overlay_policy(&mut self, policy: AssetOverlayPolicy) {
        if !self.contains_resource::<ModAssetRegistry>() {
            self.insert_resource(ModAssetRegistry::new());
        }
        self.resource::<ModAssetRegistry>()
            .set_overlay_policy(policy);
    }

    fn asset_overlay_policy(&self) -> AssetOverlayPolicy {
        self.get_resource::<ModAssetRegistry>()
            .map(ModAssetRegistry::overlay_policy)
            .unwrap_or_default()
    }

    fn set_asset_overlay_base_dir(
        &mut self,
        root_dir: impl AsRef<Path>,
    ) -> std::io::Result<PathBuf> {
        if !self.contains_resource::<ModAssetRegistry>() {
            self.insert_resource(ModAssetRegistry::new());
        }
        self.resource::<ModAssetRegistry>()
            .set_base_overlay_dir(root_dir)
    }

    fn clear_asset_overlay_base_dir(&mut self) {
        if let Some(reg) = self.get_resource::<ModAssetRegistry>() {
            reg.clear_base_overlay_dir();
        }
    }

    fn sync_asset_overlay_order(&mut self) {
        let Some(reg) = self.get_resource::<ModAssetRegistry>().cloned() else {
            return;
        };
        if let Some(dag) = self.get_resource::<ModDependencyGraph>() {
            let host = self.get_resource::<ModHost>();
            reg.sync_overlay_from_dependency_graph(dag, host);
        }
    }

    fn register_mod_asset<A: Asset>(&mut self) -> Result<BamComponent, ComponentCollisionError> {
        let full = core::any::type_name::<A>();
        let short = full.rsplit("::").next().filter(|s| *s != full);
        self.register_mod_asset_with_alias::<A>(short)
    }

    fn register_mod_asset_with_alias<A: Asset>(
        &mut self,
        alias: Option<&str>,
    ) -> Result<BamComponent, ComponentCollisionError> {
        if !self.contains_resource::<ModRegistry>() {
            self.insert_resource(ModRegistry::new());
        }
        if !self.contains_resource::<ModAssetRegistry>() {
            self.insert_resource(ModAssetRegistry::new());
        }
        let assets_res_id = self.register_component::<Assets<A>>();
        let canonical = core::any::type_name::<A>();
        let handle = self.resource_mut::<ModRegistry>().register_asset_type(
            canonical,
            alias,
            assets_res_id,
        )?;
        self.resource::<ModAssetRegistry>()
            .record_asset_type(handle, TypeId::of::<A>());
        Ok(handle)
    }
}

/// Extension trait on [`App`] for mounting mod asset directories, configuring opt-in
/// [`AssetOverlayPolicy`], and exposing [`Asset`] types.
pub trait ModAssetAppExt {
    /// Mounts `root_dir` as the isolated filesystem root for `mod_name` (`mod://<mod_name>/...`).
    fn mount_mod_asset_dir(
        &mut self,
        mod_name: &str,
        root_dir: impl AsRef<Path>,
    ) -> std::io::Result<PathBuf>;

    /// Mounts `root_dir` for `mod_name` and associates it with `mod_id` for topological overlay
    /// resolution and automatic unload cleanup.
    fn mount_mod_asset_dir_for_mod(
        &mut self,
        mod_id: ModId,
        mod_name: &str,
        root_dir: impl AsRef<Path>,
    ) -> std::io::Result<PathBuf>;

    /// Unmounts `mod_name`'s asset directory.
    fn unmount_mod_asset_dir(&mut self, mod_name: &str) -> bool;

    /// Configures the opt-in [`AssetOverlayPolicy`] on [`ModAssetRegistry`].
    fn set_asset_overlay_policy(&mut self, policy: AssetOverlayPolicy);

    /// Returns the currently active [`AssetOverlayPolicy`].
    fn asset_overlay_policy(&self) -> AssetOverlayPolicy;

    /// Configures a base/fallback directory consulted after all active mod overlay layers.
    fn set_asset_overlay_base_dir(
        &mut self,
        root_dir: impl AsRef<Path>,
    ) -> std::io::Result<PathBuf>;

    /// Clears the configured base/fallback overlay directory.
    fn clear_asset_overlay_base_dir(&mut self);

    /// Synchronizes [`ModAssetRegistry`]'s topological overlay order immediately from
    /// [`ModDependencyGraph`] and [`ModHost`].
    fn sync_asset_overlay_order(&mut self);

    /// Initializes `A` (when [`AssetServer`] is present) and exposes `A` to mods.
    fn register_mod_asset<A: Asset>(&mut self) -> Result<BamComponent, ComponentCollisionError>;

    /// Initializes `A` (when [`AssetServer`] is present) and exposes `A` with an optional alias.
    fn register_mod_asset_with_alias<A: Asset>(
        &mut self,
        alias: Option<&str>,
    ) -> Result<BamComponent, ComponentCollisionError>;
}

impl ModAssetAppExt for App {
    #[inline]
    fn mount_mod_asset_dir(
        &mut self,
        mod_name: &str,
        root_dir: impl AsRef<Path>,
    ) -> std::io::Result<PathBuf> {
        self.world_mut().mount_mod_asset_dir(mod_name, root_dir)
    }

    #[inline]
    fn mount_mod_asset_dir_for_mod(
        &mut self,
        mod_id: ModId,
        mod_name: &str,
        root_dir: impl AsRef<Path>,
    ) -> std::io::Result<PathBuf> {
        self.world_mut()
            .mount_mod_asset_dir_for_mod(mod_id, mod_name, root_dir)
    }

    #[inline]
    fn unmount_mod_asset_dir(&mut self, mod_name: &str) -> bool {
        self.world_mut().unmount_mod_asset_dir(mod_name)
    }

    #[inline]
    fn set_asset_overlay_policy(&mut self, policy: AssetOverlayPolicy) {
        self.world_mut().set_asset_overlay_policy(policy);
    }

    #[inline]
    fn asset_overlay_policy(&self) -> AssetOverlayPolicy {
        self.world().asset_overlay_policy()
    }

    #[inline]
    fn set_asset_overlay_base_dir(
        &mut self,
        root_dir: impl AsRef<Path>,
    ) -> std::io::Result<PathBuf> {
        self.world_mut().set_asset_overlay_base_dir(root_dir)
    }

    #[inline]
    fn clear_asset_overlay_base_dir(&mut self) {
        self.world_mut().clear_asset_overlay_base_dir();
    }

    #[inline]
    fn sync_asset_overlay_order(&mut self) {
        self.world_mut().sync_asset_overlay_order();
    }

    fn register_mod_asset<A: Asset>(&mut self) -> Result<BamComponent, ComponentCollisionError> {
        if self.world().contains_resource::<AssetServer>()
            && !self.world().contains_resource::<Assets<A>>()
        {
            self.init_asset::<A>();
        }
        self.world_mut().register_mod_asset::<A>()
    }

    fn register_mod_asset_with_alias<A: Asset>(
        &mut self,
        alias: Option<&str>,
    ) -> Result<BamComponent, ComponentCollisionError> {
        if self.world().contains_resource::<AssetServer>()
            && !self.world().contains_resource::<Assets<A>>()
        {
            self.init_asset::<A>();
        }
        self.world_mut().register_mod_asset_with_alias::<A>(alias)
    }
}

struct ModAssetReader {
    inner: Arc<ModAssetInner>,
}

impl ModAssetReader {
    fn new(inner: Arc<ModAssetInner>) -> Self {
        Self { inner }
    }

    fn split_mod_subpath(path: &Path) -> Result<(&str, PathBuf), AssetReaderError> {
        let mut comps = path.components();
        let Some(first) = comps.next() else {
            return Err(AssetReaderError::NotFound(path.to_path_buf()));
        };
        let Component::Normal(mod_os) = first else {
            return Err(permission_denied_error("invalid mod asset root component"));
        };
        let Some(mod_name) = mod_os.to_str() else {
            return Err(AssetReaderError::NotFound(path.to_path_buf()));
        };
        if mod_name.is_empty() || mod_name == "." || mod_name == ".." {
            return Err(permission_denied_error("invalid mod asset namespace"));
        }

        let mut subpath = PathBuf::new();
        for comp in comps {
            match comp {
                Component::Normal(seg) => subpath.push(seg),
                Component::ParentDir
                | Component::RootDir
                | Component::Prefix(_)
                | Component::CurDir => {
                    return Err(permission_denied_error(
                        "path traversal is forbidden in mod:// asset paths",
                    ));
                }
            }
        }
        Ok((mod_name, subpath))
    }

    fn resolve_within_root(root_dir: &Path, subpath: &Path) -> Result<PathBuf, AssetReaderError> {
        let candidate = if subpath.as_os_str().is_empty() {
            root_dir.to_path_buf()
        } else {
            root_dir.join(subpath)
        };

        let canonical_target = std::fs::canonicalize(&candidate).map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                AssetReaderError::NotFound(candidate.clone())
            } else {
                AssetReaderError::Io(Arc::new(err))
            }
        })?;

        if !canonical_target.starts_with(root_dir) {
            return Err(permission_denied_error(
                "resolved asset path escapes mounted mod directory",
            ));
        }

        Ok(canonical_target)
    }

    fn resolve_overlay_existing(
        &self,
        path: &Path,
        append_meta: bool,
    ) -> Result<(Option<String>, PathBuf, PathBuf, PathBuf), AssetReaderError> {
        let (namespace, base_subpath) = Self::split_mod_subpath(path)?;
        debug_assert_eq!(namespace, OVERLAY_NAMESPACE);

        let (policy, layers) = self.inner.active_overlay_layers();
        if !policy.is_enabled() {
            return Err(permission_denied_error(
                "mod://overlay/ resolution is disabled by AssetOverlayPolicy::Disabled",
            ));
        }
        if base_subpath.as_os_str().is_empty() {
            return Err(AssetReaderError::NotFound(path.to_path_buf()));
        }

        if !append_meta && let Some(sub_str) = base_subpath.to_str() {
            let normalized = format!("mod://{OVERLAY_NAMESPACE}/{}", sub_str.replace('\\', "/"));
            self.inner.record_overlay_path(&normalized);
        }

        // For `.meta` requests, find the winning layer for the base asset first so a lower layer's
        // `.meta` file cannot override metadata for a higher-priority mod's replaced asset.
        if append_meta {
            let meta_subpath = append_meta_extension(&base_subpath);
            for (owner, root_dir) in &layers {
                match Self::resolve_within_root(root_dir, &base_subpath) {
                    Ok(_) => {
                        let resolved_meta = Self::resolve_within_root(root_dir, &meta_subpath)?;
                        return Ok((owner.clone(), root_dir.clone(), meta_subpath, resolved_meta));
                    }
                    Err(AssetReaderError::NotFound(_)) => continue,
                    Err(err) => return Err(err),
                }
            }
            return Err(AssetReaderError::NotFound(path.to_path_buf()));
        }

        for (owner, root_dir) in layers {
            match Self::resolve_within_root(&root_dir, &base_subpath) {
                Ok(canonical_target) => {
                    return Ok((owner, root_dir, base_subpath, canonical_target));
                }
                Err(AssetReaderError::NotFound(_)) => continue,
                Err(err) => return Err(err),
            }
        }

        Err(AssetReaderError::NotFound(path.to_path_buf()))
    }

    fn resolve_existing_path(
        &self,
        path: &Path,
        append_meta: bool,
    ) -> Result<(PathBuf, PathBuf, PathBuf), AssetReaderError> {
        let (mod_name, mut subpath) = Self::split_mod_subpath(path)?;
        if mod_name == OVERLAY_NAMESPACE {
            let (_, root_dir, effective_subpath, canonical_target) =
                self.resolve_overlay_existing(path, append_meta)?;
            return Ok((root_dir, effective_subpath, canonical_target));
        }

        if append_meta {
            if subpath.as_os_str().is_empty() {
                return Err(AssetReaderError::NotFound(path.to_path_buf()));
            }
            subpath = append_meta_extension(&subpath);
        }

        let root_dir = self
            .inner
            .dirs
            .read()
            .ok()
            .and_then(|dirs| dirs.get(mod_name).cloned())
            .ok_or_else(|| AssetReaderError::NotFound(path.to_path_buf()))?;

        let canonical_target = Self::resolve_within_root(&root_dir, &subpath)?;
        Ok((root_dir, subpath, canonical_target))
    }
}

fn permission_denied_error(msg: &'static str) -> AssetReaderError {
    AssetReaderError::Io(Arc::new(std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        msg,
    )))
}

fn append_meta_extension(path: &Path) -> PathBuf {
    let mut meta_path = path.to_path_buf();
    let mut extension = path.extension().unwrap_or_default().to_os_string();
    if !extension.is_empty() {
        extension.push(".");
    }
    extension.push("meta");
    meta_path.set_extension(extension);
    meta_path
}

impl AssetReader for ModAssetReader {
    async fn read<'a>(&'a self, path: &'a Path) -> Result<impl Reader + 'a, AssetReaderError> {
        let (_, _, resolved) = self.resolve_existing_path(path, false)?;
        let bytes = std::fs::read(&resolved).map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                AssetReaderError::NotFound(resolved)
            } else {
                AssetReaderError::Io(Arc::new(err))
            }
        })?;
        Ok(VecReader::new(bytes))
    }

    async fn read_meta<'a>(&'a self, path: &'a Path) -> Result<impl Reader + 'a, AssetReaderError> {
        let (_, _, resolved) = self.resolve_existing_path(path, true)?;
        let bytes = std::fs::read(&resolved).map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                AssetReaderError::NotFound(resolved)
            } else {
                AssetReaderError::Io(Arc::new(err))
            }
        })?;
        Ok(VecReader::new(bytes))
    }

    async fn read_directory<'a>(
        &'a self,
        path: &'a Path,
    ) -> Result<Box<PathStream>, AssetReaderError> {
        let (root_dir, subpath, _) = self.resolve_existing_path(path, false)?;
        let reader = FileAssetReader::new(&root_dir);
        reader.read_directory(&subpath).await
    }

    async fn is_directory<'a>(&'a self, path: &'a Path) -> Result<bool, AssetReaderError> {
        let (_, _, resolved) = self.resolve_existing_path(path, false)?;
        Ok(resolved.is_dir())
    }
}

struct BevyHostAssetBackend {
    inner: Arc<ModAssetInner>,
}

impl BevyHostAssetBackend {
    fn get_asset_server(&self) -> Result<AssetServer, (FfiResult, String)> {
        self.inner
            .asset_server
            .read()
            .ok()
            .and_then(|g| g.clone())
            .ok_or_else(|| {
                (
                    FfiResult::INVALID_STATE,
                    "AssetServer is not initialized on ModAssetRegistry".to_string(),
                )
            })
    }

    fn normalize_asset_path(
        &self,
        owner_namespace: Option<&str>,
        raw_path: &str,
    ) -> Result<String, (FfiResult, String)> {
        if raw_path.is_empty() || raw_path.contains('\0') || raw_path.contains('\\') {
            return Err((
                FfiResult::INVALID_ARGUMENT,
                format!("invalid asset path '{raw_path}'"),
            ));
        }
        if let Some((scheme, rest)) = raw_path.split_once("://") {
            if scheme == "mod" {
                let Some((target_mod, subpath)) = rest.split_once('/') else {
                    return Err((
                        FfiResult::INVALID_ARGUMENT,
                        format!("mod:// asset path must have '<mod_id>/<subpath>': '{raw_path}'"),
                    ));
                };
                if target_mod.is_empty()
                    || subpath.is_empty()
                    || target_mod == "."
                    || target_mod == ".."
                    || has_unsafe_segments(subpath)
                {
                    return Err((
                        FfiResult::PERMISSION_DENIED,
                        format!("traversal rejected in asset path '{raw_path}'"),
                    ));
                }
                if target_mod == OVERLAY_NAMESPACE {
                    if !self.inner.is_overlay_enabled() {
                        return Err((
                            FfiResult::PERMISSION_DENIED,
                            format!(
                                "overlay asset access '{raw_path}' is disabled by AssetOverlayPolicy::Disabled"
                            ),
                        ));
                    }
                    let normalized = format!("mod://{OVERLAY_NAMESPACE}/{subpath}");
                    self.inner.record_overlay_path(&normalized);
                    return Ok(normalized);
                }
                if let Some(ns) = owner_namespace
                    && target_mod != ns
                {
                    return Err((
                        FfiResult::PERMISSION_DENIED,
                        format!(
                            "mod '{ns}' cannot access assets in foreign namespace '{target_mod}'"
                        ),
                    ));
                }
                return Ok(format!("mod://{target_mod}/{subpath}"));
            }
            if has_unsafe_segments(rest) {
                return Err((
                    FfiResult::PERMISSION_DENIED,
                    format!("traversal rejected in asset path '{raw_path}'"),
                ));
            }
            return Ok(raw_path.to_string());
        }

        if raw_path.starts_with('/') || has_unsafe_segments(raw_path) {
            return Err((
                FfiResult::PERMISSION_DENIED,
                format!("traversal rejected in asset path '{raw_path}'"),
            ));
        }

        if let Some(ns) = owner_namespace
            && self
                .inner
                .dirs
                .read()
                .is_ok_and(|dirs| dirs.contains_key(ns))
        {
            return Ok(format!("mod://{ns}/{raw_path}"));
        }

        Ok(raw_path.to_string())
    }
}

fn has_unsafe_segments(subpath: &str) -> bool {
    subpath
        .split('/')
        .any(|seg| seg.is_empty() || seg == "." || seg == "..")
}

fn untyped_id_to_ffi(asset_type: BamComponent, id: UntypedAssetId) -> FfiAssetHandle {
    match id {
        UntypedAssetId::Index { index, .. } => {
            FfiAssetHandle::from_index_bits(asset_type, index.to_bits())
        }
        UntypedAssetId::Uuid { uuid, .. } => {
            FfiAssetHandle::from_uuid(asset_type, ComponentUuid::from_u128(uuid.as_u128()))
        }
    }
}

impl HostAssetBackend for BevyHostAssetBackend {
    fn load_asset(
        &self,
        _snapshot: &RegistrySnapshot,
        owner_namespace: Option<&str>,
        asset_type: BamComponent,
        path: &str,
    ) -> Result<FfiAssetHandle, (FfiResult, String)> {
        let server = self.get_asset_server()?;
        let normalized = self.normalize_asset_path(owner_namespace, path)?;

        let (effective_type, untyped_handle): (BamComponent, UntypedHandle) =
            if !asset_type.is_valid() {
                let untyped_bam = self
                    .inner
                    .untyped_handle_type
                    .read()
                    .ok()
                    .and_then(|g| *g)
                    .ok_or_else(|| {
                        (
                            FfiResult::INVALID_STATE,
                            "LoadedUntypedAsset is not registered in ModAssetRegistry".to_string(),
                        )
                    })?;
                let h = server.load_builder().load_untyped(normalized);
                (untyped_bam, h.untyped())
            } else {
                let type_id = self
                    .inner
                    .bam_to_type_id
                    .read()
                    .ok()
                    .and_then(|map| map.get(&asset_type).copied())
                    .ok_or_else(|| {
                        (
                            FfiResult::INVALID_COMPONENT,
                            format!("unregistered asset_type handle {asset_type:?}"),
                        )
                    })?;
                let h = server.load_builder().load_erased(type_id, normalized);
                (asset_type, h)
            };

        let ffi_handle = untyped_id_to_ffi(effective_type, untyped_handle.id());
        let owner_key = owner_namespace.map(str::to_string);
        if let Ok(mut retained) = self.inner.retained.lock() {
            let bucket = retained.entry(owner_key).or_default();
            bucket
                .entry(ffi_handle)
                .and_modify(|(_, count)| *count += 1)
                .or_insert((untyped_handle, 1));
        }
        Ok(ffi_handle)
    }

    fn get_load_state(
        &self,
        _snapshot: &RegistrySnapshot,
        handle: &FfiAssetHandle,
    ) -> Result<FfiAssetLoadState, (FfiResult, String)> {
        if handle.is_none() {
            return Ok(FfiAssetLoadState::NOT_LOADED);
        }
        let server = self.get_asset_server()?;
        let type_id = self
            .inner
            .bam_to_type_id
            .read()
            .ok()
            .and_then(|map| map.get(&handle.asset_type).copied())
            .ok_or_else(|| {
                (
                    FfiResult::INVALID_COMPONENT,
                    format!("unknown asset_type handle {:?}", handle.asset_type),
                )
            })?;

        let untyped_id = match handle.kind {
            FfiAssetHandle::KIND_INDEX => {
                let bits = handle.to_index_bits().ok_or_else(|| {
                    (
                        FfiResult::INVALID_ARGUMENT,
                        "corrupt index handle".to_string(),
                    )
                })?;
                UntypedAssetId::Index {
                    type_id,
                    index: AssetIndex::from_bits(bits),
                }
            }
            FfiAssetHandle::KIND_UUID => UntypedAssetId::Uuid {
                type_id,
                uuid: Uuid::from_u128(handle.uuid.to_u128()),
            },
            _ => {
                return Err((
                    FfiResult::INVALID_ARGUMENT,
                    "invalid FfiAssetHandle kind".to_string(),
                ));
            }
        };

        let state = match server.get_load_state(untyped_id) {
            None | Some(LoadState::NotLoaded) => FfiAssetLoadState::NOT_LOADED,
            Some(LoadState::Loading) => FfiAssetLoadState::LOADING,
            Some(LoadState::Loaded) => FfiAssetLoadState::LOADED,
            Some(LoadState::Failed(_)) => FfiAssetLoadState::FAILED,
        };
        Ok(state)
    }

    fn release_asset(
        &self,
        _snapshot: &RegistrySnapshot,
        owner_namespace: Option<&str>,
        handle: &FfiAssetHandle,
    ) -> Result<(), (FfiResult, String)> {
        if handle.is_none() {
            return Ok(());
        }
        let owner_key = owner_namespace.map(str::to_string);
        let mut retained = self.inner.retained.lock().map_err(|_| {
            (
                FfiResult::INTERNAL_ERROR,
                "retained asset table mutex poisoned".to_string(),
            )
        })?;
        if let Some(bucket) = retained.get_mut(&owner_key)
            && let Some((_, count)) = bucket.get_mut(handle)
        {
            if *count > 1 {
                *count -= 1;
            } else {
                bucket.remove(handle);
            }
            return Ok(());
        }
        // Reject cross-owner release attempts when the handle belongs to another mod namespace.
        for (other_owner, bucket) in retained.iter() {
            if other_owner != &owner_key && bucket.contains_key(handle) {
                return Err((
                    FfiResult::PERMISSION_DENIED,
                    "cannot release asset handle owned by another mod namespace".to_string(),
                ));
            }
        }
        Err((
            FfiResult::NOT_FOUND,
            "asset handle is not retained by caller".to_string(),
        ))
    }

    fn release_all_for_owner(&self, owner_namespace: &str) {
        if let Ok(mut retained) = self.inner.retained.lock() {
            retained.remove(&Some(owner_namespace.to_string()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy_abi_mod_host::AbiModPlugin;
    use bevy_app::TaskPoolPlugin;
    use bevy_asset::AssetPlugin;
    use core::sync::atomic::{AtomicUsize, Ordering};
    use std::fs;
    use std::time::Duration;

    static TEMP_DIR_SEQ: AtomicUsize = AtomicUsize::new(0);

    struct TempTestDir {
        path: PathBuf,
    }

    impl TempTestDir {
        fn new(prefix: &str) -> Self {
            let seq = TEMP_DIR_SEQ.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "bam_asset_test_{prefix}_{}_{}",
                std::process::id(),
                seq
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            Self { path }
        }
    }

    impl Drop for TempTestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn mod_asset_reader_sandboxing_and_path_normalization() {
        let alpha_dir = TempTestDir::new("reader_alpha");
        let beta_dir = TempTestDir::new("reader_beta");
        let outside_dir = TempTestDir::new("reader_outside");

        fs::create_dir_all(alpha_dir.path.join("sub")).unwrap();
        fs::write(alpha_dir.path.join("sub/ok.txt"), b"ok").unwrap();
        fs::write(beta_dir.path.join("secret.txt"), b"secret").unwrap();
        fs::write(outside_dir.path.join("escape.txt"), b"escape").unwrap();

        #[cfg(unix)]
        std::os::unix::fs::symlink(
            outside_dir.path.join("escape.txt"),
            alpha_dir.path.join("sym_escape.txt"),
        )
        .unwrap();

        let reg = ModAssetRegistry::new();
        reg.mount_mod_asset_dir("alpha_mod", &alpha_dir.path)
            .unwrap();
        reg.mount_mod_asset_dir("beta_mod", &beta_dir.path).unwrap();

        // Reserved "overlay" namespace cannot be hijacked by a mod mount.
        assert!(
            reg.mount_mod_asset_dir(OVERLAY_NAMESPACE, &alpha_dir.path)
                .is_err()
        );

        let reader = ModAssetReader::new(Arc::clone(&reg.inner));
        assert!(
            reader
                .resolve_existing_path(Path::new("alpha_mod/sub/ok.txt"), false)
                .is_ok()
        );
        assert!(
            reader
                .resolve_existing_path(Path::new("alpha_mod/../beta_mod/secret.txt"), false)
                .is_err()
        );
        assert!(
            reader
                .resolve_existing_path(Path::new("/etc/passwd"), false)
                .is_err()
        );
        // Overlay is disabled by default.
        assert!(
            reader
                .resolve_existing_path(Path::new("overlay/sub/ok.txt"), false)
                .is_err()
        );
        #[cfg(unix)]
        assert!(
            reader
                .resolve_existing_path(Path::new("alpha_mod/sym_escape.txt"), false)
                .is_err()
        );

        let backend = BevyHostAssetBackend {
            inner: Arc::clone(&reg.inner),
        };
        assert_eq!(
            backend
                .normalize_asset_path(Some("alpha_mod"), "sub/ok.txt")
                .unwrap(),
            "mod://alpha_mod/sub/ok.txt"
        );
        assert_eq!(
            backend
                .normalize_asset_path(Some("alpha_mod"), "mod://alpha_mod/sub/ok.txt")
                .unwrap(),
            "mod://alpha_mod/sub/ok.txt"
        );
        assert_eq!(
            backend
                .normalize_asset_path(Some("alpha_mod"), "mod://beta_mod/secret.txt")
                .unwrap_err()
                .0,
            FfiResult::PERMISSION_DENIED
        );
        assert_eq!(
            backend
                .normalize_asset_path(Some("alpha_mod"), "mod://overlay/sub/ok.txt")
                .unwrap_err()
                .0,
            FfiResult::PERMISSION_DENIED
        );
        assert_eq!(
            backend
                .normalize_asset_path(Some("alpha_mod"), "../beta_mod/secret.txt")
                .unwrap_err()
                .0,
            FfiResult::PERMISSION_DENIED
        );

        // Opting into `LoadOrderPriority` enables `mod://overlay/...` while keeping cross-mod & traversal blocked.
        reg.set_overlay_policy(AssetOverlayPolicy::LoadOrderPriority);
        assert_eq!(
            backend
                .normalize_asset_path(Some("alpha_mod"), "mod://overlay/sub/ok.txt")
                .unwrap(),
            "mod://overlay/sub/ok.txt"
        );
        assert_eq!(
            backend
                .normalize_asset_path(Some("alpha_mod"), "mod://overlay/../beta_mod/secret.txt")
                .unwrap_err()
                .0,
            FfiResult::PERMISSION_DENIED
        );
        assert_eq!(
            backend
                .normalize_asset_path(Some("alpha_mod"), "mod://beta_mod/secret.txt")
                .unwrap_err()
                .0,
            FfiResult::PERMISSION_DENIED
        );
    }

    #[test]
    fn real_async_mod_asset_load_isolation_traversal_and_unload_release() {
        use bevy_asset::io::Reader;
        use bevy_asset::{AssetLoader, LoadContext};
        use bevy_reflect::TypePath;

        #[derive(Asset, TypePath, Debug, Clone)]
        struct TextAsset(String);

        #[derive(Default, TypePath)]
        struct TextAssetLoader;

        impl AssetLoader for TextAssetLoader {
            type Asset = TextAsset;
            type Settings = ();
            type Error = std::io::Error;

            async fn load(
                &self,
                reader: &mut dyn Reader,
                _settings: &(),
                _load_context: &mut LoadContext<'_>,
            ) -> Result<Self::Asset, Self::Error> {
                let mut bytes = Vec::new();
                reader.read_to_end(&mut bytes).await?;
                Ok(TextAsset(String::from_utf8_lossy(&bytes).into_owned()))
            }

            fn extensions(&self) -> &[&str] {
                &["txt"]
            }
        }

        let default_dir = TempTestDir::new("default");
        let alpha_dir = TempTestDir::new("alpha");
        let beta_dir = TempTestDir::new("beta");
        let outside_dir = TempTestDir::new("outside");

        fs::create_dir_all(alpha_dir.path.join("data")).unwrap();
        fs::write(alpha_dir.path.join("data/hello.txt"), b"hello from alpha").unwrap();
        fs::write(beta_dir.path.join("secret.txt"), b"beta secret").unwrap();
        fs::write(outside_dir.path.join("escape.txt"), b"outside").unwrap();

        #[cfg(unix)]
        std::os::unix::fs::symlink(
            outside_dir.path.join("escape.txt"),
            alpha_dir.path.join("link_escape.txt"),
        )
        .unwrap();

        let mut app = App::new();
        app.add_plugins((
            TaskPoolPlugin::default(),
            AbiModPlugin,
            ModAssetPlugin,
            AssetPlugin {
                file_path: default_dir.path.to_string_lossy().into_owned(),
                meta_check: bevy_asset::AssetMetaCheck::Never,
                ..Default::default()
            },
        ));
        app.init_asset::<TextAsset>();
        app.register_asset_loader(TextAssetLoader);
        app.finish();

        app.mount_mod_asset_dir("alpha_mod", &alpha_dir.path)
            .unwrap();
        app.mount_mod_asset_dir("beta_mod", &beta_dir.path).unwrap();
        let text_bam = app.register_mod_asset::<TextAsset>().unwrap();

        let snap = app.world().resource::<ModRegistry>().snapshot_arc();
        let backend = snap.asset_backend().unwrap().clone();

        // 1. Bare relative path normalizes to `mod://alpha_mod/data/hello.txt` and loads async.
        let h_alpha = backend
            .load_asset(&snap, Some("alpha_mod"), text_bam, "data/hello.txt")
            .unwrap();
        assert!(h_alpha.is_some());
        assert_eq!(
            app.world()
                .resource::<ModAssetRegistry>()
                .retained_count_for_owner(Some("alpha_mod")),
            1
        );

        let mut state = FfiAssetLoadState::NOT_LOADED;
        for _ in 0..40 {
            app.update();
            state = backend.get_load_state(&snap, &h_alpha).unwrap();
            if state == FfiAssetLoadState::LOADED {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(state, FfiAssetLoadState::LOADED);

        let bits = h_alpha.to_index_bits().unwrap();
        let typed_id = bevy_asset::AssetId::<TextAsset>::from(AssetIndex::from_bits(bits));
        let loaded_asset = app
            .world()
            .resource::<Assets<TextAsset>>()
            .get(typed_id)
            .unwrap();
        assert_eq!(loaded_asset.0, "hello from alpha");

        // 2. Untyped load (`BamComponent::INVALID`) succeeds and transitions to `LOADED`.
        let h_untyped = backend
            .load_asset(
                &snap,
                Some("alpha_mod"),
                BamComponent::INVALID,
                "mod://alpha_mod/data/hello.txt",
            )
            .unwrap();
        assert!(h_untyped.is_some());
        let mut untyped_state = FfiAssetLoadState::NOT_LOADED;
        for _ in 0..40 {
            app.update();
            untyped_state = backend.get_load_state(&snap, &h_untyped).unwrap();
            if untyped_state == FfiAssetLoadState::LOADED {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(untyped_state, FfiAssetLoadState::LOADED);

        // 3. Cross-mod access (`alpha_mod` -> `mod://beta_mod/...`) and `..` traversal are rejected.
        assert_eq!(
            backend
                .load_asset(
                    &snap,
                    Some("alpha_mod"),
                    text_bam,
                    "mod://beta_mod/secret.txt"
                )
                .unwrap_err()
                .0,
            FfiResult::PERMISSION_DENIED
        );
        assert_eq!(
            backend
                .load_asset(
                    &snap,
                    Some("alpha_mod"),
                    text_bam,
                    "mod://alpha_mod/../beta_mod/secret.txt"
                )
                .unwrap_err()
                .0,
            FfiResult::PERMISSION_DENIED
        );
        assert_eq!(
            backend
                .load_asset(&snap, Some("alpha_mod"), text_bam, "../outside.txt")
                .unwrap_err()
                .0,
            FfiResult::PERMISSION_DENIED
        );

        // 4. Symlink escape inside `alpha_dir` fails inside `ModAssetReader` with `FAILED`.
        #[cfg(unix)]
        {
            let h_sym = backend
                .load_asset(&snap, Some("alpha_mod"), text_bam, "link_escape.txt")
                .unwrap();
            let mut sym_state = FfiAssetLoadState::NOT_LOADED;
            for _ in 0..40 {
                app.update();
                sym_state = backend.get_load_state(&snap, &h_sym).unwrap();
                if sym_state == FfiAssetLoadState::FAILED {
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            assert_eq!(sym_state, FfiAssetLoadState::FAILED);
        }

        // 5. Foreign mod cannot release `alpha_mod`'s handle; `alpha_mod` release + bulk unload work.
        assert_eq!(
            backend
                .release_asset(&snap, Some("beta_mod"), &h_alpha)
                .unwrap_err()
                .0,
            FfiResult::PERMISSION_DENIED
        );
        assert!(
            backend
                .release_asset(&snap, Some("alpha_mod"), &h_alpha)
                .is_ok()
        );
        backend.release_all_for_owner("alpha_mod");
        assert_eq!(
            app.world()
                .resource::<ModAssetRegistry>()
                .retained_count_for_owner(Some("alpha_mod")),
            0
        );
    }
}
