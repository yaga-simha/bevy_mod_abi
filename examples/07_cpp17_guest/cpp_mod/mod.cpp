// ============================================================================
// mod.cpp — Strict ISO C++17 Guest Mod for `bevy_abi_mod` Example 07 (`ABI 0.2.0`)
//
// Demonstrates:
// 1. Including macro-generated `example_components.h` (`ModComponent::C_DECL`) with
//    C++17 `noexcept` RAII & typed view classes (`ActorStatsView`, `HostVecView`,
//    `BehaviorModeView`, `bam::VTableHandle`, `bam::UniqueGuest`).
// 2. Per-`World` RAII `CppModInstance` managed by `bam::UniqueGuest` with zero
//    process globals.
// 3. Catching all C++ exceptions at the `extern "C"` (`nounwind`) boundary via
//    `try { ... } catch (...) { return BAM_RESULT_PANIC_CAUGHT; }` (`spec.md` §3.4).
// 4. Zero-serialization contiguous SoA chunk iteration over `example::Position` and
//    `example::Velocity` alongside `example::SimConfig` singleton resource reads.
// 5. Optional typed VTable view mutation over `example::ActorStats` (`Vec<u32>`,
//    `String`, `FfiOption<u32>`, `bool`) and `example::BehaviorMode` (`#[repr(C, u8)]`).
// ============================================================================

#include "example_components.h"

#include <cmath>
#include <cstddef>
#include <cstdint>
#include <cstring>
#include <memory>
#include <stdexcept>

namespace {

// Static layout parity checks against Rust's `#[repr(C)]` `AbiPod` structs.
static_assert(sizeof(Position) == 12u, "Position must be 12 bytes");
static_assert(alignof(Position) == 4u, "Position must be 4-byte aligned");
static_assert(sizeof(Velocity) == 12u, "Velocity must be 12 bytes");
static_assert(alignof(Velocity) == 4u, "Velocity must be 4-byte aligned");
static_assert(sizeof(SimConfig) == 12u, "SimConfig must be 12 bytes");
static_assert(alignof(SimConfig) == 4u, "SimConfig must be 4-byte aligned");

constexpr BamManifest kManifest = {
    static_cast<uint32_t>(sizeof(BamManifest)),
    BAM_ABI_VERSION,
    "example_07_cpp17_mod",
    "0.2.0",
    0u,
    0u
};

// Per-World RAII instance state owned exclusively by the C++ guest allocator (`new`/`delete`).
class CppModInstance {
public:
    explicit CppModInstance(const BamHostApi& host_api) noexcept : api_(host_api) {}
    ~CppModInstance() = default;

    CppModInstance(const CppModInstance&) = delete;
    CppModInstance& operator=(const CppModInstance&) = delete;

    static void destroy(CppModInstance* ptr) noexcept {
        delete ptr;
    }

    BamResult initialize(BamLoadCtx* load_ctx) {
        if (api_.load_host_ctx == nullptr ||
            api_.resolve_component == nullptr ||
            api_.register_system == nullptr ||
            api_.sys_query == nullptr ||
            api_.iter_next_chunk == nullptr ||
            api_.iter_column_mutable == nullptr ||
            api_.iter_column_readonly == nullptr ||
            api_.iter_column_info == nullptr ||
            api_.res_ptr == nullptr) {
            return BAM_RESULT_NULL_POINTER;
        }

        const BamHostCtx* host_ctx = api_.load_host_ctx(load_ctx);
        if (host_ctx == nullptr) {
            return BAM_RESULT_NULL_POINTER;
        }

        BamResult res = api_.resolve_component(host_ctx, "example::Position", nullptr, &pos_comp_);
        if (res != BAM_RESULT_OK) {
            return res;
        }
        res = api_.resolve_component(host_ctx, "example::Velocity", nullptr, &vel_comp_);
        if (res != BAM_RESULT_OK) {
            return res;
        }
        res = api_.resolve_component(host_ctx, "example::SimConfig", nullptr, &cfg_res_);
        if (res != BAM_RESULT_OK) {
            return res;
        }

        const BamTermDesc kinematics_terms[3] = {
            bam_term_desc(pos_comp_, BAM_ACCESS_WRITE, 0u),
            bam_term_desc(vel_comp_, BAM_ACCESS_WRITE, 0u),
            bam_term_desc(cfg_res_, BAM_ACCESS_RESOURCE_READ, 0u),
        };

        BamQueryDesc query{};
        query.struct_size = static_cast<uint32_t>(sizeof(BamQueryDesc));
        query.flags       = 0u;
        query.terms       = kinematics_terms;
        query.term_count  = 3u;

        BamSystemDesc sys_desc{};
        sys_desc.struct_size = static_cast<uint32_t>(sizeof(BamSystemDesc));
        sys_desc.flags       = 0u;
        sys_desc.name        = "cpp17_damped_kinematics";
        sys_desc.queries     = &query;
        sys_desc.query_count = 1u;
        sys_desc.callback    = &CppModInstance::kinematics_trampoline;
        sys_desc.user_data   = this;
        sys_desc.schedule    = BAM_SCHEDULE_UPDATE;

        res = api_.register_system(load_ctx, &sys_desc);
        if (res != BAM_RESULT_OK) {
            return res;
        }

        // Optionally bind `ActorStats` + `BehaviorMode` VTables using `bam::VTableHandle`.
        if (api_.resolve_component(host_ctx, "example::ActorStats", nullptr, &stats_comp_) == BAM_RESULT_OK &&
            api_.resolve_component(host_ctx, "example::BehaviorMode", nullptr, &mode_comp_) == BAM_RESULT_OK) {
            stats_vt_ = bam::VTableHandle<ActorStatsVTable>::resolve(
                api_, host_ctx, "bam.v0.example::ActorStats.vtable");
            mode_vt_ = bam::VTableHandle<BehaviorModeVTable>::resolve(
                api_, host_ctx, "bam.v0.example::BehaviorMode.vtable");
            if (!stats_vt_ || !mode_vt_) {
                return BAM_RESULT_NOT_FOUND;
            }

            const BamTermDesc actor_terms[2] = {
                bam_term_desc(stats_comp_, BAM_ACCESS_WRITE, 0u),
                bam_term_desc(mode_comp_, BAM_ACCESS_WRITE, 0u),
            };
            BamQueryDesc actor_query{};
            actor_query.struct_size = static_cast<uint32_t>(sizeof(BamQueryDesc));
            actor_query.terms       = actor_terms;
            actor_query.term_count  = 2u;

            BamSystemDesc actor_sys{};
            actor_sys.struct_size = static_cast<uint32_t>(sizeof(BamSystemDesc));
            actor_sys.name        = "cpp17_actor_views";
            actor_sys.queries     = &actor_query;
            actor_sys.query_count = 1u;
            actor_sys.callback    = &CppModInstance::actor_views_trampoline;
            actor_sys.user_data   = this;
            actor_sys.schedule    = BAM_SCHEDULE_UPDATE;

            return api_.register_system(load_ctx, &actor_sys);
        }

        return BAM_RESULT_OK;
    }

private:
    static BamResult kinematics_trampoline(BamSystemCtx* sys_ctx, void* user_data) noexcept {
        if (sys_ctx == nullptr || user_data == nullptr) {
            return BAM_RESULT_NULL_POINTER;
        }
        try {
            auto* self = static_cast<CppModInstance*>(user_data);
            return self->run_kinematics(sys_ctx);
        } catch (...) {
            // Never allow C++ exceptions to unwind across an `extern "C"` (`nounwind`) boundary!
            return BAM_RESULT_PANIC_CAUGHT;
        }
    }

    static BamResult actor_views_trampoline(BamSystemCtx* sys_ctx, void* user_data) noexcept {
        if (sys_ctx == nullptr || user_data == nullptr) {
            return BAM_RESULT_NULL_POINTER;
        }
        try {
            auto* self = static_cast<CppModInstance*>(user_data);
            return self->run_actor_views(sys_ctx);
        } catch (...) {
            return BAM_RESULT_PANIC_CAUGHT;
        }
    }

    BamResult run_kinematics(BamSystemCtx* sys_ctx) {
        BamIter* iter = nullptr;
        BamResult res = api_.sys_query(sys_ctx, 0u, &iter);
        if (res != BAM_RESULT_OK) {
            return res;
        }

        const uint8_t* raw_cfg = api_.res_ptr(iter, 2u);
        if (raw_cfg == nullptr) {
            return BAM_RESULT_NULL_POINTER;
        }
        const auto* cfg = reinterpret_cast<const SimConfig*>(raw_cfg);
        if (cfg->trigger_exception != BAM_FALSE) {
            throw std::runtime_error("simulated C++17 domain exception caught at ABI boundary");
        }
        if (!std::isfinite(cfg->dt) || cfg->dt < 0.0f) {
            throw std::invalid_argument("SimConfig::dt must be finite and non-negative");
        }

        BamColumnInfo pos_info{};
        BamColumnInfo vel_info{};
        res = api_.iter_column_info(iter, 0u, &pos_info);
        if (res != BAM_RESULT_OK) {
            return res;
        }
        res = api_.iter_column_info(iter, 1u, &vel_info);
        if (res != BAM_RESULT_OK) {
            return res;
        }
        if (pos_info.size != sizeof(Position) || pos_info.align != alignof(Position) ||
            vel_info.size != sizeof(Velocity) || vel_info.align != alignof(Velocity)) {
            return BAM_RESULT_TYPE_MISMATCH;
        }

        size_t chunk_len = 0u;
        while (api_.iter_next_chunk(iter, &chunk_len) != BAM_FALSE) {
            uint8_t* pos_bytes = api_.iter_column_mutable(iter, 0u);
            uint8_t* vel_bytes = api_.iter_column_mutable(iter, 1u);
            if (pos_bytes == nullptr || vel_bytes == nullptr) {
                return BAM_RESULT_NULL_POINTER;
            }

            auto* positions  = reinterpret_cast<Position*>(pos_bytes);
            auto* velocities = reinterpret_cast<Velocity*>(vel_bytes);

            for (size_t i = 0u; i < chunk_len; ++i) {
                velocities[i].x *= cfg->damping;
                velocities[i].y *= cfg->damping;
                velocities[i].z *= cfg->damping;

                positions[i].x += velocities[i].x * cfg->dt;
                positions[i].y += velocities[i].y * cfg->dt;
                positions[i].z += velocities[i].z * cfg->dt;
            }
        }

        return BAM_RESULT_OK;
    }

    BamResult run_actor_views(BamSystemCtx* sys_ctx) {
        if (!stats_vt_ || !mode_vt_ || api_.iter_column_mutable == nullptr) {
            return BAM_RESULT_NULL_POINTER;
        }
        BamIter* iter = nullptr;
        BamResult res = api_.sys_query(sys_ctx, 0u, &iter);
        if (res != BAM_RESULT_OK) {
            return res;
        }

        BamColumnInfo stats_info{};
        BamColumnInfo mode_info{};
        res = api_.iter_column_info(iter, 0u, &stats_info);
        if (res != BAM_RESULT_OK) {
            return res;
        }
        res = api_.iter_column_info(iter, 1u, &mode_info);
        if (res != BAM_RESULT_OK) {
            return res;
        }
        if (stats_info.size != stats_vt_->header.size || mode_info.size != mode_vt_->header.size) {
            return BAM_RESULT_TYPE_MISMATCH;
        }

        size_t chunk_len = 0u;
        while (api_.iter_next_chunk(iter, &chunk_len) != BAM_FALSE) {
            uint8_t* stats_base = api_.iter_column_mutable(iter, 0u);
            uint8_t* mode_base  = api_.iter_column_mutable(iter, 1u);
            if (stats_base == nullptr || mode_base == nullptr) {
                return BAM_RESULT_NULL_POINTER;
            }

            for (size_t i = 0u; i < chunk_len; ++i) {
                ActorStatsView stats(stats_base + i * stats_info.size, stats_vt_.raw());
                BehaviorModeView mode(mode_base + i * mode_info.size, mode_vt_.raw());

                if (!stats || !mode) {
                    return BAM_RESULT_NULL_POINTER;
                }

                // Exercise `HostVecView` methods: reserve, push, pop, remove, swap_remove, range-for
                auto scores = stats.scores();
                res = scores.reserve(8u);
                if (res != BAM_RESULT_OK) return res;
                res = scores.push(50u);
                if (res != BAM_RESULT_OK) return res;
                res = scores.push(60u);
                if (res != BAM_RESULT_OK) return res;

                uint32_t popped = 0u;
                if (!scores.pop(&popped) || popped != 60u) {
                    return BAM_RESULT_INTERNAL_ERROR;
                }

                // Increment every score in-place via mutable range-for (`as_mut_slice`)
                for (uint32_t& val : scores) {
                    val += 5u;
                }

                // Update `bool`, `String`, and `FfiOption<u32>` via typed C++17 `noexcept` view
                res = stats.set_active(true);
                if (res != BAM_RESULT_OK) return res;
                res = stats.set_tag("cpp17_view_updated");
                if (res != BAM_RESULT_OK) return res;
                res = stats.set_bonus_some(777u);
                if (res != BAM_RESULT_OK) return res;

                // Exercise tagged `#[repr(C, u8)]` enum view (`tag`, `set_patrol`, `try_get_patrol_*`)
                if (mode.tag() == BehaviorMode_Tag_Idle) {
                    res = mode.set_patrol(14.5f, true);
                    if (res != BAM_RESULT_OK) return res;
                    float speed = 0.0f;
                    BamBool aggressive = BAM_FALSE;
                    if (mode.try_get_patrol_speed(&speed) != BAM_RESULT_OK ||
                        mode.try_get_patrol_aggressive(&aggressive) != BAM_RESULT_OK ||
                        speed != 14.5f || aggressive == BAM_FALSE) {
                        return BAM_RESULT_INTERNAL_ERROR;
                    }
                } else {
                    res = mode.set_attack(99u);
                    if (res != BAM_RESULT_OK) return res;
                }
            }
        }

        return BAM_RESULT_OK;
    }

    BamHostApi                            api_{};
    BamComponent                          pos_comp_{BAM_COMPONENT_INVALID};
    BamComponent                          vel_comp_{BAM_COMPONENT_INVALID};
    BamComponent                          cfg_res_{BAM_COMPONENT_INVALID};
    BamComponent                          stats_comp_{BAM_COMPONENT_INVALID};
    BamComponent                          mode_comp_{BAM_COMPONENT_INVALID};
    bam::VTableHandle<ActorStatsVTable>   stats_vt_{};
    bam::VTableHandle<BehaviorModeVTable> mode_vt_{};
};

using UniqueCppInstance = bam::UniqueGuest<CppModInstance, &CppModInstance::destroy>;

} // namespace

BAM_EXPORT const BamManifest* bevy_abi_mod_manifest(void) {
    return &kManifest;
}

BAM_EXPORT BamResult bevy_abi_mod_init(
    const BamHostApi* host_api,
    BamLoadCtx*       load_ctx,
    BamInstance**     instance_out
) {
    if (load_ctx == nullptr || instance_out == nullptr) {
        return BAM_RESULT_NULL_POINTER;
    }
    *instance_out = nullptr;

    try {
        BamHostApi local_api{};
        const BamResult copy_res = bam_host_api_copy_bounded(&local_api, sizeof(local_api), host_api);
        if (copy_res != BAM_RESULT_OK) {
            return copy_res;
        }

        UniqueCppInstance instance(new CppModInstance(local_api));
        const BamResult init_res = instance->initialize(load_ctx);
        if (init_res != BAM_RESULT_OK) {
            return init_res;
        }

        *instance_out = reinterpret_cast<BamInstance*>(instance.release());
        return BAM_RESULT_OK;
    } catch (...) {
        return BAM_RESULT_PANIC_CAUGHT;
    }
}

BAM_EXPORT void bevy_abi_mod_shutdown(BamInstance* instance) {
    if (instance == nullptr) {
        return;
    }
    try {
        UniqueCppInstance owned(reinterpret_cast<CppModInstance*>(instance));
        owned.reset();
    } catch (...) {
        // Destructors are `noexcept` in C++17, but guard unconditionally across `extern "C"`.
    }
}
