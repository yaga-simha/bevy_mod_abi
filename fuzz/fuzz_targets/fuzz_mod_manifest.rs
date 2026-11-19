#![no_main]

//! Fuzz Target: `mod.toml` parser, SemVer validation, and Kahn topological DAG resolution in
//! `bevy_abi_mod_manifest`.

use bevy_abi_mod_manifest::{ModTomlManifest, resolve_dependency_dag};
use libfuzzer_sys::fuzz_target;
use std::collections::BTreeMap;
use std::hint::black_box;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = core::str::from_utf8(data) else {
        return;
    };

    if let Ok(manifest) = ModTomlManifest::parse_str(text, "fuzz/mod.toml") {
        let _ = black_box(manifest.validate_abi_compatibility());
        let _ = black_box(resolve_dependency_dag(vec![manifest], &BTreeMap::new()));
    }
});
