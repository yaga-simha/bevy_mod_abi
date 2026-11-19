/* ============================================================================
 * bql_parity_mod.c — Strict ISO C99 Dynamic Mod for `09_bql_wire_parity`
 * ============================================================================
 * Implements the complete `bevy_bql` ABI `0.3.0` wire contract in strict ISO
 * C99 (`-std=c99 -Wall -Wextra -Wpedantic -Werror` and MSVC `/W4 /WX`):
 *   - Explicit zero-padding `AbiPod` structs + `SCHEMA_FINGERPRINT` + UUID
 *   - Declarative `BamFieldDesc[]` reflection descriptors (`ModReflectPlugin`)
 *   - `OBJECT` (`bind_named_object`) persistent root entity across reloads
 *   - `InternedStr` (`intern_symbol`) monotonic canonical tags
 *   - `RELATION` (`register_relation`, `EXCLUSIVE | CASCADE_DELETE`,
 *     `cmd_insert_relation`, `cmd_remove_relation`)
 *   - `UP ChildOf CASCADE` (`iter_up_cascade`) parent-before-child evaluation
 *   - `PARALLEL CHUNK` (`par_for_each_chunk`) + `PARALLEL READONLY CHUNK`
 *     (`par_for_each_chunk_readonly`) + `iter_count`
 *   - `ATOMIC` transactions (`cmd_checkpoint` + `cmd_rollback` vs commit)
 *   - Decoupled `SCHEDULE` (`register_system_handle`, `configure_schedule_set`
 *     with `BAM_SCHEDULE_SET_FLAG_SEQUENCE_WEAK`, `mount_system_in_schedule`)
 *   - Live v1 -> v2 schema evolution via `migrate_component_column`
 *     (compile with `-DBQL_SCHEMA_V2=1` for v2 DSO)
 * ============================================================================ */

#include "../../../include/bevy_abi_mod.h"
#include <stdlib.h>

#if defined(_MSC_VER)
#  include <intrin.h>
#endif

/* ==========================================================================
 * 1. Compile-Time `BAM_HOST_API_OFFSET_*` Wire Contract Verification
 * ========================================================================== */

#if UINTPTR_MAX == 0xffffffffffffffffULL
BAM_STATIC_ASSERT(BAM_HOST_API_OFFSET_REGISTER_COMPONENT == 32u, bql_off64_register_component);
BAM_STATIC_ASSERT(BAM_HOST_API_OFFSET_RESOLVE_COMPONENT == 40u, bql_off64_resolve_component);
BAM_STATIC_ASSERT(BAM_HOST_API_OFFSET_PAR_FOR_EACH_CHUNK == 224u, bql_off64_par_chunk);
BAM_STATIC_ASSERT(BAM_HOST_API_OFFSET_LOG_DIAGNOSTIC == 408u, bql_off64_log_diag);
BAM_STATIC_ASSERT(BAM_HOST_API_OFFSET_REPORT_GUEST_FAULT == 416u, bql_off64_report_fault);
BAM_STATIC_ASSERT(BAM_HOST_API_OFFSET_RESOLVE_COMPONENT_BY_UUID == 424u, bql_off64_resolve_uuid);
BAM_STATIC_ASSERT(BAM_HOST_API_OFFSET_COMPONENT_LAYOUT_INFO == 432u, bql_off64_layout_info);
BAM_STATIC_ASSERT(BAM_HOST_API_OFFSET_REGISTER_RELATION == 440u, bql_off64_register_rel);
BAM_STATIC_ASSERT(BAM_HOST_API_OFFSET_INTERN_SYMBOL == 448u, bql_off64_intern_sym);
BAM_STATIC_ASSERT(BAM_HOST_API_OFFSET_BIND_NAMED_OBJECT == 456u, bql_off64_bind_obj);
BAM_STATIC_ASSERT(BAM_HOST_API_OFFSET_ITER_COUNT == 464u, bql_off64_iter_count);
BAM_STATIC_ASSERT(BAM_HOST_API_OFFSET_ITER_UP_CASCADE == 472u, bql_off64_iter_up_cascade);
BAM_STATIC_ASSERT(BAM_HOST_API_OFFSET_PAR_FOR_EACH_CHUNK_READONLY == 480u, bql_off64_par_ro);
BAM_STATIC_ASSERT(BAM_HOST_API_OFFSET_REGISTER_SYSTEM_HANDLE == 488u, bql_off64_reg_sys_handle);
BAM_STATIC_ASSERT(BAM_HOST_API_OFFSET_CONFIGURE_SCHEDULE_SET == 496u, bql_off64_cfg_sched_set);
BAM_STATIC_ASSERT(BAM_HOST_API_OFFSET_MOUNT_SYSTEM_IN_SCHEDULE == 504u, bql_off64_mount_sys);
BAM_STATIC_ASSERT(BAM_HOST_API_OFFSET_CMD_CHECKPOINT == 512u, bql_off64_cmd_checkpoint);
BAM_STATIC_ASSERT(BAM_HOST_API_OFFSET_CMD_ROLLBACK == 520u, bql_off64_cmd_rollback);
BAM_STATIC_ASSERT(BAM_HOST_API_OFFSET_CMD_INSERT_RELATION == 528u, bql_off64_cmd_insert_rel);
BAM_STATIC_ASSERT(BAM_HOST_API_OFFSET_CMD_REMOVE_RELATION == 536u, bql_off64_cmd_remove_rel);
BAM_STATIC_ASSERT(BAM_HOST_API_OFFSET_MIGRATE_COMPONENT_COLUMN == 544u, bql_off64_migrate_col);
#else
BAM_STATIC_ASSERT(BAM_HOST_API_OFFSET_REGISTER_RELATION == 224u, bql_off32_register_rel);
BAM_STATIC_ASSERT(BAM_HOST_API_OFFSET_INTERN_SYMBOL == 228u, bql_off32_intern_sym);
BAM_STATIC_ASSERT(BAM_HOST_API_OFFSET_BIND_NAMED_OBJECT == 232u, bql_off32_bind_obj);
BAM_STATIC_ASSERT(BAM_HOST_API_OFFSET_ITER_COUNT == 236u, bql_off32_iter_count);
BAM_STATIC_ASSERT(BAM_HOST_API_OFFSET_ITER_UP_CASCADE == 240u, bql_off32_iter_up_cascade);
BAM_STATIC_ASSERT(BAM_HOST_API_OFFSET_PAR_FOR_EACH_CHUNK_READONLY == 244u, bql_off32_par_ro);
BAM_STATIC_ASSERT(BAM_HOST_API_OFFSET_REGISTER_SYSTEM_HANDLE == 248u, bql_off32_reg_sys_handle);
BAM_STATIC_ASSERT(BAM_HOST_API_OFFSET_CONFIGURE_SCHEDULE_SET == 252u, bql_off32_cfg_sched_set);
BAM_STATIC_ASSERT(BAM_HOST_API_OFFSET_MOUNT_SYSTEM_IN_SCHEDULE == 256u, bql_off32_mount_sys);
BAM_STATIC_ASSERT(BAM_HOST_API_OFFSET_CMD_CHECKPOINT == 260u, bql_off32_cmd_checkpoint);
BAM_STATIC_ASSERT(BAM_HOST_API_OFFSET_CMD_ROLLBACK == 264u, bql_off32_cmd_rollback);
BAM_STATIC_ASSERT(BAM_HOST_API_OFFSET_CMD_INSERT_RELATION == 268u, bql_off32_cmd_insert_rel);
BAM_STATIC_ASSERT(BAM_HOST_API_OFFSET_CMD_REMOVE_RELATION == 272u, bql_off32_cmd_remove_rel);
BAM_STATIC_ASSERT(BAM_HOST_API_OFFSET_MIGRATE_COMPONENT_COLUMN == 276u, bql_off32_migrate_col);
#endif

/* ==========================================================================
 * 2. Constants & Explicit Zero-Padding `AbiPod` Wire Structs
 * ========================================================================== */

#define BQL_NUM_ROOTS         4u
#define BQL_BRANCHES_PER_ROOT 4u
#define BQL_NUM_BRANCHES      (BQL_NUM_ROOTS * BQL_BRANCHES_PER_ROOT)
#define BQL_LEAVES_PER_BRANCH 36u
#define BQL_NUM_LEAVES        (BQL_NUM_BRANCHES * BQL_LEAVES_PER_BRANCH)
#define BQL_STATE_MAGIC       0x42514C5053544154ull
#define BQL_STATE_BLOB_LEN    72u

typedef struct SimClock {
    uint32_t frame;
    uint32_t schema_gen;
    uint64_t checksum_seed;
} SimClock;

BAM_STATIC_ASSERT(sizeof(SimClock) == 16u, bql_sim_clock_size);
BAM_STATIC_ASSERT(BAM_ALIGNOF(SimClock) == 8u, bql_sim_clock_align);

typedef struct FrameAccumulator {
    uint64_t readonly_sum;
    uint64_t readonly_count;
    uint32_t committed_tx_count;
    uint32_t rolled_back_tx_count;
    uint32_t interned_tag_alpha;
    uint32_t interned_tag_beta;
    uint64_t order_trace;
} FrameAccumulator;

BAM_STATIC_ASSERT(sizeof(FrameAccumulator) == 40u, bql_frame_acc_size);
BAM_STATIC_ASSERT(BAM_ALIGNOF(FrameAccumulator) == 8u, bql_frame_acc_align);

typedef struct NodeStateV1 {
    uint32_t node_id;
    int32_t  local_val;
    uint32_t tag_sym;
} NodeStateV1;

BAM_STATIC_ASSERT(sizeof(NodeStateV1) == 12u, bql_node_v1_size);
BAM_STATIC_ASSERT(BAM_ALIGNOF(NodeStateV1) == 4u, bql_node_v1_align);

typedef struct NodeStateV2 {
    uint32_t node_id;
    int32_t  local_val;
    uint32_t tag_sym;
    int32_t  boost;
} NodeStateV2;

BAM_STATIC_ASSERT(sizeof(NodeStateV2) == 16u, bql_node_v2_size);
BAM_STATIC_ASSERT(BAM_ALIGNOF(NodeStateV2) == 4u, bql_node_v2_align);

#ifdef BQL_SCHEMA_V2
typedef NodeStateV2 ActiveNodeState;
#else
typedef NodeStateV1 ActiveNodeState;
#endif

typedef struct RootAura {
    int32_t  effective_val;
    uint32_t aura_tag;
} RootAura;

BAM_STATIC_ASSERT(sizeof(RootAura) == 8u, bql_root_aura_size);
BAM_STATIC_ASSERT(BAM_ALIGNOF(RootAura) == 4u, bql_root_aura_align);

typedef struct WorldMetric {
    int32_t  world_val;
    int32_t  cascade_depth;
    uint32_t parent_tag;
    float    energy;
} WorldMetric;

BAM_STATIC_ASSERT(sizeof(WorldMetric) == 16u, bql_world_metric_size);
BAM_STATIC_ASSERT(BAM_ALIGNOF(WorldMetric) == 4u, bql_world_metric_align);

typedef struct TxToken {
    uint32_t frame_created;
    uint32_t kind_sym;
} TxToken;

BAM_STATIC_ASSERT(sizeof(TxToken) == 8u, bql_tx_token_size);
BAM_STATIC_ASSERT(BAM_ALIGNOF(TxToken) == 4u, bql_tx_token_align);

typedef struct EdgeWeight {
    uint32_t frame;
    int32_t  weight;
} EdgeWeight;

BAM_STATIC_ASSERT(sizeof(EdgeWeight) == 8u, bql_edge_weight_size);
BAM_STATIC_ASSERT(BAM_ALIGNOF(EdgeWeight) == 4u, bql_edge_weight_align);

/* ==========================================================================
 * 3. Portable Atomic Add, FNV-1a Schema Fingerprint & SipHash Field UUID
 * ========================================================================== */

#if defined(_MSC_VER)
static uint64_t bql_atomic_add_u64(volatile uint64_t* dst, uint64_t delta) {
    long long old_val;
    long long new_val;
    do {
        old_val = *(volatile long long*)dst;
        new_val = (long long)((uint64_t)old_val + delta);
    } while (_InterlockedCompareExchange64((volatile long long*)dst, new_val, old_val) != old_val);
    return (uint64_t)old_val;
}
#elif defined(__GNUC__) || defined(__clang__)
static uint64_t bql_atomic_add_u64(volatile uint64_t* dst, uint64_t delta) {
    return __sync_fetch_and_add(dst, delta);
}
#else
static uint64_t bql_atomic_add_u64(volatile uint64_t* dst, uint64_t delta) {
    uint64_t prev = *dst;
    *dst = prev + delta;
    return prev;
}
#endif

static uint64_t bql_fnv1a_bytes(uint64_t h, const uint8_t* bytes, size_t len) {
    size_t i;
    for (i = 0u; i < len; ++i) {
        h ^= (uint64_t)bytes[i];
        h *= 0x00000100000001B3ull;
    }
    return h;
}

static uint64_t bql_fnv1a_u64(uint64_t h, uint64_t v) {
    uint8_t le[8];
    bam_write_le64(le, v);
    return bql_fnv1a_bytes(h, le, 8u);
}

static BamFieldDesc bql_field_desc(
    BamFieldKind kind,
    const char* name,
    const char* type_tag,
    size_t offset,
    size_t size,
    size_t align
) {
    BamFieldDesc f;
    f.struct_size = (uint32_t)sizeof(BamFieldDesc);
    f.kind = kind;
    f.name = bam_str_view(name);
    f.type_tag = bam_str_view(type_tag);
    f.offset = offset;
    f.size = size;
    f.align = align;
    return f;
}

/* Matches `#[derive(ModComponent)]`'s exact 64-bit FNV-1a `SCHEMA_FINGERPRINT`. */
static uint64_t bql_compute_schema_fingerprint(
    const char* canonical_name,
    size_t struct_size,
    size_t struct_align,
    uint32_t component_flags,
    const BamFieldDesc* fields,
    size_t field_count
) {
    uint64_t h = 0xcbf29ce484222325ull;
    size_t i;
    h = bql_fnv1a_bytes(h, (const uint8_t*)canonical_name, strlen(canonical_name));
    h = bql_fnv1a_u64(h, (uint64_t)struct_size);
    h = bql_fnv1a_u64(h, (uint64_t)struct_align);
    h = bql_fnv1a_u64(h, (uint64_t)component_flags);
    for (i = 0u; i < field_count; ++i) {
        h = bql_fnv1a_u64(h, (uint64_t)fields[i].kind);
        h = bql_fnv1a_bytes(h, fields[i].name.ptr, fields[i].name.len);
        h = bql_fnv1a_bytes(h, fields[i].type_tag.ptr, fields[i].type_tag.len);
        h = bql_fnv1a_u64(h, (uint64_t)fields[i].offset);
        h = bql_fnv1a_u64(h, (uint64_t)fields[i].size);
        h = bql_fnv1a_u64(h, (uint64_t)fields[i].align);
    }
    return h;
}

/* Matches `ComponentUuid::hash_field_schema` + `ComponentUuid::from_parts` bit-for-bit. */
static BamComponentUuid bql_compute_pod_uuid(
    const char* canonical_name,
    size_t struct_size,
    size_t struct_align,
    const BamFieldDesc* fields,
    size_t field_count
) {
    uint64_t schema_hash = 0ull;
    size_t i;
    for (i = 0u; i < field_count; ++i) {
        BamSipHasher128 st;
        uint8_t le[8];
        uint8_t digest[16];
        uint8_t sep_fe = 0xFEu;
        uint8_t sep_fd = 0xFDu;

        bam_siphash128_init(&st, BAM_SIPHASH_KEY_0, BAM_SIPHASH_KEY_1);
        bam_write_le64(le, schema_hash);
        bam_siphash128_write(&st, le, 8u);
        bam_siphash128_write(&st, fields[i].name.ptr, fields[i].name.len);
        bam_siphash128_write(&st, &sep_fe, 1u);
        bam_siphash128_write(&st, fields[i].type_tag.ptr, fields[i].type_tag.len);
        bam_siphash128_write(&st, &sep_fd, 1u);
        bam_write_le64(le, (uint64_t)fields[i].offset);
        bam_siphash128_write(&st, le, 8u);
        bam_siphash128_finish(&st, digest);
        schema_hash = bam_read_le64(digest);
    }
    return bam_uuid_from_parts(canonical_name, struct_size, struct_align, schema_hash);
}

static BamComponentDesc bql_make_pod_desc(
    const char* name,
    size_t size,
    size_t align,
    const BamFieldDesc* fields,
    size_t field_count
) {
    BamComponentDesc d = bam_desc_component_with_layout(name, size, align, NULL);
    d.uuid = bql_compute_pod_uuid(name, size, align, fields, field_count);
    d.schema_fingerprint = bql_compute_schema_fingerprint(name, size, align, 0u, fields, field_count);
    d.fields = fields;
    d.field_count = field_count;
    return d;
}

#ifdef BQL_SCHEMA_V2
static int32_t bql_compute_migrated_boost(uint32_t node_id, int32_t local_val) {
    return (local_val / 4) + ((int32_t)(node_id % 64u) * 3) + 7;
}
#endif

static ActiveNodeState bql_make_initial_node(uint32_t node_id, int32_t local_val, uint32_t tag_sym) {
    ActiveNodeState n;
    n.node_id = node_id;
    n.local_val = local_val;
    n.tag_sym = tag_sym;
#ifdef BQL_SCHEMA_V2
    n.boost = bql_compute_migrated_boost(node_id, local_val);
#endif
    return n;
}

/* ==========================================================================
 * 4. Per-World Instance State & Reload Save/Restore Callbacks
 * ========================================================================== */

typedef struct ParityState {
    BamHostApi   api;
    BamComponent comp_clock;
    BamComponent comp_acc;
    BamComponent comp_child_of;
    BamComponent comp_node;
    BamComponent comp_aura;
    BamComponent comp_metric;
    BamComponent comp_tx;
    BamComponent rel_owned_by;

    uint32_t sym_root;
    uint32_t sym_branch;
    uint32_t sym_leaf;
    uint32_t sym_alpha;
    uint32_t sym_beta;
    uint32_t sym_tx_ok;
#ifdef BQL_SCHEMA_V2
    uint32_t sym_v2;
#endif

    uint64_t  bootstrapped;
    BamEntity roots[BQL_NUM_ROOTS];
    BamEntity perm_sat;
    BamEntity active_anchor;
    BamEntity active_sat;
} ParityState;

static BamResult bql_save_state(
    BamInstance* instance,
    BamStateWriterFn writer,
    void* writer_ctx
) {
    const ParityState* st = (const ParityState*)instance;
    uint8_t blob[BQL_STATE_BLOB_LEN];
    size_t i;

    if (st == NULL || writer == NULL) {
        return BAM_RESULT_NULL_POINTER;
    }
    bam_write_le64(blob + 0u, BQL_STATE_MAGIC);
    bam_write_le64(blob + 8u, st->bootstrapped);
    for (i = 0u; i < BQL_NUM_ROOTS; ++i) {
        bam_write_le64(blob + 16u + (i * 8u), st->roots[i]);
    }
    bam_write_le64(blob + 48u, st->perm_sat);
    bam_write_le64(blob + 56u, st->active_anchor);
    bam_write_le64(blob + 64u, st->active_sat);
    return writer(writer_ctx, blob, BQL_STATE_BLOB_LEN);
}

static BamResult bql_restore_state(
    BamInstance* instance,
    const uint8_t* bytes,
    size_t len
) {
    ParityState* st = (ParityState*)instance;
    size_t i;

    if (st == NULL) {
        return BAM_RESULT_NULL_POINTER;
    }
    if (len == 0u) {
        return BAM_RESULT_OK;
    }
    if (bytes == NULL || len != BQL_STATE_BLOB_LEN) {
        return BAM_RESULT_INVALID_ARGUMENT;
    }
    if (bam_read_le64(bytes + 0u) != BQL_STATE_MAGIC) {
        return BAM_RESULT_INVALID_ARGUMENT;
    }
    st->bootstrapped = bam_read_le64(bytes + 8u);
    for (i = 0u; i < BQL_NUM_ROOTS; ++i) {
        st->roots[i] = bam_read_le64(bytes + 16u + (i * 8u));
    }
    st->perm_sat = bam_read_le64(bytes + 48u);
    st->active_anchor = bam_read_le64(bytes + 56u);
    st->active_sat = bam_read_le64(bytes + 64u);
    return BAM_RESULT_OK;
}

static const BamManifest MANIFEST = {
    (uint32_t)sizeof(BamManifest),
    BAM_ABI_VERSION,
    "bql_wire_parity",
#ifdef BQL_SCHEMA_V2
    "2.0.0",
#else
    "1.0.0",
#endif
    0u,
    0u,
    bql_save_state,
    bql_restore_state
};

#ifdef BQL_SCHEMA_V2
static BamResult bql_migrate_node_v1_to_v2(
    const void* src_row,
    void* dst_row,
    void* user_data
) {
    const NodeStateV1* src = (const NodeStateV1*)src_row;
    NodeStateV2* dst = (NodeStateV2*)dst_row;
    (void)user_data;

    if (src == NULL || dst == NULL) {
        return BAM_RESULT_NULL_POINTER;
    }
    dst->node_id = src->node_id;
    dst->local_val = src->local_val;
    dst->tag_sym = src->tag_sym;
    dst->boost = bql_compute_migrated_boost(src->node_id, src->local_val);
    return BAM_RESULT_OK;
}
#endif

/* ==========================================================================
 * 5. Five Deterministic `FixedUpdate` Systems (`SEQUENCE_WEAK` + `SEQUENCE`)
 * ========================================================================== */

static BamResult sys_01_lifecycle_tx_rel(BamSystemCtx* sys_ctx, void* user_data) {
    ParityState* st = (ParityState*)user_data;
    BamIter* res_iter = NULL;
    BamCommands* cmds = NULL;
    const SimClock* clock_ptr = NULL;
    FrameAccumulator* acc = NULL;
    uint32_t frame;
    uint32_t rolled_back_inc = 0u;
    uint32_t committed_inc = 0u;
    BamResult rc;

    if (st == NULL || sys_ctx == NULL) {
        return BAM_RESULT_NULL_POINTER;
    }

    rc = st->api.sys_query(sys_ctx, 0u, &res_iter);
    if (rc != BAM_RESULT_OK) {
        return rc;
    }
    rc = st->api.sys_commands(sys_ctx, &cmds);
    if (rc != BAM_RESULT_OK) {
        return rc;
    }

    clock_ptr = (const SimClock*)(const void*)st->api.res_ptr(res_iter, 0u);
    acc = (FrameAccumulator*)(void*)st->api.res_mut_ptr(res_iter, 1u);
    if (clock_ptr == NULL || acc == NULL) {
        return BAM_RESULT_NOT_FOUND;
    }
    frame = clock_ptr->frame;

    if (st->bootstrapped == 0ull) {
        BamEntity branches[BQL_NUM_BRANCHES];
        const BamHostCtx* host_ctx = st->api.sys_host_ctx(sys_ctx);
        size_t r;
        size_t b;
        size_t l;

        if (host_ctx == NULL) {
            return BAM_RESULT_NULL_POINTER;
        }

        for (r = 0u; r < BQL_NUM_ROOTS; ++r) {
            ActiveNodeState node = bql_make_initial_node(
                (uint32_t)r,
                ((int32_t)r + 1) * 10,
                st->sym_root
            );
            RootAura aura;
            WorldMetric metric;
            BamComponent comps[3];
            const uint8_t* ptrs[3];
            BamEntity ent = BAM_ENTITY_NULL;

            aura.effective_val = 0;
            aura.aura_tag = st->sym_root;
            metric.world_val = 0;
            metric.cascade_depth = 0;
            metric.parent_tag = st->sym_root;
            metric.energy = 0.0f;

            comps[0] = st->comp_node;
            comps[1] = st->comp_aura;
            comps[2] = st->comp_metric;
            ptrs[0] = (const uint8_t*)&node;
            ptrs[1] = (const uint8_t*)&aura;
            ptrs[2] = (const uint8_t*)&metric;

            rc = st->api.cmd_spawn(cmds, comps, ptrs, 3u, &ent);
            if (rc != BAM_RESULT_OK) {
                return rc;
            }
            st->roots[r] = ent;
        }

        for (b = 0u; b < BQL_NUM_BRANCHES; ++b) {
            BamChildOf child_of;
            ActiveNodeState node = bql_make_initial_node(
                (uint32_t)(BQL_NUM_ROOTS + b),
                ((int32_t)b + 1) * 3,
                st->sym_branch
            );
            WorldMetric metric;
            BamComponent comps[3];
            const uint8_t* ptrs[3];
            BamEntity ent = BAM_ENTITY_NULL;

            child_of.parent = st->roots[b / BQL_BRANCHES_PER_ROOT];
            metric.world_val = 0;
            metric.cascade_depth = 1;
            metric.parent_tag = st->sym_root;
            metric.energy = 0.0f;

            comps[0] = st->comp_child_of;
            comps[1] = st->comp_node;
            comps[2] = st->comp_metric;
            ptrs[0] = (const uint8_t*)&child_of;
            ptrs[1] = (const uint8_t*)&node;
            ptrs[2] = (const uint8_t*)&metric;

            rc = st->api.cmd_spawn(cmds, comps, ptrs, 3u, &ent);
            if (rc != BAM_RESULT_OK) {
                return rc;
            }
            branches[b] = ent;
        }

        for (l = 0u; l < BQL_NUM_LEAVES; ++l) {
            BamChildOf child_of;
            uint32_t tag_sym = ((l % 2u) == 0u) ? st->sym_alpha : st->sym_beta;
            ActiveNodeState node = bql_make_initial_node(
                (uint32_t)(BQL_NUM_ROOTS + BQL_NUM_BRANCHES + l),
                (int32_t)(l % 17u) - 8,
                tag_sym
            );
            WorldMetric metric;
            BamComponent comps[3];
            const uint8_t* ptrs[3];
            BamEntity ent = BAM_ENTITY_NULL;

            child_of.parent = branches[l / BQL_LEAVES_PER_BRANCH];
            metric.world_val = 0;
            metric.cascade_depth = 2;
            metric.parent_tag = st->sym_branch;
            metric.energy = 0.0f;

            comps[0] = st->comp_child_of;
            comps[1] = st->comp_node;
            comps[2] = st->comp_metric;
            ptrs[0] = (const uint8_t*)&child_of;
            ptrs[1] = (const uint8_t*)&node;
            ptrs[2] = (const uint8_t*)&metric;

            rc = st->api.cmd_spawn(cmds, comps, ptrs, 3u, &ent);
            if (rc != BAM_RESULT_OK) {
                return rc;
            }
        }

        {
            TxToken perm_tok;
            EdgeWeight init_edge;
            BamComponent comps[1];
            const uint8_t* ptrs[1];
            BamEntity perm_sat = BAM_ENTITY_NULL;

            perm_tok.frame_created = frame;
            perm_tok.kind_sym = st->sym_tx_ok;
            comps[0] = st->comp_tx;
            ptrs[0] = (const uint8_t*)&perm_tok;

            rc = st->api.cmd_spawn(cmds, comps, ptrs, 1u, &perm_sat);
            if (rc != BAM_RESULT_OK) {
                return rc;
            }
            init_edge.frame = frame;
            init_edge.weight = 10;
            rc = st->api.cmd_insert_relation(
                cmds,
                perm_sat,
                st->rel_owned_by,
                st->roots[0],
                &init_edge,
                sizeof(EdgeWeight)
            );
            if (rc != BAM_RESULT_OK) {
                return rc;
            }
            st->perm_sat = perm_sat;
            st->bootstrapped = 1ull;
        }

        /* Bind persistent named `OBJECT` `"bql_parity::root_config"` to Root #0. */
        rc = st->api.bind_named_object(
            host_ctx,
            bam_str_view("bql_parity::root_config"),
            st->roots[0]
        );
        if (rc != BAM_RESULT_OK) {
            return rc;
        }
    } else {
        /* 1. Failing `ATOMIC` transaction -> `cmd_checkpoint` + `cmd_rollback` every frame. */
        {
            size_t cp = 0u;
            TxToken bogus_tok;
            EdgeWeight bogus_edge;
            BamComponent comps[1];
            const uint8_t* ptrs[1];
            BamEntity bogus = BAM_ENTITY_NULL;

            rc = st->api.cmd_checkpoint(cmds, &cp);
            if (rc != BAM_RESULT_OK) {
                return rc;
            }
            bogus_tok.frame_created = frame;
            bogus_tok.kind_sym = st->sym_beta;
            comps[0] = st->comp_tx;
            ptrs[0] = (const uint8_t*)&bogus_tok;

            rc = st->api.cmd_spawn(cmds, comps, ptrs, 1u, &bogus);
            if (rc != BAM_RESULT_OK) {
                return rc;
            }
            bogus_edge.frame = frame;
            bogus_edge.weight = -999;
            rc = st->api.cmd_insert_relation(
                cmds,
                bogus,
                st->rel_owned_by,
                st->roots[0],
                &bogus_edge,
                sizeof(EdgeWeight)
            );
            if (rc != BAM_RESULT_OK) {
                return rc;
            }
            /* Deliberately fail guard and roll back cleanly. */
            rc = st->api.cmd_rollback(cmds, cp);
            if (rc != BAM_RESULT_OK) {
                return rc;
            }
            rolled_back_inc = 1u;
        }

        /* 2. Refresh persistent `EXCLUSIVE` relation target every 10 frames. */
        if ((frame % 10u) == 3u && !bam_entity_is_null(st->perm_sat)) {
            BamEntity target = st->roots[(size_t)((frame / 10u) % BQL_NUM_ROOTS)];
            EdgeWeight edge;
            edge.frame = frame;
            edge.weight = (int32_t)frame + 100;
            rc = st->api.cmd_insert_relation(
                cmds,
                st->perm_sat,
                st->rel_owned_by,
                target,
                &edge,
                sizeof(EdgeWeight)
            );
            if (rc != BAM_RESULT_OK) {
                return rc;
            }
        }

        /* 3. Five-frame transactional relation & cascade-delete lifecycle. */
        if ((frame % 5u) == 0u) {
            size_t cp = 0u;
            TxToken anchor_tok;
            TxToken sat_tok;
            EdgeWeight first_edge;
            EdgeWeight final_edge;
            BamComponent comps[1];
            const uint8_t* ptrs[1];
            BamEntity anchor = BAM_ENTITY_NULL;
            BamEntity sat = BAM_ENTITY_NULL;

            rc = st->api.cmd_checkpoint(cmds, &cp);
            if (rc != BAM_RESULT_OK) {
                return rc;
            }
            anchor_tok.frame_created = frame;
            anchor_tok.kind_sym = st->sym_alpha;
            comps[0] = st->comp_tx;
            ptrs[0] = (const uint8_t*)&anchor_tok;
            rc = st->api.cmd_spawn(cmds, comps, ptrs, 1u, &anchor);
            if (rc != BAM_RESULT_OK) {
                (void)st->api.cmd_rollback(cmds, cp);
                return rc;
            }

            sat_tok.frame_created = frame;
            sat_tok.kind_sym = st->sym_tx_ok;
            ptrs[0] = (const uint8_t*)&sat_tok;
            rc = st->api.cmd_spawn(cmds, comps, ptrs, 1u, &sat);
            if (rc != BAM_RESULT_OK) {
                (void)st->api.cmd_rollback(cmds, cp);
                return rc;
            }

            first_edge.frame = frame;
            first_edge.weight = (int32_t)frame * 2;
            rc = st->api.cmd_insert_relation(
                cmds,
                sat,
                st->rel_owned_by,
                st->roots[1],
                &first_edge,
                sizeof(EdgeWeight)
            );
            if (rc != BAM_RESULT_OK) {
                (void)st->api.cmd_rollback(cmds, cp);
                return rc;
            }

            final_edge.frame = frame;
            final_edge.weight = (int32_t)frame * 3 + 1;
            rc = st->api.cmd_insert_relation(
                cmds,
                sat,
                st->rel_owned_by,
                anchor,
                &final_edge,
                sizeof(EdgeWeight)
            );
            if (rc != BAM_RESULT_OK) {
                (void)st->api.cmd_rollback(cmds, cp);
                return rc;
            }

            st->active_anchor = anchor;
            st->active_sat = sat;
            committed_inc = 1u;
        } else if ((frame % 5u) == 2u
                   && !bam_entity_is_null(st->active_anchor)
                   && !bam_entity_is_null(st->active_sat)) {
            TxToken extra_tok;
            EdgeWeight extra_edge;
            EdgeWeight reattach_edge;
            BamComponent comps[1];
            const uint8_t* ptrs[1];
            BamEntity extra_sat = BAM_ENTITY_NULL;

            rc = st->api.cmd_remove_relation(
                cmds,
                st->active_sat,
                st->rel_owned_by,
                st->active_anchor
            );
            if (rc != BAM_RESULT_OK) {
                return rc;
            }
            extra_tok.frame_created = frame;
            extra_tok.kind_sym = st->sym_leaf;
            comps[0] = st->comp_tx;
            ptrs[0] = (const uint8_t*)&extra_tok;
            rc = st->api.cmd_spawn(cmds, comps, ptrs, 1u, &extra_sat);
            if (rc != BAM_RESULT_OK) {
                return rc;
            }
            extra_edge.frame = frame;
            extra_edge.weight = 42;
            rc = st->api.cmd_insert_relation(
                cmds,
                extra_sat,
                st->rel_owned_by,
                st->active_anchor,
                &extra_edge,
                sizeof(EdgeWeight)
            );
            if (rc != BAM_RESULT_OK) {
                return rc;
            }
            reattach_edge.frame = frame;
            reattach_edge.weight = (int32_t)frame + 7;
            rc = st->api.cmd_insert_relation(
                cmds,
                st->active_sat,
                st->rel_owned_by,
                st->active_anchor,
                &reattach_edge,
                sizeof(EdgeWeight)
            );
            if (rc != BAM_RESULT_OK) {
                return rc;
            }
        } else if ((frame % 5u) == 4u && !bam_entity_is_null(st->active_anchor)) {
            rc = st->api.cmd_despawn(cmds, st->active_anchor);
            if (rc != BAM_RESULT_OK) {
                return rc;
            }
            st->active_anchor = BAM_ENTITY_NULL;
            st->active_sat = BAM_ENTITY_NULL;
        }
    }

    acc->committed_tx_count += committed_inc;
    acc->rolled_back_tx_count += rolled_back_inc;
    acc->interned_tag_alpha = st->sym_alpha;
    acc->interned_tag_beta = st->sym_beta;
    acc->order_trace = 1ull;
    return BAM_RESULT_OK;
}

typedef struct ParMutCtx {
    const BamHostApi* api;
    uint32_t          frame;
#ifdef BQL_SCHEMA_V2
    uint32_t          sym_v2;
#endif
} ParMutCtx;

static BamResult par_mut_nodes_worker(BamIter* sub_iter, void* user_data) {
    const ParMutCtx* pctx = (const ParMutCtx*)user_data;
    size_t count = 0u;

    if (pctx == NULL || sub_iter == NULL) {
        return BAM_RESULT_NULL_POINTER;
    }
    while (pctx->api->iter_next_chunk(sub_iter, &count) != BAM_FALSE) {
        ActiveNodeState* BAM_RESTRICT nodes =
            (ActiveNodeState*)(void*)pctx->api->iter_column_mutable(sub_iter, 0u);
        size_t i;
        if (nodes == NULL) {
            return BAM_RESULT_TYPE_MISMATCH;
        }
        for (i = 0u; i < count; ++i) {
            int32_t delta = (int32_t)((pctx->frame + nodes[i].node_id) % 11u) - 5;
#ifdef BQL_SCHEMA_V2
            int32_t b_delta = (int32_t)(pctx->frame % 7u) - 3;
            int32_t b_shifted = nodes[i].boost + b_delta + 1000;
            int32_t shifted;
            nodes[i].boost = (b_shifted % 1000) - 500;
            shifted = nodes[i].local_val + delta + (nodes[i].boost / 8) + 2000;
            nodes[i].local_val = (shifted % 2000) - 1000;
            if (pctx->frame == 250u && (nodes[i].node_id % 13u) == 0u) {
                nodes[i].tag_sym = pctx->sym_v2;
            }
#else
            int32_t shifted = nodes[i].local_val + delta + 2000;
            nodes[i].local_val = (shifted % 2000) - 1000;
#endif
        }
    }
    return BAM_RESULT_OK;
}

static BamResult sys_02_par_mutate_nodes(BamSystemCtx* sys_ctx, void* user_data) {
    ParityState* st = (ParityState*)user_data;
    BamIter* iter = NULL;
    BamIter* res_iter = NULL;
    const SimClock* clock_ptr = NULL;
    FrameAccumulator* acc = NULL;
    ParMutCtx pctx;
    BamResult rc;

    if (st == NULL || sys_ctx == NULL) {
        return BAM_RESULT_NULL_POINTER;
    }
    rc = st->api.sys_query(sys_ctx, 0u, &iter);
    if (rc != BAM_RESULT_OK) {
        return rc;
    }
    rc = st->api.sys_query(sys_ctx, 1u, &res_iter);
    if (rc != BAM_RESULT_OK) {
        return rc;
    }
    clock_ptr = (const SimClock*)(const void*)st->api.res_ptr(res_iter, 0u);
    acc = (FrameAccumulator*)(void*)st->api.res_mut_ptr(res_iter, 1u);
    if (clock_ptr == NULL || acc == NULL) {
        return BAM_RESULT_NOT_FOUND;
    }

    pctx.api = &st->api;
    pctx.frame = clock_ptr->frame;
#ifdef BQL_SCHEMA_V2
    pctx.sym_v2 = st->sym_v2;
#endif
    rc = st->api.par_for_each_chunk(iter, par_mut_nodes_worker, &pctx);
    if (rc != BAM_RESULT_OK) {
        return rc;
    }
    acc->order_trace = (acc->order_trace << 4) | 2ull;
    return BAM_RESULT_OK;
}

static BamResult sys_03_update_roots(BamSystemCtx* sys_ctx, void* user_data) {
    ParityState* st = (ParityState*)user_data;
    BamIter* iter = NULL;
    BamIter* res_iter = NULL;
    const SimClock* clock_ptr = NULL;
    FrameAccumulator* acc = NULL;
    uint32_t frame;
    size_t count = 0u;
    BamResult rc;

    if (st == NULL || sys_ctx == NULL) {
        return BAM_RESULT_NULL_POINTER;
    }
    rc = st->api.sys_query(sys_ctx, 0u, &iter);
    if (rc != BAM_RESULT_OK) {
        return rc;
    }
    rc = st->api.sys_query(sys_ctx, 1u, &res_iter);
    if (rc != BAM_RESULT_OK) {
        return rc;
    }
    clock_ptr = (const SimClock*)(const void*)st->api.res_ptr(res_iter, 0u);
    acc = (FrameAccumulator*)(void*)st->api.res_mut_ptr(res_iter, 1u);
    if (clock_ptr == NULL || acc == NULL) {
        return BAM_RESULT_NOT_FOUND;
    }
    frame = clock_ptr->frame;

    while (st->api.iter_next_chunk(iter, &count) != BAM_FALSE) {
        RootAura* BAM_RESTRICT auras =
            (RootAura*)(void*)st->api.iter_column_mutable(iter, 0u);
        const ActiveNodeState* BAM_RESTRICT nodes =
            (const ActiveNodeState*)(const void*)st->api.iter_column_readonly(iter, 1u);
        size_t i;
        if (auras == NULL || nodes == NULL) {
            return BAM_RESULT_TYPE_MISMATCH;
        }
        for (i = 0u; i < count; ++i) {
#ifdef BQL_SCHEMA_V2
            auras[i].effective_val = nodes[i].local_val * 2 + nodes[i].boost + (int32_t)(frame % 13u);
#else
            auras[i].effective_val = nodes[i].local_val * 2 + (int32_t)(frame % 13u);
#endif
            auras[i].aura_tag = nodes[i].tag_sym;
        }
    }

    acc->order_trace = (acc->order_trace << 4) | 3ull;
    return BAM_RESULT_OK;
}

static BamResult sys_04_cascade_hierarchy(BamSystemCtx* sys_ctx, void* user_data) {
    ParityState* st = (ParityState*)user_data;
    BamIter* iter = NULL;
    BamIter* res_iter = NULL;
    const SimClock* clock_ptr = NULL;
    FrameAccumulator* acc = NULL;
    size_t count = 0u;
    BamResult rc;

    if (st == NULL || sys_ctx == NULL) {
        return BAM_RESULT_NULL_POINTER;
    }
    rc = st->api.sys_query(sys_ctx, 0u, &iter);
    if (rc != BAM_RESULT_OK) {
        return rc;
    }
    rc = st->api.sys_query(sys_ctx, 1u, &res_iter);
    if (rc != BAM_RESULT_OK) {
        return rc;
    }
    clock_ptr = (const SimClock*)(const void*)st->api.res_ptr(res_iter, 0u);
    acc = (FrameAccumulator*)(void*)st->api.res_mut_ptr(res_iter, 1u);
    if (clock_ptr == NULL || acc == NULL) {
        return BAM_RESULT_NOT_FOUND;
    }
    (void)clock_ptr;

    while (st->api.iter_next_chunk(iter, &count) != BAM_FALSE) {
        const BamEntity* entities = st->api.iter_entities(iter);
        WorldMetric* BAM_RESTRICT metrics =
            (WorldMetric*)(void*)st->api.iter_column_mutable(iter, 0u);
        const ActiveNodeState* BAM_RESTRICT nodes =
            (const ActiveNodeState*)(const void*)st->api.iter_column_readonly(iter, 1u);
        const RootAura* own_auras =
            (const RootAura*)(const void*)st->api.iter_column_readonly(iter, 2u);
        size_t i;

        if (entities == NULL || metrics == NULL || nodes == NULL) {
            return BAM_RESULT_TYPE_MISMATCH;
        }
        for (i = 0u; i < count; ++i) {
            const void* ancestor_ptr = NULL;
            BamEntity ancestor_owner = BAM_ENTITY_NULL;
            int32_t root_bonus = 0;
            int32_t depth = 0;
            int32_t ancestor_sum = 0;
            uint32_t parent_tag = nodes[i].tag_sym;
            BamEntity cur = entities[i];
            int32_t world_val;
            int32_t energy_steps;

            /* Multi-hop `UP ChildOf CASCADE` for `RootAura` (term 2). */
            rc = st->api.iter_up_cascade(
                iter,
                BAM_COMPONENT_INVALID,
                entities[i],
                2u,
                &ancestor_ptr,
                &ancestor_owner
            );
            if (rc == BAM_RESULT_OK && ancestor_ptr != NULL) {
                root_bonus = ((const RootAura*)ancestor_ptr)->effective_val;
            } else if (rc == BAM_RESULT_NOT_FOUND) {
                root_bonus = (own_auras != NULL) ? own_auras[i].effective_val : 0;
            } else {
                return rc;
            }

            /* Walk upward along `ChildOf` over `NodeState` (term 1). */
            while (depth < 16) {
                ancestor_ptr = NULL;
                ancestor_owner = BAM_ENTITY_NULL;
                rc = st->api.iter_up_cascade(
                    iter,
                    BAM_COMPONENT_INVALID,
                    cur,
                    1u,
                    &ancestor_ptr,
                    &ancestor_owner
                );
                if (rc == BAM_RESULT_OK && ancestor_ptr != NULL) {
                    const ActiveNodeState* ancestor_node = (const ActiveNodeState*)ancestor_ptr;
                    if (depth == 0) {
                        parent_tag = ancestor_node->tag_sym;
                    }
                    ancestor_sum += ancestor_node->local_val;
#ifdef BQL_SCHEMA_V2
                    ancestor_sum += ancestor_node->boost;
#endif
                    depth += 1;
                    cur = ancestor_owner;
                } else if (rc == BAM_RESULT_NOT_FOUND) {
                    break;
                } else {
                    return rc;
                }
            }

#ifdef BQL_SCHEMA_V2
            world_val = root_bonus + ancestor_sum + nodes[i].local_val + nodes[i].boost;
#else
            world_val = root_bonus + ancestor_sum + nodes[i].local_val;
#endif
            energy_steps = (int32_t)(((uint32_t)world_val) & 255u) + depth * 4;

            metrics[i].world_val = world_val;
            metrics[i].cascade_depth = depth;
            metrics[i].parent_tag = parent_tag;
            metrics[i].energy = (float)energy_steps * 0.25f;
        }
    }

    acc->order_trace = (acc->order_trace << 4) | 4ull;
    return BAM_RESULT_OK;
}

typedef struct ParRoCtx {
    const BamHostApi* api;
    volatile uint64_t shared_sum;
} ParRoCtx;

static BamResult par_ro_reduce_worker(BamIter* sub_iter, void* user_data) {
    ParRoCtx* rctx = (ParRoCtx*)user_data;
    size_t count = 0u;
    uint64_t local_sum = 0ull;

    if (rctx == NULL || sub_iter == NULL) {
        return BAM_RESULT_NULL_POINTER;
    }
    while (rctx->api->iter_next_chunk(sub_iter, &count) != BAM_FALSE) {
        const ActiveNodeState* BAM_RESTRICT nodes =
            (const ActiveNodeState*)(const void*)rctx->api->iter_column_readonly(sub_iter, 0u);
        const WorldMetric* BAM_RESTRICT metrics =
            (const WorldMetric*)(const void*)rctx->api->iter_column_readonly(sub_iter, 1u);
        size_t i;

        if (nodes == NULL || metrics == NULL) {
            return BAM_RESULT_TYPE_MISMATCH;
        }
        for (i = 0u; i < count; ++i) {
            uint32_t energy_bits = 0u;
            uint64_t row_val;
            memcpy(&energy_bits, &metrics[i].energy, sizeof(uint32_t));
            row_val = (((uint64_t)nodes[i].node_id) << 32)
                ^ ((uint64_t)(uint32_t)nodes[i].local_val)
                ^ (((uint64_t)(uint32_t)metrics[i].world_val) << 16)
                ^ ((uint64_t)(uint32_t)metrics[i].cascade_depth)
                ^ (((uint64_t)nodes[i].tag_sym) << 8)
                ^ (((uint64_t)metrics[i].parent_tag) << 24)
                ^ ((uint64_t)energy_bits);
#ifdef BQL_SCHEMA_V2
            row_val ^= (((uint64_t)(uint32_t)nodes[i].boost) << 40);
#endif
            local_sum += row_val;
        }
    }
    (void)bql_atomic_add_u64(&rctx->shared_sum, local_sum);
    return BAM_RESULT_OK;
}

static BamResult sys_05_par_readonly_reduce(BamSystemCtx* sys_ctx, void* user_data) {
    ParityState* st = (ParityState*)user_data;
    BamIter* iter = NULL;
    BamIter* res_iter = NULL;
    FrameAccumulator* acc = NULL;
    size_t total_count = 0u;
    ParRoCtx rctx;
    BamResult rc;

    if (st == NULL || sys_ctx == NULL) {
        return BAM_RESULT_NULL_POINTER;
    }
    rc = st->api.sys_query(sys_ctx, 0u, &iter);
    if (rc != BAM_RESULT_OK) {
        return rc;
    }
    rc = st->api.sys_query(sys_ctx, 1u, &res_iter);
    if (rc != BAM_RESULT_OK) {
        return rc;
    }
    acc = (FrameAccumulator*)(void*)st->api.res_mut_ptr(res_iter, 0u);
    if (acc == NULL) {
        return BAM_RESULT_NOT_FOUND;
    }

    rc = st->api.iter_count(iter, &total_count);
    if (rc != BAM_RESULT_OK) {
        return rc;
    }

    rctx.api = &st->api;
    rctx.shared_sum = 0ull;
    rc = st->api.par_for_each_chunk_readonly(iter, par_ro_reduce_worker, &rctx);
    if (rc != BAM_RESULT_OK) {
        return rc;
    }

    acc->readonly_sum = rctx.shared_sum;
    acc->readonly_count = (uint64_t)total_count;
    acc->order_trace = (acc->order_trace << 4) | 5ull;
    return BAM_RESULT_OK;
}

/* ==========================================================================
 * 6. Registration Helpers & Mandatory Guest Entry Points
 * ========================================================================== */

static BamResult register_system_with_queries(
    ParityState* st,
    BamLoadCtx* load_ctx,
    const char* name,
    const BamQueryDesc* queries,
    size_t query_count,
    BamSystemFn cb,
    BamSystem* out_sys
) {
    BamSystemDesc desc;
    memset(&desc, 0, sizeof(desc));
    desc.struct_size = (uint32_t)sizeof(BamSystemDesc);
    desc.name = name;
    desc.queries = queries;
    desc.query_count = query_count;
    desc.callback = cb;
    desc.user_data = st;
    desc.schedule = BAM_SCHEDULE_FIXED_UPDATE;
    return st->api.register_system_handle(load_ctx, &desc, out_sys);
}

static BamResult intern_u32(
    const ParityState* st,
    const BamHostCtx* host_ctx,
    const char* text,
    uint32_t* out_val
) {
    BamSymbol sym = BAM_SYMBOL_INVALID;
    BamResult rc = st->api.intern_symbol(host_ctx, bam_str_view(text), &sym);
    if (rc != BAM_RESULT_OK) {
        return rc;
    }
    *out_val = sym;
    return BAM_RESULT_OK;
}

BAM_EXPORT const BamManifest* bevy_abi_mod_manifest(void) {
    return &MANIFEST;
}

BAM_EXPORT BamResult bevy_abi_mod_init(
    const BamHostApi* host_api,
    BamLoadCtx*       load_ctx,
    BamInstance**     instance_out
) {
    BamHostApi local_api;
    const BamHostCtx* host_ctx = NULL;
    ParityState* st = NULL;
    BamResult rc;

    /* Declarative ABI `0.3.0` `BamFieldDesc` metadata tables matching Rust derives. */
    BamFieldDesc node_v1_fields[3];
#ifdef BQL_SCHEMA_V2
    BamFieldDesc node_v2_fields[4];
#endif
    BamFieldDesc aura_fields[2];
    BamFieldDesc metric_fields[4];
    BamFieldDesc tx_fields[2];
    BamComponentDesc node_v1_desc;
#ifdef BQL_SCHEMA_V2
    BamComponentDesc node_v2_desc;
#endif
    BamComponentDesc aura_desc;
    BamComponentDesc metric_desc;
    BamComponentDesc tx_desc;
    BamRelationDesc rel_desc;
    BamLayoutInfo layout_info;
    BamComponent resolved_node = BAM_COMPONENT_INVALID;

    BamSystemSet set_pipeline = BAM_SYSTEM_SET_INVALID;
    BamSystemSet set_finalize = BAM_SYSTEM_SET_INVALID;
    BamScheduleSetDesc pipeline_desc;
    BamScheduleSetDesc finalize_desc;
    BamSystem sys_handles[5];

    if (load_ctx == NULL || instance_out == NULL) {
        return BAM_RESULT_NULL_POINTER;
    }
    *instance_out = NULL;

    rc = bam_host_api_copy_bounded(&local_api, sizeof(local_api), host_api);
    if (rc != BAM_RESULT_OK) {
        return rc;
    }
    if ((size_t)host_api->struct_size < BAM_HOST_API_MIN_V0_3_SIZE) {
        return BAM_RESULT_INVALID_VERSION;
    }
    if (local_api.load_host_ctx == NULL ||
        local_api.register_component == NULL ||
        local_api.resolve_component == NULL ||
        local_api.resolve_component_by_uuid == NULL ||
        local_api.component_layout_info == NULL ||
        local_api.register_relation == NULL ||
        local_api.intern_symbol == NULL ||
        local_api.bind_named_object == NULL ||
        local_api.iter_count == NULL ||
        local_api.iter_up_cascade == NULL ||
        local_api.par_for_each_chunk == NULL ||
        local_api.par_for_each_chunk_readonly == NULL ||
        local_api.register_system_handle == NULL ||
        local_api.configure_schedule_set == NULL ||
        local_api.mount_system_in_schedule == NULL ||
        local_api.cmd_checkpoint == NULL ||
        local_api.cmd_rollback == NULL ||
        local_api.cmd_insert_relation == NULL ||
        local_api.cmd_remove_relation == NULL ||
        local_api.migrate_component_column == NULL) {
        return BAM_RESULT_UNSUPPORTED;
    }

    st = (ParityState*)calloc(1u, sizeof(ParityState));
    if (st == NULL) {
        return BAM_RESULT_INTERNAL_ERROR;
    }
    st->api = local_api;

    host_ctx = st->api.load_host_ctx(load_ctx);
    if (host_ctx == NULL) {
        free(st);
        return BAM_RESULT_NULL_POINTER;
    }

    /* Resolve host-registered resources and `ChildOf`. */
    rc = st->api.resolve_component(host_ctx, "bql_parity::SimClock", NULL, &st->comp_clock);
    if (rc != BAM_RESULT_OK) { free(st); return rc; }
    rc = st->api.resolve_component(host_ctx, "bql_parity::FrameAccumulator", NULL, &st->comp_acc);
    if (rc != BAM_RESULT_OK) { free(st); return rc; }
    rc = st->api.resolve_component(host_ctx, BAM_CHILD_OF_NAME, NULL, &st->comp_child_of);
    if (rc != BAM_RESULT_OK) { free(st); return rc; }

    /* Build `BamFieldDesc` arrays & `ComponentDesc`s. */
    node_v1_fields[0] = bql_field_desc(BAM_FIELD_KIND_U32, "node_id", "u32", offsetof(NodeStateV1, node_id), sizeof(uint32_t), BAM_ALIGNOF(uint32_t));
    node_v1_fields[1] = bql_field_desc(BAM_FIELD_KIND_I32, "local_val", "i32", offsetof(NodeStateV1, local_val), sizeof(int32_t), BAM_ALIGNOF(int32_t));
    node_v1_fields[2] = bql_field_desc(BAM_FIELD_KIND_U32, "tag_sym", "u32", offsetof(NodeStateV1, tag_sym), sizeof(uint32_t), BAM_ALIGNOF(uint32_t));
    node_v1_desc = bql_make_pod_desc("bql_parity::NodeState", sizeof(NodeStateV1), BAM_ALIGNOF(NodeStateV1), node_v1_fields, 3u);

#ifdef BQL_SCHEMA_V2
    node_v2_fields[0] = bql_field_desc(BAM_FIELD_KIND_U32, "node_id", "u32", offsetof(NodeStateV2, node_id), sizeof(uint32_t), BAM_ALIGNOF(uint32_t));
    node_v2_fields[1] = bql_field_desc(BAM_FIELD_KIND_I32, "local_val", "i32", offsetof(NodeStateV2, local_val), sizeof(int32_t), BAM_ALIGNOF(int32_t));
    node_v2_fields[2] = bql_field_desc(BAM_FIELD_KIND_U32, "tag_sym", "u32", offsetof(NodeStateV2, tag_sym), sizeof(uint32_t), BAM_ALIGNOF(uint32_t));
    node_v2_fields[3] = bql_field_desc(BAM_FIELD_KIND_I32, "boost", "i32", offsetof(NodeStateV2, boost), sizeof(int32_t), BAM_ALIGNOF(int32_t));
    node_v2_desc = bql_make_pod_desc("bql_parity::NodeState", sizeof(NodeStateV2), BAM_ALIGNOF(NodeStateV2), node_v2_fields, 4u);

    /* If v1 `NodeState` already exists in the live `World`, migrate its column in-place to v2! */
    rc = st->api.resolve_component(host_ctx, "bql_parity::NodeState", NULL, &st->comp_node);
    if (rc == BAM_RESULT_OK) {
        memset(&layout_info, 0, sizeof(layout_info));
        rc = st->api.component_layout_info(host_ctx, st->comp_node, &layout_info);
        if (rc != BAM_RESULT_OK) { free(st); return rc; }
        if (layout_info.schema_fingerprint == node_v1_desc.schema_fingerprint) {
            rc = st->api.migrate_component_column(
                load_ctx,
                st->comp_node,
                node_v1_desc.schema_fingerprint,
                &node_v2_desc,
                bql_migrate_node_v1_to_v2,
                NULL
            );
            if (rc != BAM_RESULT_OK) { free(st); return rc; }
        } else if (layout_info.schema_fingerprint == node_v2_desc.schema_fingerprint) {
            rc = st->api.register_component(load_ctx, &node_v2_desc, &st->comp_node);
            if (rc != BAM_RESULT_OK) { free(st); return rc; }
        } else {
            free(st);
            return BAM_RESULT_TYPE_MISMATCH;
        }
    } else if (rc == BAM_RESULT_NOT_FOUND) {
        rc = st->api.register_component(load_ctx, &node_v2_desc, &st->comp_node);
        if (rc != BAM_RESULT_OK) { free(st); return rc; }
    } else {
        free(st);
        return rc;
    }

    rc = st->api.resolve_component_by_uuid(host_ctx, &node_v2_desc.uuid, &resolved_node);
    if (rc != BAM_RESULT_OK || resolved_node != st->comp_node) {
        free(st);
        return BAM_RESULT_TYPE_MISMATCH;
    }
#else
    rc = st->api.register_component(load_ctx, &node_v1_desc, &st->comp_node);
    if (rc != BAM_RESULT_OK) { free(st); return rc; }
    rc = st->api.resolve_component_by_uuid(host_ctx, &node_v1_desc.uuid, &resolved_node);
    if (rc != BAM_RESULT_OK || resolved_node != st->comp_node) {
        free(st);
        return BAM_RESULT_TYPE_MISMATCH;
    }
#endif

    memset(&layout_info, 0, sizeof(layout_info));
    rc = st->api.component_layout_info(host_ctx, st->comp_node, &layout_info);
    if (rc != BAM_RESULT_OK || layout_info.size != sizeof(ActiveNodeState)) {
        free(st);
        return BAM_RESULT_TYPE_MISMATCH;
    }

    aura_fields[0] = bql_field_desc(BAM_FIELD_KIND_I32, "effective_val", "i32", offsetof(RootAura, effective_val), sizeof(int32_t), BAM_ALIGNOF(int32_t));
    aura_fields[1] = bql_field_desc(BAM_FIELD_KIND_U32, "aura_tag", "u32", offsetof(RootAura, aura_tag), sizeof(uint32_t), BAM_ALIGNOF(uint32_t));
    aura_desc = bql_make_pod_desc("bql_parity::RootAura", sizeof(RootAura), BAM_ALIGNOF(RootAura), aura_fields, 2u);
    rc = st->api.register_component(load_ctx, &aura_desc, &st->comp_aura);
    if (rc != BAM_RESULT_OK) { free(st); return rc; }

    metric_fields[0] = bql_field_desc(BAM_FIELD_KIND_I32, "world_val", "i32", offsetof(WorldMetric, world_val), sizeof(int32_t), BAM_ALIGNOF(int32_t));
    metric_fields[1] = bql_field_desc(BAM_FIELD_KIND_I32, "cascade_depth", "i32", offsetof(WorldMetric, cascade_depth), sizeof(int32_t), BAM_ALIGNOF(int32_t));
    metric_fields[2] = bql_field_desc(BAM_FIELD_KIND_U32, "parent_tag", "u32", offsetof(WorldMetric, parent_tag), sizeof(uint32_t), BAM_ALIGNOF(uint32_t));
    metric_fields[3] = bql_field_desc(BAM_FIELD_KIND_F32, "energy", "f32", offsetof(WorldMetric, energy), sizeof(float), BAM_ALIGNOF(float));
    metric_desc = bql_make_pod_desc("bql_parity::WorldMetric", sizeof(WorldMetric), BAM_ALIGNOF(WorldMetric), metric_fields, 4u);
    rc = st->api.register_component(load_ctx, &metric_desc, &st->comp_metric);
    if (rc != BAM_RESULT_OK) { free(st); return rc; }

    tx_fields[0] = bql_field_desc(BAM_FIELD_KIND_U32, "frame_created", "u32", offsetof(TxToken, frame_created), sizeof(uint32_t), BAM_ALIGNOF(uint32_t));
    tx_fields[1] = bql_field_desc(BAM_FIELD_KIND_U32, "kind_sym", "u32", offsetof(TxToken, kind_sym), sizeof(uint32_t), BAM_ALIGNOF(uint32_t));
    tx_desc = bql_make_pod_desc("bql_parity::TxToken", sizeof(TxToken), BAM_ALIGNOF(TxToken), tx_fields, 2u);
    rc = st->api.register_component(load_ctx, &tx_desc, &st->comp_tx);
    if (rc != BAM_RESULT_OK) { free(st); return rc; }

    /* Register typed `RELATION` (`EXCLUSIVE | CASCADE_DELETE`) with 8-byte `EdgeWeight` payload. */
    memset(&rel_desc, 0, sizeof(rel_desc));
    rel_desc.struct_size = (uint32_t)sizeof(BamRelationDesc);
    rel_desc.flags = BAM_RELATION_FLAG_EXCLUSIVE | BAM_RELATION_FLAG_CASCADE_DELETE;
    rel_desc.name = "bql_parity::OwnedBy";
    rel_desc.uuid = bam_uuid_from_parts("bql_parity::OwnedBy", sizeof(EdgeWeight), BAM_ALIGNOF(EdgeWeight), 0ull);
    rel_desc.data_size = sizeof(EdgeWeight);
    rel_desc.data_align = BAM_ALIGNOF(EdgeWeight);
    rel_desc.max_degree = 1u;
    rc = st->api.register_relation(load_ctx, &rel_desc, &st->rel_owned_by);
    if (rc != BAM_RESULT_OK) { free(st); return rc; }

    /* Intern canonical `InternedStr` symbols monotonically in identical order to Safe Rust. */
    rc = intern_u32(st, host_ctx, "bql::tag::root", &st->sym_root);
    if (rc != BAM_RESULT_OK) { free(st); return rc; }
    rc = intern_u32(st, host_ctx, "bql::tag::branch", &st->sym_branch);
    if (rc != BAM_RESULT_OK) { free(st); return rc; }
    rc = intern_u32(st, host_ctx, "bql::tag::leaf", &st->sym_leaf);
    if (rc != BAM_RESULT_OK) { free(st); return rc; }
    rc = intern_u32(st, host_ctx, "bql::tag::alpha", &st->sym_alpha);
    if (rc != BAM_RESULT_OK) { free(st); return rc; }
    rc = intern_u32(st, host_ctx, "bql::tag::beta", &st->sym_beta);
    if (rc != BAM_RESULT_OK) { free(st); return rc; }
    rc = intern_u32(st, host_ctx, "bql::tx::committed", &st->sym_tx_ok);
    if (rc != BAM_RESULT_OK) { free(st); return rc; }
#ifdef BQL_SCHEMA_V2
    rc = intern_u32(st, host_ctx, "bql::tag::v2_boosted", &st->sym_v2);
    if (rc != BAM_RESULT_OK) { free(st); return rc; }
#endif

    /* Configure decoupled `FixedUpdate` schedule sets (`SEQUENCE_WEAK` -> `SEQUENCE`). */
    memset(&pipeline_desc, 0, sizeof(pipeline_desc));
    pipeline_desc.struct_size = (uint32_t)sizeof(BamScheduleSetDesc);
    pipeline_desc.flags = BAM_SCHEDULE_SET_FLAG_SEQUENCE_WEAK;
    pipeline_desc.schedule = BAM_SCHEDULE_FIXED_UPDATE;
    pipeline_desc.name = "bql_pipeline";
    rc = st->api.configure_schedule_set(load_ctx, &pipeline_desc, &set_pipeline);
    if (rc != BAM_RESULT_OK) { free(st); return rc; }

    memset(&finalize_desc, 0, sizeof(finalize_desc));
    finalize_desc.struct_size = (uint32_t)sizeof(BamScheduleSetDesc);
    finalize_desc.flags = BAM_SCHEDULE_SET_FLAG_SEQUENCE;
    finalize_desc.schedule = BAM_SCHEDULE_FIXED_UPDATE;
    finalize_desc.after_count = 1u;
    finalize_desc.name = "bql_finalize";
    finalize_desc.after_sets = &set_pipeline;
    rc = st->api.configure_schedule_set(load_ctx, &finalize_desc, &set_finalize);
    if (rc != BAM_RESULT_OK) { free(st); return rc; }

    /* Register all 5 system handles via `register_system_handle`. */
    {
        BamTermDesc res_rw[2];
        BamQueryDesc q_res;
        res_rw[0] = bam_term_desc(st->comp_clock, BAM_ACCESS_RESOURCE_READ, 0u);
        res_rw[1] = bam_term_desc(st->comp_acc, BAM_ACCESS_RESOURCE_WRITE, 0u);
        memset(&q_res, 0, sizeof(q_res));
        q_res.struct_size = (uint32_t)sizeof(BamQueryDesc);
        q_res.terms = res_rw;
        q_res.term_count = 2u;

        rc = register_system_with_queries(
            st, load_ctx, "sys_01_lifecycle_tx_rel", &q_res, 1u,
            sys_01_lifecycle_tx_rel, &sys_handles[0]
        );
        if (rc != BAM_RESULT_OK) { free(st); return rc; }

        {
            BamTermDesc s2_terms[1];
            BamQueryDesc s2_queries[2];
            s2_terms[0] = bam_term_desc(st->comp_node, BAM_ACCESS_WRITE, 0u);
            memset(s2_queries, 0, sizeof(s2_queries));
            s2_queries[0].struct_size = (uint32_t)sizeof(BamQueryDesc);
            s2_queries[0].terms = s2_terms;
            s2_queries[0].term_count = 1u;
            s2_queries[1] = q_res;
            rc = register_system_with_queries(
                st, load_ctx, "sys_02_par_mutate_nodes", s2_queries, 2u,
                sys_02_par_mutate_nodes, &sys_handles[1]
            );
            if (rc != BAM_RESULT_OK) { free(st); return rc; }
        }

        {
            BamTermDesc s3_terms[2];
            BamQueryDesc s3_queries[2];
            s3_terms[0] = bam_term_desc(st->comp_aura, BAM_ACCESS_WRITE, 0u);
            s3_terms[1] = bam_term_desc(st->comp_node, BAM_ACCESS_READ, 0u);
            memset(s3_queries, 0, sizeof(s3_queries));
            s3_queries[0].struct_size = (uint32_t)sizeof(BamQueryDesc);
            s3_queries[0].terms = s3_terms;
            s3_queries[0].term_count = 2u;
            s3_queries[1] = q_res;
            rc = register_system_with_queries(
                st, load_ctx, "sys_03_update_roots", s3_queries, 2u,
                sys_03_update_roots, &sys_handles[2]
            );
            if (rc != BAM_RESULT_OK) { free(st); return rc; }
        }

        {
            BamTermDesc s4_terms[3];
            BamQueryDesc s4_queries[2];
            s4_terms[0] = bam_term_desc(st->comp_metric, BAM_ACCESS_WRITE, 0u);
            s4_terms[1] = bam_term_desc(st->comp_node, BAM_ACCESS_READ, 0u);
            s4_terms[2] = bam_term_desc(st->comp_aura, BAM_ACCESS_OPTIONAL_READ, 0u);
            memset(s4_queries, 0, sizeof(s4_queries));
            s4_queries[0].struct_size = (uint32_t)sizeof(BamQueryDesc);
            s4_queries[0].terms = s4_terms;
            s4_queries[0].term_count = 3u;
            s4_queries[1] = q_res;
            rc = register_system_with_queries(
                st, load_ctx, "sys_04_cascade_hierarchy", s4_queries, 2u,
                sys_04_cascade_hierarchy, &sys_handles[3]
            );
            if (rc != BAM_RESULT_OK) { free(st); return rc; }
        }

        {
            BamTermDesc s5_terms[2];
            BamTermDesc s5_res[1];
            BamQueryDesc s5_queries[2];
            s5_terms[0] = bam_term_desc(st->comp_node, BAM_ACCESS_READ, 0u);
            s5_terms[1] = bam_term_desc(st->comp_metric, BAM_ACCESS_READ, 0u);
            s5_res[0] = bam_term_desc(st->comp_acc, BAM_ACCESS_RESOURCE_WRITE, 0u);
            memset(s5_queries, 0, sizeof(s5_queries));
            s5_queries[0].struct_size = (uint32_t)sizeof(BamQueryDesc);
            s5_queries[0].terms = s5_terms;
            s5_queries[0].term_count = 2u;
            s5_queries[1].struct_size = (uint32_t)sizeof(BamQueryDesc);
            s5_queries[1].terms = s5_res;
            s5_queries[1].term_count = 1u;
            rc = register_system_with_queries(
                st, load_ctx, "sys_05_par_readonly_reduce", s5_queries, 2u,
                sys_05_par_readonly_reduce, &sys_handles[4]
            );
            if (rc != BAM_RESULT_OK) { free(st); return rc; }
        }
    }

    /* Mount systems into `bql_pipeline` (0..3) and `bql_finalize` (4). */
    {
        size_t s;
        for (s = 0u; s < 4u; ++s) {
            rc = st->api.mount_system_in_schedule(
                load_ctx,
                sys_handles[s],
                BAM_SCHEDULE_FIXED_UPDATE,
                set_pipeline
            );
            if (rc != BAM_RESULT_OK) { free(st); return rc; }
        }
        rc = st->api.mount_system_in_schedule(
            load_ctx,
            sys_handles[4],
            BAM_SCHEDULE_FIXED_UPDATE,
            set_finalize
        );
        if (rc != BAM_RESULT_OK) { free(st); return rc; }
    }

    *instance_out = (BamInstance*)st;
    return BAM_RESULT_OK;
}

BAM_EXPORT void bevy_abi_mod_shutdown(BamInstance* instance) {
    if (instance != NULL) {
        free(instance);
    }
}
