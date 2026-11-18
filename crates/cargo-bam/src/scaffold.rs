//! Guest mod project scaffolding (`cargo bam new`).

use crate::CliError;
use crate::sdk::{C99_HEADER, CPP17_HEADER, ZIG_SDK};
use clap::ValueEnum;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

/// Supported guest mod implementation languages for `cargo bam new`.
#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
pub enum ModLanguage {
    /// Rust `cdylib` crate using `bevy_abi_mod_guest`.
    Rust,
    /// Strict ISO C99 shared library using `bevy_abi_mod.h`.
    C99,
    /// Strict ISO C++17 shared library using `bevy_abi_mod.hpp`.
    Cpp17,
    /// Zig dynamic library using `bevy_abi_mod.zig`.
    Zig,
}

impl ModLanguage {
    /// Canonical lowercase identifier written into `mod.toml`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::C99 => "c99",
            Self::Cpp17 => "cpp17",
            Self::Zig => "zig",
        }
    }
}

impl fmt::Display for ModLanguage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

fn validate_mod_name(raw: &str) -> Result<String, CliError> {
    if raw.is_empty() {
        return Err(CliError::InvalidArgument(
            "mod name must not be empty".to_owned(),
        ));
    }
    if !raw
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(CliError::InvalidArgument(format!(
            "mod name '{raw}' may only contain ASCII letters, digits, '_' or '-'"
        )));
    }
    if raw.starts_with('-') || raw.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        return Err(CliError::InvalidArgument(format!(
            "mod name '{raw}' must start with an ASCII letter or '_'"
        )));
    }
    Ok(raw.replace('-', "_"))
}

fn default_guest_sdk_dep(guest_sdk_path: Option<&Path>) -> String {
    if let Some(p) = guest_sdk_path {
        let escaped = p.to_string_lossy().replace('\\', "/");
        return format!("bevy_abi_mod_guest = {{ path = \"{escaped}\", features = [\"derive\"] }}");
    }
    let workspace_guest = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("bevy_abi_mod_guest");
    if workspace_guest.is_dir() {
        let canonical = fs::canonicalize(&workspace_guest).unwrap_or(workspace_guest);
        let escaped = canonical.to_string_lossy().replace('\\', "/");
        return format!("bevy_abi_mod_guest = {{ path = \"{escaped}\", features = [\"derive\"] }}");
    }
    "bevy_abi_mod_guest = { version = \"0.2.0\", features = [\"derive\"] }".to_owned()
}

/// Scaffolds a new buildable `bevy_abi_mod` guest mod project in `parent_dir/<name>`.
pub fn scaffold_new_mod(
    name: &str,
    lang: ModLanguage,
    parent_dir: &Path,
    guest_sdk_path: Option<&Path>,
) -> Result<PathBuf, CliError> {
    let stem = validate_mod_name(name)?;
    let project_dir = parent_dir.join(name);
    if project_dir.exists() {
        return Err(CliError::AlreadyExists(project_dir));
    }

    fs::create_dir_all(project_dir.join("src"))?;

    let mod_toml = format!(
        "[mod]\nid = \"{stem}\"\nname = \"{name}\"\nversion = \"0.2.0\"\nabi_version = \"0.2.0\"\nlanguage = \"{}\"\nentry = \"{stem}\"\n\n[dependencies]\n",
        lang.as_str()
    );
    fs::write(project_dir.join("mod.toml"), mod_toml)?;

    match lang {
        ModLanguage::Rust => {
            let dep_line = default_guest_sdk_dep(guest_sdk_path);
            let cargo_toml = format!(
                "[package]\nname = \"{stem}\"\nversion = \"0.2.0\"\nedition = \"2024\"\npublish = false\n\n[workspace]\n\n[lib]\ncrate-type = [\"cdylib\"]\n\n[dependencies]\n{dep_line}\n"
            );
            fs::write(project_dir.join("Cargo.toml"), cargo_toml)?;

            let lib_rs = format!(
                r#"//! `{name}` guest mod (`ABI 0.2.0`).

use bevy_abi_mod_guest::prelude::*;
use core::ffi::CStr;

#[derive(Default)]
pub struct ModPlugin;

impl GuestPlugin for ModPlugin {{
    const NAME: &'static CStr = c"{stem}";
    const VERSION: &'static CStr = c"0.2.0";

    fn build(&mut self, app: &mut GuestApp<'_>) -> FfiResult {{
        app.system(c"heartbeat")
            .schedule(ScheduleId::UPDATE)
            .build(|_ctx| FfiResult::OK)
    }}
}}

export_mod!(plugin: ModPlugin);
"#
            );
            fs::write(project_dir.join("src").join("lib.rs"), lib_rs)?;
        }
        ModLanguage::C99 => {
            fs::create_dir_all(project_dir.join("include"))?;
            fs::write(
                project_dir.join("include").join("bevy_abi_mod.h"),
                C99_HEADER,
            )?;

            let mod_c = format!(
                r#"#include "bevy_abi_mod.h"
#include <stddef.h>
#include <stdint.h>
#include <string.h>

typedef struct ModState {{
    BamHostApi api;
}} ModState;

static ModState g_state;

static BamResult heartbeat_cb(BamSystemCtx* sys_ctx, void* user_data) {{
    (void)sys_ctx;
    (void)user_data;
    return BAM_RESULT_OK;
}}

static const BamManifest MANIFEST = {{
    (uint32_t)sizeof(BamManifest),
    BAM_ABI_VERSION,
    "{stem}",
    "0.2.0",
    0u,
    0u,
    NULL,
    NULL
}};

BAM_EXPORT const BamManifest* bevy_abi_mod_manifest(void) {{
    return &MANIFEST;
}}

BAM_EXPORT BamResult bevy_abi_mod_init(
    const BamHostApi* api,
    BamLoadCtx* load_ctx,
    BamInstance** instance_out
) {{
    if (!api || !load_ctx || !instance_out || !api->register_system) {{
        return BAM_RESULT_NULL_POINTER;
    }}
    g_state.api = *api;

    BamSystemDesc sys;
    memset(&sys, 0, sizeof(sys));
    sys.struct_size = (uint32_t)sizeof(BamSystemDesc);
    sys.name = "heartbeat";
    sys.callback = heartbeat_cb;
    sys.schedule = BAM_SCHEDULE_UPDATE;

    BamResult res = api->register_system(load_ctx, &sys);
    if (res != BAM_RESULT_OK) {{
        return res;
    }}
    *instance_out = (BamInstance*)&g_state;
    return BAM_RESULT_OK;
}}

BAM_EXPORT void bevy_abi_mod_shutdown(BamInstance* instance) {{
    (void)instance;
}}
"#
            );
            fs::write(project_dir.join("src").join("mod.c"), mod_c)?;
        }
        ModLanguage::Cpp17 => {
            fs::create_dir_all(project_dir.join("include"))?;
            fs::write(
                project_dir.join("include").join("bevy_abi_mod.h"),
                C99_HEADER,
            )?;
            fs::write(
                project_dir.join("include").join("bevy_abi_mod.hpp"),
                CPP17_HEADER,
            )?;

            let mod_cpp = format!(
                r#"#include "bevy_abi_mod.hpp"
#include <memory>

namespace {{

constexpr BamManifest kManifest = {{
    static_cast<uint32_t>(sizeof(BamManifest)),
    BAM_ABI_VERSION,
    "{stem}",
    "0.2.0",
    0u,
    0u
}};

class CppModInstance {{
public:
    explicit CppModInstance(const BamHostApi& api) noexcept : api_(api) {{}}

    BamResult init(BamLoadCtx* load_ctx) {{
        bam::LoadContextView ctx(api_, load_ctx);
        BamSystemDesc sys{{}};
        sys.struct_size = static_cast<uint32_t>(sizeof(BamSystemDesc));
        sys.name = "heartbeat";
        sys.callback = &CppModInstance::heartbeat_trampoline;
        sys.user_data = this;
        sys.schedule = BAM_SCHEDULE_UPDATE;
        return ctx.register_system(sys);
    }}

private:
    static BamResult heartbeat_trampoline(BamSystemCtx*, void*) {{
        return bam::guard_boundary([]() {{ return BAM_RESULT_OK; }});
    }}

    BamHostApi api_;
}};

}} // namespace

BAM_EXPORT const BamManifest* bevy_abi_mod_manifest(void) {{
    return &kManifest;
}}

BAM_EXPORT BamResult bevy_abi_mod_init(
    const BamHostApi* api,
    BamLoadCtx* load_ctx,
    BamInstance** instance_out
) {{
    if (api == nullptr || load_ctx == nullptr || instance_out == nullptr) {{
        return BAM_RESULT_NULL_POINTER;
    }}
    return bam::guard_boundary([&]() -> BamResult {{
        auto inst = std::make_unique<CppModInstance>(*api);
        BamResult res = inst->init(load_ctx);
        if (!bam::is_ok(res)) {{
            return res;
        }}
        *instance_out = reinterpret_cast<BamInstance*>(inst.release());
        return BAM_RESULT_OK;
    }});
}}

BAM_EXPORT void bevy_abi_mod_shutdown(BamInstance* instance) {{
    delete reinterpret_cast<CppModInstance*>(instance);
}}
"#
            );
            fs::write(project_dir.join("src").join("mod.cpp"), mod_cpp)?;
        }
        ModLanguage::Zig => {
            fs::create_dir_all(project_dir.join("include"))?;
            fs::write(
                project_dir.join("include").join("bevy_abi_mod.h"),
                C99_HEADER,
            )?;
            fs::write(
                project_dir.join("include").join("bevy_abi_mod.zig"),
                ZIG_SDK,
            )?;

            let build_zig = format!(
                r#"const std = @import("std");

pub fn build(b: *std.Build) void {{
    const target = b.standardTargetOptions(.{{}});
    const optimize = b.standardOptimizeOption(.{{}});

    const mod = b.createModule(.{{
        .root_source_file = b.path("src/root.zig"),
        .target = target,
        .optimize = optimize,
        .link_libc = true,
    }});
    mod.addIncludePath(b.path("include"));

    const lib = b.addLibrary(.{{
        .linkage = .dynamic,
        .name = "{stem}",
        .root_module = mod,
    }});

    b.installArtifact(lib);
}}
"#
            );
            fs::write(project_dir.join("build.zig"), build_zig)?;

            let root_zig = format!(
                r#"const std = @import("std");
const c = @cImport({{
    @cInclude("bevy_abi_mod.h");
}});

const ZigModInstance = struct {{
    api: c.BamHostApi,
}};

const MANIFEST: c.BamManifest = .{{
    .struct_size = @sizeOf(c.BamManifest),
    .abi_version = c.BAM_ABI_VERSION,
    .name = "{stem}",
    .version = "0.2.0",
    .flags = 0,
    .reserved0 = 0,
    .save_state = null,
    .restore_state = null,
}};

fn heartbeatCb(_: ?*c.BamSystemCtx, _: ?*anyopaque) callconv(.c) c.BamResult {{
    return c.BAM_RESULT_OK;
}}

pub export fn bevy_abi_mod_manifest() callconv(.c) *const c.BamManifest {{
    return &MANIFEST;
}}

pub export fn bevy_abi_mod_init(
    api: ?*const c.BamHostApi,
    load_ctx: ?*c.BamLoadCtx,
    instance_out: ?*?*c.BamInstance,
) callconv(.c) c.BamResult {{
    const host_api = api orelse return c.BAM_RESULT_NULL_POINTER;
    const ctx = load_ctx orelse return c.BAM_RESULT_NULL_POINTER;
    const out = instance_out orelse return c.BAM_RESULT_NULL_POINTER;
    const register_sys = host_api.register_system orelse return c.BAM_RESULT_NULL_POINTER;

    const inst = std.heap.c_allocator.create(ZigModInstance) catch return c.BAM_RESULT_INTERNAL_ERROR;
    inst.* = .{{ .api = host_api.* }};

    var sys = std.mem.zeroes(c.BamSystemDesc);
    sys.struct_size = @sizeOf(c.BamSystemDesc);
    sys.name = "heartbeat";
    sys.callback = heartbeatCb;
    sys.schedule = c.BAM_SCHEDULE_UPDATE;

    const rc = register_sys(ctx, &sys);
    if (rc != c.BAM_RESULT_OK) {{
        std.heap.c_allocator.destroy(inst);
        return rc;
    }}
    out.* = @ptrCast(inst);
    return c.BAM_RESULT_OK;
}}

pub export fn bevy_abi_mod_shutdown(instance: ?*c.BamInstance) callconv(.c) void {{
    const raw = instance orelse return;
    const inst: *ZigModInstance = @ptrCast(@alignCast(raw));
    std.heap.c_allocator.destroy(inst);
}}
"#
            );
            fs::write(project_dir.join("src").join("root.zig"), root_zig)?;
        }
    }

    Ok(project_dir)
}
