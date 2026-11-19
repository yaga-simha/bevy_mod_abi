//! Debounced filesystem hot-reload watcher integration (`bevy_abi_mod_watcher`).
//!
//! Monitors a mods directory recursively for native shared libraries (`.so`, `.dylib`, `.dll`)
//! and declarative `mod.toml` manifests, validates SemVer DAG constraints, performs incremental
//! topological directory loading, and triggers cascading reverse-topological `reload_mod` /
//! `unload_mod` operations.

mod imp {
    use bevy_abi_mod_host::mod_dag::{
        ModDependencyGraph, ModTomlManifest, discover_mod_manifests, resolve_dependency_dag,
    };
    use bevy_abi_mod_host::world_ext::ModWorldExt;
    use bevy_abi_mod_host_sys::{LoadOptions, ModHost, ModId, canonical_mod_name_from_path};
    use bevy_app::{App, Plugin, PreUpdate};
    use bevy_ecs::prelude::Resource;
    use bevy_ecs::world::World;
    use notify_debouncer_full::notify::event::{CreateKind, ModifyKind, RemoveKind};
    use notify_debouncer_full::notify::{EventKind, RecommendedWatcher, RecursiveMode};
    use notify_debouncer_full::{DebounceEventResult, Debouncer, RecommendedCache, new_debouncer};
    use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
    use std::path::{Path, PathBuf};
    use std::sync::{Mutex, mpsc};
    use std::time::{Duration, Instant};

    /// Action taken by [`ModHotReloadWatcher`] for a changed or deleted mod artifact / manifest.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub enum ModHotReloadAction {
        /// A newly discovered mod library was loaded into the `World`.
        Loaded,
        /// An already-loaded mod library (and any dependents) was hot-reloaded in topological order.
        Reloaded,
        /// A deleted mod library (and any dependents) was cleanly unloaded.
        Unloaded,
        /// Validating, loading, or reloading the changed mod failed with a diagnostic message.
        Failed(String),
    }

    /// Outcome record emitted by [`ModHotReloadWatcher::process_pending_reloads`] and stored in
    /// [`ModHotReloadHistory`].
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct ModHotReloadOutcome {
        /// Path of the mod shared library or `mod.toml` manifest associated with the operation.
        pub path: PathBuf,
        /// Resulting [`ModId`] on `Loaded` / `Reloaded` (or unloaded [`ModId`] on `Unloaded`), or `None` on failure.
        pub mod_id: Option<ModId>,
        /// Action performed.
        pub action: ModHotReloadAction,
    }

    /// World resource recording outcomes emitted by [`ModHotReloadPlugin`]'s polling system.
    #[derive(Resource, Default, Debug, Clone)]
    pub struct ModHotReloadHistory {
        /// Chronological list of hot-reload outcomes observed in this `World`.
        pub outcomes: Vec<ModHotReloadOutcome>,
    }

    /// Debounced filesystem watcher (`notify-debouncer-full`) monitoring a directory tree for
    /// native mod shared libraries (`.so`, `.dylib`, `.dll`) and `mod.toml` manifests.
    #[derive(Resource)]
    pub struct ModHotReloadWatcher {
        watch_dir: PathBuf,
        load_options: LoadOptions,
        debounce: Duration,
        unload_on_delete: bool,
        rx: Mutex<mpsc::Receiver<DebounceEventResult>>,
        pending_deletes: Mutex<HashMap<PathBuf, Instant>>,
        _debouncer: Mutex<Debouncer<RecommendedWatcher, RecommendedCache>>,
    }

    impl ModHotReloadWatcher {
        /// Starts watching `dir` recursively with a 200ms debounce window and
        /// `LoadOptions::default().with_shadow_copy(true)`.
        pub fn watch(dir: impl AsRef<Path>) -> Result<Self, notify_debouncer_full::notify::Error> {
            Self::watch_with_options(
                dir,
                Duration::from_millis(200),
                true,
                LoadOptions::default().with_shadow_copy(true),
            )
        }

        /// Starts watching `dir` with custom debounce duration, recursion mode, and [`LoadOptions`].
        pub fn watch_with_options(
            dir: impl AsRef<Path>,
            debounce: Duration,
            recursive: bool,
            load_options: LoadOptions,
        ) -> Result<Self, notify_debouncer_full::notify::Error> {
            let watch_dir = dir.as_ref().to_path_buf();
            let (tx, rx) = mpsc::channel::<DebounceEventResult>();

            let mut debouncer = new_debouncer(debounce, None, move |res: DebounceEventResult| {
                let _ = tx.send(res);
            })?;

            let mode = if recursive {
                RecursiveMode::Recursive
            } else {
                RecursiveMode::NonRecursive
            };
            debouncer.watch(&watch_dir, mode)?;

            Ok(Self {
                watch_dir,
                load_options: load_options.with_shadow_copy(true),
                debounce,
                unload_on_delete: true,
                rx: Mutex::new(rx),
                pending_deletes: Mutex::new(HashMap::new()),
                _debouncer: Mutex::new(debouncer),
            })
        }

        /// Enables or disables automatic unloading when a watched mod binary or `mod.toml` is
        /// permanently deleted on disk (default `true`, guarded against atomic compiler renames).
        #[must_use]
        pub const fn with_unload_on_delete(mut self, enabled: bool) -> Self {
            self.unload_on_delete = enabled;
            self
        }

        /// Returns the root directory currently being watched.
        #[inline]
        pub fn watch_dir(&self) -> &Path {
            &self.watch_dir
        }

        /// Returns the [`LoadOptions`] applied when loading or reloading changed mods.
        #[inline]
        pub fn load_options(&self) -> &LoadOptions {
            &self.load_options
        }

        /// Drains all debounced filesystem events and returns deduplicated mod library or
        /// `mod.toml` paths that were created or modified.
        pub fn poll_changed_paths(&self) -> Vec<PathBuf> {
            let (changed, _deleted) = self.poll_events();
            changed
        }

        /// Drains debounced events, separating confirmed creates/modifies from deletions that have
        /// remained absent past the atomic-rename grace window (`self.debounce`).
        pub fn poll_events(&self) -> (Vec<PathBuf>, Vec<PathBuf>) {
            let mut changed = Vec::new();
            let mut seen_changed = HashSet::new();
            let mut pending = self
                .pending_deletes
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());

            if let Ok(guard) = self.rx.lock() {
                while let Ok(batch) = guard.try_recv() {
                    let Ok(events) = batch else {
                        continue;
                    };
                    for ev in events {
                        let is_remove = matches!(
                            ev.kind,
                            EventKind::Remove(RemoveKind::File | RemoveKind::Any)
                        );
                        let is_change = is_relevant_event_kind(ev.kind);
                        if !is_remove && !is_change {
                            continue;
                        }
                        for p in &ev.paths {
                            if !is_watchable_mod_path(p) {
                                continue;
                            }
                            if is_remove && !p.is_file() {
                                pending.entry(p.clone()).or_insert_with(Instant::now);
                            } else if p.is_file() {
                                pending.remove(p);
                                if seen_changed.insert(p.clone()) {
                                    changed.push(p.clone());
                                }
                            }
                        }
                    }
                }
            }

            let now = Instant::now();
            let mut deleted = Vec::new();
            let mut resolved_keys = Vec::new();
            for (path, first_seen) in pending.iter() {
                if path.is_file() {
                    resolved_keys.push((path.clone(), true));
                } else if now.duration_since(*first_seen) >= self.debounce {
                    resolved_keys.push((path.clone(), false));
                }
            }
            for (path, recreated) in resolved_keys {
                pending.remove(&path);
                if recreated {
                    if seen_changed.insert(path.clone()) {
                        changed.push(path);
                    }
                } else if self.unload_on_delete {
                    deleted.push(path);
                }
            }

            (changed, deleted)
        }

        /// Performs an incremental topological load of all currently unloaded `mod.toml` mods in
        /// `self.watch_dir()`, leaving already-loaded mods untouched.
        pub fn sync_dir(&self, world: &mut World) -> Vec<ModHotReloadOutcome> {
            load_unloaded_dag_mods(world, &self.watch_dir, &self.load_options)
        }

        /// Drains pending debounced changes/deletions and loads, reloads (cascading via
        /// [`ModDependencyGraph`]), or unloads affected mods in `world`.
        pub fn process_pending_reloads(&self, world: &mut World) -> Vec<ModHotReloadOutcome> {
            let (changed, deleted) = self.poll_events();
            apply_hot_reload_events(
                world,
                &self.watch_dir,
                &changed,
                &deleted,
                &self.load_options,
            )
        }
    }

    /// Bevy [`Plugin`] installing [`ModHotReloadWatcher`] and polling it each [`PreUpdate`] tick.
    #[derive(Clone, Debug)]
    pub struct ModHotReloadPlugin {
        watch_dir: PathBuf,
        debounce: Duration,
        recursive: bool,
        unload_on_delete: bool,
        sync_on_start: bool,
        load_options: LoadOptions,
    }

    impl ModHotReloadPlugin {
        /// Creates a [`ModHotReloadPlugin`] watching `dir` recursively with a 200ms debounce window.
        pub fn new(dir: impl AsRef<Path>) -> Self {
            Self {
                watch_dir: dir.as_ref().to_path_buf(),
                debounce: Duration::from_millis(200),
                recursive: true,
                unload_on_delete: true,
                sync_on_start: false,
                load_options: LoadOptions::default().with_shadow_copy(true),
            }
        }

        /// Overrides the filesystem debounce duration.
        #[must_use]
        pub const fn with_debounce(mut self, debounce: Duration) -> Self {
            self.debounce = debounce;
            self
        }

        /// Enables or disables recursive directory watching.
        #[must_use]
        pub const fn with_recursive(mut self, recursive: bool) -> Self {
            self.recursive = recursive;
            self
        }

        /// Enables or disables unloading mods when their binary or `mod.toml` is deleted on disk.
        #[must_use]
        pub const fn with_unload_on_delete(mut self, enabled: bool) -> Self {
            self.unload_on_delete = enabled;
            self
        }

        /// When `true`, performs an initial topological `sync_dir` load during plugin build.
        #[must_use]
        pub const fn with_sync_on_start(mut self, enabled: bool) -> Self {
            self.sync_on_start = enabled;
            self
        }

        /// Overrides the [`LoadOptions`] used when loading or reloading mods.
        #[must_use]
        pub fn with_load_options(mut self, load_options: LoadOptions) -> Self {
            self.load_options = load_options.with_shadow_copy(true);
            self
        }
    }

    impl Plugin for ModHotReloadPlugin {
        fn build(&self, app: &mut App) {
            if !app.world().contains_resource::<ModHotReloadHistory>() {
                app.init_resource::<ModHotReloadHistory>();
            }
            match ModHotReloadWatcher::watch_with_options(
                &self.watch_dir,
                self.debounce,
                self.recursive,
                self.load_options.clone(),
            ) {
                Ok(watcher) => {
                    let watcher = watcher.with_unload_on_delete(self.unload_on_delete);
                    if self.sync_on_start {
                        let initial = watcher.sync_dir(app.world_mut());
                        if let Some(mut hist) =
                            app.world_mut().get_resource_mut::<ModHotReloadHistory>()
                        {
                            hist.outcomes.extend(initial);
                        }
                    }
                    app.insert_resource(watcher);
                    app.add_systems(PreUpdate, poll_hot_reload_watcher_system);
                }
                Err(err) => {
                    tracing::error!(
                        dir = %self.watch_dir.display(),
                        error = %err,
                        "failed to initialize ModHotReloadWatcher"
                    );
                }
            }
        }
    }

    fn poll_hot_reload_watcher_system(world: &mut World) {
        let Some((changed, deleted, watch_dir, options)) =
            world.get_resource::<ModHotReloadWatcher>().map(|w| {
                let (c, d) = w.poll_events();
                (c, d, w.watch_dir().to_path_buf(), w.load_options().clone())
            })
        else {
            return;
        };
        if changed.is_empty() && deleted.is_empty() {
            return;
        }
        let outcomes = apply_hot_reload_events(world, &watch_dir, &changed, &deleted, &options);
        if !outcomes.is_empty() {
            if !world.contains_resource::<ModHotReloadHistory>() {
                world.init_resource::<ModHotReloadHistory>();
            }
            if let Some(mut history) = world.get_resource_mut::<ModHotReloadHistory>() {
                history.outcomes.extend(outcomes);
            }
        }
    }

    fn is_relevant_event_kind(kind: EventKind) -> bool {
        matches!(
            kind,
            EventKind::Create(CreateKind::File | CreateKind::Any)
                | EventKind::Modify(
                    ModifyKind::Data(_)
                        | ModifyKind::Metadata(_)
                        | ModifyKind::Name(_)
                        | ModifyKind::Any
                )
        )
    }

    fn is_dylib_path(path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .is_some_and(|ext| {
                ext.eq_ignore_ascii_case("so")
                    || ext.eq_ignore_ascii_case("dylib")
                    || ext.eq_ignore_ascii_case("dll")
            })
    }

    fn is_watchable_mod_path(path: &Path) -> bool {
        is_dylib_path(path)
            || path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n == "mod.toml")
    }

    fn is_manifest_path(path: &Path) -> bool {
        path.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n == "mod.toml")
    }

    fn same_path(a: &Path, b: &Path) -> bool {
        if a == b {
            return true;
        }
        match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
            (Ok(ca), Ok(cb)) => ca == cb,
            _ => false,
        }
    }

    fn find_loaded_mod_for_dylib(world: &World, lib_path: &Path) -> Option<(ModId, String)> {
        let host = world.get_resource::<ModHost>()?;
        let target_stem = canonical_mod_name_from_path(lib_path);

        for m in host.loaded_mods() {
            if !m.is_active() {
                continue;
            }
            if same_path(m.source_path(), lib_path) {
                return Some((m.id(), m.name().to_string()));
            }
            if canonical_mod_name_from_path(m.source_path()) == target_stem
                && m.source_path().parent() == lib_path.parent()
            {
                return Some((m.id(), m.name().to_string()));
            }
        }
        None
    }

    fn find_loaded_mod_for_manifest_dir(
        world: &World,
        manifest_path: &Path,
        parsed_name: Option<&str>,
    ) -> Option<(ModId, String, PathBuf)> {
        let host = world.get_resource::<ModHost>()?;
        let dir = manifest_path.parent()?;
        for m in host.loaded_mods() {
            if !m.is_active() {
                continue;
            }
            if let Some(name) = parsed_name
                && m.name() == name
            {
                return Some((m.id(), m.name().to_string(), m.source_path().to_path_buf()));
            }
            if let Some(src_dir) = m.source_path().parent()
                && same_path(src_dir, dir)
            {
                return Some((m.id(), m.name().to_string(), m.source_path().to_path_buf()));
            }
        }
        None
    }

    fn validate_dir_dag_for_reload(
        world: &World,
        watch_dir: &Path,
        target_mod_name: &str,
        target_manifest_path: Option<&Path>,
    ) -> Result<Option<ModTomlManifest>, String> {
        let (manifests, parse_errors) = match discover_mod_manifests(watch_dir) {
            Ok(res) => res,
            Err(err) => return Err(err.to_string()),
        };

        if let Some(t_path) = target_manifest_path {
            for err in &parse_errors {
                let err_str = err.to_string();
                if err_str.contains(&t_path.display().to_string()) {
                    return Err(err_str);
                }
            }
        }
        if !parse_errors.is_empty() && manifests.is_empty() {
            return Err(parse_errors[0].to_string());
        }

        let target_manifest = manifests
            .iter()
            .find(|m| m.name == target_mod_name)
            .cloned();

        // Include any currently loaded mods outside `watch_dir` in `already_loaded` so external
        // SemVer dependencies resolve properly.
        let manifest_names: HashSet<&str> = manifests.iter().map(|m| m.name.as_str()).collect();
        let mut external_loaded = BTreeMap::new();
        if let Some(host) = world.get_resource::<ModHost>() {
            for m in host.loaded_mods() {
                if m.is_active()
                    && !manifest_names.contains(m.name())
                    && let Ok(ver) = semver::Version::parse(m.version())
                {
                    external_loaded.insert(m.name().to_string(), ver);
                }
            }
        }

        let resolution = resolve_dependency_dag(manifests, &external_loaded);
        if resolution
            .quarantined_cycles
            .iter()
            .any(|name| name == target_mod_name)
        {
            return Err(format!(
                "mod '{target_mod_name}' participates in a dependency cycle: {}",
                resolution.quarantined_cycles.join(", ")
            ));
        }

        for diag in &resolution.diagnostics {
            let msg = diag.to_string();
            if msg.contains(&format!("'{target_mod_name}'")) {
                return Err(msg);
            }
        }

        if target_manifest.is_some()
            && !resolution
                .load_order
                .iter()
                .any(|m| m.name == target_mod_name)
        {
            return Err(format!(
                "mod '{target_mod_name}' failed DAG dependency validation"
            ));
        }

        Ok(target_manifest)
    }

    fn load_unloaded_dag_mods(
        world: &mut World,
        watch_dir: &Path,
        load_options: &LoadOptions,
    ) -> Vec<ModHotReloadOutcome> {
        let mut outcomes = Vec::new();
        let (manifests, parse_errors) = match discover_mod_manifests(watch_dir) {
            Ok(res) => res,
            Err(err) => {
                outcomes.push(ModHotReloadOutcome {
                    path: watch_dir.to_path_buf(),
                    mod_id: None,
                    action: ModHotReloadAction::Failed(err.to_string()),
                });
                return outcomes;
            }
        };

        for err in parse_errors {
            outcomes.push(ModHotReloadOutcome {
                path: watch_dir.to_path_buf(),
                mod_id: None,
                action: ModHotReloadAction::Failed(err.to_string()),
            });
        }

        let mut loaded_names: BTreeSet<String> = BTreeSet::new();
        let mut already_loaded: BTreeMap<String, semver::Version> = BTreeMap::new();
        if let Some(host) = world.get_resource::<ModHost>() {
            for loaded in host.loaded_mods() {
                if !loaded.is_active() {
                    continue;
                }
                loaded_names.insert(loaded.name().to_string());
                if let Ok(ver) = semver::Version::parse(loaded.version()) {
                    already_loaded.insert(loaded.name().to_string(), ver);
                }
            }
        }

        let unloaded_manifests: Vec<ModTomlManifest> = manifests
            .into_iter()
            .filter(|m| !loaded_names.contains(&m.name))
            .collect();

        if unloaded_manifests.is_empty() {
            return outcomes;
        }

        let resolution = resolve_dependency_dag(unloaded_manifests, &already_loaded);
        for diag in resolution.diagnostics {
            outcomes.push(ModHotReloadOutcome {
                path: watch_dir.to_path_buf(),
                mod_id: None,
                action: ModHotReloadAction::Failed(diag.to_string()),
            });
        }

        let opts = load_options.clone().with_shadow_copy(true);
        let mut failed_mods: BTreeSet<String> = BTreeSet::new();
        for manifest in resolution.load_order {
            if let Some(dep) = manifest
                .dependencies
                .keys()
                .find(|d| failed_mods.contains(*d) || !loaded_names.contains(*d))
            {
                outcomes.push(ModHotReloadOutcome {
                    path: manifest.manifest_path.clone(),
                    mod_id: None,
                    action: ModHotReloadAction::Failed(format!(
                        "mod '{}' skipped because dependency '{}' failed or is missing",
                        manifest.name, dep
                    )),
                });
                failed_mods.insert(manifest.name);
                continue;
            }

            let lib_path = match manifest.resolve_library_path() {
                Ok(p) => p,
                Err(err) => {
                    outcomes.push(ModHotReloadOutcome {
                        path: manifest.manifest_path.clone(),
                        mod_id: None,
                        action: ModHotReloadAction::Failed(err.to_string()),
                    });
                    failed_mods.insert(manifest.name);
                    continue;
                }
            };

            match world.load_mod_with(&lib_path, opts.clone()) {
                Ok(id) => {
                    loaded_names.insert(manifest.name);
                    outcomes.push(ModHotReloadOutcome {
                        path: lib_path,
                        mod_id: Some(id),
                        action: ModHotReloadAction::Loaded,
                    });
                }
                Err(err) => {
                    outcomes.push(ModHotReloadOutcome {
                        path: lib_path,
                        mod_id: None,
                        action: ModHotReloadAction::Failed(err.to_string()),
                    });
                    failed_mods.insert(manifest.name);
                }
            }
        }

        outcomes
    }

    fn collect_cascade_names(world: &World, root_id: ModId) -> Vec<String> {
        let active_ids: BTreeSet<ModId> = world
            .get_resource::<ModHost>()
            .map(|h| h.loaded_mods().iter().map(|m| m.id()).collect())
            .unwrap_or_default();
        let forward_ids = world
            .get_resource::<ModDependencyGraph>()
            .map(|dag| dag.cascading_order(root_id, &active_ids))
            .unwrap_or_else(|| vec![root_id]);
        let Some(host) = world.get_resource::<ModHost>() else {
            return Vec::new();
        };
        forward_ids
            .into_iter()
            .filter_map(|id| host.get_mod(id).map(|m| m.name().to_string()))
            .collect()
    }

    fn apply_hot_reload_events(
        world: &mut World,
        watch_dir: &Path,
        changed_paths: &[PathBuf],
        deleted_paths: &[PathBuf],
        load_options: &LoadOptions,
    ) -> Vec<ModHotReloadOutcome> {
        let mut outcomes = Vec::new();
        let opts = load_options.clone().with_shadow_copy(true);

        // 1. Handle confirmed deletions via cascading `unload_mod`.
        for del_path in deleted_paths {
            let target = if is_manifest_path(del_path) {
                find_loaded_mod_for_manifest_dir(world, del_path, None).map(|(id, _name, _lib)| id)
            } else if is_dylib_path(del_path) {
                find_loaded_mod_for_dylib(world, del_path).map(|(id, _)| id)
            } else {
                None
            };
            if let Some(mod_id) = target {
                match world.unload_mod(mod_id) {
                    Ok(()) => outcomes.push(ModHotReloadOutcome {
                        path: del_path.clone(),
                        mod_id: Some(mod_id),
                        action: ModHotReloadAction::Unloaded,
                    }),
                    Err(err) => outcomes.push(ModHotReloadOutcome {
                        path: del_path.clone(),
                        mod_id: None,
                        action: ModHotReloadAction::Failed(err.to_string()),
                    }),
                }
            }
        }

        // 2. Handle changes to already-loaded mods (re-validating `mod.toml` SemVer + DAG and
        //    cascading reload across dependents in reverse/forward topological order).
        let mut reloaded_in_batch: HashSet<String> = HashSet::new();
        let mut needs_dag_sync = false;
        let mut standalone_dylibs: Vec<PathBuf> = Vec::new();

        for path in changed_paths {
            if is_manifest_path(path) {
                needs_dag_sync = true;
                let parsed = match ModTomlManifest::from_file(path) {
                    Ok(m) => m,
                    Err(err) => {
                        outcomes.push(ModHotReloadOutcome {
                            path: path.clone(),
                            mod_id: None,
                            action: ModHotReloadAction::Failed(err.to_string()),
                        });
                        continue;
                    }
                };
                if let Some((mod_id, mod_name, lib_path)) =
                    find_loaded_mod_for_manifest_dir(world, path, Some(&parsed.name))
                {
                    if reloaded_in_batch.contains(&mod_name) {
                        continue;
                    }
                    match validate_dir_dag_for_reload(world, watch_dir, &mod_name, Some(path)) {
                        Ok(validated_opt) => {
                            let resolved_lib = validated_opt
                                .as_ref()
                                .and_then(|m| m.resolve_library_path().ok())
                                .unwrap_or(lib_path);
                            let cascade_names = collect_cascade_names(world, mod_id);
                            match world.reload_mod_from(mod_id, &resolved_lib, opts.clone()) {
                                Ok(new_id) => {
                                    reloaded_in_batch.extend(cascade_names);
                                    if let Some(m) = validated_opt {
                                        let deps: Vec<String> = m
                                            .dependencies
                                            .keys()
                                            .chain(m.optional_dependencies.keys())
                                            .cloned()
                                            .collect();
                                        if let Some(mut dag) =
                                            world.get_resource_mut::<ModDependencyGraph>()
                                        {
                                            dag.register_loaded_mod(
                                                new_id,
                                                m.name,
                                                resolved_lib.clone(),
                                                opts.clone(),
                                                deps,
                                            );
                                        }
                                    }
                                    outcomes.push(ModHotReloadOutcome {
                                        path: resolved_lib,
                                        mod_id: Some(new_id),
                                        action: ModHotReloadAction::Reloaded,
                                    });
                                }
                                Err(err) => outcomes.push(ModHotReloadOutcome {
                                    path: resolved_lib,
                                    mod_id: None,
                                    action: ModHotReloadAction::Failed(err.to_string()),
                                }),
                            }
                        }
                        Err(err) => outcomes.push(ModHotReloadOutcome {
                            path: path.clone(),
                            mod_id: None,
                            action: ModHotReloadAction::Failed(err),
                        }),
                    }
                }
            } else if is_dylib_path(path) && path.is_file() {
                if let Some((mod_id, mod_name)) = find_loaded_mod_for_dylib(world, path) {
                    if reloaded_in_batch.contains(&mod_name) {
                        continue;
                    }
                    let sidecar_manifest = path.parent().map(|d| d.join("mod.toml"));
                    let sidecar_opt = sidecar_manifest.as_deref().filter(|p| p.is_file());
                    if let Err(err) =
                        validate_dir_dag_for_reload(world, watch_dir, &mod_name, sidecar_opt)
                    {
                        outcomes.push(ModHotReloadOutcome {
                            path: path.clone(),
                            mod_id: None,
                            action: ModHotReloadAction::Failed(err),
                        });
                        continue;
                    }
                    let cascade_names = collect_cascade_names(world, mod_id);
                    match world.reload_mod_from(mod_id, path, opts.clone()) {
                        Ok(new_id) => {
                            reloaded_in_batch.extend(cascade_names);
                            outcomes.push(ModHotReloadOutcome {
                                path: path.clone(),
                                mod_id: Some(new_id),
                                action: ModHotReloadAction::Reloaded,
                            });
                        }
                        Err(err) => outcomes.push(ModHotReloadOutcome {
                            path: path.clone(),
                            mod_id: None,
                            action: ModHotReloadAction::Failed(err.to_string()),
                        }),
                    }
                } else {
                    let has_manifest = path
                        .parent()
                        .is_some_and(|dir| dir.join("mod.toml").is_file());
                    if has_manifest {
                        needs_dag_sync = true;
                    } else {
                        standalone_dylibs.push(path.clone());
                    }
                }
            }
        }

        // 3. Incrementally load any newly added `mod.toml` mods (or newly satisfied dependents) in
        //    Kahn topological order.
        if needs_dag_sync {
            outcomes.extend(load_unloaded_dag_mods(world, watch_dir, &opts));
        }

        // 4. Load any newly added standalone shared libraries without a `mod.toml`.
        let mut seen_standalone = HashSet::new();
        for lib_path in standalone_dylibs {
            if find_loaded_mod_for_dylib(world, &lib_path).is_some() {
                continue;
            }
            let key = std::fs::canonicalize(&lib_path).unwrap_or_else(|_| lib_path.clone());
            if !seen_standalone.insert(key) {
                continue;
            }
            match world.load_mod_with(&lib_path, opts.clone()) {
                Ok(id) => outcomes.push(ModHotReloadOutcome {
                    path: lib_path,
                    mod_id: Some(id),
                    action: ModHotReloadAction::Loaded,
                }),
                Err(err) => outcomes.push(ModHotReloadOutcome {
                    path: lib_path,
                    mod_id: None,
                    action: ModHotReloadAction::Failed(err.to_string()),
                }),
            }
        }

        outcomes
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use bevy_abi_mod_host::app_ext::ModAppExt;
        use bevy_abi_mod_host::events::{ModLoaded, ModUnloaded};
        use bevy_abi_mod_host::plugin::AbiModPlugin;
        use bevy_abi_mod_macros::ModComponent;
        use bevy_ecs::message::{MessageCursor, Messages};
        use bevy_ecs::prelude::Resource;
        use std::process::Command;
        use std::sync::atomic::{AtomicU64, Ordering};

        #[repr(C)]
        #[derive(Resource, ModComponent, Clone, Copy, Debug, Default, PartialEq, Eq)]
        #[mod_component(name = "watcher_test::WatchCounter")]
        struct WatchCounter {
            value: u32,
        }

        struct TempDirGuard(PathBuf);
        impl TempDirGuard {
            fn new(prefix: &str) -> Self {
                static NEXT_ID: AtomicU64 = AtomicU64::new(1);
                let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
                let dir =
                    std::env::temp_dir().join(format!("{prefix}_{}_{}", std::process::id(), id));
                std::fs::create_dir_all(&dir).unwrap();
                Self(dir)
            }
        }
        impl Drop for TempDirGuard {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }

        fn compile_c_watch_mod(
            src_dir: &Path,
            out_dylib: &Path,
            mod_name: &str,
            mod_version: &str,
            delta: u32,
        ) {
            let include_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join("..")
                .join("include");
            let c_src = src_dir.join(format!("{mod_name}.c"));
            let code = format!(
                r#"#include "bevy_abi_mod.h"
#include <stddef.h>
#include <stdint.h>
#include <string.h>

typedef struct WatchCounter {{
    uint32_t value;
}} WatchCounter;

typedef struct ModState {{
    BamHostApi api;
    BamComponent counter_res;
}} ModState;

static ModState g_state;

static BamResult tick_cb(BamSystemCtx* sys_ctx, void* ud) {{
    (void)ud;
    WatchCounter* c = (WatchCounter*)g_state.api.sys_res_mut_ptr(sys_ctx, g_state.counter_res, NULL);
    if (!c) return BAM_RESULT_NULL_POINTER;
    c->value += {delta}u;
    return BAM_RESULT_OK;
}}

static const BamManifest MANIFEST = {{
    (uint32_t)sizeof(BamManifest),
    BAM_ABI_VERSION,
    "{mod_name}",
    "{mod_version}",
    0u,
    0u,
    NULL,
    NULL
}};

BAM_EXPORT const BamManifest* bevy_abi_mod_manifest(void) {{
    return &MANIFEST;
}}

BAM_EXPORT BamResult bevy_abi_mod_init(const BamHostApi* api, BamLoadCtx* load_ctx, BamInstance** out) {{
    if (!api || !load_ctx || !out) return BAM_RESULT_NULL_POINTER;
    g_state.api = *api;
    const BamHostCtx* hctx = api->load_host_ctx(load_ctx);
    BamResult r = api->resolve_component(hctx, "watcher_test::WatchCounter", NULL, &g_state.counter_res);
    if (r != BAM_RESULT_OK) return r;
    BamTermDesc term = bam_term_desc(g_state.counter_res, BAM_ACCESS_RESOURCE_WRITE, 0u);
    BamQueryDesc q;
    q.struct_size = (uint32_t)sizeof(BamQueryDesc);
    q.flags = 0u;
    q.terms = &term;
    q.term_count = 1u;
    BamSystemDesc sys;
    memset(&sys, 0, sizeof(sys));
    sys.struct_size = (uint32_t)sizeof(BamSystemDesc);
    sys.name = "{mod_name}_tick";
    sys.queries = &q;
    sys.query_count = 1u;
    sys.callback = tick_cb;
    sys.schedule = BAM_SCHEDULE_UPDATE;
    r = api->register_system(load_ctx, &sys);
    if (r != BAM_RESULT_OK) return r;
    *out = (BamInstance*)&g_state;
    return BAM_RESULT_OK;
}}

BAM_EXPORT void bevy_abi_mod_shutdown(BamInstance* instance) {{
    (void)instance;
}}
"#
            );
            std::fs::write(&c_src, code).unwrap();

            let cc = if let Ok(cc) = std::env::var("CC")
                && !cc.trim().is_empty()
            {
                cc.trim().to_owned()
            } else {
                #[cfg(target_os = "windows")]
                let cands = ["clang", "clang-cl", "cl.exe", "cl", "gcc", "cc"];
                #[cfg(not(target_os = "windows"))]
                let cands = ["clang", "gcc", "cc"];
                cands
                    .iter()
                    .copied()
                    .find(|cand| {
                        let is_cl = std::path::Path::new(cand)
                            .file_name()
                            .and_then(|s| s.to_str())
                            .is_some_and(|n| {
                                n.eq_ignore_ascii_case("cl") || n.eq_ignore_ascii_case("cl.exe")
                            });
                        let res = if is_cl {
                            Command::new(cand).output()
                        } else {
                            Command::new(cand).arg("--version").output()
                        };
                        matches!(res, Ok(out) if out.status.success() || (is_cl && !out.stderr.is_empty()))
                    })
                    .unwrap_or(if cfg!(target_os = "windows") {
                        "gcc"
                    } else {
                        "cc"
                    })
                    .to_owned()
            };

            let staging = src_dir.join(format!(
                "{mod_name}_tmp_out.{}",
                std::env::consts::DLL_EXTENSION
            ));
            let is_msvc = std::path::Path::new(&cc)
                .file_name()
                .and_then(|s| s.to_str())
                .is_some_and(|n| {
                    let s = n
                        .strip_suffix(".exe")
                        .or_else(|| n.strip_suffix(".EXE"))
                        .unwrap_or(n);
                    s.eq_ignore_ascii_case("cl") || s.eq_ignore_ascii_case("clang-cl")
                });
            let mut cmd = Command::new(&cc);
            if is_msvc {
                let obj_path = src_dir.join(format!("{mod_name}_tmp.obj"));
                let implib_path = src_dir.join(format!("{mod_name}_tmp.lib"));
                cmd.args(["/nologo", "/LD", "/std:c11", "/W4", "/WX", "/O2"])
                    .arg(format!("/I{}", include_dir.display()))
                    .arg(format!("/Fo{}", obj_path.display()))
                    .arg(&c_src)
                    .arg("/link")
                    .arg(format!("/IMPLIB:{}", implib_path.display()))
                    .arg(format!("/OUT:{}", staging.display()));
            } else {
                cmd.args(["-std=c99", "-Wall", "-Wextra", "-Werror"])
                    .arg(format!("-I{}", include_dir.display()))
                    .args(["-O2", "-shared"]);
                if !cfg!(target_os = "windows") {
                    cmd.arg("-fPIC");
                }
                cmd.arg(&c_src).arg("-o").arg(&staging);
            }
            let out = cmd
                .output()
                .expect("C compiler must be available for watcher test");
            assert!(
                out.status.success(),
                "C compiler failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            if std::fs::rename(&staging, out_dylib).is_err() {
                std::fs::copy(&staging, out_dylib).unwrap();
                let _ = std::fs::remove_file(&staging);
            }
        }

        fn dylib_filename(stem: &str) -> String {
            let prefix = std::env::consts::DLL_PREFIX;
            let ext = std::env::consts::DLL_EXTENSION;
            format!("{prefix}{stem}.{ext}")
        }

        #[test]
        fn watcher_live_reloads_modified_shared_library() {
            let temp = TempDirGuard::new("bam_hot_reload_watcher");
            let mods_dir = temp.0.join("mods");
            let build_dir = temp.0.join("build");
            std::fs::create_dir_all(&mods_dir).unwrap();
            std::fs::create_dir_all(&build_dir).unwrap();

            let dylib_path = mods_dir.join(dylib_filename("watch_mod"));
            compile_c_watch_mod(&build_dir, &dylib_path, "watch_mod", "0.2.0", 1);

            let mut app = App::new();
            app.add_plugins(AbiModPlugin);
            app.add_plugins(
                ModHotReloadPlugin::new(&mods_dir).with_debounce(Duration::from_millis(50)),
            );
            app.insert_resource(WatchCounter::default());
            app.register_mod_resource::<WatchCounter>().unwrap();

            app.load_mod_with(&dylib_path, LoadOptions::new().with_shadow_copy(true))
                .expect("initial load");
            app.update();
            assert_eq!(app.world().resource::<WatchCounter>().value, 1);

            std::thread::sleep(Duration::from_millis(120));
            compile_c_watch_mod(&build_dir, &dylib_path, "watch_mod", "0.2.0", 10);

            let mut reloaded = false;
            for _ in 0..30 {
                std::thread::sleep(Duration::from_millis(80));
                app.update();
                if app
                    .world()
                    .resource::<ModHotReloadHistory>()
                    .outcomes
                    .iter()
                    .any(|o| o.action == ModHotReloadAction::Reloaded)
                {
                    reloaded = true;
                    break;
                }
            }
            assert!(
                reloaded,
                "ModHotReloadPlugin did not detect and reload modified shared library"
            );

            let before = app.world().resource::<WatchCounter>().value;
            app.update();
            let after = app.world().resource::<WatchCounter>().value;
            assert_eq!(
                after - before,
                10,
                "reloaded v2 system must increment WatchCounter by 10 per tick"
            );
        }

        #[test]
        fn watcher_cascades_dag_reload_on_base_mod_rebuild_and_rejects_broken_semver_edit() {
            let temp = TempDirGuard::new("bam_hot_reload_dag");
            let mods_dir = temp.0.join("mods");
            let base_dir = mods_dir.join("base_mod");
            let addon_dir = mods_dir.join("addon_mod");
            let build_dir = temp.0.join("build");
            std::fs::create_dir_all(&base_dir).unwrap();
            std::fs::create_dir_all(&addon_dir).unwrap();
            std::fs::create_dir_all(&build_dir).unwrap();

            let base_dylib = base_dir.join(dylib_filename("base_mod"));
            let addon_dylib = addon_dir.join(dylib_filename("addon_mod"));

            std::fs::write(
                base_dir.join("mod.toml"),
                "[mod]\nid = \"base_mod\"\nversion = \"1.0.0\"\nabi_version = \"^0.3.0\"\n",
            )
            .unwrap();
            std::fs::write(
                addon_dir.join("mod.toml"),
                "[mod]\nid = \"addon_mod\"\nversion = \"1.0.0\"\nabi_version = \"^0.3.0\"\n\n[dependencies]\nbase_mod = \"^1.0\"\n",
            )
            .unwrap();

            compile_c_watch_mod(&build_dir, &base_dylib, "base_mod", "1.0.0", 2);
            compile_c_watch_mod(&build_dir, &addon_dylib, "addon_mod", "1.0.0", 3);

            let mut app = App::new();
            app.add_plugins(AbiModPlugin);
            app.insert_resource(WatchCounter::default());
            app.register_mod_resource::<WatchCounter>().unwrap();

            let report = app
                .world_mut()
                .load_mods_dir_with(&mods_dir, LoadOptions::new().with_shadow_copy(true))
                .expect("load_mods_dir");
            assert_eq!(report.loaded.len(), 2);
            assert_eq!(report.loaded[0].0, "base_mod");
            assert_eq!(report.loaded[1].0, "addon_mod");

            app.add_plugins(
                ModHotReloadPlugin::new(&mods_dir).with_debounce(Duration::from_millis(50)),
            );

            let mut unload_cursor = MessageCursor::<ModUnloaded>::default();
            let mut load_cursor = MessageCursor::<ModLoaded>::default();
            {
                let msgs_u = app.world().resource::<Messages<ModUnloaded>>();
                let _ = unload_cursor.read(msgs_u).count();
                let msgs_l = app.world().resource::<Messages<ModLoaded>>();
                let _ = load_cursor.read(msgs_l).count();
            }

            app.update();
            assert_eq!(app.world().resource::<WatchCounter>().value, 5); // 2 + 3

            // Rebuild ONLY `base_mod` on disk with delta = 20 -> cascading reload must reload
            // `addon_mod` -> `base_mod` (unload) then `base_mod` -> `addon_mod` (load)!
            std::thread::sleep(Duration::from_millis(120));
            compile_c_watch_mod(&build_dir, &base_dylib, "base_mod", "1.0.0", 20);

            let mut reloaded = false;
            for _ in 0..40 {
                std::thread::sleep(Duration::from_millis(80));
                app.update();
                if app
                    .world()
                    .resource::<ModHotReloadHistory>()
                    .outcomes
                    .iter()
                    .any(|o| o.action == ModHotReloadAction::Reloaded)
                {
                    reloaded = true;
                    break;
                }
            }
            assert!(
                reloaded,
                "ModHotReloadWatcher must cascade reload when base_mod changes"
            );

            let unloaded_order: Vec<String> = {
                let msgs = app.world().resource::<Messages<ModUnloaded>>();
                unload_cursor.read(msgs).map(|m| m.name.clone()).collect()
            };
            let loaded_order: Vec<String> = {
                let msgs = app.world().resource::<Messages<ModLoaded>>();
                load_cursor.read(msgs).map(|m| m.name.clone()).collect()
            };
            assert_eq!(unloaded_order, vec!["addon_mod", "base_mod"]);
            assert_eq!(loaded_order, vec!["base_mod", "addon_mod"]);

            let before = app.world().resource::<WatchCounter>().value;
            app.update();
            let after = app.world().resource::<WatchCounter>().value;
            assert_eq!(
                after - before,
                23,
                "cascaded reload must run v2 base_mod (20) + addon_mod (3)"
            );

            // Introducing a SemVer mismatch in `addon_mod/mod.toml` must fail safely with a
            // `Failed` outcome while keeping both active mods loaded and running!
            std::thread::sleep(Duration::from_millis(120));
            std::fs::write(
                addon_dir.join("mod.toml"),
                "[mod]\nid = \"addon_mod\"\nversion = \"1.0.0\"\n\n[dependencies]\nbase_mod = \"^9.0\"\n",
            )
            .unwrap();

            let mut saw_failure = false;
            for _ in 0..40 {
                std::thread::sleep(Duration::from_millis(80));
                app.update();
                if app
                    .world()
                    .resource::<ModHotReloadHistory>()
                    .outcomes
                    .iter()
                    .any(|o| matches!(o.action, ModHotReloadAction::Failed(_)))
                {
                    saw_failure = true;
                    break;
                }
            }
            assert!(
                saw_failure,
                "invalid mod.toml SemVer edit must record Failed outcome"
            );
            assert_eq!(app.world().resource::<ModHost>().len(), 2);
            let before_after_err = app.world().resource::<WatchCounter>().value;
            app.update();
            let after_after_err = app.world().resource::<WatchCounter>().value;
            assert_eq!(after_after_err - before_after_err, 23);
        }
    }
}

pub use imp::{
    ModHotReloadAction, ModHotReloadHistory, ModHotReloadOutcome, ModHotReloadPlugin,
    ModHotReloadWatcher,
};
