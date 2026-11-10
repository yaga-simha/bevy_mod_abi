#include "../../include/bevy_abi_mod.h"
#include <math.h>
#include <stdlib.h>

#define SIM_DT 0.016f
#define MAX_STACK_EMITTERS 512

/* ============================================================================
 * 1. VTable Mirror Structs Matching `#[derive(ModExposable)]`
 * ============================================================================ */

typedef struct Vec3fVTable {
    size_t size;
    size_t align;
    float (*get_x)(const void* ptr);
    void  (*set_x)(void* ptr, float val);
    float (*get_y)(const void* ptr);
    void  (*set_y)(void* ptr, float val);
    float (*get_z)(const void* ptr);
    void  (*set_z)(void* ptr, float val);
} Vec3fVTable;

typedef struct Transform3DVTable {
    size_t size;
    size_t align;
    const void* (*ptr_position)(const void* ptr);
    void*       (*ptr_mut_position)(void* ptr);
    const void* (*ptr_forward)(const void* ptr);
    void*       (*ptr_mut_forward)(void* ptr);
    float       (*get_scale)(const void* ptr);
    void        (*set_scale)(void* ptr, float val);
} Transform3DVTable;

typedef struct KinematicsVTable {
    size_t size;
    size_t align;
    const void* (*ptr_velocity)(const void* ptr);
    void*       (*ptr_mut_velocity)(void* ptr);
    float       (*get_drag)(const void* ptr);
    void        (*set_drag)(void* ptr, float val);
    float       (*get_max_speed)(const void* ptr);
    void        (*set_max_speed)(void* ptr, float val);
} KinematicsVTable;

typedef struct VitalityVTable {
    size_t size;
    size_t align;
    float (*get_health)(const void* ptr);
    void  (*set_health)(void* ptr, float val);
    float (*get_max_health)(const void* ptr);
    void  (*set_max_health)(void* ptr, float val);
    float (*get_shield)(const void* ptr);
    void  (*set_shield)(void* ptr, float val);
    float (*get_energy)(const void* ptr);
    void  (*set_energy)(void* ptr, float val);
} VitalityVTable;

typedef struct ResistanceVTable {
    size_t size;
    size_t align;
    float (*get_kinetic)(const void* ptr);
    void  (*set_kinetic)(void* ptr, float val);
    float (*get_thermal)(const void* ptr);
    void  (*set_thermal)(void* ptr, float val);
    float (*get_ionic)(const void* ptr);
    void  (*set_ionic)(void* ptr, float val);
} ResistanceVTable;

typedef struct AuraEmitterVTable {
    size_t size;
    size_t align;
    float (*get_radius)(const void* ptr);
    void  (*set_radius)(void* ptr, float val);
    float (*get_force)(const void* ptr);
    void  (*set_force)(void* ptr, float val);
    float (*get_thermal_dps)(const void* ptr);
    void  (*set_thermal_dps)(void* ptr, float val);
    float (*get_heal_hps)(const void* ptr);
    void  (*set_heal_hps)(void* ptr, float val);
    float (*get_falloff)(const void* ptr);
    void  (*set_falloff)(void* ptr, float val);
} AuraEmitterVTable;

typedef struct FactionVTable {
    size_t size;
    size_t align;
    uint32_t (*get_team_id)(const void* ptr);
    void     (*set_team_id)(void* ptr, uint32_t val);
    uint32_t (*get_serial_id)(const void* ptr);
    void     (*set_serial_id)(void* ptr, uint32_t val);
} FactionVTable;

/* ============================================================================
 * 2. Mod Static State
 * ============================================================================ */

static uint32_t g_variant = 0u;
static const HostApi* g_api = NULL;
static IterResetFn g_iter_reset = NULL;

static const Vec3fVTable*       g_v3_vt   = NULL;
static const Transform3DVTable* g_tf_vt   = NULL;
static const KinematicsVTable*  g_kin_vt  = NULL;
static const VitalityVTable*    g_vit_vt  = NULL;
static const ResistanceVTable*  g_res_vt  = NULL;
static const AuraEmitterVTable* g_aura_vt = NULL;
static const FactionVTable*     g_fac_vt  = NULL;

BEVY_ABI_MOD_EXPORT void c_abi_c99_mod_set_variant(uint32_t variant) {
    g_variant = variant;
}

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
 * Variant 0: Chunk-Optimized Nested Query ($O(U + E)$ C-ABI calls)
 * ============================================================================ */

static FfiResult c99_sys_chunk_optimized(
    RawChunkIter* const* iters,
    size_t iter_count,
    RawCommandQueue* commands,
    void* user_data
) {
    (void)commands;
    (void)user_data;
    if (iter_count < 2 || iters == NULL) return FFI_RESULT_INTERNAL_ERROR;

    RawChunkIter* unit_iter = iters[0];
    RawChunkIter* emit_iter = iters[1];

    UnpackedEmitter stack_buf[MAX_STACK_EMITTERS];
    UnpackedEmitter* heap_buf = NULL;
    size_t cap = MAX_STACK_EMITTERS;
    size_t total_emitters = 0;

    size_t e_count;
    while ((e_count = g_api->iter_next_chunk(emit_iter)) > 0) {
        const uint8_t* e_tf_col   = g_api->iter_column_readonly(emit_iter, 0);
        const uint8_t* e_aura_col = g_api->iter_column_readonly(emit_iter, 1);
        const uint8_t* e_fac_col  = g_api->iter_column_readonly(emit_iter, 2);

        for (size_t r = 0; r < e_count; ++r) {
            const void* tf_p   = e_tf_col   + r * g_tf_vt->size;
            const void* aura_p = e_aura_col + r * g_aura_vt->size;
            const void* fac_p  = e_fac_col  + r * g_fac_vt->size;

            const void* pos_p = g_tf_vt->ptr_position(tf_p);
            const void* fwd_p = g_tf_vt->ptr_forward(tf_p);
            float radius = g_aura_vt->get_radius(aura_p);

            UnpackedEmitter em;
            em.px = g_v3_vt->get_x(pos_p);
            em.py = g_v3_vt->get_y(pos_p);
            em.pz = g_v3_vt->get_z(pos_p);
            em.fx = g_v3_vt->get_x(fwd_p);
            em.fy = g_v3_vt->get_y(fwd_p);
            em.fz = g_v3_vt->get_z(fwd_p);
            em.radius = radius;
            em.rad_sq = radius * radius;
            em.force = g_aura_vt->get_force(aura_p);
            em.thermal_dps = g_aura_vt->get_thermal_dps(aura_p);
            em.heal_hps = g_aura_vt->get_heal_hps(aura_p);
            em.falloff = g_aura_vt->get_falloff(aura_p);
            em.team_id = g_fac_vt->get_team_id(fac_p);

            if (total_emitters < cap) {
                if (heap_buf != NULL) {
                    heap_buf[total_emitters] = em;
                } else {
                    stack_buf[total_emitters] = em;
                }
            } else {
                size_t new_cap = cap * 2;
                UnpackedEmitter* nb = (UnpackedEmitter*)malloc(new_cap * sizeof(UnpackedEmitter));
                if (nb == NULL) {
                    if (heap_buf != NULL) free(heap_buf);
                    return FFI_RESULT_INTERNAL_ERROR;
                }
                const UnpackedEmitter* src = (heap_buf != NULL) ? heap_buf : stack_buf;
                for (size_t k = 0; k < total_emitters; ++k) nb[k] = src[k];
                if (heap_buf != NULL) free(heap_buf);
                heap_buf = nb;
                cap = new_cap;
                heap_buf[total_emitters] = em;
            }
            total_emitters++;
        }
    }

    const UnpackedEmitter* emitters = (heap_buf != NULL) ? heap_buf : stack_buf;

    size_t u_count;
    while ((u_count = g_api->iter_next_chunk(unit_iter)) > 0) {
        uint8_t*       u_tf_col  = g_api->iter_column_mutable(unit_iter, 0);
        uint8_t*       u_kin_col = g_api->iter_column_mutable(unit_iter, 1);
        uint8_t*       u_vit_col = g_api->iter_column_mutable(unit_iter, 2);
        const uint8_t* u_res_col = g_api->iter_column_readonly(unit_iter, 3);
        const uint8_t* u_fac_col = g_api->iter_column_readonly(unit_iter, 4);

        for (size_t u = 0; u < u_count; ++u) {
            void*       tf_p  = u_tf_col  + u * g_tf_vt->size;
            void*       kin_p = u_kin_col + u * g_kin_vt->size;
            void*       vit_p = u_vit_col + u * g_vit_vt->size;
            const void* res_p = u_res_col + u * g_res_vt->size;
            const void* fac_p = u_fac_col + u * g_fac_vt->size;

            void* pos_p = g_tf_vt->ptr_mut_position(tf_p);
            float ux = g_v3_vt->get_x(pos_p);
            float uy = g_v3_vt->get_y(pos_p);
            float uz = g_v3_vt->get_z(pos_p);

            void* vel_p = g_kin_vt->ptr_mut_velocity(kin_p);
            float vx = g_v3_vt->get_x(vel_p);
            float vy = g_v3_vt->get_y(vel_p);
            float vz = g_v3_vt->get_z(vel_p);
            float drag = g_kin_vt->get_drag(kin_p);
            float max_speed = g_kin_vt->get_max_speed(kin_p);

            float health = g_vit_vt->get_health(vit_p);
            float max_health = g_vit_vt->get_max_health(vit_p);
            float shield = g_vit_vt->get_shield(vit_p);
            float energy = g_vit_vt->get_energy(vit_p);

            float r_kin = g_res_vt->get_kinetic(res_p);
            float r_thm = g_res_vt->get_thermal(res_p);
            float r_ion = g_res_vt->get_ionic(res_p);
            uint32_t u_team = g_fac_vt->get_team_id(fac_p);

            for (size_t e = 0; e < total_emitters; ++e) {
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
                        vx += em->fx * boost;
                        vy += em->fy * boost;
                        vz += em->fz * boost;

                        float heal = em->heal_hps * atten * SIM_DT * (1.0f + 0.1f * r_ion);
                        float nh = health + heal;
                        health = (nh > max_health) ? max_health : nh;

                        float ne = energy + 2.5f * atten * SIM_DT;
                        energy = (ne > 100.0f) ? 100.0f : ne;
                    } else {
                        float imp = em->force * atten * SIM_DT * (1.0f - 0.5f * r_kin) * inv_dist;
                        vx += dx * imp;
                        vy += dy * imp;
                        vz += dz * imp;

                        float raw_dmg = em->thermal_dps * atten * SIM_DT * (1.0f - 0.6f * r_thm);
                        float absorbed = (shield >= raw_dmg) ? raw_dmg : shield;
                        shield -= absorbed;
                        float nh = health - (raw_dmg - absorbed);
                        health = (nh < 0.0f) ? 0.0f : nh;

                        float drain = 1.5f * atten * SIM_DT * (1.0f - 0.4f * r_ion);
                        float ne = energy - drain;
                        energy = (ne < 0.0f) ? 0.0f : ne;
                    }
                }
            }

            float damp = 1.0f / (1.0f + drag * SIM_DT);
            vx *= damp;
            vy *= damp;
            vz *= damp;

            float speed_sq = vx * vx + vy * vy + vz * vz;
            float max_sq = max_speed * max_speed;
            if (speed_sq > max_sq && speed_sq > 1.0e-12f) {
                float sc = max_speed / sqrtf(speed_sq);
                vx *= sc;
                vy *= sc;
                vz *= sc;
            }

            g_v3_vt->set_x(vel_p, vx);
            g_v3_vt->set_y(vel_p, vy);
            g_v3_vt->set_z(vel_p, vz);

            g_v3_vt->set_x(pos_p, ux + vx * SIM_DT);
            g_v3_vt->set_y(pos_p, uy + vy * SIM_DT);
            g_v3_vt->set_z(pos_p, uz + vz * SIM_DT);

            float final_speed_sq = vx * vx + vy * vy + vz * vz;
            if (final_speed_sq > 1.0e-6f) {
                float inv_s = 1.0f / sqrtf(final_speed_sq);
                void* fwd_p = g_tf_vt->ptr_mut_forward(tf_p);
                g_v3_vt->set_x(fwd_p, vx * inv_s);
                g_v3_vt->set_y(fwd_p, vy * inv_s);
                g_v3_vt->set_z(fwd_p, vz * inv_s);
            }

            g_vit_vt->set_health(vit_p, health);
            g_vit_vt->set_shield(vit_p, shield);
            g_vit_vt->set_energy(vit_p, energy);
        }
    }

    if (heap_buf != NULL) free(heap_buf);
    return FFI_RESULT_OK;
}

/* ============================================================================
 * Variant 1: Entity-Nested VTable-per-Pair ($O(U \times E)$ C-ABI getter calls)
 * ============================================================================ */

static FfiResult c99_sys_entity_nested_vtable(
    RawChunkIter* const* iters,
    size_t iter_count,
    RawCommandQueue* commands,
    void* user_data
) {
    (void)commands;
    (void)user_data;
    if (iter_count < 2 || iters == NULL) return FFI_RESULT_INTERNAL_ERROR;

    RawChunkIter* unit_iter = iters[0];
    RawChunkIter* emit_iter = iters[1];

    size_t u_count;
    while ((u_count = g_api->iter_next_chunk(unit_iter)) > 0) {
        uint8_t*       u_tf_col  = g_api->iter_column_mutable(unit_iter, 0);
        uint8_t*       u_kin_col = g_api->iter_column_mutable(unit_iter, 1);
        uint8_t*       u_vit_col = g_api->iter_column_mutable(unit_iter, 2);
        const uint8_t* u_res_col = g_api->iter_column_readonly(unit_iter, 3);
        const uint8_t* u_fac_col = g_api->iter_column_readonly(unit_iter, 4);

        for (size_t u = 0; u < u_count; ++u) {
            void*       tf_p  = u_tf_col  + u * g_tf_vt->size;
            void*       kin_p = u_kin_col + u * g_kin_vt->size;
            void*       vit_p = u_vit_col + u * g_vit_vt->size;
            const void* res_p = u_res_col + u * g_res_vt->size;
            const void* fac_p = u_fac_col + u * g_fac_vt->size;

            void* pos_p = g_tf_vt->ptr_mut_position(tf_p);
            float ux = g_v3_vt->get_x(pos_p);
            float uy = g_v3_vt->get_y(pos_p);
            float uz = g_v3_vt->get_z(pos_p);

            void* vel_p = g_kin_vt->ptr_mut_velocity(kin_p);
            float vx = g_v3_vt->get_x(vel_p);
            float vy = g_v3_vt->get_y(vel_p);
            float vz = g_v3_vt->get_z(vel_p);
            float drag = g_kin_vt->get_drag(kin_p);
            float max_speed = g_kin_vt->get_max_speed(kin_p);

            float health = g_vit_vt->get_health(vit_p);
            float max_health = g_vit_vt->get_max_health(vit_p);
            float shield = g_vit_vt->get_shield(vit_p);
            float energy = g_vit_vt->get_energy(vit_p);

            float r_kin = g_res_vt->get_kinetic(res_p);
            float r_thm = g_res_vt->get_thermal(res_p);
            float r_ion = g_res_vt->get_ionic(res_p);
            uint32_t u_team = g_fac_vt->get_team_id(fac_p);

            g_iter_reset(emit_iter);
            size_t e_count;
            while ((e_count = g_api->iter_next_chunk(emit_iter)) > 0) {
                const uint8_t* e_tf_col   = g_api->iter_column_readonly(emit_iter, 0);
                const uint8_t* e_aura_col = g_api->iter_column_readonly(emit_iter, 1);
                const uint8_t* e_fac_col  = g_api->iter_column_readonly(emit_iter, 2);

                for (size_t e = 0; e < e_count; ++e) {
                    const void* e_tf_p   = e_tf_col   + e * g_tf_vt->size;
                    const void* e_aura_p = e_aura_col + e * g_aura_vt->size;
                    const void* e_fac_p  = e_fac_col  + e * g_fac_vt->size;

                    const void* e_pos_p = g_tf_vt->ptr_position(e_tf_p);
                    float dx = ux - g_v3_vt->get_x(e_pos_p);
                    float dy = uy - g_v3_vt->get_y(e_pos_p);
                    float dz = uz - g_v3_vt->get_z(e_pos_p);
                    float dist_sq = dx * dx + dy * dy + dz * dz + 0.25f;
                    float radius = g_aura_vt->get_radius(e_aura_p);

                    if (dist_sq < radius * radius) {
                        float dist = sqrtf(dist_sq);
                        float inv_dist = 1.0f / dist;
                        float falloff = g_aura_vt->get_falloff(e_aura_p);
                        float norm = 1.0f - (dist / radius);
                        float atten = norm / (1.0f + falloff * dist_sq);
                        float force = g_aura_vt->get_force(e_aura_p);
                        uint32_t e_team = g_fac_vt->get_team_id(e_fac_p);

                        if (u_team == e_team) {
                            const void* e_fwd_p = g_tf_vt->ptr_forward(e_tf_p);
                            float boost = force * 0.25f * atten * SIM_DT;
                            vx += g_v3_vt->get_x(e_fwd_p) * boost;
                            vy += g_v3_vt->get_y(e_fwd_p) * boost;
                            vz += g_v3_vt->get_z(e_fwd_p) * boost;

                            float heal_hps = g_aura_vt->get_heal_hps(e_aura_p);
                            float heal = heal_hps * atten * SIM_DT * (1.0f + 0.1f * r_ion);
                            float nh = health + heal;
                            health = (nh > max_health) ? max_health : nh;

                            float ne = energy + 2.5f * atten * SIM_DT;
                            energy = (ne > 100.0f) ? 100.0f : ne;
                        } else {
                            float imp = force * atten * SIM_DT * (1.0f - 0.5f * r_kin) * inv_dist;
                            vx += dx * imp;
                            vy += dy * imp;
                            vz += dz * imp;

                            float tdps = g_aura_vt->get_thermal_dps(e_aura_p);
                            float raw_dmg = tdps * atten * SIM_DT * (1.0f - 0.6f * r_thm);
                            float absorbed = (shield >= raw_dmg) ? raw_dmg : shield;
                            shield -= absorbed;
                            float nh = health - (raw_dmg - absorbed);
                            health = (nh < 0.0f) ? 0.0f : nh;

                            float drain = 1.5f * atten * SIM_DT * (1.0f - 0.4f * r_ion);
                            float ne = energy - drain;
                            energy = (ne < 0.0f) ? 0.0f : ne;
                        }
                    }
                }
            }

            float damp = 1.0f / (1.0f + drag * SIM_DT);
            vx *= damp;
            vy *= damp;
            vz *= damp;

            float speed_sq = vx * vx + vy * vy + vz * vz;
            float max_sq = max_speed * max_speed;
            if (speed_sq > max_sq && speed_sq > 1.0e-12f) {
                float sc = max_speed / sqrtf(speed_sq);
                vx *= sc;
                vy *= sc;
                vz *= sc;
            }

            g_v3_vt->set_x(vel_p, vx);
            g_v3_vt->set_y(vel_p, vy);
            g_v3_vt->set_z(vel_p, vz);

            g_v3_vt->set_x(pos_p, ux + vx * SIM_DT);
            g_v3_vt->set_y(pos_p, uy + vy * SIM_DT);
            g_v3_vt->set_z(pos_p, uz + vz * SIM_DT);

            float final_speed_sq = vx * vx + vy * vy + vz * vz;
            if (final_speed_sq > 1.0e-6f) {
                float inv_s = 1.0f / sqrtf(final_speed_sq);
                void* fwd_p = g_tf_vt->ptr_mut_forward(tf_p);
                g_v3_vt->set_x(fwd_p, vx * inv_s);
                g_v3_vt->set_y(fwd_p, vy * inv_s);
                g_v3_vt->set_z(fwd_p, vz * inv_s);
            }

            g_vit_vt->set_health(vit_p, health);
            g_vit_vt->set_shield(vit_p, shield);
            g_vit_vt->set_energy(vit_p, energy);
        }
    }

    return FFI_RESULT_OK;
}

/* ============================================================================
 * Variant 2: Naive Chatty FFI Anti-Pattern (Per-Pair VTable Read/Write + Tick)
 * ============================================================================ */

static FfiResult c99_sys_naive_chatty_ffi(
    RawChunkIter* const* iters,
    size_t iter_count,
    RawCommandQueue* commands,
    void* user_data
) {
    (void)commands;
    (void)user_data;
    if (iter_count < 2 || iters == NULL) return FFI_RESULT_INTERNAL_ERROR;

    RawChunkIter* unit_iter = iters[0];
    RawChunkIter* emit_iter = iters[1];

    size_t u_count;
    while ((u_count = g_api->iter_next_chunk(unit_iter)) > 0) {
        uint8_t*       u_tf_col  = g_api->iter_column_mutable(unit_iter, 0);
        uint8_t*       u_kin_col = g_api->iter_column_mutable(unit_iter, 1);
        uint8_t*       u_vit_col = g_api->iter_column_mutable(unit_iter, 2);
        const uint8_t* u_res_col = g_api->iter_column_readonly(unit_iter, 3);
        const uint8_t* u_fac_col = g_api->iter_column_readonly(unit_iter, 4);

        size_t u_tf_stride  = g_api->iter_column_stride(unit_iter, 0);
        size_t u_kin_stride = g_api->iter_column_stride(unit_iter, 1);
        size_t u_vit_stride = g_api->iter_column_stride(unit_iter, 2);
        size_t u_res_stride = g_api->iter_column_stride(unit_iter, 3);
        size_t u_fac_stride = g_api->iter_column_stride(unit_iter, 4);

        for (size_t u = 0; u < u_count; ++u) {
            void*       tf_p  = u_tf_col  + u * u_tf_stride;
            void*       kin_p = u_kin_col + u * u_kin_stride;
            void*       vit_p = u_vit_col + u * u_vit_stride;
            const void* res_p = u_res_col + u * u_res_stride;
            const void* fac_p = u_fac_col + u * u_fac_stride;

            g_iter_reset(emit_iter);
            size_t e_count;
            while ((e_count = g_api->iter_next_chunk(emit_iter)) > 0) {
                const uint8_t* e_tf_col   = g_api->iter_column_readonly(emit_iter, 0);
                const uint8_t* e_aura_col = g_api->iter_column_readonly(emit_iter, 1);
                const uint8_t* e_fac_col  = g_api->iter_column_readonly(emit_iter, 2);

                for (size_t e = 0; e < e_count; ++e) {
                    const void* e_tf_p   = e_tf_col   + e * g_tf_vt->size;
                    const void* e_aura_p = e_aura_col + e * g_aura_vt->size;
                    const void* e_fac_p  = e_fac_col  + e * g_fac_vt->size;

                    const void* u_pos_ro = g_tf_vt->ptr_position(tf_p);
                    const void* e_pos_ro = g_tf_vt->ptr_position(e_tf_p);

                    float dx = g_v3_vt->get_x(u_pos_ro) - g_v3_vt->get_x(e_pos_ro);
                    float dy = g_v3_vt->get_y(u_pos_ro) - g_v3_vt->get_y(e_pos_ro);
                    float dz = g_v3_vt->get_z(u_pos_ro) - g_v3_vt->get_z(e_pos_ro);
                    float dist_sq = dx * dx + dy * dy + dz * dz + 0.25f;
                    float radius = g_aura_vt->get_radius(e_aura_p);

                    if (dist_sq < radius * radius) {
                        float dist = sqrtf(dist_sq);
                        float inv_dist = 1.0f / dist;
                        float falloff = g_aura_vt->get_falloff(e_aura_p);
                        float norm = 1.0f - (dist / radius);
                        float atten = norm / (1.0f + falloff * dist_sq);
                        float force = g_aura_vt->get_force(e_aura_p);

                        uint32_t u_team = g_fac_vt->get_team_id(fac_p);
                        uint32_t e_team = g_fac_vt->get_team_id(e_fac_p);
                        void* vel_mut = g_kin_vt->ptr_mut_velocity(kin_p);

                        if (u_team == e_team) {
                            const void* e_fwd_ro = g_tf_vt->ptr_forward(e_tf_p);
                            float boost = force * 0.25f * atten * SIM_DT;
                            g_v3_vt->set_x(vel_mut, g_v3_vt->get_x(vel_mut) + g_v3_vt->get_x(e_fwd_ro) * boost);
                            g_v3_vt->set_y(vel_mut, g_v3_vt->get_y(vel_mut) + g_v3_vt->get_y(e_fwd_ro) * boost);
                            g_v3_vt->set_z(vel_mut, g_v3_vt->get_z(vel_mut) + g_v3_vt->get_z(e_fwd_ro) * boost);

                            float heal_hps = g_aura_vt->get_heal_hps(e_aura_p);
                            float r_ion = g_res_vt->get_ionic(res_p);
                            float heal = heal_hps * atten * SIM_DT * (1.0f + 0.1f * r_ion);
                            float max_h = g_vit_vt->get_max_health(vit_p);
                            float nh = g_vit_vt->get_health(vit_p) + heal;
                            g_vit_vt->set_health(vit_p, (nh > max_h) ? max_h : nh);

                            float ne = g_vit_vt->get_energy(vit_p) + 2.5f * atten * SIM_DT;
                            g_vit_vt->set_energy(vit_p, (ne > 100.0f) ? 100.0f : ne);
                        } else {
                            float r_kin = g_res_vt->get_kinetic(res_p);
                            float imp = force * atten * SIM_DT * (1.0f - 0.5f * r_kin) * inv_dist;
                            g_v3_vt->set_x(vel_mut, g_v3_vt->get_x(vel_mut) + dx * imp);
                            g_v3_vt->set_y(vel_mut, g_v3_vt->get_y(vel_mut) + dy * imp);
                            g_v3_vt->set_z(vel_mut, g_v3_vt->get_z(vel_mut) + dz * imp);

                            float tdps = g_aura_vt->get_thermal_dps(e_aura_p);
                            float r_thm = g_res_vt->get_thermal(res_p);
                            float raw_dmg = tdps * atten * SIM_DT * (1.0f - 0.6f * r_thm);
                            float cur_sh = g_vit_vt->get_shield(vit_p);
                            float absorbed = (cur_sh >= raw_dmg) ? raw_dmg : cur_sh;
                            g_vit_vt->set_shield(vit_p, cur_sh - absorbed);
                            float nh = g_vit_vt->get_health(vit_p) - (raw_dmg - absorbed);
                            g_vit_vt->set_health(vit_p, (nh < 0.0f) ? 0.0f : nh);

                            float r_ion = g_res_vt->get_ionic(res_p);
                            float drain = 1.5f * atten * SIM_DT * (1.0f - 0.4f * r_ion);
                            float ne = g_vit_vt->get_energy(vit_p) - drain;
                            g_vit_vt->set_energy(vit_p, (ne < 0.0f) ? 0.0f : ne);
                        }

                        g_api->iter_mark_changed(unit_iter, 1, u);
                        g_api->iter_mark_changed(unit_iter, 2, u);
                    }
                }
            }

            void* vel_mut = g_kin_vt->ptr_mut_velocity(kin_p);
            float drag = g_kin_vt->get_drag(kin_p);
            float max_speed = g_kin_vt->get_max_speed(kin_p);
            float damp = 1.0f / (1.0f + drag * SIM_DT);

            float vx = g_v3_vt->get_x(vel_mut) * damp;
            float vy = g_v3_vt->get_y(vel_mut) * damp;
            float vz = g_v3_vt->get_z(vel_mut) * damp;

            float speed_sq = vx * vx + vy * vy + vz * vz;
            float max_sq = max_speed * max_speed;
            if (speed_sq > max_sq && speed_sq > 1.0e-12f) {
                float sc = max_speed / sqrtf(speed_sq);
                vx *= sc;
                vy *= sc;
                vz *= sc;
            }

            g_v3_vt->set_x(vel_mut, vx);
            g_v3_vt->set_y(vel_mut, vy);
            g_v3_vt->set_z(vel_mut, vz);

            void* pos_mut = g_tf_vt->ptr_mut_position(tf_p);
            g_v3_vt->set_x(pos_mut, g_v3_vt->get_x(pos_mut) + vx * SIM_DT);
            g_v3_vt->set_y(pos_mut, g_v3_vt->get_y(pos_mut) + vy * SIM_DT);
            g_v3_vt->set_z(pos_mut, g_v3_vt->get_z(pos_mut) + vz * SIM_DT);

            float final_speed_sq = vx * vx + vy * vy + vz * vz;
            if (final_speed_sq > 1.0e-6f) {
                float inv_s = 1.0f / sqrtf(final_speed_sq);
                void* fwd_mut = g_tf_vt->ptr_mut_forward(tf_p);
                g_v3_vt->set_x(fwd_mut, vx * inv_s);
                g_v3_vt->set_y(fwd_mut, vy * inv_s);
                g_v3_vt->set_z(fwd_mut, vz * inv_s);
            }

            g_api->iter_mark_changed(unit_iter, 0, u);
        }
    }

    return FFI_RESULT_OK;
}

/* ============================================================================
 * Lifecycle Entry Points
 * ============================================================================ */

BEVY_ABI_MOD_EXPORT uint32_t bevy_abi_mod_abi_version(void) {
    return BEVY_ABI_VERSION_PACKED;
}

BEVY_ABI_MOD_EXPORT FfiResult bevy_abi_mod_init(
    GetProcAddressFn get_proc_address,
    const HostApi* host_api,
    void* host_context
) {
    if (!host_api || !host_context) return FFI_RESULT_NULL_POINTER;
    g_api = host_api;

    const void* reg_mq_raw = get_proc_address("register_multi_query_system");
    const void* reset_raw  = get_proc_address("iter_reset");

    g_v3_vt   = (const Vec3fVTable*)get_proc_address("Vec3f_vtable");
    g_tf_vt   = (const Transform3DVTable*)get_proc_address("Transform3D_vtable");
    g_kin_vt  = (const KinematicsVTable*)get_proc_address("Kinematics_vtable");
    g_vit_vt  = (const VitalityVTable*)get_proc_address("Vitality_vtable");
    g_res_vt  = (const ResistanceVTable*)get_proc_address("Resistance_vtable");
    g_aura_vt = (const AuraEmitterVTable*)get_proc_address("AuraEmitter_vtable");
    g_fac_vt  = (const FactionVTable*)get_proc_address("Faction_vtable");

    if (!reg_mq_raw || !reset_raw || !g_v3_vt || !g_tf_vt || !g_kin_vt ||
        !g_vit_vt || !g_res_vt || !g_aura_vt || !g_fac_vt) {
        return FFI_RESULT_INVALID_COMPONENT;
    }

    RegisterMultiQuerySystemFn register_mq;
    *(const void**)(&register_mq) = reg_mq_raw;
    *(const void**)(&g_iter_reset) = reset_raw;

    ComponentAccessDesc unit_clause[] = {
        { "Transform3D",      COMPONENT_ACCESS_WRITE },
        { "Kinematics",       COMPONENT_ACCESS_WRITE },
        { "Vitality",         COMPONENT_ACCESS_WRITE },
        { "Resistance",       COMPONENT_ACCESS_READ },
        { "Faction",          COMPONENT_ACCESS_READ },
        { "CombatUnitTag",    COMPONENT_ACCESS_FILTER_ONLY },
        { "EmitterBeaconTag", COMPONENT_ACCESS_WITHOUT },
        { "FrozenFlag",       COMPONENT_ACCESS_WITHOUT }
    };

    ComponentAccessDesc emitter_clause[] = {
        { "Transform3D",      COMPONENT_ACCESS_READ },
        { "AuraEmitter",      COMPONENT_ACCESS_READ },
        { "Faction",          COMPONENT_ACCESS_READ },
        { "EmitterBeaconTag", COMPONENT_ACCESS_FILTER_ONLY },
        { "CombatUnitTag",    COMPONENT_ACCESS_WITHOUT },
        { "FrozenFlag",       COMPONENT_ACCESS_WITHOUT }
    };

    QuerySpec specs[2] = {
        { unit_clause,    8 },
        { emitter_clause, 6 }
    };

    const char* sys_name = "c99_cabi_chunk_optimized";
    MultiQuerySystemCallback cb = c99_sys_chunk_optimized;
    if (g_variant == 1u) {
        sys_name = "c99_cabi_entity_nested_vtable";
        cb = c99_sys_entity_nested_vtable;
    } else if (g_variant >= 2u) {
        sys_name = "c99_cabi_naive_chatty_ffi";
        cb = c99_sys_naive_chatty_ffi;
    }

    MultiQuerySystemDescriptor desc;
    desc.struct_size = (uint32_t)sizeof(MultiQuerySystemDescriptor);
    desc.name = sys_name;
    desc.query_specs = specs;
    desc.query_spec_count = 2;
    desc.callback = cb;
    desc.user_data = NULL;

    return register_mq(host_context, &desc);
}

BEVY_ABI_MOD_EXPORT FfiResult bevy_abi_mod_shutdown(void* host_context) {
    (void)host_context;
    return FFI_RESULT_OK;
}
