//! Example 3: Cross-Language Interoperability (Host Application).
//!
//! Loads an ISO C99 shared library (`c_mod`) and an independent Rust `.cdylib` (`rust_mod`) into
//! the same Bevy `Update` schedule operating on the same drone entities.

#![allow(clippy::type_complexity)]

/// Exposed ECS components for Example 3.
pub mod components;

use bevy_abi_mod::prelude::*;
use bevy_app::{App, Update};
use bevy_ecs::entity::Entity;
use bevy_ecs::message::Messages;
use bevy_ecs::query::{Changed, Or};
use bevy_ecs::system::Query;
use components::{ColorRgba, Position, Velocity};
use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
#[cfg(windows)]
use std::sync::OnceLock;

/// Host monitoring system observing mutations produced by both C99 and Rust mods.
fn host_telemetry_monitor(
    query: Query<
        (Entity, &Position, &Velocity, &ColorRgba),
        Or<(Changed<Position>, Changed<ColorRgba>)>,
    >,
) {
    for (entity, pos, vel, color) in &query {
        println!(
            "  Drone {:?} | Pos: ({:6.2}, {:6.2}, {:6.2}) [C99] | Vel: ({:4.1}, {:4.1}) | Color: RGBA({:.2}, {:.2}, {:.2}, {:.2}) [Rust]",
            entity, pos.x, pos.y, pos.z, vel.x, vel.y, color.r, color.g, color.b, color.a
        );
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

fn get_guest_dylib_paths() -> (PathBuf, PathBuf) {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let (prefix, ext) = if cfg!(target_os = "windows") {
        ("", "dll")
    } else if cfg!(target_os = "macos") {
        ("lib", "dylib")
    } else {
        ("lib", "so")
    };

    if let Some(dir) = env::var_os("BAM_FIXTURE_DIR").filter(|v| !v.is_empty()) {
        let dir = PathBuf::from(dir);
        let staged_c = dir.join(format!("{prefix}example_03_c_mod.{ext}"));
        let staged_rust = dir.join(format!("{prefix}example_03_rust_mod.{ext}"));
        if staged_c.is_file() && staged_rust.is_file() {
            return (staged_c, staged_rust);
        }
    }

    let c_dylib = manifest_dir
        .join("c_mod")
        .join(format!("{prefix}c_mod.{ext}"));
    let rust_dylib = manifest_dir
        .join("rust_mod")
        .join("target/release")
        .join(format!("{prefix}rust_mod.{ext}"));

    (c_dylib, rust_dylib)
}

fn ensure_mods_built() -> (PathBuf, PathBuf) {
    let (c_dylib, rust_dylib) = get_guest_dylib_paths();
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));

    if !c_dylib.exists() {
        let cc = find_c_compiler();
        let c_dir = manifest_dir.join("c_mod");
        let include_dir = manifest_dir.join("../../include");
        println!("[HOST] Compiling C99/C11 guest mod via `{cc}`...");
        let pid = std::process::id();
        let obj_path = env::temp_dir().join(format!("example_03_c_mod_{pid}.obj"));
        let implib_path = env::temp_dir().join(format!("example_03_c_mod_{pid}.lib"));
        let exp_path = env::temp_dir().join(format!("example_03_c_mod_{pid}.exp"));

        let mut cmd = msvc_command(&cc);
        cmd.current_dir(&c_dir);
        if is_msvc_style_driver(&cc) {
            cmd.args(["/nologo", "/LD", "/TC", "/std:c11", "/W4", "/WX", "/O2"])
                .arg("/I")
                .arg(&include_dir)
                .arg(msvc_path_arg("/Fo:", &obj_path))
                .arg(msvc_path_arg("/Fe:", &c_dylib))
                .arg("mod.c")
                .arg("/link")
                .arg(msvc_path_arg("/IMPLIB:", &implib_path));
        } else {
            cmd.args([
                "-std=c99",
                "-Wall",
                "-Wextra",
                "-Wpedantic",
                "-Werror",
                "-O3",
                "-shared",
            ])
            .arg(format!("-I{}", include_dir.display()));
            if !cfg!(target_os = "windows") {
                cmd.arg("-fPIC");
            }
            cmd.arg("mod.c").arg("-o").arg(&c_dylib);
        }
        let status = cmd
            .status()
            .unwrap_or_else(|e| panic!("Failed to execute `{cc}` for c_mod: {e}"));
        let _ = fs::remove_file(&obj_path);
        let _ = fs::remove_file(&implib_path);
        let _ = fs::remove_file(&exp_path);
        assert!(status.success(), "Failed to build C99/C11 guest mod");
    }

    if !rust_dylib.exists() {
        println!("[HOST] Compiling Rust guest mod via cargo...");
        let status = Command::new("cargo")
            .arg("build")
            .arg("--manifest-path")
            .arg(manifest_dir.join("rust_mod/Cargo.toml"))
            .arg("--release")
            .status()
            .expect("Failed to execute cargo build for rust_mod");
        assert!(status.success(), "Failed to build Rust guest mod");
    }

    (c_dylib, rust_dylib)
}

fn main() {
    println!("==================================================================");
    println!("  bevy_abi_mod: Example 3 — Cross-Language Interoperability       ");
    println!("  ISO C99 Guest Mod + Rust Guest Mod in One Bevy ECS Schedule     ");
    println!("==================================================================\n");

    let (c_dylib, rust_dylib) = ensure_mods_built();
    println!("[HOST] C99 Mod Library : {}", c_dylib.display());
    println!("[HOST] Rust Mod Library: {}\n", rust_dylib.display());

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);

    app.register_mod_component::<Position>(
        ComponentOptions::new().with_raw_insert().with_default(),
    )
    .expect("register Position");
    app.register_mod_component::<Velocity>(
        ComponentOptions::new().with_raw_insert().with_default(),
    )
    .expect("register Velocity");
    app.register_mod_component::<ColorRgba>(
        ComponentOptions::new().with_raw_insert().with_default(),
    )
    .expect("register ColorRgba");

    app.add_systems(Update, host_telemetry_monitor);

    println!("[HOST] Spawning 3 test drones with initial coordinates:");
    let e0 = app
        .world_mut()
        .spawn((
            Position {
                x: 0.0,
                y: 0.0,
                z: 0.0,
            },
            Velocity {
                x: 10.0,
                y: 5.0,
                z: 0.0,
            },
            ColorRgba {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 1.0,
            },
        ))
        .id();
    let _e1 = app
        .world_mut()
        .spawn((
            Position {
                x: 5.0,
                y: -2.0,
                z: 0.0,
            },
            Velocity {
                x: -2.0,
                y: 8.0,
                z: 0.0,
            },
            ColorRgba {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 1.0,
            },
        ))
        .id();
    let _e2 = app
        .world_mut()
        .spawn((
            Position {
                x: -10.0,
                y: 15.0,
                z: 0.0,
            },
            Velocity {
                x: 4.0,
                y: -3.0,
                z: 0.0,
            },
            ColorRgba {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 1.0,
            },
        ))
        .id();

    let load_opts = LoadOptions {
        shadow_copy: true,
        strict: true,
        ..Default::default()
    };

    println!("\n[HOST] Loading C99 guest mod...");
    let c_mod_id = app
        .load_mod_with(&c_dylib, load_opts.clone())
        .expect("Failed to load C99 mod");
    println!("[HOST] Loaded C99 mod (ModId: {c_mod_id:?})");

    println!("[HOST] Loading Rust guest mod...");
    let rust_mod_id = app
        .load_mod_with(&rust_dylib, load_opts)
        .expect("Failed to load Rust mod");
    println!("[HOST] Loaded Rust mod (ModId: {rust_mod_id:?})\n");
    assert_eq!(app.world().resource::<Messages<ModLoaded>>().len(), 2);

    println!("--- Commencing Simulation Ticks ---");
    for tick in 1..=5 {
        println!("\n>> Tick {tick}");
        app.update();
    }

    assert_eq!(
        app.world().resource::<Messages<ModSystemFaulted>>().len(),
        0
    );

    let p0 = *app.world().get::<Position>(e0).expect("drone e0 Position");
    let c0 = *app
        .world()
        .get::<ColorRgba>(e0)
        .expect("drone e0 ColorRgba");
    assert!(
        (p0.x - 5.0).abs() < 1e-4,
        "C99 mod must have updated Position"
    );
    assert!(
        c0.r > 0.0,
        "Rust mod must have updated ColorRgba based on Position"
    );
    println!(
        "\n[HOST] Verified cooperative state for Drone e0: Pos ({:.2}, {:.2}, {:.2}), Color ({:.2}, {:.2}, {:.2})",
        p0.x, p0.y, p0.z, c0.r, c0.g, c0.b
    );

    println!("\n[HOST] Unloading both mods...");
    app.unload_mod(c_mod_id).expect("Failed to unload C99 mod");
    app.unload_mod(rust_mod_id)
        .expect("Failed to unload Rust mod");
    assert_eq!(app.world().resource::<Messages<ModUnloaded>>().len(), 2);

    println!("[HOST] Executing 1 post-unload tick (expecting 0 mutations):");
    app.update();

    let p0_after = *app.world().get::<Position>(e0).expect("drone e0 Position");
    assert_eq!(
        p0, p0_after,
        "Position must not change after mods are unloaded"
    );

    println!("\n==================================================================");
    println!("  Cross-language C99 + Rust interoperability verified cleanly.    ");
    println!("==================================================================");
}
