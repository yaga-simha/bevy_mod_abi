//! Isolated `mod://<mod_name>/...` asset source, traversal-safe `AssetReader`, and
//! [`HostAssetBackend`] implementation for `bevy_asset` (`0.20`).
//!
//! Enforces `#![forbid(unsafe_code)]`, per-mod directory sandboxing (`..` and symlink escape
//! rejection), owner-scoped namespace isolation, typed (`LoadBuilder::load_erased`) and untyped
//! (`LoadBuilder::load_untyped`) async loading, and automatic handle release on mod unload.

use bevy_abi_mod_host_sys::{
    ComponentCollisionError, HostAssetBackend, ModRegistry, RegistrySnapshot,
};
use bevy_abi_mod_sys::{BamComponent, ComponentUuid, FfiAssetHandle, FfiAssetLoadState, FfiResult};
use bevy_app::{App, Plugin};
use bevy_asset::io::file::FileAssetReader;
use bevy_asset::io::{
    AssetReader, AssetReaderError, AssetSourceBuilder, PathStream, Reader, VecReader,
};
use bevy_asset::uuid::Uuid;
use bevy_asset::{
    Asset, AssetApp, AssetIndex, AssetServer, Assets, LoadState, LoadedUntypedAsset,
    UntypedAssetId, UntypedHandle,
};
use bevy_ecs::prelude::Resource;
use bevy_ecs::world::World;
use bevy_platform::collections::HashMap;
use core::any::TypeId;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

#[derive(Default)]
struct ModAssetInner {
    dirs: RwLock<HashMap<String, PathBuf>>,
    asset_server: RwLock<Option<AssetServer>>,
    bam_to_type_id: RwLock<HashMap<BamComponent, TypeId>>,
    untyped_handle_type: RwLock<Option<BamComponent>>,
    retained: Mutex<HashMap<Option<String>, HashMap<FfiAssetHandle, (UntypedHandle, usize)>>>,
}

/// Resource tracking per-mod asset root directories, registered `BamComponent -> TypeId` asset
/// mappings, and host-retained strong [`UntypedHandle`] pins.
#[derive(Resource, Clone, Default)]
pub struct ModAssetRegistry {
    inner: Arc<ModAssetInner>,
}

impl ModAssetRegistry {
    /// Creates an empty [`ModAssetRegistry`].
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
        if mod_name.is_empty() || mod_name.contains('/') || mod_name.contains('\\') {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "invalid mod asset namespace",
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
        Ok(canonical)
    }

    /// Unmounts `mod_name`'s asset directory, returning `true` if it was mounted.
    pub fn unmount_mod_asset_dir(&self, mod_name: &str) -> bool {
        self.inner
            .dirs
            .write()
            .is_ok_and(|mut dirs| dirs.remove(mod_name).is_some())
    }

    /// Returns `true` if `mod_name` currently has a mounted asset directory.
    pub fn has_mod_asset_dir(&self, mod_name: &str) -> bool {
        self.inner
            .dirs
            .read()
            .is_ok_and(|dirs| dirs.contains_key(mod_name))
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

/// Bevy [`Plugin`] mounting the isolated `"mod"` [`AssetSourceBuilder`] (`mod://<mod_name>/...`),
/// installing [`ModAssetRegistry`] and a [`HostAssetBackend`] on [`ModRegistry`].
///
/// Must be added **before** Bevy's [`AssetPlugin`](bevy_asset::AssetPlugin) so the `"mod"` asset
/// source is registered into [`AssetSourceBuilders`](bevy_asset::io::AssetSourceBuilders).
#[derive(Default, Debug, Clone, Copy)]
pub struct ModAssetPlugin;

impl Plugin for ModAssetPlugin {
    fn build(&self, app: &mut App) {
        let asset_reg = if let Some(existing) = app.world().get_resource::<ModAssetRegistry>() {
            existing.clone()
        } else {
            let reg = ModAssetRegistry::new();
            app.insert_resource(reg.clone());
            reg
        };

        let reader_inner = Arc::clone(&asset_reg.inner);
        app.register_asset_source(
            "mod",
            AssetSourceBuilder::new(move || {
                Box::new(ModAssetReader::new(Arc::clone(&reader_inner)))
            }),
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

        if let Some(server) = app.world().get_resource::<AssetServer>().cloned() {
            asset_reg.bind_asset_server(server);
        }
    }

    fn finish(&self, app: &mut App) {
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
}

/// Extension trait on [`World`] for mounting mod asset directories and exposing [`Asset`] types
/// across the C ABI.
pub trait ModAssetWorldExt {
    /// Mounts `root_dir` as the isolated filesystem root for `mod_name` (`mod://<mod_name>/...`).
    fn mount_mod_asset_dir(
        &mut self,
        mod_name: &str,
        root_dir: impl AsRef<Path>,
    ) -> std::io::Result<PathBuf>;

    /// Unmounts `mod_name`'s asset directory.
    fn unmount_mod_asset_dir(&mut self, mod_name: &str) -> bool;

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

    fn unmount_mod_asset_dir(&mut self, mod_name: &str) -> bool {
        self.get_resource::<ModAssetRegistry>()
            .is_some_and(|reg| reg.unmount_mod_asset_dir(mod_name))
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

/// Extension trait on [`App`] for mounting mod asset directories and exposing [`Asset`] types.
pub trait ModAssetAppExt {
    /// Mounts `root_dir` as the isolated filesystem root for `mod_name` (`mod://<mod_name>/...`).
    fn mount_mod_asset_dir(
        &mut self,
        mod_name: &str,
        root_dir: impl AsRef<Path>,
    ) -> std::io::Result<PathBuf>;

    /// Unmounts `mod_name`'s asset directory.
    fn unmount_mod_asset_dir(&mut self, mod_name: &str) -> bool;

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
    fn unmount_mod_asset_dir(&mut self, mod_name: &str) -> bool {
        self.world_mut().unmount_mod_asset_dir(mod_name)
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

    fn resolve_existing_path(
        &self,
        path: &Path,
        append_meta: bool,
    ) -> Result<(PathBuf, PathBuf, PathBuf), AssetReaderError> {
        let (mod_name, mut subpath) = Self::split_mod_subpath(path)?;
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

        let candidate = if subpath.as_os_str().is_empty() {
            root_dir.clone()
        } else {
            root_dir.join(&subpath)
        };

        let canonical_target = std::fs::canonicalize(&candidate).map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                AssetReaderError::NotFound(candidate.clone())
            } else {
                AssetReaderError::Io(Arc::new(err))
            }
        })?;

        if !canonical_target.starts_with(&root_dir) {
            return Err(permission_denied_error(
                "resolved asset path escapes mounted mod directory",
            ));
        }

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
                .normalize_asset_path(Some("alpha_mod"), "../beta_mod/secret.txt")
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
