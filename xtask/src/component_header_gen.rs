//! Generated C99 component & VTable header emitter (`cargo xtask gen-component-headers`).
//!
//! Emits `include/generated/testbed_components.h` from the canonical `#[derive(ModComponent)]`
//! definitions shared by the host integration testbed and `tests/asymmetric_c_mod/mod.c`.

#![allow(dead_code, unreachable_pub)]

use bevy_abi_mod_macros::ModComponent;
use std::fs;
use std::path::Path;

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Default, ModComponent)]
#[mod_component(name = "testbed::Position")]
pub struct Position {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Default, ModComponent)]
#[mod_component(name = "testbed::Velocity")]
pub struct Velocity {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Default, ModComponent)]
#[mod_component(name = "testbed::CombatHealth")]
pub struct CombatHealth {
    pub current: i32,
    pub max: i32,
}

#[derive(Debug, Clone, PartialEq, Default, ModComponent)]
#[mod_component(name = "testbed::NameTag")]
pub struct NameTag {
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Default, ModComponent)]
#[mod_component(name = "testbed::MetadataTag")]
pub struct MetadataTag {
    pub label: String,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Default, ModComponent)]
#[mod_component(name = "testbed::Mass")]
pub struct Mass {
    pub value: f32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Default, ModComponent)]
#[mod_component(name = "testbed::Armor")]
pub struct Armor {
    pub defense: i32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Default, ModComponent)]
#[mod_component(name = "testbed::FilteredTransform")]
pub struct FilteredTransform {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Default, ModComponent)]
#[mod_component(name = "testbed::CFilteredTransform")]
pub struct CFilteredTransform {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Default, ModComponent)]
#[mod_component(name = "testbed::ChangeSensor")]
pub struct ChangeSensor {
    pub val: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Default, ModComponent)]
#[mod_component(name = "testbed::CChangeSensor")]
pub struct CChangeSensor {
    pub val: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Default, ModComponent)]
#[mod_component(name = "testbed::FreshSpawnTag")]
pub struct FreshSpawnTag {
    pub code: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Default, ModComponent)]
#[mod_component(name = "testbed::CFreshSpawnTag")]
pub struct CFreshSpawnTag {
    pub code: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Default, ModComponent)]
#[mod_component(name = "testbed::TransientMarker")]
pub struct TransientMarker {
    pub dummy: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Default, ModComponent)]
#[mod_component(name = "testbed::CTransientMarker")]
pub struct CTransientMarker {
    pub dummy: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Default, ModComponent)]
#[mod_component(name = "testbed::RustFilterTelemetry")]
pub struct RustFilterTelemetry {
    pub scale: f32,
    pub matched_rows: u32,
    pub optional_vel_hits: u32,
    pub _pad0: u32,
    pub spawned_default_entity: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Default, ModComponent)]
#[mod_component(name = "testbed::CFilterTelemetry")]
pub struct CFilterTelemetry {
    pub scale: f32,
    pub matched_rows: u32,
    pub optional_vel_hits: u32,
    pub _pad0: u32,
    pub spawned_default_entity: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Default, ModComponent)]
#[mod_component(name = "c_interop::Vec3")]
pub struct Vec3 {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

#[derive(Debug, Clone, PartialEq, Default, ModComponent)]
#[mod_component(name = "c_interop::CInteropComponent")]
pub struct CInteropComponent {
    pub health: i32,
    pub pos: Vec3,
    pub label: String,
}

/// Renders a standalone C99 + C++17 (`#ifdef __cplusplus` RAII / typed views) component header
/// from a slice of `(canonical_name, is_abi_pod, c_decl)` sections.
pub(crate) use bevy_abi_mod_sdk_gen::{render_component_header, section_for};

/// Renders the deterministic C99/C++17 header `include/generated/testbed_components.h`.
pub fn generate_testbed_components_header() -> String {
    render_component_header(
        "BEVY_ABI_MOD_GENERATED_TESTBED_COMPONENTS_H",
        "../bevy_abi_mod.h",
        &[
            section_for::<Position>(),
            section_for::<Velocity>(),
            section_for::<CombatHealth>(),
            section_for::<NameTag>(),
            section_for::<MetadataTag>(),
            section_for::<Mass>(),
            section_for::<Armor>(),
            section_for::<FilteredTransform>(),
            section_for::<CFilteredTransform>(),
            section_for::<ChangeSensor>(),
            section_for::<CChangeSensor>(),
            section_for::<FreshSpawnTag>(),
            section_for::<CFreshSpawnTag>(),
            section_for::<TransientMarker>(),
            section_for::<CTransientMarker>(),
            section_for::<RustFilterTelemetry>(),
            section_for::<CFilterTelemetry>(),
            section_for::<Vec3>(),
            section_for::<CInteropComponent>(),
        ],
    )
}

/// Writes `include/generated/testbed_components.h` or checks that it matches byte-for-byte.
pub fn write_or_check_component_headers(root: &Path, check_only: bool) -> Result<(), String> {
    let gen_dir = root.join("include").join("generated");
    let header_path = gen_dir.join("testbed_components.h");
    let expected = generate_testbed_components_header();

    if check_only {
        let existing_raw = fs::read_to_string(&header_path).map_err(|e| {
            format!(
                "missing {} ({e}); run `cargo xtask gen-component-headers`",
                header_path.display()
            )
        })?;
        let existing = existing_raw.replace("\r\n", "\n");
        if existing != expected {
            return Err(format!(
                "`{}` is out of date with `ModComponent::C_DECL`; run `cargo xtask gen-component-headers` and commit the result",
                header_path.display()
            ));
        }
        println!(
            "==> Verified `{}` matches `ModComponent::C_DECL` byte-for-byte",
            header_path.display()
        );
        return Ok(());
    }

    fs::create_dir_all(&gen_dir)
        .map_err(|e| format!("failed to create {}: {e}", gen_dir.display()))?;
    fs::write(&header_path, expected)
        .map_err(|e| format!("failed to write {}: {e}", header_path.display()))?;
    println!(
        "==> Wrote generated component header {}",
        header_path.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy_abi_mod_sys::FfiOption;

    #[repr(C)]
    #[derive(Debug, Clone, Copy, Default, PartialEq, ModComponent)]
    #[mod_component(name = "header_test::OptPod", summary_tick, require(Position))]
    struct OptPod {
        pub pos: Position,
        pub target_id: FfiOption<u32>,
    }

    #[derive(Debug, Clone, Default, PartialEq, ModComponent)]
    #[mod_component(name = "header_test::HostBag")]
    struct HostBag {
        pub active: bool,
        pub label: String,
        pub scores: Vec<u32>,
        pub bonus: FfiOption<i32>,
    }

    #[repr(u8)]
    #[derive(Debug, Clone, Copy, PartialEq, Eq, ModComponent)]
    #[mod_component(name = "header_test::Mode")]
    enum Mode {
        Idle = 0,
        Active = 1,
    }

    #[repr(C, u8)]
    #[derive(Debug, Clone, PartialEq, ModComponent)]
    #[mod_component(name = "header_test::StateMachine")]
    enum StateMachine {
        Off = 0,
        Running { speed: f32, tag: String } = 1,
        Paused(Vec<u16>, FfiOption<u8>) = 2,
    }

    struct TempTestDir(std::path::PathBuf);
    impl Drop for TempTestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn generated_headers_compile_under_strict_c99_and_cpp17() {
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let root = manifest_dir.parent().expect("workspace root");
        let include_dir = root.join("include");

        let tmp_dir =
            std::env::temp_dir().join(format!("bam_comp_hdr_test_{}", std::process::id()));
        let gen_subdir = tmp_dir.join("generated");
        fs::create_dir_all(&gen_subdir).expect("create temp test dir");
        let _guard = TempTestDir(tmp_dir.clone());
        fs::copy(
            include_dir.join("bevy_abi_mod.h"),
            tmp_dir.join("bevy_abi_mod.h"),
        )
        .expect("copy bevy_abi_mod.h");
        let testbed_hdr = gen_subdir.join("testbed.h");
        fs::write(&testbed_hdr, generate_testbed_components_header()).expect("write testbed.h");

        let full_hdr_path = tmp_dir.join("all_features.h");
        let full_hdr = render_component_header(
            "ALL_FEATURES_H",
            "bevy_abi_mod.h",
            &[
                section_for::<Position>(),
                section_for::<OptPod>(),
                section_for::<HostBag>(),
                section_for::<Mode>(),
                section_for::<StateMachine>(),
            ],
        );
        fs::write(&full_hdr_path, full_hdr).expect("write all_features.h");

        // 1. Strict ISO C99/C11 compilation check
        let cc = crate::c_compiler();
        for hdr in [&testbed_hdr, &full_hdr_path] {
            let status = crate::c_syntax_only_cmd(&cc, Some(&include_dir), hdr)
                .status()
                .expect("spawn C compiler");
            assert!(
                status.success(),
                "C syntax check failed for {}",
                hdr.display()
            );
        }

        // 2. Strict ISO C++17 compilation check (including template views & HostVecView)
        let cxx = crate::cxx_compiler();
        for hdr in [&testbed_hdr, &full_hdr_path] {
            let status = crate::cxx17_syntax_only_cmd(&cxx, Some(&include_dir), hdr)
                .status()
                .expect("spawn C++17 compiler");
            assert!(
                status.success(),
                "C++17 syntax check failed for {}",
                hdr.display()
            );
        }
    }
}
