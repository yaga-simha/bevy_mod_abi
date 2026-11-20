#![forbid(unsafe_code)]

//! Integration tests for `bridge-asset` (`ModAssetPlugin`, `ModAssetRegistry`,
//! `AssetOverlayPolicy`, path sandboxing, async loading, and multi-layer overlay directory
//! merging via `AssetServer::load_folder`).

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use bevy_abi_mod_bridges::asset::{
    AssetOverlayPolicy, ModAssetAppExt, ModAssetPlugin, ModAssetRegistry, OVERLAY_NAMESPACE,
};
use bevy_abi_mod_host::AbiModPlugin;
use bevy_abi_mod_host_sys::ModRegistry;
use bevy_abi_mod_sys::{BamComponent, FfiAssetLoadState, FfiResult};
use bevy_app::{App, TaskPoolPlugin};
use bevy_asset::io::Reader;
use bevy_asset::{
    Asset, AssetApp, AssetId, AssetIndex, AssetLoader, AssetMetaCheck, AssetPlugin, AssetServer,
    Assets, LoadContext, LoadState, LoadedFolder,
};
use bevy_reflect::TypePath;

static TEMP_DIR_SEQ: AtomicUsize = AtomicUsize::new(0);

struct TempTestDir {
    path: PathBuf,
}

impl TempTestDir {
    fn new(prefix: &str) -> Self {
        let seq = TEMP_DIR_SEQ.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "bam_asset_integ_{prefix}_{}_{}",
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

fn setup_asset_app(default_dir: &TempTestDir) -> App {
    let mut app = App::new();
    app.add_plugins((
        TaskPoolPlugin::default(),
        AbiModPlugin,
        ModAssetPlugin,
        AssetPlugin {
            file_path: default_dir.path.to_string_lossy().into_owned(),
            meta_check: AssetMetaCheck::Never,
            ..Default::default()
        },
    ));
    app.init_asset::<TextAsset>();
    app.register_asset_loader(TextAssetLoader);
    app.finish();
    app
}

#[test]
fn mod_asset_reader_sandboxing_and_path_normalization() {
    let default_dir = TempTestDir::new("norm_default");
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

    let mut app = setup_asset_app(&default_dir);
    app.mount_mod_asset_dir("alpha_mod", &alpha_dir.path)
        .unwrap();
    app.mount_mod_asset_dir("beta_mod", &beta_dir.path).unwrap();

    // Reserved "overlay" namespace cannot be hijacked by a mod mount.
    assert!(
        app.mount_mod_asset_dir(OVERLAY_NAMESPACE, &alpha_dir.path)
            .is_err()
    );

    let text_bam = app.register_mod_asset::<TextAsset>().unwrap();
    let snap = app.world().resource::<ModRegistry>().snapshot_arc();
    let backend = snap.asset_backend().unwrap().clone();

    // Valid relative & explicit mod:// paths resolve cleanly.
    let h_rel = backend
        .load_asset(&snap, Some("alpha_mod"), text_bam, "sub/ok.txt")
        .unwrap();
    let h_abs = backend
        .load_asset(
            &snap,
            Some("alpha_mod"),
            text_bam,
            "mod://alpha_mod/sub/ok.txt",
        )
        .unwrap();
    assert_eq!(h_rel, h_abs);

    // Cross-mod, traversal, absolute paths, and disabled overlay are rejected.
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
                "mod://overlay/sub/ok.txt"
            )
            .unwrap_err()
            .0,
        FfiResult::PERMISSION_DENIED
    );
    assert_eq!(
        backend
            .load_asset(&snap, Some("alpha_mod"), text_bam, "../beta_mod/secret.txt")
            .unwrap_err()
            .0,
        FfiResult::PERMISSION_DENIED
    );
    assert_eq!(
        backend
            .load_asset(&snap, Some("alpha_mod"), text_bam, "/etc/passwd")
            .unwrap_err()
            .0,
        FfiResult::PERMISSION_DENIED
    );

    // Opting into `LoadOrderPriority` enables `mod://overlay/...` while keeping cross-mod &
    // traversal blocked.
    app.world()
        .resource::<ModAssetRegistry>()
        .set_overlay_policy(AssetOverlayPolicy::LoadOrderPriority);
    let h_overlay = backend
        .load_asset(
            &snap,
            Some("alpha_mod"),
            text_bam,
            "mod://overlay/sub/ok.txt",
        )
        .unwrap();
    assert!(h_overlay.is_some());
    assert_eq!(
        backend
            .load_asset(
                &snap,
                Some("alpha_mod"),
                text_bam,
                "mod://overlay/../beta_mod/secret.txt"
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
                "mod://beta_mod/secret.txt"
            )
            .unwrap_err()
            .0,
        FfiResult::PERMISSION_DENIED
    );
}

#[test]
fn real_async_mod_asset_load_isolation_traversal_and_unload_release() {
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

    let mut app = setup_asset_app(&default_dir);

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
    let typed_id = AssetId::<TextAsset>::from(AssetIndex::from_bits(bits));
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

#[test]
fn multi_layer_overlay_read_directory_merges_and_deduplicates_entries() {
    let default_dir = TempTestDir::new("overlay_dir_default");
    let base_dir = TempTestDir::new("overlay_dir_base");
    let mod_a_dir = TempTestDir::new("overlay_dir_mod_a");
    let mod_b_dir = TempTestDir::new("overlay_dir_mod_b");
    let outside_dir = TempTestDir::new("overlay_dir_outside");

    fs::create_dir_all(base_dir.path.join("shared/nested")).unwrap();
    fs::write(base_dir.path.join("shared/base_only.txt"), b"from_base").unwrap();
    fs::write(base_dir.path.join("shared/override.txt"), b"base_version").unwrap();
    fs::write(base_dir.path.join("shared/nested/deep.txt"), b"deep_base").unwrap();

    fs::create_dir_all(mod_a_dir.path.join("shared")).unwrap();
    fs::write(mod_a_dir.path.join("shared/alpha_only.txt"), b"from_alpha").unwrap();
    fs::write(mod_a_dir.path.join("shared/override.txt"), b"alpha_version").unwrap();
    fs::write(mod_a_dir.path.join("shared/.hidden.txt"), b"hidden").unwrap();
    fs::write(mod_a_dir.path.join("shared/alpha_only.txt.meta"), b"meta").unwrap();

    fs::create_dir_all(mod_b_dir.path.join("shared/nested")).unwrap();
    fs::write(mod_b_dir.path.join("shared/beta_only.txt"), b"from_beta").unwrap();
    fs::write(mod_b_dir.path.join("shared/override.txt"), b"beta_version").unwrap();
    fs::write(
        mod_b_dir.path.join("shared/nested/deep_beta.txt"),
        b"deep_beta",
    )
    .unwrap();

    fs::write(outside_dir.path.join("escaped.txt"), b"escaped").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(
        outside_dir.path.join("escaped.txt"),
        mod_b_dir.path.join("shared/symlink_escape.txt"),
    )
    .unwrap();

    let mut app = setup_asset_app(&default_dir);
    app.set_asset_overlay_base_dir(&base_dir.path).unwrap();
    app.mount_mod_asset_dir("mod_a", &mod_a_dir.path).unwrap();
    app.mount_mod_asset_dir("mod_b", &mod_b_dir.path).unwrap();
    app.set_asset_overlay_policy(AssetOverlayPolicy::LoadOrderPriority);

    // `AssetServer::load_folder("mod://overlay/shared")` exercises `ModAssetReader::read_directory`
    // and `is_directory` recursively across all active overlay layers!
    let folder_handle = app
        .world()
        .resource::<AssetServer>()
        .load_folder("mod://overlay/shared");

    let mut loaded = false;
    for _ in 0..80 {
        app.update();
        if matches!(
            app.world()
                .resource::<AssetServer>()
                .get_load_state(&folder_handle),
            Some(LoadState::Loaded)
        ) {
            loaded = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(loaded, "mod://overlay/shared folder did not finish loading");

    let folder = app
        .world()
        .resource::<Assets<LoadedFolder>>()
        .get(&folder_handle)
        .expect("LoadedFolder asset");
    let texts = app.world().resource::<Assets<TextAsset>>();

    let mut by_path: BTreeMap<String, String> = BTreeMap::new();
    for handle in &folder.handles {
        let path_str = handle
            .path()
            .expect("folder entry has asset path")
            .to_string();
        let typed_id = handle
            .id()
            .try_typed::<TextAsset>()
            .expect("TextAsset handle");
        let text = texts.get(typed_id).expect("loaded TextAsset").0.clone();
        assert!(
            by_path.insert(path_str.clone(), text).is_none(),
            "duplicate overlay directory entry yielded for {path_str}"
        );
    }

    // Must merge entries across ALL 3 layers (base + mod_a + mod_b) AND recursive subdirectories,
    // deduplicating `override.txt` to highest-priority layer (`mod_b`), while excluding dotfiles,
    // `.meta` sidecars, and symlink escapes!
    assert_eq!(by_path.len(), 6, "merged entries: {by_path:?}");
    assert_eq!(
        by_path
            .get("mod://overlay/shared/base_only.txt")
            .map(String::as_str),
        Some("from_base")
    );
    assert_eq!(
        by_path
            .get("mod://overlay/shared/alpha_only.txt")
            .map(String::as_str),
        Some("from_alpha")
    );
    assert_eq!(
        by_path
            .get("mod://overlay/shared/beta_only.txt")
            .map(String::as_str),
        Some("from_beta")
    );
    assert_eq!(
        by_path
            .get("mod://overlay/shared/override.txt")
            .map(String::as_str),
        Some("beta_version")
    );
    assert_eq!(
        by_path
            .get("mod://overlay/shared/nested/deep.txt")
            .map(String::as_str),
        Some("deep_base")
    );
    assert_eq!(
        by_path
            .get("mod://overlay/shared/nested/deep_beta.txt")
            .map(String::as_str),
        Some("deep_beta")
    );
    assert!(!by_path.contains_key("mod://overlay/shared/.hidden.txt"));
    assert!(!by_path.contains_key("mod://overlay/shared/alpha_only.txt.meta"));
    assert!(!by_path.contains_key("mod://overlay/shared/symlink_escape.txt"));
}
