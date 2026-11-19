//! Deterministic single-source C99 header generator for `include/bevy_abi_mod.h`.
//!
//! Reads ABI constants, discriminants, flags, limits, and verified `size_of` / `align_of` /
//! `offset_of!` metadata directly from `bevy_abi_mod_sys` so the C99 wire contract cannot drift
//! from the Rust source of truth.

use std::fmt::Write as _;
use std::fs;
use std::mem::{align_of, offset_of, size_of};
use std::path::Path;

use bevy_abi_mod_sys::{
    ABI_VERSION, ABI_VERSION_MAJOR, ABI_VERSION_MINOR, ABI_VERSION_PATCH, AccessMode,
    BAM_CHILD_OF_NAME, BAM_CHILDREN_NAME, BAM_COMPONENT_FLAG_IMMUTABLE,
    BAM_COMPONENT_FLAG_SPARSE_SET, BAM_COMPONENT_FLAG_SUMMARY_TICK, BAM_COMPONENT_KNOWN_FLAGS,
    BAM_LOG_DEBUG, BAM_LOG_ERROR, BAM_LOG_INFO, BAM_LOG_TRACE, BAM_LOG_WARN,
    BAM_MANIFEST_KNOWN_FLAGS, BAM_MAX_CHANGE_AGE, BAM_OBSERVER_FLAG_PROPAGATE,
    BAM_OBSERVER_KNOWN_FLAGS, BAM_QUERY_KNOWN_FLAGS, BAM_RELATION_FLAG_CASCADE_DESPAWN,
    BAM_RELATION_FLAG_EXCLUSIVE, BAM_RELATION_FLAG_SYMMETRIC, BAM_RELATION_KNOWN_FLAGS,
    BAM_REQUIRED_FLAG_CUSTOM_BYTES, BAM_REQUIRED_FLAG_INIT_FN, BAM_REQUIRED_FLAG_RESOLVE_BY_UUID,
    BAM_REQUIRED_KNOWN_FLAGS, BAM_SCHEDULE_SET_KNOWN_FLAGS, BAM_STORAGE_FLAG_ENTITY_EVENT,
    BAM_STORAGE_FLAG_EVENT, BAM_STORAGE_FLAG_IMMUTABLE, BAM_STORAGE_FLAG_MESSAGE,
    BAM_STORAGE_FLAG_RESOURCE, BAM_STORAGE_FLAG_SPARSE_SET, BAM_STORAGE_FLAG_SUMMARY_TICK,
    BAM_SYSTEM_FLAG_AMBIGUOUS_WITH_ALL, BAM_SYSTEM_FLAG_MAIN_THREAD, BAM_SYSTEM_KNOWN_FLAGS,
    BAM_TERM_KNOWN_FLAGS, BAM_TRIGGER_FLAG_PROPAGATE, BAM_TRIGGER_KNOWN_FLAGS, BamChangeTicks,
    BamColumnInfo, BamComponent, BamEventKind, BamFieldDesc, BamFieldKind, BamHostApi,
    BamLayoutInfo, BamManifest, BamObserverDesc, BamQueryDesc, BamRelationDesc,
    BamRequiredComponent, BamScheduleSetDesc, BamSymbol, BamSystem, BamSystemDesc, BamSystemSet,
    BamTermDesc, BamTriggerDesc, ComponentDesc, ComponentUuid, FfiAssetHandle, FfiAssetLoadState,
    FfiBool, FfiChildOf, FfiEntity, FfiOption, FfiResult, MAX_QUERIES_PER_SYSTEM,
    MAX_TERMS_PER_QUERY, MappedStringVTable, SIPHASH_KEY_0, SIPHASH_KEY_1, ScheduleId, StrView,
    VTableHeader,
};

#[derive(Clone, Copy)]
enum Align32 {
    Fixed(usize),
    U64,
}

struct FieldLayoutSpec {
    c_name: &'static str,
    offset_64: usize,
    offset_32: usize,
    rust_host_offset: usize,
}

struct StructLayoutSpec {
    c_name: &'static str,
    assert_stem: &'static str,
    size_64: usize,
    align_64: usize,
    size_32: usize,
    align_32: Align32,
    rust_host_size: usize,
    rust_host_align: usize,
    fields: &'static [FieldLayoutSpec],
}

fn wire_struct_specs() -> Vec<StructLayoutSpec> {
    vec![
        StructLayoutSpec {
            c_name: "BamComponentUuid",
            assert_stem: "bam_component_uuid",
            size_64: 16,
            align_64: 1,
            size_32: 16,
            align_32: Align32::Fixed(1),
            rust_host_size: size_of::<ComponentUuid>(),
            rust_host_align: align_of::<ComponentUuid>(),
            fields: &[FieldLayoutSpec {
                c_name: "bytes",
                offset_64: 0,
                offset_32: 0,
                rust_host_offset: offset_of!(ComponentUuid, bytes),
            }],
        },
        StructLayoutSpec {
            c_name: "BamStrView",
            assert_stem: "bam_str_view",
            size_64: 16,
            align_64: 8,
            size_32: 8,
            align_32: Align32::Fixed(4),
            rust_host_size: size_of::<StrView>(),
            rust_host_align: align_of::<StrView>(),
            fields: &[
                FieldLayoutSpec {
                    c_name: "ptr",
                    offset_64: 0,
                    offset_32: 0,
                    rust_host_offset: offset_of!(StrView, ptr),
                },
                FieldLayoutSpec {
                    c_name: "len",
                    offset_64: 8,
                    offset_32: 4,
                    rust_host_offset: offset_of!(StrView, len),
                },
            ],
        },
        StructLayoutSpec {
            c_name: "BamManifest",
            assert_stem: "bam_manifest",
            size_64: 48,
            align_64: 8,
            size_32: 32,
            align_32: Align32::Fixed(4),
            rust_host_size: size_of::<BamManifest>(),
            rust_host_align: align_of::<BamManifest>(),
            fields: &[
                FieldLayoutSpec {
                    c_name: "struct_size",
                    offset_64: 0,
                    offset_32: 0,
                    rust_host_offset: offset_of!(BamManifest, struct_size),
                },
                FieldLayoutSpec {
                    c_name: "abi_version",
                    offset_64: 4,
                    offset_32: 4,
                    rust_host_offset: offset_of!(BamManifest, abi_version),
                },
                FieldLayoutSpec {
                    c_name: "name",
                    offset_64: 8,
                    offset_32: 8,
                    rust_host_offset: offset_of!(BamManifest, name),
                },
                FieldLayoutSpec {
                    c_name: "version",
                    offset_64: 16,
                    offset_32: 12,
                    rust_host_offset: offset_of!(BamManifest, version),
                },
                FieldLayoutSpec {
                    c_name: "flags",
                    offset_64: 24,
                    offset_32: 16,
                    rust_host_offset: offset_of!(BamManifest, flags),
                },
                FieldLayoutSpec {
                    c_name: "reserved0",
                    offset_64: 28,
                    offset_32: 20,
                    rust_host_offset: offset_of!(BamManifest, reserved0),
                },
                FieldLayoutSpec {
                    c_name: "save_state",
                    offset_64: 32,
                    offset_32: 24,
                    rust_host_offset: offset_of!(BamManifest, save_state),
                },
                FieldLayoutSpec {
                    c_name: "restore_state",
                    offset_64: 40,
                    offset_32: 28,
                    rust_host_offset: offset_of!(BamManifest, restore_state),
                },
            ],
        },
        StructLayoutSpec {
            c_name: "BamAssetHandle",
            assert_stem: "bam_asset_handle",
            size_64: 32,
            align_64: 4,
            size_32: 32,
            align_32: Align32::Fixed(4),
            rust_host_size: size_of::<FfiAssetHandle>(),
            rust_host_align: align_of::<FfiAssetHandle>(),
            fields: &[
                FieldLayoutSpec {
                    c_name: "kind",
                    offset_64: 0,
                    offset_32: 0,
                    rust_host_offset: offset_of!(FfiAssetHandle, kind),
                },
                FieldLayoutSpec {
                    c_name: "reserved0",
                    offset_64: 1,
                    offset_32: 1,
                    rust_host_offset: offset_of!(FfiAssetHandle, reserved0),
                },
                FieldLayoutSpec {
                    c_name: "asset_type",
                    offset_64: 4,
                    offset_32: 4,
                    rust_host_offset: offset_of!(FfiAssetHandle, asset_type),
                },
                FieldLayoutSpec {
                    c_name: "index",
                    offset_64: 8,
                    offset_32: 8,
                    rust_host_offset: offset_of!(FfiAssetHandle, index),
                },
                FieldLayoutSpec {
                    c_name: "generation",
                    offset_64: 12,
                    offset_32: 12,
                    rust_host_offset: offset_of!(FfiAssetHandle, generation),
                },
                FieldLayoutSpec {
                    c_name: "uuid",
                    offset_64: 16,
                    offset_32: 16,
                    rust_host_offset: offset_of!(FfiAssetHandle, uuid),
                },
            ],
        },
        StructLayoutSpec {
            c_name: "BamTermDesc",
            assert_stem: "bam_term_desc",
            size_64: 12,
            align_64: 4,
            size_32: 12,
            align_32: Align32::Fixed(4),
            rust_host_size: size_of::<BamTermDesc>(),
            rust_host_align: align_of::<BamTermDesc>(),
            fields: &[
                FieldLayoutSpec {
                    c_name: "component",
                    offset_64: 0,
                    offset_32: 0,
                    rust_host_offset: offset_of!(BamTermDesc, component),
                },
                FieldLayoutSpec {
                    c_name: "mode",
                    offset_64: 4,
                    offset_32: 4,
                    rust_host_offset: offset_of!(BamTermDesc, mode),
                },
                FieldLayoutSpec {
                    c_name: "flags",
                    offset_64: 8,
                    offset_32: 8,
                    rust_host_offset: offset_of!(BamTermDesc, flags),
                },
                FieldLayoutSpec {
                    c_name: "or_group",
                    offset_64: 10,
                    offset_32: 10,
                    rust_host_offset: offset_of!(BamTermDesc, or_group),
                },
                FieldLayoutSpec {
                    c_name: "reserved0",
                    offset_64: 11,
                    offset_32: 11,
                    rust_host_offset: offset_of!(BamTermDesc, reserved0),
                },
            ],
        },
        StructLayoutSpec {
            c_name: "BamQueryDesc",
            assert_stem: "bam_query_desc",
            size_64: 24,
            align_64: 8,
            size_32: 16,
            align_32: Align32::Fixed(4),
            rust_host_size: size_of::<BamQueryDesc>(),
            rust_host_align: align_of::<BamQueryDesc>(),
            fields: &[
                FieldLayoutSpec {
                    c_name: "struct_size",
                    offset_64: 0,
                    offset_32: 0,
                    rust_host_offset: offset_of!(BamQueryDesc, struct_size),
                },
                FieldLayoutSpec {
                    c_name: "flags",
                    offset_64: 4,
                    offset_32: 4,
                    rust_host_offset: offset_of!(BamQueryDesc, flags),
                },
                FieldLayoutSpec {
                    c_name: "terms",
                    offset_64: 8,
                    offset_32: 8,
                    rust_host_offset: offset_of!(BamQueryDesc, terms),
                },
                FieldLayoutSpec {
                    c_name: "term_count",
                    offset_64: 16,
                    offset_32: 12,
                    rust_host_offset: offset_of!(BamQueryDesc, term_count),
                },
            ],
        },
        StructLayoutSpec {
            c_name: "BamSystemDesc",
            assert_stem: "bam_system_desc",
            size_64: 168,
            align_64: 8,
            size_32: 92,
            align_32: Align32::Fixed(4),
            rust_host_size: size_of::<BamSystemDesc>(),
            rust_host_align: align_of::<BamSystemDesc>(),
            fields: &[
                FieldLayoutSpec {
                    c_name: "struct_size",
                    offset_64: 0,
                    offset_32: 0,
                    rust_host_offset: offset_of!(BamSystemDesc, struct_size),
                },
                FieldLayoutSpec {
                    c_name: "flags",
                    offset_64: 4,
                    offset_32: 4,
                    rust_host_offset: offset_of!(BamSystemDesc, flags),
                },
                FieldLayoutSpec {
                    c_name: "name",
                    offset_64: 8,
                    offset_32: 8,
                    rust_host_offset: offset_of!(BamSystemDesc, name),
                },
                FieldLayoutSpec {
                    c_name: "queries",
                    offset_64: 16,
                    offset_32: 12,
                    rust_host_offset: offset_of!(BamSystemDesc, queries),
                },
                FieldLayoutSpec {
                    c_name: "query_count",
                    offset_64: 24,
                    offset_32: 16,
                    rust_host_offset: offset_of!(BamSystemDesc, query_count),
                },
                FieldLayoutSpec {
                    c_name: "callback",
                    offset_64: 32,
                    offset_32: 20,
                    rust_host_offset: offset_of!(BamSystemDesc, callback),
                },
                FieldLayoutSpec {
                    c_name: "user_data",
                    offset_64: 40,
                    offset_32: 24,
                    rust_host_offset: offset_of!(BamSystemDesc, user_data),
                },
                FieldLayoutSpec {
                    c_name: "schedule",
                    offset_64: 48,
                    offset_32: 28,
                    rust_host_offset: offset_of!(BamSystemDesc, schedule),
                },
                FieldLayoutSpec {
                    c_name: "reserved0",
                    offset_64: 52,
                    offset_32: 32,
                    rust_host_offset: offset_of!(BamSystemDesc, reserved0),
                },
                FieldLayoutSpec {
                    c_name: "after",
                    offset_64: 56,
                    offset_32: 36,
                    rust_host_offset: offset_of!(BamSystemDesc, after),
                },
                FieldLayoutSpec {
                    c_name: "after_count",
                    offset_64: 64,
                    offset_32: 40,
                    rust_host_offset: offset_of!(BamSystemDesc, after_count),
                },
                FieldLayoutSpec {
                    c_name: "before",
                    offset_64: 72,
                    offset_32: 44,
                    rust_host_offset: offset_of!(BamSystemDesc, before),
                },
                FieldLayoutSpec {
                    c_name: "before_count",
                    offset_64: 80,
                    offset_32: 48,
                    rust_host_offset: offset_of!(BamSystemDesc, before_count),
                },
                FieldLayoutSpec {
                    c_name: "run_if",
                    offset_64: 88,
                    offset_32: 52,
                    rust_host_offset: offset_of!(BamSystemDesc, run_if),
                },
                FieldLayoutSpec {
                    c_name: "run_if_user_data",
                    offset_64: 96,
                    offset_32: 56,
                    rust_host_offset: offset_of!(BamSystemDesc, run_if_user_data),
                },
                FieldLayoutSpec {
                    c_name: "in_sets",
                    offset_64: 104,
                    offset_32: 60,
                    rust_host_offset: offset_of!(BamSystemDesc, in_sets),
                },
                FieldLayoutSpec {
                    c_name: "in_set_count",
                    offset_64: 112,
                    offset_32: 64,
                    rust_host_offset: offset_of!(BamSystemDesc, in_set_count),
                },
                FieldLayoutSpec {
                    c_name: "after_weak",
                    offset_64: 120,
                    offset_32: 68,
                    rust_host_offset: offset_of!(BamSystemDesc, after_weak),
                },
                FieldLayoutSpec {
                    c_name: "after_weak_count",
                    offset_64: 128,
                    offset_32: 72,
                    rust_host_offset: offset_of!(BamSystemDesc, after_weak_count),
                },
                FieldLayoutSpec {
                    c_name: "before_weak",
                    offset_64: 136,
                    offset_32: 76,
                    rust_host_offset: offset_of!(BamSystemDesc, before_weak),
                },
                FieldLayoutSpec {
                    c_name: "before_weak_count",
                    offset_64: 144,
                    offset_32: 80,
                    rust_host_offset: offset_of!(BamSystemDesc, before_weak_count),
                },
                FieldLayoutSpec {
                    c_name: "ambiguous_with",
                    offset_64: 152,
                    offset_32: 84,
                    rust_host_offset: offset_of!(BamSystemDesc, ambiguous_with),
                },
                FieldLayoutSpec {
                    c_name: "ambiguous_with_count",
                    offset_64: 160,
                    offset_32: 88,
                    rust_host_offset: offset_of!(BamSystemDesc, ambiguous_with_count),
                },
            ],
        },
        StructLayoutSpec {
            c_name: "BamObserverDesc",
            assert_stem: "bam_observer_desc",
            size_64: 88,
            align_64: 8,
            size_32: 64,
            align_32: Align32::Fixed(4),
            rust_host_size: size_of::<BamObserverDesc>(),
            rust_host_align: align_of::<BamObserverDesc>(),
            fields: &[
                FieldLayoutSpec {
                    c_name: "struct_size",
                    offset_64: 0,
                    offset_32: 0,
                    rust_host_offset: offset_of!(BamObserverDesc, struct_size),
                },
                FieldLayoutSpec {
                    c_name: "flags",
                    offset_64: 4,
                    offset_32: 4,
                    rust_host_offset: offset_of!(BamObserverDesc, flags),
                },
                FieldLayoutSpec {
                    c_name: "name",
                    offset_64: 8,
                    offset_32: 8,
                    rust_host_offset: offset_of!(BamObserverDesc, name),
                },
                FieldLayoutSpec {
                    c_name: "event",
                    offset_64: 16,
                    offset_32: 12,
                    rust_host_offset: offset_of!(BamObserverDesc, event),
                },
                FieldLayoutSpec {
                    c_name: "entity_lo",
                    offset_64: 20,
                    offset_32: 16,
                    rust_host_offset: offset_of!(BamObserverDesc, entity_lo),
                },
                FieldLayoutSpec {
                    c_name: "entity_hi",
                    offset_64: 24,
                    offset_32: 20,
                    rust_host_offset: offset_of!(BamObserverDesc, entity_hi),
                },
                FieldLayoutSpec {
                    c_name: "reserved0",
                    offset_64: 28,
                    offset_32: 24,
                    rust_host_offset: offset_of!(BamObserverDesc, reserved0),
                },
                FieldLayoutSpec {
                    c_name: "callback",
                    offset_64: 32,
                    offset_32: 28,
                    rust_host_offset: offset_of!(BamObserverDesc, callback),
                },
                FieldLayoutSpec {
                    c_name: "user_data",
                    offset_64: 40,
                    offset_32: 32,
                    rust_host_offset: offset_of!(BamObserverDesc, user_data),
                },
                FieldLayoutSpec {
                    c_name: "event_kind",
                    offset_64: 48,
                    offset_32: 36,
                    rust_host_offset: offset_of!(BamObserverDesc, event_kind),
                },
                FieldLayoutSpec {
                    c_name: "reserved1",
                    offset_64: 52,
                    offset_32: 40,
                    rust_host_offset: offset_of!(BamObserverDesc, reserved1),
                },
                FieldLayoutSpec {
                    c_name: "watched_components",
                    offset_64: 56,
                    offset_32: 44,
                    rust_host_offset: offset_of!(BamObserverDesc, watched_components),
                },
                FieldLayoutSpec {
                    c_name: "watched_component_count",
                    offset_64: 64,
                    offset_32: 48,
                    rust_host_offset: offset_of!(BamObserverDesc, watched_component_count),
                },
                FieldLayoutSpec {
                    c_name: "mut_callback",
                    offset_64: 72,
                    offset_32: 52,
                    rust_host_offset: offset_of!(BamObserverDesc, mut_callback),
                },
                FieldLayoutSpec {
                    c_name: "propagate_relationship",
                    offset_64: 80,
                    offset_32: 56,
                    rust_host_offset: offset_of!(BamObserverDesc, propagate_relationship),
                },
                FieldLayoutSpec {
                    c_name: "reserved2",
                    offset_64: 84,
                    offset_32: 60,
                    rust_host_offset: offset_of!(BamObserverDesc, reserved2),
                },
            ],
        },
        StructLayoutSpec {
            c_name: "BamTriggerDesc",
            assert_stem: "bam_trigger_desc",
            size_64: 64,
            align_64: 8,
            size_32: 48,
            align_32: Align32::Fixed(4),
            rust_host_size: size_of::<BamTriggerDesc>(),
            rust_host_align: align_of::<BamTriggerDesc>(),
            fields: &[
                FieldLayoutSpec {
                    c_name: "struct_size",
                    offset_64: 0,
                    offset_32: 0,
                    rust_host_offset: offset_of!(BamTriggerDesc, struct_size),
                },
                FieldLayoutSpec {
                    c_name: "flags",
                    offset_64: 4,
                    offset_32: 4,
                    rust_host_offset: offset_of!(BamTriggerDesc, flags),
                },
                FieldLayoutSpec {
                    c_name: "event",
                    offset_64: 8,
                    offset_32: 8,
                    rust_host_offset: offset_of!(BamTriggerDesc, event),
                },
                FieldLayoutSpec {
                    c_name: "event_kind",
                    offset_64: 12,
                    offset_32: 12,
                    rust_host_offset: offset_of!(BamTriggerDesc, event_kind),
                },
                FieldLayoutSpec {
                    c_name: "entity_lo",
                    offset_64: 16,
                    offset_32: 16,
                    rust_host_offset: offset_of!(BamTriggerDesc, entity_lo),
                },
                FieldLayoutSpec {
                    c_name: "entity_hi",
                    offset_64: 20,
                    offset_32: 20,
                    rust_host_offset: offset_of!(BamTriggerDesc, entity_hi),
                },
                FieldLayoutSpec {
                    c_name: "data_ptr",
                    offset_64: 24,
                    offset_32: 24,
                    rust_host_offset: offset_of!(BamTriggerDesc, data_ptr),
                },
                FieldLayoutSpec {
                    c_name: "data_len",
                    offset_64: 32,
                    offset_32: 28,
                    rust_host_offset: offset_of!(BamTriggerDesc, data_len),
                },
                FieldLayoutSpec {
                    c_name: "watched_components",
                    offset_64: 40,
                    offset_32: 32,
                    rust_host_offset: offset_of!(BamTriggerDesc, watched_components),
                },
                FieldLayoutSpec {
                    c_name: "watched_component_count",
                    offset_64: 48,
                    offset_32: 36,
                    rust_host_offset: offset_of!(BamTriggerDesc, watched_component_count),
                },
                FieldLayoutSpec {
                    c_name: "propagate_relationship",
                    offset_64: 56,
                    offset_32: 40,
                    rust_host_offset: offset_of!(BamTriggerDesc, propagate_relationship),
                },
                FieldLayoutSpec {
                    c_name: "reserved0",
                    offset_64: 60,
                    offset_32: 44,
                    rust_host_offset: offset_of!(BamTriggerDesc, reserved0),
                },
            ],
        },
        StructLayoutSpec {
            c_name: "BamChangeTicks",
            assert_stem: "bam_change_ticks",
            size_64: 16,
            align_64: 4,
            size_32: 16,
            align_32: Align32::Fixed(4),
            rust_host_size: size_of::<BamChangeTicks>(),
            rust_host_align: align_of::<BamChangeTicks>(),
            fields: &[
                FieldLayoutSpec {
                    c_name: "added",
                    offset_64: 0,
                    offset_32: 0,
                    rust_host_offset: offset_of!(BamChangeTicks, added),
                },
                FieldLayoutSpec {
                    c_name: "changed",
                    offset_64: 4,
                    offset_32: 4,
                    rust_host_offset: offset_of!(BamChangeTicks, changed),
                },
                FieldLayoutSpec {
                    c_name: "last_run",
                    offset_64: 8,
                    offset_32: 8,
                    rust_host_offset: offset_of!(BamChangeTicks, last_run),
                },
                FieldLayoutSpec {
                    c_name: "this_run",
                    offset_64: 12,
                    offset_32: 12,
                    rust_host_offset: offset_of!(BamChangeTicks, this_run),
                },
            ],
        },
        StructLayoutSpec {
            c_name: "BamFieldDesc",
            assert_stem: "bam_field_desc",
            size_64: 64,
            align_64: 8,
            size_32: 36,
            align_32: Align32::Fixed(4),
            rust_host_size: size_of::<BamFieldDesc>(),
            rust_host_align: align_of::<BamFieldDesc>(),
            fields: &[
                FieldLayoutSpec {
                    c_name: "struct_size",
                    offset_64: 0,
                    offset_32: 0,
                    rust_host_offset: offset_of!(BamFieldDesc, struct_size),
                },
                FieldLayoutSpec {
                    c_name: "kind",
                    offset_64: 4,
                    offset_32: 4,
                    rust_host_offset: offset_of!(BamFieldDesc, kind),
                },
                FieldLayoutSpec {
                    c_name: "name",
                    offset_64: 8,
                    offset_32: 8,
                    rust_host_offset: offset_of!(BamFieldDesc, name),
                },
                FieldLayoutSpec {
                    c_name: "type_tag",
                    offset_64: 24,
                    offset_32: 16,
                    rust_host_offset: offset_of!(BamFieldDesc, type_tag),
                },
                FieldLayoutSpec {
                    c_name: "offset",
                    offset_64: 40,
                    offset_32: 24,
                    rust_host_offset: offset_of!(BamFieldDesc, offset),
                },
                FieldLayoutSpec {
                    c_name: "size",
                    offset_64: 48,
                    offset_32: 28,
                    rust_host_offset: offset_of!(BamFieldDesc, size),
                },
                FieldLayoutSpec {
                    c_name: "align",
                    offset_64: 56,
                    offset_32: 32,
                    rust_host_offset: offset_of!(BamFieldDesc, align),
                },
            ],
        },
        StructLayoutSpec {
            c_name: "BamRequiredComponent",
            assert_stem: "bam_required_component",
            size_64: 64,
            align_64: 8,
            size_32: 48,
            align_32: Align32::Fixed(4),
            rust_host_size: size_of::<BamRequiredComponent>(),
            rust_host_align: align_of::<BamRequiredComponent>(),
            fields: &[
                FieldLayoutSpec {
                    c_name: "struct_size",
                    offset_64: 0,
                    offset_32: 0,
                    rust_host_offset: offset_of!(BamRequiredComponent, struct_size),
                },
                FieldLayoutSpec {
                    c_name: "flags",
                    offset_64: 4,
                    offset_32: 4,
                    rust_host_offset: offset_of!(BamRequiredComponent, flags),
                },
                FieldLayoutSpec {
                    c_name: "component",
                    offset_64: 8,
                    offset_32: 8,
                    rust_host_offset: offset_of!(BamRequiredComponent, component),
                },
                FieldLayoutSpec {
                    c_name: "reserved0",
                    offset_64: 12,
                    offset_32: 12,
                    rust_host_offset: offset_of!(BamRequiredComponent, reserved0),
                },
                FieldLayoutSpec {
                    c_name: "uuid",
                    offset_64: 16,
                    offset_32: 16,
                    rust_host_offset: offset_of!(BamRequiredComponent, uuid),
                },
                FieldLayoutSpec {
                    c_name: "init_bytes",
                    offset_64: 32,
                    offset_32: 32,
                    rust_host_offset: offset_of!(BamRequiredComponent, init_bytes),
                },
                FieldLayoutSpec {
                    c_name: "init_size",
                    offset_64: 40,
                    offset_32: 36,
                    rust_host_offset: offset_of!(BamRequiredComponent, init_size),
                },
                FieldLayoutSpec {
                    c_name: "init_fn",
                    offset_64: 48,
                    offset_32: 40,
                    rust_host_offset: offset_of!(BamRequiredComponent, init_fn),
                },
                FieldLayoutSpec {
                    c_name: "user_data",
                    offset_64: 56,
                    offset_32: 44,
                    rust_host_offset: offset_of!(BamRequiredComponent, user_data),
                },
            ],
        },
        StructLayoutSpec {
            c_name: "BamRelationDesc",
            assert_stem: "bam_relation_desc",
            size_64: 56,
            align_64: 8,
            size_32: 44,
            align_32: Align32::Fixed(4),
            rust_host_size: size_of::<BamRelationDesc>(),
            rust_host_align: align_of::<BamRelationDesc>(),
            fields: &[
                FieldLayoutSpec {
                    c_name: "struct_size",
                    offset_64: 0,
                    offset_32: 0,
                    rust_host_offset: offset_of!(BamRelationDesc, struct_size),
                },
                FieldLayoutSpec {
                    c_name: "flags",
                    offset_64: 4,
                    offset_32: 4,
                    rust_host_offset: offset_of!(BamRelationDesc, flags),
                },
                FieldLayoutSpec {
                    c_name: "name",
                    offset_64: 8,
                    offset_32: 8,
                    rust_host_offset: offset_of!(BamRelationDesc, name),
                },
                FieldLayoutSpec {
                    c_name: "uuid",
                    offset_64: 16,
                    offset_32: 12,
                    rust_host_offset: offset_of!(BamRelationDesc, uuid),
                },
                FieldLayoutSpec {
                    c_name: "data_size",
                    offset_64: 32,
                    offset_32: 28,
                    rust_host_offset: offset_of!(BamRelationDesc, data_size),
                },
                FieldLayoutSpec {
                    c_name: "data_align",
                    offset_64: 40,
                    offset_32: 32,
                    rust_host_offset: offset_of!(BamRelationDesc, data_align),
                },
                FieldLayoutSpec {
                    c_name: "max_degree",
                    offset_64: 48,
                    offset_32: 36,
                    rust_host_offset: offset_of!(BamRelationDesc, max_degree),
                },
                FieldLayoutSpec {
                    c_name: "reserved0",
                    offset_64: 52,
                    offset_32: 40,
                    rust_host_offset: offset_of!(BamRelationDesc, reserved0),
                },
            ],
        },
        StructLayoutSpec {
            c_name: "BamLayoutInfo",
            assert_stem: "bam_layout_info",
            size_64: 64,
            align_64: 8,
            size_32: 48,
            align_32: Align32::U64,
            rust_host_size: size_of::<BamLayoutInfo>(),
            rust_host_align: align_of::<BamLayoutInfo>(),
            fields: &[
                FieldLayoutSpec {
                    c_name: "struct_size",
                    offset_64: 0,
                    offset_32: 0,
                    rust_host_offset: offset_of!(BamLayoutInfo, struct_size),
                },
                FieldLayoutSpec {
                    c_name: "flags",
                    offset_64: 4,
                    offset_32: 4,
                    rust_host_offset: offset_of!(BamLayoutInfo, flags),
                },
                FieldLayoutSpec {
                    c_name: "uuid",
                    offset_64: 8,
                    offset_32: 8,
                    rust_host_offset: offset_of!(BamLayoutInfo, uuid),
                },
                FieldLayoutSpec {
                    c_name: "schema_fingerprint",
                    offset_64: 24,
                    offset_32: 24,
                    rust_host_offset: offset_of!(BamLayoutInfo, schema_fingerprint),
                },
                FieldLayoutSpec {
                    c_name: "size",
                    offset_64: 32,
                    offset_32: 32,
                    rust_host_offset: offset_of!(BamLayoutInfo, size),
                },
                FieldLayoutSpec {
                    c_name: "align",
                    offset_64: 40,
                    offset_32: 36,
                    rust_host_offset: offset_of!(BamLayoutInfo, align),
                },
                FieldLayoutSpec {
                    c_name: "fields",
                    offset_64: 48,
                    offset_32: 40,
                    rust_host_offset: offset_of!(BamLayoutInfo, fields),
                },
                FieldLayoutSpec {
                    c_name: "field_count",
                    offset_64: 56,
                    offset_32: 44,
                    rust_host_offset: offset_of!(BamLayoutInfo, field_count),
                },
            ],
        },
        StructLayoutSpec {
            c_name: "BamScheduleSetDesc",
            assert_stem: "bam_schedule_set_desc",
            size_64: 48,
            align_64: 8,
            size_32: 36,
            align_32: Align32::Fixed(4),
            rust_host_size: size_of::<BamScheduleSetDesc>(),
            rust_host_align: align_of::<BamScheduleSetDesc>(),
            fields: &[
                FieldLayoutSpec {
                    c_name: "struct_size",
                    offset_64: 0,
                    offset_32: 0,
                    rust_host_offset: offset_of!(BamScheduleSetDesc, struct_size),
                },
                FieldLayoutSpec {
                    c_name: "flags",
                    offset_64: 4,
                    offset_32: 4,
                    rust_host_offset: offset_of!(BamScheduleSetDesc, flags),
                },
                FieldLayoutSpec {
                    c_name: "schedule",
                    offset_64: 8,
                    offset_32: 8,
                    rust_host_offset: offset_of!(BamScheduleSetDesc, schedule),
                },
                FieldLayoutSpec {
                    c_name: "before_count",
                    offset_64: 12,
                    offset_32: 12,
                    rust_host_offset: offset_of!(BamScheduleSetDesc, before_count),
                },
                FieldLayoutSpec {
                    c_name: "after_count",
                    offset_64: 16,
                    offset_32: 16,
                    rust_host_offset: offset_of!(BamScheduleSetDesc, after_count),
                },
                FieldLayoutSpec {
                    c_name: "reserved0",
                    offset_64: 20,
                    offset_32: 20,
                    rust_host_offset: offset_of!(BamScheduleSetDesc, reserved0),
                },
                FieldLayoutSpec {
                    c_name: "name",
                    offset_64: 24,
                    offset_32: 24,
                    rust_host_offset: offset_of!(BamScheduleSetDesc, name),
                },
                FieldLayoutSpec {
                    c_name: "before_sets",
                    offset_64: 32,
                    offset_32: 28,
                    rust_host_offset: offset_of!(BamScheduleSetDesc, before_sets),
                },
                FieldLayoutSpec {
                    c_name: "after_sets",
                    offset_64: 40,
                    offset_32: 32,
                    rust_host_offset: offset_of!(BamScheduleSetDesc, after_sets),
                },
            ],
        },
        StructLayoutSpec {
            c_name: "BamComponentDesc",
            assert_stem: "bam_component_desc",
            size_64: 176,
            align_64: 8,
            size_32: 104,
            align_32: Align32::U64,
            rust_host_size: size_of::<ComponentDesc>(),
            rust_host_align: align_of::<ComponentDesc>(),
            fields: &[
                FieldLayoutSpec {
                    c_name: "struct_size",
                    offset_64: 0,
                    offset_32: 0,
                    rust_host_offset: offset_of!(ComponentDesc, struct_size),
                },
                FieldLayoutSpec {
                    c_name: "flags",
                    offset_64: 4,
                    offset_32: 4,
                    rust_host_offset: offset_of!(ComponentDesc, flags),
                },
                FieldLayoutSpec {
                    c_name: "name",
                    offset_64: 8,
                    offset_32: 8,
                    rust_host_offset: offset_of!(ComponentDesc, name),
                },
                FieldLayoutSpec {
                    c_name: "uuid",
                    offset_64: 16,
                    offset_32: 12,
                    rust_host_offset: offset_of!(ComponentDesc, uuid),
                },
                FieldLayoutSpec {
                    c_name: "size",
                    offset_64: 32,
                    offset_32: 28,
                    rust_host_offset: offset_of!(ComponentDesc, size),
                },
                FieldLayoutSpec {
                    c_name: "align",
                    offset_64: 40,
                    offset_32: 32,
                    rust_host_offset: offset_of!(ComponentDesc, align),
                },
                FieldLayoutSpec {
                    c_name: "drop_fn",
                    offset_64: 48,
                    offset_32: 36,
                    rust_host_offset: offset_of!(ComponentDesc, drop_fn),
                },
                FieldLayoutSpec {
                    c_name: "vtable_ptr",
                    offset_64: 56,
                    offset_32: 40,
                    rust_host_offset: offset_of!(ComponentDesc, vtable_ptr),
                },
                FieldLayoutSpec {
                    c_name: "on_add",
                    offset_64: 64,
                    offset_32: 44,
                    rust_host_offset: offset_of!(ComponentDesc, on_add),
                },
                FieldLayoutSpec {
                    c_name: "on_remove",
                    offset_64: 72,
                    offset_32: 48,
                    rust_host_offset: offset_of!(ComponentDesc, on_remove),
                },
                FieldLayoutSpec {
                    c_name: "hook_user_data",
                    offset_64: 80,
                    offset_32: 52,
                    rust_host_offset: offset_of!(ComponentDesc, hook_user_data),
                },
                FieldLayoutSpec {
                    c_name: "on_insert",
                    offset_64: 88,
                    offset_32: 56,
                    rust_host_offset: offset_of!(ComponentDesc, on_insert),
                },
                FieldLayoutSpec {
                    c_name: "on_discard",
                    offset_64: 96,
                    offset_32: 60,
                    rust_host_offset: offset_of!(ComponentDesc, on_discard),
                },
                FieldLayoutSpec {
                    c_name: "on_despawn",
                    offset_64: 104,
                    offset_32: 64,
                    rust_host_offset: offset_of!(ComponentDesc, on_despawn),
                },
                FieldLayoutSpec {
                    c_name: "clone_fn",
                    offset_64: 112,
                    offset_32: 68,
                    rust_host_offset: offset_of!(ComponentDesc, clone_fn),
                },
                FieldLayoutSpec {
                    c_name: "required_components",
                    offset_64: 120,
                    offset_32: 72,
                    rust_host_offset: offset_of!(ComponentDesc, required_components),
                },
                FieldLayoutSpec {
                    c_name: "required_component_count",
                    offset_64: 128,
                    offset_32: 76,
                    rust_host_offset: offset_of!(ComponentDesc, required_component_count),
                },
                FieldLayoutSpec {
                    c_name: "schema_fingerprint",
                    offset_64: 136,
                    offset_32: 80,
                    rust_host_offset: offset_of!(ComponentDesc, schema_fingerprint),
                },
                FieldLayoutSpec {
                    c_name: "fields",
                    offset_64: 144,
                    offset_32: 88,
                    rust_host_offset: offset_of!(ComponentDesc, fields),
                },
                FieldLayoutSpec {
                    c_name: "field_count",
                    offset_64: 152,
                    offset_32: 92,
                    rust_host_offset: offset_of!(ComponentDesc, field_count),
                },
                FieldLayoutSpec {
                    c_name: "required_init_descs",
                    offset_64: 160,
                    offset_32: 96,
                    rust_host_offset: offset_of!(ComponentDesc, required_init_descs),
                },
                FieldLayoutSpec {
                    c_name: "required_init_desc_count",
                    offset_64: 168,
                    offset_32: 100,
                    rust_host_offset: offset_of!(ComponentDesc, required_init_desc_count),
                },
            ],
        },
        StructLayoutSpec {
            c_name: "BamColumnInfo",
            assert_stem: "bam_column_info",
            size_64: 48,
            align_64: 8,
            size_32: 40,
            align_32: Align32::Fixed(4),
            rust_host_size: size_of::<BamColumnInfo>(),
            rust_host_align: align_of::<BamColumnInfo>(),
            fields: &[
                FieldLayoutSpec {
                    c_name: "component",
                    offset_64: 0,
                    offset_32: 0,
                    rust_host_offset: offset_of!(BamColumnInfo, component),
                },
                FieldLayoutSpec {
                    c_name: "mode",
                    offset_64: 4,
                    offset_32: 4,
                    rust_host_offset: offset_of!(BamColumnInfo, mode),
                },
                FieldLayoutSpec {
                    c_name: "uuid",
                    offset_64: 8,
                    offset_32: 8,
                    rust_host_offset: offset_of!(BamColumnInfo, uuid),
                },
                FieldLayoutSpec {
                    c_name: "size",
                    offset_64: 24,
                    offset_32: 24,
                    rust_host_offset: offset_of!(BamColumnInfo, size),
                },
                FieldLayoutSpec {
                    c_name: "align",
                    offset_64: 32,
                    offset_32: 28,
                    rust_host_offset: offset_of!(BamColumnInfo, align),
                },
                FieldLayoutSpec {
                    c_name: "storage_flags",
                    offset_64: 40,
                    offset_32: 32,
                    rust_host_offset: offset_of!(BamColumnInfo, storage_flags),
                },
                FieldLayoutSpec {
                    c_name: "reserved0",
                    offset_64: 44,
                    offset_32: 36,
                    rust_host_offset: offset_of!(BamColumnInfo, reserved0),
                },
            ],
        },
        StructLayoutSpec {
            c_name: "BamVTableHeader",
            assert_stem: "bam_vtable_header",
            size_64: 40,
            align_64: 8,
            size_32: 32,
            align_32: Align32::Fixed(4),
            rust_host_size: size_of::<VTableHeader>(),
            rust_host_align: align_of::<VTableHeader>(),
            fields: &[
                FieldLayoutSpec {
                    c_name: "struct_size",
                    offset_64: 0,
                    offset_32: 0,
                    rust_host_offset: offset_of!(VTableHeader, struct_size),
                },
                FieldLayoutSpec {
                    c_name: "abi_version",
                    offset_64: 4,
                    offset_32: 4,
                    rust_host_offset: offset_of!(VTableHeader, abi_version),
                },
                FieldLayoutSpec {
                    c_name: "uuid",
                    offset_64: 8,
                    offset_32: 8,
                    rust_host_offset: offset_of!(VTableHeader, uuid),
                },
                FieldLayoutSpec {
                    c_name: "size",
                    offset_64: 24,
                    offset_32: 24,
                    rust_host_offset: offset_of!(VTableHeader, size),
                },
                FieldLayoutSpec {
                    c_name: "align",
                    offset_64: 32,
                    offset_32: 28,
                    rust_host_offset: offset_of!(VTableHeader, align),
                },
            ],
        },
        StructLayoutSpec {
            c_name: "BamMappedStringVTable",
            assert_stem: "bam_mapped_string_vtable",
            size_64: 56,
            align_64: 8,
            size_32: 40,
            align_32: Align32::Fixed(4),
            rust_host_size: size_of::<MappedStringVTable>(),
            rust_host_align: align_of::<MappedStringVTable>(),
            fields: &[
                FieldLayoutSpec {
                    c_name: "header",
                    offset_64: 0,
                    offset_32: 0,
                    rust_host_offset: offset_of!(MappedStringVTable, header),
                },
                FieldLayoutSpec {
                    c_name: "get",
                    offset_64: 40,
                    offset_32: 32,
                    rust_host_offset: offset_of!(MappedStringVTable, get),
                },
                FieldLayoutSpec {
                    c_name: "set",
                    offset_64: 48,
                    offset_32: 36,
                    rust_host_offset: offset_of!(MappedStringVTable, set),
                },
            ],
        },
        StructLayoutSpec {
            c_name: "BamHostApi",
            assert_stem: "bam_host_api",
            size_64: 552,
            align_64: 8,
            size_32: 280,
            align_32: Align32::Fixed(4),
            rust_host_size: size_of::<BamHostApi>(),
            rust_host_align: align_of::<BamHostApi>(),
            fields: &[
                FieldLayoutSpec {
                    c_name: "struct_size",
                    offset_64: 0,
                    offset_32: 0,
                    rust_host_offset: offset_of!(BamHostApi, struct_size),
                },
                FieldLayoutSpec {
                    c_name: "abi_version",
                    offset_64: 4,
                    offset_32: 4,
                    rust_host_offset: offset_of!(BamHostApi, abi_version),
                },
                FieldLayoutSpec {
                    c_name: "get_proc_address",
                    offset_64: 8,
                    offset_32: 8,
                    rust_host_offset: offset_of!(BamHostApi, get_proc_address),
                },
                FieldLayoutSpec {
                    c_name: "last_error",
                    offset_64: 16,
                    offset_32: 12,
                    rust_host_offset: offset_of!(BamHostApi, last_error),
                },
                FieldLayoutSpec {
                    c_name: "load_host_ctx",
                    offset_64: 24,
                    offset_32: 16,
                    rust_host_offset: offset_of!(BamHostApi, load_host_ctx),
                },
                FieldLayoutSpec {
                    c_name: "register_component",
                    offset_64: 32,
                    offset_32: 20,
                    rust_host_offset: offset_of!(BamHostApi, register_component),
                },
                FieldLayoutSpec {
                    c_name: "resolve_component",
                    offset_64: 40,
                    offset_32: 24,
                    rust_host_offset: offset_of!(BamHostApi, resolve_component),
                },
                FieldLayoutSpec {
                    c_name: "register_system",
                    offset_64: 48,
                    offset_32: 28,
                    rust_host_offset: offset_of!(BamHostApi, register_system),
                },
                FieldLayoutSpec {
                    c_name: "sys_host_ctx",
                    offset_64: 56,
                    offset_32: 32,
                    rust_host_offset: offset_of!(BamHostApi, sys_host_ctx),
                },
                FieldLayoutSpec {
                    c_name: "sys_query",
                    offset_64: 64,
                    offset_32: 36,
                    rust_host_offset: offset_of!(BamHostApi, sys_query),
                },
                FieldLayoutSpec {
                    c_name: "sys_commands",
                    offset_64: 72,
                    offset_32: 40,
                    rust_host_offset: offset_of!(BamHostApi, sys_commands),
                },
                FieldLayoutSpec {
                    c_name: "iter_next_chunk",
                    offset_64: 80,
                    offset_32: 44,
                    rust_host_offset: offset_of!(BamHostApi, iter_next_chunk),
                },
                FieldLayoutSpec {
                    c_name: "iter_reset",
                    offset_64: 88,
                    offset_32: 48,
                    rust_host_offset: offset_of!(BamHostApi, iter_reset),
                },
                FieldLayoutSpec {
                    c_name: "iter_entities",
                    offset_64: 96,
                    offset_32: 52,
                    rust_host_offset: offset_of!(BamHostApi, iter_entities),
                },
                FieldLayoutSpec {
                    c_name: "iter_column_readonly",
                    offset_64: 104,
                    offset_32: 56,
                    rust_host_offset: offset_of!(BamHostApi, iter_column_readonly),
                },
                FieldLayoutSpec {
                    c_name: "iter_column_mutable",
                    offset_64: 112,
                    offset_32: 60,
                    rust_host_offset: offset_of!(BamHostApi, iter_column_mutable),
                },
                FieldLayoutSpec {
                    c_name: "iter_column_mutable_untracked",
                    offset_64: 120,
                    offset_32: 64,
                    rust_host_offset: offset_of!(BamHostApi, iter_column_mutable_untracked),
                },
                FieldLayoutSpec {
                    c_name: "iter_mark_changed_range",
                    offset_64: 128,
                    offset_32: 68,
                    rust_host_offset: offset_of!(BamHostApi, iter_mark_changed_range),
                },
                FieldLayoutSpec {
                    c_name: "iter_column_rows_readonly",
                    offset_64: 136,
                    offset_32: 72,
                    rust_host_offset: offset_of!(BamHostApi, iter_column_rows_readonly),
                },
                FieldLayoutSpec {
                    c_name: "iter_column_rows_mutable",
                    offset_64: 144,
                    offset_32: 76,
                    rust_host_offset: offset_of!(BamHostApi, iter_column_rows_mutable),
                },
                FieldLayoutSpec {
                    c_name: "iter_column_info",
                    offset_64: 152,
                    offset_32: 80,
                    rust_host_offset: offset_of!(BamHostApi, iter_column_info),
                },
                FieldLayoutSpec {
                    c_name: "res_ptr",
                    offset_64: 160,
                    offset_32: 84,
                    rust_host_offset: offset_of!(BamHostApi, res_ptr),
                },
                FieldLayoutSpec {
                    c_name: "res_mut_ptr",
                    offset_64: 168,
                    offset_32: 88,
                    rust_host_offset: offset_of!(BamHostApi, res_mut_ptr),
                },
                FieldLayoutSpec {
                    c_name: "cmd_spawn_empty",
                    offset_64: 176,
                    offset_32: 92,
                    rust_host_offset: offset_of!(BamHostApi, cmd_spawn_empty),
                },
                FieldLayoutSpec {
                    c_name: "cmd_spawn",
                    offset_64: 184,
                    offset_32: 96,
                    rust_host_offset: offset_of!(BamHostApi, cmd_spawn),
                },
                FieldLayoutSpec {
                    c_name: "cmd_despawn",
                    offset_64: 192,
                    offset_32: 100,
                    rust_host_offset: offset_of!(BamHostApi, cmd_despawn),
                },
                FieldLayoutSpec {
                    c_name: "cmd_insert",
                    offset_64: 200,
                    offset_32: 104,
                    rust_host_offset: offset_of!(BamHostApi, cmd_insert),
                },
                FieldLayoutSpec {
                    c_name: "cmd_insert_default",
                    offset_64: 208,
                    offset_32: 108,
                    rust_host_offset: offset_of!(BamHostApi, cmd_insert_default),
                },
                FieldLayoutSpec {
                    c_name: "cmd_remove",
                    offset_64: 216,
                    offset_32: 112,
                    rust_host_offset: offset_of!(BamHostApi, cmd_remove),
                },
                FieldLayoutSpec {
                    c_name: "par_for_each_chunk",
                    offset_64: 224,
                    offset_32: 116,
                    rust_host_offset: offset_of!(BamHostApi, par_for_each_chunk),
                },
                FieldLayoutSpec {
                    c_name: "sys_entity_get",
                    offset_64: 232,
                    offset_32: 120,
                    rust_host_offset: offset_of!(BamHostApi, sys_entity_get),
                },
                FieldLayoutSpec {
                    c_name: "sys_entity_get_mut",
                    offset_64: 240,
                    offset_32: 124,
                    rust_host_offset: offset_of!(BamHostApi, sys_entity_get_mut),
                },
                FieldLayoutSpec {
                    c_name: "msg_read_batch",
                    offset_64: 248,
                    offset_32: 128,
                    rust_host_offset: offset_of!(BamHostApi, msg_read_batch),
                },
                FieldLayoutSpec {
                    c_name: "msg_send",
                    offset_64: 256,
                    offset_32: 132,
                    rust_host_offset: offset_of!(BamHostApi, msg_send),
                },
                FieldLayoutSpec {
                    c_name: "msg_write_batch",
                    offset_64: 264,
                    offset_32: 136,
                    rust_host_offset: offset_of!(BamHostApi, msg_write_batch),
                },
                FieldLayoutSpec {
                    c_name: "res_ticks",
                    offset_64: 272,
                    offset_32: 140,
                    rust_host_offset: offset_of!(BamHostApi, res_ticks),
                },
                FieldLayoutSpec {
                    c_name: "sys_res_ptr",
                    offset_64: 280,
                    offset_32: 144,
                    rust_host_offset: offset_of!(BamHostApi, sys_res_ptr),
                },
                FieldLayoutSpec {
                    c_name: "sys_res_mut_ptr",
                    offset_64: 288,
                    offset_32: 148,
                    rust_host_offset: offset_of!(BamHostApi, sys_res_mut_ptr),
                },
                FieldLayoutSpec {
                    c_name: "register_observer",
                    offset_64: 296,
                    offset_32: 152,
                    rust_host_offset: offset_of!(BamHostApi, register_observer),
                },
                FieldLayoutSpec {
                    c_name: "cmd_trigger",
                    offset_64: 304,
                    offset_32: 156,
                    rust_host_offset: offset_of!(BamHostApi, cmd_trigger),
                },
                FieldLayoutSpec {
                    c_name: "cmd_spawn_persistent",
                    offset_64: 312,
                    offset_32: 160,
                    rust_host_offset: offset_of!(BamHostApi, cmd_spawn_persistent),
                },
                FieldLayoutSpec {
                    c_name: "cmd_spawn_batch",
                    offset_64: 320,
                    offset_32: 164,
                    rust_host_offset: offset_of!(BamHostApi, cmd_spawn_batch),
                },
                FieldLayoutSpec {
                    c_name: "cmd_despawn_all",
                    offset_64: 328,
                    offset_32: 168,
                    rust_host_offset: offset_of!(BamHostApi, cmd_despawn_all),
                },
                FieldLayoutSpec {
                    c_name: "cmd_insert_if_new",
                    offset_64: 336,
                    offset_32: 172,
                    rust_host_offset: offset_of!(BamHostApi, cmd_insert_if_new),
                },
                FieldLayoutSpec {
                    c_name: "cmd_trigger_ex",
                    offset_64: 344,
                    offset_32: 176,
                    rust_host_offset: offset_of!(BamHostApi, cmd_trigger_ex),
                },
                FieldLayoutSpec {
                    c_name: "cmd_observe",
                    offset_64: 352,
                    offset_32: 180,
                    rust_host_offset: offset_of!(BamHostApi, cmd_observe),
                },
                FieldLayoutSpec {
                    c_name: "iter_read_removed",
                    offset_64: 360,
                    offset_32: 184,
                    rust_host_offset: offset_of!(BamHostApi, iter_read_removed),
                },
                FieldLayoutSpec {
                    c_name: "sys_entity_children",
                    offset_64: 368,
                    offset_32: 188,
                    rust_host_offset: offset_of!(BamHostApi, sys_entity_children),
                },
                FieldLayoutSpec {
                    c_name: "asset_load",
                    offset_64: 376,
                    offset_32: 192,
                    rust_host_offset: offset_of!(BamHostApi, asset_load),
                },
                FieldLayoutSpec {
                    c_name: "asset_get_load_state",
                    offset_64: 384,
                    offset_32: 196,
                    rust_host_offset: offset_of!(BamHostApi, asset_get_load_state),
                },
                FieldLayoutSpec {
                    c_name: "asset_release",
                    offset_64: 392,
                    offset_32: 200,
                    rust_host_offset: offset_of!(BamHostApi, asset_release),
                },
                FieldLayoutSpec {
                    c_name: "cmd_spawn_scene",
                    offset_64: 400,
                    offset_32: 204,
                    rust_host_offset: offset_of!(BamHostApi, cmd_spawn_scene),
                },
                FieldLayoutSpec {
                    c_name: "log_diagnostic",
                    offset_64: 408,
                    offset_32: 208,
                    rust_host_offset: offset_of!(BamHostApi, log_diagnostic),
                },
                FieldLayoutSpec {
                    c_name: "report_guest_fault",
                    offset_64: 416,
                    offset_32: 212,
                    rust_host_offset: offset_of!(BamHostApi, report_guest_fault),
                },
                FieldLayoutSpec {
                    c_name: "resolve_component_by_uuid",
                    offset_64: 424,
                    offset_32: 216,
                    rust_host_offset: offset_of!(BamHostApi, resolve_component_by_uuid),
                },
                FieldLayoutSpec {
                    c_name: "component_layout_info",
                    offset_64: 432,
                    offset_32: 220,
                    rust_host_offset: offset_of!(BamHostApi, component_layout_info),
                },
                FieldLayoutSpec {
                    c_name: "register_relation",
                    offset_64: 440,
                    offset_32: 224,
                    rust_host_offset: offset_of!(BamHostApi, register_relation),
                },
                FieldLayoutSpec {
                    c_name: "intern_symbol",
                    offset_64: 448,
                    offset_32: 228,
                    rust_host_offset: offset_of!(BamHostApi, intern_symbol),
                },
                FieldLayoutSpec {
                    c_name: "bind_named_object",
                    offset_64: 456,
                    offset_32: 232,
                    rust_host_offset: offset_of!(BamHostApi, bind_named_object),
                },
                FieldLayoutSpec {
                    c_name: "iter_count",
                    offset_64: 464,
                    offset_32: 236,
                    rust_host_offset: offset_of!(BamHostApi, iter_count),
                },
                FieldLayoutSpec {
                    c_name: "iter_up_cascade",
                    offset_64: 472,
                    offset_32: 240,
                    rust_host_offset: offset_of!(BamHostApi, iter_up_cascade),
                },
                FieldLayoutSpec {
                    c_name: "par_for_each_chunk_readonly",
                    offset_64: 480,
                    offset_32: 244,
                    rust_host_offset: offset_of!(BamHostApi, par_for_each_chunk_readonly),
                },
                FieldLayoutSpec {
                    c_name: "register_system_handle",
                    offset_64: 488,
                    offset_32: 248,
                    rust_host_offset: offset_of!(BamHostApi, register_system_handle),
                },
                FieldLayoutSpec {
                    c_name: "configure_schedule_set",
                    offset_64: 496,
                    offset_32: 252,
                    rust_host_offset: offset_of!(BamHostApi, configure_schedule_set),
                },
                FieldLayoutSpec {
                    c_name: "mount_system_in_schedule",
                    offset_64: 504,
                    offset_32: 256,
                    rust_host_offset: offset_of!(BamHostApi, mount_system_in_schedule),
                },
                FieldLayoutSpec {
                    c_name: "cmd_checkpoint",
                    offset_64: 512,
                    offset_32: 260,
                    rust_host_offset: offset_of!(BamHostApi, cmd_checkpoint),
                },
                FieldLayoutSpec {
                    c_name: "cmd_rollback",
                    offset_64: 520,
                    offset_32: 264,
                    rust_host_offset: offset_of!(BamHostApi, cmd_rollback),
                },
                FieldLayoutSpec {
                    c_name: "cmd_insert_relation",
                    offset_64: 528,
                    offset_32: 268,
                    rust_host_offset: offset_of!(BamHostApi, cmd_insert_relation),
                },
                FieldLayoutSpec {
                    c_name: "cmd_remove_relation",
                    offset_64: 536,
                    offset_32: 272,
                    rust_host_offset: offset_of!(BamHostApi, cmd_remove_relation),
                },
                FieldLayoutSpec {
                    c_name: "migrate_component_column",
                    offset_64: 544,
                    offset_32: 276,
                    rust_host_offset: offset_of!(BamHostApi, migrate_component_column),
                },
            ],
        },
    ]
}

fn verify_host_rust_layouts(specs: &[StructLayoutSpec]) -> Result<(), String> {
    let ptr_bits = usize::BITS as usize;
    if size_of::<FfiBool>() != 1
        || size_of::<BamComponent>() != 4
        || size_of::<BamSystem>() != 4
        || size_of::<BamSystemSet>() != 4
        || size_of::<BamSymbol>() != 4
        || size_of::<BamFieldKind>() != 4
        || size_of::<FfiEntity>() != 8
        || size_of::<FfiChildOf>() != 8
        || size_of::<FfiResult>() != 4
        || size_of::<AccessMode>() != 4
        || size_of::<ScheduleId>() != 4
        || size_of::<BamEventKind>() != 4
        || size_of::<FfiAssetLoadState>() != 4
        || size_of::<FfiOption<u32>>() != 8
        || offset_of!(FfiOption<u32>, is_some) != 0
        || offset_of!(FfiOption<u32>, value) != 4
    {
        return Err("primitive newtype layout mismatch in bevy_abi_mod_sys".to_owned());
    }

    for spec in specs {
        let (expected_sz, expected_al) = match ptr_bits {
            64 => (spec.size_64, spec.align_64),
            32 => (
                spec.size_32,
                match spec.align_32 {
                    Align32::Fixed(a) => a,
                    Align32::U64 => align_of::<u64>(),
                },
            ),
            other => return Err(format!("unsupported host pointer width {other} bits")),
        };

        if spec.rust_host_size != expected_sz {
            return Err(format!(
                "size_of::<{}>() is {} on {ptr_bits}-bit host, expected {expected_sz}",
                spec.c_name, spec.rust_host_size
            ));
        }
        if spec.rust_host_align != expected_al {
            return Err(format!(
                "align_of::<{}>() is {} on {ptr_bits}-bit host, expected {expected_al}",
                spec.c_name, spec.rust_host_align
            ));
        }

        for field in spec.fields {
            let expected_off = if ptr_bits == 64 {
                field.offset_64
            } else {
                field.offset_32
            };
            if field.rust_host_offset != expected_off {
                return Err(format!(
                    "offset_of!({}, {}) is {} on {ptr_bits}-bit host, expected {expected_off}",
                    spec.c_name, field.c_name, field.rust_host_offset
                ));
            }
        }
    }

    Ok(())
}

/// Renders the complete deterministic ISO C99 header from `bevy_abi_mod_sys` constants and layouts.
pub fn render_c99_header() -> Result<String, String> {
    let specs = wire_struct_specs();
    verify_host_rust_layouts(&specs)?;

    let host_api_spec = specs
        .iter()
        .find(|s| s.c_name == "BamHostApi")
        .ok_or_else(|| "missing BamHostApi layout spec".to_owned())?;
    let host_api_slot_count = host_api_spec
        .fields
        .iter()
        .filter(|f| f.c_name != "struct_size" && f.c_name != "abi_version")
        .count();

    let mut out = String::with_capacity(48 * 1024);

    out.push_str(
        "/*\n\
         * AUTO-GENERATED BY `cargo xtask header` FROM `crates/bevy_abi_mod_sys`.\n\
         * DO NOT EDIT MANUALLY — run `cargo xtask header` to regenerate.\n\
         *\n\
         * `bevy_abi_mod` C99 ABI Wire Header (`0.3.0`)\n\
         * Zero-serialization, asymmetric `extern \"C\"` modding interface for Bevy ECS.\n\
         *\n\
         * Threading & Concurrency Model:\n\
         * - `bevy_abi_mod_manifest`, `bevy_abi_mod_init`, and `bevy_abi_mod_shutdown` run\n\
         *   sequentially on the thread driving the target Bevy `World`.\n\
         * - Registered `BamSystemFn` and `BamRunIfFn` callbacks execute on Bevy's multi-threaded\n\
         *   `ComputeTaskPool` worker threads unless `BAM_SYSTEM_FLAG_MAIN_THREAD` is set on\n\
         *   `BamSystemDesc.flags`. Any mutable C/C++/Zig static state shared across systems\n\
         *   that can run in parallel MUST be synchronized or stored per-instance via `user_data`.\n\
         * - `BamSystemCtx*`, `BamIter*`, and `BamCommands*` handles are valid strictly for the\n\
         *   duration of the callback invocation and MUST NOT be stashed in globals.\n\
         *\n\
         * Native Trust & Nounwind Boundary:\n\
         * - Unwinding (C++ exceptions, `longjmp`, or Rust panics) across any `extern \"C\"`\n\
         *   callback boundary is undefined behavior.\n\
         * - Native dynamic libraries share the host process address space; validate `BamResult`\n\
         *   return codes and query `api->last_error(ctx, buf, cap)` for diagnostics.\n\
         */\n\n\
         #ifndef BEVY_ABI_MOD_H\n\
         #define BEVY_ABI_MOD_H\n\n\
         #include <stddef.h>\n\
         #include <stdint.h>\n\
         #include <string.h>\n\n\
         #ifdef __cplusplus\n\
         extern \"C\" {\n\
         #endif\n\n",
    );

    // 1. Compiler portability macros (pure ISO C99 `-pedantic -Werror`, C11, C++11+, & MSVC `/W4 /WX` clean)
    out.push_str(
        "/* ==========================================================================\n\
         * 1. Compiler Portability, Export & Static Layout Assertion Macros\n\
         * ========================================================================== */\n\n\
         #if defined(_WIN32) || defined(__CYGWIN__)\n\
         #  ifdef __cplusplus\n\
         #    define BAM_EXPORT extern \"C\" __declspec(dllexport)\n\
         #  else\n\
         #    define BAM_EXPORT __declspec(dllexport)\n\
         #  endif\n\
         #elif defined(__GNUC__) || defined(__clang__)\n\
         #  ifdef __cplusplus\n\
         #    define BAM_EXPORT extern \"C\" __attribute__((visibility(\"default\")))\n\
         #  else\n\
         #    define BAM_EXPORT __attribute__((visibility(\"default\")))\n\
         #  endif\n\
         #else\n\
         #  ifdef __cplusplus\n\
         #    define BAM_EXPORT extern \"C\"\n\
         #  else\n\
         #    define BAM_EXPORT\n\
         #  endif\n\
         #endif\n\n\
         #if defined(__GNUC__) || defined(__clang__)\n\
         #  define BAM_UNUSED __attribute__((unused))\n\
         #else\n\
         #  define BAM_UNUSED\n\
         #endif\n\n\
         #if defined(__cplusplus) && ((__cplusplus >= 201103L) || (defined(_MSVC_LANG) && _MSVC_LANG >= 201103L))\n\
         #  define BAM_ALIGNOF(type) alignof(type)\n\
         #elif defined(__STDC_VERSION__) && (__STDC_VERSION__ >= 201112L)\n\
         #  define BAM_ALIGNOF(type) _Alignof(type)\n\
         #elif defined(__GNUC__) || defined(__clang__)\n\
         #  define BAM_ALIGNOF(type) (__extension__ __alignof__(type))\n\
         #elif defined(_MSC_VER)\n\
         #  define BAM_ALIGNOF(type) __alignof(type)\n\
         #else\n\
         #  define BAM_ALIGNOF(type) offsetof(struct { char _bam_pad; type _bam_val; }, _bam_val)\n\
         #endif\n\n\
         #if defined(__cplusplus) && ((__cplusplus >= 201103L) || (defined(_MSVC_LANG) && _MSVC_LANG >= 201103L))\n\
         #  define BAM_STATIC_ASSERT(cond, name) static_assert((cond), #name)\n\
         #elif defined(__STDC_VERSION__) && (__STDC_VERSION__ >= 201112L)\n\
         #  define BAM_STATIC_ASSERT(cond, name) _Static_assert((cond), #name)\n\
         #elif defined(__GNUC__) || defined(__clang__)\n\
         #  define BAM_STATIC_ASSERT(cond, name) __extension__ _Static_assert((cond), #name)\n\
         #else\n\
         #  define BAM_STATIC_ASSERT(cond, name) typedef char bam_static_assert_##name[(cond) ? 1 : -1]\n\
         #endif\n\n",
    );

    // 2. Version & limits emitted from `bevy_abi_mod_sys`
    let _ = writeln!(
        out,
        "/* ==========================================================================\n\
         * 2. ABI SemVer Version, Architectural Limits & Domain Constants\n\
         * ========================================================================== */\n\n\
         #define BAM_ABI_VERSION_MAJOR {ABI_VERSION_MAJOR}u\n\
         #define BAM_ABI_VERSION_MINOR {ABI_VERSION_MINOR}u\n\
         #define BAM_ABI_VERSION_PATCH {ABI_VERSION_PATCH}u\n\
         #define BAM_PACK_ABI_VERSION(major, minor, patch) \\\n\
             ((((uint32_t)(major) & 0xFFu) << 24) | (((uint32_t)(minor) & 0xFFu) << 16) | ((uint32_t)(patch) & 0xFFFFu))\n\
         #define BAM_ABI_VERSION 0x{ABI_VERSION:08X}u\n\n\
         #define BAM_MAX_TERMS_PER_QUERY    {MAX_TERMS_PER_QUERY}u\n\
         #define BAM_MAX_QUERIES_PER_SYSTEM {MAX_QUERIES_PER_SYSTEM}u\n\
         #define BAM_HOST_API_SLOT_COUNT    {host_api_slot_count}u\n\n\
         #define BAM_SIPHASH_KEY_0 0x{SIPHASH_KEY_0:016x}ull\n\
         #define BAM_SIPHASH_KEY_1 0x{SIPHASH_KEY_1:016x}ull\n\
         #define BAM_MAX_CHANGE_AGE 0x{BAM_MAX_CHANGE_AGE:08X}u\n\
         #define BAM_CHILD_OF_NAME \"{BAM_CHILD_OF_NAME}\"\n\
         #define BAM_CHILDREN_NAME \"{BAM_CHILDREN_NAME}\"\n"
    );

    // 3. Primitives, Result taxonomy, AccessMode, ScheduleId, FieldKind, LogLevel, Flags
    let _ = writeln!(
        out,
        "/* ==========================================================================\n\
         * 3. Primitive Newtypes, Handles, Status Codes, Modes & Flags\n\
         * ========================================================================== */\n\n\
         typedef uint8_t  BamBool;\n\
         #define BAM_FALSE ((BamBool){}u)\n\
         #define BAM_TRUE  ((BamBool){}u)\n\n\
         typedef uint32_t BamComponent;\n\
         #define BAM_COMPONENT_INVALID ((BamComponent)0x{:08X}u)\n\n\
         typedef uint32_t BamSystem;\n\
         #define BAM_SYSTEM_INVALID ((BamSystem)0x{:08X}u)\n\n\
         typedef uint32_t BamSystemSet;\n\
         #define BAM_SYSTEM_SET_INVALID ((BamSystemSet)0x{:08X}u)\n\n\
         typedef uint32_t BamSymbol;\n\
         #define BAM_SYMBOL_INVALID ((BamSymbol)0x{:08X}u)\n\n\
         typedef uint64_t BamEntity;\n\
         #define BAM_ENTITY_NULL ((BamEntity){}ull)\n\n\
         typedef uint32_t BamResult;\n\
         #define BAM_RESULT_OK                ((BamResult){}u)\n\
         #define BAM_RESULT_INVALID_COMPONENT ((BamResult){}u)\n\
         #define BAM_RESULT_ACCESS_CONFLICT   ((BamResult){}u)\n\
         #define BAM_RESULT_PANIC_CAUGHT      ((BamResult){}u)\n\
         #define BAM_RESULT_NULL_POINTER      ((BamResult){}u)\n\
         #define BAM_RESULT_INVALID_UTF8      ((BamResult){}u)\n\
         #define BAM_RESULT_PERMISSION_DENIED ((BamResult){}u)\n\
         #define BAM_RESULT_INVALID_VERSION   ((BamResult){}u)\n\
         #define BAM_RESULT_INVALID_ARGUMENT  ((BamResult){}u)\n\
         #define BAM_RESULT_OUT_OF_RANGE      ((BamResult){}u)\n\
         #define BAM_RESULT_NOT_FOUND         ((BamResult){}u)\n\
         #define BAM_RESULT_INVALID_STATE     ((BamResult){}u)\n\
         #define BAM_RESULT_INVALID_ENTITY    ((BamResult){}u)\n\
         #define BAM_RESULT_CAPACITY_EXCEEDED ((BamResult){}u)\n\
         #define BAM_RESULT_UNSUPPORTED       ((BamResult){}u)\n\
         #define BAM_RESULT_TYPE_MISMATCH     ((BamResult){}u)\n\
         #define BAM_RESULT_INTERNAL_ERROR    ((BamResult){}u)\n\n\
         typedef uint32_t BamAccessMode;\n\
         #define BAM_ACCESS_READ           ((BamAccessMode){}u)\n\
         #define BAM_ACCESS_WRITE          ((BamAccessMode){}u)\n\
         #define BAM_ACCESS_FILTER_ONLY    ((BamAccessMode){}u)\n\
         #define BAM_ACCESS_WITHOUT        ((BamAccessMode){}u)\n\
         #define BAM_ACCESS_OPTIONAL_READ  ((BamAccessMode){}u)\n\
         #define BAM_ACCESS_OPTIONAL_WRITE ((BamAccessMode){}u)\n\
         #define BAM_ACCESS_CHANGED        ((BamAccessMode){}u)\n\
         #define BAM_ACCESS_ADDED          ((BamAccessMode){}u)\n\
         #define BAM_ACCESS_RESOURCE_READ  ((BamAccessMode){}u)\n\
         #define BAM_ACCESS_RESOURCE_WRITE ((BamAccessMode){}u)\n\
         #define BAM_ACCESS_MESSAGE_READ   ((BamAccessMode){}u)\n\
         #define BAM_ACCESS_MESSAGE_WRITE  ((BamAccessMode){}u)\n\
         #define BAM_ACCESS_ALLOW          ((BamAccessMode){}u)\n\
         #define BAM_ACCESS_HAS            ((BamAccessMode){}u)\n\
         #define BAM_ACCESS_REMOVED_READ   ((BamAccessMode){}u)\n\n\
         typedef uint32_t BamEventKind;\n\
         #define BAM_EVENT_KIND_CUSTOM  ((BamEventKind){}u)\n\
         #define BAM_EVENT_KIND_ADD     ((BamEventKind){}u)\n\
         #define BAM_EVENT_KIND_INSERT  ((BamEventKind){}u)\n\
         #define BAM_EVENT_KIND_DISCARD ((BamEventKind){}u)\n\
         #define BAM_EVENT_KIND_REMOVE  ((BamEventKind){}u)\n\
         #define BAM_EVENT_KIND_DESPAWN ((BamEventKind){}u)\n\
         #define BAM_EVENT_KIND_READY   ((BamEventKind){}u)\n\n\
         typedef uint32_t BamFieldKind;\n\
         #define BAM_FIELD_KIND_UNKNOWN    ((BamFieldKind){}u)\n\
         #define BAM_FIELD_KIND_BOOL       ((BamFieldKind){}u)\n\
         #define BAM_FIELD_KIND_U8         ((BamFieldKind){}u)\n\
         #define BAM_FIELD_KIND_I8         ((BamFieldKind){}u)\n\
         #define BAM_FIELD_KIND_U16        ((BamFieldKind){}u)\n\
         #define BAM_FIELD_KIND_I16        ((BamFieldKind){}u)\n\
         #define BAM_FIELD_KIND_U32        ((BamFieldKind){}u)\n\
         #define BAM_FIELD_KIND_I32        ((BamFieldKind){}u)\n\
         #define BAM_FIELD_KIND_U64        ((BamFieldKind){}u)\n\
         #define BAM_FIELD_KIND_I64        ((BamFieldKind){}u)\n\
         #define BAM_FIELD_KIND_F32        ((BamFieldKind){}u)\n\
         #define BAM_FIELD_KIND_F64        ((BamFieldKind){}u)\n\
         #define BAM_FIELD_KIND_USIZE      ((BamFieldKind){}u)\n\
         #define BAM_FIELD_KIND_ISIZE      ((BamFieldKind){}u)\n\
         #define BAM_FIELD_KIND_STRING     ((BamFieldKind){}u)\n\
         #define BAM_FIELD_KIND_VEC        ((BamFieldKind){}u)\n\
         #define BAM_FIELD_KIND_FFI_OPTION ((BamFieldKind){}u)\n\
         #define BAM_FIELD_KIND_NESTED     ((BamFieldKind){}u)\n\
         #define BAM_FIELD_KIND_OPAQUE     ((BamFieldKind){}u)\n\n\
         #define BAM_LOG_TRACE {BAM_LOG_TRACE}u\n\
         #define BAM_LOG_DEBUG {BAM_LOG_DEBUG}u\n\
         #define BAM_LOG_INFO  {BAM_LOG_INFO}u\n\
         #define BAM_LOG_WARN  {BAM_LOG_WARN}u\n\
         #define BAM_LOG_ERROR {BAM_LOG_ERROR}u\n\n\
         #define BAM_ASSET_HANDLE_KIND_NONE  ((uint8_t){}u)\n\
         #define BAM_ASSET_HANDLE_KIND_INDEX ((uint8_t){}u)\n\
         #define BAM_ASSET_HANDLE_KIND_UUID  ((uint8_t){}u)\n\n\
         typedef uint32_t BamAssetLoadState;\n\
         #define BAM_ASSET_LOAD_STATE_NOT_LOADED ((BamAssetLoadState){}u)\n\
         #define BAM_ASSET_LOAD_STATE_LOADING    ((BamAssetLoadState){}u)\n\
         #define BAM_ASSET_LOAD_STATE_LOADED     ((BamAssetLoadState){}u)\n\
         #define BAM_ASSET_LOAD_STATE_FAILED     ((BamAssetLoadState){}u)\n\n\
         typedef uint32_t BamScheduleId;\n\
         #define BAM_SCHEDULE_UPDATE       ((BamScheduleId){}u)\n\
         #define BAM_SCHEDULE_STARTUP      ((BamScheduleId){}u)\n\
         #define BAM_SCHEDULE_PRE_UPDATE   ((BamScheduleId){}u)\n\
         #define BAM_SCHEDULE_POST_UPDATE  ((BamScheduleId){}u)\n\
         #define BAM_SCHEDULE_FIXED_UPDATE ((BamScheduleId){}u)\n\
         #define BAM_SCHEDULE_LAST         ((BamScheduleId){}u)\n\
         #define BAM_SCHEDULE_CUSTOM_BASE  ((BamScheduleId){}u)\n\n\
         #define BAM_MANIFEST_KNOWN_FLAGS            0x{BAM_MANIFEST_KNOWN_FLAGS:08X}u\n\
         #define BAM_TERM_KNOWN_FLAGS                0x{BAM_TERM_KNOWN_FLAGS:04X}u\n\
         #define BAM_QUERY_KNOWN_FLAGS               0x{BAM_QUERY_KNOWN_FLAGS:08X}u\n\
         #define BAM_SYSTEM_FLAG_MAIN_THREAD         0x{BAM_SYSTEM_FLAG_MAIN_THREAD:08X}u\n\
         #define BAM_SYSTEM_FLAG_AMBIGUOUS_WITH_ALL  0x{BAM_SYSTEM_FLAG_AMBIGUOUS_WITH_ALL:08X}u\n\
         #define BAM_SYSTEM_KNOWN_FLAGS              0x{BAM_SYSTEM_KNOWN_FLAGS:08X}u\n\
         #define BAM_OBSERVER_FLAG_PROPAGATE         0x{BAM_OBSERVER_FLAG_PROPAGATE:08X}u\n\
         #define BAM_OBSERVER_KNOWN_FLAGS            0x{BAM_OBSERVER_KNOWN_FLAGS:08X}u\n\
         #define BAM_TRIGGER_FLAG_PROPAGATE          0x{BAM_TRIGGER_FLAG_PROPAGATE:08X}u\n\
         #define BAM_TRIGGER_KNOWN_FLAGS             0x{BAM_TRIGGER_KNOWN_FLAGS:08X}u\n\
         #define BAM_COMPONENT_FLAG_SPARSE_SET       0x{BAM_COMPONENT_FLAG_SPARSE_SET:08X}u\n\
         #define BAM_COMPONENT_FLAG_IMMUTABLE        0x{BAM_COMPONENT_FLAG_IMMUTABLE:08X}u\n\
         #define BAM_COMPONENT_FLAG_SUMMARY_TICK     0x{BAM_COMPONENT_FLAG_SUMMARY_TICK:08X}u\n\
         #define BAM_COMPONENT_KNOWN_FLAGS           0x{BAM_COMPONENT_KNOWN_FLAGS:08X}u\n\
         #define BAM_REQUIRED_FLAG_CUSTOM_BYTES      0x{BAM_REQUIRED_FLAG_CUSTOM_BYTES:08X}u\n\
         #define BAM_REQUIRED_FLAG_INIT_FN           0x{BAM_REQUIRED_FLAG_INIT_FN:08X}u\n\
         #define BAM_REQUIRED_FLAG_RESOLVE_BY_UUID   0x{BAM_REQUIRED_FLAG_RESOLVE_BY_UUID:08X}u\n\
         #define BAM_REQUIRED_KNOWN_FLAGS            0x{BAM_REQUIRED_KNOWN_FLAGS:08X}u\n\
         #define BAM_RELATION_FLAG_EXCLUSIVE         0x{BAM_RELATION_FLAG_EXCLUSIVE:08X}u\n\
         #define BAM_RELATION_FLAG_CASCADE_DESPAWN   0x{BAM_RELATION_FLAG_CASCADE_DESPAWN:08X}u\n\
         #define BAM_RELATION_FLAG_SYMMETRIC         0x{BAM_RELATION_FLAG_SYMMETRIC:08X}u\n\
         #define BAM_RELATION_KNOWN_FLAGS            0x{BAM_RELATION_KNOWN_FLAGS:08X}u\n\
         #define BAM_SCHEDULE_SET_KNOWN_FLAGS        0x{BAM_SCHEDULE_SET_KNOWN_FLAGS:08X}u\n\
         #define BAM_STORAGE_FLAG_SPARSE_SET         0x{BAM_STORAGE_FLAG_SPARSE_SET:08X}u\n\
         #define BAM_STORAGE_FLAG_IMMUTABLE          0x{BAM_STORAGE_FLAG_IMMUTABLE:08X}u\n\
         #define BAM_STORAGE_FLAG_RESOURCE           0x{BAM_STORAGE_FLAG_RESOURCE:08X}u\n\
         #define BAM_STORAGE_FLAG_MESSAGE            0x{BAM_STORAGE_FLAG_MESSAGE:08X}u\n\
         #define BAM_STORAGE_FLAG_EVENT              0x{BAM_STORAGE_FLAG_EVENT:08X}u\n\
         #define BAM_STORAGE_FLAG_ENTITY_EVENT       0x{BAM_STORAGE_FLAG_ENTITY_EVENT:08X}u\n\
         #define BAM_STORAGE_FLAG_SUMMARY_TICK       0x{BAM_STORAGE_FLAG_SUMMARY_TICK:08X}u\n",
        FfiBool::FALSE.0,
        FfiBool::TRUE.0,
        BamComponent::INVALID.0,
        BamSystem::INVALID.0,
        BamSystemSet::INVALID.0,
        BamSymbol::INVALID.0,
        FfiEntity::NULL.0,
        FfiResult::OK.0,
        FfiResult::INVALID_COMPONENT.0,
        FfiResult::ACCESS_CONFLICT.0,
        FfiResult::PANIC_CAUGHT.0,
        FfiResult::NULL_POINTER.0,
        FfiResult::INVALID_UTF8.0,
        FfiResult::PERMISSION_DENIED.0,
        FfiResult::INVALID_VERSION.0,
        FfiResult::INVALID_ARGUMENT.0,
        FfiResult::OUT_OF_RANGE.0,
        FfiResult::NOT_FOUND.0,
        FfiResult::INVALID_STATE.0,
        FfiResult::INVALID_ENTITY.0,
        FfiResult::CAPACITY_EXCEEDED.0,
        FfiResult::UNSUPPORTED.0,
        FfiResult::TYPE_MISMATCH.0,
        FfiResult::INTERNAL_ERROR.0,
        AccessMode::READ.0,
        AccessMode::WRITE.0,
        AccessMode::FILTER_ONLY.0,
        AccessMode::WITHOUT.0,
        AccessMode::OPTIONAL_READ.0,
        AccessMode::OPTIONAL_WRITE.0,
        AccessMode::CHANGED.0,
        AccessMode::ADDED.0,
        AccessMode::RESOURCE_READ.0,
        AccessMode::RESOURCE_WRITE.0,
        AccessMode::MESSAGE_READ.0,
        AccessMode::MESSAGE_WRITE.0,
        AccessMode::ALLOW.0,
        AccessMode::HAS.0,
        AccessMode::REMOVED_READ.0,
        BamEventKind::CUSTOM.0,
        BamEventKind::ADD.0,
        BamEventKind::INSERT.0,
        BamEventKind::DISCARD.0,
        BamEventKind::REMOVE.0,
        BamEventKind::DESPAWN.0,
        BamEventKind::READY.0,
        BamFieldKind::UNKNOWN.0,
        BamFieldKind::BOOL.0,
        BamFieldKind::U8.0,
        BamFieldKind::I8.0,
        BamFieldKind::U16.0,
        BamFieldKind::I16.0,
        BamFieldKind::U32.0,
        BamFieldKind::I32.0,
        BamFieldKind::U64.0,
        BamFieldKind::I64.0,
        BamFieldKind::F32.0,
        BamFieldKind::F64.0,
        BamFieldKind::USIZE.0,
        BamFieldKind::ISIZE.0,
        BamFieldKind::STRING.0,
        BamFieldKind::VEC.0,
        BamFieldKind::FFI_OPTION.0,
        BamFieldKind::NESTED.0,
        BamFieldKind::OPAQUE.0,
        FfiAssetHandle::KIND_NONE,
        FfiAssetHandle::KIND_INDEX,
        FfiAssetHandle::KIND_UUID,
        FfiAssetLoadState::NOT_LOADED.0,
        FfiAssetLoadState::LOADING.0,
        FfiAssetLoadState::LOADED.0,
        FfiAssetLoadState::FAILED.0,
        ScheduleId::UPDATE.0,
        ScheduleId::STARTUP.0,
        ScheduleId::PRE_UPDATE.0,
        ScheduleId::POST_UPDATE.0,
        ScheduleId::FIXED_UPDATE.0,
        ScheduleId::LAST.0,
        ScheduleId::CUSTOM_BASE,
    );

    // 4. Opaque handles, structs, and function pointers
    let cpp14_guard = "#if defined(__cplusplus) && ((__cplusplus >= 201402L) || (defined(_MSVC_LANG) && _MSVC_LANG >= 201402L))";
    let _ = writeln!(
        out,
        "/* ==========================================================================\n\
         * 4. Opaque Context Handles, Wire Descriptors & Function Pointer Typedefs\n\
         * ========================================================================== */\n\n\
         typedef struct BamHostCtx   BamHostCtx;\n\
         typedef struct BamLoadCtx   BamLoadCtx;\n\
         typedef struct BamSystemCtx BamSystemCtx;\n\
         typedef struct BamIter      BamIter;\n\
         typedef struct BamCommands  BamCommands;\n\
         typedef struct BamInstance  BamInstance;\n\
         typedef struct BamHostApi   BamHostApi;\n\n\
         typedef struct BamStrView {{\n\
             const uint8_t* ptr;\n\
             size_t         len;\n\
         }} BamStrView;\n\n\
         typedef BamStrView BamByteSlice;\n\n\
         typedef struct BamComponentUuid {{\n\
             uint8_t bytes[16];\n\
         }} BamComponentUuid;\n\n\
         #define BAM_OPTION_STRUCT(ElemType, StructName) \\\n\
             typedef struct StructName {{                 \\\n\
                 BamBool  is_some;                       \\\n\
                 ElemType value;                         \\\n\
             }} StructName\n\n\
         typedef struct BamAssetHandle {{\n\
             uint8_t          kind;\n\
             uint8_t          reserved0[3];\n\
             BamComponent     asset_type;\n\
             uint32_t         index;\n\
             uint32_t         generation;\n\
             BamComponentUuid uuid;\n\
         }} BamAssetHandle;\n\n\
         typedef struct BamChildOf {{\n\
             BamEntity parent;\n\
         }} BamChildOf;\n\n\
         typedef struct BamChangeTicks {{\n\
             uint32_t added;\n\
             uint32_t changed;\n\
             uint32_t last_run;\n\
             uint32_t this_run;\n\
         }} BamChangeTicks;\n\n\
         typedef BamResult (*BamStateWriterFn)(void* ctx, const uint8_t* bytes, size_t len);\n\
         typedef BamResult (*BamSaveStateFn)(BamInstance* instance, BamStateWriterFn writer, void* writer_ctx);\n\
         typedef BamResult (*BamRestoreStateFn)(BamInstance* instance, const uint8_t* bytes, size_t len);\n\n\
         typedef struct BamManifest {{\n\
             uint32_t    struct_size;\n\
             uint32_t    abi_version;\n\
             const char* name;\n\
             const char* version;\n\
             uint32_t    flags;\n\
             uint32_t    reserved0;\n\
         {cpp14_guard}\n\
             BamSaveStateFn    save_state    = nullptr;\n\
             BamRestoreStateFn restore_state = nullptr;\n\
         #else\n\
             BamSaveStateFn    save_state;\n\
             BamRestoreStateFn restore_state;\n\
         #endif\n\
         }} BamManifest;\n\n\
         typedef void      (*BamComponentDropFn)(void* ptr);\n\
         typedef void      (*BamComponentHookFn)(BamEntity entity, const void* data_ptr, void* user_data);\n\
         typedef BamResult (*BamComponentCloneFn)(const void* src, void* dst, void* user_data);\n\
         typedef BamResult (*BamRequiredInitFn)(void* dst, void* user_data);\n\
         typedef BamResult (*BamRowMigrateFn)(const void* src_row, void* dst_row, void* user_data);\n\
         typedef BamResult (*BamSystemFn)(BamSystemCtx* sys_ctx, void* user_data);\n\
         typedef BamBool   (*BamRunIfFn)(void* user_data);\n\
         typedef BamResult (*BamObserverFn)(BamEntity entity, const void* event_data, BamCommands* commands, void* user_data);\n\
         typedef BamResult (*BamObserverMutFn)(BamEntity entity, uint8_t* event_data, BamBool* propagate, BamCommands* commands, void* user_data);\n\n\
         typedef struct BamTermDesc {{\n\
             BamComponent  component;\n\
             BamAccessMode mode;\n\
             uint16_t      flags;\n\
             uint8_t       or_group;\n\
             uint8_t       reserved0;\n\
         }} BamTermDesc;\n\n\
         typedef struct BamQueryDesc {{\n\
             uint32_t           struct_size;\n\
             uint32_t           flags;\n\
             const BamTermDesc* terms;\n\
             size_t             term_count;\n\
         }} BamQueryDesc;\n\n\
         typedef struct BamSystemDesc {{\n\
             uint32_t            struct_size;\n\
             uint32_t            flags;\n\
             const char*         name;\n\
             const BamQueryDesc* queries;\n\
             size_t              query_count;\n\
             BamSystemFn         callback;\n\
             void*               user_data;\n\
             BamScheduleId       schedule;\n\
             uint32_t            reserved0;\n\
             const char* const*  after;\n\
             size_t              after_count;\n\
             const char* const*  before;\n\
             size_t              before_count;\n\
             BamRunIfFn          run_if;\n\
             void*               run_if_user_data;\n\
         {cpp14_guard}\n\
             const char* const*  in_sets              = nullptr;\n\
             size_t              in_set_count         = 0;\n\
             const char* const*  after_weak           = nullptr;\n\
             size_t              after_weak_count     = 0;\n\
             const char* const*  before_weak          = nullptr;\n\
             size_t              before_weak_count    = 0;\n\
             const char* const*  ambiguous_with       = nullptr;\n\
             size_t              ambiguous_with_count = 0;\n\
         #else\n\
             const char* const*  in_sets;\n\
             size_t              in_set_count;\n\
             const char* const*  after_weak;\n\
             size_t              after_weak_count;\n\
             const char* const*  before_weak;\n\
             size_t              before_weak_count;\n\
             const char* const*  ambiguous_with;\n\
             size_t              ambiguous_with_count;\n\
         #endif\n\
         }} BamSystemDesc;\n\n\
         typedef struct BamObserverDesc {{\n\
             uint32_t      struct_size;\n\
             uint32_t      flags;\n\
             const char*   name;\n\
             BamComponent  event;\n\
             uint32_t      entity_lo;\n\
             uint32_t      entity_hi;\n\
             uint32_t      reserved0;\n\
             BamObserverFn callback;\n\
             void*         user_data;\n\
         {cpp14_guard}\n\
             BamEventKind        event_kind              = BAM_EVENT_KIND_CUSTOM;\n\
             uint32_t            reserved1               = 0;\n\
             const BamComponent* watched_components      = nullptr;\n\
             size_t              watched_component_count = 0;\n\
             BamObserverMutFn    mut_callback            = nullptr;\n\
             BamComponent        propagate_relationship  = BAM_COMPONENT_INVALID;\n\
             uint32_t            reserved2               = 0;\n\
         #else\n\
             BamEventKind        event_kind;\n\
             uint32_t            reserved1;\n\
             const BamComponent* watched_components;\n\
             size_t              watched_component_count;\n\
             BamObserverMutFn    mut_callback;\n\
             BamComponent        propagate_relationship;\n\
             uint32_t            reserved2;\n\
         #endif\n\
         }} BamObserverDesc;\n\n\
         typedef struct BamTriggerDesc {{\n\
             uint32_t            struct_size;\n\
             uint32_t            flags;\n\
             BamComponent        event;\n\
             BamEventKind        event_kind;\n\
             uint32_t            entity_lo;\n\
             uint32_t            entity_hi;\n\
             uint8_t*            data_ptr;\n\
             size_t              data_len;\n\
             const BamComponent* watched_components;\n\
             size_t              watched_component_count;\n\
             BamComponent        propagate_relationship;\n\
             uint32_t            reserved0;\n\
         }} BamTriggerDesc;\n\n\
         typedef struct BamFieldDesc {{\n\
             uint32_t     struct_size;\n\
             BamFieldKind kind;\n\
             BamStrView   name;\n\
             BamStrView   type_tag;\n\
             size_t       offset;\n\
             size_t       size;\n\
             size_t       align;\n\
         }} BamFieldDesc;\n\n\
         typedef struct BamRequiredComponent {{\n\
             uint32_t          struct_size;\n\
             uint32_t          flags;\n\
             BamComponent      component;\n\
             uint32_t          reserved0;\n\
             BamComponentUuid  uuid;\n\
             const uint8_t*    init_bytes;\n\
             size_t            init_size;\n\
             BamRequiredInitFn init_fn;\n\
             void*             user_data;\n\
         }} BamRequiredComponent;\n\n\
         typedef struct BamRelationDesc {{\n\
             uint32_t         struct_size;\n\
             uint32_t         flags;\n\
             const char*      name;\n\
             BamComponentUuid uuid;\n\
             size_t           data_size;\n\
             size_t           data_align;\n\
             uint32_t         max_degree;\n\
             uint32_t         reserved0;\n\
         }} BamRelationDesc;\n\n\
         typedef struct BamLayoutInfo {{\n\
             uint32_t            struct_size;\n\
             uint32_t            flags;\n\
             BamComponentUuid    uuid;\n\
             uint64_t            schema_fingerprint;\n\
             size_t              size;\n\
             size_t              align;\n\
             const BamFieldDesc* fields;\n\
             size_t              field_count;\n\
         }} BamLayoutInfo;\n\n\
         typedef struct BamScheduleSetDesc {{\n\
             uint32_t            struct_size;\n\
             uint32_t            flags;\n\
             BamScheduleId       schedule;\n\
             uint32_t            before_count;\n\
             uint32_t            after_count;\n\
             uint32_t            reserved0;\n\
             const char*         name;\n\
             const BamSystemSet* before_sets;\n\
             const BamSystemSet* after_sets;\n\
         }} BamScheduleSetDesc;\n\n\
         typedef struct BamComponentDesc {{\n\
             uint32_t           struct_size;\n\
             uint32_t           flags;\n\
             const char*        name;\n\
             BamComponentUuid   uuid;\n\
             size_t             size;\n\
             size_t             align;\n\
             BamComponentDropFn drop_fn;\n\
             const void*        vtable_ptr;\n\
         {cpp14_guard}\n\
             BamComponentHookFn          on_add                   = nullptr;\n\
             BamComponentHookFn          on_remove                = nullptr;\n\
             void*                       hook_user_data           = nullptr;\n\
             BamComponentHookFn          on_insert                = nullptr;\n\
             BamComponentHookFn          on_discard               = nullptr;\n\
             BamComponentHookFn          on_despawn               = nullptr;\n\
             BamComponentCloneFn         clone_fn                 = nullptr;\n\
             const BamComponent*         required_components      = nullptr;\n\
             size_t                      required_component_count = 0;\n\
             uint64_t                    schema_fingerprint       = 0ull;\n\
             const BamFieldDesc*         fields                   = nullptr;\n\
             size_t                      field_count              = 0;\n\
             const BamRequiredComponent* required_init_descs      = nullptr;\n\
             size_t                      required_init_desc_count = 0;\n\
         #else\n\
             BamComponentHookFn          on_add;\n\
             BamComponentHookFn          on_remove;\n\
             void*                       hook_user_data;\n\
             BamComponentHookFn          on_insert;\n\
             BamComponentHookFn          on_discard;\n\
             BamComponentHookFn          on_despawn;\n\
             BamComponentCloneFn         clone_fn;\n\
             const BamComponent*         required_components;\n\
             size_t                      required_component_count;\n\
             uint64_t                    schema_fingerprint;\n\
             const BamFieldDesc*         fields;\n\
             size_t                      field_count;\n\
             const BamRequiredComponent* required_init_descs;\n\
             size_t                      required_init_desc_count;\n\
         #endif\n\
         }} BamComponentDesc;\n\n\
         typedef struct BamColumnInfo {{\n\
             BamComponent     component;\n\
             BamAccessMode    mode;\n\
             BamComponentUuid uuid;\n\
             size_t           size;\n\
             size_t           align;\n\
             uint32_t         storage_flags;\n\
             uint32_t         reserved0;\n\
         }} BamColumnInfo;\n\n\
         typedef struct BamVTableHeader {{\n\
             uint32_t         struct_size;\n\
             uint32_t         abi_version;\n\
             BamComponentUuid uuid;\n\
             size_t           size;\n\
             size_t           align;\n\
         }} BamVTableHeader;\n\n\
         #define BAM_MAPPED_WIRE_VTABLE_STRUCT(WireType, StructName) \\\n\
             typedef struct StructName {{                             \\\n\
                 BamVTableHeader header;                             \\\n\
                 BamResult (*get)(const void* ptr, WireType* out);   \\\n\
                 BamResult (*set)(void* ptr, const WireType* in_v);  \\\n\
             }} StructName\n\n\
         typedef BamStrView (*BamMappedStringGetFn)(const void* ptr);\n\
         typedef BamResult  (*BamMappedStringSetFn)(void* ptr, BamStrView view);\n\n\
         typedef struct BamMappedStringVTable {{\n\
             BamVTableHeader      header;\n\
             BamMappedStringGetFn get;\n\
             BamMappedStringSetFn set;\n\
         }} BamMappedStringVTable;\n"
    );

    // BamHostApi function pointer typedefs + 68-slot struct + MIN_SIZE + OFFSET macros
    out.push_str(
        "/* --- Mandatory Guest Entry-Point Typedefs --- */\n\
         typedef const BamManifest* (*BamManifestFn)(void);\n\
         typedef BamResult          (*BamInitFn)(const BamHostApi* api, BamLoadCtx* load_ctx, BamInstance** instance_out);\n\
         typedef void               (*BamShutdownFn)(BamInstance* instance);\n\n\
         /* --- 1. Diagnostics & Custom Symbol Discovery --- */\n\
         typedef const void* (*BamGetProcAddressFn)(const BamHostCtx* ctx, const char* name);\n\
         typedef size_t      (*BamLastErrorFn)(const BamHostCtx* ctx, char* buf, size_t cap);\n\n\
         /* --- 2. Init-Time Registration & Handle Resolution --- */\n\
         typedef const BamHostCtx* (*BamLoadHostCtxFn)(const BamLoadCtx* load_ctx);\n\
         typedef BamResult         (*BamRegisterComponentFn)(BamLoadCtx* load_ctx, const BamComponentDesc* desc, BamComponent* out_component);\n\
         typedef BamResult         (*BamResolveComponentFn)(const BamHostCtx* ctx, const char* name, const BamComponentUuid* uuid, BamComponent* out_component);\n\
         typedef BamResult         (*BamRegisterSystemFn)(BamLoadCtx* load_ctx, const BamSystemDesc* desc);\n\n\
         /* --- 3. System Execution Context Accessors --- */\n\
         typedef const BamHostCtx* (*BamSysHostCtxFn)(const BamSystemCtx* sys_ctx);\n\
         typedef BamResult         (*BamSysQueryFn)(BamSystemCtx* sys_ctx, size_t query_index, BamIter** out_iter);\n\
         typedef BamResult         (*BamSysCommandsFn)(BamSystemCtx* sys_ctx, BamCommands** out_commands);\n\n\
         /* --- 4. Chunk & Sparse-Row Iteration --- */\n\
         typedef BamBool            (*BamIterNextChunkFn)(BamIter* iter, size_t* out_len);\n\
         typedef BamResult          (*BamIterResetFn)(BamIter* iter);\n\
         typedef const BamEntity*   (*BamIterEntitiesFn)(const BamIter* iter);\n\
         typedef const uint8_t*     (*BamIterColumnReadonlyFn)(const BamIter* iter, size_t term_index);\n\
         typedef uint8_t*           (*BamIterColumnMutableFn)(BamIter* iter, size_t term_index);\n\
         typedef uint8_t*           (*BamIterColumnMutableUntrackedFn)(BamIter* iter, size_t term_index);\n\
         typedef BamResult          (*BamIterMarkChangedRangeFn)(BamIter* iter, size_t term_index, size_t start_row, size_t row_count);\n\
         typedef const uint8_t* const* (*BamIterColumnRowsReadonlyFn)(BamIter* iter, size_t term_index);\n\
         typedef uint8_t* const*    (*BamIterColumnRowsMutableFn)(BamIter* iter, size_t term_index);\n\
         typedef BamResult          (*BamIterColumnInfoFn)(const BamIter* iter, size_t term_index, BamColumnInfo* out_info);\n\n\
         /* --- 5. Declaration-Indexed Singleton Resource Access --- */\n\
         typedef const uint8_t* (*BamResPtrFn)(const BamIter* iter, size_t term_index);\n\
         typedef uint8_t*       (*BamResMutPtrFn)(BamIter* iter, size_t term_index);\n\n\
         /* --- 6. Handle-Based & Batched Deferred Commands --- */\n\
         typedef BamResult (*BamCmdSpawnEmptyFn)(BamCommands* commands, BamEntity* out_entity);\n\
         typedef BamResult (*BamCmdSpawnFn)(BamCommands* commands, const BamComponent* components, const uint8_t* const* data_ptrs, size_t count, BamEntity* out_entity);\n\
         typedef BamResult (*BamCmdDespawnFn)(BamCommands* commands, BamEntity entity);\n\
         typedef BamResult (*BamCmdInsertFn)(BamCommands* commands, BamEntity entity, BamComponent component, const uint8_t* data_ptr, size_t data_len);\n\
         typedef BamResult (*BamCmdInsertDefaultFn)(BamCommands* commands, BamEntity entity, BamComponent component);\n\
         typedef BamResult (*BamCmdRemoveFn)(BamCommands* commands, BamEntity entity, BamComponent component);\n\n\
         /* --- 7. Extended Engine Slots (ABI 0.2.x Extensions) --- */\n\
         typedef BamResult      (*BamChunkTaskFn)(BamIter* iter, void* user_data);\n\
         typedef BamResult      (*BamParForEachChunkFn)(BamIter* iter, BamChunkTaskFn chunk_fn, void* user_data);\n\
         typedef const uint8_t* (*BamSysEntityGetFn)(const BamSystemCtx* sys_ctx, size_t query_index, BamEntity entity, size_t term_index);\n\
         typedef uint8_t*       (*BamSysEntityGetMutFn)(BamSystemCtx* sys_ctx, size_t query_index, BamEntity entity, size_t term_index);\n\
         typedef BamResult      (*BamMsgReadBatchFn)(BamIter* iter, size_t term_index, uint8_t* out_buf, size_t max_count, size_t* out_read);\n\
         typedef BamResult      (*BamMsgSendFn)(BamIter* iter, size_t term_index, const uint8_t* data_ptr, size_t data_len);\n\
         typedef BamResult      (*BamMsgWriteBatchFn)(BamIter* iter, size_t term_index, const uint8_t* in_buf, size_t count);\n\
         typedef BamResult      (*BamResTicksFn)(const BamIter* iter, size_t term_index, BamChangeTicks* out_ticks);\n\
         typedef const uint8_t* (*BamSysResPtrFn)(const BamSystemCtx* sys_ctx, BamComponent component, BamChangeTicks* out_ticks);\n\
         typedef uint8_t*       (*BamSysResMutPtrFn)(BamSystemCtx* sys_ctx, BamComponent component, BamChangeTicks* out_ticks);\n\
         typedef BamResult      (*BamRegisterObserverFn)(BamLoadCtx* load_ctx, const BamObserverDesc* desc);\n\
         typedef BamResult      (*BamCmdTriggerFn)(BamCommands* commands, BamComponent event, BamEntity target, const uint8_t* data_ptr, size_t data_len);\n\
         typedef BamResult      (*BamCmdSpawnBatchFn)(BamCommands* commands, const BamComponent* components, const uint8_t* const* column_ptrs, size_t component_count, size_t entity_count, BamEntity* out_entities);\n\
         typedef BamResult      (*BamCmdDespawnAllFn)(BamCommands* commands, const BamTermDesc* terms, size_t term_count);\n\
         typedef BamResult      (*BamCmdTriggerExFn)(BamCommands* commands, const BamTriggerDesc* desc);\n\
         typedef BamResult      (*BamCmdObserveFn)(BamCommands* commands, const BamObserverDesc* desc, BamEntity* out_entity);\n\
         typedef BamResult      (*BamIterReadRemovedFn)(BamIter* iter, size_t term_index, BamEntity* out_entities, size_t max_count, size_t* out_read);\n\
         typedef BamResult      (*BamSysEntityChildrenFn)(const BamSystemCtx* sys_ctx, BamEntity entity, BamComponent relationship_target, const BamEntity** out_entities, size_t* out_len);\n\
         typedef BamResult      (*BamAssetLoadFn)(const BamHostCtx* ctx, BamComponent asset_type, BamStrView path, BamAssetHandle* out_handle);\n\
         typedef BamResult      (*BamAssetGetLoadStateFn)(const BamHostCtx* ctx, const BamAssetHandle* handle, BamAssetLoadState* out_state);\n\
         typedef BamResult      (*BamAssetReleaseFn)(const BamHostCtx* ctx, const BamAssetHandle* handle);\n\
         typedef BamResult      (*BamCmdSpawnSceneFn)(BamCommands* commands, const BamAssetHandle* scene_handle, BamEntity parent, BamEntity* out_entity);\n\n\
         /* --- 8. ABI 0.3.0 Diagnostics, Schema Reflection, Relations, Scheduling & Checkpoints --- */\n\
         typedef BamResult (*BamLogDiagnosticFn)(const BamHostCtx* ctx, uint32_t level, BamStrView message);\n\
         typedef BamResult (*BamReportGuestFaultFn)(const BamHostCtx* ctx, BamStrView message);\n\
         typedef BamResult (*BamResolveComponentByUuidFn)(const BamHostCtx* ctx, const BamComponentUuid* uuid, BamComponent* out_component);\n\
         typedef BamResult (*BamComponentLayoutInfoFn)(const BamHostCtx* ctx, BamComponent component, BamLayoutInfo* out_info);\n\
         typedef BamResult (*BamRegisterRelationFn)(BamLoadCtx* load_ctx, const BamRelationDesc* desc, BamComponent* out_component);\n\
         typedef BamResult (*BamInternSymbolFn)(const BamHostCtx* ctx, BamStrView symbol, BamSymbol* out_symbol);\n\
         typedef BamResult (*BamBindNamedObjectFn)(const BamHostCtx* ctx, BamStrView name, BamEntity entity);\n\
         typedef BamResult (*BamIterCountFn)(BamIter* iter, size_t* out_count);\n\
         typedef BamResult (*BamIterUpCascadeFn)(BamIter* iter, BamComponent relation, BamEntity entity, size_t term_index, const void** out_ptr, BamEntity* out_owner);\n\
         typedef BamResult (*BamRegisterSystemHandleFn)(BamLoadCtx* load_ctx, const BamSystemDesc* desc, BamSystem* out_system);\n\
         typedef BamResult (*BamConfigureScheduleSetFn)(BamLoadCtx* load_ctx, const BamScheduleSetDesc* desc, BamSystemSet* out_set);\n\
         typedef BamResult (*BamMountSystemInScheduleFn)(BamLoadCtx* load_ctx, BamSystem system, BamScheduleId schedule, BamSystemSet set);\n\
         typedef BamResult (*BamCmdCheckpointFn)(BamCommands* commands, size_t* out_checkpoint);\n\
         typedef BamResult (*BamCmdRollbackFn)(BamCommands* commands, size_t checkpoint);\n\
         typedef BamResult (*BamCmdInsertRelationFn)(BamCommands* commands, BamEntity entity, BamComponent relation, BamEntity target, const void* data, size_t data_size);\n\
         typedef BamResult (*BamCmdRemoveRelationFn)(BamCommands* commands, BamEntity entity, BamComponent relation, BamEntity target);\n\
         typedef BamResult (*BamMigrateComponentColumnFn)(BamLoadCtx* load_ctx, BamComponent component, uint64_t old_fingerprint, const BamComponentDesc* new_desc, BamRowMigrateFn migrate_fn, void* user_data);\n\n\
         struct BamHostApi {\n\
             uint32_t struct_size;\n\
             uint32_t abi_version;\n\n\
             /* Diagnostics & custom symbols */\n\
             BamGetProcAddressFn get_proc_address;\n\
             BamLastErrorFn      last_error;\n\n\
             /* Registration & handle resolution */\n\
             BamLoadHostCtxFn       load_host_ctx;\n\
             BamRegisterComponentFn register_component;\n\
             BamResolveComponentFn  resolve_component;\n\
             BamRegisterSystemFn    register_system;\n\n\
             /* System invocation context */\n\
             BamSysHostCtxFn  sys_host_ctx;\n\
             BamSysQueryFn    sys_query;\n\
             BamSysCommandsFn sys_commands;\n\n\
             /* Chunk & sparse-row iteration */\n\
             BamIterNextChunkFn              iter_next_chunk;\n\
             BamIterResetFn                  iter_reset;\n\
             BamIterEntitiesFn               iter_entities;\n\
             BamIterColumnReadonlyFn         iter_column_readonly;\n\
             BamIterColumnMutableFn          iter_column_mutable;\n\
             BamIterColumnMutableUntrackedFn iter_column_mutable_untracked;\n\
             BamIterMarkChangedRangeFn       iter_mark_changed_range;\n\
             BamIterColumnRowsReadonlyFn     iter_column_rows_readonly;\n\
             BamIterColumnRowsMutableFn      iter_column_rows_mutable;\n\
             BamIterColumnInfoFn             iter_column_info;\n\n\
             /* Declaration-indexed singleton resources */\n\
             BamResPtrFn    res_ptr;\n\
             BamResMutPtrFn res_mut_ptr;\n\n\
             /* Handle-based & batched deferred commands */\n\
             BamCmdSpawnEmptyFn    cmd_spawn_empty;\n\
             BamCmdSpawnFn         cmd_spawn;\n\
             BamCmdDespawnFn       cmd_despawn;\n\
             BamCmdInsertFn        cmd_insert;\n\
             BamCmdInsertDefaultFn cmd_insert_default;\n\
             BamCmdRemoveFn        cmd_remove;\n\n\
             /* Extended engine slots (appended after `BAM_HOST_API_MIN_V0_2_SIZE`) */\n\
             BamParForEachChunkFn    par_for_each_chunk;\n\
             BamSysEntityGetFn       sys_entity_get;\n\
             BamSysEntityGetMutFn    sys_entity_get_mut;\n\
             BamMsgReadBatchFn       msg_read_batch;\n\
             BamMsgSendFn            msg_send;\n\
             BamMsgWriteBatchFn      msg_write_batch;\n\
             BamResTicksFn           res_ticks;\n\
             BamSysResPtrFn          sys_res_ptr;\n\
             BamSysResMutPtrFn       sys_res_mut_ptr;\n\
             BamRegisterObserverFn   register_observer;\n\
             BamCmdTriggerFn         cmd_trigger;\n\
             BamCmdSpawnFn           cmd_spawn_persistent;\n\
             BamCmdSpawnBatchFn      cmd_spawn_batch;\n\
             BamCmdDespawnAllFn      cmd_despawn_all;\n\
             BamCmdInsertFn          cmd_insert_if_new;\n\
             BamCmdTriggerExFn       cmd_trigger_ex;\n\
             BamCmdObserveFn         cmd_observe;\n\
             BamIterReadRemovedFn    iter_read_removed;\n\
             BamSysEntityChildrenFn  sys_entity_children;\n\
             BamAssetLoadFn          asset_load;\n\
             BamAssetGetLoadStateFn  asset_get_load_state;\n\
             BamAssetReleaseFn       asset_release;\n\
             BamCmdSpawnSceneFn      cmd_spawn_scene;\n\n\
             /* ABI 0.3.0 diagnostics, schema reflection, relations, scheduling & checkpoints */\n\
             BamLogDiagnosticFn          log_diagnostic;\n\
             BamReportGuestFaultFn       report_guest_fault;\n\
             BamResolveComponentByUuidFn resolve_component_by_uuid;\n\
             BamComponentLayoutInfoFn    component_layout_info;\n\
             BamRegisterRelationFn       register_relation;\n\
             BamInternSymbolFn           intern_symbol;\n\
             BamBindNamedObjectFn        bind_named_object;\n\
             BamIterCountFn              iter_count;\n\
             BamIterUpCascadeFn          iter_up_cascade;\n\
             BamParForEachChunkFn        par_for_each_chunk_readonly;\n\
             BamRegisterSystemHandleFn   register_system_handle;\n\
             BamConfigureScheduleSetFn   configure_schedule_set;\n\
             BamMountSystemInScheduleFn  mount_system_in_schedule;\n\
             BamCmdCheckpointFn          cmd_checkpoint;\n\
             BamCmdRollbackFn            cmd_rollback;\n\
             BamCmdInsertRelationFn      cmd_insert_relation;\n\
             BamCmdRemoveRelationFn      cmd_remove_relation;\n\
             BamMigrateComponentColumnFn migrate_component_column;\n\
         };\n\n\
         #define BAM_MANIFEST_MIN_V0_2_SIZE            (offsetof(BamManifest, reserved0) + sizeof(uint32_t))\n\
         #define BAM_QUERY_DESC_MIN_V0_2_SIZE          (offsetof(BamQueryDesc, term_count) + sizeof(size_t))\n\
         #define BAM_SYSTEM_DESC_MIN_V0_2_SIZE         (offsetof(BamSystemDesc, run_if_user_data) + sizeof(void*))\n\
         #define BAM_OBSERVER_DESC_MIN_V0_2_SIZE       (offsetof(BamObserverDesc, user_data) + sizeof(void*))\n\
         #define BAM_TRIGGER_DESC_MIN_V0_2_SIZE        (offsetof(BamTriggerDesc, reserved0) + sizeof(uint32_t))\n\
         #define BAM_COMPONENT_DESC_MIN_V0_2_SIZE      (offsetof(BamComponentDesc, vtable_ptr) + sizeof(const void*))\n\
         #define BAM_COMPONENT_DESC_MIN_V0_3_SIZE      sizeof(BamComponentDesc)\n\
         #define BAM_FIELD_DESC_MIN_V0_3_SIZE          sizeof(BamFieldDesc)\n\
         #define BAM_REQUIRED_COMPONENT_MIN_V0_3_SIZE  sizeof(BamRequiredComponent)\n\
         #define BAM_RELATION_DESC_MIN_V0_3_SIZE       sizeof(BamRelationDesc)\n\
         #define BAM_LAYOUT_INFO_MIN_V0_3_SIZE         sizeof(BamLayoutInfo)\n\
         #define BAM_SCHEDULE_SET_DESC_MIN_V0_3_SIZE   sizeof(BamScheduleSetDesc)\n\
         #define BAM_VTABLE_HEADER_MIN_V0_2_SIZE       (offsetof(BamVTableHeader, align) + sizeof(size_t))\n\
         #define BAM_HOST_API_MIN_V0_2_SIZE            (offsetof(BamHostApi, cmd_remove) + sizeof(BamCmdRemoveFn))\n\
         #define BAM_HOST_API_MIN_V0_3_SIZE            sizeof(BamHostApi)\n\n\
         /* Byte offset constants for every BamHostApi function-pointer slot (0..67): */\n",
    );

    for field in host_api_spec.fields {
        if field.c_name == "struct_size" || field.c_name == "abi_version" {
            continue;
        }
        let macro_suffix = field.c_name.to_ascii_uppercase();
        let _ = writeln!(
            out,
            "#define BAM_HOST_API_OFFSET_{macro_suffix} offsetof(BamHostApi, {})",
            field.c_name
        );
    }
    out.push('\n');

    // 5. Inline C99 helpers
    out.push_str(
        "/* ==========================================================================\n\
         * 5. ISO C99 Inline Helpers (`bam_*`)\n\
         * ========================================================================== */\n\n\
         static BAM_UNUSED inline uint32_t bam_pack_abi_version(uint32_t major, uint32_t minor, uint32_t patch) {\n\
             return BAM_PACK_ABI_VERSION(major, minor, patch);\n\
         }\n\n\
         static BAM_UNUSED inline uint32_t bam_version_major(uint32_t version) {\n\
             return (version >> 24) & 0xFFu;\n\
         }\n\n\
         static BAM_UNUSED inline uint32_t bam_version_minor(uint32_t version) {\n\
             return (version >> 16) & 0xFFu;\n\
         }\n\n\
         static BAM_UNUSED inline uint32_t bam_version_patch(uint32_t version) {\n\
             return version & 0xFFFFu;\n\
         }\n\n\
         static BAM_UNUSED inline int bam_version_is_compatible(uint32_t host_version, uint32_t guest_version) {\n\
             uint32_t h_maj = bam_version_major(host_version);\n\
             uint32_t h_min = bam_version_minor(host_version);\n\
             uint32_t g_maj = bam_version_major(guest_version);\n\
             uint32_t g_min = bam_version_minor(guest_version);\n\
             if (h_maj == 0u || g_maj == 0u) {\n\
                 return (h_maj == 0u) && (g_maj == 0u) && (h_min == g_min);\n\
             }\n\
             return (h_maj == g_maj) && (g_min <= h_min);\n\
         }\n\n\
         static BAM_UNUSED inline const char* bam_result_name(BamResult code) {\n\
             switch (code) {\n\
                 case BAM_RESULT_OK:                return \"OK\";\n\
                 case BAM_RESULT_INVALID_COMPONENT: return \"INVALID_COMPONENT\";\n\
                 case BAM_RESULT_ACCESS_CONFLICT:   return \"ACCESS_CONFLICT\";\n\
                 case BAM_RESULT_PANIC_CAUGHT:      return \"PANIC_CAUGHT\";\n\
                 case BAM_RESULT_NULL_POINTER:      return \"NULL_POINTER\";\n\
                 case BAM_RESULT_INVALID_UTF8:      return \"INVALID_UTF8\";\n\
                 case BAM_RESULT_PERMISSION_DENIED: return \"PERMISSION_DENIED\";\n\
                 case BAM_RESULT_INVALID_VERSION:   return \"INVALID_VERSION\";\n\
                 case BAM_RESULT_INVALID_ARGUMENT:  return \"INVALID_ARGUMENT\";\n\
                 case BAM_RESULT_OUT_OF_RANGE:      return \"OUT_OF_RANGE\";\n\
                 case BAM_RESULT_NOT_FOUND:         return \"NOT_FOUND\";\n\
                 case BAM_RESULT_INVALID_STATE:     return \"INVALID_STATE\";\n\
                 case BAM_RESULT_INVALID_ENTITY:    return \"INVALID_ENTITY\";\n\
                 case BAM_RESULT_CAPACITY_EXCEEDED: return \"CAPACITY_EXCEEDED\";\n\
                 case BAM_RESULT_UNSUPPORTED:       return \"UNSUPPORTED\";\n\
                 case BAM_RESULT_TYPE_MISMATCH:     return \"TYPE_MISMATCH\";\n\
                 case BAM_RESULT_INTERNAL_ERROR:    return \"INTERNAL_ERROR\";\n\
                 default:                           return \"UNKNOWN\";\n\
             }\n\
         }\n\n\
         static BAM_UNUSED inline BamStrView bam_str_view_len(const uint8_t* ptr, size_t len) {\n\
             BamStrView v;\n\
             v.ptr = ptr;\n\
             v.len = len;\n\
             return v;\n\
         }\n\n\
         static BAM_UNUSED inline BamStrView bam_str_view(const char* cstr) {\n\
             if (cstr == NULL) {\n\
                 return bam_str_view_len(NULL, 0u);\n\
             }\n\
             return bam_str_view_len((const uint8_t*)cstr, strlen(cstr));\n\
         }\n\n\
         static BAM_UNUSED inline int bam_entity_is_null(BamEntity entity) {\n\
             return entity == BAM_ENTITY_NULL;\n\
         }\n\n\
         static BAM_UNUSED inline int bam_component_is_valid(BamComponent component) {\n\
             return component != BAM_COMPONENT_INVALID;\n\
         }\n\n\
         static BAM_UNUSED inline BamTermDesc bam_term_desc(BamComponent component, BamAccessMode mode, uint16_t flags) {\n\
             BamTermDesc t;\n\
             t.component = component;\n\
             t.mode = mode;\n\
             t.flags = flags;\n\
             t.or_group = 0u;\n\
             t.reserved0 = 0u;\n\
             return t;\n\
         }\n\n\
         static BAM_UNUSED inline BamTermDesc bam_term_desc_or(\n\
             BamComponent component,\n\
             BamAccessMode mode,\n\
             uint16_t flags,\n\
             uint8_t or_group\n\
         ) {\n\
             BamTermDesc t;\n\
             t.component = component;\n\
             t.mode = mode;\n\
             t.flags = flags;\n\
             t.or_group = or_group;\n\
             t.reserved0 = 0u;\n\
             return t;\n\
         }\n\n\
         static BAM_UNUSED inline BamAssetHandle bam_asset_handle_none(void) {\n\
             BamAssetHandle h;\n\
             memset(&h, 0, sizeof(BamAssetHandle));\n\
             h.kind = BAM_ASSET_HANDLE_KIND_NONE;\n\
             return h;\n\
         }\n\n\
         static BAM_UNUSED inline BamAssetHandle bam_asset_handle_from_index(\n\
             BamComponent asset_type,\n\
             uint32_t index,\n\
             uint32_t generation\n\
         ) {\n\
             BamAssetHandle h = bam_asset_handle_none();\n\
             h.kind = BAM_ASSET_HANDLE_KIND_INDEX;\n\
             h.asset_type = asset_type;\n\
             h.index = index;\n\
             h.generation = generation;\n\
             return h;\n\
         }\n\n\
         static BAM_UNUSED inline BamAssetHandle bam_asset_handle_from_uuid(\n\
             BamComponent asset_type,\n\
             BamComponentUuid uuid\n\
         ) {\n\
             BamAssetHandle h = bam_asset_handle_none();\n\
             h.kind = BAM_ASSET_HANDLE_KIND_UUID;\n\
             h.asset_type = asset_type;\n\
             h.uuid = uuid;\n\
             return h;\n\
         }\n\n\
         static BAM_UNUSED inline int bam_asset_handle_is_none(const BamAssetHandle* h) {\n\
             return (h == NULL) || (h->kind == BAM_ASSET_HANDLE_KIND_NONE);\n\
         }\n\n\
         static BAM_UNUSED inline int bam_asset_handle_is_some(const BamAssetHandle* h) {\n\
             return (h != NULL) && (h->kind == BAM_ASSET_HANDLE_KIND_INDEX || h->kind == BAM_ASSET_HANDLE_KIND_UUID);\n\
         }\n\n\
         static BAM_UNUSED inline uint32_t bam_host_api_raw_struct_size(const BamHostApi* api) {\n\
             if (api == NULL) {\n\
                 return 0u;\n\
             }\n\
             return api->struct_size;\n\
         }\n\n\
         static BAM_UNUSED inline int bam_host_api_covers_slot(const BamHostApi* api, size_t slot_offset, size_t slot_size) {\n\
             size_t sz;\n\
             if (api == NULL) {\n\
                 return 0;\n\
             }\n\
             sz = (size_t)api->struct_size;\n\
             return (slot_offset <= sz) && (slot_size <= (sz - slot_offset));\n\
         }\n\n\
         static BAM_UNUSED inline BamResult bam_host_api_copy_bounded(\n\
             BamHostApi* dst,\n\
             size_t dst_size,\n\
             const BamHostApi* src\n\
         ) {\n\
             size_t src_size;\n\
             size_t copy_len;\n\
             if (dst == NULL || src == NULL) {\n\
                 return BAM_RESULT_NULL_POINTER;\n\
             }\n\
             src_size = (size_t)bam_host_api_raw_struct_size(src);\n\
             if (src_size < BAM_HOST_API_MIN_V0_2_SIZE || dst_size < BAM_HOST_API_MIN_V0_2_SIZE) {\n\
                 return BAM_RESULT_INVALID_VERSION;\n\
             }\n\
             if (!bam_version_is_compatible(src->abi_version, BAM_ABI_VERSION)) {\n\
                 return BAM_RESULT_INVALID_VERSION;\n\
             }\n\
             memset(dst, 0, dst_size);\n\
             copy_len = src_size < dst_size ? src_size : dst_size;\n\
             memcpy(dst, src, copy_len);\n\
             return BAM_RESULT_OK;\n\
         }\n\n\
         static BAM_UNUSED inline uint64_t bam_rotl64(uint64_t x, unsigned int bits) {\n\
             return (x << bits) | (x >> (64u - bits));\n\
         }\n\n\
         static BAM_UNUSED inline void bam_sipround(uint64_t* v0, uint64_t* v1, uint64_t* v2, uint64_t* v3) {\n\
             *v0 += *v1;\n\
             *v1 = bam_rotl64(*v1, 13u) ^ *v0;\n\
             *v0 = bam_rotl64(*v0, 32u);\n\
             *v2 += *v3;\n\
             *v3 = bam_rotl64(*v3, 16u) ^ *v2;\n\
             *v0 += *v3;\n\
             *v3 = bam_rotl64(*v3, 21u) ^ *v0;\n\
             *v2 += *v1;\n\
             *v1 = bam_rotl64(*v1, 17u) ^ *v2;\n\
             *v2 = bam_rotl64(*v2, 32u);\n\
         }\n\n\
         static BAM_UNUSED inline uint64_t bam_read_le64(const uint8_t* p) {\n\
             return ((uint64_t)p[0])\n\
                  | ((uint64_t)p[1] << 8)\n\
                  | ((uint64_t)p[2] << 16)\n\
                  | ((uint64_t)p[3] << 24)\n\
                  | ((uint64_t)p[4] << 32)\n\
                  | ((uint64_t)p[5] << 40)\n\
                  | ((uint64_t)p[6] << 48)\n\
                  | ((uint64_t)p[7] << 56);\n\
         }\n\n\
         static BAM_UNUSED inline void bam_write_le64(uint8_t* dst, uint64_t v) {\n\
             dst[0] = (uint8_t)(v & 0xFFu);\n\
             dst[1] = (uint8_t)((v >> 8) & 0xFFu);\n\
             dst[2] = (uint8_t)((v >> 16) & 0xFFu);\n\
             dst[3] = (uint8_t)((v >> 24) & 0xFFu);\n\
             dst[4] = (uint8_t)((v >> 32) & 0xFFu);\n\
             dst[5] = (uint8_t)((v >> 40) & 0xFFu);\n\
             dst[6] = (uint8_t)((v >> 48) & 0xFFu);\n\
             dst[7] = (uint8_t)((v >> 56) & 0xFFu);\n\
         }\n\n\
         typedef struct BamSipHasher128 {\n\
             uint64_t v0;\n\
             uint64_t v1;\n\
             uint64_t v2;\n\
             uint64_t v3;\n\
             uint8_t  tail[8];\n\
             size_t   tail_len;\n\
             size_t   total_len;\n\
         } BamSipHasher128;\n\n\
         static BAM_UNUSED inline void bam_siphash128_init(BamSipHasher128* st, uint64_t k0, uint64_t k1) {\n\
             size_t i;\n\
             st->v0 = 0x736f6d6570736575ull ^ k0;\n\
             st->v1 = (0x646f72616e646f6dull ^ k1) ^ 0xeeull;\n\
             st->v2 = 0x6c7967656e657261ull ^ k0;\n\
             st->v3 = 0x7465646279746573ull ^ k1;\n\
             for (i = 0u; i < 8u; ++i) {\n\
                 st->tail[i] = 0u;\n\
             }\n\
             st->tail_len = 0u;\n\
             st->total_len = 0u;\n\
         }\n\n\
         static BAM_UNUSED inline void bam_siphash128_write(BamSipHasher128* st, const uint8_t* bytes, size_t len) {\n\
             size_t idx = 0u;\n\
             uint64_t m;\n\
             st->total_len += len;\n\
             if (st->tail_len > 0u) {\n\
                 while (idx < len && st->tail_len < 8u) {\n\
                     st->tail[st->tail_len] = bytes[idx];\n\
                     st->tail_len += 1u;\n\
                     idx += 1u;\n\
                 }\n\
                 if (st->tail_len == 8u) {\n\
                     m = bam_read_le64(st->tail);\n\
                     st->v3 ^= m;\n\
                     bam_sipround(&st->v0, &st->v1, &st->v2, &st->v3);\n\
                     bam_sipround(&st->v0, &st->v1, &st->v2, &st->v3);\n\
                     st->v0 ^= m;\n\
                     st->tail_len = 0u;\n\
                 }\n\
             }\n\
             while (idx + 8u <= len) {\n\
                 m = bam_read_le64(bytes + idx);\n\
                 st->v3 ^= m;\n\
                 bam_sipround(&st->v0, &st->v1, &st->v2, &st->v3);\n\
                 bam_sipround(&st->v0, &st->v1, &st->v2, &st->v3);\n\
                 st->v0 ^= m;\n\
                 idx += 8u;\n\
             }\n\
             while (idx < len && st->tail_len < 8u) {\n\
                 st->tail[st->tail_len] = bytes[idx];\n\
                 st->tail_len += 1u;\n\
                 idx += 1u;\n\
             }\n\
         }\n\n\
         static BAM_UNUSED inline void bam_siphash128_finish(BamSipHasher128* st, uint8_t out_tag[16]) {\n\
             uint64_t b = (((uint64_t)st->total_len) & 0xFFull) << 56;\n\
             uint64_t h0;\n\
             uint64_t h1;\n\
             size_t i;\n\
             for (i = 0u; i < st->tail_len; ++i) {\n\
                 b |= ((uint64_t)st->tail[i]) << (8u * i);\n\
             }\n\
             st->v3 ^= b;\n\
             bam_sipround(&st->v0, &st->v1, &st->v2, &st->v3);\n\
             bam_sipround(&st->v0, &st->v1, &st->v2, &st->v3);\n\
             st->v0 ^= b;\n\
             st->v2 ^= 0xeeull;\n\
             bam_sipround(&st->v0, &st->v1, &st->v2, &st->v3);\n\
             bam_sipround(&st->v0, &st->v1, &st->v2, &st->v3);\n\
             bam_sipround(&st->v0, &st->v1, &st->v2, &st->v3);\n\
             bam_sipround(&st->v0, &st->v1, &st->v2, &st->v3);\n\
             h0 = st->v0 ^ st->v1 ^ st->v2 ^ st->v3;\n\
             st->v1 ^= 0xddull;\n\
             bam_sipround(&st->v0, &st->v1, &st->v2, &st->v3);\n\
             bam_sipround(&st->v0, &st->v1, &st->v2, &st->v3);\n\
             bam_sipround(&st->v0, &st->v1, &st->v2, &st->v3);\n\
             bam_sipround(&st->v0, &st->v1, &st->v2, &st->v3);\n\
             h1 = st->v0 ^ st->v1 ^ st->v2 ^ st->v3;\n\
             bam_write_le64(out_tag, h0);\n\
             bam_write_le64(out_tag + 8, h1);\n\
         }\n\n\
         static BAM_UNUSED inline BamComponentUuid bam_uuid_from_parts(\n\
             const char* canonical_name,\n\
             size_t size,\n\
             size_t align,\n\
             uint64_t field_schema_hash\n\
         ) {\n\
             BamSipHasher128 st;\n\
             BamComponentUuid uuid;\n\
             uint8_t sep = 0xFFu;\n\
             uint8_t le_buf[8];\n\
             size_t name_len = (canonical_name != NULL) ? strlen(canonical_name) : 0u;\n\
             bam_siphash128_init(&st, BAM_SIPHASH_KEY_0, BAM_SIPHASH_KEY_1);\n\
             if (name_len > 0u) {\n\
                 bam_siphash128_write(&st, (const uint8_t*)canonical_name, name_len);\n\
             }\n\
             bam_siphash128_write(&st, &sep, 1u);\n\
             bam_write_le64(le_buf, (uint64_t)size);\n\
             bam_siphash128_write(&st, le_buf, 8u);\n\
             bam_write_le64(le_buf, (uint64_t)align);\n\
             bam_siphash128_write(&st, le_buf, 8u);\n\
             bam_write_le64(le_buf, field_schema_hash);\n\
             bam_siphash128_write(&st, le_buf, 8u);\n\
             bam_siphash128_finish(&st, uuid.bytes);\n\
             return uuid;\n\
         }\n\n\
         static BAM_UNUSED inline BamComponentUuid bam_uuid_from_canonical_name(const char* canonical_name) {\n\
             return bam_uuid_from_parts(canonical_name, 0u, 0u, 0ull);\n\
         }\n\n\
         static BAM_UNUSED inline int bam_uuid_equals(BamComponentUuid a, BamComponentUuid b) {\n\
             return memcmp(a.bytes, b.bytes, 16u) == 0;\n\
         }\n\n\
         static BAM_UNUSED inline int bam_ticks_is_newer(uint32_t tick, uint32_t last_run, uint32_t this_run) {\n\
             uint32_t ticks_since_insert = this_run - tick;\n\
             uint32_t clamped_insert = (ticks_since_insert < BAM_MAX_CHANGE_AGE) ? ticks_since_insert : BAM_MAX_CHANGE_AGE;\n\
             uint32_t ticks_since_system = this_run - last_run;\n\
             uint32_t clamped_system = (ticks_since_system < BAM_MAX_CHANGE_AGE) ? ticks_since_system : BAM_MAX_CHANGE_AGE;\n\
             return clamped_system > clamped_insert;\n\
         }\n\n\
         static BAM_UNUSED inline int bam_ticks_is_added(const BamChangeTicks* ticks) {\n\
             if (ticks == NULL) {\n\
                 return 0;\n\
             }\n\
             return bam_ticks_is_newer(ticks->added, ticks->last_run, ticks->this_run);\n\
         }\n\n\
         static BAM_UNUSED inline int bam_ticks_is_changed(const BamChangeTicks* ticks) {\n\
             if (ticks == NULL) {\n\
                 return 0;\n\
             }\n\
             return bam_ticks_is_newer(ticks->changed, ticks->last_run, ticks->this_run);\n\
         }\n\n\
         static BAM_UNUSED inline BamComponentDesc bam_desc_component_with_layout(\n\
             const char* name,\n\
             size_t size,\n\
             size_t align,\n\
             BamComponentDropFn drop_fn\n\
         ) {\n\
             BamComponentDesc d;\n\
             d.struct_size = (uint32_t)sizeof(BamComponentDesc);\n\
             d.flags = 0u;\n\
             d.name = name;\n\
             d.uuid = bam_uuid_from_parts(name, size, align, 0ull);\n\
             d.size = size;\n\
             d.align = align;\n\
             d.drop_fn = drop_fn;\n\
             d.vtable_ptr = NULL;\n\
             d.on_add = NULL;\n\
             d.on_remove = NULL;\n\
             d.hook_user_data = NULL;\n\
             d.on_insert = NULL;\n\
             d.on_discard = NULL;\n\
             d.on_despawn = NULL;\n\
             d.clone_fn = NULL;\n\
             d.required_components = NULL;\n\
             d.required_component_count = 0u;\n\
             d.schema_fingerprint = 0ull;\n\
             d.fields = NULL;\n\
             d.field_count = 0u;\n\
             d.required_init_descs = NULL;\n\
             d.required_init_desc_count = 0u;\n\
             return d;\n\
         }\n\n\
         static BAM_UNUSED inline BamObserverDesc bam_observer_desc(\n\
             BamComponent event,\n\
             BamEntity entity,\n\
             BamObserverFn callback,\n\
             void* user_data\n\
         ) {\n\
             BamObserverDesc d;\n\
             d.struct_size = (uint32_t)sizeof(BamObserverDesc);\n\
             d.flags = 0u;\n\
             d.name = NULL;\n\
             d.event = event;\n\
             d.entity_lo = (uint32_t)(entity & 0xFFFFFFFFull);\n\
             d.entity_hi = (uint32_t)((entity >> 32) & 0xFFFFFFFFull);\n\
             d.reserved0 = 0u;\n\
             d.callback = callback;\n\
             d.user_data = user_data;\n\
             d.event_kind = BAM_EVENT_KIND_CUSTOM;\n\
             d.reserved1 = 0u;\n\
             d.watched_components = NULL;\n\
             d.watched_component_count = 0u;\n\
             d.mut_callback = NULL;\n\
             d.propagate_relationship = BAM_COMPONENT_INVALID;\n\
             d.reserved2 = 0u;\n\
             return d;\n\
         }\n\n\
         static BAM_UNUSED inline BamTriggerDesc bam_trigger_desc(\n\
             BamComponent event,\n\
             BamEntity entity,\n\
             uint8_t* data_ptr,\n\
             size_t data_len\n\
         ) {\n\
             BamTriggerDesc d;\n\
             d.struct_size = (uint32_t)sizeof(BamTriggerDesc);\n\
             d.flags = 0u;\n\
             d.event = event;\n\
             d.event_kind = BAM_EVENT_KIND_CUSTOM;\n\
             d.entity_lo = (uint32_t)(entity & 0xFFFFFFFFull);\n\
             d.entity_hi = (uint32_t)((entity >> 32) & 0xFFFFFFFFull);\n\
             d.data_ptr = data_ptr;\n\
             d.data_len = data_len;\n\
             d.watched_components = NULL;\n\
             d.watched_component_count = 0u;\n\
             d.propagate_relationship = BAM_COMPONENT_INVALID;\n\
             d.reserved0 = 0u;\n\
             return d;\n\
         }\n\n",
    );

    // 6. Compile-time layout assertions emitted from `specs`
    out.push_str(
        "/* ==========================================================================\n\
         * 6. Auto-Generated Layout Assertions (`sizeof`, `alignof`, `offsetof`)\n\
         * ========================================================================== */\n\n\
         BAM_STATIC_ASSERT(sizeof(BamBool) == 1u, bam_bool_size);\n\
         BAM_STATIC_ASSERT(BAM_ALIGNOF(BamBool) == 1u, bam_bool_align);\n\
         BAM_STATIC_ASSERT(sizeof(BamComponent) == 4u, bam_component_size);\n\
         BAM_STATIC_ASSERT(BAM_ALIGNOF(BamComponent) == 4u, bam_component_align);\n\
         BAM_STATIC_ASSERT(sizeof(BamSystem) == 4u, bam_system_size);\n\
         BAM_STATIC_ASSERT(BAM_ALIGNOF(BamSystem) == 4u, bam_system_align);\n\
         BAM_STATIC_ASSERT(sizeof(BamSystemSet) == 4u, bam_system_set_size);\n\
         BAM_STATIC_ASSERT(BAM_ALIGNOF(BamSystemSet) == 4u, bam_system_set_align);\n\
         BAM_STATIC_ASSERT(sizeof(BamSymbol) == 4u, bam_symbol_size);\n\
         BAM_STATIC_ASSERT(BAM_ALIGNOF(BamSymbol) == 4u, bam_symbol_align);\n\
         BAM_STATIC_ASSERT(sizeof(BamFieldKind) == 4u, bam_field_kind_size);\n\
         BAM_STATIC_ASSERT(BAM_ALIGNOF(BamFieldKind) == 4u, bam_field_kind_align);\n\
         BAM_STATIC_ASSERT(sizeof(BamEntity) == 8u, bam_entity_size);\n\
         BAM_STATIC_ASSERT(sizeof(BamChildOf) == 8u, bam_child_of_size);\n\
         BAM_STATIC_ASSERT(offsetof(BamChildOf, parent) == 0u, bam_child_of_off_parent);\n\
         BAM_STATIC_ASSERT(sizeof(BamResult) == 4u, bam_result_size);\n\
         BAM_STATIC_ASSERT(BAM_ALIGNOF(BamResult) == 4u, bam_result_align);\n\
         BAM_STATIC_ASSERT(sizeof(BamAccessMode) == 4u, bam_access_mode_size);\n\
         BAM_STATIC_ASSERT(BAM_ALIGNOF(BamAccessMode) == 4u, bam_access_mode_align);\n\
         BAM_STATIC_ASSERT(sizeof(BamScheduleId) == 4u, bam_schedule_id_size);\n\
         BAM_STATIC_ASSERT(BAM_ALIGNOF(BamScheduleId) == 4u, bam_schedule_id_align);\n\
         BAM_STATIC_ASSERT(sizeof(BamEventKind) == 4u, bam_event_kind_size);\n\
         BAM_STATIC_ASSERT(BAM_ALIGNOF(BamEventKind) == 4u, bam_event_kind_align);\n\
         BAM_STATIC_ASSERT(sizeof(BamAssetLoadState) == 4u, bam_asset_load_state_size);\n\
         BAM_STATIC_ASSERT(BAM_ALIGNOF(BamAssetLoadState) == 4u, bam_asset_load_state_align);\n\n",
    );

    // 64-bit assertions
    out.push_str("#if UINTPTR_MAX == 0xffffffffffffffffULL\n");
    for spec in &specs {
        let _ = writeln!(
            out,
            "BAM_STATIC_ASSERT(sizeof({}) == {}u, {}_size_64);",
            spec.c_name, spec.size_64, spec.assert_stem
        );
        let _ = writeln!(
            out,
            "BAM_STATIC_ASSERT(BAM_ALIGNOF({}) == {}u, {}_align_64);",
            spec.c_name, spec.align_64, spec.assert_stem
        );
        for field in spec.fields {
            let _ = writeln!(
                out,
                "BAM_STATIC_ASSERT(offsetof({}, {}) == {}u, {}_off_{}_64);",
                spec.c_name, field.c_name, field.offset_64, spec.assert_stem, field.c_name
            );
        }
    }
    out.push_str(
        "BAM_STATIC_ASSERT(BAM_MANIFEST_MIN_V0_2_SIZE == 32u, bam_manifest_min_64);\n\
         BAM_STATIC_ASSERT(BAM_QUERY_DESC_MIN_V0_2_SIZE == 24u, bam_query_desc_min_64);\n\
         BAM_STATIC_ASSERT(BAM_SYSTEM_DESC_MIN_V0_2_SIZE == 104u, bam_system_desc_min_64);\n\
         BAM_STATIC_ASSERT(BAM_OBSERVER_DESC_MIN_V0_2_SIZE == 48u, bam_observer_desc_min_64);\n\
         BAM_STATIC_ASSERT(BAM_TRIGGER_DESC_MIN_V0_2_SIZE == 64u, bam_trigger_desc_min_64);\n\
         BAM_STATIC_ASSERT(BAM_COMPONENT_DESC_MIN_V0_2_SIZE == 64u, bam_component_desc_min_64);\n\
         BAM_STATIC_ASSERT(BAM_COMPONENT_DESC_MIN_V0_3_SIZE == 176u, bam_component_desc_min_v0_3_64);\n\
         BAM_STATIC_ASSERT(BAM_FIELD_DESC_MIN_V0_3_SIZE == 64u, bam_field_desc_min_v0_3_64);\n\
         BAM_STATIC_ASSERT(BAM_REQUIRED_COMPONENT_MIN_V0_3_SIZE == 64u, bam_required_component_min_v0_3_64);\n\
         BAM_STATIC_ASSERT(BAM_RELATION_DESC_MIN_V0_3_SIZE == 56u, bam_relation_desc_min_v0_3_64);\n\
         BAM_STATIC_ASSERT(BAM_LAYOUT_INFO_MIN_V0_3_SIZE == 64u, bam_layout_info_min_v0_3_64);\n\
         BAM_STATIC_ASSERT(BAM_SCHEDULE_SET_DESC_MIN_V0_3_SIZE == 48u, bam_schedule_set_desc_min_v0_3_64);\n\
         BAM_STATIC_ASSERT(BAM_VTABLE_HEADER_MIN_V0_2_SIZE == 40u, bam_vtable_header_min_64);\n\
         BAM_STATIC_ASSERT(BAM_HOST_API_MIN_V0_2_SIZE == 224u, bam_host_api_min_64);\n\
         BAM_STATIC_ASSERT(BAM_HOST_API_MIN_V0_3_SIZE == 552u, bam_host_api_min_v0_3_64);\n\
         BAM_STATIC_ASSERT(BAM_ALIGNOF(BamChildOf) == 8u, bam_child_of_align_64);\n",
    );

    // 32-bit assertions
    out.push_str("#elif UINTPTR_MAX == 0xffffffffUL\n");
    for spec in &specs {
        let _ = writeln!(
            out,
            "BAM_STATIC_ASSERT(sizeof({}) == {}u, {}_size_32);",
            spec.c_name, spec.size_32, spec.assert_stem
        );
        match spec.align_32 {
            Align32::Fixed(align) => {
                let _ = writeln!(
                    out,
                    "BAM_STATIC_ASSERT(BAM_ALIGNOF({}) == {}u, {}_align_32);",
                    spec.c_name, align, spec.assert_stem
                );
            }
            Align32::U64 => {
                let _ = writeln!(
                    out,
                    "BAM_STATIC_ASSERT(BAM_ALIGNOF({}) == BAM_ALIGNOF(uint64_t), {}_align_32);",
                    spec.c_name, spec.assert_stem
                );
            }
        }
        for field in spec.fields {
            let _ = writeln!(
                out,
                "BAM_STATIC_ASSERT(offsetof({}, {}) == {}u, {}_off_{}_32);",
                spec.c_name, field.c_name, field.offset_32, spec.assert_stem, field.c_name
            );
        }
    }
    out.push_str(
        "BAM_STATIC_ASSERT(BAM_MANIFEST_MIN_V0_2_SIZE == 24u, bam_manifest_min_32);\n\
         BAM_STATIC_ASSERT(BAM_QUERY_DESC_MIN_V0_2_SIZE == 16u, bam_query_desc_min_32);\n\
         BAM_STATIC_ASSERT(BAM_SYSTEM_DESC_MIN_V0_2_SIZE == 60u, bam_system_desc_min_32);\n\
         BAM_STATIC_ASSERT(BAM_OBSERVER_DESC_MIN_V0_2_SIZE == 36u, bam_observer_desc_min_32);\n\
         BAM_STATIC_ASSERT(BAM_TRIGGER_DESC_MIN_V0_2_SIZE == 48u, bam_trigger_desc_min_32);\n\
         BAM_STATIC_ASSERT(BAM_COMPONENT_DESC_MIN_V0_2_SIZE == 44u, bam_component_desc_min_32);\n\
         BAM_STATIC_ASSERT(BAM_COMPONENT_DESC_MIN_V0_3_SIZE == 104u, bam_component_desc_min_v0_3_32);\n\
         BAM_STATIC_ASSERT(BAM_FIELD_DESC_MIN_V0_3_SIZE == 36u, bam_field_desc_min_v0_3_32);\n\
         BAM_STATIC_ASSERT(BAM_REQUIRED_COMPONENT_MIN_V0_3_SIZE == 48u, bam_required_component_min_v0_3_32);\n\
         BAM_STATIC_ASSERT(BAM_RELATION_DESC_MIN_V0_3_SIZE == 44u, bam_relation_desc_min_v0_3_32);\n\
         BAM_STATIC_ASSERT(BAM_LAYOUT_INFO_MIN_V0_3_SIZE == 48u, bam_layout_info_min_v0_3_32);\n\
         BAM_STATIC_ASSERT(BAM_SCHEDULE_SET_DESC_MIN_V0_3_SIZE == 36u, bam_schedule_set_desc_min_v0_3_32);\n\
         BAM_STATIC_ASSERT(BAM_VTABLE_HEADER_MIN_V0_2_SIZE == 32u, bam_vtable_header_min_32);\n\
         BAM_STATIC_ASSERT(BAM_HOST_API_MIN_V0_2_SIZE == 116u, bam_host_api_min_32);\n\
         BAM_STATIC_ASSERT(BAM_HOST_API_MIN_V0_3_SIZE == 280u, bam_host_api_min_v0_3_32);\n",
    );
    out.push_str("#endif\n\n");

    out.push_str(
        "#ifdef __cplusplus\n\
         }\n\
         #endif\n\n\
         #endif /* BEVY_ABI_MOD_H */\n",
    );

    Ok(out)
}

/// Writes (or verifies with `check_only`) `include/bevy_abi_mod.h` deterministically.
pub fn write_or_check_header(root: &Path, check_only: bool) -> Result<(), String> {
    let rendered = render_c99_header()?;
    let include_dir = root.join("include");
    let header_path = include_dir.join("bevy_abi_mod.h");

    if check_only {
        let existing = fs::read_to_string(&header_path)
            .map_err(|e| format!("failed to read {}: {e}", header_path.display()))?
            .replace("\r\n", "\n");
        if existing != rendered {
            return Err(format!(
                "{} is out of date with crates/bevy_abi_mod_sys (run `cargo xtask header`)",
                header_path.display()
            ));
        }
        return Ok(());
    }

    fs::create_dir_all(&include_dir)
        .map_err(|e| format!("failed to create {}: {e}", include_dir.display()))?;

    let up_to_date = fs::read_to_string(&header_path)
        .map(|existing| existing.replace("\r\n", "\n") == rendered)
        .unwrap_or(false);
    if !up_to_date {
        fs::write(&header_path, rendered)
            .map_err(|e| format!("failed to write {}: {e}", header_path.display()))?;
        println!("==> Generated {}", header_path.display());
    } else {
        println!("==> {} is up to date", header_path.display());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_generation_is_deterministic_and_bam_prefixed() {
        let first = render_c99_header().expect("first render must succeed");
        let second = render_c99_header().expect("second render must succeed");
        assert_eq!(first, second);
        assert!(first.contains("#define BAM_ABI_VERSION 0x00030000u"));
        assert!(first.contains("#define BAM_HOST_API_SLOT_COUNT    68u"));
        assert!(first.contains("define BAM_EXPORT"));
        assert!(first.contains("__declspec(dllexport)"));
        assert!(first.contains(
            "#define BAM_HOST_API_OFFSET_LOG_DIAGNOSTIC offsetof(BamHostApi, log_diagnostic)"
        ));
        assert!(first.contains("#define BAM_HOST_API_OFFSET_MIGRATE_COMPONENT_COLUMN offsetof(BamHostApi, migrate_component_column)"));
        assert!(first.contains("typedef struct BamHostApi   BamHostApi;"));
        assert!(
            first.contains("BAM_STATIC_ASSERT(sizeof(BamHostApi) == 552u, bam_host_api_size_64);")
        );
        assert!(
            first.contains("BAM_STATIC_ASSERT(sizeof(BamHostApi) == 280u, bam_host_api_size_32);")
        );
        assert!(first.contains(
            "BAM_STATIC_ASSERT(sizeof(BamComponentDesc) == 176u, bam_component_desc_size_64);"
        ));
        assert!(first.contains(
            "BAM_STATIC_ASSERT(sizeof(BamComponentDesc) == 104u, bam_component_desc_size_32);"
        ));
        assert!(first.contains("BAM_STATIC_ASSERT(BAM_HOST_API_MIN_V0_2_SIZE == 224u"));
        assert!(first.contains("BAM_STATIC_ASSERT(BAM_HOST_API_MIN_V0_3_SIZE == 552u"));
        // Verify old v0.1 unprefixed C typedefs are completely absent:
        assert!(!first.contains("typedef struct HostApi"));
        assert!(!first.contains("SystemDescriptor"));
        assert!(!first.contains("RawChunkIter"));
        assert!(!first.contains("ScheduleLabelId"));

        // Golden cross-language UUID byte vectors checked identically in `tests/standalone_c_test.c`:
        assert_eq!(
            ComponentUuid::from_parts("combat_mod::Health", 4, 4, 0).bytes,
            [
                0x45, 0x1c, 0x42, 0x67, 0x54, 0xb2, 0xcd, 0x60, 0xc6, 0x72, 0x1c, 0x1f, 0xf8, 0x89,
                0x07, 0xbd
            ]
        );
        assert_eq!(
            ComponentUuid::from_canonical_name("my_game::Inventory").bytes,
            [
                0xe0, 0x21, 0x62, 0xed, 0x29, 0xac, 0xc1, 0x48, 0x32, 0xa1, 0x71, 0x6c, 0x5b, 0x7b,
                0x9c, 0xcb
            ]
        );
    }
}
