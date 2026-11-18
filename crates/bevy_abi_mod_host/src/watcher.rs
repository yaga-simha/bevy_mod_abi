//! Debounced filesystem hot-reload watcher integration (`feature = "hot-reload-watcher"`).

#[cfg(feature = "hot-reload-watcher")]
mod imp {
    use crate::world_ext::ModWorldExt;
    use bevy_abi_mod_host_sys::{LoadOptions, ModHost, ModId, canonical_mod_name_from_path};
    use bevy_app::{App, Plugin, PreUpdate};
    use bevy_ecs::prelude::Resource;
    use bevy_ecs::world::World;
    use notify_debouncer_full::notify::event::{CreateKind, ModifyKind};
    use notify_debouncer_full::notify::{EventKind, RecommendedWatcher, RecursiveMode};
    use notify_debouncer_full::{DebounceEventResult, Debouncer, RecommendedCache, new_debouncer};
    use serde::Deserialize;
    use std::collections::HashSet;
    use std::path::{Path, PathBuf};
    use std::sync::{Mutex, mpsc};
    use std::time::Duration;

    /// Action taken by [`ModHotReloadWatcher`] for a changed mod artifact or manifest.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub enum ModHotReloadAction {
        /// A newly discovered mod library was loaded into the `World`.
        Loaded,
        /// An already-loaded mod library was hot-reloaded in place.
        Reloaded,
        /// Loading or reloading the changed mod failed with a diagnostic message.
        Failed(String),
    }

    /// Outcome record emitted by [`ModHotReloadWatcher::process_pending_reloads`] and stored in
    /// [`ModHotReloadHistory`].
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct ModHotReloadOutcome {
        /// Path of the mod shared library that triggered the operation.
        pub path: PathBuf,
        /// Resulting [`ModId`] on success (`Loaded` / `Reloaded`), or `None` on failure.
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

    /// Debounced filesystem watcher (`notify-debouncer-full`) monitoring a directory for native
    /// mod shared libraries (`.so`, `.dylib`, `.dll`) and `mod.toml` manifest edits.
    #[derive(Resource)]
    pub struct ModHotReloadWatcher {
        watch_dir: PathBuf,
        load_options: LoadOptions,
        rx: Mutex<mpsc::Receiver<DebounceEventResult>>,
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
                rx: Mutex::new(rx),
                _debouncer: Mutex::new(debouncer),
            })
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
            let mut out = Vec::new();
            let mut seen = HashSet::new();

            let Ok(guard) = self.rx.lock() else {
                return out;
            };
            while let Ok(batch) = guard.try_recv() {
                let Ok(events) = batch else {
                    continue;
                };
                for ev in events {
                    if !is_relevant_event_kind(ev.kind) {
                        continue;
                    }
                    for p in &ev.paths {
                        if is_watchable_mod_path(p) && seen.insert(p.clone()) {
                            out.push(p.clone());
                        }
                    }
                }
            }
            out
        }

        /// Drains pending debounced changes and loads or reloads affected mods in `world`.
        pub fn process_pending_reloads(&self, world: &mut World) -> Vec<ModHotReloadOutcome> {
            let paths = self.poll_changed_paths();
            apply_hot_reload_paths(world, &paths, &self.load_options)
        }
    }

    /// Bevy [`Plugin`] installing [`ModHotReloadWatcher`] and polling it each [`PreUpdate`] tick.
    #[derive(Clone, Debug)]
    pub struct ModHotReloadPlugin {
        watch_dir: PathBuf,
        debounce: Duration,
        recursive: bool,
        load_options: LoadOptions,
    }

    impl ModHotReloadPlugin {
        /// Creates a [`ModHotReloadPlugin`] watching `dir` with a 200ms debounce window.
        pub fn new(dir: impl AsRef<Path>) -> Self {
            Self {
                watch_dir: dir.as_ref().to_path_buf(),
                debounce: Duration::from_millis(200),
                recursive: true,
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
        let Some((paths, options)) = world
            .get_resource::<ModHotReloadWatcher>()
            .map(|w| (w.poll_changed_paths(), w.load_options().clone()))
        else {
            return;
        };
        if paths.is_empty() {
            return;
        }
        let outcomes = apply_hot_reload_paths(world, &paths, &options);
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

    #[derive(Deserialize)]
    struct MinimalWatcherManifest {
        #[serde(rename = "mod")]
        mod_section: Option<MinimalWatcherModSection>,
    }

    #[derive(Deserialize)]
    struct MinimalWatcherModSection {
        entry: Option<String>,
        id: Option<String>,
        name: Option<String>,
    }

    fn resolve_library_from_manifest(manifest_path: &Path) -> Option<PathBuf> {
        let dir = manifest_path.parent()?;
        let content = std::fs::read_to_string(manifest_path).ok()?;
        let parsed: MinimalWatcherManifest = toml::from_str(&content).ok()?;
        if let Some(sec) = parsed.mod_section {
            if let Some(entry) = sec.entry {
                let candidate = dir.join(&entry);
                if candidate.is_file() && is_dylib_path(&candidate) {
                    return Some(candidate);
                }
                let platform_name = format_dylib_filename(&entry);
                let platform_candidate = dir.join(&platform_name);
                if platform_candidate.is_file() {
                    return Some(platform_candidate);
                }
            }
            if let Some(stem) = sec.id.or(sec.name) {
                let candidate = dir.join(format_dylib_filename(&stem));
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
        std::fs::read_dir(dir).ok()?.flatten().find_map(|entry| {
            let p = entry.path();
            (p.is_file() && is_dylib_path(&p)).then_some(p)
        })
    }

    fn format_dylib_filename(stem: &str) -> String {
        if cfg!(target_os = "windows") {
            format!("{stem}.dll")
        } else if cfg!(target_os = "macos") {
            format!("lib{stem}.dylib")
        } else {
            format!("lib{stem}.so")
        }
    }

    fn find_loaded_mod_for_path(world: &World, lib_path: &Path) -> Option<ModId> {
        let host = world.get_resource::<ModHost>()?;
        let canon_target = std::fs::canonicalize(lib_path).ok();
        let target_stem = canonical_mod_name_from_path(lib_path);

        for m in host.loaded_mods() {
            if !m.is_active() {
                continue;
            }
            if m.source_path() == lib_path {
                return Some(m.id());
            }
            if let Some(ref ct) = canon_target
                && std::fs::canonicalize(m.source_path()).ok().as_ref() == Some(ct)
            {
                return Some(m.id());
            }
            if canonical_mod_name_from_path(m.source_path()) == target_stem
                && m.source_path().parent() == lib_path.parent()
            {
                return Some(m.id());
            }
        }
        None
    }

    fn apply_hot_reload_paths(
        world: &mut World,
        paths: &[PathBuf],
        load_options: &LoadOptions,
    ) -> Vec<ModHotReloadOutcome> {
        let mut resolved_libs = Vec::new();
        let mut seen_libs = HashSet::new();

        for path in paths {
            let lib_path = if path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n == "mod.toml")
            {
                match resolve_library_from_manifest(path) {
                    Some(p) => p,
                    None => continue,
                }
            } else if is_dylib_path(path) && path.is_file() {
                path.clone()
            } else {
                continue;
            };

            let dedup_key = std::fs::canonicalize(&lib_path).unwrap_or_else(|_| lib_path.clone());
            if seen_libs.insert(dedup_key) {
                resolved_libs.push(lib_path);
            }
        }

        let mut outcomes = Vec::with_capacity(resolved_libs.len());
        let opts = load_options.clone().with_shadow_copy(true);
        for lib_path in resolved_libs {
            if let Some(existing_id) = find_loaded_mod_for_path(world, &lib_path) {
                match world.reload_mod_from(existing_id, &lib_path, opts.clone()) {
                    Ok(new_id) => outcomes.push(ModHotReloadOutcome {
                        path: lib_path,
                        mod_id: Some(new_id),
                        action: ModHotReloadAction::Reloaded,
                    }),
                    Err(err) => outcomes.push(ModHotReloadOutcome {
                        path: lib_path,
                        mod_id: None,
                        action: ModHotReloadAction::Failed(err.to_string()),
                    }),
                }
            } else {
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
        }
        outcomes
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::app_ext::ModAppExt;
        use crate::plugin::AbiModPlugin;
        use bevy_abi_mod_macros::ModComponent;
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

        fn compile_c_watch_mod(src_dir: &Path, out_dylib: &Path, delta: u32) {
            let include_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join("..")
                .join("include");
            let c_src = src_dir.join("watch_mod.c");
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
    "watch_mod",
    "0.2.0",
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
    sys.name = "watch_tick";
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

            let cc = std::env::var("CC")
                .ok()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| {
                    if cfg!(target_os = "windows") {
                        "gcc".to_owned()
                    } else {
                        "cc".to_owned()
                    }
                });

            let staging = src_dir.join("tmp_out_lib");
            let mut cmd = Command::new(&cc);
            cmd.args(["-std=c99", "-Wall", "-Wextra", "-Werror"])
                .arg(format!("-I{}", include_dir.display()))
                .args(["-O2", "-shared"]);
            if !cfg!(target_os = "windows") {
                cmd.arg("-fPIC");
            }
            cmd.arg(&c_src).arg("-o").arg(&staging);
            let out = cmd
                .output()
                .expect("C compiler must be available for watcher test");
            assert!(
                out.status.success(),
                "C compiler failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            std::fs::rename(&staging, out_dylib).unwrap();
        }

        #[test]
        fn watcher_live_reloads_modified_shared_library() {
            let temp = TempDirGuard::new("bam_hot_reload_watcher");
            let mods_dir = temp.0.join("mods");
            let build_dir = temp.0.join("build");
            std::fs::create_dir_all(&mods_dir).unwrap();
            std::fs::create_dir_all(&build_dir).unwrap();

            let dylib_path = mods_dir.join(format_dylib_filename("watch_mod"));
            compile_c_watch_mod(&build_dir, &dylib_path, 1);

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

            // Wait briefly so filesystem mtime/debouncer sees a distinct modification.
            std::thread::sleep(Duration::from_millis(120));
            compile_c_watch_mod(&build_dir, &dylib_path, 10);

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
    }
}

#[cfg(feature = "hot-reload-watcher")]
pub use imp::{
    ModHotReloadAction, ModHotReloadHistory, ModHotReloadOutcome, ModHotReloadPlugin,
    ModHotReloadWatcher,
};
