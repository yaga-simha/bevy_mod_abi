#![forbid(unsafe_code)]

//! Workspace build, fixture, header, benchmark, Miri, and CI automation runner (`cargo xtask`).

mod header_gen;

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

#[derive(Clone, Debug)]
struct BuiltFixture {
    key: &'static str,
    language: &'static str,
    profile: &'static str,
    filename: String,
    dest_path: PathBuf,
    source_rel: String,
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("xtask error: {err}");
            ExitCode::FAILURE
        }
    }
}

fn repo_root() -> Result<PathBuf, String> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| "failed to resolve workspace root from xtask CARGO_MANIFEST_DIR".to_owned())
}

fn dylib_naming() -> (&'static str, &'static str) {
    if cfg!(target_os = "windows") {
        ("", "dll")
    } else if cfg!(target_os = "macos") {
        ("lib", "dylib")
    } else {
        ("lib", "so")
    }
}

fn c_compiler() -> String {
    env::var("CC")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| {
            if cfg!(target_os = "windows") {
                "gcc".to_owned()
            } else {
                "cc".to_owned()
            }
        })
}

fn cargo_bin() -> String {
    env::var("CARGO")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "cargo".to_owned())
}

fn run(args: &[String]) -> Result<(), String> {
    let Some(subcmd) = args.first().map(String::as_str) else {
        print_help();
        return Ok(());
    };

    let rest = &args[1..];
    match subcmd {
        "help" | "--help" | "-h" => {
            print_help();
            Ok(())
        }
        "fixtures" => cmd_fixtures(rest),
        "lint" => cmd_lint(rest),
        "ci" => cmd_ci(rest),
        "header" => cmd_header(rest),
        "bench" => cmd_bench(rest),
        "miri" => cmd_miri(rest),
        "hook" => cmd_hook(rest),
        other => Err(format!(
            "unknown subcommand `{other}` (expected `fixtures`, `lint`, `ci`, `header`, `bench`, `miri`, or `hook`)"
        )),
    }
}

fn print_help() {
    println!("Usage: cargo xtask <subcommand> [options]");
    println!();
    println!("Subcommands:");
    println!(
        "  fixtures [--examples] [--benches] [--all]   Build guest mod fixtures into target/fixtures/"
    );
    println!(
        "  lint [--quick]                              Run fmt, clippy, zero-Bevy check, C99 header, bevy_lint, and Bevy idiom audits"
    );
    println!(
        "  ci                                          Run fmt, clippy, zero-Bevy check, fixtures, C99, and workspace tests"
    );
    println!(
        "  header [--check] [--test-c99]               Generate & validate include/bevy_abi_mod.h as strict ISO C99"
    );
    println!(
        "  bench [--permutation] [-- <args>...]        Run Criterion/host benchmarks or full permutation matrix"
    );
    println!(
        "  miri [--all]                                Run Miri (-Zmiri-tree-borrows) on ABI and guest crates"
    );
    println!(
        "  hook <install|uninstall>                    Install or remove git pre-commit hook for automated lint checks"
    );
}

fn run_cmd(cmd: &mut Command, description: &str) -> Result<(), String> {
    println!("==> {description}");
    let status = cmd
        .status()
        .map_err(|e| format!("failed to spawn `{description}`: {e}"))?;
    if !status.success() {
        return Err(format!("`{description}` failed with status {status}"));
    }
    Ok(())
}

fn build_rust_cdylib(
    root: &Path,
    manifest_rel: &str,
    crate_stem: &str,
    release: bool,
) -> Result<PathBuf, String> {
    let manifest_path = root.join(manifest_rel);
    let mut cmd = Command::new(cargo_bin());
    cmd.current_dir(root)
        .arg("build")
        .arg("--manifest-path")
        .arg(&manifest_path);
    if release {
        cmd.arg("--release");
    }
    let profile_dir = if release { "release" } else { "debug" };
    run_cmd(
        &mut cmd,
        &format!("cargo build {manifest_rel} ({profile_dir})"),
    )?;

    let (prefix, ext) = dylib_naming();
    let crate_dir = manifest_path
        .parent()
        .ok_or_else(|| format!("invalid manifest path {manifest_rel}"))?;
    let artifact = crate_dir
        .join("target")
        .join(profile_dir)
        .join(format!("{prefix}{crate_stem}.{ext}"));
    if !artifact.is_file() {
        return Err(format!(
            "expected Rust cdylib artifact was not produced at {}",
            artifact.display()
        ));
    }
    Ok(artifact)
}

fn build_c99_mod(
    root: &Path,
    dir_rel: &str,
    src_file: &str,
    artifact_stem: &str,
    opt_flag: &str,
    link_math: bool,
) -> Result<PathBuf, String> {
    let mod_dir = root.join(dir_rel);
    let (prefix, ext) = dylib_naming();
    let out_name = format!("{prefix}{artifact_stem}.{ext}");
    let out_path = mod_dir.join(&out_name);

    if !cfg!(target_os = "windows") && mod_dir.join("Makefile").is_file() {
        let mut cmd = Command::new("make");
        cmd.arg("-C").arg(&mod_dir);
        if let Ok(cc) = env::var("CC")
            && !cc.trim().is_empty()
        {
            cmd.arg(format!("CC={cc}"));
        }
        run_cmd(&mut cmd, &format!("make -C {dir_rel}"))?;
    } else {
        let cc = c_compiler();
        let include_dir = root.join("include");
        let mut cmd = Command::new(&cc);
        cmd.current_dir(&mod_dir)
            .args(["-std=c99", "-Wall", "-Wextra", "-Werror"])
            .arg(format!("-I{}", include_dir.display()))
            .arg(opt_flag)
            .arg("-shared");
        if !cfg!(target_os = "windows") {
            cmd.arg("-fPIC");
        }
        cmd.arg(src_file).arg("-o").arg(&out_path);
        if link_math && !cfg!(target_os = "windows") {
            cmd.arg("-lm");
        }
        run_cmd(
            &mut cmd,
            &format!("{cc} {dir_rel}/{src_file} -> {out_name}"),
        )?;
    }

    if !out_path.is_file() {
        return Err(format!(
            "expected C99 shared library was not produced at {}",
            out_path.display()
        ));
    }
    Ok(out_path)
}

fn build_c99_test_fixture(root: &Path, stem: &str) -> Result<PathBuf, String> {
    let src_dir = root.join("tests").join("fixtures").join("c_mods");
    let src_path = src_dir.join(format!("{stem}.c"));
    let build_dir = root.join("target").join("c_mods_build");
    fs::create_dir_all(&build_dir)
        .map_err(|e| format!("failed to create {}: {e}", build_dir.display()))?;

    let (prefix, ext) = dylib_naming();
    let out_name = format!("{prefix}{stem}.{ext}");
    let out_path = build_dir.join(&out_name);

    let cc = c_compiler();
    let include_dir = root.join("include");
    let mut cmd = Command::new(&cc);
    cmd.current_dir(root)
        .args(["-std=c99", "-Wall", "-Wextra", "-Werror"])
        .arg(format!("-I{}", include_dir.display()))
        .args(["-O2", "-shared"]);
    if !cfg!(target_os = "windows") {
        cmd.arg("-fPIC");
    }
    cmd.arg(&src_path).arg("-o").arg(&out_path);
    run_cmd(
        &mut cmd,
        &format!("{cc} tests/fixtures/c_mods/{stem}.c -> target/c_mods_build/{out_name}"),
    )?;

    if !out_path.is_file() {
        return Err(format!(
            "expected C99 test fixture was not produced at {}",
            out_path.display()
        ));
    }
    Ok(out_path)
}

fn stage_fixture(
    root: &Path,
    fixtures_dir: &Path,
    key: &'static str,
    language: &'static str,
    profile: &'static str,
    source_path: &Path,
    dest_stem: &str,
) -> Result<BuiltFixture, String> {
    let (prefix, ext) = dylib_naming();
    let filename = format!("{prefix}{dest_stem}.{ext}");
    let dest_path = fixtures_dir.join(&filename);
    fs::copy(source_path, &dest_path).map_err(|e| {
        format!(
            "failed to copy {} -> {}: {e}",
            source_path.display(),
            dest_path.display()
        )
    })?;

    let source_rel = source_path
        .strip_prefix(root)
        .unwrap_or(source_path)
        .to_string_lossy()
        .replace('\\', "/");

    Ok(BuiltFixture {
        key,
        language,
        profile,
        filename,
        dest_path,
        source_rel,
    })
}

fn json_escape(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out
}

fn write_manifest(fixtures_dir: &Path, built: &[BuiltFixture]) -> Result<PathBuf, String> {
    let (prefix, ext) = dylib_naming();
    let mut json = String::new();
    json.push_str("{\n");
    json.push_str("  \"schema_version\": 1,\n");
    json.push_str(&format!(
        "  \"os\": \"{}\",\n",
        json_escape(env::consts::OS)
    ));
    json.push_str(&format!(
        "  \"arch\": \"{}\",\n",
        json_escape(env::consts::ARCH)
    ));
    json.push_str(&format!(
        "  \"dylib_prefix\": \"{}\",\n",
        json_escape(prefix)
    ));
    json.push_str(&format!(
        "  \"dylib_extension\": \"{}\",\n",
        json_escape(ext)
    ));
    json.push_str("  \"fixtures\": {\n");

    for (idx, item) in built.iter().enumerate() {
        let comma = if idx + 1 == built.len() { "" } else { "," };
        let abs_path = item.dest_path.to_string_lossy().replace('\\', "/");
        json.push_str(&format!("    \"{}\": {{\n", json_escape(item.key)));
        json.push_str(&format!(
            "      \"language\": \"{}\",\n",
            json_escape(item.language)
        ));
        json.push_str(&format!(
            "      \"profile\": \"{}\",\n",
            json_escape(item.profile)
        ));
        json.push_str(&format!(
            "      \"filename\": \"{}\",\n",
            json_escape(&item.filename)
        ));
        json.push_str(&format!(
            "      \"path\": \"{}\",\n",
            json_escape(&abs_path)
        ));
        json.push_str(&format!(
            "      \"source_path\": \"{}\"\n",
            json_escape(&item.source_rel)
        ));
        json.push_str(&format!("    }}{comma}\n"));
    }

    json.push_str("  }\n");
    json.push_str("}\n");

    let manifest_path = fixtures_dir.join("manifest.json");
    fs::write(&manifest_path, json)
        .map_err(|e| format!("failed to write {}: {e}", manifest_path.display()))?;
    Ok(manifest_path)
}

fn example_mod_is_v0_2_ready(root: &Path, source_rel: &str) -> bool {
    let Ok(content) = fs::read_to_string(root.join(source_rel)) else {
        return false;
    };
    !content.contains("ComponentAccessDesc")
        && !content.contains("BEVY_ABI_MOD_EXPORT")
        && !content.contains(".add_system(")
        && !content.contains(".add_system_if(")
        && !content.contains(".add_multi_query_system")
}

fn build_fixtures_internal(
    root: &Path,
    include_examples: bool,
    include_benches: bool,
) -> Result<PathBuf, String> {
    let fixtures_dir = root.join("target").join("fixtures");
    fs::create_dir_all(&fixtures_dir)
        .map_err(|e| format!("failed to create {}: {e}", fixtures_dir.display()))?;

    let mut built = Vec::new();

    // 1. Core asymmetric test fixtures + `panic = "abort"` Rust guest fixture (`G-05`)
    let rust_mod = build_rust_cdylib(
        root,
        "tests/asymmetric_rust_mod/Cargo.toml",
        "asymmetric_rust_mod",
        true,
    )?;
    built.push(stage_fixture(
        root,
        &fixtures_dir,
        "asymmetric_rust_mod",
        "rust",
        "release",
        &rust_mod,
        "asymmetric_rust_mod",
    )?);

    let abort_mod = build_rust_cdylib(
        root,
        "tests/fixtures/panic_abort_mod/Cargo.toml",
        "panic_abort_mod",
        true,
    )?;
    built.push(stage_fixture(
        root,
        &fixtures_dir,
        "panic_abort_mod",
        "rust-abort",
        "release",
        &abort_mod,
        "panic_abort_mod",
    )?);

    let c_mod = build_c99_mod(
        root,
        "tests/asymmetric_c_mod",
        "mod.c",
        "asymmetric_c_mod",
        "-O3",
        false,
    )?;
    built.push(stage_fixture(
        root,
        &fixtures_dir,
        "asymmetric_c_mod",
        "c99",
        "release",
        &c_mod,
        "asymmetric_c_mod",
    )?);

    // 2. Hermetic C99 integration test fixtures (`tests/fixtures/c_mods/*.c`)
    let c_test_fixtures: &[&str] = &[
        "c_uuid_oracle",
        "s05_vtable_mod",
        "s06_fail_init",
        "s06_strict_fail",
        "l01_late_startup",
        "mod_alpha",
        "mod_beta",
        "mod_producer",
        "mod_consumer",
        "l05c_clean_mod",
    ];
    for &stem in c_test_fixtures {
        let compiled = build_c99_test_fixture(root, stem)?;
        let mut staged =
            stage_fixture(root, &fixtures_dir, stem, "c99", "release", &compiled, stem)?;
        staged.source_rel = format!("tests/fixtures/c_mods/{stem}.c");
        built.push(staged);
    }

    // 3. Lightweight Bevy-free example 01..04 guest mods exercised by host E2E tests
    let test_rust_examples: &[(&str, &str, &str, &str, &str)] = &[
        (
            "example_01_guest_mod",
            "examples/01_minimal_handshake/guest_mod/Cargo.toml",
            "examples/01_minimal_handshake/guest_mod/src/lib.rs",
            "minimal_guest_mod",
            "minimal_guest_mod",
        ),
        (
            "example_02_guest_mod",
            "examples/02_multi_system_pipeline/guest_mod/Cargo.toml",
            "examples/02_multi_system_pipeline/guest_mod/src/lib.rs",
            "example_02_guest_mod",
            "example_02_guest_mod",
        ),
        (
            "example_03_rust_mod",
            "examples/03_cross_language_interop/rust_mod/Cargo.toml",
            "examples/03_cross_language_interop/rust_mod/src/lib.rs",
            "rust_mod",
            "example_03_rust_mod",
        ),
        (
            "mod_gravity",
            "examples/04_visual_game/mods/mod_gravity/Cargo.toml",
            "examples/04_visual_game/mods/mod_gravity/src/lib.rs",
            "mod_gravity",
            "mod_gravity",
        ),
        (
            "mod_rainbow",
            "examples/04_visual_game/mods/mod_rainbow/Cargo.toml",
            "examples/04_visual_game/mods/mod_rainbow/src/lib.rs",
            "mod_rainbow",
            "mod_rainbow",
        ),
        (
            "mod_vortex",
            "examples/04_visual_game/mods/mod_vortex/Cargo.toml",
            "examples/04_visual_game/mods/mod_vortex/src/lib.rs",
            "mod_vortex",
            "mod_vortex",
        ),
    ];

    for &(key, manifest_rel, source_rel, crate_stem, dest_stem) in test_rust_examples {
        if !include_examples && !example_mod_is_v0_2_ready(root, source_rel) {
            println!(
                "==> [Notice] Skipping unported example fixture `{key}` ({source_rel}); pass `--examples` to require"
            );
            continue;
        }
        let src = build_rust_cdylib(root, manifest_rel, crate_stem, true)?;
        built.push(stage_fixture(
            root,
            &fixtures_dir,
            key,
            "rust",
            "release",
            &src,
            dest_stem,
        )?);
    }

    if include_examples
        || example_mod_is_v0_2_ready(root, "examples/03_cross_language_interop/c_mod/mod.c")
    {
        let ex3_c = build_c99_mod(
            root,
            "examples/03_cross_language_interop/c_mod",
            "mod.c",
            "c_mod",
            "-O3",
            false,
        )?;
        built.push(stage_fixture(
            root,
            &fixtures_dir,
            "example_03_c_mod",
            "c99",
            "release",
            &ex3_c,
            "example_03_c_mod",
        )?);
    } else {
        println!(
            "==> [Notice] Skipping unported example fixture `example_03_c_mod`; pass `--examples` to require"
        );
    }

    // 4. Additional optional example guest mods (Examples 05..06)
    if include_examples {
        let extra_rust_examples: &[(&str, &str, &str, &str)] = &[
            (
                "mod_boing",
                "examples/05_cute_3d_winit/mods/mod_boing/Cargo.toml",
                "mod_boing",
                "mod_boing",
            ),
            (
                "mod_carousel",
                "examples/05_cute_3d_winit/mods/mod_carousel/Cargo.toml",
                "mod_carousel",
                "mod_carousel",
            ),
            (
                "mod_shield_emitter",
                "examples/06_cross_mod_components/mods/mod_shield_emitter/Cargo.toml",
                "mod_shield_emitter",
                "mod_shield_emitter",
            ),
            (
                "mod_overcharge_siphon",
                "examples/06_cross_mod_components/mods/mod_overcharge_siphon/Cargo.toml",
                "mod_overcharge_siphon",
                "mod_overcharge_siphon",
            ),
        ];

        for &(key, manifest_rel, crate_stem, dest_stem) in extra_rust_examples {
            let src = build_rust_cdylib(root, manifest_rel, crate_stem, true)?;
            built.push(stage_fixture(
                root,
                &fixtures_dir,
                key,
                "rust",
                "release",
                &src,
                dest_stem,
            )?);
        }

        let ex5_c = build_c99_mod(
            root,
            "examples/05_cute_3d_winit/mods/mod_cuddle_c99",
            "mod.c",
            "mod_cuddle",
            "-O3",
            true,
        )?;
        built.push(stage_fixture(
            root,
            &fixtures_dir,
            "mod_cuddle",
            "c99",
            "release",
            &ex5_c,
            "mod_cuddle",
        )?);
    }

    // 3. Optional benchmark guest mods
    if include_benches {
        let bench_c_rel = build_c99_mod(
            root,
            "benches/c_abi_c99_mod",
            "mod.c",
            "c_abi_c99_mod_release",
            "-O3",
            true,
        )?;
        let (prefix, ext) = dylib_naming();
        let bench_c_dbg_path = root
            .join("benches/c_abi_c99_mod")
            .join(format!("{prefix}c_abi_c99_mod_debug.{ext}"));
        let bench_c_dbg = if bench_c_dbg_path.is_file() {
            bench_c_dbg_path
        } else {
            build_c99_mod(
                root,
                "benches/c_abi_c99_mod",
                "mod.c",
                "c_abi_c99_mod_debug",
                "-O0",
                true,
            )?
        };
        built.push(stage_fixture(
            root,
            &fixtures_dir,
            "c_abi_c99_mod_debug",
            "c99",
            "debug",
            &bench_c_dbg,
            "c_abi_c99_mod_debug",
        )?);
        built.push(stage_fixture(
            root,
            &fixtures_dir,
            "c_abi_c99_mod_release",
            "c99",
            "release",
            &bench_c_rel,
            "c_abi_c99_mod_release",
        )?);

        let bench_rust_dbg = build_rust_cdylib(
            root,
            "benches/c_abi_rust_mod/Cargo.toml",
            "c_abi_rust_mod",
            false,
        )?;
        built.push(stage_fixture(
            root,
            &fixtures_dir,
            "c_abi_rust_mod_debug",
            "rust",
            "debug",
            &bench_rust_dbg,
            "c_abi_rust_mod_debug",
        )?);

        let bench_rust_rel = build_rust_cdylib(
            root,
            "benches/c_abi_rust_mod/Cargo.toml",
            "c_abi_rust_mod",
            true,
        )?;
        built.push(stage_fixture(
            root,
            &fixtures_dir,
            "c_abi_rust_mod_release",
            "rust",
            "release",
            &bench_rust_rel,
            "c_abi_rust_mod_release",
        )?);
    }

    let manifest_path = write_manifest(&fixtures_dir, &built)?;
    println!(
        "==> Staged {} fixture(s) in {} (manifest: {})",
        built.len(),
        fixtures_dir.display(),
        manifest_path.display()
    );
    Ok(fixtures_dir)
}

fn cmd_fixtures(args: &[String]) -> Result<(), String> {
    let mut include_examples = false;
    let mut include_benches = false;

    for arg in args {
        match arg.as_str() {
            "--all" => {
                include_examples = true;
                include_benches = true;
            }
            "--examples" => include_examples = true,
            "--benches" => include_benches = true,
            other => {
                return Err(format!(
                    "unsupported `fixtures` flag `{other}` (expected `--all`, `--examples`, or `--benches`)"
                ));
            }
        }
    }

    let root = repo_root()?;
    build_fixtures_internal(&root, include_examples, include_benches)?;
    Ok(())
}

fn verify_zero_bevy_deps(root: &Path) -> Result<(), String> {
    println!("==> Checking zero Bevy dependencies in ABI, guest, and macro crates");
    let output = Command::new(cargo_bin())
        .current_dir(root)
        .args([
            "tree",
            "-p",
            "bevy_abi_mod_sys",
            "-p",
            "bevy_abi_mod_guest_sys",
            "-p",
            "bevy_abi_mod_guest",
            "-p",
            "bevy_abi_mod_macros",
        ])
        .output()
        .map_err(|e| format!("failed to run `cargo tree`: {e}"))?;

    if !output.status.success() {
        return Err(format!(
            "`cargo tree` failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let leaked: Vec<&str> = stdout
        .lines()
        .filter(|line| !line.contains("bevy_abi_mod"))
        .filter(|line| line.to_ascii_lowercase().contains("bevy"))
        .collect();

    if !leaked.is_empty() {
        return Err(format!(
            "Bevy dependency leaked into Bevy-free crates:\n{}",
            leaked.join("\n")
        ));
    }
    Ok(())
}

fn verify_c99_header_and_tests(root: &Path, run_standalone_test: bool) -> Result<(), String> {
    let header_path = root.join("include").join("bevy_abi_mod.h");
    let meta = fs::metadata(&header_path)
        .map_err(|e| format!("missing C header {}: {e}", header_path.display()))?;
    if meta.len() == 0 {
        return Err(format!("C header {} is empty", header_path.display()));
    }

    let cc = c_compiler();
    let mut syntax_cmd = Command::new(&cc);
    syntax_cmd
        .current_dir(root)
        .args([
            "-std=c99",
            "-Wall",
            "-Wextra",
            "-Werror",
            "-pedantic",
            "-fsyntax-only",
            "-x",
            "c",
        ])
        .arg(&header_path);
    run_cmd(
        &mut syntax_cmd,
        &format!("{cc} -std=c99 -pedantic -fsyntax-only include/bevy_abi_mod.h"),
    )?;

    if run_standalone_test {
        let target_dir = root.join("target");
        fs::create_dir_all(&target_dir)
            .map_err(|e| format!("failed to create {}: {e}", target_dir.display()))?;
        let exe_name = if cfg!(target_os = "windows") {
            "standalone_c_test.exe"
        } else {
            "standalone_c_test"
        };
        let exe_path = target_dir.join(exe_name);
        let mut build_test = Command::new(&cc);
        build_test
            .current_dir(root)
            .args([
                "-std=c99",
                "-Wall",
                "-Wextra",
                "-Werror",
                "-pedantic",
                "-Iinclude",
            ])
            .arg(root.join("tests").join("standalone_c_test.c"))
            .arg("-o")
            .arg(&exe_path);
        run_cmd(
            &mut build_test,
            &format!("{cc} -std=c99 -pedantic tests/standalone_c_test.c -> target/{exe_name}"),
        )?;

        let mut exec_test = Command::new(&exe_path);
        exec_test.current_dir(root);
        run_cmd(&mut exec_test, "target/standalone_c_test")?;
    }

    Ok(())
}

fn cmd_header(args: &[String]) -> Result<(), String> {
    let mut run_standalone = false;
    let mut check_only = false;
    for arg in args {
        match arg.as_str() {
            "--test-c99" => run_standalone = true,
            "--check" => check_only = true,
            other => {
                return Err(format!(
                    "unsupported `header` flag `{other}` (expected `--check` or `--test-c99`)"
                ));
            }
        }
    }

    let root = repo_root()?;
    header_gen::write_or_check_header(&root, check_only)?;
    verify_c99_header_and_tests(&root, run_standalone)
}

fn cmd_lint(args: &[String]) -> Result<(), String> {
    let mut quick = false;
    for arg in args {
        match arg.as_str() {
            "--quick" => quick = true,
            other => {
                return Err(format!(
                    "unsupported `lint` flag `{other}` (expected `--quick`)"
                ));
            }
        }
    }

    let root = repo_root()?;

    let mut fmt_cmd = Command::new(cargo_bin());
    fmt_cmd
        .current_dir(&root)
        .args(["fmt", "--all", "--", "--check"]);
    run_cmd(&mut fmt_cmd, "cargo fmt --all -- --check")?;

    let mut clippy_cmd = Command::new(cargo_bin());
    clippy_cmd.current_dir(&root).args([
        "clippy",
        "--workspace",
        "--all-targets",
        "--",
        "-D",
        "warnings",
    ]);
    run_cmd(
        &mut clippy_cmd,
        "cargo clippy --workspace --all-targets -- -D warnings",
    )?;

    verify_zero_bevy_deps(&root)?;
    verify_forbid_unsafe(&root)?;
    header_gen::write_or_check_header(&root, true)?;
    verify_c99_header_and_tests(&root, !quick)?;
    check_or_run_bevy_lint(&root)?;
    audit_bevy_idioms(&root)?;

    if !quick {
        println!("==> All idiomatic Bevy and workspace lint checks passed");
    }
    Ok(())
}

fn cmd_ci(args: &[String]) -> Result<(), String> {
    if !args.is_empty() {
        return Err(format!("unexpected arguments for `ci`: {}", args.join(" ")));
    }
    let root = repo_root()?;

    let mut fmt_cmd = Command::new(cargo_bin());
    fmt_cmd
        .current_dir(&root)
        .args(["fmt", "--all", "--", "--check"]);
    run_cmd(&mut fmt_cmd, "cargo fmt --all -- --check")?;

    let mut clippy_cmd = Command::new(cargo_bin());
    clippy_cmd.current_dir(&root).args([
        "clippy",
        "--workspace",
        "--all-targets",
        "--",
        "-D",
        "warnings",
    ]);
    run_cmd(
        &mut clippy_cmd,
        "cargo clippy --workspace --all-targets -- -D warnings",
    )?;

    verify_zero_bevy_deps(&root)?;

    let fixtures_dir = build_fixtures_internal(&root, false, false)?;
    header_gen::write_or_check_header(&root, true)?;
    verify_c99_header_and_tests(&root, true)?;

    let mut test_cmd = Command::new(cargo_bin());
    test_cmd
        .current_dir(&root)
        .env("BAM_FIXTURE_DIR", &fixtures_dir)
        .args(["test"]);
    run_cmd(&mut test_cmd, "cargo test")?;

    Ok(())
}

fn cmd_bench(args: &[String]) -> Result<(), String> {
    let root = repo_root()?;
    let mut permutation = false;
    let mut passthrough = Vec::new();
    let mut after_sep = false;

    for arg in args {
        if after_sep {
            passthrough.push(arg.clone());
            continue;
        }
        match arg.as_str() {
            "--" => after_sep = true,
            "--permutation" => permutation = true,
            other => passthrough.push(other.to_owned()),
        }
    }

    let fixtures_dir = build_fixtures_internal(&root, false, permutation)?;

    if permutation {
        let mut cmd = Command::new(cargo_bin());
        cmd.current_dir(&root)
            .env("BAM_FIXTURE_DIR", &fixtures_dir)
            .args(["run", "--release", "-p", "permutation_bench"]);
        if !passthrough.is_empty() {
            cmd.arg("--").args(&passthrough);
        }
        run_cmd(&mut cmd, "cargo run --release -p permutation_bench")
    } else {
        let mut cmd = Command::new(cargo_bin());
        cmd.current_dir(&root)
            .env("BAM_FIXTURE_DIR", &fixtures_dir)
            .args(["bench", "-p", "bevy_abi_mod_host"]);
        if !passthrough.is_empty() {
            cmd.arg("--").args(&passthrough);
        }
        run_cmd(&mut cmd, "cargo bench -p bevy_abi_mod_host")
    }
}

fn cmd_miri(args: &[String]) -> Result<(), String> {
    let mut run_all = false;
    for arg in args {
        match arg.as_str() {
            "--all" => run_all = true,
            other => {
                return Err(format!(
                    "unsupported `miri` flag `{other}` (expected `--all`)"
                ));
            }
        }
    }

    let root = repo_root()?;
    let tree_borrows_flags = match env::var("MIRIFLAGS") {
        Ok(existing) if !existing.trim().is_empty() => format!("{existing} -Zmiri-tree-borrows"),
        _ => "-Zmiri-tree-borrows".to_owned(),
    };
    let strict_prov_flags = if tree_borrows_flags.contains("-Zmiri-strict-provenance") {
        tree_borrows_flags.clone()
    } else {
        format!("{tree_borrows_flags} -Zmiri-strict-provenance")
    };

    let mut base_cmd = Command::new("rustup");
    base_cmd
        .current_dir(&root)
        .env_remove("CARGO")
        .env_remove("RUSTC")
        .env("MIRIFLAGS", &strict_prov_flags)
        .args([
            "run",
            "nightly",
            "cargo",
            "miri",
            "test",
            "-p",
            "bevy_abi_mod_sys",
            "-p",
            "bevy_abi_mod_guest_sys",
            "-p",
            "bevy_abi_mod_guest",
        ]);
    run_cmd(
        &mut base_cmd,
        "rustup run nightly cargo miri test (-Zmiri-tree-borrows -Zmiri-strict-provenance) -p bevy_abi_mod_sys -p bevy_abi_mod_guest_sys -p bevy_abi_mod_guest",
    )?;

    if run_all {
        let mut macros_cmd = Command::new("rustup");
        macros_cmd
            .current_dir(&root)
            .env_remove("CARGO")
            .env_remove("RUSTC")
            .env("MIRIFLAGS", &tree_borrows_flags)
            .args([
                "run",
                "nightly",
                "cargo",
                "miri",
                "test",
                "-p",
                "bevy_abi_mod_macros",
                "--lib",
                "--test",
                "mod_component_vtable",
            ]);
        run_cmd(
            &mut macros_cmd,
            "rustup run nightly cargo miri test -p bevy_abi_mod_macros --lib --test mod_component_vtable",
        )?;

        let mut host_sys_cmd = Command::new("rustup");
        host_sys_cmd
            .current_dir(&root)
            .env_remove("CARGO")
            .env_remove("RUSTC")
            .env("MIRIFLAGS", &tree_borrows_flags)
            .args([
                "run",
                "nightly",
                "cargo",
                "miri",
                "test",
                "-p",
                "bevy_abi_mod_host_sys",
                "--lib",
            ]);
        run_cmd(
            &mut host_sys_cmd,
            "rustup run nightly cargo miri test -p bevy_abi_mod_host_sys --lib",
        )?;
    }

    Ok(())
}

fn verify_forbid_unsafe(root: &Path) -> Result<(), String> {
    println!("==> Verifying #![forbid(unsafe_code)] in safe host, guest, and xtask crates");
    let safe_crates = [
        "crates/bevy_abi_mod/src/lib.rs",
        "crates/bevy_abi_mod_host/src/lib.rs",
        "crates/bevy_abi_mod_guest/src/lib.rs",
        "xtask/src/main.rs",
    ];

    for rel_path in &safe_crates {
        let full_path = root.join(rel_path);
        let content = fs::read_to_string(&full_path)
            .map_err(|e| format!("failed to read {}: {e}", full_path.display()))?;
        if !content.contains("#![forbid(unsafe_code)]") {
            return Err(format!(
                "{} is missing required `#![forbid(unsafe_code)]` declaration",
                full_path.display()
            ));
        }
    }
    Ok(())
}

fn check_or_run_bevy_lint(root: &Path) -> Result<(), String> {
    println!("==> Checking for specialized `bevy_lint`...");
    let which_cmd = if cfg!(target_os = "windows") {
        "where"
    } else {
        "which"
    };
    let has_bevy_lint = Command::new(which_cmd)
        .arg("bevy_lint")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    let has_bevy_cli = Command::new(which_cmd)
        .arg("bevy")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    if has_bevy_lint {
        println!("==> Executing `bevy_lint` on host crates...");
        let mut cmd = Command::new("bevy_lint");
        cmd.current_dir(root).args(["-p", "bevy_abi_mod_host"]);
        let _ = run_cmd(&mut cmd, "bevy_lint -p bevy_abi_mod_host");
    } else if has_bevy_cli {
        println!("==> Executing `bevy lint` on host crates...");
        let mut cmd = Command::new("bevy");
        cmd.current_dir(root)
            .args(["lint", "--", "-p", "bevy_abi_mod_host"]);
        let _ = run_cmd(&mut cmd, "bevy lint -- -p bevy_abi_mod_host");
    } else {
        println!("==> [Notice] Specialized `bevy_lint` tool not found on PATH.");
        println!("    To install the official Bevy linter (The Bevy Flock):");
        println!("      cargo install --git https://github.com/TheBevyFlock/bevy_cli bevy_cli");
        println!("      bevy lint install");
    }
    Ok(())
}

fn audit_bevy_idioms(root: &Path) -> Result<(), String> {
    println!("==> Performing static idiomatic Bevy & Rust audit...");
    let host_src = root.join("crates").join("bevy_abi_mod_host").join("src");
    if host_src.is_dir() {
        audit_dir_bevy_idioms(&host_src)?;
    }
    Ok(())
}

fn audit_dir_bevy_idioms(dir: &Path) -> Result<(), String> {
    let entries =
        fs::read_dir(dir).map_err(|e| format!("failed to read {}: {e}", dir.display()))?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            audit_dir_bevy_idioms(&path)?;
        } else if path.extension().and_then(|s| s.to_str()) == Some("rs") {
            let content = fs::read_to_string(&path)
                .map_err(|e| format!("failed to read {}: {e}", path.display()))?;
            for (line_idx, line) in content.lines().enumerate() {
                let trimmed = line.trim();
                if trimmed.contains("&mut ResMut<") {
                    eprintln!(
                        "  [anti-pattern] {}:{}: `&mut ResMut<T>` is redundant and non-idiomatic. Use `mut res: ResMut<T>`.",
                        path.display(),
                        line_idx + 1
                    );
                }
                if (trimmed.contains(".after(") || trimmed.contains(".before("))
                    && !trimmed.contains("Set")
                    && !trimmed.starts_with("//")
                {
                    eprintln!(
                        "  [idiom critique] {}:{}: direct system chaining with `{}` detected. Consider grouping systems into a `SystemSet` instead.",
                        path.display(),
                        line_idx + 1,
                        trimmed
                    );
                }
            }
        }
    }
    Ok(())
}

fn cmd_hook(args: &[String]) -> Result<(), String> {
    let root = repo_root()?;
    let action = args.first().map(String::as_str).unwrap_or("help");
    let hook_path = root.join(".git").join("hooks").join("pre-commit");

    match action {
        "install" => {
            let hook_script = "#!/bin/sh\n# Generated by `cargo xtask hook install`\nset -e\necho \"==> [pre-commit] Running idiomatic Bevy & Rust checks...\"\ncargo xtask lint --quick\n";
            fs::write(&hook_path, hook_script)
                .map_err(|e| format!("failed to write {}: {e}", hook_path.display()))?;

            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let perms = fs::Permissions::from_mode(0o755);
                fs::set_permissions(&hook_path, perms).map_err(|e| {
                    format!(
                        "failed to set executable permissions on {}: {e}",
                        hook_path.display()
                    )
                })?;
            }

            println!(
                "==> Pre-commit hook installed successfully at {}",
                hook_path.display()
            );
            Ok(())
        }
        "uninstall" => {
            if hook_path.exists() {
                fs::remove_file(&hook_path)
                    .map_err(|e| format!("failed to remove {}: {e}", hook_path.display()))?;
                println!(
                    "==> Pre-commit hook uninstalled from {}",
                    hook_path.display()
                );
            } else {
                println!("==> No pre-commit hook found at {}", hook_path.display());
            }
            Ok(())
        }
        _ => Err("usage: cargo xtask hook <install|uninstall>".to_owned()),
    }
}
