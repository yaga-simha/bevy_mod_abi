#include "../../include/bevy_abi_mod.h"
#include <math.h>
#include <stdlib.h>

#define SIM_DT 0.016f
#define MAX_STACK_EMITTERS 512

/* ============================================================================
 * 1. VTable Mirror Structs Matching `#[derive(ModComponent)]` (`M-04`, `M-05`)
 * ============================================================================ */

typedef struct Vec3fVTable {
    BamVTableHeader header;
    float     (*get_x)(const void* ptr);
    BamResult (*set_x)(void* ptr, float val);
    float     (*get_y)(const void* ptr);
    BamResult (*set_y)(void* ptr, float val);
    float     (*get_z)(const void* ptr);
    BamResult (*set_z)(void* ptr, float val);
} Vec3fVTable;

typedef struct Transform3DVTable {
    BamVTableHeader header;
    const void* (*ptr_position)(const void* ptr);
    void*       (*ptr_mut_position)(void* ptr);
    const void* (*ptr_forward)(const void* ptr);
    void*       (*ptr_mut_forward)(void* ptr);
    float       (*get_scale)(const void* ptr);
    BamResult   (*set_scale)(void* ptr, float val);
} Transform3DVTable;

typedef struct KinematicsVTable {
    BamVTableHeader header;
    const void* (*ptr_velocity)(const void* ptr);
    void*       (*ptr_mut_velocity)(void* ptr);
    float       (*get_drag)(const void* ptr);
    BamResult   (*set_drag)(void* ptr, float val);
    float       (*get_max_speed)(const void* ptr);
    BamResult   (*set_max_speed)(void* ptr, float val);
} KinematicsVTable;

typedef struct VitalityVTable {
    BamVTableHeader header;
    float     (*get_health)(const void* ptr);
    BamResult (*set_health)(void* ptr, float val);
    float     (*get_max_health)(const void* ptr);
    BamResult (*set_max_health)(void* ptr, float val);
    float     (*get_shield)(const void* ptr);
    BamResult (*set_shield)(void* ptr, float val);
    float     (*get_energy)(const void* ptr);
    BamResult (*set_energy)(void* ptr, float val);
} VitalityVTable;

typedef struct ResistanceVTable {
    BamVTableHeader header;
    float     (*get_kinetic)(const void* ptr);
    BamResult (*set_kinetic)(void* ptr, float val);
    float     (*get_thermal)(const void* ptr);
    BamResult (*set_thermal)(void* ptr, float val);
    float     (*get_ionic)(const void* ptr);
    BamResult (*set_ionic)(void* ptr, float val);
} ResistanceVTable;

typedef struct AuraEmitterVTable {
    BamVTableHeader header;
    float     (*get_radius)(const void* ptr);
    BamResult (*set_radius)(void* ptr, float val);
    float     (*get_force)(const void* ptr);
    BamResult (*set_force)(void* ptr, float val);
    float     (*get_thermal_dps)(const void* ptr);
    BamResult (*set_thermal_dps)(void* ptr, float val);
    float     (*get_heal_hps)(const void* ptr);
    BamResult (*set_heal_hps)(void* ptr, float val);
    float     (*get_falloff)(const void* ptr);
    BamResult (*set_falloff)(void* ptr, float val);
} AuraEmitterVTable;

typedef struct FactionVTable {
    BamVTableHeader header;
    uint32_t  (*get_team_id)(const void* ptr);
    BamResult (*set_team_id)(void* ptr, uint32_t val);
    uint32_t  (*get_serial_id)(const void* ptr);
    BamResult (*set_serial_id)(void* ptr, uint32_t val);
} FactionVTable;

/* ============================================================================
 * 2. Per-Instance `ModState` (`L-02`) & Pre-Load Variant Selector
 * ============================================================================ */

static uint32_t g_variant = 0u;

BAM_EXPORT void c_abi_c99_mod_set_variant(uint32_t variant) {
    g_variant = variant;
}

typedef struct ModState {
    BamHostApi               api;
    const Vec3fVTable*       v3_vt;
    const Transform3DVTable* tf_vt;
    const KinematicsVTable*  kin_vt;
    const VitalityVTable*    vit_vt;
    const ResistanceVTable*  res_vt;
    const AuraEmitterVTable* aura_vt;
    const FactionVTable*     fac_vt;
} ModState;

static const BamManifest MANIFEST = {
    (uint32_t)sizeof(BamManifest),
    BAM_ABI_VERSION,
    "c_abi_c99_mod",
    "0.2.0",
    0u,
    0u
};

typedef struct UnpackedEmitter {
    float px, py, pz;
    float fx, fy, fz;
    float radius;
    float rad_sq;
    float force;
    float thermal_dps;
    float heal_hps;
    float falloff;
    uint32_t team_id;
} UnpackedEmitter;

/* ============================================================================
 * Variant 0: Chunk-Optimized Nested Query (`O(U + E)` C-ABI calls)
 * ============================================================================ */

static BamResult c99_sys_chunk_optimized(BamSystemCtx* sys_ctx, void* user_data) {
    ModState* st = (ModState*)user_data;
    BamIter* unit_iter = NULL;
    BamIter* emit_iter = NULL;
    UnpackedEmitter stack_buf[MAX_STACK_EMITTERS];
    UnpackedEmitter* heap_buf = NULL;
    const UnpackedEmitter* emitters;
    size_t cap = MAX_STACK_EMITTERS;
    size_t total_emitters = 0u;
    size_t e_count = 0u;
    size_t u_count = 0u;
    BamResult res;

    if (st == NULL) return BAM_RESULT_NULL_POINTER;
    res = st->api.sys_query(sys_ctx, 0u, &unit_iter);
    if (res != BAM_RESULT_OK) return res;
    res = st->api.sys_query(sys_ctx, 1u, &emit_iter);
    if (res != BAM_RESULT_OK) return res;

    while (st->api.iter_next_chunk(emit_iter, &e_count) != BAM_FALSE && e_count > 0u) {
        const uint8_t* e_tf_col   = st->api.iter_column_readonly(emit_iter, 0u);
        const uint8_t* e_aura_col = st->api.iter_column_readonly(emit_iter, 1u);
        const uint8_t* e_fac_col  = st->api.iter_column_readonly(emit_iter, 2u);
        size_t r;

        for (r = 0u; r < e_count; ++r) {
            const void* tf_p   = e_tf_col   + r * st->tf_vt->header.size;
            const void* aura_p = e_aura_col + r * st->aura_vt->header.size;
            const void* fac_p  = e_fac_col  + r * st->fac_vt->header.size;

            const void* pos_p = st->tf_vt->ptr_position(tf_p);
            const void* fwd_p = st->tf_vt->ptr_forward(tf_p);
            float radius = st->aura_vt->get_radius(aura_p);

            UnpackedEmitter em;
            em.px = st->v3_vt->get_x(pos_p);
            em.py = st->v3_vt->get_y(pos_p);
            em.pz = st->v3_vt->get_z(pos_p);
            em.fx = st->v3_vt->get_x(fwd_p);
            em.fy = st->v3_vt->get_y(fwd_p);
            em.fz = st->v3_vt->get_z(fwd_p);
            em.radius = radius;
            em.rad_sq = radius * radius;
            em.force = st->aura_vt->get_force(aura_p);
            em.thermal_dps = st->aura_vt->get_thermal_dps(aura_p);
            em.heal_hps = st->aura_vt->get_heal_hps(aura_p);
            em.falloff = st->aura_vt->get_falloff(aura_p);
            em.team_id = st->fac_vt->get_team_id(fac_p);

            if (total_emitters < cap) {
                if (heap_buf != NULL) {
                    heap_buf[total_emitters] = em;
                } else {
                    stack_buf[total_emitters] = em;
                }
            } else {
                size_t new_cap = cap * 2u;
                UnpackedEmitter* nb = (UnpackedEmitter*)malloc(new_cap * sizeof(UnpackedEmitter));
                const UnpackedEmitter* src;
                size_t k;
                if (nb == NULL) {
                    if (heap_buf != NULL) free(heap_buf);
                    return BAM_RESULT_INTERNAL_ERROR;
                }
                src = (heap_buf != NULL) ? heap_buf : stack_buf;
                for (k = 0u; k < total_emitters; ++k) nb[k] = src[k];
                if (heap_buf != NULL) free(heap_buf);
                heap_buf = nb;
                cap = new_cap;
                heap_buf[total_emitters] = em;
            }
            total_emitters++;
        }
    }

    emitters = (heap_buf != NULL) ? heap_buf : stack_buf;

    while (st->api.iter_next_chunk(unit_iter, &u_count) != BAM_FALSE && u_count > 0u) {
        uint8_t*       u_tf_col  = st->api.iter_column_mutable(unit_iter, 0u);
        uint8_t*       u_kin_col = st->api.iter_column_mutable(unit_iter, 1u);
        uint8_t*       u_vit_col = st->api.iter_column_mutable(unit_iter, 2u);
        const uint8_t* u_res_col = st->api.iter_column_readonly(unit_iter, 3u);
        const uint8_t* u_fac_col = st->api.iter_column_readonly(unit_iter, 4u);
        size_t u;

        for (u = 0u; u < u_count; ++u) {
            void*       tf_p  = u_tf_col  + u * st->tf_vt->header.size;
            void*       kin_p = u_kin_col + u * st->kin_vt->header.size;
            void*       vit_p = u_vit_col + u * st->vit_vt->header.size;
            const void* res_p = u_res_col + u * st->res_vt->header.size;
            const void* fac_p = u_fac_col + u * st->fac_vt->header.size;

            void* pos_p = st->tf_vt->ptr_mut_position(tf_p);
            float ux = st->v3_vt->get_x(pos_p);
            float uy = st->v3_vt->get_y(pos_p);
            float uz = st->v3_vt->get_z(pos_p);

            void* vel_p = st->kin_vt->ptr_mut_velocity(kin_p);
            float vx = st->v3_vt->get_x(vel_p);
            float vy = st->v3_vt->get_y(vel_p);
            float vz = st->v3_vt->get_z(vel_p);
            float drag = st->kin_vt->get_drag(kin_p);
            float max_speed = st->kin_vt->get_max_speed(kin_p);

            float health = st->vit_vt->get_health(vit_p);
            float max_health = st->vit_vt->get_max_health(vit_p);
            float shield = st->vit_vt->get_shield(vit_p);
            float energy = st->vit_vt->get_energy(vit_p);

            float r_kin = st->res_vt->get_kinetic(res_p);
            float r_thm = st->res_vt->get_thermal(res_p);
            float r_ion = st->res_vt->get_ionic(res_p);
            uint32_t u_team = st->fac_vt->get_team_id(fac_p);
            size_t e;
            float damp, speed_sq, max_sq, final_speed_sq;

            for (e = 0u; e < total_emitters; ++e) {
                const UnpackedEmitter* em = &emitters[e];
                float dx = ux - em->px;
                float dy = uy - em->py;
                float dz = uz - em->pz;
                float dist_sq = dx * dx + dy * dy + dz * dz + 0.25f;

                if (dist_sq < em->rad_sq) {
                    float dist = sqrtf(dist_sq);
                    float inv_dist = 1.0f / dist;
                    float norm = 1.0f - (dist / em->radius);
                    float atten = norm / (1.0f + em->falloff * dist_sq);

                    if (u_team == em->team_id) {
                        float boost = em->force * 0.25f * atten * SIM_DT;
                        float heal, nh, ne;
                        vx += em->fx * boost;
                        vy += em->fy * boost;
                        vz += em->fz * boost;

                        heal = em->heal_hps * atten * SIM_DT * (1.0f + 0.1f * r_ion);
                        nh = health + heal;
                        health = (nh > max_health) ? max_health : nh;

                        ne = energy + 2.5f * atten * SIM_DT;
                        energy = (ne > 100.0f) ? 100.0f : ne;
                    } else {
                        float imp = em->force * atten * SIM_DT * (1.0f - 0.5f * r_kin) * inv_dist;
                        float raw_dmg, absorbed, nh, drain, ne;
                        vx += dx * imp;
                        vy += dy * imp;
                        vz += dz * imp;

                        raw_dmg = em->thermal_dps * atten * SIM_DT * (1.0f - 0.6f * r_thm);
                        absorbed = (shield >= raw_dmg) ? raw_dmg : shield;
                        shield -= absorbed;
                        nh = health - (raw_dmg - absorbed);
                        health = (nh < 0.0f) ? 0.0f : nh;

                        drain = 1.5f * atten * SIM_DT * (1.0f - 0.4f * r_ion);
                        ne = energy - drain;
                        energy = (ne < 0.0f) ? 0.0f : ne;
                    }
                }
            }

            damp = 1.0f / (1.0f + drag * SIM_DT);
            vx *= damp;
            vy *= damp;
            vz *= damp;

            speed_sq = vx * vx + vy * vy + vz * vz;
            max_sq = max_speed * max_speed;
            if (speed_sq > max_sq && speed_sq > 1.0e-12f) {
                float sc = max_speed / sqrtf(speed_sq);
                vx *= sc;
                vy *= sc;
                vz *= sc;
            }

            (void)st->v3_vt->set_x(vel_p, vx);
            (void)st->v3_vt->set_y(vel_p, vy);
            (void)st->v3_vt->set_z(vel_p, vz);

            (void)st->v3_vt->set_x(pos_p, ux + vx * SIM_DT);
            (void)st->v3_vt->set_y(pos_p, uy + vy * SIM_DT);
            (void)st->v3_vt->set_z(pos_p, uz + vz * SIM_DT);

            final_speed_sq = vx * vx + vy * vy + vz * vz;
            if (final_speed_sq > 1.0e-6f) {
                float inv_s = 1.0f / sqrtf(final_speed_sq);
                void* fwd_p = st->tf_vt->ptr_mut_forward(tf_p);
                (void)st->v3_vt->set_x(fwd_p, vx * inv_s);
                (void)st->v3_vt->set_y(fwd_p, vy * inv_s);
                (void)st->v3_vt->set_z(fwd_p, vz * inv_s);
            }

            (void)st->vit_vt->set_health(vit_p, health);
            (void)st->vit_vt->set_shield(vit_p, shield);
            (void)st->vit_vt->set_energy(vit_p, energy);
        }
    }

    if (heap_buf != NULL) free(heap_buf);
    return BAM_RESULT_OK;
}

/* ============================================================================
 * Variant 1: Entity-Nested VTable-per-Pair (`O(U * E)` C-ABI getter calls)
 * ============================================================================ */

static BamResult c99_sys_entity_nested_vtable(BamSystemCtx* sys_ctx, void* user_data) {
    ModState* st = (ModState*)user_data;
    BamIter* unit_iter = NULL;
    BamIter* emit_iter = NULL;
    size_t u_count = 0u;
    BamResult res;

    if (st == NULL) return BAM_RESULT_NULL_POINTER;
    res = st->api.sys_query(sys_ctx, 0u, &unit_iter);
    if (res != BAM_RESULT_OK) return res;
    res = st->api.sys_query(sys_ctx, 1u, &emit_iter);
    if (res != BAM_RESULT_OK) return res;

    while (st->api.iter_next_chunk(unit_iter, &u_count) != BAM_FALSE && u_count > 0u) {
        uint8_t*       u_tf_col  = st->api.iter_column_mutable(unit_iter, 0u);
        uint8_t*       u_kin_col = st->api.iter_column_mutable(unit_iter, 1u);
        uint8_t*       u_vit_col = st->api.iter_column_mutable(unit_iter, 2u);
        const uint8_t* u_res_col = st->api.iter_column_readonly(unit_iter, 3u);
        const uint8_t* u_fac_col = st->api.iter_column_readonly(unit_iter, 4u);
        size_t u;

        for (u = 0u; u < u_count; ++u) {
            void*       tf_p  = u_tf_col  + u * st->tf_vt->header.size;
            void*       kin_p = u_kin_col + u * st->kin_vt->header.size;
            void*       vit_p = u_vit_col + u * st->vit_vt->header.size;
            const void* res_p = u_res_col + u * st->res_vt->header.size;
            const void* fac_p = u_fac_col + u * st->fac_vt->header.size;

            void* pos_p = st->tf_vt->ptr_mut_position(tf_p);
            float ux = st->v3_vt->get_x(pos_p);
            float uy = st->v3_vt->get_y(pos_p);
            float uz = st->v3_vt->get_z(pos_p);

            void* vel_p = st->kin_vt->ptr_mut_velocity(kin_p);
            float vx = st->v3_vt->get_x(vel_p);
            float vy = st->v3_vt->get_y(vel_p);
            float vz = st->v3_vt->get_z(vel_p);
            float drag = st->kin_vt->get_drag(kin_p);
            float max_speed = st->kin_vt->get_max_speed(kin_p);

            float health = st->vit_vt->get_health(vit_p);
            float max_health = st->vit_vt->get_max_health(vit_p);
            float shield = st->vit_vt->get_shield(vit_p);
            float energy = st->vit_vt->get_energy(vit_p);

            float r_kin = st->res_vt->get_kinetic(res_p);
            float r_thm = st->res_vt->get_thermal(res_p);
            float r_ion = st->res_vt->get_ionic(res_p);
            uint32_t u_team = st->fac_vt->get_team_id(fac_p);
            size_t e_count = 0u;
            float damp, speed_sq, max_sq, final_speed_sq;

            (void)st->api.iter_reset(emit_iter);
            while (st->api.iter_next_chunk(emit_iter, &e_count) != BAM_FALSE && e_count > 0u) {
                const uint8_t* e_tf_col   = st->api.iter_column_readonly(emit_iter, 0u);
                const uint8_t* e_aura_col = st->api.iter_column_readonly(emit_iter, 1u);
                const uint8_t* e_fac_col  = st->api.iter_column_readonly(emit_iter, 2u);
                size_t e;

                for (e = 0u; e < e_count; ++e) {
                    const void* e_tf_p   = e_tf_col   + e * st->tf_vt->header.size;
                    const void* e_aura_p = e_aura_col + e * st->aura_vt->header.size;
                    const void* e_fac_p  = e_fac_col  + e * st->fac_vt->header.size;

                    const void* e_pos_p = st->tf_vt->ptr_position(e_tf_p);
                    float dx = ux - st->v3_vt->get_x(e_pos_p);
                    float dy = uy - st->v3_vt->get_y(e_pos_p);
                    float dz = uz - st->v3_vt->get_z(e_pos_p);
                    float dist_sq = dx * dx + dy * dy + dz * dz + 0.25f;
                    float radius = st->aura_vt->get_radius(e_aura_p);

                    if (dist_sq < radius * radius) {
                        float dist = sqrtf(dist_sq);
                        float inv_dist = 1.0f / dist;
                        float falloff = st->aura_vt->get_falloff(e_aura_p);
                        float norm = 1.0f - (dist / radius);
                        float atten = norm / (1.0f + falloff * dist_sq);
                        float force = st->aura_vt->get_force(e_aura_p);
                        uint32_t e_team = st->fac_vt->get_team_id(e_fac_p);

                        if (u_team == e_team) {
                            const void* e_fwd_p = st->tf_vt->ptr_forward(e_tf_p);
                            float boost = force * 0.25f * atten * SIM_DT;
                            float heal_hps, heal, nh, ne;
                            vx += st->v3_vt->get_x(e_fwd_p) * boost;
                            vy += st->v3_vt->get_y(e_fwd_p) * boost;
                            vz += st->v3_vt->get_z(e_fwd_p) * boost;

                            heal_hps = st->aura_vt->get_heal_hps(e_aura_p);
                            heal = heal_hps * atten * SIM_DT * (1.0f + 0.1f * r_ion);
                            nh = health + heal;
                            health = (nh > max_health) ? max_health : nh;

                            ne = energy + 2.5f * atten * SIM_DT;
                            energy = (ne > 100.0f) ? 100.0f : ne;
                        } else {
                            float imp = force * atten * SIM_DT * (1.0f - 0.5f * r_kin) * inv_dist;
                            float tdps, raw_dmg, absorbed, nh, drain, ne;
                            vx += dx * imp;
                            vy += dy * imp;
                            vz += dz * imp;

                            tdps = st->aura_vt->get_thermal_dps(e_aura_p);
                            raw_dmg = tdps * atten * SIM_DT * (1.0f - 0.6f * r_thm);
                            absorbed = (shield >= raw_dmg) ? raw_dmg : shield;
                            shield -= absorbed;
                            nh = health - (raw_dmg - absorbed);
                            health = (nh < 0.0f) ? 0.0f : nh;

                            drain = 1.5f * atten * SIM_DT * (1.0f - 0.4f * r_ion);
                            ne = energy - drain;
                            energy = (ne < 0.0f) ? 0.0f : ne;
                        }
                    }
                }
            }

            damp = 1.0f / (1.0f + drag * SIM_DT);
            vx *= damp;
            vy *= damp;
            vz *= damp;

            speed_sq = vx * vx + vy * vy + vz * vz;
            max_sq = max_speed * max_speed;
            if (speed_sq > max_sq && speed_sq > 1.0e-12f) {
                float sc = max_speed / sqrtf(speed_sq);
                vx *= sc;
                vy *= sc;
                vz *= sc;
            }

            (void)st->v3_vt->set_x(vel_p, vx);
            (void)st->v3_vt->set_y(vel_p, vy);
            (void)st->v3_vt->set_z(vel_p, vz);

            (void)st->v3_vt->set_x(pos_p, ux + vx * SIM_DT);
            (void)st->v3_vt->set_y(pos_p, uy + vy * SIM_DT);
            (void)st->v3_vt->set_z(pos_p, uz + vz * SIM_DT);

            final_speed_sq = vx * vx + vy * vy + vz * vz;
            if (final_speed_sq > 1.0e-6f) {
                float inv_s = 1.0f / sqrtf(final_speed_sq);
                void* fwd_p = st->tf_vt->ptr_mut_forward(tf_p);
                (void)st->v3_vt->set_x(fwd_p, vx * inv_s);
                (void)st->v3_vt->set_y(fwd_p, vy * inv_s);
                (void)st->v3_vt->set_z(fwd_p, vz * inv_s);
            }

            (void)st->vit_vt->set_health(vit_p, health);
            (void)st->vit_vt->set_shield(vit_p, shield);
            (void)st->vit_vt->set_energy(vit_p, energy);
        }
    }

    return BAM_RESULT_OK;
}

/* ============================================================================
 * Variant 2: Naive Chatty FFI Anti-Pattern (Per-Pair VTable Read/Write + Tick)
 * ============================================================================ */

static BamResult c99_sys_naive_chatty_ffi(BamSystemCtx* sys_ctx, void* user_data) {
    ModState* st = (ModState*)user_data;
    BamIter* unit_iter = NULL;
    BamIter* emit_iter = NULL;
    size_t u_count = 0u;
    BamResult res;

    if (st == NULL) return BAM_RESULT_NULL_POINTER;
    res = st->api.sys_query(sys_ctx, 0u, &unit_iter);
    if (res != BAM_RESULT_OK) return res;
    res = st->api.sys_query(sys_ctx, 1u, &emit_iter);
    if (res != BAM_RESULT_OK) return res;

    while (st->api.iter_next_chunk(unit_iter, &u_count) != BAM_FALSE && u_count > 0u) {
        uint8_t*       u_tf_col  = st->api.iter_column_mutable_untracked(unit_iter, 0u);
        uint8_t*       u_kin_col = st->api.iter_column_mutable_untracked(unit_iter, 1u);
        uint8_t*       u_vit_col = st->api.iter_column_mutable_untracked(unit_iter, 2u);
        const uint8_t* u_res_col = st->api.iter_column_readonly(unit_iter, 3u);
        const uint8_t* u_fac_col = st->api.iter_column_readonly(unit_iter, 4u);

        BamColumnInfo info0, info1, info2, info3, info4;
        size_t u;
        (void)st->api.iter_column_info(unit_iter, 0u, &info0);
        (void)st->api.iter_column_info(unit_iter, 1u, &info1);
        (void)st->api.iter_column_info(unit_iter, 2u, &info2);
        (void)st->api.iter_column_info(unit_iter, 3u, &info3);
        (void)st->api.iter_column_info(unit_iter, 4u, &info4);

        for (u = 0u; u < u_count; ++u) {
            void*       tf_p  = u_tf_col  + u * info0.size;
            void*       kin_p = u_kin_col + u * info1.size;
            void*       vit_p = u_vit_col + u * info2.size;
            const void* res_p = u_res_col + u * info3.size;
            const void* fac_p = u_fac_col + u * info4.size;
            size_t e_count = 0u;
            void* vel_mut;
            void* pos_mut;
            float drag, max_speed, damp, vx, vy, vz, speed_sq, max_sq, final_speed_sq;

            (void)st->api.iter_reset(emit_iter);
            while (st->api.iter_next_chunk(emit_iter, &e_count) != BAM_FALSE && e_count > 0u) {
                const uint8_t* e_tf_col   = st->api.iter_column_readonly(emit_iter, 0u);
                const uint8_t* e_aura_col = st->api.iter_column_readonly(emit_iter, 1u);
                const uint8_t* e_fac_col  = st->api.iter_column_readonly(emit_iter, 2u);
                size_t e;

                for (e = 0u; e < e_count; ++e) {
                    const void* e_tf_p   = e_tf_col   + e * st->tf_vt->header.size;
                    const void* e_aura_p = e_aura_col + e * st->aura_vt->header.size;
                    const void* e_fac_p  = e_fac_col  + e * st->fac_vt->header.size;

                    const void* u_pos_ro = st->tf_vt->ptr_position(tf_p);
                    const void* e_pos_ro = st->tf_vt->ptr_position(e_tf_p);

                    float dx = st->v3_vt->get_x(u_pos_ro) - st->v3_vt->get_x(e_pos_ro);
                    float dy = st->v3_vt->get_y(u_pos_ro) - st->v3_vt->get_y(e_pos_ro);
                    float dz = st->v3_vt->get_z(u_pos_ro) - st->v3_vt->get_z(e_pos_ro);
                    float dist_sq = dx * dx + dy * dy + dz * dz + 0.25f;
                    float radius = st->aura_vt->get_radius(e_aura_p);

                    if (dist_sq < radius * radius) {
                        float dist = sqrtf(dist_sq);
                        float inv_dist = 1.0f / dist;
                        float falloff = st->aura_vt->get_falloff(e_aura_p);
                        float norm = 1.0f - (dist / radius);
                        float atten = norm / (1.0f + falloff * dist_sq);
                        float force = st->aura_vt->get_force(e_aura_p);

                        uint32_t u_team = st->fac_vt->get_team_id(fac_p);
                        uint32_t e_team = st->fac_vt->get_team_id(e_fac_p);
                        void* v_mut = st->kin_vt->ptr_mut_velocity(kin_p);

                        if (u_team == e_team) {
                            const void* e_fwd_ro = st->tf_vt->ptr_forward(e_tf_p);
                            float boost = force * 0.25f * atten * SIM_DT;
                            float heal_hps, r_ion, heal, max_h, nh, ne;
                            (void)st->v3_vt->set_x(v_mut, st->v3_vt->get_x(v_mut) + st->v3_vt->get_x(e_fwd_ro) * boost);
                            (void)st->v3_vt->set_y(v_mut, st->v3_vt->get_y(v_mut) + st->v3_vt->get_y(e_fwd_ro) * boost);
                            (void)st->v3_vt->set_z(v_mut, st->v3_vt->get_z(v_mut) + st->v3_vt->get_z(e_fwd_ro) * boost);

                            heal_hps = st->aura_vt->get_heal_hps(e_aura_p);
                            r_ion = st->res_vt->get_ionic(res_p);
                            heal = heal_hps * atten * SIM_DT * (1.0f + 0.1f * r_ion);
                            max_h = st->vit_vt->get_max_health(vit_p);
                            nh = st->vit_vt->get_health(vit_p) + heal;
                            (void)st->vit_vt->set_health(vit_p, (nh > max_h) ? max_h : nh);

                            ne = st->vit_vt->get_energy(vit_p) + 2.5f * atten * SIM_DT;
                            (void)st->vit_vt->set_energy(vit_p, (ne > 100.0f) ? 100.0f : ne);
                        } else {
                            float r_kin = st->res_vt->get_kinetic(res_p);
                            float imp = force * atten * SIM_DT * (1.0f - 0.5f * r_kin) * inv_dist;
                            float tdps, r_thm, raw_dmg, cur_sh, absorbed, nh, r_ion, drain, ne;
                            (void)st->v3_vt->set_x(v_mut, st->v3_vt->get_x(v_mut) + dx * imp);
                            (void)st->v3_vt->set_y(v_mut, st->v3_vt->get_y(v_mut) + dy * imp);
                            (void)st->v3_vt->set_z(v_mut, st->v3_vt->get_z(v_mut) + dz * imp);

                            tdps = st->aura_vt->get_thermal_dps(e_aura_p);
                            r_thm = st->res_vt->get_thermal(res_p);
                            raw_dmg = tdps * atten * SIM_DT * (1.0f - 0.6f * r_thm);
                            cur_sh = st->vit_vt->get_shield(vit_p);
                            absorbed = (cur_sh >= raw_dmg) ? raw_dmg : cur_sh;
                            (void)st->vit_vt->set_shield(vit_p, cur_sh - absorbed);
                            nh = st->vit_vt->get_health(vit_p) - (raw_dmg - absorbed);
                            (void)st->vit_vt->set_health(vit_p, (nh < 0.0f) ? 0.0f : nh);

                            r_ion = st->res_vt->get_ionic(res_p);
                            drain = 1.5f * atten * SIM_DT * (1.0f - 0.4f * r_ion);
                            ne = st->vit_vt->get_energy(vit_p) - drain;
                            (void)st->vit_vt->set_energy(vit_p, (ne < 0.0f) ? 0.0f : ne);
                        }

                        (void)st->api.iter_mark_changed_range(unit_iter, 1u, u, 1u);
                        (void)st->api.iter_mark_changed_range(unit_iter, 2u, u, 1u);
                    }
                }
            }

            vel_mut = st->kin_vt->ptr_mut_velocity(kin_p);
            drag = st->kin_vt->get_drag(kin_p);
            max_speed = st->kin_vt->get_max_speed(kin_p);
            damp = 1.0f / (1.0f + drag * SIM_DT);

            vx = st->v3_vt->get_x(vel_mut) * damp;
            vy = st->v3_vt->get_y(vel_mut) * damp;
            vz = st->v3_vt->get_z(vel_mut) * damp;

            speed_sq = vx * vx + vy * vy + vz * vz;
            max_sq = max_speed * max_speed;
            if (speed_sq > max_sq && speed_sq > 1.0e-12f) {
                float sc = max_speed / sqrtf(speed_sq);
                vx *= sc;
                vy *= sc;
                vz *= sc;
            }

            (void)st->v3_vt->set_x(vel_mut, vx);
            (void)st->v3_vt->set_y(vel_mut, vy);
            (void)st->v3_vt->set_z(vel_mut, vz);

            pos_mut = st->tf_vt->ptr_mut_position(tf_p);
            (void)st->v3_vt->set_x(pos_mut, st->v3_vt->get_x(pos_mut) + vx * SIM_DT);
            (void)st->v3_vt->set_y(pos_mut, st->v3_vt->get_y(pos_mut) + vy * SIM_DT);
            (void)st->v3_vt->set_z(pos_mut, st->v3_vt->get_z(pos_mut) + vz * SIM_DT);

            final_speed_sq = vx * vx + vy * vy + vz * vz;
            if (final_speed_sq > 1.0e-6f) {
                float inv_s = 1.0f / sqrtf(final_speed_sq);
                void* fwd_mut = st->tf_vt->ptr_mut_forward(tf_p);
                (void)st->v3_vt->set_x(fwd_mut, vx * inv_s);
                (void)st->v3_vt->set_y(fwd_mut, vy * inv_s);
                (void)st->v3_vt->set_z(fwd_mut, vz * inv_s);
            }

            (void)st->api.iter_mark_changed_range(unit_iter, 0u, u, 1u);
        }
    }

    return BAM_RESULT_OK;
}

/* ============================================================================
 * Mandatory ABI 0.2 Lifecycle Entry Points (`L-02`, `L-08`)
 * ============================================================================ */

BAM_EXPORT const BamManifest* bevy_abi_mod_manifest(void) {
    return &MANIFEST;
}

BAM_EXPORT BamResult bevy_abi_mod_init(
    const BamHostApi* host_api,
    BamLoadCtx*       load_ctx,
    BamInstance**     instance_out
) {
    BamHostApi local_api;
    const BamHostCtx* host_ctx;
    ModState* st;
    BamComponent tf_h, kin_h, vit_h, res_h, aura_h, fac_h, unit_tag_h, emit_tag_h, frozen_h;
    BamTermDesc unit_clause[8];
    BamTermDesc emitter_clause[6];
    BamQueryDesc queries[2];
    BamSystemDesc desc;
    const char* sys_name = "c99_cabi_chunk_optimized";
    BamSystemFn cb = c99_sys_chunk_optimized;
    BamResult res;

    if (load_ctx == NULL || instance_out == NULL) return BAM_RESULT_NULL_POINTER;
    *instance_out = NULL;

    res = bam_host_api_copy_bounded(&local_api, sizeof(local_api), host_api);
    if (res != BAM_RESULT_OK) return res;

    st = (ModState*)calloc(1u, sizeof(ModState));
    if (st == NULL) return BAM_RESULT_INTERNAL_ERROR;
    st->api = local_api;

    host_ctx = st->api.load_host_ctx(load_ctx);
    if (host_ctx == NULL) {
        free(st);
        return BAM_RESULT_NULL_POINTER;
    }

    st->v3_vt   = (const Vec3fVTable*)st->api.get_proc_address(host_ctx, "bam.v0.shared_scenario::Vec3f.vtable");
    st->tf_vt   = (const Transform3DVTable*)st->api.get_proc_address(host_ctx, "bam.v0.shared_scenario::Transform3D.vtable");
    st->kin_vt  = (const KinematicsVTable*)st->api.get_proc_address(host_ctx, "bam.v0.shared_scenario::Kinematics.vtable");
    st->vit_vt  = (const VitalityVTable*)st->api.get_proc_address(host_ctx, "bam.v0.shared_scenario::Vitality.vtable");
    st->res_vt  = (const ResistanceVTable*)st->api.get_proc_address(host_ctx, "bam.v0.shared_scenario::Resistance.vtable");
    st->aura_vt = (const AuraEmitterVTable*)st->api.get_proc_address(host_ctx, "bam.v0.shared_scenario::AuraEmitter.vtable");
    st->fac_vt  = (const FactionVTable*)st->api.get_proc_address(host_ctx, "bam.v0.shared_scenario::Faction.vtable");

    if (!st->v3_vt || !st->tf_vt || !st->kin_vt || !st->vit_vt ||
        !st->res_vt || !st->aura_vt || !st->fac_vt) {
        free(st);
        return BAM_RESULT_INVALID_COMPONENT;
    }

    if (st->api.resolve_component(host_ctx, "shared_scenario::Transform3D", NULL, &tf_h) != BAM_RESULT_OK ||
        st->api.resolve_component(host_ctx, "shared_scenario::Kinematics", NULL, &kin_h) != BAM_RESULT_OK ||
        st->api.resolve_component(host_ctx, "shared_scenario::Vitality", NULL, &vit_h) != BAM_RESULT_OK ||
        st->api.resolve_component(host_ctx, "shared_scenario::Resistance", NULL, &res_h) != BAM_RESULT_OK ||
        st->api.resolve_component(host_ctx, "shared_scenario::AuraEmitter", NULL, &aura_h) != BAM_RESULT_OK ||
        st->api.resolve_component(host_ctx, "shared_scenario::Faction", NULL, &fac_h) != BAM_RESULT_OK ||
        st->api.resolve_component(host_ctx, "shared_scenario::CombatUnitTag", NULL, &unit_tag_h) != BAM_RESULT_OK ||
        st->api.resolve_component(host_ctx, "shared_scenario::EmitterBeaconTag", NULL, &emit_tag_h) != BAM_RESULT_OK ||
        st->api.resolve_component(host_ctx, "shared_scenario::FrozenFlag", NULL, &frozen_h) != BAM_RESULT_OK) {
        free(st);
        return BAM_RESULT_INVALID_COMPONENT;
    }

    unit_clause[0] = bam_term_desc(tf_h,       BAM_ACCESS_WRITE,       0u);
    unit_clause[1] = bam_term_desc(kin_h,      BAM_ACCESS_WRITE,       0u);
    unit_clause[2] = bam_term_desc(vit_h,      BAM_ACCESS_WRITE,       0u);
    unit_clause[3] = bam_term_desc(res_h,      BAM_ACCESS_READ,        0u);
    unit_clause[4] = bam_term_desc(fac_h,      BAM_ACCESS_READ,        0u);
    unit_clause[5] = bam_term_desc(unit_tag_h, BAM_ACCESS_FILTER_ONLY, 0u);
    unit_clause[6] = bam_term_desc(emit_tag_h, BAM_ACCESS_WITHOUT,     0u);
    unit_clause[7] = bam_term_desc(frozen_h,   BAM_ACCESS_WITHOUT,     0u);

    emitter_clause[0] = bam_term_desc(tf_h,       BAM_ACCESS_READ,        0u);
    emitter_clause[1] = bam_term_desc(aura_h,     BAM_ACCESS_READ,        0u);
    emitter_clause[2] = bam_term_desc(fac_h,      BAM_ACCESS_READ,        0u);
    emitter_clause[3] = bam_term_desc(emit_tag_h, BAM_ACCESS_FILTER_ONLY, 0u);
    emitter_clause[4] = bam_term_desc(unit_tag_h, BAM_ACCESS_WITHOUT,     0u);
    emitter_clause[5] = bam_term_desc(frozen_h,   BAM_ACCESS_WITHOUT,     0u);

    memset(queries, 0, sizeof(queries));
    queries[0].struct_size = (uint32_t)sizeof(BamQueryDesc);
    queries[0].terms       = unit_clause;
    queries[0].term_count  = 8u;

    queries[1].struct_size = (uint32_t)sizeof(BamQueryDesc);
    queries[1].terms       = emitter_clause;
    queries[1].term_count  = 6u;

    if (g_variant == 1u) {
        sys_name = "c99_cabi_entity_nested_vtable";
        cb = c99_sys_entity_nested_vtable;
    } else if (g_variant >= 2u) {
        sys_name = "c99_cabi_naive_chatty_ffi";
        cb = c99_sys_naive_chatty_ffi;
    }

    memset(&desc, 0, sizeof(desc));
    desc.struct_size = (uint32_t)sizeof(BamSystemDesc);
    desc.name        = sys_name;
    desc.queries     = queries;
    desc.query_count = 2u;
    desc.callback    = cb;
    desc.user_data   = st;
    desc.schedule    = BAM_SCHEDULE_UPDATE;

    res = st->api.register_system(load_ctx, &desc);
    if (res != BAM_RESULT_OK) {
        free(st);
        return res;
    }

    *instance_out = (BamInstance*)st;
    return BAM_RESULT_OK;
}

BAM_EXPORT void bevy_abi_mod_shutdown(BamInstance* instance) {
    if (instance != NULL) {
        free(instance);
    }
}
