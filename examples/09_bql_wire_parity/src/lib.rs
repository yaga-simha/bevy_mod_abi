#![forbid(unsafe_code)]

//! End-to-end `bevy_bql` ABI `0.3.0` ISO C99 vs. Safe Rust Guest Differential Wire Parity Harness.
//!
//! Compiles dual strict ISO C99 (`v1` & `v2`) and `#![forbid(unsafe_code)]` Safe Rust (`v1` & `v2`)
//! native DSOs implementing the full `bevy_bql` runtime contract, drives two independent
//! deterministic Bevy `0.20` `App` / `World` simulations for 500 `FixedUpdate` frames with a live
//! mid-run `v1 -> v2` `migrate_component_column` reload at frame `250`, and asserts bit-identical
//! sorted archetype component column byte hashes across both worlds on every single frame.

use std::env;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
#[cfg(windows)]
use std::sync::OnceLock;

use bevy_abi_mod::prelude::*;
use bevy_abi_mod::{RegisteredComponent, read_entity_pod_bytes};
use bevy_abi_mod_host_sys::loader::{BamSymbolTable, ModNamedObjects};
use bevy_abi_mod_sys::{
    BAM_STORAGE_FLAG_ENTITY_EVENT, BAM_STORAGE_FLAG_EVENT, BAM_STORAGE_FLAG_MESSAGE,
    BAM_STORAGE_FLAG_RESOURCE, ComponentUuid, fnv1a_64_init, fnv1a_64_update_bytes,
    fnv1a_64_update_u64,
};
use bevy_app::{App, FixedUpdate};
use bevy_ecs::entity::Entity;
use bevy_ecs::message::Messages;
use bevy_ecs::prelude::Resource;
use bevy_ecs::world::World;
use bevy_reflect::ReflectRef;
use bevy_reflect::structs::Struct;
use sha2::{Digest, Sha256};
use tempfile::TempDir;

/// Number of root entities spawned in the hierarchy forest.
pub const NUM_ROOTS: usize = 4;
/// Number of branch entities per root.
pub const BRANCHES_PER_ROOT: usize = 4;
/// Number of leaf entities per branch.
pub const LEAVES_PER_BRANCH: usize = 36;
/// Total number of hierarchy nodes (`4 + 16 + 576 = 596`).
pub const TOTAL_HIERARCHY_NODES: usize = NUM_ROOTS
    + (NUM_ROOTS * BRANCHES_PER_ROOT)
    + (NUM_ROOTS * BRANCHES_PER_ROOT * LEAVES_PER_BRANCH);
/// Deterministic seed written into [`SimClock::checksum_seed`].
pub const SIM_CHECKSUM_SEED: u64 = 0x517C_C1B7_2722_0A95;
/// Expected 5-step execution trace proving `SEQUENCE_WEAK` (`.chain_weak()`) + `after_sets` order.
pub const EXPECTED_ORDER_TRACE: u64 = 0x12345;
/// Default frame count for the differential wire parity simulation.
pub const DEFAULT_TOTAL_FRAMES: u32 = 500;
/// Default live `v1 -> v2` migration reload frame boundary.
pub const DEFAULT_MIGRATE_FRAME: u32 = 250;

/// Host-registered deterministic simulation clock resource (`16B`, align `8`).
#[repr(C)]
#[derive(Resource, Clone, Copy, Default, Debug, PartialEq, Eq, ModComponent)]
#[mod_component(name = "bql_parity::SimClock")]
pub struct SimClock {
    /// Zero-based deterministic `FixedUpdate` frame index (`0..500`).
    pub frame: u32,
    /// Active guest schema generation (`1` before reload, `2` after live migration).
    pub schema_gen: u32,
    /// Fixed deterministic simulation seed.
    pub checksum_seed: u64,
}

/// Host-registered per-frame accumulator capturing parallel readonly reductions, `iter_count`,
/// `ATOMIC` transaction counts, `InternedStr` handles, and decoupled schedule execution trace.
#[repr(C)]
#[derive(Resource, Clone, Copy, Default, Debug, PartialEq, Eq, ModComponent)]
#[mod_component(name = "bql_parity::FrameAccumulator")]
pub struct FrameAccumulator {
    /// Wrapping sum reduced concurrently via `par_for_each_chunk_readonly`.
    pub readonly_sum: u64,
    /// Non-destructive match count reported by `iter_count`.
    pub readonly_count: u64,
    /// Total committed `ATOMIC` transactions across the simulation.
    pub committed_tx_count: u32,
    /// Total rolled-back `ATOMIC` transactions across the simulation.
    pub rolled_back_tx_count: u32,
    /// Interned `BamSymbol` raw ID for `"bql::tag::alpha"`.
    pub interned_tag_alpha: u32,
    /// Interned `BamSymbol` raw ID for `"bql::tag::beta"`.
    pub interned_tag_beta: u32,
    /// Bit-packed 5-system execution trace (`0x12345` when `SEQUENCE_WEAK` + `SEQUENCE` hold).
    pub order_trace: u64,
}

/// Compiled and staged dual C99 (`v1`/`v2`) and Safe Rust (`v1`/`v2`) shared libraries.
#[derive(Debug)]
pub struct ParityDsos {
    _stage_dir: TempDir,
    /// Strict ISO C99 v1 shared library path.
    pub c99_v1: PathBuf,
    /// Strict ISO C99 v2 shared library path (compiled with `-DBQL_SCHEMA_V2=1`).
    pub c99_v2: PathBuf,
    /// Safe Rust (`#![forbid(unsafe_code)]`) v1 `.cdylib` path.
    pub rust_v1: PathBuf,
    /// Safe Rust (`#![forbid(unsafe_code)]`) v2 `.cdylib` path (`--features v2`).
    pub rust_v2: PathBuf,
}

/// Deterministic cryptographic + FNV-1a digest of one Bevy `World`'s sorted archetype component
/// column bytes, host resources, `ModRelationGraph`, `BamSymbolTable`, and `ModNamedObjects`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WorldStateDigest {
    /// SHA-256 digest across all sorted archetype component column bytes and wire state.
    pub sha256: [u8; 32],
    /// 64-bit FNV-1a digest across the identical canonical byte stream.
    pub fnv1a64: u64,
    /// Number of non-empty archetypes containing registered POD components.
    pub archetype_count: usize,
    /// Total live entities across hashed archetypes.
    pub entity_count: usize,
    /// Total raw POD component column bytes hashed.
    pub pod_bytes_hashed: usize,
    /// Total participating entities carrying active relation marker components.
    pub relation_participant_count: usize,
}

impl WorldStateDigest {
    /// Formats [`Self::sha256`] as lowercase 64-character hexadecimal.
    pub fn sha256_hex(&self) -> String {
        let mut s = String::with_capacity(64);
        for b in self.sha256 {
            let _ = write!(&mut s, "{b:02x}");
        }
        s
    }
}

/// Summary report returned by [`run_differential_parity`] after all frames pass bit-identical checks.
#[derive(Clone, Debug)]
pub struct ParityReport {
    /// Total deterministic `FixedUpdate` frames executed and verified.
    pub total_frames: u32,
    /// Frame index at which live `v1 -> v2` `migrate_component_column` reload executed.
    pub migrate_at_frame: u32,
    /// Digest after frame `0`.
    pub frame_0_digest: WorldStateDigest,
    /// Digest after last v1 frame (`migrate_at_frame - 1`).
    pub pre_reload_digest: WorldStateDigest,
    /// Digest immediately after live `v1 -> v2` migration (before `migrate_at_frame` ticks).
    pub post_migration_digest: WorldStateDigest,
    /// Digest after final frame (`total_frames - 1`).
    pub final_digest: WorldStateDigest,
    /// Final `FrameAccumulator` snapshot (bit-identical across C99 and Rust worlds).
    pub final_accumulator: FrameAccumulator,
    /// Number of live entities whose `NodeState` rows migrated from 12B v1 -> 16B v2.
    pub migrated_rows: usize,
    /// Total symbols in `BamSymbolTable` after v2 reload.
    pub symbol_count: usize,
    /// Total distinct bit-identical frame hashes verified (must equal `total_frames`).
    pub verified_frames: u32,
}

// ============================================================================
// Portable C99 + Rust Guest DSO Builder
// ============================================================================

fn dylib_prefix_and_ext() -> (&'static str, &'static str) {
    if cfg!(target_os = "windows") {
        ("", "dll")
    } else if cfg!(target_os = "macos") {
        ("lib", "dylib")
    } else {
        ("lib", "so")
    }
}

fn is_msvc_style_driver(tool: &str) -> bool {
    let file_name = Path::new(tool)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(tool);
    let stem = file_name
        .strip_suffix(".exe")
        .or_else(|| file_name.strip_suffix(".EXE"))
        .unwrap_or(file_name);
    stem.eq_ignore_ascii_case("cl") || stem.eq_ignore_ascii_case("clang-cl")
}

fn is_plain_cl_driver(tool: &str) -> bool {
    let file_name = Path::new(tool)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(tool);
    let stem = file_name
        .strip_suffix(".exe")
        .or_else(|| file_name.strip_suffix(".EXE"))
        .unwrap_or(file_name);
    stem.eq_ignore_ascii_case("cl")
}

fn msvc_path_arg(prefix: &str, path: &Path) -> OsString {
    let mut arg = OsString::from(prefix);
    arg.push(path.as_os_str());
    arg
}

#[cfg(windows)]
#[derive(Clone, Debug)]
struct MsvcEnv {
    vars: Vec<(String, String)>,
    cl_exe: Option<PathBuf>,
}

#[cfg(windows)]
fn is_msvc_env_key(key: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    matches!(
        upper.as_str(),
        "PATH"
            | "INCLUDE"
            | "EXTERNAL_INCLUDE"
            | "LIB"
            | "LIBPATH"
            | "PLATFORM"
            | "DEVENVDIR"
            | "VISUALSTUDIOVERSION"
            | "PREFERREDTOOLARCHITECTURE"
            | "UCRTVERSION"
            | "UNIVERSALCRTSDKDIR"
            | "VCINSTALLDIR"
            | "VSINSTALLDIR"
            | "NETFXSDKDIR"
    ) || upper.starts_with("VSCMD_")
        || upper.starts_with("VCTOOLS")
        || upper.starts_with("WINDOWSSDK")
}

#[cfg(windows)]
fn find_exe_in_path_list(exe_name: &str, path_value: &str) -> Option<PathBuf> {
    let has_exe_ext = Path::new(exe_name)
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("exe"));
    for dir in env::split_paths(path_value) {
        let direct = dir.join(exe_name);
        if direct.is_file() {
            return Some(direct);
        }
        if !has_exe_ext {
            let with_exe = dir.join(format!("{exe_name}.exe"));
            if with_exe.is_file() {
                return Some(with_exe);
            }
        }
    }
    None
}

#[cfg(windows)]
fn cached_msvc_env() -> Option<&'static MsvcEnv> {
    use std::os::windows::process::CommandExt;

    static MSVC_ENV: OnceLock<Option<MsvcEnv>> = OnceLock::new();
    MSVC_ENV
        .get_or_init(|| {
            let mut vswhere_bins = Vec::new();
            for env_key in ["ProgramFiles(x86)", "ProgramFiles"] {
                if let Some(base) = env::var_os(env_key) {
                    let cand = PathBuf::from(base)
                        .join("Microsoft Visual Studio")
                        .join("Installer")
                        .join("vswhere.exe");
                    if cand.is_file() && !vswhere_bins.contains(&cand) {
                        vswhere_bins.push(cand);
                    }
                }
            }
            vswhere_bins.push(PathBuf::from("vswhere.exe"));

            let mut roots = Vec::new();
            for vswhere in &vswhere_bins {
                let Ok(out) = Command::new(vswhere)
                    .args([
                        "-latest",
                        "-products",
                        "*",
                        "-requires",
                        "Microsoft.VisualStudio.Component.VC.Tools.x86.x64",
                        "-property",
                        "installationPath",
                    ])
                    .output()
                else {
                    continue;
                };
                if out.status.success() {
                    for line in String::from_utf8_lossy(&out.stdout).lines() {
                        let p = PathBuf::from(line.trim());
                        if p.is_dir() && !roots.contains(&p) {
                            roots.push(p);
                        }
                    }
                    if !roots.is_empty() {
                        break;
                    }
                }
            }

            let comspec = env::var_os("COMSPEC").unwrap_or_else(|| OsString::from("cmd.exe"));
            for root in roots {
                let bat = root
                    .join("VC")
                    .join("Auxiliary")
                    .join("Build")
                    .join("vcvars64.bat");
                if !bat.is_file() {
                    continue;
                }
                let Ok(output) = Command::new(&comspec)
                    .env("VSCMD_SKIP_SENDTELEMETRY", "1")
                    .env("BAM_VCVARS_BAT", &bat)
                    .args(["/D", "/S", "/C"])
                    .raw_arg("\"call \"%BAM_VCVARS_BAT%\" >NUL 2>&1 && set\"")
                    .output()
                else {
                    continue;
                };
                if !output.status.success() {
                    continue;
                }
                let mut vars: Vec<(String, String)> = Vec::new();
                for raw_line in String::from_utf8_lossy(&output.stdout).lines() {
                    let line = raw_line.trim_end_matches('\r');
                    if line.is_empty() || line.starts_with('=') {
                        continue;
                    }
                    let Some((raw_key, value)) = line.split_once('=') else {
                        continue;
                    };
                    let key = raw_key.trim();
                    if key.is_empty() || !is_msvc_env_key(key) {
                        continue;
                    }
                    vars.push((key.to_owned(), value.to_owned()));
                }
                let Some(path_val) = vars
                    .iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case("PATH"))
                    .map(|(_, v)| v.as_str())
                else {
                    continue;
                };
                let has_inc = vars
                    .iter()
                    .any(|(k, v)| k.eq_ignore_ascii_case("INCLUDE") && !v.is_empty());
                let has_lib = vars
                    .iter()
                    .any(|(k, v)| k.eq_ignore_ascii_case("LIB") && !v.is_empty());
                if has_inc && has_lib {
                    let cl_exe = find_exe_in_path_list("cl.exe", path_val);
                    return Some(MsvcEnv { vars, cl_exe });
                }
            }
            None
        })
        .as_ref()
}

fn msvc_command(program: &str) -> Command {
    #[cfg(windows)]
    if is_msvc_style_driver(program) {
        let has_include = env::var_os("INCLUDE").is_some_and(|v| !v.is_empty());
        let has_lib = env::var_os("LIB").is_some_and(|v| !v.is_empty());
        let explicit_path = Path::new(program).components().count() > 1;
        let direct_driver_ok = if !is_plain_cl_driver(program) {
            true
        } else if explicit_path {
            Path::new(program).is_file()
        } else {
            env::var("PATH")
                .ok()
                .and_then(|path_val| find_exe_in_path_list(program, &path_val))
                .is_some()
        };

        if !(has_include && has_lib && direct_driver_ok)
            && let Some(msvc_env) = cached_msvc_env()
        {
            let mut cmd = if is_plain_cl_driver(program) && !explicit_path {
                match &msvc_env.cl_exe {
                    Some(cl_path) => Command::new(cl_path),
                    None => Command::new(program),
                }
            } else {
                Command::new(program)
            };
            for (k, v) in &msvc_env.vars {
                cmd.env(k, v);
            }
            return cmd;
        }
    }
    Command::new(program)
}

fn find_c_compiler() -> String {
    if let Ok(cc) = env::var("CC")
        && !cc.trim().is_empty()
    {
        return cc.trim().to_owned();
    }
    #[cfg(target_os = "windows")]
    let candidates = ["clang", "clang-cl", "cl.exe", "cl", "gcc", "cc"];
    #[cfg(not(target_os = "windows"))]
    let candidates = ["clang", "gcc", "cc"];
    for cand in candidates {
        let is_plain_cl = is_plain_cl_driver(cand);
        let res = if is_plain_cl {
            msvc_command(cand).output()
        } else {
            msvc_command(cand).arg("--version").output()
        };
        if let Ok(out) = res
            && (out.status.success() || (is_plain_cl && !out.stderr.is_empty()))
        {
            return cand.to_owned();
        }
    }
    if cfg!(target_os = "windows") {
        "gcc".to_owned()
    } else {
        "cc".to_owned()
    }
}

fn compile_c99_dso(
    cc: &str,
    src_path: &Path,
    include_dir: &Path,
    stage_dir: &Path,
    out_path: &Path,
    v2: bool,
) {
    let tag = if v2 { "v2" } else { "v1" };
    let obj_path = stage_dir.join(format!("bql_parity_c_{tag}.obj"));
    let implib_path = stage_dir.join(format!("bql_parity_c_{tag}.lib"));

    let mut cmd = msvc_command(cc);
    if is_msvc_style_driver(cc) {
        cmd.args(["/nologo", "/LD", "/TC", "/std:c11", "/W4", "/WX", "/O2"])
            .arg("/I")
            .arg(include_dir)
            .arg(msvc_path_arg("/Fo:", &obj_path))
            .arg(msvc_path_arg("/Fe:", out_path));
        if v2 {
            cmd.arg("/DBQL_SCHEMA_V2=1");
        }
        cmd.arg(src_path)
            .arg("/link")
            .arg(msvc_path_arg("/IMPLIB:", &implib_path));
    } else {
        cmd.args([
            "-std=c99",
            "-Wall",
            "-Wextra",
            "-Wpedantic",
            "-Werror",
            "-O2",
            "-shared",
        ])
        .arg(format!("-I{}", include_dir.display()));
        if !cfg!(target_os = "windows") {
            cmd.arg("-fPIC");
        }
        if v2 {
            cmd.arg("-DBQL_SCHEMA_V2=1");
        }
        cmd.arg(src_path).arg("-o").arg(out_path);
    }

    let output = cmd
        .output()
        .unwrap_or_else(|e| panic!("failed to invoke C99 compiler `{cc}`: {e}"));
    assert!(
        output.status.success(),
        "C99 {tag} compilation failed (`{cc}`):\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn compile_rust_dso(rust_manifest: &Path, cargo_target_dir: &Path, out_path: &Path, v2: bool) {
    let (prefix, ext) = dylib_prefix_and_ext();
    let cargo = env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let mut cmd = Command::new(&cargo);
    cmd.arg("build")
        .arg("--manifest-path")
        .arg(rust_manifest)
        .arg("--release")
        .arg("--target-dir")
        .arg(cargo_target_dir);
    if v2 {
        cmd.args(["--features", "v2"]);
    }
    let output = cmd
        .output()
        .unwrap_or_else(|e| panic!("failed to invoke cargo for rust_mod: {e}"));
    assert!(
        output.status.success(),
        "Rust guest {} build failed:\nstdout:\n{}\nstderr:\n{}",
        if v2 { "v2" } else { "v1" },
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let built = cargo_target_dir
        .join("release")
        .join(format!("{prefix}bql_parity_rust_mod.{ext}"));
    assert!(
        built.is_file(),
        "expected built Rust guest cdylib at {}",
        built.display()
    );
    fs::copy(&built, out_path).unwrap_or_else(|e| {
        panic!(
            "failed to stage Rust guest cdylib from {} to {}: {e}",
            built.display(),
            out_path.display()
        )
    });
}

/// Compiles and stages all 4 guest DSOs (C99 `v1`/`v2` and Safe Rust `v1`/`v2`) into an isolated
/// temporary directory.
pub fn build_parity_dsos() -> ParityDsos {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let include_dir = manifest_dir.join("../../include");
    let c_src = manifest_dir.join("c_mod/bql_parity_mod.c");
    let rust_manifest = manifest_dir.join("rust_mod/Cargo.toml");
    let (prefix, ext) = dylib_prefix_and_ext();

    let stage_dir = tempfile::tempdir().expect("create temporary DSO staging directory");
    let c99_v1 = stage_dir
        .path()
        .join(format!("{prefix}bql_parity_c_v1.{ext}"));
    let c99_v2 = stage_dir
        .path()
        .join(format!("{prefix}bql_parity_c_v2.{ext}"));
    let rust_v1 = stage_dir
        .path()
        .join(format!("{prefix}bql_parity_rust_v1.{ext}"));
    let rust_v2 = stage_dir
        .path()
        .join(format!("{prefix}bql_parity_rust_v2.{ext}"));

    let cc = find_c_compiler();
    compile_c99_dso(&cc, &c_src, &include_dir, stage_dir.path(), &c99_v1, false);
    compile_c99_dso(&cc, &c_src, &include_dir, stage_dir.path(), &c99_v2, true);

    let cargo_target_dir = stage_dir.path().join("rust_target");
    compile_rust_dso(&rust_manifest, &cargo_target_dir, &rust_v1, false);
    compile_rust_dso(&rust_manifest, &cargo_target_dir, &rust_v2, true);

    ParityDsos {
        _stage_dir: stage_dir,
        c99_v1,
        c99_v2,
        rust_v1,
        rust_v2,
    }
}

// ============================================================================
// Deterministic Archetype Column Byte Hasher & Diagnostics (`forbid(unsafe)`)
// ============================================================================

struct DualHasher {
    sha: Sha256,
    fnv: u64,
}

impl DualHasher {
    fn new() -> Self {
        Self {
            sha: Sha256::new(),
            fnv: fnv1a_64_init(),
        }
    }

    fn update_bytes(&mut self, bytes: &[u8]) {
        self.sha.update(bytes);
        self.fnv = fnv1a_64_update_bytes(self.fnv, bytes);
    }

    fn update_u64(&mut self, val: u64) {
        self.sha.update(val.to_le_bytes());
        self.fnv = fnv1a_64_update_u64(self.fnv, val);
    }

    fn update_str(&mut self, s: &str) {
        self.update_u64(s.len() as u64);
        self.update_bytes(s.as_bytes());
    }

    fn finish(self) -> ([u8; 32], u64) {
        let digest = self.sha.finalize();
        let mut sha256 = [0u8; 32];
        sha256.copy_from_slice(&digest);
        (sha256, self.fnv)
    }
}

fn is_hashable_entity_pod(entry: &RegisteredComponent) -> bool {
    let non_entity_mask = BAM_STORAGE_FLAG_RESOURCE
        | BAM_STORAGE_FLAG_MESSAGE
        | BAM_STORAGE_FLAG_EVENT
        | BAM_STORAGE_FLAG_ENTITY_EVENT;
    entry.is_abi_pod && (entry.storage_flags & non_entity_mask) == 0
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct ComponentSig {
    name: String,
    uuid_bytes: [u8; 16],
    schema_fingerprint: u64,
    size: usize,
    align: usize,
    handle_raw: u32,
}

/// Computes a deterministic SHA-256 + FNV-1a 64-bit hash over ALL sorted archetype component column
/// bytes, host resources ([`SimClock`], [`FrameAccumulator`]), `ModRelationGraph` relation
/// participants, [`BamSymbolTable`], and [`ModNamedObjects`].
pub fn hash_world_state(world: &World) -> WorldStateDigest {
    let mut hasher = DualHasher::new();

    let registry = world.resource::<ModRegistry>();
    let snap = registry.snapshot();

    let mut pod_components: Vec<&RegisteredComponent> = snap
        .entries()
        .iter()
        .filter(|e| is_hashable_entity_pod(e))
        .collect();
    pod_components.sort_by(|a, b| {
        a.name
            .as_ref()
            .cmp(b.name.as_ref())
            .then(a.handle.0.cmp(&b.handle.0))
    });

    // 1. Collect and canonically sort all non-empty archetypes carrying registered POD components.
    struct ArchRecord<'a> {
        sig: Vec<ComponentSig>,
        components: Vec<&'a RegisteredComponent>,
        entities: Vec<Entity>,
    }

    let mut arch_records: Vec<ArchRecord<'_>> = Vec::new();
    for arch in world.archetypes().iter() {
        if arch.is_empty() {
            continue;
        }
        let mut comps = Vec::new();
        let mut sig = Vec::new();
        for &entry in &pod_components {
            if arch.contains(entry.component_id) {
                comps.push(entry);
                sig.push(ComponentSig {
                    name: entry.name.to_string(),
                    uuid_bytes: entry.uuid.bytes,
                    schema_fingerprint: entry.schema_fingerprint,
                    size: entry.size,
                    align: entry.align,
                    handle_raw: entry.handle.0,
                });
            }
        }
        if comps.is_empty() {
            continue;
        }
        let mut entities: Vec<Entity> = arch.entities().iter().map(|ae| ae.id()).collect();
        entities.sort_unstable_by_key(|e| e.to_bits());
        arch_records.push(ArchRecord {
            sig,
            components: comps,
            entities,
        });
    }
    arch_records.sort_by(|a, b| {
        a.sig
            .cmp(&b.sig)
            .then_with(|| a.entities.len().cmp(&b.entities.len()))
            .then_with(|| {
                a.entities
                    .first()
                    .map(|e| e.to_bits())
                    .cmp(&b.entities.first().map(|e| e.to_bits()))
            })
    });

    hasher.update_str("ARCHETYPES");
    hasher.update_u64(arch_records.len() as u64);

    let mut total_entities = 0usize;
    let mut total_pod_bytes = 0usize;

    for record in &arch_records {
        total_entities += record.entities.len();
        hasher.update_u64(record.sig.len() as u64);
        for s in &record.sig {
            hasher.update_str(&s.name);
            hasher.update_bytes(&s.uuid_bytes);
            hasher.update_u64(s.schema_fingerprint);
            hasher.update_u64(s.size as u64);
            hasher.update_u64(s.align as u64);
        }
        hasher.update_u64(record.entities.len() as u64);

        for &entry in &record.components {
            hasher.update_str(&entry.name);
            for &entity in &record.entities {
                hasher.update_u64(entity.to_bits());
                let bytes =
                    read_entity_pod_bytes(world, entity, entry.handle).unwrap_or_else(|e| {
                        panic!(
                            "read_entity_pod_bytes failed for {} on {:?}: {:?}",
                            entry.name, entity, e
                        )
                    });
                assert_eq!(
                    bytes.len(),
                    entry.size,
                    "POD byte length mismatch for {}",
                    entry.name
                );
                total_pod_bytes += bytes.len();
                hasher.update_bytes(&bytes);
            }
        }
    }

    // 2. Hash host resources (`SimClock` and `FrameAccumulator`).
    let clock = world.resource::<SimClock>();
    hasher.update_str("SIM_CLOCK");
    hasher.update_u64(u64::from(clock.frame));
    hasher.update_u64(u64::from(clock.schema_gen));
    hasher.update_u64(clock.checksum_seed);

    let acc = world.resource::<FrameAccumulator>();
    hasher.update_str("FRAME_ACCUMULATOR");
    hasher.update_u64(acc.readonly_sum);
    hasher.update_u64(acc.readonly_count);
    hasher.update_u64(u64::from(acc.committed_tx_count));
    hasher.update_u64(u64::from(acc.rolled_back_tx_count));
    hasher.update_u64(u64::from(acc.interned_tag_alpha));
    hasher.update_u64(u64::from(acc.interned_tag_beta));
    hasher.update_u64(acc.order_trace);

    // 3. Hash active relationship marker participants (`bql_parity::OwnedBy` sparse marker) in sorted order.
    let mut total_relation_participants = 0usize;
    hasher.update_str("RELATIONS");
    if let Some(rel_entry) = snap.get_by_name("bql_parity::OwnedBy") {
        let mut participants: Vec<u64> = world
            .archetypes()
            .iter()
            .filter(|a| a.contains(rel_entry.component_id))
            .flat_map(|a| a.entities().iter().map(|ae| ae.id().to_bits()))
            .collect();
        participants.sort_unstable();
        total_relation_participants = participants.len();
        hasher.update_str(&rel_entry.name);
        hasher.update_bytes(&rel_entry.uuid.bytes);
        hasher.update_u64(participants.len() as u64);
        for bits in participants {
            hasher.update_u64(bits);
        }
    }

    // 4. Hash `BamSymbolTable` monotonic entries (`0..len`).
    hasher.update_str("SYMBOLS");
    if let Some(sym_table) = world.get_resource::<BamSymbolTable>() {
        let len = sym_table.len();
        hasher.update_u64(len as u64);
        for raw in 0..len {
            let sym = BamSymbol::from_raw(raw as u32);
            let s = sym_table.resolve_symbol_str(sym).unwrap_or("");
            hasher.update_u64(raw as u64);
            hasher.update_str(s);
        }
    }

    // 5. Hash `ModNamedObjects` persistent entity bindings.
    hasher.update_str("NAMED_OBJECTS");
    if let Some(named_objs) = world.get_resource::<ModNamedObjects>() {
        let mut entries = named_objs.entries();
        entries.sort_by(|a, b| a.0.as_ref().cmp(b.0.as_ref()));
        hasher.update_u64(entries.len() as u64);
        for (name, uuid, entity) in entries {
            hasher.update_str(&name);
            hasher.update_bytes(&uuid.bytes);
            hasher.update_u64(entity.to_bits());
        }
    }

    let (sha256, fnv1a64) = hasher.finish();
    WorldStateDigest {
        sha256,
        fnv1a64,
        archetype_count: arch_records.len(),
        entity_count: total_entities,
        pod_bytes_hashed: total_pod_bytes,
        relation_participant_count: total_relation_participants,
    }
}

/// Produces a human-readable diagnostic diff when C99 and Rust worlds diverge.
pub fn diagnose_world_divergence(c_world: &World, r_world: &World) -> String {
    let mut out = String::new();
    let c_acc = c_world.resource::<FrameAccumulator>();
    let r_acc = r_world.resource::<FrameAccumulator>();
    if c_acc != r_acc {
        let _ = writeln!(
            &mut out,
            "FrameAccumulator mismatch:\n  C99 : {c_acc:?}\n  Rust: {r_acc:?}"
        );
    }

    let c_reg = c_world.resource::<ModRegistry>().snapshot();
    let r_reg = r_world.resource::<ModRegistry>().snapshot();
    for comp_name in [
        "bql_parity::NodeState",
        "bql_parity::RootAura",
        "bql_parity::WorldMetric",
        "bql_parity::TxToken",
        bevy_abi_mod_sys::BAM_CHILD_OF_NAME,
    ] {
        let Some(ce) = c_reg.get_by_name(comp_name) else {
            let _ = writeln!(&mut out, "C99 missing component {comp_name}");
            continue;
        };
        let Some(re) = r_reg.get_by_name(comp_name) else {
            let _ = writeln!(&mut out, "Rust missing component {comp_name}");
            continue;
        };
        if ce.uuid != re.uuid
            || ce.schema_fingerprint != re.schema_fingerprint
            || ce.size != re.size
        {
            let _ = writeln!(
                &mut out,
                "Metadata mismatch for {comp_name}: C99=({:?}, 0x{:016x}, {}) vs Rust=({:?}, 0x{:016x}, {})",
                ce.uuid, ce.schema_fingerprint, ce.size, re.uuid, re.schema_fingerprint, re.size
            );
            continue;
        }
        let mut c_ents: Vec<Entity> = c_world
            .archetypes()
            .iter()
            .filter(|a| a.contains(ce.component_id))
            .flat_map(|a| a.entities().iter().map(|ae| ae.id()))
            .collect();
        let mut r_ents: Vec<Entity> = r_world
            .archetypes()
            .iter()
            .filter(|a| a.contains(re.component_id))
            .flat_map(|a| a.entities().iter().map(|ae| ae.id()))
            .collect();
        c_ents.sort_unstable_by_key(|e| e.to_bits());
        r_ents.sort_unstable_by_key(|e| e.to_bits());
        if c_ents != r_ents {
            let _ = writeln!(
                &mut out,
                "Entity set mismatch for {comp_name}: C99 len={} vs Rust len={}",
                c_ents.len(),
                r_ents.len()
            );
            continue;
        }
        for ent in c_ents {
            let cb = read_entity_pod_bytes(c_world, ent, ce.handle);
            let rb = read_entity_pod_bytes(r_world, ent, re.handle);
            if cb != rb {
                let _ = writeln!(
                    &mut out,
                    "Bytes mismatch for {comp_name} on {ent:?}:\n  C99 : {cb:?}\n  Rust: {rb:?}"
                );
                break;
            }
        }
    }
    if out.is_empty() {
        out.push_str("divergence in relation graph, symbols, named objects, or archetype layout");
    }
    out
}

// ============================================================================
// Host World Setup, Reflection & Migration Parity Verification
// ============================================================================

/// Creates a fresh deterministic headless Bevy `App` configured with `AbiModPlugin`,
/// `ModReflectPlugin`, `FixedUpdate`, and the shared [`SimClock`] / [`FrameAccumulator`] resources.
pub fn build_parity_app() -> App {
    let mut app = App::new();
    app.add_plugins(AbiModPlugin::with_error_policy(
        ModErrorPolicy::ReportToBevy,
    ));
    app.add_plugins(ModReflectPlugin);
    app.init_schedule(FixedUpdate);

    app.register_mod_resource::<SimClock>()
        .expect("register SimClock resource");
    app.register_mod_resource::<FrameAccumulator>()
        .expect("register FrameAccumulator resource");

    app.insert_resource(SimClock {
        frame: 0,
        schema_gen: 1,
        checksum_seed: SIM_CHECKSUM_SEED,
    });
    app.insert_resource(FrameAccumulator::default());
    app
}

/// Asserts that C99 and Safe Rust worlds registered identical `ComponentUuid`, `schema_fingerprint`,
/// `(size, align)`, and declarative `BamFieldDesc[]` schemas for all 4 guest POD components.
pub fn assert_wire_schema_metadata_parity(c_world: &World, r_world: &World, expected_v2: bool) {
    let c_reg = c_world.resource::<ModRegistry>().snapshot();
    let r_reg = r_world.resource::<ModRegistry>().snapshot();

    let expected_node_size = if expected_v2 { 16 } else { 12 };
    let expected_node_fields = if expected_v2 { 4 } else { 3 };

    let specs = [
        (
            "bql_parity::NodeState",
            expected_node_size,
            4usize,
            expected_node_fields,
        ),
        ("bql_parity::RootAura", 8usize, 4usize, 2usize),
        ("bql_parity::WorldMetric", 16usize, 4usize, 4usize),
        ("bql_parity::TxToken", 8usize, 4usize, 2usize),
    ];

    for (name, exp_size, exp_align, exp_fields) in specs {
        let ce = c_reg
            .get_by_name(name)
            .unwrap_or_else(|| panic!("C99 world missing component {name}"));
        let re = r_reg
            .get_by_name(name)
            .unwrap_or_else(|| panic!("Rust world missing component {name}"));

        assert_eq!(ce.handle, re.handle, "handle mismatch for {name}");
        assert_eq!(ce.uuid, re.uuid, "UUID mismatch for {name}");
        assert_ne!(
            ce.schema_fingerprint, 0,
            "schema_fingerprint must be non-zero for {name}"
        );
        assert_eq!(
            ce.schema_fingerprint, re.schema_fingerprint,
            "schema_fingerprint mismatch for {name}: C99=0x{:016x} vs Rust=0x{:016x}",
            ce.schema_fingerprint, re.schema_fingerprint
        );
        assert_eq!(ce.size, exp_size, "C99 size mismatch for {name}");
        assert_eq!(re.size, exp_size, "Rust size mismatch for {name}");
        assert_eq!(ce.align, exp_align, "C99 align mismatch for {name}");
        assert_eq!(re.align, exp_align, "Rust align mismatch for {name}");
        assert_eq!(
            ce.fields.len(),
            exp_fields,
            "C99 field_count mismatch for {name}"
        );
        assert_eq!(
            re.fields.len(),
            exp_fields,
            "Rust field_count mismatch for {name}"
        );
    }
}

/// Synchronizes `ModReflectPlugin` declarative `BamFieldDesc[]` schemas on both worlds and verifies
/// live `bevy_reflect` (`ReflectFromPtr` + `DynamicStruct`) inspection produces identical fields
/// and values on the bound named root entity.
pub fn assert_reflect_parity(c_world: &mut World, r_world: &mut World, expected_v2: bool) {
    let c_synced = c_world.sync_bam_field_reflection();
    let r_synced = r_world.sync_bam_field_reflection();
    assert!(
        c_synced >= 4,
        "C99 expected >= 4 reflected schemas, got {c_synced}"
    );
    assert!(
        r_synced >= 4,
        "Rust expected >= 4 reflected schemas, got {r_synced}"
    );

    let root_c = c_world
        .resource::<ModNamedObjects>()
        .get_alive(c_world, "bql_parity::root_config")
        .expect("C99 named root_config alive");
    let root_r = r_world
        .resource::<ModNamedObjects>()
        .get_alive(r_world, "bql_parity::root_config")
        .expect("Rust named root_config alive");
    assert_eq!(
        root_c, root_r,
        "named root_config entity must match across worlds"
    );

    let node_h_c = c_world
        .resource::<ModRegistry>()
        .get_by_name("bql_parity::NodeState")
        .expect("C99 NodeState")
        .handle;
    let node_h_r = r_world
        .resource::<ModRegistry>()
        .get_by_name("bql_parity::NodeState")
        .expect("Rust NodeState")
        .handle;
    assert_eq!(node_h_c, node_h_r);

    // Inspect via `ReflectFromPtr` (`reflect_mod_component`) directly through `bevy_reflect`.
    let c_node_ref = c_world
        .reflect_mod_component(root_c, node_h_c)
        .expect("C99 reflect_mod_component NodeState");
    let r_node_ref = r_world
        .reflect_mod_component(root_r, node_h_r)
        .expect("Rust reflect_mod_component NodeState");

    let (ReflectRef::Struct(cs), ReflectRef::Struct(rs)) =
        (c_node_ref.reflect_ref(), r_node_ref.reflect_ref())
    else {
        panic!("expected Struct reflection for NodeState");
    };
    let expected_field_names: &[&str] = if expected_v2 {
        &["node_id", "local_val", "tag_sym", "boost"]
    } else {
        &["node_id", "local_val", "tag_sym"]
    };
    assert_eq!(cs.field_len(), expected_field_names.len());
    assert_eq!(rs.field_len(), expected_field_names.len());
    for (idx, &fname) in expected_field_names.iter().enumerate() {
        assert_eq!(cs.name_at(idx), Some(fname));
        assert_eq!(rs.name_at(idx), Some(fname));
    }

    let c_node_dyn = c_world
        .export_bam_fields_dynamic_struct(root_c, node_h_c)
        .expect("C99 NodeState DynamicStruct");
    let r_node_dyn = r_world
        .export_bam_fields_dynamic_struct(root_r, node_h_r)
        .expect("Rust NodeState DynamicStruct");
    assert_eq!(
        c_node_dyn
            .field("node_id")
            .and_then(|f| f.try_downcast_ref::<u32>()),
        Some(&0u32)
    );
    assert_eq!(
        c_node_dyn
            .field("node_id")
            .and_then(|f| f.try_downcast_ref::<u32>()),
        r_node_dyn
            .field("node_id")
            .and_then(|f| f.try_downcast_ref::<u32>())
    );
    assert_eq!(
        c_node_dyn
            .field("local_val")
            .and_then(|f| f.try_downcast_ref::<i32>()),
        r_node_dyn
            .field("local_val")
            .and_then(|f| f.try_downcast_ref::<i32>())
    );
    assert_eq!(
        c_node_dyn
            .field("tag_sym")
            .and_then(|f| f.try_downcast_ref::<u32>()),
        r_node_dyn
            .field("tag_sym")
            .and_then(|f| f.try_downcast_ref::<u32>())
    );
    if expected_v2 {
        let c_boost = c_node_dyn
            .field("boost")
            .and_then(|f| f.try_downcast_ref::<i32>())
            .expect("C99 v2 NodeState must expose reflected `boost: i32`");
        let r_boost = r_node_dyn
            .field("boost")
            .and_then(|f| f.try_downcast_ref::<i32>())
            .expect("Rust v2 NodeState must expose reflected `boost: i32`");
        assert_eq!(
            c_boost, r_boost,
            "reflected v2 `boost` mismatch on root_config"
        );
    }

    // Also reflect `WorldMetric` (including `f32` `energy` field).
    let wm_h_c = c_world
        .resource::<ModRegistry>()
        .get_by_name("bql_parity::WorldMetric")
        .expect("C99 WorldMetric")
        .handle;
    let wm_h_r = r_world
        .resource::<ModRegistry>()
        .get_by_name("bql_parity::WorldMetric")
        .expect("Rust WorldMetric")
        .handle;
    let c_wm_dyn = c_world
        .export_bam_fields_dynamic_struct(root_c, wm_h_c)
        .expect("C99 WorldMetric DynamicStruct");
    let r_wm_dyn = r_world
        .export_bam_fields_dynamic_struct(root_r, wm_h_r)
        .expect("Rust WorldMetric DynamicStruct");
    assert_eq!(
        c_wm_dyn
            .field("world_val")
            .and_then(|f| f.try_downcast_ref::<i32>()),
        r_wm_dyn
            .field("world_val")
            .and_then(|f| f.try_downcast_ref::<i32>())
    );
    assert_eq!(
        c_wm_dyn
            .field("cascade_depth")
            .and_then(|f| f.try_downcast_ref::<i32>()),
        Some(&0i32)
    );
    let c_energy = c_wm_dyn
        .field("energy")
        .and_then(|f| f.try_downcast_ref::<f32>())
        .expect("C99 WorldMetric energy");
    let r_energy = r_wm_dyn
        .field("energy")
        .and_then(|f| f.try_downcast_ref::<f32>())
        .expect("Rust WorldMetric energy");
    assert_eq!(
        c_energy.to_bits(),
        r_energy.to_bits(),
        "reflected f32 energy bits must match"
    );
}

fn snapshot_v1_node_rows(world: &World) -> Vec<(Entity, u32, i32, u32)> {
    let entry = world
        .resource::<ModRegistry>()
        .get_by_name("bql_parity::NodeState")
        .expect("NodeState registered")
        .clone();
    assert_eq!(
        entry.size, 12,
        "expected 12-byte v1 NodeState before migration"
    );
    let mut rows = Vec::new();
    for arch in world.archetypes().iter() {
        if !arch.contains(entry.component_id) {
            continue;
        }
        for ae in arch.entities() {
            let ent = ae.id();
            let bytes = read_entity_pod_bytes(world, ent, entry.handle).expect("read v1 row");
            let node_id = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
            let local_val = i32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
            let tag_sym = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
            rows.push((ent, node_id, local_val, tag_sym));
        }
    }
    rows.sort_unstable_by_key(|r| r.0.to_bits());
    rows
}

fn assert_migrated_v2_rows(world: &World, expected_v1: &[(Entity, u32, i32, u32)], label: &str) {
    let entry = world
        .resource::<ModRegistry>()
        .get_by_name("bql_parity::NodeState")
        .expect("NodeState registered after migration")
        .clone();
    assert_eq!(
        entry.size, 16,
        "{label}: expected 16-byte v2 NodeState after migration"
    );
    for &(ent, node_id, local_val, tag_sym) in expected_v1 {
        let bytes = read_entity_pod_bytes(world, ent, entry.handle)
            .unwrap_or_else(|e| panic!("{label}: failed to read migrated row on {ent:?}: {e:?}"));
        assert_eq!(bytes.len(), 16);
        let got_id = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        let got_val = i32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
        let got_tag = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
        let got_boost = i32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]);
        let exp_boost = (local_val / 4) + ((node_id % 64) as i32) * 3 + 7;
        assert_eq!(
            got_id, node_id,
            "{label}: migrated node_id mismatch on {ent:?}"
        );
        assert_eq!(
            got_val, local_val,
            "{label}: migrated local_val mismatch on {ent:?}"
        );
        assert_eq!(
            got_tag, tag_sym,
            "{label}: migrated tag_sym mismatch on {ent:?}"
        );
        assert_eq!(
            got_boost, exp_boost,
            "{label}: migrated boost mismatch on {ent:?}"
        );
    }
}

/// Runs two independent deterministic Bevy `0.20` `App` simulations (C99 vs. Safe Rust) for
/// `total_frames` deterministic `FixedUpdate` ticks, performing a live `v1 -> v2`
/// `migrate_component_column` reload at `migrate_at_frame` and asserting bit-identical archetype
/// component column byte hashes on every single frame.
pub fn run_differential_parity(
    dsos: &ParityDsos,
    total_frames: u32,
    migrate_at_frame: u32,
) -> ParityReport {
    assert!(total_frames >= 2, "total_frames must be >= 2");
    assert!(
        migrate_at_frame > 0 && migrate_at_frame < total_frames,
        "migrate_at_frame ({migrate_at_frame}) must be strictly inside 1..{total_frames}"
    );

    let mut c99_app = build_parity_app();
    let mut rust_app = build_parity_app();

    let load_opts = LoadOptions {
        shadow_copy: true,
        strict: true,
        ..Default::default()
    };

    let mut c99_mod_id = c99_app
        .load_mod_with(&dsos.c99_v1, load_opts.clone())
        .expect("load C99 v1 guest DSO");
    let mut rust_mod_id = rust_app
        .load_mod_with(&dsos.rust_v1, load_opts.clone())
        .expect("load Safe Rust v1 guest DSO");

    // Verify v1 wire schema metadata parity immediately after init.
    assert_wire_schema_metadata_parity(c99_app.world(), rust_app.world(), false);

    let mut frame_0_digest = None;
    let mut pre_reload_digest = None;
    let mut post_migration_digest = None;
    let mut final_digest = None;
    let mut migrated_rows = 0usize;
    let mut verified_frames = 0u32;

    for frame in 0..total_frames {
        // Perform live v1 -> v2 schema migration reload right before `migrate_at_frame`.
        if frame == migrate_at_frame {
            let pre_root_c = c99_app
                .world()
                .resource::<ModNamedObjects>()
                .get_alive(c99_app.world(), "bql_parity::root_config")
                .expect("C99 named root_config alive before reload");
            let pre_root_r = rust_app
                .world()
                .resource::<ModNamedObjects>()
                .get_alive(rust_app.world(), "bql_parity::root_config")
                .expect("Rust named root_config alive before reload");
            assert_eq!(pre_root_c, pre_root_r);
            assert_eq!(
                c99_app.world().resource::<ModNamedObjects>().get_by_uuid(
                    ComponentUuid::from_parts("bql_parity::root_config", 0, 0, 0)
                ),
                Some(pre_root_c)
            );

            let pre_node_handle_c = c99_app
                .world()
                .resource::<ModRegistry>()
                .get_by_name("bql_parity::NodeState")
                .expect("C99 NodeState before reload")
                .handle;
            let pre_node_handle_r = rust_app
                .world()
                .resource::<ModRegistry>()
                .get_by_name("bql_parity::NodeState")
                .expect("Rust NodeState before reload")
                .handle;
            assert_eq!(pre_node_handle_c, pre_node_handle_r);

            let pre_v1_rows_c = snapshot_v1_node_rows(c99_app.world());
            let pre_v1_rows_r = snapshot_v1_node_rows(rust_app.world());
            assert_eq!(pre_v1_rows_c.len(), TOTAL_HIERARCHY_NODES);
            assert_eq!(pre_v1_rows_c, pre_v1_rows_r);
            migrated_rows = pre_v1_rows_c.len();

            let pre_syms: Vec<(String, BamSymbol)> = [
                "bql::tag::root",
                "bql::tag::branch",
                "bql::tag::leaf",
                "bql::tag::alpha",
                "bql::tag::beta",
                "bql::tx::committed",
            ]
            .into_iter()
            .map(|name| {
                let s = c99_app
                    .world()
                    .resource::<BamSymbolTable>()
                    .lookup(name)
                    .unwrap_or_else(|| panic!("missing pre-reload symbol {name}"));
                (name.to_owned(), s)
            })
            .collect();

            // Live `reload_mod_from` v1 -> v2 invoking `migrate_component_column` in both worlds!
            c99_mod_id = c99_app
                .world_mut()
                .reload_mod_from(c99_mod_id, &dsos.c99_v2, load_opts.clone())
                .expect("reload C99 v1 -> v2");
            rust_mod_id = rust_app
                .world_mut()
                .reload_mod_from(rust_mod_id, &dsos.rust_v2, load_opts.clone())
                .expect("reload Safe Rust v1 -> v2");

            // Verify v2 schema metadata, same `BamComponent` handle, named object survival,
            // symbol stability, and exact migrated v2 column data!
            assert_wire_schema_metadata_parity(c99_app.world(), rust_app.world(), true);

            let post_node_handle_c = c99_app
                .world()
                .resource::<ModRegistry>()
                .get_by_name("bql_parity::NodeState")
                .expect("C99 NodeState after reload")
                .handle;
            let post_node_handle_r = rust_app
                .world()
                .resource::<ModRegistry>()
                .get_by_name("bql_parity::NodeState")
                .expect("Rust NodeState after reload")
                .handle;
            assert_eq!(
                post_node_handle_c, pre_node_handle_c,
                "migrate_component_column must preserve BamComponent handle in C99"
            );
            assert_eq!(
                post_node_handle_r, pre_node_handle_r,
                "migrate_component_column must preserve BamComponent handle in Rust"
            );

            // Named object survival + `ModScoped` stripped!
            let post_root_c = c99_app
                .world()
                .resource::<ModNamedObjects>()
                .get_alive(c99_app.world(), "bql_parity::root_config")
                .expect("C99 named root_config survived reload");
            let post_root_r = rust_app
                .world()
                .resource::<ModNamedObjects>()
                .get_alive(rust_app.world(), "bql_parity::root_config")
                .expect("Rust named root_config survived reload");
            assert_eq!(post_root_c, pre_root_c);
            assert_eq!(post_root_r, pre_root_r);
            assert!(
                c99_app.world().get::<ModScoped>(post_root_c).is_none(),
                "named object must have ModScoped stripped across reload"
            );
            assert!(
                rust_app.world().get::<ModScoped>(post_root_r).is_none(),
                "named object must have ModScoped stripped across reload"
            );

            // Symbol stability across reload + monotonic v2 symbol!
            for (name, expected_sym) in &pre_syms {
                assert_eq!(
                    c99_app.world().resource::<BamSymbolTable>().lookup(name),
                    Some(*expected_sym),
                    "C99 symbol {name} drifted across reload"
                );
                assert_eq!(
                    rust_app.world().resource::<BamSymbolTable>().lookup(name),
                    Some(*expected_sym),
                    "Rust symbol {name} drifted across reload"
                );
            }
            let c_v2_sym = c99_app
                .world()
                .resource::<BamSymbolTable>()
                .lookup("bql::tag::v2_boosted")
                .expect("C99 v2 symbol interned");
            let r_v2_sym = rust_app
                .world()
                .resource::<BamSymbolTable>()
                .lookup("bql::tag::v2_boosted")
                .expect("Rust v2 symbol interned");
            assert_eq!(c_v2_sym, r_v2_sym);
            assert_eq!(c_v2_sym, BamSymbol::from_raw(6));

            // Exact v1 -> v2 migrated rows across all 596 live entities!
            assert_migrated_v2_rows(c99_app.world(), &pre_v1_rows_c, "C99");
            assert_migrated_v2_rows(rust_app.world(), &pre_v1_rows_r, "Rust");

            // Persistent `OwnedBy` relation edge survived reload (marker remains on endpoints)!
            let rel_cid_c = c99_app
                .world()
                .resource::<ModRegistry>()
                .get_by_name("bql_parity::OwnedBy")
                .expect("C99 OwnedBy relation")
                .component_id;
            let rel_participants_c: usize = c99_app
                .world()
                .archetypes()
                .iter()
                .filter(|a| a.contains(rel_cid_c))
                .map(|a| a.len() as usize)
                .sum();
            assert!(
                rel_participants_c >= 2,
                "persistent OwnedBy relation endpoints must survive reload (got {rel_participants_c})"
            );

            // Reflection parity on migrated 4-field v2 `NodeState`!
            assert_reflect_parity(c99_app.world_mut(), rust_app.world_mut(), true);

            // Immediate post-migration world hash parity before frame 250 ticks!
            let c_mig_hash = hash_world_state(c99_app.world());
            let r_mig_hash = hash_world_state(rust_app.world());
            assert_eq!(
                c_mig_hash,
                r_mig_hash,
                "post-migration world state diverged before frame {frame}:\n{}",
                diagnose_world_divergence(c99_app.world(), rust_app.world())
            );
            post_migration_digest = Some(c_mig_hash);
        }

        let schema_gen = if frame < migrate_at_frame { 1 } else { 2 };
        c99_app.world_mut().resource_mut::<SimClock>().frame = frame;
        c99_app.world_mut().resource_mut::<SimClock>().schema_gen = schema_gen;
        rust_app.world_mut().resource_mut::<SimClock>().frame = frame;
        rust_app.world_mut().resource_mut::<SimClock>().schema_gen = schema_gen;

        c99_app.world_mut().run_schedule(FixedUpdate);
        rust_app.world_mut().run_schedule(FixedUpdate);

        // Zero guest faults allowed on either world.
        assert_eq!(
            c99_app
                .world()
                .resource::<Messages<ModSystemFaulted>>()
                .len(),
            0,
            "C99 mod faulted on frame {frame}"
        );
        assert_eq!(
            rust_app
                .world()
                .resource::<Messages<ModSystemFaulted>>()
                .len(),
            0,
            "Rust mod faulted on frame {frame}"
        );

        // Verify `SEQUENCE_WEAK` (`.chain_weak()`) + `after_sets` order & `iter_count` on every frame.
        let c_acc = *c99_app.world().resource::<FrameAccumulator>();
        let r_acc = *rust_app.world().resource::<FrameAccumulator>();
        assert_eq!(
            c_acc.order_trace, EXPECTED_ORDER_TRACE,
            "C99 schedule order trace mismatch on frame {frame}: 0x{:x}",
            c_acc.order_trace
        );
        assert_eq!(
            r_acc.order_trace, EXPECTED_ORDER_TRACE,
            "Rust schedule order trace mismatch on frame {frame}: 0x{:x}",
            r_acc.order_trace
        );
        assert_eq!(
            c_acc.readonly_count, TOTAL_HIERARCHY_NODES as u64,
            "C99 iter_count mismatch on frame {frame}"
        );
        assert_eq!(
            r_acc.readonly_count, TOTAL_HIERARCHY_NODES as u64,
            "Rust iter_count mismatch on frame {frame}"
        );

        // On frame 1, verify v1 `ModReflectPlugin` declarative reflection parity on live entities.
        if frame == 1 {
            assert_reflect_parity(c99_app.world_mut(), rust_app.world_mut(), false);
        }

        // Assert bit-identical sorted archetype component column byte hashes on EVERY frame!
        let c_digest = hash_world_state(c99_app.world());
        let r_digest = hash_world_state(rust_app.world());
        assert_eq!(
            c_digest,
            r_digest,
            "C99 vs Safe Rust wire parity diverged on frame {frame} (schema_gen={schema_gen}):\n  C99  sha256={} fnv=0x{:016x}\n  Rust sha256={} fnv=0x{:016x}\n{}",
            c_digest.sha256_hex(),
            c_digest.fnv1a64,
            r_digest.sha256_hex(),
            r_digest.fnv1a64,
            diagnose_world_divergence(c99_app.world(), rust_app.world())
        );

        if frame == 0 {
            frame_0_digest = Some(c_digest);
        }
        if frame + 1 == migrate_at_frame {
            pre_reload_digest = Some(c_digest);
        }
        if frame + 1 == total_frames {
            final_digest = Some(c_digest);
        }
        verified_frames += 1;
    }

    let final_accumulator = *c99_app.world().resource::<FrameAccumulator>();
    assert_eq!(
        final_accumulator,
        *rust_app.world().resource::<FrameAccumulator>()
    );
    assert_eq!(
        final_accumulator.rolled_back_tx_count,
        total_frames - 1,
        "every frame >= 1 executes one rolled-back ATOMIC transaction"
    );
    assert_eq!(
        final_accumulator.committed_tx_count,
        (total_frames - 1) / 5,
        "every 5th frame >= 1 executes one committed ATOMIC transaction"
    );

    let symbol_count = c99_app.world().resource::<BamSymbolTable>().len();
    assert_eq!(
        symbol_count,
        rust_app.world().resource::<BamSymbolTable>().len()
    );
    assert_eq!(symbol_count, 7, "6 v1 symbols + 1 v2 symbol");

    // Verify full mod unload preserves the bound persistent `OBJECT` `"bql_parity::root_config"`.
    let root_before_unload_c = c99_app
        .world()
        .resource::<ModNamedObjects>()
        .get_alive(c99_app.world(), "bql_parity::root_config")
        .expect("C99 named object alive before unload");
    let root_before_unload_r = rust_app
        .world()
        .resource::<ModNamedObjects>()
        .get_alive(rust_app.world(), "bql_parity::root_config")
        .expect("Rust named object alive before unload");
    assert_eq!(root_before_unload_c, root_before_unload_r);

    c99_app
        .world_mut()
        .unload_mod(c99_mod_id)
        .expect("unload C99 v2 mod");
    rust_app
        .world_mut()
        .unload_mod(rust_mod_id)
        .expect("unload Rust v2 mod");

    assert_eq!(
        c99_app
            .world()
            .resource::<ModNamedObjects>()
            .get_alive(c99_app.world(), "bql_parity::root_config"),
        Some(root_before_unload_c),
        "C99 named object must survive full mod unload"
    );
    assert_eq!(
        rust_app
            .world()
            .resource::<ModNamedObjects>()
            .get_alive(rust_app.world(), "bql_parity::root_config"),
        Some(root_before_unload_r),
        "Rust named object must survive full mod unload"
    );

    ParityReport {
        total_frames,
        migrate_at_frame,
        frame_0_digest: frame_0_digest.expect("frame 0 digest"),
        pre_reload_digest: pre_reload_digest.expect("pre-reload digest"),
        post_migration_digest: post_migration_digest.expect("post-migration digest"),
        final_digest: final_digest.expect("final digest"),
        final_accumulator,
        migrated_rows,
        symbol_count,
        verified_frames,
    }
}
