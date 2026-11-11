//! Per-shared-image reference counting so multi-World loads coordinate `bevy_abi_mod_shutdown`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum SharedLibKey {
    #[cfg(unix)]
    UnixInode {
        dev: u64,
        ino: u64,
    },
    CanonicalPath(PathBuf),
}

impl SharedLibKey {
    fn from_path(path: &Path) -> Self {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if let Ok(meta) = std::fs::metadata(path) {
                return Self::UnixInode {
                    dev: meta.dev(),
                    ino: meta.ino(),
                };
            }
        }
        Self::CanonicalPath(std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()))
    }
}

#[derive(Default)]
pub(crate) struct SharedLibState {
    pub(crate) active_count: usize,
}

static ACTIVE_SHARED_LIBS: LazyLock<Mutex<HashMap<SharedLibKey, Arc<Mutex<SharedLibState>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub(crate) fn shared_lib_state_for_path(path: &Path) -> Arc<Mutex<SharedLibState>> {
    let key = SharedLibKey::from_path(path);
    let mut map = ACTIVE_SHARED_LIBS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    Arc::clone(
        map.entry(key)
            .or_insert_with(|| Arc::new(Mutex::new(SharedLibState::default()))),
    )
}

/// Derives a deterministic logical module name from a shared library path
/// (`libfoo.dylib` / `libfoo.so` / `foo.dll` -> `"foo"`).
pub fn canonical_mod_name_from_path(path: &Path) -> String {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unnamed_mod");
    stem.strip_prefix("lib")
        .filter(|rest| !rest.is_empty())
        .unwrap_or(stem)
        .to_string()
}
