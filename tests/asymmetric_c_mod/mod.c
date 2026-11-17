#include "../../include/bevy_abi_mod.h"
#include "../../include/generated/testbed_components.h"
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

BAM_STATIC_ASSERT(sizeof(Phase4CRes) == 24u, phase4_c_res_size);
BAM_STATIC_ASSERT(BAM_ALIGNOF(Phase4CRes) == 8u, phase4_c_res_align);

/* Dynamic Component Definition */
typedef struct CModCustomData {
    float   mana;
    int32_t level;
} CModCustomData;

BAM_STATIC_ASSERT(sizeof(CModCustomData) == 8u, c_mod_custom_data_size);
BAM_STATIC_ASSERT(BAM_ALIGNOF(CModCustomData) == 4u, c_mod_custom_data_align);
BAM_STATIC_ASSERT(offsetof(CModCustomData, mana) == 0u, c_mod_custom_data_mana_off);
BAM_STATIC_ASSERT(offsetof(CModCustomData, level) == 4u, c_mod_custom_data_level_off);

/* ============================================================================
 * Per-World Instance State (`L-02`) & Test Harness Flags
 * ============================================================================ */

struct BamInstance {
    BamHostApi                api;
    BamComponent              pos_comp;
    const PositionVTable*     pos_vt;
    BamComponent              vel_comp;
    const VelocityVTable*     vel_vt;
    BamComponent              health_comp;
    const CombatHealthVTable* health_vt;
    BamComponent              tag_comp;
    const MetadataTagVTable*  tag_vt;
    BamComponent              nametag_comp;
    BamComponent              custom_comp;
    BamComponent              p4_tf_comp;
    const PositionVTable*     p4_tf_vt;
    BamComponent              p4_sensor_comp;
    BamComponent              p4_fresh_comp;
    BamComponent              p4_removable_comp;
    BamComponent              p4_res_comp;
};

static volatile BamBool g_c_mod_shutdown_called = BAM_FALSE;
static volatile BamBool g_c_phase4_run_enabled = BAM_TRUE;

static const BamManifest MANIFEST = {
    .struct_size = (uint32_t)sizeof(BamManifest),
    .abi_version = BAM_ABI_VERSION,
    .name = "asymmetric_c_mod",
    .version = "0.2.0",
    .flags = 0u,
    .reserved0 = 0u
};

/* ============================================================================
 * Resolution Helpers
 * ============================================================================ */

static const void* resolve_any_vtable_and_comp(
    const BamHostApi* api,
    const BamHostCtx* host_ctx,
    const char* const* vtable_symbols,
    size_t candidate_count,
    size_t min_vtable_size,
    BamComponent* out_comp
) {
    size_t i;
    *out_comp = BAM_COMPONENT_INVALID;
    for (i = 0u; i < candidate_count; ++i) {
        const BamVTableHeader* hdr =
            (const BamVTableHeader*)api->get_proc_address(host_ctx, vtable_symbols[i]);
        if (hdr != NULL &&
            hdr->struct_size >= (uint32_t)min_vtable_size &&
            bam_version_is_compatible(BAM_ABI_VERSION, hdr->abi_version)) {
            BamComponent resolved = BAM_COMPONENT_INVALID;
            if (api->resolve_component(host_ctx, NULL, &hdr->uuid, &resolved) == BAM_RESULT_OK &&
                bam_component_is_valid(resolved)) {
                *out_comp = resolved;
                return (const void*)hdr;
            }
        }
    }
    return NULL;
}

static BamComponent resolve_any_comp_by_name(
    const BamHostApi* api,
    const BamHostCtx* host_ctx,
    const char* const* names,
    size_t count
) {
    size_t i;
    for (i = 0u; i < count; ++i) {
        BamComponent c = BAM_COMPONENT_INVALID;
        if (api->resolve_component(host_ctx, names[i], NULL, &c) == BAM_RESULT_OK &&
            bam_component_is_valid(c)) {
            return c;
        }
    }
    return BAM_COMPONENT_INVALID;
}

/* ============================================================================
 * Scenario Systems
 * ============================================================================ */

/*
 * Scenario 1: 3D Kinematics System
 * Queries Position (WRITE, term 0). Advances z by 1.0f per tick via VTable.
 */
static BamResult c_kinematics_system(BamSystemCtx* sys_ctx, void* user_data) {
    BamInstance* st = (BamInstance*)user_data;
    BamIter* iter = NULL;
    BamColumnInfo col_info;
    size_t count = 0u;
    BamResult rc;

    if (st == NULL || st->pos_vt == NULL) {
        return BAM_RESULT_INTERNAL_ERROR;
    }
    rc = st->api.sys_query(sys_ctx, 0u, &iter);
    if (rc != BAM_RESULT_OK) {
        return rc;
    }
    rc = st->api.iter_column_info(iter, 0u, &col_info);
    if (rc != BAM_RESULT_OK) {
        return rc;
    }

    while (st->api.iter_next_chunk(iter, &count) != BAM_FALSE) {
        uint8_t* pos_base = st->api.iter_column_mutable(iter, 0u);
        size_t i;
        if (pos_base == NULL) {
            return BAM_RESULT_NULL_POINTER;
        }
        for (i = 0u; i < count; ++i) {
            void* p_ptr = (void*)(pos_base + (i * col_info.size));
            float z = st->pos_vt->get_z(p_ptr);
            BamResult sres = st->pos_vt->set_z(p_ptr, z + 1.0f);
            if (sres != BAM_RESULT_OK) {
                return sres;
            }
        }
    }

    return BAM_RESULT_OK;
}

/*
 * Scenario 2: Combat Damage System
 * Queries CombatHealth (WRITE, term 0), subtracts 10 HP, and despawns at <= 0.
 */
static BamResult c_combat_system(BamSystemCtx* sys_ctx, void* user_data) {
    BamInstance* st = (BamInstance*)user_data;
    BamIter* iter = NULL;
    BamCommands* commands = NULL;
    BamColumnInfo col_info;
    size_t count = 0u;
    const int32_t damage = 10;
    BamResult rc;

    if (st == NULL || st->health_vt == NULL) {
        return BAM_RESULT_INTERNAL_ERROR;
    }
    rc = st->api.sys_query(sys_ctx, 0u, &iter);
    if (rc != BAM_RESULT_OK) {
        return rc;
    }
    rc = st->api.sys_commands(sys_ctx, &commands);
    if (rc != BAM_RESULT_OK) {
        return rc;
    }
    rc = st->api.iter_column_info(iter, 0u, &col_info);
    if (rc != BAM_RESULT_OK) {
        return rc;
    }

    while (st->api.iter_next_chunk(iter, &count) != BAM_FALSE) {
        uint8_t* health_base = st->api.iter_column_mutable(iter, 0u);
        const BamEntity* entities = st->api.iter_entities(iter);
        size_t i;
        if (health_base == NULL) {
            return BAM_RESULT_NULL_POINTER;
        }
        for (i = 0u; i < count; ++i) {
            void* h_ptr = (void*)(health_base + (i * col_info.size));
            int32_t cur = st->health_vt->get_current(h_ptr);
            int32_t new_hp = cur > damage ? (cur - damage) : 0;
            BamResult sres = st->health_vt->set_current(h_ptr, new_hp);
            if (sres != BAM_RESULT_OK) {
                return sres;
            }
            if (new_hp <= 0 && commands != NULL && entities != NULL) {
                st->api.cmd_despawn(commands, entities[i]);
            }
        }
    }

    return BAM_RESULT_OK;
}

/*
 * Scenario 3: Dynamic Metadata System
 * Queries MetadataTag/NameTag (WRITE, term 0), upgrades "grunt*" -> "elite_grunt".
 */
static BamResult c_metadata_system(BamSystemCtx* sys_ctx, void* user_data) {
    BamInstance* st = (BamInstance*)user_data;
    BamIter* iter = NULL;
    BamColumnInfo col_info;
    size_t count = 0u;
    BamResult rc;

    if (st == NULL || st->tag_vt == NULL) {
        return BAM_RESULT_INTERNAL_ERROR;
    }
    rc = st->api.sys_query(sys_ctx, 0u, &iter);
    if (rc != BAM_RESULT_OK) {
        return rc;
    }
    rc = st->api.iter_column_info(iter, 0u, &col_info);
    if (rc != BAM_RESULT_OK) {
        return rc;
    }

    while (st->api.iter_next_chunk(iter, &count) != BAM_FALSE) {
        uint8_t* tag_base = st->api.iter_column_mutable_untracked(iter, 0u);
        size_t i;
        if (tag_base == NULL) {
            return BAM_RESULT_NULL_POINTER;
        }
        for (i = 0u; i < count; ++i) {
            void* elem = (void*)(tag_base + (i * col_info.size));
            BamStrView view = st->tag_vt->get_label(elem);
            if (view.len >= 5u && view.ptr != NULL && memcmp(view.ptr, "grunt", 5u) == 0) {
                BamResult sres = st->tag_vt->set_label(elem, bam_str_view("elite_grunt"));
                if (sres != BAM_RESULT_OK) {
                    return sres;
                }
                rc = st->api.iter_mark_changed_range(iter, 0u, i, 1u);
                if (rc != BAM_RESULT_OK) {
                    return rc;
                }
            }
        }
    }

    return BAM_RESULT_OK;
}

/*
 * Scenario 4: Deferred Commands System
 * Exercises handle-based `cmd_spawn_empty`, `cmd_insert`, and `cmd_despawn`.
 */
static BamResult c_command_system(BamSystemCtx* sys_ctx, void* user_data) {
    BamInstance* st = (BamInstance*)user_data;
    BamCommands* commands = NULL;
    BamEntity spawned = BAM_ENTITY_NULL;
    struct { float x; float y; float z; } pos_init = { 50.0f, 60.0f, 70.0f };
    BamResult rc;

    if (st == NULL) {
        return BAM_RESULT_INTERNAL_ERROR;
    }
    rc = st->api.sys_commands(sys_ctx, &commands);
    if (rc != BAM_RESULT_OK || commands == NULL) {
        return rc;
    }

    rc = st->api.cmd_spawn_empty(commands, &spawned);
    if (rc != BAM_RESULT_OK || bam_entity_is_null(spawned)) {
        return BAM_RESULT_INTERNAL_ERROR;
    }

    if (bam_component_is_valid(st->pos_comp)) {
        rc = st->api.cmd_insert(
            commands,
            spawned,
            st->pos_comp,
            (const uint8_t*)&pos_init,
            sizeof(pos_init)
        );
        if (rc != BAM_RESULT_OK) {
            return rc;
        }
    }

    return st->api.cmd_despawn(commands, spawned);
}

/*
 * Scenario 5: Dynamic Component System (`asymmetric_c_mod::CModCustomData`)
 */
static BamResult c_custom_data_system(BamSystemCtx* sys_ctx, void* user_data) {
    BamInstance* st = (BamInstance*)user_data;
    BamIter* iter = NULL;
    BamColumnInfo col_info;
    size_t count = 0u;
    BamResult rc;

    if (st == NULL) {
        return BAM_RESULT_INTERNAL_ERROR;
    }
    rc = st->api.sys_query(sys_ctx, 0u, &iter);
    if (rc != BAM_RESULT_OK) {
        return rc;
    }
    rc = st->api.iter_column_info(iter, 0u, &col_info);
    if (rc != BAM_RESULT_OK) {
        return rc;
    }

    while (st->api.iter_next_chunk(iter, &count) != BAM_FALSE) {
        uint8_t* base = st->api.iter_column_mutable(iter, 0u);
        size_t i;
        if (base == NULL) {
            return BAM_RESULT_NULL_POINTER;
        }
        for (i = 0u; i < count; ++i) {
            CModCustomData* data = (CModCustomData*)(void*)(base + (i * col_info.size));
            data->mana += 5.0f;
            data->level += 1;
        }
    }

    return BAM_RESULT_OK;
}

/*
 * Scenario 6: Optional Columns, Changed/Added Row Filters, Resources, `run_if`,
 * `cmd_remove`, and `cmd_insert_default`.
 */
static BamBool c_phase4_run_if(void* user_data) {
    (void)user_data;
    return g_c_phase4_run_enabled;
}

static BamResult c_phase4_ecs_system(BamSystemCtx* sys_ctx, void* user_data) {
    BamInstance* st = (BamInstance*)user_data;
    BamIter* iter = NULL;
    BamCommands* commands = NULL;
    Phase4CRes* res;
    BamColumnInfo tf_info;
    BamColumnInfo vel_info;
    size_t count = 0u;
    BamResult rc;

    if (st == NULL || st->p4_tf_vt == NULL) {
        return BAM_RESULT_INTERNAL_ERROR;
    }
    rc = st->api.sys_query(sys_ctx, 0u, &iter);
    if (rc != BAM_RESULT_OK) {
        return rc;
    }
    rc = st->api.sys_commands(sys_ctx, &commands);
    if (rc != BAM_RESULT_OK) {
        return rc;
    }

    res = (Phase4CRes*)(void*)st->api.res_mut_ptr(iter, 4u);
    if (res == NULL) {
        return BAM_RESULT_INVALID_COMPONENT;
    }

    if (res->spawned_default_entity == 0u &&
        commands != NULL &&
        bam_component_is_valid(st->nametag_comp)) {
        BamEntity ent = BAM_ENTITY_NULL;
        if (st->api.cmd_spawn_empty(commands, &ent) == BAM_RESULT_OK &&
            !bam_entity_is_null(ent)) {
            if (st->api.cmd_insert_default(commands, ent, st->nametag_comp) == BAM_RESULT_OK) {
                res->spawned_default_entity = ent;
            }
        }
    }

    if (st->api.iter_column_info(iter, 0u, &tf_info) != BAM_RESULT_OK ||
        st->api.iter_column_info(iter, 1u, &vel_info) != BAM_RESULT_OK) {
        return BAM_RESULT_INTERNAL_ERROR;
    }

    while (st->api.iter_next_chunk(iter, &count) != BAM_FALSE) {
        uint8_t* pos_base = st->api.iter_column_mutable(iter, 0u);
        const uint8_t* opt_vel_base = st->api.iter_column_readonly(iter, 1u);
        const BamEntity* entities = st->api.iter_entities(iter);
        size_t i;

        if (pos_base == NULL) {
            return BAM_RESULT_NULL_POINTER;
        }
        for (i = 0u; i < count; ++i) {
            void* p_ptr = (void*)(pos_base + (i * tf_info.size));
            float py = st->p4_tf_vt->get_y(p_ptr);
            BamResult sres;
            if (opt_vel_base != NULL && st->vel_vt != NULL) {
                const void* v_ptr = (const void*)(opt_vel_base + (i * vel_info.size));
                float vy = st->vel_vt->get_y(v_ptr);
                sres = st->p4_tf_vt->set_y(p_ptr, py + (vy * res->scale));
                res->optional_vel_hits += 1u;
            } else {
                sres = st->p4_tf_vt->set_y(p_ptr, py + (1.0f * res->scale));
            }
            if (sres != BAM_RESULT_OK) {
                return sres;
            }
            res->matched_rows += 1u;

            if (commands != NULL && entities != NULL && bam_component_is_valid(st->p4_removable_comp)) {
                st->api.cmd_remove(commands, entities[i], st->p4_removable_comp);
            }
        }
    }

    return BAM_RESULT_OK;
}

/* ============================================================================
 * Mandatory ABI `0.2.0` Entry Points & Test Harness Exports
 * ============================================================================ */

BAM_EXPORT const BamManifest* bevy_abi_mod_manifest(void) {
    return &MANIFEST;
}

BAM_EXPORT bool asymmetric_c_mod_was_shutdown(void) {
    return g_c_mod_shutdown_called != BAM_FALSE;
}

BAM_EXPORT void asymmetric_c_mod_set_phase4_enabled(BamBool enabled) {
    g_c_phase4_run_enabled = (enabled != BAM_FALSE) ? BAM_TRUE : BAM_FALSE;
}

static BamResult register_single_query_system(
    BamInstance* st,
    BamLoadCtx* load_ctx,
    const char* sys_name,
    const BamTermDesc* terms,
    size_t term_count,
    BamSystemFn callback,
    BamRunIfFn run_if
) {
    BamQueryDesc q_desc;
    BamSystemDesc s_desc;

    memset(&q_desc, 0, sizeof(q_desc));
    q_desc.struct_size = (uint32_t)sizeof(BamQueryDesc);
    q_desc.flags = 0u;
    q_desc.terms = terms;
    q_desc.term_count = term_count;

    memset(&s_desc, 0, sizeof(s_desc));
    s_desc.struct_size = (uint32_t)sizeof(BamSystemDesc);
    s_desc.flags = 0u;
    s_desc.name = sys_name;
    s_desc.queries = (term_count > 0u) ? &q_desc : NULL;
    s_desc.query_count = (term_count > 0u) ? 1u : 0u;
    s_desc.callback = callback;
    s_desc.user_data = st;
    s_desc.schedule = BAM_SCHEDULE_UPDATE;
    s_desc.reserved0 = 0u;
    s_desc.run_if = run_if;
    s_desc.run_if_user_data = st;

    return st->api.register_system(load_ctx, &s_desc);
}

BAM_EXPORT BamResult bevy_abi_mod_init(
    const BamHostApi* host_api,
    BamLoadCtx* load_ctx,
    BamInstance** instance_out
) {
    static const char* const POS_VT_CANDIDATES[] = {
        "bam.v0.testbed::Position.vtable",
        "bam.v0.e2e::asymmetric_mods::Position.vtable",
        "bam.v0.Position.vtable"
    };
    static const char* const VEL_VT_CANDIDATES[] = {
        "bam.v0.testbed::Velocity.vtable",
        "bam.v0.e2e::asymmetric_mods::Velocity.vtable",
        "bam.v0.Velocity.vtable"
    };
    static const char* const HEALTH_VT_CANDIDATES[] = {
        "bam.v0.testbed::CombatHealth.vtable",
        "bam.v0.testbed::Health.vtable",
        "bam.v0.e2e::asymmetric_mods::CombatHealth.vtable",
        "bam.v0.CombatHealth.vtable"
    };
    static const char* const TAG_VT_CANDIDATES[] = {
        "bam.v0.testbed::MetadataTag.vtable",
        "bam.v0.testbed::NameTag.vtable",
        "bam.v0.MetadataTag.vtable",
        "bam.v0.NameTag.vtable"
    };
    static const char* const NAMETAG_NAMES[] = {
        "testbed::NameTag",
        "e2e::asymmetric_mods::NameTag",
        "ffi::sparse_and_resources::p4_resources_and_optional::NameTag",
        "NameTag"
    };
    static const char* const P4_TF_VT_CANDIDATES[] = {
        "bam.v0.testbed::Phase4CTransform.vtable",
        "bam.v0.e2e::asymmetric_mods::Phase4CTransform.vtable",
        "bam.v0.ffi::sparse_and_resources::p4_resources_and_optional::Phase4CTransform.vtable"
    };
    static const char* const P4_SENSOR_NAMES[] = {
        "testbed::Phase4CSensor",
        "e2e::asymmetric_mods::Phase4CSensor",
        "ffi::sparse_and_resources::p4_resources_and_optional::Phase4CSensor"
    };
    static const char* const P4_FRESH_NAMES[] = {
        "testbed::Phase4CFreshTag",
        "e2e::asymmetric_mods::Phase4CFreshTag",
        "ffi::sparse_and_resources::p4_resources_and_optional::Phase4CFreshTag"
    };
    static const char* const P4_REM_NAMES[] = {
        "testbed::Phase4CRemovable",
        "e2e::asymmetric_mods::Phase4CRemovable",
        "ffi::sparse_and_resources::p4_resources_and_optional::Phase4CRemovable"
    };
    static const char* const P4_RES_NAMES[] = {
        "testbed::Phase4CRes",
        "e2e::asymmetric_mods::Phase4CRes",
        "ffi::sparse_and_resources::p4_resources_and_optional::Phase4CRes"
    };

    BamHostApi local_api;
    const BamHostCtx* host_ctx;
    BamInstance* st;
    BamResult rc;

    if (load_ctx == NULL || instance_out == NULL) {
        return BAM_RESULT_NULL_POINTER;
    }
    *instance_out = NULL;

    rc = bam_host_api_copy_bounded(&local_api, sizeof(local_api), host_api);
    if (rc != BAM_RESULT_OK) {
        return rc;
    }
    host_ctx = local_api.load_host_ctx(load_ctx);
    if (host_ctx == NULL) {
        return BAM_RESULT_INVALID_STATE;
    }

    st = (BamInstance*)calloc(1u, sizeof(BamInstance));
    if (st == NULL) {
        return BAM_RESULT_INTERNAL_ERROR;
    }
    st->api = local_api;

    st->pos_vt = (const PositionVTable*)resolve_any_vtable_and_comp(
        &st->api, host_ctx, POS_VT_CANDIDATES, 3u, sizeof(PositionVTable), &st->pos_comp
    );
    st->vel_vt = (const VelocityVTable*)resolve_any_vtable_and_comp(
        &st->api, host_ctx, VEL_VT_CANDIDATES, 3u, sizeof(VelocityVTable), &st->vel_comp
    );
    st->health_vt = (const CombatHealthVTable*)resolve_any_vtable_and_comp(
        &st->api, host_ctx, HEALTH_VT_CANDIDATES, 4u, sizeof(CombatHealthVTable), &st->health_comp
    );
    st->tag_vt = (const MetadataTagVTable*)resolve_any_vtable_and_comp(
        &st->api, host_ctx, TAG_VT_CANDIDATES, 4u, sizeof(MetadataTagVTable), &st->tag_comp
    );
    st->nametag_comp = resolve_any_comp_by_name(&st->api, host_ctx, NAMETAG_NAMES, 4u);

    /* 1. Register Kinematics System if Position is exposed */
    if (st->pos_vt != NULL && bam_component_is_valid(st->pos_comp)) {
        BamTermDesc terms[1];
        terms[0] = bam_term_desc(st->pos_comp, BAM_ACCESS_WRITE, 0u);
        rc = register_single_query_system(
            st, load_ctx, "c_kinematics", terms, 1u, c_kinematics_system, NULL
        );
        if (rc != BAM_RESULT_OK) {
            free(st);
            return rc;
        }
    }

    /* 2. Register Combat Damage System if CombatHealth is exposed */
    if (st->health_vt != NULL && bam_component_is_valid(st->health_comp)) {
        BamTermDesc terms[1];
        terms[0] = bam_term_desc(st->health_comp, BAM_ACCESS_WRITE, 0u);
        rc = register_single_query_system(
            st, load_ctx, "c_combat_damage", terms, 1u, c_combat_system, NULL
        );
        if (rc != BAM_RESULT_OK) {
            free(st);
            return rc;
        }
    }

    /* 3. Register Metadata Inspector System if MetadataTag/NameTag is exposed */
    if (st->tag_vt != NULL && bam_component_is_valid(st->tag_comp)) {
        BamTermDesc terms[1];
        terms[0] = bam_term_desc(st->tag_comp, BAM_ACCESS_WRITE, 0u);
        rc = register_single_query_system(
            st, load_ctx, "c_metadata_inspector", terms, 1u, c_metadata_system, NULL
        );
        if (rc != BAM_RESULT_OK) {
            free(st);
            return rc;
        }
    }

    /* 4. Register Command Spawner System when Position is exposed */
    if (bam_component_is_valid(st->pos_comp)) {
        rc = register_single_query_system(
            st, load_ctx, "c_command_spawner", NULL, 0u, c_command_system, NULL
        );
        if (rc != BAM_RESULT_OK) {
            free(st);
            return rc;
        }
    }

    /* 5. Register `asymmetric_c_mod::CModCustomData` dynamic component & system */
    {
        BamComponentDesc cdesc;
        memset(&cdesc, 0, sizeof(cdesc));
        cdesc.struct_size = (uint32_t)sizeof(BamComponentDesc);
        cdesc.flags = 0u;
        cdesc.name = "CModCustomData";
        cdesc.uuid = bam_uuid_from_parts(
            "asymmetric_c_mod::CModCustomData",
            sizeof(CModCustomData),
            BAM_ALIGNOF(CModCustomData),
            0ull
        );
        cdesc.size = sizeof(CModCustomData);
        cdesc.align = BAM_ALIGNOF(CModCustomData);
        cdesc.drop_fn = NULL;
        cdesc.vtable_ptr = NULL;

        rc = st->api.register_component(load_ctx, &cdesc, &st->custom_comp);
        if (rc != BAM_RESULT_OK) {
            free(st);
            return rc;
        }

        {
            BamTermDesc terms[1];
            terms[0] = bam_term_desc(st->custom_comp, BAM_ACCESS_WRITE, 0u);
            rc = register_single_query_system(
                st, load_ctx, "c_custom_data_system", terms, 1u, c_custom_data_system, NULL
            );
            if (rc != BAM_RESULT_OK) {
                free(st);
                return rc;
            }
        }
    }

    /* 6. Register Phase 4 Conditional System when Phase4C* components & resource exist */
    st->p4_tf_vt = (const PositionVTable*)resolve_any_vtable_and_comp(
        &st->api, host_ctx, P4_TF_VT_CANDIDATES, 3u, sizeof(PositionVTable), &st->p4_tf_comp
    );
    st->p4_sensor_comp = resolve_any_comp_by_name(&st->api, host_ctx, P4_SENSOR_NAMES, 3u);
    st->p4_fresh_comp = resolve_any_comp_by_name(&st->api, host_ctx, P4_FRESH_NAMES, 3u);
    st->p4_removable_comp = resolve_any_comp_by_name(&st->api, host_ctx, P4_REM_NAMES, 3u);
    st->p4_res_comp = resolve_any_comp_by_name(&st->api, host_ctx, P4_RES_NAMES, 3u);

    if (st->p4_tf_vt != NULL &&
        bam_component_is_valid(st->p4_tf_comp) &&
        bam_component_is_valid(st->vel_comp) &&
        bam_component_is_valid(st->p4_sensor_comp) &&
        bam_component_is_valid(st->p4_fresh_comp) &&
        bam_component_is_valid(st->p4_res_comp)) {
        BamTermDesc terms[5];
        terms[0] = bam_term_desc(st->p4_tf_comp, BAM_ACCESS_WRITE, 0u);
        terms[1] = bam_term_desc(st->vel_comp, BAM_ACCESS_OPTIONAL_READ, 0u);
        terms[2] = bam_term_desc(st->p4_sensor_comp, BAM_ACCESS_CHANGED, 0u);
        terms[3] = bam_term_desc(st->p4_fresh_comp, BAM_ACCESS_ADDED, 0u);
        terms[4] = bam_term_desc(st->p4_res_comp, BAM_ACCESS_RESOURCE_WRITE, 0u);

        rc = register_single_query_system(
            st, load_ctx, "c_phase4_ecs_system", terms, 5u, c_phase4_ecs_system, c_phase4_run_if
        );
        if (rc != BAM_RESULT_OK) {
            free(st);
            return rc;
        }
    }

    *instance_out = st;
    return BAM_RESULT_OK;
}

BAM_EXPORT void bevy_abi_mod_shutdown(BamInstance* instance) {
    g_c_mod_shutdown_called = BAM_TRUE;
    g_c_phase4_run_enabled = BAM_TRUE;
    if (instance != NULL) {
        free(instance);
    }
}

/*
 * Cross-language VTable verification helper for `crates/bevy_abi_mod_host/tests/ffi/c_vtable_interop.rs`.
 */
BAM_EXPORT BamResult bevy_c_mod_test_vtable_mutation(
    BamGetProcAddressFn get_proc_address,
    const BamHostCtx* host_ctx,
    void* column_base_ptr,
    size_t count
) {
    const CInteropComponentVTable* comp_vt;
    const Vec3VTable* vec3_vt;
    uint8_t* base;
    BamStrView mutated_view;
    size_t i;

    if (get_proc_address == NULL || column_base_ptr == NULL) {
        return BAM_RESULT_NULL_POINTER;
    }

    comp_vt = (const CInteropComponentVTable*)get_proc_address(
        host_ctx, "bam.v0.c_interop::CInteropComponent.vtable"
    );
    vec3_vt = (const Vec3VTable*)get_proc_address(
        host_ctx, "bam.v0.c_interop::Vec3.vtable"
    );

    if (comp_vt == NULL || vec3_vt == NULL) {
        return BAM_RESULT_INVALID_COMPONENT;
    }
    if (comp_vt->header.struct_size < (uint32_t)sizeof(CInteropComponentVTable) ||
        vec3_vt->header.struct_size < (uint32_t)sizeof(Vec3VTable) ||
        !bam_version_is_compatible(BAM_ABI_VERSION, comp_vt->header.abi_version) ||
        !bam_version_is_compatible(BAM_ABI_VERSION, vec3_vt->header.abi_version)) {
        return BAM_RESULT_INVALID_VERSION;
    }

    base = (uint8_t*)column_base_ptr;
    mutated_view = bam_str_view("mutated_from_c99");

    for (i = 0u; i < count; ++i) {
        void* elem = (void*)(base + (i * comp_vt->header.size));
        int32_t hp = comp_vt->get_health(elem);
        void* pos_ptr;
        BamResult rc = comp_vt->set_health(elem, hp + 1000);
        if (rc != BAM_RESULT_OK) {
            return rc;
        }

        pos_ptr = comp_vt->ptr_mut_pos(elem);
        if (pos_ptr != NULL) {
            float x = vec3_vt->get_x(pos_ptr);
            float y = vec3_vt->get_y(pos_ptr);
            float z = vec3_vt->get_z(pos_ptr);
            if (vec3_vt->set_x(pos_ptr, x + 10.5f) != BAM_RESULT_OK ||
                vec3_vt->set_y(pos_ptr, y + 20.5f) != BAM_RESULT_OK ||
                vec3_vt->set_z(pos_ptr, z + 30.5f) != BAM_RESULT_OK) {
                return BAM_RESULT_INTERNAL_ERROR;
            }
        }

        rc = comp_vt->set_label(elem, mutated_view);
        if (rc != BAM_RESULT_OK) {
            return rc;
        }
    }

    return BAM_RESULT_OK;
}
