//! Versioned save-game persistence (`ModSaveGamePlugin`) for persistent (`!ModScoped`) modded
//! Bevy ECS entities (`ABI 0.3.0`).
//!
//! Serializes persistent entities carrying host- or mod-registered components into a deterministic,
//! self-describing [`ModSaveArchive`] (JSON or checksummed binary) carrying explicit per-component
//! headers `(mod_id, mod_name, semver, component_uuid, schema_fingerprint, size, align, format,
//! fields)` and restores them under one of three explicit recovery policies ([`ModSaveLoadPolicy`]):
//!
//! - [`ModSaveLoadPolicy::StrictRequireMods`]: rejects load atomically with
//!   [`ModSaveLoadError::MissingMods`] or [`ModSaveLoadError::VersionMismatch`] if any required
//!   mod, version, or component schema is absent or incompatible.
//! - [`ModSaveLoadPolicy::StripMissingModComponents`]: restores all surviving host and active-mod
//!   components cleanly while stripping components belonging to disabled/missing mods and recording
//!   exact counts in [`ModSaveLoadReport`].
//! - [`ModSaveLoadPolicy::MigrateOnLoad`]: when `schema_fingerprint` (or layout) changed between
//!   save and active registration, executes a registered custom migrator ([`ModComponentMigrator`])
//!   or automatic field-name-matched `BamFieldDesc` / witness / `DynamicStruct` migration.

use crate::reflect::{ModReflectRegistry, ModReflectWorldExt};
use bevy_abi_mod_host::{LoadedModInfo, ModHost, ModId, ModRegistry, ModScoped};
use bevy_abi_mod_host_sys::loader::ExportedFieldRecord;
use bevy_abi_mod_host_sys::{
    BamFieldBytePatch, BamFieldDescriptor, BamFieldPodKind, RegisteredComponent,
    WitnessFieldDescriptor, export_entity_component_fields, insert_entity_component_from_fields,
    insert_entity_pod_with_field_patches, inspect_component_bam_fields,
    inspect_component_witness_fields, inspect_component_witness_fields_in_registry,
    read_entity_pod_bytes, write_entity_pod_bytes,
};
use bevy_abi_mod_sys::{
    BAM_CHILD_OF_NAME, BAM_CHILDREN_NAME, BamComponent, BamFieldKind, ComponentUuid,
    FNV1A_64_OFFSET_BASIS, FNV1A_64_PRIME, FfiResult,
};
use bevy_app::{App, Plugin};
use bevy_ecs::entity::Entity;
use bevy_ecs::entity_disabling::Disabled;
use bevy_ecs::name::Name;
use bevy_ecs::prelude::Resource;
use bevy_ecs::reflect::{AppTypeRegistry, ReflectComponent};
use bevy_ecs::resource::IsResource;
use bevy_ecs::world::World;
use bevy_platform::collections::{HashMap, HashSet};
use bevy_reflect::serde::{ReflectSerializer, TypedReflectDeserializer};
use bevy_reflect::structs::DynamicStruct;
use bevy_reflect::{PartialReflect, ReflectRef, TypeInfo};
use core::any::TypeId;
use core::fmt;
use serde::de::DeserializeSeed;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Number, Value};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

/// Current [`ModSaveArchive`] format version.
pub const MOD_SAVE_ARCHIVE_VERSION: u32 = 1;

/// Magic header prefix for checksummed binary [`ModSaveArchive`] blobs (`"BAMSAVE1"`).
pub const MOD_SAVE_BINARY_MAGIC: [u8; 8] = *b"BAMSAVE1";

const BINARY_HEADER_LEN: usize = 24;

/// Formats a 128-bit [`ComponentUuid`] into canonical lowercase 36-character `"8-4-4-4-12"` hex.
pub(crate) fn format_component_uuid(uuid: ComponentUuid) -> String {
    let b = uuid.as_bytes();
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0],
        b[1],
        b[2],
        b[3],
        b[4],
        b[5],
        b[6],
        b[7],
        b[8],
        b[9],
        b[10],
        b[11],
        b[12],
        b[13],
        b[14],
        b[15]
    )
}

fn fnv1a_64_update(mut hash: u64, bytes: &[u8]) -> u64 {
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV1A_64_PRIME);
    }
    hash
}

fn fnv1a_64_bytes(bytes: &[u8]) -> u64 {
    fnv1a_64_update(FNV1A_64_OFFSET_BASIS, bytes)
}

fn encode_hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

fn decode_hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn decode_hex(input: &str) -> Result<Vec<u8>, ModSaveLoadError> {
    let raw = input.as_bytes();
    if !raw.len().is_multiple_of(2) {
        return Err(ModSaveLoadError::CorruptArchive(format!(
            "hex payload has odd length {}",
            raw.len()
        )));
    }
    let mut out = Vec::with_capacity(raw.len() / 2);
    for pair in raw.as_chunks::<2>().0 {
        let hi = decode_hex_nibble(pair[0]).ok_or_else(|| {
            ModSaveLoadError::CorruptArchive(format!("invalid hex byte 0x{:02x}", pair[0]))
        })?;
        let lo = decode_hex_nibble(pair[1]).ok_or_else(|| {
            ModSaveLoadError::CorruptArchive(format!("invalid hex byte 0x{:02x}", pair[1]))
        })?;
        out.push((hi << 4) | lo);
    }
    Ok(out)
}

fn encode_base64_standard(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = if chunk.len() > 1 {
            u32::from(chunk[1])
        } else {
            0
        };
        let b2 = if chunk.len() > 2 {
            u32::from(chunk[2])
        } else {
            0
        };
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((triple >> 18) & 0x3f) as usize] as char);
        out.push(TABLE[((triple >> 12) & 0x3f) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[((triple >> 6) & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(triple & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

fn decode_base64_val(b: u8) -> Option<u8> {
    match b {
        b'A'..=b'Z' => Some(b - b'A'),
        b'a'..=b'z' => Some(b - b'a' + 26),
        b'0'..=b'9' => Some(b - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

fn decode_base64_standard(input: &str) -> Result<Vec<u8>, ModSaveLoadError> {
    let raw = input.as_bytes();
    if raw.is_empty() {
        return Ok(Vec::new());
    }
    if !raw.len().is_multiple_of(4) {
        return Err(ModSaveLoadError::CorruptArchive(format!(
            "base64 payload length {} is not a multiple of 4",
            raw.len()
        )));
    }
    let mut out = Vec::with_capacity((raw.len() / 4) * 3);
    let chunk_count = raw.len() / 4;
    for (idx, chunk) in raw.as_chunks::<4>().0.iter().enumerate() {
        let is_last = idx + 1 == chunk_count;
        let v0 = decode_base64_val(chunk[0]).ok_or_else(|| {
            ModSaveLoadError::CorruptArchive("invalid base64 character".to_owned())
        })?;
        let v1 = decode_base64_val(chunk[1]).ok_or_else(|| {
            ModSaveLoadError::CorruptArchive("invalid base64 character".to_owned())
        })?;
        if chunk[2] == b'=' {
            if !is_last || chunk[3] != b'=' {
                return Err(ModSaveLoadError::CorruptArchive(
                    "invalid base64 padding".to_owned(),
                ));
            }
            out.push((v0 << 2) | (v1 >> 4));
            continue;
        }
        let v2 = decode_base64_val(chunk[2]).ok_or_else(|| {
            ModSaveLoadError::CorruptArchive("invalid base64 character".to_owned())
        })?;
        if chunk[3] == b'=' {
            if !is_last {
                return Err(ModSaveLoadError::CorruptArchive(
                    "invalid base64 padding".to_owned(),
                ));
            }
            out.push((v0 << 2) | (v1 >> 4));
            out.push((v1 << 4) | (v2 >> 2));
            continue;
        }
        let v3 = decode_base64_val(chunk[3]).ok_or_else(|| {
            ModSaveLoadError::CorruptArchive("invalid base64 character".to_owned())
        })?;
        out.push((v0 << 2) | (v1 >> 4));
        out.push((v1 << 4) | (v2 >> 2));
        out.push((v2 << 6) | v3);
    }
    Ok(out)
}

/// Serialization format used for a component type inside a [`ModSaveArchive`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SavedComponentFormat {
    /// Raw `AbiPod` bytes encoded as lowercase hexadecimal.
    RawPodHex,
    /// Raw `AbiPod` bytes encoded as standard RFC 4648 Base64.
    RawPodBase64,
    /// JSON object keyed by declarative [`bevy_abi_mod_sys::BamFieldDesc`] field names.
    BamFieldsJson,
    /// JSON object keyed by `"bam.v0.<name>.field_info"` witness field names.
    WitnessFieldsJson,
    /// JSON value produced by Bevy's native `bevy_reflect` (`ReflectSerializer`).
    ReflectedJson,
}

/// Field layout descriptor persisted inside [`SavedComponentHeader`] so schema evolution can
/// decode saved raw POD bytes or field maps by field name even after the v1 mod is unloaded.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedFieldHeader {
    /// Field identifier.
    pub name: String,
    /// Normalized Rust/ABI type tag (e.g. `"f32"`, `"u32"`, `"String"`, `"Vec3"`).
    pub type_tag: String,
    /// Raw [`BamFieldKind`] discriminant (`0` if sourced from a witness without a `BamFieldKind`).
    pub kind: u32,
    /// Byte offset inside the saved component layout.
    pub offset: usize,
    /// Byte size of the field (`0` for dynamically sized witness fields like `String`/`Vec`).
    pub size: usize,
    /// Byte alignment of the field (`1` when unspecified).
    pub align: usize,
}

/// Active mod summary recorded in [`ModSaveArchive::mods`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedModMetadata {
    /// Runtime `ModId` recorded when the save was created.
    pub mod_id: u64,
    /// Canonical mod name (`BamManifest::name` / `LoadedModInfo::name`).
    pub mod_name: String,
    /// SemVer string declared by the mod at save time.
    pub semver: String,
}

/// Explicit per-component schema and ownership header stored in [`ModSaveArchive`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedComponentHeader {
    /// Owning `ModId` (`None` for host-owned or unowned components).
    pub mod_id: Option<u64>,
    /// Owning mod canonical name (`"host"` for host-registered components).
    pub mod_name: String,
    /// Owning mod SemVer string (or host version string for host-owned components).
    pub semver: String,
    /// Canonical component name in [`ModRegistry`] (e.g. `"mod_a::FactionState"`).
    pub component_name: String,
    /// Canonical hyphenated 128-bit [`ComponentUuid`] string.
    pub component_uuid: String,
    /// Effective 64-bit schema fingerprint (`RegisteredComponent::schema_fingerprint`, or a
    /// deterministic FNV-1a layout+field hash when the raw registration left it `0`).
    pub schema_fingerprint: u64,
    /// Component byte size (`size_of::<T>()`).
    pub size: usize,
    /// Component byte alignment (`align_of::<T>()`).
    pub align: usize,
    /// Whether this component was `AbiPod` at save time.
    pub is_abi_pod: bool,
    /// Whether this component was registered dynamically by a guest mod.
    pub is_dynamic: bool,
    /// Payload encoding format used by entity entries referencing this header.
    pub format: SavedComponentFormat,
    /// Saved field metadata (`BamFieldDesc` or witness descriptors) enabling automatic field-name
    /// schema migration even for `RawPodHex` / `RawPodBase64` payloads.
    #[serde(default)]
    pub fields: Vec<SavedFieldHeader>,
}

impl SavedComponentHeader {
    /// Parses [`Self::component_uuid`] back into a [`ComponentUuid`], if valid.
    #[inline]
    pub fn parsed_uuid(&self) -> Option<ComponentUuid> {
        ComponentUuid::try_parse_hex(&self.component_uuid)
    }

    /// Returns `true` when this component was owned by the host rather than a guest mod.
    #[inline]
    pub fn is_host_component(&self) -> bool {
        self.mod_id.is_none() && !self.is_dynamic && self.mod_name == "host"
    }
}

/// Encoded payload for a single component instance on a saved entity.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "encoding", content = "data")]
pub enum SavedComponentPayload {
    /// Lowercase hex-encoded raw `AbiPod` bytes.
    RawHex(String),
    /// Standard Base64-encoded raw `AbiPod` bytes.
    RawBase64(String),
    /// Field-name keyed JSON object (`BamFieldDesc` or witness fields).
    JsonFields(Map<String, Value>),
    /// Arbitrary JSON value produced by `ReflectSerializer`.
    JsonValue(Value),
}

impl SavedComponentPayload {
    /// Decodes raw bytes when this payload holds `RawHex` or `RawBase64`.
    pub fn decode_raw_bytes(&self) -> Result<Vec<u8>, ModSaveLoadError> {
        match self {
            Self::RawHex(s) => decode_hex(s),
            Self::RawBase64(s) => decode_base64_standard(s),
            Self::JsonFields(_) | Self::JsonValue(_) => Err(ModSaveLoadError::CorruptArchive(
                "expected raw POD byte payload, found JSON payload".to_owned(),
            )),
        }
    }

    /// Borrows the JSON field map if this payload is [`Self::JsonFields`] (or a JSON object inside
    /// [`Self::JsonValue`]).
    #[inline]
    pub fn as_json_fields(&self) -> Option<&Map<String, Value>> {
        match self {
            Self::JsonFields(map) | Self::JsonValue(Value::Object(map)) => Some(map),
            Self::RawHex(_) | Self::RawBase64(_) | Self::JsonValue(_) => None,
        }
    }
}

/// Single saved component record on a [`SavedEntityRecord`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SavedComponentEntry {
    /// Index into [`ModSaveArchive::component_headers`].
    pub header_index: usize,
    /// Encoded component payload.
    pub payload: SavedComponentPayload,
}

/// Single persistent (`!ModScoped`) entity persisted in a [`ModSaveArchive`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SavedEntityRecord {
    /// Original Bevy [`Entity::to_bits`] recorded at save time.
    pub source_entity_bits: u64,
    /// Optional Bevy [`Name`] component value preserved alongside modded components.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// All persistent components serialized for this entity.
    pub components: Vec<SavedComponentEntry>,
}

/// Versioned, self-describing save archive of persistent (`!ModScoped`) modded ECS entities.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModSaveArchive {
    /// Archive format version (`MOD_SAVE_ARCHIVE_VERSION`).
    pub format_version: u32,
    /// Active mods referenced by serialized component headers.
    pub mods: Vec<SavedModMetadata>,
    /// Per-component schema, ownership, and format headers.
    pub component_headers: Vec<SavedComponentHeader>,
    /// Persistent entities and their serialized components.
    pub entities: Vec<SavedEntityRecord>,
}

impl ModSaveArchive {
    /// Serializes this archive into a compact UTF-8 JSON string.
    pub fn to_json_string(&self) -> Result<String, ModSaveLoadError> {
        serde_json::to_string(self).map_err(|e| ModSaveLoadError::Serialization(e.to_string()))
    }

    /// Serializes this archive into a pretty-printed UTF-8 JSON string.
    pub fn to_json_string_pretty(&self) -> Result<String, ModSaveLoadError> {
        serde_json::to_string_pretty(self)
            .map_err(|e| ModSaveLoadError::Serialization(e.to_string()))
    }

    /// Deserializes a [`ModSaveArchive`] from a JSON string.
    pub fn from_json_str(json: &str) -> Result<Self, ModSaveLoadError> {
        let archive: Self = serde_json::from_str(json)
            .map_err(|e| ModSaveLoadError::Serialization(e.to_string()))?;
        archive.validate_version()?;
        Ok(archive)
    }

    /// Serializes this archive into compact JSON UTF-8 bytes.
    pub fn to_json_bytes(&self) -> Result<Vec<u8>, ModSaveLoadError> {
        serde_json::to_vec(self).map_err(|e| ModSaveLoadError::Serialization(e.to_string()))
    }

    /// Deserializes a [`ModSaveArchive`] from JSON UTF-8 bytes.
    pub fn from_json_bytes(bytes: &[u8]) -> Result<Self, ModSaveLoadError> {
        let archive: Self = serde_json::from_slice(bytes)
            .map_err(|e| ModSaveLoadError::Serialization(e.to_string()))?;
        archive.validate_version()?;
        Ok(archive)
    }

    /// Encodes this archive into a checksummed framed binary blob (`BAMSAVE1` magic + version +
    /// 64-bit FNV-1a integrity checksum + payload length + payload).
    pub fn to_binary_bytes(&self) -> Result<Vec<u8>, ModSaveLoadError> {
        let payload = self.to_json_bytes()?;
        let payload_len = u32::try_from(payload.len()).map_err(|_| {
            ModSaveLoadError::Serialization("save archive exceeds 4 GiB binary frame limit".into())
        })?;
        let checksum = fnv1a_64_bytes(&payload);
        let mut out = Vec::with_capacity(BINARY_HEADER_LEN + payload.len());
        out.extend_from_slice(&MOD_SAVE_BINARY_MAGIC);
        out.extend_from_slice(&self.format_version.to_le_bytes());
        out.extend_from_slice(&checksum.to_le_bytes());
        out.extend_from_slice(&payload_len.to_le_bytes());
        out.extend_from_slice(&payload);
        Ok(out)
    }

    /// Decodes and integrity-checks a binary [`ModSaveArchive`] created by [`Self::to_binary_bytes`].
    pub fn from_binary_bytes(bytes: &[u8]) -> Result<Self, ModSaveLoadError> {
        if bytes.len() < BINARY_HEADER_LEN {
            return Err(ModSaveLoadError::CorruptArchive(format!(
                "binary save archive truncated: {} bytes < {BINARY_HEADER_LEN}",
                bytes.len()
            )));
        }
        if bytes[0..8] != MOD_SAVE_BINARY_MAGIC {
            return Err(ModSaveLoadError::CorruptArchive(
                "binary save archive magic mismatch (expected BAMSAVE1)".to_owned(),
            ));
        }
        let version = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
        if version > MOD_SAVE_ARCHIVE_VERSION || version == 0 {
            return Err(ModSaveLoadError::UnsupportedArchiveVersion {
                found: version,
                max_supported: MOD_SAVE_ARCHIVE_VERSION,
            });
        }
        let expected_checksum = u64::from_le_bytes([
            bytes[12], bytes[13], bytes[14], bytes[15], bytes[16], bytes[17], bytes[18], bytes[19],
        ]);
        let payload_len = u32::from_le_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]) as usize;
        let payload = &bytes[BINARY_HEADER_LEN..];
        if payload.len() != payload_len {
            return Err(ModSaveLoadError::CorruptArchive(format!(
                "binary save archive length mismatch: header={payload_len}, actual={}",
                payload.len()
            )));
        }
        let actual_checksum = fnv1a_64_bytes(payload);
        if actual_checksum != expected_checksum {
            return Err(ModSaveLoadError::CorruptArchive(format!(
                "binary save archive checksum mismatch: expected 0x{expected_checksum:016x}, got 0x{actual_checksum:016x}"
            )));
        }
        Self::from_json_bytes(payload)
    }

    /// Writes this archive as pretty JSON to `path`.
    pub fn write_json_file(&self, path: impl AsRef<Path>) -> Result<(), ModSaveLoadError> {
        let text = self.to_json_string_pretty()?;
        std::fs::write(path, text)?;
        Ok(())
    }

    /// Reads a JSON [`ModSaveArchive`] from `path`.
    pub fn read_json_file(path: impl AsRef<Path>) -> Result<Self, ModSaveLoadError> {
        let text = std::fs::read_to_string(path)?;
        Self::from_json_str(&text)
    }

    /// Writes this archive in checksummed binary format to `path`.
    pub fn write_binary_file(&self, path: impl AsRef<Path>) -> Result<(), ModSaveLoadError> {
        let bytes = self.to_binary_bytes()?;
        std::fs::write(path, bytes)?;
        Ok(())
    }

    /// Reads a checksummed binary [`ModSaveArchive`] from `path`.
    pub fn read_binary_file(path: impl AsRef<Path>) -> Result<Self, ModSaveLoadError> {
        let bytes = std::fs::read(path)?;
        Self::from_binary_bytes(&bytes)
    }

    fn validate_version(&self) -> Result<(), ModSaveLoadError> {
        if self.format_version == 0 || self.format_version > MOD_SAVE_ARCHIVE_VERSION {
            return Err(ModSaveLoadError::UnsupportedArchiveVersion {
                found: self.format_version,
                max_supported: MOD_SAVE_ARCHIVE_VERSION,
            });
        }
        Ok(())
    }
}

/// Preferred payload encoding when saving persistent components into a [`ModSaveArchive`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ModSaveFormatPreference {
    /// Save `AbiPod` components as raw lowercase hex bytes (compact and byte-for-byte exact) and
    /// non-POD components as witness/declarative/reflected JSON.
    #[default]
    AutoPodThenJson,
    /// Save `AbiPod` components as standard Base64 raw bytes and non-POD components as JSON.
    AutoPodBase64ThenJson,
    /// Prefer human-readable JSON field objects (`BamFieldsJson` / `WitnessFieldsJson` /
    /// `ReflectedJson`) whenever field metadata is available, falling back to `RawPodHex`.
    PreferJsonFields,
}

/// Options controlling [`ModSaveWorldExt::save_mod_archive_with`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModSaveOptions {
    /// Preferred component payload encoding format.
    pub format_preference: ModSaveFormatPreference,
    /// Include persistent entities tagged with Bevy's [`Disabled`] component (default: `true`).
    pub include_disabled_entities: bool,
    /// SemVer string stamped onto host-owned component headers (default: `"0.0.0"`).
    pub host_semver: String,
    /// Optional whitelist of canonical component names to save (`None` saves all persistent
    /// components registered in [`ModRegistry`]).
    pub include_component_filter: Option<Vec<String>>,
}

impl Default for ModSaveOptions {
    fn default() -> Self {
        Self {
            format_preference: ModSaveFormatPreference::AutoPodThenJson,
            include_disabled_entities: true,
            host_semver: "0.0.0".to_owned(),
            include_component_filter: None,
        }
    }
}

/// Recovery policy applied when loading a [`ModSaveArchive`] into a Bevy [`World`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ModSaveLoadPolicy {
    /// Fail load atomically if any required mod is missing, its SemVer differs from the archive,
    /// or any saved component schema/fingerprint is missing or mismatched.
    #[default]
    StrictRequireMods,
    /// Restore all surviving host and active-mod components while cleanly stripping components
    /// belonging to disabled or missing mods, recording full diagnostics in [`ModSaveLoadReport`].
    StripMissingModComponents,
    /// Upgrade components whose `schema_fingerprint` (or layout) changed between save and active
    /// registration using registered [`ModComponentMigrator`] callbacks or automatic
    /// field-name-matched `BamFieldDesc` / witness / `DynamicStruct` migration. Missing/disabled
    /// mods are stripped cleanly and counted in [`ModSaveLoadReport::stripped_components`].
    MigrateOnLoad,
}

/// Structured errors returned by save/load operations.
#[derive(Debug)]
pub enum ModSaveLoadError {
    /// One or more required mods referenced by the archive are not loaded (`StrictRequireMods`).
    MissingMods {
        /// Deduplicated, sorted names of missing mods.
        missing: Vec<String>,
    },
    /// An active mod's SemVer does not match the saved SemVer (`StrictRequireMods`).
    VersionMismatch {
        /// Canonical mod name.
        mod_name: String,
        /// SemVer recorded in the save archive.
        saved_semver: String,
        /// SemVer currently active in the target [`World`].
        active_semver: String,
    },
    /// A saved component type is not registered in the target [`ModRegistry`].
    MissingComponentSchema {
        /// Canonical component name.
        component_name: String,
        /// Saved 128-bit UUID string.
        component_uuid: String,
    },
    /// Active component registration has a different `schema_fingerprint` or incompatible layout
    /// under a policy that does not perform schema migration.
    SchemaFingerprintMismatch {
        /// Canonical component name.
        component_name: String,
        /// Fingerprint stored in the archive.
        saved_fingerprint: u64,
        /// Fingerprint registered in the active [`World`].
        active_fingerprint: u64,
    },
    /// Save archive carries an unknown or unsupported format version.
    UnsupportedArchiveVersion {
        /// Format version discovered in the archive.
        found: u32,
        /// Highest format version supported by this build.
        max_supported: u32,
    },
    /// Archive framing, checksum, hex/base64, or header indices are invalid.
    CorruptArchive(String),
    /// Migration under [`ModSaveLoadPolicy::MigrateOnLoad`] failed for a present component.
    MigrationFailed {
        /// Canonical component name.
        component_name: String,
        /// Human-readable reason explaining the migration failure.
        reason: String,
    },
    /// Underlying host FFI component read/write error.
    Ffi {
        /// Canonical component name (or `"<world>"`).
        component_name: String,
        /// FFI status code returned by `bevy_abi_mod_host_sys`.
        status: FfiResult,
    },
    /// JSON serialization or deserialization error.
    Serialization(String),
    /// Filesystem I/O error.
    Io(std::io::Error),
}

impl fmt::Display for ModSaveLoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingMods { missing } => {
                write!(f, "missing required mods: {}", missing.join(", "))
            }
            Self::VersionMismatch {
                mod_name,
                saved_semver,
                active_semver,
            } => write!(
                f,
                "mod '{mod_name}' version mismatch: archive has {saved_semver}, active world has {active_semver}"
            ),
            Self::MissingComponentSchema {
                component_name,
                component_uuid,
            } => write!(
                f,
                "missing component schema '{component_name}' (uuid={component_uuid})"
            ),
            Self::SchemaFingerprintMismatch {
                component_name,
                saved_fingerprint,
                active_fingerprint,
            } => write!(
                f,
                "schema fingerprint mismatch for '{component_name}': archive=0x{saved_fingerprint:016x}, active=0x{active_fingerprint:016x}"
            ),
            Self::UnsupportedArchiveVersion {
                found,
                max_supported,
            } => write!(
                f,
                "unsupported save archive version {found} (max supported: {max_supported})"
            ),
            Self::CorruptArchive(msg) => write!(f, "corrupt save archive: {msg}"),
            Self::MigrationFailed {
                component_name,
                reason,
            } => write!(f, "migration failed for '{component_name}': {reason}"),
            Self::Ffi {
                component_name,
                status,
            } => write!(
                f,
                "FFI error while processing '{component_name}': {status:?}"
            ),
            Self::Serialization(msg) => write!(f, "archive serialization error: {msg}"),
            Self::Io(err) => write!(f, "archive I/O error: {err}"),
        }
    }
}

impl std::error::Error for ModSaveLoadError {}

impl From<std::io::Error> for ModSaveLoadError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}

/// Structured load summary returned by [`ModSaveWorldExt::load_mod_archive`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModSaveLoadReport {
    /// Number of entities spawned and restored into the target [`World`].
    pub restored_entities: usize,
    /// Total number of component instances restored (including migrated components).
    pub restored_components: usize,
    /// Total number of component instances stripped due to missing/disabled mods or custom
    /// [`MigratedComponentPayload::Strip`].
    pub stripped_components: usize,
    /// Total number of component instances upgraded via [`ModSaveLoadPolicy::MigrateOnLoad`].
    pub migrated_components: usize,
    /// Sorted, deduplicated canonical names of components stripped during load.
    pub stripped_component_names: Vec<String>,
    /// Sorted, deduplicated canonical names of components migrated during load.
    pub migrated_component_names: Vec<String>,
    /// Mapping from saved [`SavedEntityRecord::source_entity_bits`] to spawned live [`Entity`] IDs.
    pub entity_map: HashMap<u64, Entity>,
}

/// Output produced by a user-registered [`ModComponentMigrator`] callback.
#[derive(Clone, Debug, PartialEq)]
pub enum MigratedComponentPayload {
    /// Upgraded raw `AbiPod` bytes matching the active component's `(size, align)`.
    RawBytes(Vec<u8>),
    /// Upgraded field map matching the active component's `BamFieldDesc` or witness fields.
    JsonFields(Map<String, Value>),
    /// Upgraded reflected JSON value matching the active component's `AppTypeRegistry` type.
    JsonValue(Value),
    /// Strip this component instance instead of restoring it.
    Strip,
}

/// Metadata passed to a custom [`ModComponentMigrator`] during [`ModSaveLoadPolicy::MigrateOnLoad`].
#[derive(Clone, Debug)]
pub struct ComponentMigrationContext<'a> {
    /// Saved component header from the archive.
    pub saved_header: &'a SavedComponentHeader,
    /// Active canonical component name in [`ModRegistry`].
    pub active_name: &'a str,
    /// Active 128-bit [`ComponentUuid`].
    pub active_uuid: ComponentUuid,
    /// Active effective 64-bit schema fingerprint.
    pub active_schema_fingerprint: u64,
    /// Active component byte size.
    pub active_size: usize,
    /// Active component byte alignment.
    pub active_align: usize,
    /// Active validated declarative `BamFieldDesc` descriptors (empty if none).
    pub active_bam_fields: &'a [BamFieldDescriptor],
    /// Active witness field descriptors (empty if none).
    pub active_witness_fields: &'a [WitnessFieldDescriptor],
}

/// Thread-safe custom schema migration callback invoked during [`ModSaveLoadPolicy::MigrateOnLoad`].
pub type ModComponentMigrator = Arc<
    dyn Fn(
            &ComponentMigrationContext<'_>,
            &SavedComponentPayload,
        ) -> Result<MigratedComponentPayload, String>
        + Send
        + Sync,
>;

/// Host resource storing save-game defaults, custom component schema migrators, and optional
/// explicit `(ModId -> (mod_name, semver))` overrides.
#[derive(Resource, Default, Clone)]
pub struct ModSaveRegistry {
    default_policy: ModSaveLoadPolicy,
    default_options: ModSaveOptions,
    migrators_by_name: HashMap<String, ModComponentMigrator>,
    migrators_by_uuid: HashMap<ComponentUuid, ModComponentMigrator>,
    mod_metadata_overrides: HashMap<ModId, (String, String)>,
}

impl ModSaveRegistry {
    /// Creates an empty [`ModSaveRegistry`] with default policy and options.
    #[inline]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the default [`ModSaveLoadPolicy`].
    #[inline]
    pub const fn default_policy(&self) -> ModSaveLoadPolicy {
        self.default_policy
    }

    /// Sets the default [`ModSaveLoadPolicy`].
    #[inline]
    pub fn set_default_policy(&mut self, policy: ModSaveLoadPolicy) {
        self.default_policy = policy;
    }

    /// Borrows the default [`ModSaveOptions`].
    #[inline]
    pub fn default_options(&self) -> &ModSaveOptions {
        &self.default_options
    }

    /// Replaces the default [`ModSaveOptions`].
    #[inline]
    pub fn set_default_options(&mut self, options: ModSaveOptions) {
        self.default_options = options;
    }

    /// Registers a custom schema migrator keyed by canonical component name (e.g.
    /// `"mod_a::PlayerStats"`).
    pub fn register_migrator<F>(&mut self, component_name: impl Into<String>, migrator: F)
    where
        F: Fn(
                &ComponentMigrationContext<'_>,
                &SavedComponentPayload,
            ) -> Result<MigratedComponentPayload, String>
            + Send
            + Sync
            + 'static,
    {
        self.migrators_by_name
            .insert(component_name.into(), Arc::new(migrator));
    }

    /// Registers a custom schema migrator keyed by 128-bit [`ComponentUuid`].
    pub fn register_migrator_by_uuid<F>(&mut self, uuid: ComponentUuid, migrator: F)
    where
        F: Fn(
                &ComponentMigrationContext<'_>,
                &SavedComponentPayload,
            ) -> Result<MigratedComponentPayload, String>
            + Send
            + Sync
            + 'static,
    {
        self.migrators_by_uuid.insert(uuid, Arc::new(migrator));
    }

    /// Registers explicit `(mod_name, semver)` metadata for `mod_id` (used alongside live
    /// [`ModHost`] and [`LoadedModInfo`] discovery).
    pub fn register_mod_metadata(
        &mut self,
        mod_id: ModId,
        mod_name: impl Into<String>,
        semver: impl Into<String>,
    ) {
        self.mod_metadata_overrides
            .insert(mod_id, (mod_name.into(), semver.into()));
    }

    /// Removes explicit metadata registered for `mod_id`.
    pub fn unregister_mod_metadata(&mut self, mod_id: ModId) {
        self.mod_metadata_overrides.remove(&mod_id);
    }

    fn find_migrator(
        &self,
        saved_header: &SavedComponentHeader,
        active_entry: &RegisteredComponent,
    ) -> Option<ModComponentMigrator> {
        if let Some(m) = self.migrators_by_name.get(active_entry.name.as_ref()) {
            return Some(Arc::clone(m));
        }
        if let Some(m) = self.migrators_by_name.get(&saved_header.component_name) {
            return Some(Arc::clone(m));
        }
        if let Some(m) = self.migrators_by_uuid.get(&active_entry.uuid) {
            return Some(Arc::clone(m));
        }
        if let Some(saved_uuid) = saved_header.parsed_uuid()
            && let Some(m) = self.migrators_by_uuid.get(&saved_uuid)
        {
            return Some(Arc::clone(m));
        }
        None
    }
}

/// Bevy [`Plugin`] enabling persistent (`!ModScoped`) modded save-game serialization, recovery
/// policies, and schema migration via [`ModSaveRegistry`].
#[derive(Default, Debug, Clone)]
pub struct ModSaveGamePlugin {
    /// Default load recovery policy stored in [`ModSaveRegistry`].
    pub default_policy: ModSaveLoadPolicy,
    /// Default serialization options stored in [`ModSaveRegistry`].
    pub default_options: ModSaveOptions,
}

impl ModSaveGamePlugin {
    /// Creates a [`ModSaveGamePlugin`] with default policy ([`ModSaveLoadPolicy::StrictRequireMods`]).
    #[inline]
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a [`ModSaveGamePlugin`] configured with `policy`.
    #[inline]
    pub fn with_policy(policy: ModSaveLoadPolicy) -> Self {
        Self {
            default_policy: policy,
            default_options: ModSaveOptions::default(),
        }
    }

    /// Sets default [`ModSaveOptions`] on this plugin builder.
    #[inline]
    #[must_use]
    pub fn with_options(mut self, options: ModSaveOptions) -> Self {
        self.default_options = options;
        self
    }
}

impl Plugin for ModSaveGamePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<AppTypeRegistry>();
        app.init_resource::<ModReflectRegistry>();
        if let Some(mut save_reg) = app.world_mut().get_resource_mut::<ModSaveRegistry>() {
            save_reg.set_default_policy(self.default_policy);
            save_reg.set_default_options(self.default_options.clone());
        } else {
            let mut save_reg = ModSaveRegistry::new();
            save_reg.set_default_policy(self.default_policy);
            save_reg.set_default_options(self.default_options.clone());
            app.insert_resource(save_reg);
        }
    }
}

/// Safe [`World`] extension trait for saving and restoring persistent (`!ModScoped`) modded
/// entities and registering schema migrators.
pub trait ModSaveWorldExt {
    /// Captures all persistent (`!ModScoped`) modded entities using [`ModSaveRegistry`]'s default
    /// [`ModSaveOptions`].
    fn save_mod_archive(&self) -> Result<ModSaveArchive, ModSaveLoadError>;

    /// Captures all persistent (`!ModScoped`) modded entities with explicit [`ModSaveOptions`].
    fn save_mod_archive_with(
        &self,
        options: &ModSaveOptions,
    ) -> Result<ModSaveArchive, ModSaveLoadError>;

    /// Loads `archive` into `self` under `policy`, returning a structured [`ModSaveLoadReport`].
    fn load_mod_archive(
        &mut self,
        archive: &ModSaveArchive,
        policy: ModSaveLoadPolicy,
    ) -> Result<ModSaveLoadReport, ModSaveLoadError>;

    /// Registers a custom component schema migrator in [`ModSaveRegistry`].
    fn register_save_migrator<F>(&mut self, component_name: impl Into<String>, migrator: F)
    where
        F: Fn(
                &ComponentMigrationContext<'_>,
                &SavedComponentPayload,
            ) -> Result<MigratedComponentPayload, String>
            + Send
            + Sync
            + 'static;
}

impl ModSaveWorldExt for World {
    fn save_mod_archive(&self) -> Result<ModSaveArchive, ModSaveLoadError> {
        let options = self
            .get_resource::<ModSaveRegistry>()
            .map(|r| r.default_options().clone())
            .unwrap_or_default();
        self.save_mod_archive_with(&options)
    }

    fn save_mod_archive_with(
        &self,
        options: &ModSaveOptions,
    ) -> Result<ModSaveArchive, ModSaveLoadError> {
        save_world_to_archive(self, options)
    }

    fn load_mod_archive(
        &mut self,
        archive: &ModSaveArchive,
        policy: ModSaveLoadPolicy,
    ) -> Result<ModSaveLoadReport, ModSaveLoadError> {
        load_archive_into_world(self, archive, policy)
    }

    fn register_save_migrator<F>(&mut self, component_name: impl Into<String>, migrator: F)
    where
        F: Fn(
                &ComponentMigrationContext<'_>,
                &SavedComponentPayload,
            ) -> Result<MigratedComponentPayload, String>
            + Send
            + Sync
            + 'static,
    {
        if !self.contains_resource::<ModSaveRegistry>() {
            self.insert_resource(ModSaveRegistry::new());
        }
        self.resource_mut::<ModSaveRegistry>()
            .register_migrator(component_name, migrator);
    }
}

/// Safe [`App`] extension trait forwarding to [`ModSaveWorldExt`].
pub trait ModSaveAppExt {
    /// Captures all persistent (`!ModScoped`) modded entities from `self.world()`.
    fn save_mod_archive(&self) -> Result<ModSaveArchive, ModSaveLoadError>;

    /// Captures all persistent (`!ModScoped`) modded entities with explicit [`ModSaveOptions`].
    fn save_mod_archive_with(
        &self,
        options: &ModSaveOptions,
    ) -> Result<ModSaveArchive, ModSaveLoadError>;

    /// Loads `archive` into `self.world_mut()` under `policy`.
    fn load_mod_archive(
        &mut self,
        archive: &ModSaveArchive,
        policy: ModSaveLoadPolicy,
    ) -> Result<ModSaveLoadReport, ModSaveLoadError>;

    /// Registers a custom component schema migrator in [`ModSaveRegistry`].
    fn register_save_migrator<F>(
        &mut self,
        component_name: impl Into<String>,
        migrator: F,
    ) -> &mut Self
    where
        F: Fn(
                &ComponentMigrationContext<'_>,
                &SavedComponentPayload,
            ) -> Result<MigratedComponentPayload, String>
            + Send
            + Sync
            + 'static;
}

impl ModSaveAppExt for App {
    fn save_mod_archive(&self) -> Result<ModSaveArchive, ModSaveLoadError> {
        self.world().save_mod_archive()
    }

    fn save_mod_archive_with(
        &self,
        options: &ModSaveOptions,
    ) -> Result<ModSaveArchive, ModSaveLoadError> {
        self.world().save_mod_archive_with(options)
    }

    fn load_mod_archive(
        &mut self,
        archive: &ModSaveArchive,
        policy: ModSaveLoadPolicy,
    ) -> Result<ModSaveLoadReport, ModSaveLoadError> {
        self.world_mut().load_mod_archive(archive, policy)
    }

    fn register_save_migrator<F>(
        &mut self,
        component_name: impl Into<String>,
        migrator: F,
    ) -> &mut Self
    where
        F: Fn(
                &ComponentMigrationContext<'_>,
                &SavedComponentPayload,
            ) -> Result<MigratedComponentPayload, String>
            + Send
            + Sync
            + 'static,
    {
        self.world_mut()
            .register_save_migrator(component_name, migrator);
        self
    }
}

// ============================================================================
// Active Mod Discovery & Schema Fingerprint Resolution
// ============================================================================

#[derive(Clone, Debug)]
struct ActiveModEntry {
    id: ModId,
    name: String,
    semver: String,
}

struct ActiveModIndex {
    by_id: HashMap<ModId, ActiveModEntry>,
    by_name: HashMap<String, ActiveModEntry>,
}

fn collect_active_mods(world: &World) -> ActiveModIndex {
    let mut by_id: HashMap<ModId, ActiveModEntry> = HashMap::default();
    let mut by_name: HashMap<String, ActiveModEntry> = HashMap::default();

    if let Some(save_reg) = world.get_resource::<ModSaveRegistry>() {
        for (&id, (name, semver)) in &save_reg.mod_metadata_overrides {
            let entry = ActiveModEntry {
                id,
                name: name.clone(),
                semver: semver.clone(),
            };
            by_name.insert(entry.name.clone(), entry.clone());
            by_id.insert(id, entry);
        }
    }

    if let Some(info_id) = world.components().component_id::<LoadedModInfo>() {
        for arch in world.archetypes().iter() {
            if !arch.contains(info_id) {
                continue;
            }
            for arch_ent in arch.entities() {
                if let Some(info) = world.get::<LoadedModInfo>(arch_ent.id()) {
                    let entry = ActiveModEntry {
                        id: info.id,
                        name: info.name.clone(),
                        semver: info.version.clone(),
                    };
                    by_name.insert(entry.name.clone(), entry.clone());
                    by_id.insert(info.id, entry);
                }
            }
        }
    }

    if let Some(host) = world.get_resource::<ModHost>() {
        for loaded in host.loaded_mods() {
            if !loaded.is_active() {
                continue;
            }
            let entry = ActiveModEntry {
                id: loaded.id(),
                name: loaded.name().to_owned(),
                semver: loaded.version().to_owned(),
            };
            by_name.insert(entry.name.clone(), entry.clone());
            by_id.insert(loaded.id(), entry);
        }
    }

    ActiveModIndex { by_id, by_name }
}

fn is_persistable_registry_entry(
    entry: &RegisteredComponent,
    registry: &ModRegistry,
    filter: Option<&HashSet<String>>,
) -> bool {
    if entry.is_resource()
        || entry.is_message()
        || entry.is_event()
        || entry.is_entity_event()
        || registry.snapshot().is_relation(entry.handle)
    {
        return false;
    }
    if entry.name.as_ref() == BAM_CHILD_OF_NAME || entry.name.as_ref() == BAM_CHILDREN_NAME {
        return false;
    }
    if let Some(allowed) = filter
        && !allowed.contains(entry.name.as_ref())
    {
        return false;
    }
    true
}

fn collect_saved_field_headers(
    world: &World,
    entry: &RegisteredComponent,
) -> (
    Vec<SavedFieldHeader>,
    Vec<BamFieldDescriptor>,
    Vec<WitnessFieldDescriptor>,
) {
    let bam_fields = inspect_component_bam_fields(world, entry.handle)
        .ok()
        .flatten()
        .unwrap_or_default();
    let witness_fields = inspect_component_witness_fields(world, entry.handle)
        .or_else(|| {
            let reg = world.get_resource::<ModRegistry>()?;
            inspect_component_witness_fields_in_registry(reg, entry.handle)
        })
        .unwrap_or_default();

    let headers = if !bam_fields.is_empty() {
        bam_fields
            .iter()
            .map(|f| SavedFieldHeader {
                name: f.name.clone(),
                type_tag: f.type_tag.clone(),
                kind: f.kind.0,
                offset: f.offset,
                size: f.size,
                align: f.align,
            })
            .collect()
    } else if !witness_fields.is_empty() {
        witness_fields
            .iter()
            .map(|f| SavedFieldHeader {
                name: f.name.clone(),
                type_tag: f.type_tag.clone(),
                kind: BamFieldKind::UNKNOWN.0,
                offset: f.offset,
                size: 0,
                align: 1,
            })
            .collect()
    } else {
        Vec::new()
    };
    (headers, bam_fields, witness_fields)
}

fn effective_schema_fingerprint(
    entry: &RegisteredComponent,
    field_headers: &[SavedFieldHeader],
) -> u64 {
    if entry.schema_fingerprint != 0 {
        return entry.schema_fingerprint;
    }
    let mut h = FNV1A_64_OFFSET_BASIS;
    h = fnv1a_64_update(h, entry.name.as_bytes());
    h = fnv1a_64_update(h, &[0xff]);
    h = fnv1a_64_update(h, &(entry.size as u64).to_le_bytes());
    h = fnv1a_64_update(h, &(entry.align as u64).to_le_bytes());
    h = fnv1a_64_update(h, &[u8::from(entry.is_abi_pod)]);
    for field in field_headers {
        h = fnv1a_64_update(h, field.name.as_bytes());
        h = fnv1a_64_update(h, &[0xfe]);
        h = fnv1a_64_update(h, field.type_tag.as_bytes());
        h = fnv1a_64_update(h, &[0xfd]);
        h = fnv1a_64_update(h, &field.kind.to_le_bytes());
        h = fnv1a_64_update(h, &(field.offset as u64).to_le_bytes());
        h = fnv1a_64_update(h, &(field.size as u64).to_le_bytes());
        h = fnv1a_64_update(h, &(field.align as u64).to_le_bytes());
    }
    h
}

fn infer_mod_namespace(component_name: &str) -> Option<&str> {
    component_name
        .split_once("::")
        .map(|(ns, _)| ns)
        .filter(|ns| !ns.is_empty())
}

// ============================================================================
// Save Pipeline
// ============================================================================

struct SaveComponentPlan {
    handle: BamComponent,
    component_id: bevy_ecs::component::ComponentId,
    header_index: usize,
    header: SavedComponentHeader,
    bam_fields: Vec<BamFieldDescriptor>,
}

fn choose_save_format(
    world: &World,
    entry: &RegisteredComponent,
    bam_fields: &[BamFieldDescriptor],
    witness_fields: &[WitnessFieldDescriptor],
    pref: ModSaveFormatPreference,
) -> Option<SavedComponentFormat> {
    let can_raw_pod = entry.is_abi_pod && entry.drop_fn.is_none();
    let has_bam_pod_fields = bam_fields.iter().any(|f| f.pod_kind.is_some());
    let has_witness = !witness_fields.is_empty();
    let has_typed_reflect = has_typed_reflect_support(world, entry);

    match pref {
        ModSaveFormatPreference::AutoPodThenJson => {
            if can_raw_pod {
                Some(SavedComponentFormat::RawPodHex)
            } else if has_witness {
                Some(SavedComponentFormat::WitnessFieldsJson)
            } else if has_bam_pod_fields {
                Some(SavedComponentFormat::BamFieldsJson)
            } else if has_typed_reflect {
                Some(SavedComponentFormat::ReflectedJson)
            } else {
                None
            }
        }
        ModSaveFormatPreference::AutoPodBase64ThenJson => {
            if can_raw_pod {
                Some(SavedComponentFormat::RawPodBase64)
            } else if has_witness {
                Some(SavedComponentFormat::WitnessFieldsJson)
            } else if has_bam_pod_fields {
                Some(SavedComponentFormat::BamFieldsJson)
            } else if has_typed_reflect {
                Some(SavedComponentFormat::ReflectedJson)
            } else {
                None
            }
        }
        ModSaveFormatPreference::PreferJsonFields => {
            if has_bam_pod_fields && can_raw_pod {
                Some(SavedComponentFormat::BamFieldsJson)
            } else if has_witness {
                Some(SavedComponentFormat::WitnessFieldsJson)
            } else if has_typed_reflect {
                Some(SavedComponentFormat::ReflectedJson)
            } else if can_raw_pod {
                Some(SavedComponentFormat::RawPodHex)
            } else {
                None
            }
        }
    }
}

fn has_typed_reflect_support(world: &World, entry: &RegisteredComponent) -> bool {
    let Some(app_types) = world.get_resource::<AppTypeRegistry>() else {
        return false;
    };
    let Some(info) = world.components().get_info(entry.component_id) else {
        return false;
    };
    let Some(tid) = info.type_id() else {
        return false;
    };
    let types = app_types.read();
    types
        .get(tid)
        .is_some_and(|reg| reg.data::<bevy_reflect::ReflectFromPtr>().is_some())
}

fn save_world_to_archive(
    world: &World,
    options: &ModSaveOptions,
) -> Result<ModSaveArchive, ModSaveLoadError> {
    let Some(registry) = world.get_resource::<ModRegistry>() else {
        return Ok(ModSaveArchive {
            format_version: MOD_SAVE_ARCHIVE_VERSION,
            mods: Vec::new(),
            component_headers: Vec::new(),
            entities: Vec::new(),
        });
    };

    let active_mods = collect_active_mods(world);
    let filter_set: Option<HashSet<String>> = options
        .include_component_filter
        .as_ref()
        .map(|list| list.iter().cloned().collect());

    let mut candidate_entries: Vec<&RegisteredComponent> = registry
        .snapshot()
        .entries()
        .iter()
        .filter(|entry| is_persistable_registry_entry(entry, registry, filter_set.as_ref()))
        .collect();
    candidate_entries.sort_by(|a, b| a.name.cmp(&b.name));

    let mut plans: Vec<SaveComponentPlan> = Vec::new();
    for entry in candidate_entries {
        let (field_headers, bam_fields, witness_fields) = collect_saved_field_headers(world, entry);
        let Some(format) = choose_save_format(
            world,
            entry,
            &bam_fields,
            &witness_fields,
            options.format_preference,
        ) else {
            continue;
        };

        let (mod_id, mod_name, semver) = if let Some(owner_id) = entry.owner {
            if let Some(active) = active_mods.by_id.get(&owner_id) {
                (Some(owner_id.0), active.name.clone(), active.semver.clone())
            } else {
                let fallback_name = infer_mod_namespace(entry.name.as_ref())
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("mod_{}", owner_id.0));
                (Some(owner_id.0), fallback_name, "0.0.0".to_owned())
            }
        } else if entry.is_dynamic {
            if let Some(ns) = infer_mod_namespace(entry.name.as_ref())
                && let Some(active) = active_mods.by_name.get(ns)
            {
                (
                    Some(active.id.0),
                    active.name.clone(),
                    active.semver.clone(),
                )
            } else {
                let name = infer_mod_namespace(entry.name.as_ref())
                    .unwrap_or("dynamic")
                    .to_owned();
                (None, name, "0.0.0".to_owned())
            }
        } else {
            (None, "host".to_owned(), options.host_semver.clone())
        };

        let schema_fingerprint = effective_schema_fingerprint(entry, &field_headers);
        let header_index = plans.len();
        let header = SavedComponentHeader {
            mod_id,
            mod_name,
            semver,
            component_name: entry.name.to_string(),
            component_uuid: format_component_uuid(entry.uuid),
            schema_fingerprint,
            size: entry.size,
            align: entry.align,
            is_abi_pod: entry.is_abi_pod,
            is_dynamic: entry.is_dynamic,
            format,
            fields: field_headers,
        };
        plans.push(SaveComponentPlan {
            handle: entry.handle,
            component_id: entry.component_id,
            header_index,
            header,
            bam_fields,
        });
    }

    let mod_scoped_id = world.components().component_id::<ModScoped>();
    let is_resource_id = world.components().component_id::<IsResource>();
    let disabled_id = world.components().component_id::<Disabled>();
    let loaded_mod_info_id = world.components().component_id::<LoadedModInfo>();
    let name_comp_id = world.components().component_id::<Name>();

    let mut used_header_flags = vec![false; plans.len()];
    let mut raw_entities: Vec<SavedEntityRecord> = Vec::new();

    for arch in world.archetypes().iter() {
        if mod_scoped_id.is_some_and(|id| arch.contains(id))
            || is_resource_id.is_some_and(|id| arch.contains(id))
            || loaded_mod_info_id.is_some_and(|id| arch.contains(id))
        {
            continue;
        }
        if !options.include_disabled_entities && disabled_id.is_some_and(|id| arch.contains(id)) {
            continue;
        }

        let matching_plan_indices: Vec<usize> = plans
            .iter()
            .enumerate()
            .filter_map(|(idx, plan)| arch.contains(plan.component_id).then_some(idx))
            .collect();
        if matching_plan_indices.is_empty() {
            continue;
        }
        let has_name = name_comp_id.is_some_and(|id| arch.contains(id));

        for arch_ent in arch.entities() {
            let entity = arch_ent.id();
            let mut entries = Vec::with_capacity(matching_plan_indices.len());
            for &plan_idx in &matching_plan_indices {
                let plan = &plans[plan_idx];
                let payload = serialize_entity_component(world, entity, plan)?;
                used_header_flags[plan_idx] = true;
                entries.push(SavedComponentEntry {
                    header_index: plan.header_index,
                    payload,
                });
            }
            if !entries.is_empty() {
                let entity_name = if has_name {
                    world.get::<Name>(entity).map(|n| n.as_str().to_owned())
                } else {
                    None
                };
                raw_entities.push(SavedEntityRecord {
                    source_entity_bits: entity.to_bits(),
                    name: entity_name,
                    components: entries,
                });
            }
        }
    }

    // Compact header list to only headers referenced by at least one saved entity.
    let mut old_to_new: Vec<Option<usize>> = vec![None; plans.len()];
    let mut component_headers = Vec::new();
    let mut referenced_mods: BTreeMap<String, SavedModMetadata> = BTreeMap::new();

    for (old_idx, (plan, used)) in plans.into_iter().zip(used_header_flags).enumerate() {
        if !used {
            continue;
        }
        if let Some(mid) = plan.header.mod_id {
            referenced_mods
                .entry(plan.header.mod_name.clone())
                .or_insert_with(|| SavedModMetadata {
                    mod_id: mid,
                    mod_name: plan.header.mod_name.clone(),
                    semver: plan.header.semver.clone(),
                });
        }
        let new_idx = component_headers.len();
        old_to_new[old_idx] = Some(new_idx);
        component_headers.push(plan.header);
    }

    for ent in &mut raw_entities {
        for comp in &mut ent.components {
            if let Some(new_idx) = old_to_new[comp.header_index] {
                comp.header_index = new_idx;
            }
        }
    }
    raw_entities.sort_by_key(|e| e.source_entity_bits);

    Ok(ModSaveArchive {
        format_version: MOD_SAVE_ARCHIVE_VERSION,
        mods: referenced_mods.into_values().collect(),
        component_headers,
        entities: raw_entities,
    })
}

fn serialize_entity_component(
    world: &World,
    entity: Entity,
    plan: &SaveComponentPlan,
) -> Result<SavedComponentPayload, ModSaveLoadError> {
    let comp_name = &plan.header.component_name;
    match plan.header.format {
        SavedComponentFormat::RawPodHex => {
            let bytes = read_entity_pod_bytes(world, entity, plan.handle).map_err(|status| {
                ModSaveLoadError::Ffi {
                    component_name: comp_name.clone(),
                    status,
                }
            })?;
            Ok(SavedComponentPayload::RawHex(encode_hex_lower(&bytes)))
        }
        SavedComponentFormat::RawPodBase64 => {
            let bytes = read_entity_pod_bytes(world, entity, plan.handle).map_err(|status| {
                ModSaveLoadError::Ffi {
                    component_name: comp_name.clone(),
                    status,
                }
            })?;
            Ok(SavedComponentPayload::RawBase64(encode_base64_standard(
                &bytes,
            )))
        }
        SavedComponentFormat::BamFieldsJson => {
            let bytes = read_entity_pod_bytes(world, entity, plan.handle).map_err(|status| {
                ModSaveLoadError::Ffi {
                    component_name: comp_name.clone(),
                    status,
                }
            })?;
            let map = decode_pod_bytes_with_bam_fields(&bytes, &plan.bam_fields)?;
            Ok(SavedComponentPayload::JsonFields(map))
        }
        SavedComponentFormat::WitnessFieldsJson => {
            let records =
                export_entity_component_fields(world, entity, plan.handle).map_err(|status| {
                    ModSaveLoadError::Ffi {
                        component_name: comp_name.clone(),
                        status,
                    }
                })?;
            let map = exported_records_to_json_map(&records);
            Ok(SavedComponentPayload::JsonFields(map))
        }
        SavedComponentFormat::ReflectedJson => {
            let reflect = world
                .reflect_mod_component(entity, plan.handle)
                .map_err(|status| ModSaveLoadError::Ffi {
                    component_name: comp_name.clone(),
                    status,
                })?;
            let dyn_val = reflect.to_dynamic().map_err(|e| {
                ModSaveLoadError::Serialization(format!(
                    "reflect clone failed for '{comp_name}': {e}"
                ))
            })?;
            let app_types = world.get_resource::<AppTypeRegistry>().ok_or_else(|| {
                ModSaveLoadError::Serialization("AppTypeRegistry missing".to_owned())
            })?;
            let types = app_types.read();
            let ser = ReflectSerializer::new(dyn_val.as_partial_reflect(), &types);
            let value = serde_json::to_value(&ser)
                .map_err(|e| ModSaveLoadError::Serialization(e.to_string()))?;
            Ok(SavedComponentPayload::JsonValue(value))
        }
    }
}

// ============================================================================
// Load & Migration Pipeline
// ============================================================================

enum HeaderLoadPlan {
    DirectRestore {
        active: RegisteredComponent,
        bam_fields: Vec<BamFieldDescriptor>,
        witness_fields: Vec<WitnessFieldDescriptor>,
    },
    Migrate {
        active: RegisteredComponent,
        active_fingerprint: u64,
        bam_fields: Vec<BamFieldDescriptor>,
        witness_fields: Vec<WitnessFieldDescriptor>,
        migrator: Option<ModComponentMigrator>,
    },
    Strip {
        component_name: String,
    },
}

fn resolve_active_component(
    registry: &ModRegistry,
    header: &SavedComponentHeader,
) -> Option<RegisteredComponent> {
    if let Some(entry) = registry.get_by_name(&header.component_name) {
        return Some(entry.clone());
    }
    if let Some(uuid) = header.parsed_uuid()
        && let Some(entry) = registry.get_by_uuid(&uuid)
    {
        return Some(entry.clone());
    }
    None
}

fn load_archive_into_world(
    world: &mut World,
    archive: &ModSaveArchive,
    policy: ModSaveLoadPolicy,
) -> Result<ModSaveLoadReport, ModSaveLoadError> {
    archive.validate_version()?;

    // Pre-validate entity header indices before mutating world.
    let header_count = archive.component_headers.len();
    for ent in &archive.entities {
        for comp in &ent.components {
            if comp.header_index >= header_count {
                return Err(ModSaveLoadError::CorruptArchive(format!(
                    "entity 0x{:x} references header_index {} >= header count {header_count}",
                    ent.source_entity_bits, comp.header_index
                )));
            }
        }
    }

    let registry = world
        .get_resource::<ModRegistry>()
        .cloned()
        .unwrap_or_default();
    let save_reg = world
        .get_resource::<ModSaveRegistry>()
        .cloned()
        .unwrap_or_default();
    let active_mods = collect_active_mods(world);

    // Phase 1: Inspect every component header and construct a validated load plan atomically.
    let mut missing_mods: Vec<String> = Vec::new();
    let mut header_plans: Vec<HeaderLoadPlan> = Vec::with_capacity(header_count);

    for header in &archive.component_headers {
        let active_entry_opt = resolve_active_component(&registry, header);

        // Determine owning mod liveness.
        let mod_active_info: Option<&ActiveModEntry> = if header.is_host_component() {
            None
        } else {
            active_mods.by_name.get(&header.mod_name)
        };

        let owning_mod_missing = if header.is_host_component() {
            false
        } else if header.mod_id.is_some() {
            // Component was explicitly owned by a loaded mod at save time.
            mod_active_info.is_none()
                || active_entry_opt
                    .as_ref()
                    .is_some_and(|e| e.is_dynamic && e.owner.is_none())
        } else {
            // Dynamic component saved without a ModId: missing if unregistered or evicted.
            active_entry_opt.is_none()
        };

        if owning_mod_missing {
            match policy {
                ModSaveLoadPolicy::StrictRequireMods => {
                    if !missing_mods.contains(&header.mod_name) {
                        missing_mods.push(header.mod_name.clone());
                    }
                    continue;
                }
                ModSaveLoadPolicy::StripMissingModComponents | ModSaveLoadPolicy::MigrateOnLoad => {
                    header_plans.push(HeaderLoadPlan::Strip {
                        component_name: header.component_name.clone(),
                    });
                    continue;
                }
            }
        }

        // Owning mod is active (or host) — check SemVer under StrictRequireMods.
        if policy == ModSaveLoadPolicy::StrictRequireMods
            && let Some(active_mod) = mod_active_info
            && active_mod.semver != header.semver
        {
            return Err(ModSaveLoadError::VersionMismatch {
                mod_name: header.mod_name.clone(),
                saved_semver: header.semver.clone(),
                active_semver: active_mod.semver.clone(),
            });
        }

        let Some(active_entry) = active_entry_opt else {
            match policy {
                ModSaveLoadPolicy::StrictRequireMods => {
                    if !header.is_host_component() {
                        if !missing_mods.contains(&header.mod_name) {
                            missing_mods.push(header.mod_name.clone());
                        }
                        continue;
                    }
                    return Err(ModSaveLoadError::MissingComponentSchema {
                        component_name: header.component_name.clone(),
                        component_uuid: header.component_uuid.clone(),
                    });
                }
                ModSaveLoadPolicy::StripMissingModComponents | ModSaveLoadPolicy::MigrateOnLoad => {
                    header_plans.push(HeaderLoadPlan::Strip {
                        component_name: header.component_name.clone(),
                    });
                    continue;
                }
            }
        };

        let (active_field_headers, active_bam_fields, active_witness_fields) =
            collect_saved_field_headers(world, &active_entry);
        let active_fingerprint = effective_schema_fingerprint(&active_entry, &active_field_headers);

        let layout_or_schema_changed = active_fingerprint != header.schema_fingerprint
            || active_entry.size != header.size
            || active_entry.align != header.align;

        if layout_or_schema_changed {
            match policy {
                ModSaveLoadPolicy::StrictRequireMods => {
                    return Err(ModSaveLoadError::SchemaFingerprintMismatch {
                        component_name: header.component_name.clone(),
                        saved_fingerprint: header.schema_fingerprint,
                        active_fingerprint,
                    });
                }
                ModSaveLoadPolicy::StripMissingModComponents => {
                    header_plans.push(HeaderLoadPlan::Strip {
                        component_name: header.component_name.clone(),
                    });
                }
                ModSaveLoadPolicy::MigrateOnLoad => {
                    let migrator = save_reg.find_migrator(header, &active_entry);
                    header_plans.push(HeaderLoadPlan::Migrate {
                        active: active_entry,
                        active_fingerprint,
                        bam_fields: active_bam_fields,
                        witness_fields: active_witness_fields,
                        migrator,
                    });
                }
            }
        } else {
            header_plans.push(HeaderLoadPlan::DirectRestore {
                active: active_entry,
                bam_fields: active_bam_fields,
                witness_fields: active_witness_fields,
            });
        }
    }

    if !missing_mods.is_empty() {
        missing_mods.sort();
        return Err(ModSaveLoadError::MissingMods {
            missing: missing_mods,
        });
    }

    // Phase 2: Spawn entities and restore/migrate/strip component payloads.
    let mut report = ModSaveLoadReport::default();
    let mut stripped_names: HashSet<String> = HashSet::default();
    let mut migrated_names: HashSet<String> = HashSet::default();

    for ent_record in &archive.entities {
        // Count how many components survive on this entity before spawning.
        let mut surviving_count = 0usize;
        let mut stripped_here = 0usize;
        for comp in &ent_record.components {
            match &header_plans[comp.header_index] {
                HeaderLoadPlan::Strip { component_name } => {
                    stripped_here += 1;
                    stripped_names.insert(component_name.clone());
                }
                HeaderLoadPlan::DirectRestore { .. } | HeaderLoadPlan::Migrate { .. } => {
                    surviving_count += 1;
                }
            }
        }

        if surviving_count == 0 && ent_record.name.is_none() {
            report.stripped_components += stripped_here;
            continue;
        }

        let entity = if let Some(ref name) = ent_record.name {
            world.spawn(Name::new(name.clone())).id()
        } else {
            world.spawn_empty().id()
        };

        let mut entity_restored_any = ent_record.name.is_some();
        for comp in &ent_record.components {
            let header = &archive.component_headers[comp.header_index];
            match &header_plans[comp.header_index] {
                HeaderLoadPlan::Strip { component_name } => {
                    report.stripped_components += 1;
                    stripped_names.insert(component_name.clone());
                }
                HeaderLoadPlan::DirectRestore {
                    active,
                    bam_fields,
                    witness_fields,
                } => {
                    restore_direct_component(
                        world,
                        entity,
                        header,
                        active,
                        bam_fields,
                        witness_fields,
                        &comp.payload,
                    )?;
                    report.restored_components += 1;
                    entity_restored_any = true;
                }
                HeaderLoadPlan::Migrate {
                    active,
                    active_fingerprint,
                    bam_fields,
                    witness_fields,
                    migrator,
                } => {
                    let outcome = migrate_component(
                        world,
                        entity,
                        header,
                        active,
                        *active_fingerprint,
                        bam_fields,
                        witness_fields,
                        migrator.as_ref(),
                        &comp.payload,
                    )?;
                    match outcome {
                        MigrationOutcome::Migrated => {
                            report.migrated_components += 1;
                            report.restored_components += 1;
                            migrated_names.insert(active.name.to_string());
                            entity_restored_any = true;
                        }
                        MigrationOutcome::Stripped => {
                            report.stripped_components += 1;
                            stripped_names.insert(header.component_name.clone());
                        }
                    }
                }
            }
        }

        if entity_restored_any {
            report.restored_entities += 1;
            report
                .entity_map
                .insert(ent_record.source_entity_bits, entity);
        } else if let Ok(ent_mut) = world.get_entity_mut(entity) {
            ent_mut.despawn();
        }
    }

    let mut stripped_vec: Vec<String> = stripped_names.into_iter().collect();
    stripped_vec.sort();
    report.stripped_component_names = stripped_vec;

    let mut migrated_vec: Vec<String> = migrated_names.into_iter().collect();
    migrated_vec.sort();
    report.migrated_component_names = migrated_vec;

    Ok(report)
}

fn restore_direct_component(
    world: &mut World,
    entity: Entity,
    header: &SavedComponentHeader,
    active: &RegisteredComponent,
    bam_fields: &[BamFieldDescriptor],
    witness_fields: &[WitnessFieldDescriptor],
    payload: &SavedComponentPayload,
) -> Result<(), ModSaveLoadError> {
    match payload {
        SavedComponentPayload::RawHex(_) | SavedComponentPayload::RawBase64(_) => {
            let bytes = payload.decode_raw_bytes()?;
            if bytes.len() != active.size {
                return Err(ModSaveLoadError::CorruptArchive(format!(
                    "component '{}' raw byte length {} != active size {}",
                    active.name,
                    bytes.len(),
                    active.size
                )));
            }
            match write_entity_pod_bytes(world, entity, active.handle, &bytes) {
                Ok(()) => Ok(()),
                Err(FfiResult::PERMISSION_DENIED) if !header.fields.is_empty() => {
                    // Fallback for opaque host components that expose witness/declarative fields.
                    let map = decode_raw_bytes_using_saved_fields(&bytes, &header.fields);
                    apply_json_fields_to_entity(
                        world,
                        entity,
                        active,
                        bam_fields,
                        witness_fields,
                        &map,
                    )
                }
                Err(status) => Err(ModSaveLoadError::Ffi {
                    component_name: active.name.to_string(),
                    status,
                }),
            }
        }
        SavedComponentPayload::JsonFields(map) => {
            apply_json_fields_to_entity(world, entity, active, bam_fields, witness_fields, map)
        }
        SavedComponentPayload::JsonValue(val) => {
            apply_reflected_json_to_entity(world, entity, active, val)
        }
    }
}

enum MigrationOutcome {
    Migrated,
    Stripped,
}

#[allow(clippy::too_many_arguments)]
fn migrate_component(
    world: &mut World,
    entity: Entity,
    saved_header: &SavedComponentHeader,
    active: &RegisteredComponent,
    active_fingerprint: u64,
    bam_fields: &[BamFieldDescriptor],
    witness_fields: &[WitnessFieldDescriptor],
    migrator: Option<&ModComponentMigrator>,
    payload: &SavedComponentPayload,
) -> Result<MigrationOutcome, ModSaveLoadError> {
    // 1. Registered custom migrator takes top priority.
    if let Some(custom) = migrator {
        let ctx = ComponentMigrationContext {
            saved_header,
            active_name: active.name.as_ref(),
            active_uuid: active.uuid,
            active_schema_fingerprint: active_fingerprint,
            active_size: active.size,
            active_align: active.align,
            active_bam_fields: bam_fields,
            active_witness_fields: witness_fields,
        };
        let upgraded =
            custom(&ctx, payload).map_err(|reason| ModSaveLoadError::MigrationFailed {
                component_name: active.name.to_string(),
                reason,
            })?;
        match upgraded {
            MigratedComponentPayload::RawBytes(bytes) => {
                if bytes.len() != active.size {
                    return Err(ModSaveLoadError::MigrationFailed {
                        component_name: active.name.to_string(),
                        reason: format!(
                            "custom migrator returned {} bytes, expected {}",
                            bytes.len(),
                            active.size
                        ),
                    });
                }
                write_entity_pod_bytes(world, entity, active.handle, &bytes).map_err(|status| {
                    ModSaveLoadError::Ffi {
                        component_name: active.name.to_string(),
                        status,
                    }
                })?;
                return Ok(MigrationOutcome::Migrated);
            }
            MigratedComponentPayload::JsonFields(map) => {
                apply_json_fields_to_entity(
                    world,
                    entity,
                    active,
                    bam_fields,
                    witness_fields,
                    &map,
                )?;
                return Ok(MigrationOutcome::Migrated);
            }
            MigratedComponentPayload::JsonValue(val) => {
                apply_reflected_json_to_entity(world, entity, active, &val)?;
                return Ok(MigrationOutcome::Migrated);
            }
            MigratedComponentPayload::Strip => return Ok(MigrationOutcome::Stripped),
        }
    }

    // 2. Automatic field-name-matched migration (BamFieldDesc, witness, or reflected DynamicStruct).
    let source_fields: Map<String, Value> = match payload {
        SavedComponentPayload::JsonFields(map)
        | SavedComponentPayload::JsonValue(Value::Object(map)) => map.clone(),
        SavedComponentPayload::RawHex(_) | SavedComponentPayload::RawBase64(_) => {
            if saved_header.fields.is_empty() {
                return Err(ModSaveLoadError::MigrationFailed {
                    component_name: active.name.to_string(),
                    reason: "saved raw POD component has no field metadata and no custom migrator is registered".to_owned(),
                });
            }
            let bytes = payload.decode_raw_bytes()?;
            decode_raw_bytes_using_saved_fields(&bytes, &saved_header.fields)
        }
        SavedComponentPayload::JsonValue(_) => {
            return Err(ModSaveLoadError::MigrationFailed {
                component_name: active.name.to_string(),
                reason:
                    "saved JSON payload is not a field object and no custom migrator is registered"
                        .to_owned(),
            });
        }
    };

    if !bam_fields.is_empty() && active.is_abi_pod && active.drop_fn.is_none() {
        let mut patches = Vec::new();
        for field in bam_fields {
            let Some(pod_kind) = field.pod_kind else {
                continue;
            };
            if let Some(val) = source_fields.get(&field.name) {
                let bytes = encode_json_to_pod_field_bytes(&field.name, pod_kind, val).map_err(
                    |reason| ModSaveLoadError::MigrationFailed {
                        component_name: active.name.to_string(),
                        reason,
                    },
                )?;
                patches.push(BamFieldBytePatch {
                    offset: field.offset,
                    bytes,
                });
            }
        }
        insert_entity_pod_with_field_patches(world, entity, active.handle, &patches).map_err(
            |status| ModSaveLoadError::Ffi {
                component_name: active.name.to_string(),
                status,
            },
        )?;
        return Ok(MigrationOutcome::Migrated);
    }

    if !witness_fields.is_empty() {
        let mut records = Vec::new();
        for wf in witness_fields {
            if let Some(val) = source_fields.get(&wf.name) {
                let rec =
                    json_to_witness_record(&wf.name, &wf.type_tag, val).map_err(|reason| {
                        ModSaveLoadError::MigrationFailed {
                            component_name: active.name.to_string(),
                            reason,
                        }
                    })?;
                records.push(rec);
            }
        }
        insert_entity_component_from_fields(world, entity, active.handle, &records).map_err(
            |status| ModSaveLoadError::Ffi {
                component_name: active.name.to_string(),
                status,
            },
        )?;
        return Ok(MigrationOutcome::Migrated);
    }

    if let Some(migrated) = try_migrate_typed_reflect_struct(world, entity, active, &source_fields)?
        && migrated
    {
        return Ok(MigrationOutcome::Migrated);
    }

    Err(ModSaveLoadError::MigrationFailed {
        component_name: active.name.to_string(),
        reason: "active component has neither BamFieldDesc, witness, nor reflected struct schema for automatic migration".to_owned(),
    })
}

fn apply_json_fields_to_entity(
    world: &mut World,
    entity: Entity,
    active: &RegisteredComponent,
    bam_fields: &[BamFieldDescriptor],
    witness_fields: &[WitnessFieldDescriptor],
    map: &Map<String, Value>,
) -> Result<(), ModSaveLoadError> {
    if !witness_fields.is_empty() {
        let mut records = Vec::new();
        for (k, v) in map {
            if let Some(wf) = witness_fields.iter().find(|f| f.name == *k) {
                let rec = json_to_witness_record(k, &wf.type_tag, v).map_err(|reason| {
                    ModSaveLoadError::MigrationFailed {
                        component_name: active.name.to_string(),
                        reason,
                    }
                })?;
                records.push(rec);
            }
        }
        return insert_entity_component_from_fields(world, entity, active.handle, &records)
            .map_err(|status| ModSaveLoadError::Ffi {
                component_name: active.name.to_string(),
                status,
            });
    }

    if !bam_fields.is_empty() && active.is_abi_pod && active.drop_fn.is_none() {
        let mut patches = Vec::new();
        for (k, v) in map {
            if let Some(bf) = bam_fields.iter().find(|f| f.name == *k)
                && let Some(pod_kind) = bf.pod_kind
            {
                let bytes = encode_json_to_pod_field_bytes(k, pod_kind, v).map_err(|reason| {
                    ModSaveLoadError::MigrationFailed {
                        component_name: active.name.to_string(),
                        reason,
                    }
                })?;
                patches.push(BamFieldBytePatch {
                    offset: bf.offset,
                    bytes,
                });
            }
        }
        return insert_entity_pod_with_field_patches(world, entity, active.handle, &patches)
            .map_err(|status| ModSaveLoadError::Ffi {
                component_name: active.name.to_string(),
                status,
            });
    }

    apply_reflected_json_to_entity(world, entity, active, &Value::Object(map.clone()))
}

fn apply_reflected_json_to_entity(
    world: &mut World,
    entity: Entity,
    active: &RegisteredComponent,
    value: &Value,
) -> Result<(), ModSaveLoadError> {
    let type_id: TypeId = world
        .components()
        .get_info(active.component_id)
        .and_then(|info| info.type_id())
        .ok_or_else(|| ModSaveLoadError::MigrationFailed {
            component_name: active.name.to_string(),
            reason: "component lacks a Rust TypeId for ReflectComponent insert".to_owned(),
        })?;
    let app_types = world
        .get_resource::<AppTypeRegistry>()
        .ok_or_else(|| ModSaveLoadError::Serialization("AppTypeRegistry missing".to_owned()))?
        .clone();
    let (deserialized, reflect_comp) = {
        let types = app_types.read();
        let reg = types
            .get(type_id)
            .ok_or_else(|| ModSaveLoadError::MigrationFailed {
                component_name: active.name.to_string(),
                reason: "type not registered in AppTypeRegistry".to_owned(),
            })?;
        let reflect_comp = reg.data::<ReflectComponent>().cloned().ok_or_else(|| {
            ModSaveLoadError::MigrationFailed {
                component_name: active.name.to_string(),
                reason: "type lacks ReflectComponent type data".to_owned(),
            }
        })?;
        let de = TypedReflectDeserializer::new(reg, &types);
        let val = de.deserialize(value.clone()).map_err(|e| {
            ModSaveLoadError::Serialization(format!(
                "reflect deserialize failed for '{}': {e}",
                active.name
            ))
        })?;
        (val, reflect_comp)
    };
    let types = app_types.read();
    let mut ent_mut = world
        .get_entity_mut(entity)
        .map_err(|_| ModSaveLoadError::Ffi {
            component_name: active.name.to_string(),
            status: FfiResult::INVALID_ENTITY,
        })?;
    reflect_comp.insert(&mut ent_mut, deserialized.as_partial_reflect(), &types);
    Ok(())
}

fn try_migrate_typed_reflect_struct(
    world: &mut World,
    entity: Entity,
    active: &RegisteredComponent,
    source_fields: &Map<String, Value>,
) -> Result<Option<bool>, ModSaveLoadError> {
    let Some(type_id) = world
        .components()
        .get_info(active.component_id)
        .and_then(|info| info.type_id())
    else {
        return Ok(None);
    };
    let Some(app_types) = world.get_resource::<AppTypeRegistry>().cloned() else {
        return Ok(None);
    };
    let (reflect_comp, default_box, field_names) = {
        let types = app_types.read();
        let Some(reg) = types.get(type_id) else {
            return Ok(None);
        };
        let Some(reflect_comp) = reg.data::<ReflectComponent>().cloned() else {
            return Ok(None);
        };
        let Some(reflect_default) = reg.data::<bevy_reflect::prelude::ReflectDefault>() else {
            return Ok(None);
        };
        let TypeInfo::Struct(struct_info) = reg.type_info() else {
            return Ok(None);
        };
        let names: Vec<String> = (0..struct_info.field_len())
            .filter_map(|i| struct_info.field_at(i).map(|f| f.name().to_owned()))
            .collect();
        (reflect_comp, reflect_default.default(), names)
    };

    let mut dyn_struct = DynamicStruct::default();
    if let ReflectRef::Struct(s) = default_box.reflect_ref() {
        for name in &field_names {
            if let Some(json_val) = source_fields.get(name)
                && let Some(default_field) = s.field(name)
                && let Some(converted) = json_to_matching_reflect_field(default_field, json_val)
            {
                dyn_struct.insert_boxed(name.clone(), converted);
            }
        }
    }

    let mut target = default_box;
    target.apply(dyn_struct.as_partial_reflect());

    let types = app_types.read();
    let mut ent_mut = world
        .get_entity_mut(entity)
        .map_err(|_| ModSaveLoadError::Ffi {
            component_name: active.name.to_string(),
            status: FfiResult::INVALID_ENTITY,
        })?;
    reflect_comp.insert(&mut ent_mut, target.as_partial_reflect(), &types);
    Ok(Some(true))
}

fn json_to_matching_reflect_field(
    template: &dyn PartialReflect,
    val: &Value,
) -> Option<Box<dyn PartialReflect>> {
    if template.try_downcast_ref::<bool>().is_some() {
        return val
            .as_bool()
            .map(|b| Box::new(b) as Box<dyn PartialReflect>);
    }
    if template.try_downcast_ref::<f32>().is_some() {
        return val
            .as_f64()
            .map(|f| Box::new(f as f32) as Box<dyn PartialReflect>);
    }
    if template.try_downcast_ref::<f64>().is_some() {
        return val.as_f64().map(|f| Box::new(f) as Box<dyn PartialReflect>);
    }
    if template.try_downcast_ref::<i32>().is_some() {
        return val
            .as_i64()
            .and_then(|n| i32::try_from(n).ok())
            .map(|n| Box::new(n) as Box<dyn PartialReflect>);
    }
    if template.try_downcast_ref::<i64>().is_some() {
        return val.as_i64().map(|n| Box::new(n) as Box<dyn PartialReflect>);
    }
    if template.try_downcast_ref::<u32>().is_some() {
        return val
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .map(|n| Box::new(n) as Box<dyn PartialReflect>);
    }
    if template.try_downcast_ref::<u64>().is_some() {
        return val.as_u64().map(|n| Box::new(n) as Box<dyn PartialReflect>);
    }
    if template.try_downcast_ref::<String>().is_some() {
        return val
            .as_str()
            .map(|s| Box::new(s.to_owned()) as Box<dyn PartialReflect>);
    }
    None
}

// ============================================================================
// POD / BamFieldDesc / Witness JSON Codecs
// ============================================================================

fn decode_pod_bytes_with_bam_fields(
    raw: &[u8],
    fields: &[BamFieldDescriptor],
) -> Result<Map<String, Value>, ModSaveLoadError> {
    let mut map = Map::new();
    for field in fields {
        let Some(pod_kind) = field.pod_kind else {
            continue;
        };
        let end = field
            .offset
            .checked_add(field.size)
            .ok_or_else(|| ModSaveLoadError::CorruptArchive("field offset overflow".to_owned()))?;
        let slice = raw.get(field.offset..end).ok_or_else(|| {
            ModSaveLoadError::CorruptArchive(format!(
                "field '{}' range {}..{} out of bounds for {} bytes",
                field.name,
                field.offset,
                end,
                raw.len()
            ))
        })?;
        if let Some(val) = decode_pod_field_bytes_to_json(pod_kind, slice) {
            map.insert(field.name.clone(), val);
        }
    }
    Ok(map)
}

fn decode_raw_bytes_using_saved_fields(
    raw: &[u8],
    saved_fields: &[SavedFieldHeader],
) -> Map<String, Value> {
    let mut map = Map::new();
    for field in saved_fields {
        let Some(pod_kind) =
            BamFieldPodKind::classify(BamFieldKind(field.kind), &field.type_tag, field.size)
        else {
            continue;
        };
        let Some(end) = field.offset.checked_add(field.size) else {
            continue;
        };
        let Some(slice) = raw.get(field.offset..end) else {
            continue;
        };
        if let Some(val) = decode_pod_field_bytes_to_json(pod_kind, slice) {
            map.insert(field.name.clone(), val);
        }
    }
    map
}

fn f32_to_json(v: f32) -> Value {
    Number::from_f64(f64::from(v))
        .map(Value::Number)
        .unwrap_or(Value::Null)
}

fn f64_to_json(v: f64) -> Value {
    Number::from_f64(v)
        .map(Value::Number)
        .unwrap_or(Value::Null)
}

fn decode_pod_field_bytes_to_json(kind: BamFieldPodKind, bytes: &[u8]) -> Option<Value> {
    match kind {
        BamFieldPodKind::Bool => Some(Value::Bool(*bytes.first()? != 0)),
        BamFieldPodKind::U8 => Some(Value::from(*bytes.first()?)),
        BamFieldPodKind::I8 => Some(Value::from(i8::from_ne_bytes(bytes.try_into().ok()?))),
        BamFieldPodKind::U16 => Some(Value::from(u16::from_ne_bytes(bytes.try_into().ok()?))),
        BamFieldPodKind::I16 => Some(Value::from(i16::from_ne_bytes(bytes.try_into().ok()?))),
        BamFieldPodKind::U32 | BamFieldPodKind::Component => {
            Some(Value::from(u32::from_ne_bytes(bytes.try_into().ok()?)))
        }
        BamFieldPodKind::I32 => Some(Value::from(i32::from_ne_bytes(bytes.try_into().ok()?))),
        BamFieldPodKind::U64 | BamFieldPodKind::Entity => {
            Some(Value::from(u64::from_ne_bytes(bytes.try_into().ok()?)))
        }
        BamFieldPodKind::I64 => Some(Value::from(i64::from_ne_bytes(bytes.try_into().ok()?))),
        BamFieldPodKind::Usize => Some(Value::from(
            usize::from_ne_bytes(bytes.try_into().ok()?) as u64
        )),
        BamFieldPodKind::Isize => Some(Value::from(
            isize::from_ne_bytes(bytes.try_into().ok()?) as i64
        )),
        BamFieldPodKind::F32 => Some(f32_to_json(f32::from_ne_bytes(bytes.try_into().ok()?))),
        BamFieldPodKind::F64 => Some(f64_to_json(f64::from_ne_bytes(bytes.try_into().ok()?))),
        BamFieldPodKind::Vec2 | BamFieldPodKind::Vec3 | BamFieldPodKind::Vec4 => {
            let mut arr = Vec::with_capacity(bytes.len() / 4);
            for &c in bytes.as_chunks::<4>().0 {
                arr.push(f32_to_json(f32::from_ne_bytes(c)));
            }
            Some(Value::Array(arr))
        }
        BamFieldPodKind::DVec2 | BamFieldPodKind::DVec3 | BamFieldPodKind::DVec4 => {
            let mut arr = Vec::with_capacity(bytes.len() / 8);
            for &c in bytes.as_chunks::<8>().0 {
                arr.push(f64_to_json(f64::from_ne_bytes(c)));
            }
            Some(Value::Array(arr))
        }
        BamFieldPodKind::UVec2 | BamFieldPodKind::UVec3 | BamFieldPodKind::UVec4 => {
            let mut arr = Vec::with_capacity(bytes.len() / 4);
            for &c in bytes.as_chunks::<4>().0 {
                arr.push(Value::from(u32::from_ne_bytes(c)));
            }
            Some(Value::Array(arr))
        }
        BamFieldPodKind::IVec2 | BamFieldPodKind::IVec3 | BamFieldPodKind::IVec4 => {
            let mut arr = Vec::with_capacity(bytes.len() / 4);
            for &c in bytes.as_chunks::<4>().0 {
                arr.push(Value::from(i32::from_ne_bytes(c)));
            }
            Some(Value::Array(arr))
        }
        BamFieldPodKind::Bytes2
        | BamFieldPodKind::Bytes3
        | BamFieldPodKind::Bytes4
        | BamFieldPodKind::Bytes8
        | BamFieldPodKind::Bytes16 => Some(Value::Array(
            bytes.iter().map(|&b| Value::from(b)).collect(),
        )),
    }
}

fn json_as_i128(field_name: &str, v: &Value) -> Result<i128, String> {
    if let Some(n) = v.as_i64() {
        Ok(i128::from(n))
    } else if let Some(n) = v.as_u64() {
        Ok(i128::from(n))
    } else {
        Err(format!("field '{field_name}' expected integer value"))
    }
}

fn json_as_f64(field_name: &str, v: &Value) -> Result<f64, String> {
    v.as_f64()
        .ok_or_else(|| format!("field '{field_name}' expected numeric value"))
}

fn encode_json_to_pod_field_bytes(
    field_name: &str,
    kind: BamFieldPodKind,
    val: &Value,
) -> Result<Vec<u8>, String> {
    match kind {
        BamFieldPodKind::Bool => val
            .as_bool()
            .map(|b| vec![u8::from(b)])
            .ok_or_else(|| format!("field '{field_name}' expected bool")),
        BamFieldPodKind::U8 => u8::try_from(json_as_i128(field_name, val)?)
            .map(|x| x.to_ne_bytes().to_vec())
            .map_err(|_| format!("field '{field_name}' out of range for u8")),
        BamFieldPodKind::I8 => i8::try_from(json_as_i128(field_name, val)?)
            .map(|x| x.to_ne_bytes().to_vec())
            .map_err(|_| format!("field '{field_name}' out of range for i8")),
        BamFieldPodKind::U16 => u16::try_from(json_as_i128(field_name, val)?)
            .map(|x| x.to_ne_bytes().to_vec())
            .map_err(|_| format!("field '{field_name}' out of range for u16")),
        BamFieldPodKind::I16 => i16::try_from(json_as_i128(field_name, val)?)
            .map(|x| x.to_ne_bytes().to_vec())
            .map_err(|_| format!("field '{field_name}' out of range for i16")),
        BamFieldPodKind::U32 | BamFieldPodKind::Component => {
            u32::try_from(json_as_i128(field_name, val)?)
                .map(|x| x.to_ne_bytes().to_vec())
                .map_err(|_| format!("field '{field_name}' out of range for u32"))
        }
        BamFieldPodKind::I32 => i32::try_from(json_as_i128(field_name, val)?)
            .map(|x| x.to_ne_bytes().to_vec())
            .map_err(|_| format!("field '{field_name}' out of range for i32")),
        BamFieldPodKind::U64 | BamFieldPodKind::Entity => {
            u64::try_from(json_as_i128(field_name, val)?)
                .map(|x| x.to_ne_bytes().to_vec())
                .map_err(|_| format!("field '{field_name}' out of range for u64"))
        }
        BamFieldPodKind::I64 => i64::try_from(json_as_i128(field_name, val)?)
            .map(|x| x.to_ne_bytes().to_vec())
            .map_err(|_| format!("field '{field_name}' out of range for i64")),
        BamFieldPodKind::Usize => usize::try_from(json_as_i128(field_name, val)?)
            .map(|x| x.to_ne_bytes().to_vec())
            .map_err(|_| format!("field '{field_name}' out of range for usize")),
        BamFieldPodKind::Isize => isize::try_from(json_as_i128(field_name, val)?)
            .map(|x| x.to_ne_bytes().to_vec())
            .map_err(|_| format!("field '{field_name}' out of range for isize")),
        BamFieldPodKind::F32 => Ok((json_as_f64(field_name, val)? as f32)
            .to_ne_bytes()
            .to_vec()),
        BamFieldPodKind::F64 => Ok(json_as_f64(field_name, val)?.to_ne_bytes().to_vec()),
        BamFieldPodKind::Vec2 | BamFieldPodKind::Vec3 | BamFieldPodKind::Vec4 => {
            let expected = kind.byte_size() / 4;
            let arr = val
                .as_array()
                .ok_or_else(|| format!("field '{field_name}' expected array"))?;
            if arr.len() != expected {
                return Err(format!(
                    "field '{field_name}' expected {expected} elements, got {}",
                    arr.len()
                ));
            }
            let mut out = Vec::with_capacity(kind.byte_size());
            for el in arr {
                out.extend_from_slice(&(json_as_f64(field_name, el)? as f32).to_ne_bytes());
            }
            Ok(out)
        }
        BamFieldPodKind::DVec2 | BamFieldPodKind::DVec3 | BamFieldPodKind::DVec4 => {
            let expected = kind.byte_size() / 8;
            let arr = val
                .as_array()
                .ok_or_else(|| format!("field '{field_name}' expected array"))?;
            if arr.len() != expected {
                return Err(format!(
                    "field '{field_name}' expected {expected} elements, got {}",
                    arr.len()
                ));
            }
            let mut out = Vec::with_capacity(kind.byte_size());
            for el in arr {
                out.extend_from_slice(&json_as_f64(field_name, el)?.to_ne_bytes());
            }
            Ok(out)
        }
        BamFieldPodKind::UVec2 | BamFieldPodKind::UVec3 | BamFieldPodKind::UVec4 => {
            let expected = kind.byte_size() / 4;
            let arr = val
                .as_array()
                .ok_or_else(|| format!("field '{field_name}' expected array"))?;
            if arr.len() != expected {
                return Err(format!(
                    "field '{field_name}' expected {expected} elements, got {}",
                    arr.len()
                ));
            }
            let mut out = Vec::with_capacity(kind.byte_size());
            for el in arr {
                let n = u32::try_from(json_as_i128(field_name, el)?)
                    .map_err(|_| format!("field '{field_name}' element out of range for u32"))?;
                out.extend_from_slice(&n.to_ne_bytes());
            }
            Ok(out)
        }
        BamFieldPodKind::IVec2 | BamFieldPodKind::IVec3 | BamFieldPodKind::IVec4 => {
            let expected = kind.byte_size() / 4;
            let arr = val
                .as_array()
                .ok_or_else(|| format!("field '{field_name}' expected array"))?;
            if arr.len() != expected {
                return Err(format!(
                    "field '{field_name}' expected {expected} elements, got {}",
                    arr.len()
                ));
            }
            let mut out = Vec::with_capacity(kind.byte_size());
            for el in arr {
                let n = i32::try_from(json_as_i128(field_name, el)?)
                    .map_err(|_| format!("field '{field_name}' element out of range for i32"))?;
                out.extend_from_slice(&n.to_ne_bytes());
            }
            Ok(out)
        }
        BamFieldPodKind::Bytes2
        | BamFieldPodKind::Bytes3
        | BamFieldPodKind::Bytes4
        | BamFieldPodKind::Bytes8
        | BamFieldPodKind::Bytes16 => {
            let expected = kind.byte_size();
            let arr = val
                .as_array()
                .ok_or_else(|| format!("field '{field_name}' expected byte array"))?;
            if arr.len() != expected {
                return Err(format!(
                    "field '{field_name}' expected {expected} bytes, got {}",
                    arr.len()
                ));
            }
            let mut out = Vec::with_capacity(expected);
            for el in arr {
                let b = u8::try_from(json_as_i128(field_name, el)?)
                    .map_err(|_| format!("field '{field_name}' element out of range for u8"))?;
                out.push(b);
            }
            Ok(out)
        }
    }
}

fn exported_records_to_json_map(records: &[ExportedFieldRecord]) -> Map<String, Value> {
    let mut map = Map::with_capacity(records.len());
    for rec in records {
        if let Some(v) = decode_witness_record_to_json(&rec.type_tag, &rec.bytes) {
            map.insert(rec.field_name.clone(), v);
        }
    }
    map
}

fn decode_witness_record_to_json(type_tag: &str, bytes: &[u8]) -> Option<Value> {
    match type_tag {
        "bool" => Some(Value::Bool(*bytes.first()? != 0)),
        "u8" => Some(Value::from(*bytes.first()?)),
        "i8" => Some(Value::from(i8::from_le_bytes(bytes.try_into().ok()?))),
        "u16" => Some(Value::from(u16::from_le_bytes(bytes.try_into().ok()?))),
        "i16" => Some(Value::from(i16::from_le_bytes(bytes.try_into().ok()?))),
        "u32" => Some(Value::from(u32::from_le_bytes(bytes.try_into().ok()?))),
        "i32" => Some(Value::from(i32::from_le_bytes(bytes.try_into().ok()?))),
        "u64" | "Entity" | "FfiEntity" => {
            Some(Value::from(u64::from_le_bytes(bytes.try_into().ok()?)))
        }
        "i64" => Some(Value::from(i64::from_le_bytes(bytes.try_into().ok()?))),
        "f32" => Some(f32_to_json(f32::from_le_bytes(bytes.try_into().ok()?))),
        "f64" => Some(f64_to_json(f64::from_le_bytes(bytes.try_into().ok()?))),
        "String" => core::str::from_utf8(bytes)
            .ok()
            .map(|s| Value::String(s.to_owned())),
        "Vec<u8>" => Some(Value::Array(
            bytes.iter().map(|&b| Value::from(b)).collect(),
        )),
        "Vec<u32>" => {
            if !bytes.len().is_multiple_of(4) {
                return None;
            }
            Some(Value::Array(
                bytes
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|&c| Value::from(u32::from_le_bytes(c)))
                    .collect(),
            ))
        }
        "Vec<i32>" => {
            if !bytes.len().is_multiple_of(4) {
                return None;
            }
            Some(Value::Array(
                bytes
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|&c| Value::from(i32::from_le_bytes(c)))
                    .collect(),
            ))
        }
        "Vec<f32>" => {
            if !bytes.len().is_multiple_of(4) {
                return None;
            }
            Some(Value::Array(
                bytes
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|&c| f32_to_json(f32::from_le_bytes(c)))
                    .collect(),
            ))
        }
        "FfiOption<u32>" => match *bytes.first()? {
            0 => Some(Value::Null),
            _ if bytes.len() == 5 => Some(Value::from(u32::from_le_bytes([
                bytes[1], bytes[2], bytes[3], bytes[4],
            ]))),
            _ => None,
        },
        "FfiOption<i32>" => match *bytes.first()? {
            0 => Some(Value::Null),
            _ if bytes.len() == 5 => Some(Value::from(i32::from_le_bytes([
                bytes[1], bytes[2], bytes[3], bytes[4],
            ]))),
            _ => None,
        },
        "FfiOption<f32>" => match *bytes.first()? {
            0 => Some(Value::Null),
            _ if bytes.len() == 5 => Some(f32_to_json(f32::from_le_bytes([
                bytes[1], bytes[2], bytes[3], bytes[4],
            ]))),
            _ => None,
        },
        _ => Some(Value::String(encode_hex_lower(bytes))),
    }
}

fn json_to_witness_record(
    field_name: &str,
    type_tag: &str,
    val: &Value,
) -> Result<ExportedFieldRecord, String> {
    let bytes = match type_tag {
        "bool" => {
            vec![u8::from(val.as_bool().ok_or_else(|| {
                format!("field '{field_name}' expected bool")
            })?)]
        }
        "u8" => u8::try_from(json_as_i128(field_name, val)?)
            .map(|x| x.to_le_bytes().to_vec())
            .map_err(|_| format!("field '{field_name}' out of range for u8"))?,
        "i8" => i8::try_from(json_as_i128(field_name, val)?)
            .map(|x| x.to_le_bytes().to_vec())
            .map_err(|_| format!("field '{field_name}' out of range for i8"))?,
        "u16" => u16::try_from(json_as_i128(field_name, val)?)
            .map(|x| x.to_le_bytes().to_vec())
            .map_err(|_| format!("field '{field_name}' out of range for u16"))?,
        "i16" => i16::try_from(json_as_i128(field_name, val)?)
            .map(|x| x.to_le_bytes().to_vec())
            .map_err(|_| format!("field '{field_name}' out of range for i16"))?,
        "u32" => u32::try_from(json_as_i128(field_name, val)?)
            .map(|x| x.to_le_bytes().to_vec())
            .map_err(|_| format!("field '{field_name}' out of range for u32"))?,
        "i32" => i32::try_from(json_as_i128(field_name, val)?)
            .map(|x| x.to_le_bytes().to_vec())
            .map_err(|_| format!("field '{field_name}' out of range for i32"))?,
        "u64" | "Entity" | "FfiEntity" => u64::try_from(json_as_i128(field_name, val)?)
            .map(|x| x.to_le_bytes().to_vec())
            .map_err(|_| format!("field '{field_name}' out of range for u64"))?,
        "i64" => i64::try_from(json_as_i128(field_name, val)?)
            .map(|x| x.to_le_bytes().to_vec())
            .map_err(|_| format!("field '{field_name}' out of range for i64"))?,
        "f32" => (json_as_f64(field_name, val)? as f32)
            .to_le_bytes()
            .to_vec(),
        "f64" => json_as_f64(field_name, val)?.to_le_bytes().to_vec(),
        "String" => val
            .as_str()
            .ok_or_else(|| format!("field '{field_name}' expected string"))?
            .as_bytes()
            .to_vec(),
        "Vec<u8>" => {
            let arr = val
                .as_array()
                .ok_or_else(|| format!("field '{field_name}' expected array"))?;
            let mut out = Vec::with_capacity(arr.len());
            for el in arr {
                out.push(
                    u8::try_from(json_as_i128(field_name, el)?)
                        .map_err(|_| format!("field '{field_name}' element out of range for u8"))?,
                );
            }
            out
        }
        "Vec<u32>" => {
            let arr = val
                .as_array()
                .ok_or_else(|| format!("field '{field_name}' expected array"))?;
            let mut out = Vec::with_capacity(arr.len() * 4);
            for el in arr {
                let n = u32::try_from(json_as_i128(field_name, el)?)
                    .map_err(|_| format!("field '{field_name}' element out of range for u32"))?;
                out.extend_from_slice(&n.to_le_bytes());
            }
            out
        }
        "Vec<i32>" => {
            let arr = val
                .as_array()
                .ok_or_else(|| format!("field '{field_name}' expected array"))?;
            let mut out = Vec::with_capacity(arr.len() * 4);
            for el in arr {
                let n = i32::try_from(json_as_i128(field_name, el)?)
                    .map_err(|_| format!("field '{field_name}' element out of range for i32"))?;
                out.extend_from_slice(&n.to_le_bytes());
            }
            out
        }
        "Vec<f32>" => {
            let arr = val
                .as_array()
                .ok_or_else(|| format!("field '{field_name}' expected array"))?;
            let mut out = Vec::with_capacity(arr.len() * 4);
            for el in arr {
                out.extend_from_slice(&(json_as_f64(field_name, el)? as f32).to_le_bytes());
            }
            out
        }
        "FfiOption<u32>" => {
            if val.is_null() {
                vec![0u8]
            } else {
                let n = u32::try_from(json_as_i128(field_name, val)?)
                    .map_err(|_| format!("field '{field_name}' out of range for u32"))?;
                let mut out = vec![1u8];
                out.extend_from_slice(&n.to_le_bytes());
                out
            }
        }
        "FfiOption<i32>" => {
            if val.is_null() {
                vec![0u8]
            } else {
                let n = i32::try_from(json_as_i128(field_name, val)?)
                    .map_err(|_| format!("field '{field_name}' out of range for i32"))?;
                let mut out = vec![1u8];
                out.extend_from_slice(&n.to_le_bytes());
                out
            }
        }
        "FfiOption<f32>" => {
            if val.is_null() {
                vec![0u8]
            } else {
                let f = json_as_f64(field_name, val)? as f32;
                let mut out = vec![1u8];
                out.extend_from_slice(&f.to_le_bytes());
                out
            }
        }
        other => {
            return Err(format!(
                "unsupported witness field type_tag '{other}' for field '{field_name}'"
            ));
        }
    };
    Ok(ExportedFieldRecord {
        field_name: field_name.to_owned(),
        type_tag: type_tag.to_owned(),
        bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_and_base64_codecs_roundtrip_and_reject_corrupt_inputs() {
        for len in [0usize, 1, 2, 3, 4, 15, 16, 33, 64] {
            let input: Vec<u8> = (0..len).map(|i| ((i * 37 + 11) & 0xff) as u8).collect();
            let hex = encode_hex_lower(&input);
            assert_eq!(decode_hex(&hex).unwrap(), input);
            let b64 = encode_base64_standard(&input);
            assert_eq!(decode_base64_standard(&b64).unwrap(), input);
        }
        assert!(decode_hex("abc").is_err());
        assert!(decode_hex("zz").is_err());
        assert!(decode_base64_standard("abc").is_err());
        assert!(decode_base64_standard("ab=c").is_err());
        assert!(decode_base64_standard("!!!!").is_err());
    }

    #[test]
    fn binary_archive_detects_magic_version_truncation_and_checksum_corruption() {
        let archive = ModSaveArchive {
            format_version: MOD_SAVE_ARCHIVE_VERSION,
            mods: Vec::new(),
            component_headers: Vec::new(),
            entities: Vec::new(),
        };
        let bytes = archive.to_binary_bytes().unwrap();
        assert_eq!(ModSaveArchive::from_binary_bytes(&bytes).unwrap(), archive);

        // Corrupt payload byte -> checksum mismatch.
        let mut flipped = bytes.clone();
        *flipped.last_mut().unwrap() ^= 0x01;
        assert!(matches!(
            ModSaveArchive::from_binary_bytes(&flipped),
            Err(ModSaveLoadError::CorruptArchive(_))
        ));

        // Corrupt magic.
        let mut bad_magic = bytes.clone();
        bad_magic[0] = b'X';
        assert!(matches!(
            ModSaveArchive::from_binary_bytes(&bad_magic),
            Err(ModSaveLoadError::CorruptArchive(_))
        ));

        // Unsupported version.
        let mut bad_ver = bytes.clone();
        bad_ver[8..12].copy_from_slice(&99u32.to_le_bytes());
        assert!(matches!(
            ModSaveArchive::from_binary_bytes(&bad_ver),
            Err(ModSaveLoadError::UnsupportedArchiveVersion { found: 99, .. })
        ));

        // Truncated.
        assert!(matches!(
            ModSaveArchive::from_binary_bytes(&bytes[..10]),
            Err(ModSaveLoadError::CorruptArchive(_))
        ));
    }
}
