#![deny(unsafe_op_in_unsafe_fn)]
#![deny(improper_ctypes_definitions)]

//! Real `rustc` subprocess compile-fail test suite for `#[derive(AbiPod)]` and `#[derive(ModExposable)]`.
//!
//! Unlike pure `syn` token-level checks, invoking `rustc --emit=metadata` validates both:
//! 1. Procedural macro AST diagnostics (`missing #[repr(C)]`, `packed`, `enum`, `generics`), and
//! 2. Post-expansion Rust type-system / trait-bound rejections (`!AbiPod`, `!FfiLayout`, `!Copy`,
//!    `needs_drop`, and `Vec<BadElem>` inside `#[derive(ModExposable)]`).

// Force Cargo to build and link both `abi_contract` and `mod_derive` before running this test binary.
use abi_contract::ExposableComponent as _;
use mod_derive::{AbiPod, ModExposable};

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;
use std::time::SystemTime;

#[repr(C)]
#[derive(Copy, Clone, AbiPod)]
struct LinkAnchorPod {
    x: f32,
}

#[repr(C)]
#[derive(Copy, Clone, ModExposable)]
struct LinkAnchorComp {
    val: LinkAnchorPod,
}

#[test]
fn link_anchor_traits_are_live() {
    fn assert_pod<T: abi_contract::AbiPod>() {}
    fn assert_layout<T: abi_contract::FfiLayout>() {}
    assert_pod::<LinkAnchorPod>();
    assert_pod::<LinkAnchorComp>();
    assert_layout::<LinkAnchorComp>();
    assert_eq!(LinkAnchorComp::COMPONENT_NAME, "LinkAnchorComp");
}

struct CompilerArtifacts {
    rustc: PathBuf,
    abi_contract_rlib: PathBuf,
    mod_derive_dylib: PathBuf,
    deps_dirs: Vec<PathBuf>,
    scratch_dir: PathBuf,
}

static ARTIFACTS: OnceLock<CompilerArtifacts> = OnceLock::new();
static SNIPPET_COUNTER: AtomicUsize = AtomicUsize::new(0);

fn mtime(path: &Path) -> SystemTime {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

fn collect_matching_files(dirs: &[PathBuf], prefix: &str, extensions: &[&str]) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for dir in dirs {
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            let Some(ext) = path.extension().and_then(|s| s.to_str()) else {
                continue;
            };
            let matches_prefix = name.starts_with(prefix)
                || name
                    .strip_prefix("lib")
                    .is_some_and(|s| s.starts_with(prefix));
            if matches_prefix && extensions.contains(&ext) && !found.contains(&path) {
                found.push(path);
            }
        }
    }
    found.sort_by_key(|a| std::cmp::Reverse(mtime(a)));
    found
}

fn run_rustc_with(
    rustc: &Path,
    abi_contract_rlib: &Path,
    mod_derive_dylib: &Path,
    deps_dirs: &[PathBuf],
    scratch_dir: &Path,
    label: &str,
    source: &str,
) -> (bool, String) {
    let id = SNIPPET_COUNTER.fetch_add(1, Ordering::SeqCst);
    let src_path = scratch_dir.join(format!("{label}_{id}.rs"));
    let out_path = scratch_dir.join(format!("lib{label}_{id}.rmeta"));

    fs::write(&src_path, source).expect("failed to write compile-fail snippet");

    let mut cmd = Command::new(rustc);
    cmd.arg("--edition=2021")
        .arg("--crate-type=lib")
        .arg("--crate-name")
        .arg(format!("cf_{id}"))
        .arg("--emit=metadata")
        .arg("-o")
        .arg(&out_path)
        .arg("--extern")
        .arg(format!("abi_contract={}", abi_contract_rlib.display()))
        .arg("--extern")
        .arg(format!("mod_derive={}", mod_derive_dylib.display()))
        .arg("--color=never");

    for dir in deps_dirs {
        cmd.arg("-L").arg(format!("dependency={}", dir.display()));
    }

    cmd.arg(&src_path);

    let output = cmd.output().expect("failed to invoke rustc");
    let _ = fs::remove_file(&src_path);
    let _ = fs::remove_file(&out_path);

    let mut combined = String::from_utf8_lossy(&output.stdout).into_owned();
    combined.push_str(&String::from_utf8_lossy(&output.stderr));
    (output.status.success(), combined)
}

fn compiler_artifacts() -> &'static CompilerArtifacts {
    ARTIFACTS.get_or_init(|| {
        let rustc = env::var_os("RUSTC")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("rustc"));

        let exe = env::current_exe().expect("current_exe");
        let exe_dir = exe.parent().expect("exe parent").to_path_buf();

        let mut deps_dirs = vec![exe_dir.clone()];
        if let Some(parent) = exe_dir.parent() {
            let sibling_deps = parent.join("deps");
            if sibling_deps.is_dir() && !deps_dirs.contains(&sibling_deps) {
                deps_dirs.push(sibling_deps);
            }
            // When `--target <triple>` is passed, host proc-macro dylibs live in `target/debug/deps`
            // or `target/release/deps` two levels above `target/<triple>/<profile>/deps`.
            if let Some(profile_name) = parent.file_name().and_then(|n| n.to_str()) {
                if let Some(target_root) = parent.parent().and_then(|p| p.parent()) {
                    let host_deps = target_root.join(profile_name).join("deps");
                    if host_deps.is_dir() && !deps_dirs.contains(&host_deps) {
                        deps_dirs.push(host_deps);
                    }
                }
            }
        }

        let rlibs = collect_matching_files(&deps_dirs, "abi_contract", &["rlib"]);
        assert!(
            !rlibs.is_empty(),
            "could not locate libabi_contract*.rlib in {deps_dirs:?}"
        );

        let dylibs = collect_matching_files(&deps_dirs, "mod_derive", &["dylib", "so", "dll"]);
        assert!(
            !dylibs.is_empty(),
            "could not locate mod_derive proc-macro library in {deps_dirs:?}"
        );

        let scratch_dir = env::temp_dir().join(format!(
            "bevy_abi_mod_compile_fail_{}",
            std::process::id()
        ));
        fs::create_dir_all(&scratch_dir).expect("create compile_fail scratch dir");

        let positive_probe = r#"
            use abi_contract::{AbiPod, FfiLayout};
            use mod_derive::{AbiPod, ModExposable};

            #[repr(C)]
            #[derive(Copy, Clone, AbiPod)]
            pub struct ProbePod {
                pub x: f32,
                pub y: u32,
            }

            #[repr(C)]
            #[derive(Copy, Clone, ModExposable)]
            pub struct ProbeComp {
                pub inner: ProbePod,
            }

            #[derive(ModExposable)]
            pub struct ProbeVecHolder {
                pub items: Vec<ProbeComp>,
            }

            pub fn check_bounds<T: AbiPod + FfiLayout>() {}
            pub fn verify() {
                check_bounds::<ProbePod>();
                check_bounds::<ProbeComp>();
            }
        "#;

        let mut last_err = String::new();
        for rlib in &rlibs {
            for dylib in &dylibs {
                let (ok, diag) = run_rustc_with(
                    &rustc,
                    rlib,
                    dylib,
                    &deps_dirs,
                    &scratch_dir,
                    "positive_probe",
                    positive_probe,
                );
                if ok {
                    return CompilerArtifacts {
                        rustc,
                        abi_contract_rlib: rlib.clone(),
                        mod_derive_dylib: dylib.clone(),
                        deps_dirs,
                        scratch_dir,
                    };
                }
                last_err = diag;
            }
        }

        panic!(
            "positive probe snippet failed against all candidate (abi_contract, mod_derive) pairs.\nLast rustc output:\n{last_err}"
        );
    })
}

fn assert_compiles(label: &str, source: &str) {
    let env = compiler_artifacts();
    let (ok, diag) = run_rustc_with(
        &env.rustc,
        &env.abi_contract_rlib,
        &env.mod_derive_dylib,
        &env.deps_dirs,
        &env.scratch_dir,
        label,
        source,
    );
    assert!(
        ok,
        "expected `{label}` to compile cleanly, but rustc failed:\n{diag}"
    );
}

fn assert_compile_fails(label: &str, source: &str, expected_substrings_any: &[&str]) {
    let env = compiler_artifacts();
    let (ok, diag) = run_rustc_with(
        &env.rustc,
        &env.abi_contract_rlib,
        &env.mod_derive_dylib,
        &env.deps_dirs,
        &env.scratch_dir,
        label,
        source,
    );
    assert!(
        !ok,
        "expected `{label}` to FAIL compilation, but rustc succeeded!\nSource:\n{source}"
    );
    assert!(
        expected_substrings_any.iter().any(|needle| diag.contains(needle)),
        "expected `{label}` compiler diagnostics to contain at least one of {expected_substrings_any:?}, got:\n{diag}"
    );
}

#[test]
#[cfg_attr(miri, ignore)]
fn positive_control_valid_abi_pod_and_mod_exposable_compile() {
    assert_compiles(
        "valid_positive_control",
        r#"
        use abi_contract::{AbiPod, ExposableComponent, FfiEntity, FfiLayout};
        use mod_derive::{AbiPod, ModExposable};

        #[repr(C)]
        #[derive(Copy, Clone, AbiPod)]
        pub struct ValidC {
            pub entity: FfiEntity,
            pub coords: [f32; 3],
            pub flags: u32,
        }

        #[repr(transparent)]
        #[derive(Copy, Clone, AbiPod)]
        pub struct ValidTransparent(pub u64);

        #[repr(C)]
        #[derive(Copy, Clone, ModExposable)]
        pub struct ValidPodComp {
            pub c: ValidC,
            pub id: ValidTransparent,
        }

        #[derive(ModExposable)]
        pub struct ValidVecConsumer {
            pub list: Vec<ValidPodComp>,
            pub name: String,
            pub enabled: bool,
        }

        const _: () = {
            assert!(<ValidPodComp as ExposableComponent>::IS_POD);
            assert!(<ValidPodComp as ExposableComponent>::IS_RAW_INSERTABLE);
            assert!(!<ValidVecConsumer as ExposableComponent>::IS_POD);
            assert!(!<ValidVecConsumer as ExposableComponent>::IS_RAW_INSERTABLE);
        };

        pub fn require_all<T: AbiPod + FfiLayout>() {}
        pub fn test_bounds() {
            require_all::<ValidC>();
            require_all::<ValidTransparent>();
            require_all::<ValidPodComp>();
        }
        "#,
    );
}

#[test]
#[cfg_attr(miri, ignore)]
fn abi_pod_rejects_missing_repr_c_and_packed_and_enums_and_generics() {
    // 1. Struct without #[repr(C)] or #[repr(transparent)]
    assert_compile_fails(
        "abi_pod_no_repr_c",
        r#"
        use mod_derive::AbiPod;

        #[derive(Copy, Clone, AbiPod)]
        pub struct MissingReprC {
            pub x: f32,
            pub y: f32,
        }
        "#,
        &["requires `#[repr(C)]` or `#[repr(transparent)]`"],
    );

    // 2. #[repr(C, packed)] struct
    assert_compile_fails(
        "abi_pod_repr_c_packed",
        r#"
        use mod_derive::AbiPod;

        #[repr(C, packed)]
        #[derive(Copy, Clone, AbiPod)]
        pub struct PackedStruct {
            pub tag: u8,
            pub value: u32,
        }
        "#,
        &["packed"],
    );

    // 3. Enum (even with #[repr(u32)])
    assert_compile_fails(
        "abi_pod_enum",
        r#"
        use mod_derive::AbiPod;

        #[repr(u32)]
        #[derive(Copy, Clone, AbiPod)]
        pub enum StateEnum {
            Idle = 0,
            Moving = 1,
        }
        "#,
        &["can only be applied to structs"],
    );

    // 4. Generic struct
    assert_compile_fails(
        "abi_pod_generic",
        r#"
        use mod_derive::AbiPod;

        #[repr(C)]
        #[derive(Copy, Clone, AbiPod)]
        pub struct GenericPod<T> {
            pub value: T,
        }
        "#,
        &["does not support generic structs"],
    );
}

#[test]
#[cfg_attr(miri, ignore)]
fn abi_pod_rejects_bool_string_vec_and_drop_types() {
    // 1. Struct with `bool` field (not bit-valid across 0..=255)
    assert_compile_fails(
        "abi_pod_bool_field",
        r#"
        use mod_derive::AbiPod;

        #[repr(C)]
        #[derive(Copy, Clone, AbiPod)]
        pub struct BoolInPod {
            pub active: bool,
            pub _pad: [u8; 3],
            pub score: u32,
        }
        "#,
        &["AbiPod", "bool"],
    );

    // 2. Struct with `String` field
    assert_compile_fails(
        "abi_pod_string_field",
        r#"
        use mod_derive::AbiPod;

        #[repr(C)]
        #[derive(AbiPod)]
        pub struct StringInPod {
            pub label: String,
        }
        "#,
        &["AbiPod", "Copy", "needs_drop"],
    );

    // 3. Struct with `Vec<u8>` field
    assert_compile_fails(
        "abi_pod_vec_u8_field",
        r#"
        use mod_derive::AbiPod;

        #[repr(C)]
        #[derive(AbiPod)]
        pub struct VecInPod {
            pub payload: Vec<u8>,
        }
        "#,
        &["AbiPod", "Copy", "needs_drop"],
    );

    // 4a. Struct implementing `Drop` directly
    assert_compile_fails(
        "abi_pod_direct_drop",
        r#"
        use mod_derive::AbiPod;

        #[repr(C)]
        #[derive(AbiPod)]
        pub struct DirectDropPod {
            pub fd: i32,
        }

        impl Drop for DirectDropPod {
            fn drop(&mut self) {}
        }
        "#,
        &["Copy", "needs_drop"],
    );

    // 4b. Struct containing a custom `Drop` field
    assert_compile_fails(
        "abi_pod_nested_drop_field",
        r#"
        use mod_derive::AbiPod;

        #[repr(C)]
        pub struct CustomDroppable(pub u64);
        impl Drop for CustomDroppable {
            fn drop(&mut self) {}
        }

        #[repr(C)]
        #[derive(AbiPod)]
        pub struct OuterWithDroppable {
            pub handle: CustomDroppable,
        }
        "#,
        &["AbiPod", "Copy", "needs_drop"],
    );
}

#[test]
#[cfg_attr(miri, ignore)]
fn mod_exposable_with_string_or_bool_does_not_impl_abi_pod_or_ffi_layout() {
    // 1. ModExposable with String does NOT implement AbiPod
    assert_compile_fails(
        "mod_exposable_string_not_abi_pod",
        r#"
        use abi_contract::AbiPod;
        use mod_derive::ModExposable;

        #[repr(C)]
        #[derive(ModExposable)]
        pub struct NamedComp {
            pub title: String,
        }

        fn require_pod<T: AbiPod>() {}
        pub fn probe() {
            require_pod::<NamedComp>();
        }
        "#,
        &["AbiPod` is not implemented for `NamedComp`", "AbiPod"],
    );

    // 2. ModExposable with String does NOT implement FfiLayout
    assert_compile_fails(
        "mod_exposable_string_not_ffi_layout",
        r#"
        use abi_contract::FfiLayout;
        use mod_derive::ModExposable;

        #[repr(C)]
        #[derive(ModExposable)]
        pub struct NamedComp {
            pub title: String,
        }

        fn require_layout<T: FfiLayout>() {}
        pub fn probe() {
            require_layout::<NamedComp>();
        }
        "#,
        &["FfiLayout` is not implemented for `NamedComp`", "FfiLayout"],
    );

    // 3. ModExposable with String CANNOT be used as `Vec<NamedComp>` inside another ModExposable struct
    assert_compile_fails(
        "mod_exposable_vec_of_string_comp_rejected",
        r#"
        use mod_derive::ModExposable;

        #[repr(C)]
        #[derive(ModExposable)]
        pub struct NamedComp {
            pub title: String,
        }

        #[derive(ModExposable)]
        pub struct OuterInventory {
            pub bad_items: Vec<NamedComp>,
        }
        "#,
        &[
            "AbiPod` is not implemented for `NamedComp`",
            "assert_vec_elem_ffi",
        ],
    );

    // 4. ModExposable with `bool` (even `#[repr(C)]` + `Copy`) does NOT implement AbiPod
    assert_compile_fails(
        "mod_exposable_bool_not_abi_pod",
        r#"
        use abi_contract::AbiPod;
        use mod_derive::ModExposable;

        #[repr(C)]
        #[derive(Copy, Clone, ModExposable)]
        pub struct BoolComp {
            pub flag: bool,
            pub count: u32,
        }

        fn require_pod<T: AbiPod>() {}
        pub fn probe() {
            require_pod::<BoolComp>();
        }
        "#,
        &["AbiPod` is not implemented for `BoolComp`", "AbiPod"],
    );

    // 5. ModExposable with `bool` does NOT implement FfiLayout
    assert_compile_fails(
        "mod_exposable_bool_not_ffi_layout",
        r#"
        use abi_contract::FfiLayout;
        use mod_derive::ModExposable;

        #[repr(C)]
        #[derive(Copy, Clone, ModExposable)]
        pub struct BoolComp {
            pub flag: bool,
            pub count: u32,
        }

        fn require_layout<T: FfiLayout>() {}
        pub fn probe() {
            require_layout::<BoolComp>();
        }
        "#,
        &["FfiLayout` is not implemented for `BoolComp`", "FfiLayout"],
    );

    // 6. ModExposable with `bool` CANNOT be used as `Vec<BoolComp>` inside another ModExposable struct
    assert_compile_fails(
        "mod_exposable_vec_of_bool_comp_rejected",
        r#"
        use mod_derive::ModExposable;

        #[repr(C)]
        #[derive(Copy, Clone, ModExposable)]
        pub struct BoolComp {
            pub flag: bool,
            pub count: u32,
        }

        #[derive(ModExposable)]
        pub struct OuterFlags {
            pub flags: Vec<BoolComp>,
        }
        "#,
        &[
            "AbiPod` is not implemented for `BoolComp`",
            "assert_vec_elem_ffi",
        ],
    );
}

#[test]
#[cfg_attr(miri, ignore)]
fn mod_exposable_rejects_repr_rust_vec_elements_and_non_opaque_non_pod_nesting() {
    // 1. `#[repr(Rust)]` ModExposable struct does NOT implement `AbiPod`, so `Vec<ReprRustPos>` is rejected
    assert_compile_fails(
        "mod_exposable_vec_of_repr_rust_rejected",
        r#"
        use mod_derive::ModExposable;

        #[derive(Copy, Clone, ModExposable)]
        pub struct ReprRustPos {
            pub x: f32,
            pub y: f32,
        }

        #[derive(ModExposable)]
        pub struct PathWaypoints {
            pub points: Vec<ReprRustPos>,
        }
        "#,
        &[
            "AbiPod` is not implemented for `ReprRustPos`",
            "assert_vec_elem_ffi",
        ],
    );

    // 2. Nesting a `String`-owning ModExposable struct without `#[mod_component(opaque)]` is rejected
    assert_compile_fails(
        "mod_exposable_non_opaque_nested_string_rejected",
        r#"
        use mod_derive::ModExposable;

        #[derive(ModExposable)]
        pub struct InnerLabel {
            pub text: String,
        }

        #[derive(ModExposable)]
        pub struct NonOpaqueOuter {
            pub label: InnerLabel,
        }
        "#,
        &[
            "FfiLayout` is not implemented for `InnerLabel`",
            "assert_nested_ffi",
        ],
    );

    // 3. `#[mod_component(no_pod)]` suppresses `AbiPod` even on `#[repr(C)]` Copy structs
    assert_compile_fails(
        "mod_exposable_no_pod_suppresses_abi_pod",
        r#"
        use abi_contract::AbiPod;
        use mod_derive::ModExposable;

        #[repr(C)]
        #[derive(Copy, Clone, ModExposable)]
        #[mod_component(no_pod)]
        pub struct ExplicitNoPod {
            pub value: u64,
        }

        fn require_pod<T: AbiPod>() {}
        pub fn probe() {
            require_pod::<ExplicitNoPod>();
        }
        "#,
        &["AbiPod` is not implemented for `ExplicitNoPod`", "AbiPod"],
    );
}
