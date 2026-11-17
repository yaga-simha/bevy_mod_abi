//! Dynamic shared-library loader, shadow-copying (`LoadOptions`), RAII rollback transactions,
//! and `World`-scoped unload (`L-02`, `L-03`, `L-08`).

pub mod error;
pub mod mod_host;
pub mod shared_lib;
pub(crate) mod transaction;

pub use error::{ModLoadError, is_abi_version_compatible};
pub use mod_host::{
    LoadedMod, LoadedModInfo, ModHost, ModId, ModLoaded, ModUnloaded,
    excise_and_unload_mod_in_world, load_mod_in_world, record_mod_named_sets_in_world,
    restore_mod_state_in_world, rollback_unscheduled_mod_in_world, save_mod_state_in_world,
    unload_all_mods_in_world,
};
pub use shared_lib::{LoadOptions, canonical_mod_name_from_path};
