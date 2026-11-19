#![no_main]
#![forbid(unsafe_code)]

//! Fuzz Target: `.bampack` archive parser/verifier (`bevy_abi_mod_trust`), `mod.toml` parser &
//! Kahn topological DAG resolution (`bevy_abi_mod_manifest`), and runtime `ModDependencyGraph`
//! cascading order (`bevy_abi_mod_host`), asserting zero panics, zero OOMs, and strict topological
//! invariants on arbitrary malformed input.

use bevy_abi_mod_host::{LoadOptions, ModDependencyGraph, ModId};
use bevy_abi_mod_manifest::{ModTomlManifest, resolve_dependency_dag};
use bevy_abi_mod_trust::{
    BAMPACK_MAGIC, BAMPACK_VERSION, BamPackBuilder, DEFAULT_BAMPACK_NAMESPACE, ModTrustPolicy,
};
use libfuzzer_sys::fuzz_target;
use std::collections::{BTreeMap, BTreeSet};
use std::hint::black_box;
use std::path::PathBuf;

// Fixed valid Ed25519 trust anchor used solely for parser/verifier fuzzing.
const FUZZ_ED25519_PUB: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIFhmZmZmZmZmZmZmZmZmZmZmZmZmZmZmZmZmZmZmZmZm fuzz@abi";

/// Cap fuzzed input size to keep per-input allocation strictly bounded even under huge corpus files.
const MAX_FUZZ_BYTES: usize = 256 * 1024;
const MAX_MANIFEST_CHUNKS: usize = 16;

fn fuzz_bampack(data: &[u8]) {
    let Ok(base_policy) = ModTrustPolicy::from_ca_openssh(FUZZ_ED25519_PUB) else {
        return;
    };
    let Ok(policy) = base_policy
        .add_public_key_openssh(FUZZ_ED25519_PUB)
        .and_then(|p| p.with_namespace(DEFAULT_BAMPACK_NAMESPACE))
    else {
        return;
    };
    let policy = policy.with_max_binary_bytes(MAX_FUZZ_BYTES as u64);

    // 1. Direct raw archive parsing & cryptographic verification.
    let _ = black_box(policy.verify_bampack_bytes(data));

    // 2. Header-piercing variant: prepend valid 24-byte `BAMPACK\0` v1 header with fuzzed
    //    `entry_count` so coverage rapidly reaches entry path validation, UTF-8, traversal,
    //    duplicate detection, `content_len` overflow, SHA-256 checks, and SSHSIG trailers.
    if let Some((&count_byte, rest)) = data.split_first() {
        let entry_count = u32::from(count_byte % 8);
        let mut synthetic = Vec::with_capacity(24 + rest.len());
        synthetic.extend_from_slice(BAMPACK_MAGIC);
        synthetic.extend_from_slice(&BAMPACK_VERSION.to_le_bytes());
        synthetic.extend_from_slice(&0u32.to_le_bytes()); // flags = 0
        synthetic.extend_from_slice(&entry_count.to_le_bytes());
        synthetic.extend_from_slice(&0u32.to_le_bytes()); // reserved = 0
        synthetic.extend_from_slice(rest);
        let _ = black_box(policy.verify_bampack_bytes(&synthetic));
    }

    // 3. `BamPackBuilder::add_entry` path normalization, traversal, NUL, and duplicate guards.
    let mut builder = BamPackBuilder::new();
    for (idx, chunk) in data.split(|&b| b == 0xFF).take(8).enumerate() {
        let split_at = chunk.len().min(64);
        if let Ok(path_str) = core::str::from_utf8(&chunk[..split_at])
            && let Ok(next) = builder
                .clone()
                .add_entry(path_str, chunk[split_at..].to_vec())
        {
            builder = next;
        }
        let _ = black_box(idx);
    }
    let _ = black_box(builder);
}

fn verify_dag_invariants(
    resolution: &bevy_abi_mod_manifest::ManifestDagResolution,
    already_loaded: &BTreeMap<String, semver::Version>,
) {
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    let quarantined: BTreeSet<&str> = resolution
        .quarantined_cycles
        .iter()
        .map(String::as_str)
        .collect();

    for manifest in &resolution.load_order {
        assert!(
            !quarantined.contains(manifest.name.as_str()),
            "quarantined cyclic mod '{}' appeared in load_order",
            manifest.name
        );
        for dep in manifest.dependencies.keys() {
            assert!(
                already_loaded.contains_key(dep) || seen.contains(dep.as_str()),
                "mod '{}' scheduled before required dependency '{dep}'",
                manifest.name
            );
        }
        assert!(
            seen.insert(manifest.name.as_str()),
            "duplicate mod '{}' in topological load_order",
            manifest.name
        );
    }
}

fn fuzz_manifests_and_dag(data: &[u8]) {
    // 1. Direct single-document `ModTomlManifest::parse_str` on arbitrary UTF-8.
    let mut parsed_manifests: Vec<ModTomlManifest> = Vec::new();
    if let Ok(full_text) = core::str::from_utf8(data)
        && let Ok(manifest) = ModTomlManifest::parse_str(full_text, "fuzz/mod.toml")
    {
        let _ = black_box(manifest.validate_abi_compatibility());
        parsed_manifests.push(manifest);
    }

    // 2. Multi-manifest batch split by NUL bytes to exercise multi-node Kahn topological sort,
    //    SemVer constraints, duplicate-name diagnostics, and cyclic subgraph quarantine.
    for (idx, chunk) in data
        .split(|&b| b == 0)
        .take(MAX_MANIFEST_CHUNKS)
        .enumerate()
    {
        if let Ok(text) = core::str::from_utf8(chunk)
            && let Ok(manifest) =
                ModTomlManifest::parse_str(text, format!("fuzz/mod_{idx}/mod.toml"))
        {
            let _ = black_box(manifest.validate_abi_compatibility());
            parsed_manifests.push(manifest);
        }
    }

    // 3. Structured synthetic multi-mod TOML generator driven by raw bytes so the fuzzer deeply
    //    explores SemVer ranges, optional deps, diamond DAGs, and cycles even on binary inputs.
    if data.len() >= 6 {
        let count = usize::from(data[0] % 8) + 1;
        let mut pos = 1usize;
        for idx in 0..count {
            if pos >= data.len() {
                break;
            }
            let maj = data[pos] % 3;
            let min = data.get(pos + 1).copied().unwrap_or(0) % 4;
            let pat = data.get(pos + 2).copied().unwrap_or(0) % 4;
            let dep_mask = data.get(pos + 3).copied().unwrap_or(0);
            let opt_mask = data.get(pos + 4).copied().unwrap_or(0);
            pos = pos.saturating_add(5);

            let mut doc = format!(
                "[mod]\nid = \"m{idx}\"\nversion = \"{maj}.{min}.{pat}\"\n[dependencies]\n"
            );
            for target in 0..count {
                if (dep_mask & (1u8 << target)) != 0 {
                    let req = match (dep_mask.wrapping_add(target as u8)) % 4 {
                        0 => ">=0.0.0",
                        1 => "^1.0.0",
                        2 => "<2.0.0",
                        _ => "*",
                    };
                    doc.push_str(&format!("m{target} = \"{req}\"\n"));
                }
            }
            doc.push_str("[optional_dependencies]\n");
            for target in 0..count {
                if (opt_mask & (1u8 << target)) != 0 {
                    doc.push_str(&format!("m{target} = \">=0.0.0\"\n"));
                }
            }
            if let Ok(manifest) =
                ModTomlManifest::parse_str(&doc, format!("fuzz/synth_{idx}/mod.toml"))
            {
                parsed_manifests.push(manifest);
            }
        }
    }

    let mut already_loaded: BTreeMap<String, semver::Version> = BTreeMap::new();
    if let Some(&b) = data.first()
        && b & 1 != 0
    {
        already_loaded.insert("m0".to_owned(), semver::Version::new(1, 0, 0));
    }

    let resolution = resolve_dependency_dag(parsed_manifests.clone(), &already_loaded);
    verify_dag_invariants(&resolution, &already_loaded);

    // 4. Exercise runtime `ModDependencyGraph` registration, edge insertion, cascading order
    //    (including cyclic runtime graphs), and unregistration.
    let mut graph = ModDependencyGraph::new();
    let mut active_ids = BTreeSet::new();
    for (idx, manifest) in parsed_manifests
        .iter()
        .take(MAX_MANIFEST_CHUNKS)
        .enumerate()
    {
        let id = ModId((idx as u64) + 1);
        let deps: Vec<String> = manifest
            .dependencies
            .keys()
            .chain(manifest.optional_dependencies.keys())
            .cloned()
            .collect();
        graph.register_loaded_mod(
            id,
            manifest.name.clone(),
            PathBuf::from(format!("fuzz/{}.dylib", manifest.name)),
            LoadOptions::new().with_shadow_copy(false),
            deps,
        );
        active_ids.insert(id);
    }

    for &root in &active_ids {
        let cascade = graph.cascading_order(root, &active_ids);
        let unique: BTreeSet<ModId> = cascade.iter().copied().collect();
        assert_eq!(
            cascade.len(),
            unique.len(),
            "ModDependencyGraph::cascading_order returned duplicate ModIds"
        );
        assert!(
            unique.contains(&root),
            "cascading_order must always contain root {root:?}"
        );
        for &member in &cascade {
            assert!(
                active_ids.contains(&member),
                "cascading_order returned inactive ModId {member:?}"
            );
        }
    }

    for &id in &active_ids {
        graph.unregister_mod(id);
    }
    graph.clear();

    // 5. On the verified acyclic `resolution.load_order`, `cascading_order(root)` MUST begin with `root`.
    let mut acyclic_graph = ModDependencyGraph::new();
    let mut acyclic_ids = BTreeSet::new();
    for (idx, manifest) in resolution
        .load_order
        .iter()
        .take(MAX_MANIFEST_CHUNKS)
        .enumerate()
    {
        let id = ModId((idx as u64) + 100);
        let deps: Vec<String> = manifest
            .dependencies
            .keys()
            .chain(manifest.optional_dependencies.keys())
            .cloned()
            .collect();
        acyclic_graph.register_loaded_mod(
            id,
            manifest.name.clone(),
            PathBuf::from(format!("fuzz/acyclic/{}.dylib", manifest.name)),
            LoadOptions::new().with_shadow_copy(false),
            deps,
        );
        acyclic_ids.insert(id);
    }
    for &root in &acyclic_ids {
        let cascade = acyclic_graph.cascading_order(root, &acyclic_ids);
        assert_eq!(
            cascade.first().copied(),
            Some(root),
            "acyclic cascading_order must begin with root {root:?}"
        );
    }
}

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_FUZZ_BYTES {
        return;
    }
    fuzz_bampack(data);
    fuzz_manifests_and_dag(data);
});
