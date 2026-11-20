//! Security validation tests for `bevy_abi_mod_manifest`: strict `[mod].id` and `[mod].entry`
//! sanitization, Windows/NTFS hazards, bounded manifest reads, and symlink confinement.

use bevy_abi_mod_manifest::{
    MAX_ENTRY_PATH_DEPTH, MAX_ENTRY_PATH_LEN, MAX_MANIFEST_BYTES, MAX_MOD_ID_LEN, ManifestError,
    ModTomlManifest, validate_manifest_entry_path, validate_mod_id,
};
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use tempfile::TempDir;

#[test]
fn rejects_unsafe_mod_ids_in_parse_and_validator() {
    let invalid_ids = [
        "",
        "..",
        ".",
        "../evil",
        "../../escape",
        "foo/bar",
        "foo\\bar",
        "/abs",
        "C:evil",
        "\\\\server\\share",
        "mod:ads:$DATA",
        "bad\0id",
        "bad\nid",
        "bad\rid",
        "bad\x01id",
        "bad\x7fid",
        "-leading_dash",
        "has space",
        "trailing_space ",
        "has.dot",
        "CON",
        "con",
        "PRN",
        "AUX",
        "NUL",
        "COM1",
        "com9",
        "LPT1",
        "lpt9",
    ];

    for bad in invalid_ids {
        assert!(
            validate_mod_id(bad).is_err(),
            "validate_mod_id must reject {bad:?}"
        );
        if !bad.is_empty() && !bad.trim().is_empty() {
            let escaped = toml::Value::String(bad.to_owned()).to_string();
            let doc_id = format!("[mod]\nid = {escaped}\nversion = \"1.0.0\"\n");
            assert!(
                ModTomlManifest::parse_str(&doc_id, "mod/mod.toml").is_err(),
                "parse_str([mod].id) must reject {bad:?}"
            );
            let doc_name = format!("[mod]\nname = {escaped}\nversion = \"1.0.0\"\n");
            assert!(
                ModTomlManifest::parse_str(&doc_name, "mod/mod.toml").is_err(),
                "parse_str([mod].name fallback) must reject {bad:?}"
            );
        }
    }

    let too_long = "a".repeat(MAX_MOD_ID_LEN + 1);
    assert!(validate_mod_id(&too_long).is_err());

    // Valid canonical IDs with human-readable display name still succeed.
    let ok = "[mod]\nid = \"core_mod-1\"\nname = \"Core Mod (v1)\"\nversion = \"1.0.0\"\n";
    let parsed = ModTomlManifest::parse_str(ok, "mod/mod.toml").unwrap();
    assert_eq!(parsed.id(), "core_mod-1");
    assert_eq!(parsed.display_name.as_deref(), Some("Core Mod (v1)"));
}

#[test]
fn rejects_unsafe_entry_paths_across_traversal_windows_ntfs_and_control_bytes() {
    let invalid_entries = [
        "",
        ".",
        "..",
        "../evil.so",
        "../../../../tmp/evil.so",
        "bin/../evil.so",
        "./libmod.so",
        "bin/./libmod.so",
        "/usr/lib/evil.so",
        "/evil.so",
        "bin/",
        "bin//libmod.so",
        "bin\\libmod.dll",
        "\\\\server\\share\\evil.dll",
        "C:\\evil.dll",
        "C:/evil.dll",
        "C:evil.dll",
        "libmod.dll:evil:$DATA",
        "bin/libmod.dll:stream",
        "lib\0mod.so",
        "lib\nmod.so",
        "lib\rmod.so",
        "lib\x1fmod.so",
        "lib\x7fmod.so",
        "libmod.so.",
        "libmod.so ",
        "dir./libmod.so",
        "dir /libmod.so",
        "CON",
        "con.dll",
        "PRN.so",
        "AUX.dylib",
        "NUL.dll",
        "bin/COM1.dll",
        "bin/com9.so",
        "bin/LPT1.dylib",
        "bin/lpt9",
    ];

    for bad in invalid_entries {
        assert!(
            validate_manifest_entry_path(bad).is_err(),
            "validate_manifest_entry_path must reject {bad:?}"
        );
        let escaped = toml::Value::String(bad.to_owned()).to_string();
        let doc_entry = format!("[mod]\nid = \"ok\"\nversion = \"1.0.0\"\nentry = {escaped}\n");
        assert!(
            ModTomlManifest::parse_str(&doc_entry, "mod/mod.toml").is_err(),
            "parse_str([mod].entry) must reject {bad:?}"
        );
        let doc_lib = format!("[mod]\nid = \"ok\"\nversion = \"1.0.0\"\nlibrary = {escaped}\n");
        assert!(
            ModTomlManifest::parse_str(&doc_lib, "mod/mod.toml").is_err(),
            "parse_str([mod].library) must reject {bad:?}"
        );
    }

    // Excessive depth and length rejected.
    let too_deep = vec!["d"; MAX_ENTRY_PATH_DEPTH + 1].join("/");
    assert!(validate_manifest_entry_path(&too_deep).is_err());

    let too_long = format!("bin/{}.so", "x".repeat(MAX_ENTRY_PATH_LEN));
    assert!(validate_manifest_entry_path(&too_long).is_err());

    // Valid nested relative paths succeed.
    assert!(validate_manifest_entry_path("libmod.so").is_ok());
    assert!(validate_manifest_entry_path("bin/x64/my_mod.dll").is_ok());
}

#[test]
fn resolve_library_path_rejects_struct_tampering_and_confines_nested_files() {
    let tmp = TempDir::new().unwrap();
    let mod_dir = tmp.path().join("my_mod");
    fs::create_dir_all(mod_dir.join("bin")).unwrap();
    let real_lib = mod_dir.join("bin/libmy_mod.so");
    fs::write(&real_lib, b"fake-lib").unwrap();

    let manifest_path = mod_dir.join("mod.toml");
    let valid = ModTomlManifest::parse_str(
        "[mod]\nid = \"my_mod\"\nversion = \"1.0.0\"\nentry = \"bin/libmy_mod.so\"\n",
        &manifest_path,
    )
    .unwrap();
    assert_eq!(valid.resolve_library_path().unwrap(), real_lib);

    // Manually constructed struct bypassing `parse_str` still fails closed!
    let forged_entry = ModTomlManifest {
        name: "my_mod".to_owned(),
        display_name: None,
        version: semver::Version::new(1, 0, 0),
        abi_version: None,
        entry: Some("../outside.so".to_owned()),
        language: None,
        dependencies: BTreeMap::new(),
        optional_dependencies: BTreeMap::new(),
        manifest_path: manifest_path.clone(),
    };
    assert!(matches!(
        forged_entry.resolve_library_path(),
        Err(ManifestError::Io { .. })
    ));

    let forged_id = ModTomlManifest {
        name: "../../outside".to_owned(),
        display_name: None,
        version: semver::Version::new(1, 0, 0),
        abi_version: None,
        entry: None,
        language: None,
        dependencies: BTreeMap::new(),
        optional_dependencies: BTreeMap::new(),
        manifest_path,
    };
    assert!(matches!(
        forged_id.resolve_library_path(),
        Err(ManifestError::Io { .. })
    ));
}

#[cfg(unix)]
#[test]
fn resolve_library_path_rejects_file_dir_and_fallback_symlink_escapes() {
    use std::os::unix::fs::symlink;

    let tmp = TempDir::new().unwrap();
    let outside_dir = tmp.path().join("outside");
    fs::create_dir_all(&outside_dir).unwrap();
    let outside_lib = outside_dir.join("evil.dylib");
    fs::write(&outside_lib, b"evil-payload").unwrap();

    // 1. Direct symlink via `[mod].entry`.
    let mod1 = tmp.path().join("mod1");
    fs::create_dir_all(&mod1).unwrap();
    symlink(&outside_lib, mod1.join("entry.dylib")).unwrap();
    let m1 = ModTomlManifest::parse_str(
        "[mod]\nid = \"mod1\"\nversion = \"1.0.0\"\nentry = \"entry.dylib\"\n",
        mod1.join("mod.toml"),
    )
    .unwrap();
    let err1 = m1.resolve_library_path().unwrap_err();
    assert!(
        matches!(
            err1,
            ManifestError::Io { ref source, .. }
                if source.kind() == std::io::ErrorKind::PermissionDenied
        ),
        "expected PermissionDenied for symlinked entry, got {err1:?}"
    );

    // 2. Intermediate symlinked directory (`bin -> outside`).
    let mod2 = tmp.path().join("mod2");
    fs::create_dir_all(&mod2).unwrap();
    symlink(&outside_dir, mod2.join("bin")).unwrap();
    let m2 = ModTomlManifest::parse_str(
        "[mod]\nid = \"mod2\"\nversion = \"1.0.0\"\nentry = \"bin/evil.dylib\"\n",
        mod2.join("mod.toml"),
    )
    .unwrap();
    let err2 = m2.resolve_library_path().unwrap_err();
    assert!(
        matches!(
            err2,
            ManifestError::Io { ref source, .. }
                if source.kind() == std::io::ErrorKind::PermissionDenied
        ),
        "expected PermissionDenied for symlinked parent dir, got {err2:?}"
    );

    // 3. Fallback default name (`libmod3.<ext>`) pointing to symlink outside root.
    let mod3 = tmp.path().join("mod3");
    fs::create_dir_all(&mod3).unwrap();
    let default_name = format!(
        "{}mod3.{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_EXTENSION
    );
    symlink(&outside_lib, mod3.join(&default_name)).unwrap();
    let m3 = ModTomlManifest::parse_str(
        "[mod]\nid = \"mod3\"\nversion = \"1.0.0\"\n",
        mod3.join("mod.toml"),
    )
    .unwrap();
    let err3 = m3.resolve_library_path().unwrap_err();
    assert!(
        matches!(
            err3,
            ManifestError::Io { ref source, .. }
                if source.kind() == std::io::ErrorKind::PermissionDenied
        ),
        "expected PermissionDenied for symlinked fallback library, got {err3:?}"
    );

    // 4. Single-library read_dir fallback pointing to symlink outside root.
    let mod4 = tmp.path().join("mod4");
    fs::create_dir_all(&mod4).unwrap();
    let other_name = format!("something_else.{}", std::env::consts::DLL_EXTENSION);
    symlink(&outside_lib, mod4.join(&other_name)).unwrap();
    let m4 = ModTomlManifest::parse_str(
        "[mod]\nid = \"mod4\"\nversion = \"1.0.0\"\n",
        mod4.join("mod.toml"),
    )
    .unwrap();
    let err4 = m4.resolve_library_path().unwrap_err();
    assert!(
        matches!(
            err4,
            ManifestError::Io { ref source, .. }
                if source.kind() == std::io::ErrorKind::PermissionDenied
        ),
        "expected PermissionDenied for symlinked single-file fallback, got {err4:?}"
    );
}

#[test]
fn from_file_rejects_oversized_manifests() {
    let tmp = TempDir::new().unwrap();
    let oversized: PathBuf = tmp.path().join("mod.toml");
    let mut big = String::from("[mod]\nid = \"ok\"\nversion = \"1.0.0\"\n# ");
    big.extend(std::iter::repeat_n('a', (MAX_MANIFEST_BYTES as usize) + 64));
    fs::write(&oversized, big).unwrap();

    let err = ModTomlManifest::from_file(&oversized).unwrap_err();
    assert!(matches!(err, ManifestError::Io { .. }));
}
