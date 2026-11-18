//! Toolchain, ABI, and mod-directory diagnostics (`cargo bam doctor`).

use crate::dag::{self, ValidatedModManifest};
use crate::{CliError, sdk};
use std::env;
use std::path::Path;
use std::process::Command;

/// Status of an individual toolchain or SDK diagnostic check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCheck {
    /// Human-readable check name (e.g., `rustc`, `c99`, `sdk_headers`).
    pub name: String,
    /// Whether the probe succeeded.
    pub available: bool,
    /// Whether failure makes [`run_doctor`] return an error.
    pub required: bool,
    /// Detected version string, path, or failure reason.
    pub detail: String,
}

/// Summary produced by [`inspect_environment`] / [`run_doctor`].
#[derive(Debug, Clone)]
pub struct DoctorReport {
    /// Host ABI SemVer enforced by this `cargo-bam` binary.
    pub host_abi_version: String,
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

fn probe_candidates(candidates: &[String], args: &[&str]) -> Option<String> {
    for candidate in candidates {
        if let Some(found) = probe_command(candidate, args) {
            return Some(found);
        }
    }
    None
}

fn c_candidates() -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(cc) = env::var("CC")
        && !cc.trim().is_empty()
    {
        out.push(cc);
    }
    for default_cc in ["cc", "gcc", "clang"] {
        if !out.iter().any(|c| c == default_cc) {
            out.push(default_cc.to_string());
        }
    }
    out
}

fn cxx_candidates() -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(cxx) = env::var("CXX")
        && !cxx.trim().is_empty()
    {
        out.push(cxx);
    }
    for default_cxx in ["c++", "g++", "clang++"] {
        if !out.iter().any(|c| c == default_cxx) {
            out.push(default_cxx.to_string());
        }
    }
    out
}

/// Inspects Rust, C99/C++17/Zig toolchains and embedded SDK headers without failing on optional tools.
pub fn inspect_environment(require_c: bool) -> DoctorReport {
    let host_abi_version = dag::host_abi_semver().to_string();
    let mut tools = Vec::new();

    match probe_command("cargo", &["--version"]) {
        Some(detail) => tools.push(ToolCheck {
            name: "cargo".to_string(),
            available: true,
            required: true,
            detail,
        }),
        None => tools.push(ToolCheck {
            name: "cargo".to_string(),
            available: false,
            required: true,
            detail: "cargo executable not found in PATH".to_string(),
        }),
    }

    match probe_command("rustc", &["--version"]) {
        Some(detail) => tools.push(ToolCheck {
            name: "rustc".to_string(),
            available: true,
            required: true,
            detail,
        }),
        None => tools.push(ToolCheck {
            name: "rustc".to_string(),
            available: false,
            required: true,
            detail: "rustc executable not found in PATH".to_string(),
        }),
    }

    let cc_found = probe_candidates(&c_candidates(), &["--version"])
        .or_else(|| cfg!(windows).then(|| probe_command("cl", &[])).flatten());
    tools.push(ToolCheck {
        name: "c99".to_string(),
        available: cc_found.is_some(),
        required: require_c,
        detail: cc_found
            .unwrap_or_else(|| "not found (set CC or install cc/gcc/clang)".to_string()),
    });

    let cxx_found = probe_candidates(&cxx_candidates(), &["--version"])
        .or_else(|| cfg!(windows).then(|| probe_command("cl", &[])).flatten());
    tools.push(ToolCheck {
        name: "cpp17".to_string(),
        available: cxx_found.is_some(),
        required: require_c,
        detail: cxx_found
            .unwrap_or_else(|| "not found (set CXX or install c++/g++/clang++)".to_string()),
    });

    let zig_found = probe_command("zig", &["version"]);
    tools.push(ToolCheck {
        name: "zig".to_string(),
        available: zig_found.is_some(),
        required: false,
        detail: zig_found.unwrap_or_else(|| "optional zig toolchain not installed".to_string()),
    });

    let headers_ok = sdk::C99_HEADER.contains("BAM_ABI_VERSION")
        && sdk::CPP17_HEADER.contains("BEVY_ABI_MOD_HPP")
        && !sdk::ZIG_SDK.trim().is_empty();
    tools.push(ToolCheck {
        name: "sdk_headers".to_string(),
        available: headers_ok,
        required: true,
        detail: format!(
            "embedded C99 ({} B), C++17 ({} B), Zig ({} B)",
            sdk::C99_HEADER.len(),
            sdk::CPP17_HEADER.len(),
            sdk::ZIG_SDK.len()
        ),
    });

    let ok = tools.iter().all(|t| !t.required || t.available);
    DoctorReport {
        host_abi_version,
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
