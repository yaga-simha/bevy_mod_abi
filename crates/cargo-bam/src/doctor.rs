//! Toolchain, ABI `0.3.0` layout, embedded SDK parity, and mod-directory DAG diagnostics
//! (`cargo bam doctor`).

use crate::builder::{c_candidates, cxx_candidates, probe_compiler_banner};
use crate::dag::{self, ValidatedModManifest};
use crate::{CliError, sdk};
use bevy_abi_mod_sys::{ABI_VERSION, BAM_HOST_API_SLOT_COUNT, BamHostApi};
use std::collections::BTreeSet;
use std::env;
use std::mem::{align_of, size_of};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Minimum recommended Zig minor version (`0.14`).
pub const RECOMMENDED_ZIG_MIN_MINOR: u64 = 14;

/// Maximum recommended Zig minor version (`0.16`).
pub const RECOMMENDED_ZIG_MAX_MINOR: u64 = 16;

/// Status of an individual toolchain, ABI, or SDK diagnostic check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCheck {
    /// Human-readable check name (e.g., `rustc`, `rust_target_abi`, `msvc`, `clang`, `gcc`, `zig`, `sdk_headers`).
    pub name: String,
    /// Whether the probe succeeded.
    pub available: bool,
    /// Whether failure makes [`run_doctor`] return an error.
    pub required: bool,
    /// Detected version string, path, layout info, or failure reason.
    pub detail: String,
}

/// Summary produced by [`inspect_environment`] / [`run_doctor`].
#[derive(Debug, Clone)]
pub struct DoctorReport {
    /// Host ABI SemVer enforced by this `cargo-bam` binary (e.g. `"0.3.0"`).
    pub host_abi_version: String,
    /// Packed hexadecimal ABI constant (e.g. `"0x00030000"`).
    pub abi_version_hex: String,
    /// Detected Rust host target triple (e.g. `"x86_64-pc-windows-msvc"` or `"aarch64-apple-darwin"`).
    pub host_target_triple: String,
    /// Number of function-pointer slots in [`BamHostApi`] (`68`).
    pub host_api_slots: usize,
    /// Byte size of [`BamHostApi`] on this target (`552` on 64-bit, `280` on 32-bit).
    pub host_api_size: usize,
    /// Ordered list of diagnostic probes.
    pub tools: Vec<ToolCheck>,
    /// Optional topologically ordered manifests when `--dir` was supplied.
    pub validated_mods: Option<Vec<ValidatedModManifest>>,
    /// `true` iff every required check passed.
    pub ok: bool,
}

fn probe_command(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program).args(args).output().ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if output.status.success() {
        let first_line = stdout
            .lines()
            .next()
            .filter(|l| !l.trim().is_empty())
            .or_else(|| stderr.lines().next().filter(|l| !l.trim().is_empty()))
            .unwrap_or("ok");
        Some(format!("{program}: {first_line}"))
    } else {
        None
    }
}

fn probe_raw_first_line(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    stdout
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .or_else(|| stderr.lines().map(str::trim).find(|l| !l.is_empty()))
        .map(str::to_owned)
}

fn probe_candidates(candidates: &[String]) -> Option<String> {
    candidates
        .iter()
        .find_map(|candidate| probe_compiler_banner(candidate))
}

fn parse_rustc_host_triple(verbose_output: &str) -> Option<String> {
    verbose_output.lines().find_map(|line| {
        let trimmed = line.trim();
        trimmed
            .strip_prefix("host:")
            .map(str::trim)
            .filter(|triple| !triple.is_empty())
            .map(str::to_owned)
    })
}

fn detect_rust_target_triple() -> String {
    if let Ok(output) = Command::new("rustc").arg("-vV").output()
        && output.status.success()
    {
        let stdout = String::from_utf8_lossy(&output.stdout);
        if let Some(triple) = parse_rustc_host_triple(&stdout) {
            return triple;
        }
    }
    format!("{}-{}", env::consts::ARCH, env::consts::OS)
}

fn probe_vswhere_path() -> Option<PathBuf> {
    for env_key in ["ProgramFiles(x86)", "ProgramFiles"] {
        if let Some(base) = env::var_os(env_key) {
            let candidate = PathBuf::from(base)
                .join("Microsoft Visual Studio")
                .join("Installer")
                .join("vswhere.exe");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    if let Some(path_val) = env::var_os("PATH") {
        for dir in env::split_paths(&path_val) {
            let candidate = dir.join("vswhere.exe");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

fn format_msvc_readiness(
    is_windows: bool,
    cl_banner: Option<&str>,
    has_include: bool,
    has_lib: bool,
    vswhere: Option<&Path>,
) -> (bool, String) {
    let include_state = if has_include { "set" } else { "unset" };
    let lib_state = if has_lib { "set" } else { "unset" };
    let vswhere_state = match vswhere {
        Some(path) => format!("found ({})", path.display()),
        None => "absent".to_owned(),
    };

    if let Some(banner) = cl_banner {
        if !is_windows {
            return (true, banner.to_owned());
        }
        let env_note = if has_include && has_lib {
            format!("INCLUDE={include_state}, LIB={lib_state}, vswhere={vswhere_state}")
        } else if vswhere.is_some() {
            format!(
                "shell INCLUDE={include_state}, LIB={lib_state}; auto-discovered via vswhere={vswhere_state}"
            )
        } else {
            format!(
                "warning: INCLUDE={include_state}, LIB={lib_state}, vswhere={vswhere_state} (run from Developer Command Prompt)"
            )
        };
        (true, format!("{banner} [{env_note}]"))
    } else if is_windows {
        (
            false,
            format!(
                "cl.exe not found (INCLUDE={include_state}, LIB={lib_state}, vswhere={vswhere_state}; install Visual Studio C++ Build Tools)"
            ),
        )
    } else {
        (
            false,
            format!(
                "not applicable on {} (MSVC cl.exe / INCLUDE / LIB / vswhere.exe are Windows-only)",
                env::consts::OS
            ),
        )
    }
}

fn probe_msvc_tool() -> ToolCheck {
    let cl_banner = probe_compiler_banner("cl.exe").or_else(|| probe_compiler_banner("cl"));
    let has_include = env::var_os("INCLUDE").is_some_and(|v| !v.is_empty());
    let has_lib = env::var_os("LIB").is_some_and(|v| !v.is_empty());
    let vswhere = if cfg!(windows) {
        probe_vswhere_path()
    } else {
        None
    };

    let (available, detail) = format_msvc_readiness(
        cfg!(windows),
        cl_banner.as_deref(),
        has_include,
        has_lib,
        vswhere.as_deref(),
    );

    ToolCheck {
        name: "msvc".to_owned(),
        available,
        required: false,
        detail,
    }
}

fn probe_compiler_family(name: &str, programs: &[&str]) -> ToolCheck {
    let mut found = Vec::new();
    for prog in programs {
        if let Some(banner) = probe_compiler_banner(prog) {
            found.push(banner);
        }
    }
    let available = !found.is_empty();
    let detail = if available {
        found.join(" | ")
    } else {
        format!("not found on PATH (checked {})", programs.join(", "))
    };
    ToolCheck {
        name: name.to_owned(),
        available,
        required: false,
        detail,
    }
}

fn parse_zig_major_minor(version_str: &str) -> Option<(u64, u64)> {
    let token = version_str.split_whitespace().next()?;
    let core = token.split(['-', '+']).next()?;
    let mut parts = core.split('.');
    let major: u64 = parts.next()?.parse().ok()?;
    let minor: u64 = parts.next()?.parse().ok()?;
    Some((major, minor))
}

fn format_zig_status(raw_version: Option<&str>) -> (bool, String) {
    match raw_version {
        Some(ver) => match parse_zig_major_minor(ver) {
            Some((0, minor))
                if (RECOMMENDED_ZIG_MIN_MINOR..=RECOMMENDED_ZIG_MAX_MINOR).contains(&minor) =>
            {
                (
                    true,
                    format!(
                        "zig: {ver} (within recommended 0.{RECOMMENDED_ZIG_MIN_MINOR}-0.{RECOMMENDED_ZIG_MAX_MINOR} range)"
                    ),
                )
            }
            Some(_) => (
                true,
                format!(
                    "zig: {ver} (warning: recommended Zig version is 0.{RECOMMENDED_ZIG_MIN_MINOR}-0.{RECOMMENDED_ZIG_MAX_MINOR})"
                ),
            ),
            None => (
                true,
                format!(
                    "zig: {ver} (could not parse version; recommended: 0.{RECOMMENDED_ZIG_MIN_MINOR}-0.{RECOMMENDED_ZIG_MAX_MINOR})"
                ),
            ),
        },
        None => (
            false,
            format!(
                "optional zig toolchain not installed (recommended: Zig 0.{RECOMMENDED_ZIG_MIN_MINOR}-0.{RECOMMENDED_ZIG_MAX_MINOR})"
            ),
        ),
    }
}

fn extract_c99_host_api_slots(c99_header: &str) -> Vec<(String, String)> {
    const PREFIX: &str = "#define BAM_HOST_API_OFFSET_";
    let mut slots = Vec::new();
    let mut seen = BTreeSet::new();
    for line in c99_header.lines() {
        let trimmed = line.trim();
        let Some(rest) = trimmed.strip_prefix(PREFIX) else {
            continue;
        };
        let Some((upper, rhs)) = rest.split_once(" offsetof(BamHostApi, ") else {
            continue;
        };
        let Some(field) = rhs.strip_suffix(')') else {
            continue;
        };
        if seen.insert(upper.to_owned()) {
            slots.push((upper.to_owned(), field.to_owned()));
        }
    }
    slots
}

fn check_sdk_header_parity() -> ToolCheck {
    let c99_header = sdk::c99_header();
    let cpp_header = sdk::CPP17_HEADER;
    let zig_sdk = sdk::ZIG_SDK;
    let slots = extract_c99_host_api_slots(&c99_header);

    let expected_abi_macro = format!("#define BAM_ABI_VERSION 0x{ABI_VERSION:08X}u");
    let expected_slot_count_macro = format!("{BAM_HOST_API_SLOT_COUNT}u");
    let expected_zig_slot_count =
        format!("pub const SLOT_COUNT: usize = {BAM_HOST_API_SLOT_COUNT};");

    let mut issues: Vec<String> = Vec::new();
    if !c99_header.contains(&expected_abi_macro) {
        issues.push(format!("C99 header missing `{expected_abi_macro}`"));
    }
    if !c99_header.contains("BAM_HOST_API_SLOT_COUNT")
        || !c99_header.contains(&expected_slot_count_macro)
    {
        issues.push("C99 header missing BAM_HOST_API_SLOT_COUNT=68".to_owned());
    }
    if !c99_header.contains("BAM_RESTRICT") {
        issues.push("C99 header missing BAM_RESTRICT".to_owned());
    }
    if !cpp_header.contains("BEVY_ABI_MOD_HPP") || !cpp_header.contains("BAM_RESTRICT") {
        issues.push("C++17 header missing guard or BAM_RESTRICT".to_owned());
    }
    if !zig_sdk.contains(&expected_zig_slot_count) || !zig_sdk.contains("BAM_RESTRICT_QUALIFIER") {
        issues.push("Zig SDK missing SLOT_COUNT=68 or BAM_RESTRICT_QUALIFIER".to_owned());
    }
    if slots.len() != BAM_HOST_API_SLOT_COUNT {
        issues.push(format!(
            "C99 header defines {} BAM_HOST_API_OFFSET_* slots, expected {BAM_HOST_API_SLOT_COUNT}",
            slots.len()
        ));
    }

    let mut matched_slots = 0usize;
    for (upper, field) in &slots {
        let cpp_needle =
            format!("inline constexpr std::size_t {upper} = BAM_HOST_API_OFFSET_{upper};");
        let zig_offset_needle =
            format!("pub const {upper}: usize = @offsetOf(c.BamHostApi, \"{field}\");");
        let zig_alias_needle =
            format!("pub const BAM_HOST_API_OFFSET_{upper}: usize = offsets.{upper};");

        if cpp_header.contains(&cpp_needle)
            && zig_sdk.contains(&zig_offset_needle)
            && zig_sdk.contains(&zig_alias_needle)
        {
            matched_slots += 1;
        } else if issues.len() < 4 {
            issues.push(format!(
                "slot `{field}` parity mismatch across C99/C++17/Zig"
            ));
        }
    }

    let parity_ok = issues.is_empty() && matched_slots == BAM_HOST_API_SLOT_COUNT;
    let detail = if parity_ok {
        format!(
            "embedded C99 ({} B), C++17 ({} B), Zig ({} B) — {matched_slots}/{BAM_HOST_API_SLOT_COUNT} BamHostApi slot offsets verified",
            c99_header.len(),
            cpp_header.len(),
            zig_sdk.len()
        )
    } else {
        format!(
            "parity mismatch ({matched_slots}/{BAM_HOST_API_SLOT_COUNT} slots): {}",
            issues.join("; ")
        )
    };

    ToolCheck {
        name: "sdk_headers".to_owned(),
        available: parity_ok,
        required: true,
        detail,
    }
}

/// Inspects Rust target + `ABI 0.3.0` (`0x0003_0000`, 68-slot [`BamHostApi`]) layout,
/// MSVC/Clang/GCC/C99/C++17/Zig toolchains, and embedded SDK header parity without failing on
/// optional tools.
#[must_use]
pub fn inspect_environment(require_c: bool) -> DoctorReport {
    let host_abi_version = dag::host_abi_semver().to_string();
    let abi_version_hex = format!("0x{ABI_VERSION:08X}");
    let host_target_triple = detect_rust_target_triple();
    let host_api_slots = BAM_HOST_API_SLOT_COUNT;
    let host_api_size = size_of::<BamHostApi>();
    let host_api_align = align_of::<BamHostApi>();
    let ptr_size = size_of::<usize>();
    let expected_host_api_size = 8 + host_api_slots * ptr_size;

    let mut tools = Vec::new();

    match probe_command("cargo", &["--version"]) {
        Some(detail) => tools.push(ToolCheck {
            name: "cargo".to_owned(),
            available: true,
            required: true,
            detail,
        }),
        None => tools.push(ToolCheck {
            name: "cargo".to_owned(),
            available: false,
            required: true,
            detail: "cargo executable not found in PATH".to_owned(),
        }),
    }

    match probe_command("rustc", &["--version"]) {
        Some(detail) => tools.push(ToolCheck {
            name: "rustc".to_owned(),
            available: true,
            required: true,
            detail,
        }),
        None => tools.push(ToolCheck {
            name: "rustc".to_owned(),
            available: false,
            required: true,
            detail: "rustc executable not found in PATH".to_owned(),
        }),
    }

    let abi_layout_ok = ABI_VERSION == 0x0003_0000
        && host_api_slots == 68
        && host_api_size == expected_host_api_size
        && BamHostApi::MIN_V0_3_SIZE == expected_host_api_size
        && host_api_align == align_of::<usize>();
    tools.push(ToolCheck {
        name: "rust_target_abi".to_owned(),
        available: abi_layout_ok,
        required: true,
        detail: format!(
            "target {host_target_triple}, ABI {host_abi_version} ({abi_version_hex}), {host_api_slots}-slot BamHostApi ({host_api_size} B, align {host_api_align}, {}-bit)",
            ptr_size * 8
        ),
    });

    let cc_found = probe_candidates(&c_candidates());
    tools.push(ToolCheck {
        name: "c99".to_owned(),
        available: cc_found.is_some(),
        required: require_c,
        detail: cc_found
            .unwrap_or_else(|| "not found (set CC or install clang/cl.exe/gcc/cc)".to_owned()),
    });

    let cxx_found = probe_candidates(&cxx_candidates());
    tools.push(ToolCheck {
        name: "cpp17".to_owned(),
        available: cxx_found.is_some(),
        required: require_c,
        detail: cxx_found
            .unwrap_or_else(|| "not found (set CXX or install clang++/cl.exe/g++/c++)".to_owned()),
    });

    tools.push(probe_msvc_tool());

    #[cfg(windows)]
    let clang_progs = ["clang", "clang++", "clang-cl"];
    #[cfg(not(windows))]
    let clang_progs = ["clang", "clang++"];
    tools.push(probe_compiler_family("clang", &clang_progs));

    tools.push(probe_compiler_family("gcc", &["gcc", "g++"]));

    let zig_raw = probe_raw_first_line("zig", &["version"]);
    let (zig_available, zig_detail) = format_zig_status(zig_raw.as_deref());
    tools.push(ToolCheck {
        name: "zig".to_owned(),
        available: zig_available,
        required: false,
        detail: zig_detail,
    });

    tools.push(check_sdk_header_parity());

    let ok = tools.iter().all(|t| !t.required || t.available);
    DoctorReport {
        host_abi_version,
        abi_version_hex,
        host_target_triple,
        host_api_slots,
        host_api_size,
        tools,
        validated_mods: None,
        ok,
    }
}

/// Runs [`inspect_environment`] and optionally validates a mods directory via [`dag::check_mods_dir`].
pub fn run_doctor(mods_dir: Option<&Path>, require_c: bool) -> Result<DoctorReport, CliError> {
    let mut report = inspect_environment(require_c);
    if let Some(dir) = mods_dir {
        let order = dag::check_mods_dir(dir)?;
        report.validated_mods = Some(order);
    }
    if !report.ok {
        let missing: Vec<String> = report
            .tools
            .iter()
            .filter(|t| t.required && !t.available)
            .map(|t| format!("{} ({})", t.name, t.detail))
            .collect();
        return Err(CliError::Command(format!(
            "doctor check failed: {}",
            missing.join("; ")
        )));
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rustc_target_triple_and_zig_versions() {
        let verbose =
            "rustc 1.98.1\nbinary: rustc\nhost: x86_64-pc-windows-msvc\nrelease: 1.98.1\n";
        assert_eq!(
            parse_rustc_host_triple(verbose).as_deref(),
            Some("x86_64-pc-windows-msvc")
        );

        assert_eq!(parse_zig_major_minor("0.14.1"), Some((0, 14)));
        assert_eq!(parse_zig_major_minor("0.15.0-dev.123+abc"), Some((0, 15)));
        assert_eq!(parse_zig_major_minor("0.16.0"), Some((0, 16)));
        assert_eq!(parse_zig_major_minor("0.13.0"), Some((0, 13)));

        let (ok14, msg14) = format_zig_status(Some("0.14.1"));
        assert!(ok14 && msg14.contains("within recommended 0.14-0.16"));

        let (ok13, msg13) = format_zig_status(Some("0.13.0"));
        assert!(ok13 && msg13.contains("warning: recommended Zig version is 0.14-0.16"));

        let (absent, msg_absent) = format_zig_status(None);
        assert!(!absent && msg_absent.contains("0.14-0.16"));
    }

    #[test]
    fn formats_msvc_readiness_for_windows_and_non_windows() {
        let vswhere =
            Path::new("C:/Program Files (x86)/Microsoft Visual Studio/Installer/vswhere.exe");
        let (ok_env, msg_env) = format_msvc_readiness(
            true,
            Some("cl.exe: Microsoft (R) C/C++ Optimizing Compiler"),
            true,
            true,
            Some(vswhere),
        );
        assert!(ok_env);
        assert!(msg_env.contains("INCLUDE=set"));
        assert!(msg_env.contains("LIB=set"));
        assert!(msg_env.contains("vswhere=found"));

        let (ok_auto, msg_auto) = format_msvc_readiness(
            true,
            Some("cl.exe: Microsoft (R) C/C++ Optimizing Compiler"),
            false,
            false,
            Some(vswhere),
        );
        assert!(ok_auto);
        assert!(msg_auto.contains("auto-discovered via vswhere"));

        let (missing_win, msg_missing_win) = format_msvc_readiness(true, None, false, false, None);
        assert!(!missing_win);
        assert!(msg_missing_win.contains("INCLUDE=unset"));
        assert!(msg_missing_win.contains("LIB=unset"));
        assert!(msg_missing_win.contains("vswhere=absent"));

        let (non_win, msg_non_win) = format_msvc_readiness(false, None, false, false, None);
        assert!(!non_win);
        assert!(msg_non_win.contains("Windows-only"));
    }

    #[test]
    fn doctor_verifies_abi_0_3_layout_and_sdk_parity() {
        let report = inspect_environment(false);
        assert!(report.ok);
        assert_eq!(report.host_abi_version, "0.3.0");
        assert_eq!(report.abi_version_hex, "0x00030000");
        assert_eq!(report.host_api_slots, 68);
        assert!(!report.host_target_triple.is_empty());

        let abi_check = report
            .tools
            .iter()
            .find(|t| t.name == "rust_target_abi")
            .expect("rust_target_abi check must exist");
        assert!(abi_check.available);
        assert!(abi_check.detail.contains("0x00030000"));
        assert!(abi_check.detail.contains("68-slot BamHostApi"));

        let sdk_check = report
            .tools
            .iter()
            .find(|t| t.name == "sdk_headers")
            .expect("sdk_headers check must exist");
        assert!(sdk_check.available);
        assert!(
            sdk_check
                .detail
                .contains("68/68 BamHostApi slot offsets verified")
        );

        assert!(report.tools.iter().any(|t| t.name == "msvc"));
        assert!(report.tools.iter().any(|t| t.name == "clang"));
        assert!(report.tools.iter().any(|t| t.name == "gcc"));
        assert!(report.tools.iter().any(|t| t.name == "zig"));
    }
}
