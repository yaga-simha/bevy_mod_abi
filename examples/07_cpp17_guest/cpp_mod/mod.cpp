// ============================================================================
// mod.cpp — Strict ISO C++17 Guest Mod for `bevy_abi_mod` Example 07 (`ABI 0.3.0`)
//
// Demonstrates:
// 1. Idiomatic `bam::GuestApp`, `bam::SystemContext`, `bam::Chunk`, and `bam::Commands`
//    wrappers (`bam_cpp17_sdk.hpp`) paired with macro-generated `example_components.h`
//    (`ModComponent::C_DECL`) typed views (`ActorStatsView`, `HostVecView`, `BehaviorModeView`).
// 2. Per-`World` RAII `CppModInstance` managed by `bam::UniqueGuest` with zero
//    process globals.
// 3. Catching all C++ exceptions at the `extern "C"` (`nounwind`) boundary via
//    `try { ... } catch (...) { return BAM_RESULT_PANIC_CAUGHT; }` (`spec.md` §3.4).
// 4. Zero-serialization contiguous SoA chunk iteration over `example::Position` and
//    `example::Velocity` alongside `example::SimConfig` singleton resource reads.
// 5. Optional typed VTable view mutation (`ActorStatsView`, `BehaviorModeView`),
//    `BAM_ACCESS_HAS` (`Has<ActorStats>`), `BAM_CHILD_OF_NAME` hierarchy reads,
//    `BAM_EVENT_KIND_ADD` lifecycle observer, and `cmd_spawn_batch` SoA bulk spawn.
// ============================================================================

#include "bam_cpp17_sdk.hpp"
#include "example_components.h"

#include <cmath>
#include <cstddef>
#include <cstdint>
#include <cstring>
#include <memory>
#include <stdexcept>
#include <vector>

namespace {

// Static layout parity checks against Rust's `#[repr(C)]` `AbiPod` structs.
static_assert(sizeof(Position) == 12u, "Position must be 12 bytes");
static_assert(alignof(Position) == 4u, "Position must be 4-byte aligned");
static_assert(sizeof(Velocity) == 12u, "Velocity must be 12 bytes");
static_assert(alignof(Velocity) == 4u, "Velocity must be 4-byte aligned");
static_assert(sizeof(SimConfig) == 12u, "SimConfig must be 12 bytes");
static_assert(alignof(SimConfig) == 4u, "SimConfig must be 4-byte aligned");
static_assert(sizeof(CppTelemetry) == 24u, "CppTelemetry must be 24 bytes");
static_assert(alignof(CppTelemetry) == 4u, "CppTelemetry must be 4-byte aligned");

constexpr BamManifest kManifest = {
    static_cast<uint32_t>(sizeof(BamManifest)),
    BAM_ABI_VERSION,
    "example_07_cpp17_mod",
    "0.2.0",
    0u,
    0u,
    nullptr,
    nullptr
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

        bam::GuestApp app(api_, load_ctx);
        if (!app.valid()) {
            return BAM_RESULT_NULL_POINTER;
        }

        BamResult res = app.resolve_component("example::Position", pos_comp_);
        if (res != BAM_RESULT_OK) {
            return res;
        }
        res = app.resolve_component("example::Velocity", vel_comp_);
        if (res != BAM_RESULT_OK) {
            return res;
        }
        res = app.resolve_component("example::SimConfig", cfg_res_);
        if (res != BAM_RESULT_OK) {
            return res;
        }

        const BamTermDesc kinematics_terms[3] = {
            bam_term_desc(pos_comp_, BAM_ACCESS_WRITE, 0u),
            bam_term_desc(vel_comp_, BAM_ACCESS_WRITE, 0u),
            bam_term_desc(cfg_res_, BAM_ACCESS_RESOURCE_READ, 0u),
        };
        res = app.register_system(
            "cpp17_damped_kinematics",
            kinematics_terms,
            3u,
            &CppModInstance::kinematics_trampoline,
            this,
            BAM_SCHEDULE_UPDATE);
        if (res != BAM_RESULT_OK) {
            return res;
        }

        // Optionally bind `ActorStats` + `BehaviorMode` VTables using `bam::VTableHandle`.
        if (app.try_resolve_component("example::ActorStats", stats_comp_) &&
            app.try_resolve_component("example::BehaviorMode", mode_comp_)) {
            stats_vt_ = bam::VTableHandle<ActorStatsVTable>::resolve(
                api_, app.host_ctx(), "bam.v0.example::ActorStats.vtable");
            mode_vt_ = bam::VTableHandle<BehaviorModeVTable>::resolve(
                api_, app.host_ctx(), "bam.v0.example::BehaviorMode.vtable");
            if (!stats_vt_ || !mode_vt_) {
                return BAM_RESULT_NOT_FOUND;
            }

            const BamTermDesc actor_terms[2] = {
                bam_term_desc(stats_comp_, BAM_ACCESS_WRITE, 0u),
                bam_term_desc(mode_comp_, BAM_ACCESS_WRITE, 0u),
            };
            res = app.register_system(
                "cpp17_actor_views",
                actor_terms,
                2u,
                &CppModInstance::actor_views_trampoline,
                this,
                BAM_SCHEDULE_UPDATE);
            if (res != BAM_RESULT_OK) {
                return res;
            }

            // Bind `BAM_EVENT_KIND_ADD` lifecycle observer,
            // `BAM_ACCESS_HAS`, `BamChildOf` optional hierarchy read, and `cmd_spawn_batch`.
            if (app.try_resolve_component("example::CppTelemetry", telemetry_res_) &&
                app.try_resolve_component(BAM_CHILD_OF_NAME, child_of_comp_)) {
                res = app.register_lifecycle_observer(
                    "cpp17_on_actor_added",
                    BAM_EVENT_KIND_ADD,
                    stats_comp_,
                    &CppModInstance::on_actor_added_trampoline,
                    this);
                if (res != BAM_RESULT_OK) {
                    return res;
                }

                const BamTermDesc has_terms[4] = {
                    bam_term_desc(pos_comp_, BAM_ACCESS_READ, 0u),
                    bam_term_desc(stats_comp_, BAM_ACCESS_HAS, 0u),
                    bam_term_desc(child_of_comp_, BAM_ACCESS_OPTIONAL_READ, 0u),
                    bam_term_desc(telemetry_res_, BAM_ACCESS_RESOURCE_WRITE, 0u),
                };
                res = app.register_system(
                    "cpp17_has_hierarchy_and_batch",
                    has_terms,
                    4u,
                    &CppModInstance::has_hierarchy_trampoline,
                    this,
                    BAM_SCHEDULE_UPDATE);
                if (res != BAM_RESULT_OK) {
                    return res;
                }
            }
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

    static BamResult has_hierarchy_trampoline(BamSystemCtx* sys_ctx, void* user_data) noexcept {
        if (sys_ctx == nullptr || user_data == nullptr) {
            return BAM_RESULT_NULL_POINTER;
        }
        try {
            auto* self = static_cast<CppModInstance*>(user_data);
            return self->run_has_hierarchy_and_batch(sys_ctx);
        } catch (...) {
            return BAM_RESULT_PANIC_CAUGHT;
        }
    }

    static BamResult on_actor_added_trampoline(
        BamEntity    target,
        const void*  event_data,
        BamCommands* commands,
        void*        user_data
    ) noexcept {
        (void)target;
        (void)event_data;
        (void)commands;
        if (user_data == nullptr) {
            return BAM_RESULT_NULL_POINTER;
        }
        auto* self = static_cast<CppModInstance*>(user_data);
        self->observed_adds_ += 1u;
        return BAM_RESULT_OK;
    }

    BamResult run_kinematics(BamSystemCtx* raw_ctx) {
        bam::SystemContext ctx(api_, raw_ctx);
        BamIter* iter = nullptr;
        BamResult res = ctx.query(0u, iter);
        if (res != BAM_RESULT_OK) {
            return res;
        }

        const auto* cfg = ctx.res_ptr<SimConfig>(iter, 2u);
        if (cfg == nullptr) {
            return BAM_RESULT_NULL_POINTER;
        }
        if (cfg->trigger_exception != BAM_FALSE) {
            throw std::runtime_error("simulated C++17 domain exception caught at ABI boundary");
        }
        if (!std::isfinite(cfg->dt) || cfg->dt < 0.0f) {
            throw std::invalid_argument("SimConfig::dt must be finite and non-negative");
        }

        BamColumnInfo pos_info{};
        BamColumnInfo vel_info{};
        res = ctx.column_info(iter, 0u, pos_info);
        if (res != BAM_RESULT_OK) {
            return res;
        }
        res = ctx.column_info(iter, 1u, vel_info);
        if (res != BAM_RESULT_OK) {
            return res;
        }
        if (pos_info.size != sizeof(Position) || pos_info.align != alignof(Position) ||
            vel_info.size != sizeof(Velocity) || vel_info.align != alignof(Velocity)) {
            return BAM_RESULT_TYPE_MISMATCH;
        }

        size_t chunk_len = 0u;
        while (ctx.next_chunk(iter, chunk_len)) {
            bam::Chunk chunk(api_, iter, chunk_len);
            auto* positions  = chunk.column_mutable<Position>(0u);
            auto* velocities = chunk.column_mutable<Velocity>(1u);
            if (positions == nullptr || velocities == nullptr) {
                return BAM_RESULT_NULL_POINTER;
            }

            for (size_t i = 0u; i < chunk.len(); ++i) {
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

    BamResult run_actor_views(BamSystemCtx* raw_ctx) {
        if (!stats_vt_ || !mode_vt_ || api_.iter_column_mutable == nullptr) {
            return BAM_RESULT_NULL_POINTER;
        }
        bam::SystemContext ctx(api_, raw_ctx);
        BamIter* iter = nullptr;
        BamResult res = ctx.query(0u, iter);
        if (res != BAM_RESULT_OK) {
            return res;
        }

        BamColumnInfo stats_info{};
        BamColumnInfo mode_info{};
        res = ctx.column_info(iter, 0u, stats_info);
        if (res != BAM_RESULT_OK) {
            return res;
        }
        res = ctx.column_info(iter, 1u, mode_info);
        if (res != BAM_RESULT_OK) {
            return res;
        }
        if (stats_info.size != stats_vt_->header.size || mode_info.size != mode_vt_->header.size) {
            return BAM_RESULT_TYPE_MISMATCH;
        }

        size_t chunk_len = 0u;
        while (ctx.next_chunk(iter, chunk_len)) {
            bam::Chunk chunk(api_, iter, chunk_len);
            for (size_t i = 0u; i < chunk.len(); ++i) {
                uint8_t* stats_ptr = chunk.rows_mutable(0u, i, stats_info.size);
                uint8_t* mode_ptr  = chunk.rows_mutable(1u, i, mode_info.size);
                if (stats_ptr == nullptr || mode_ptr == nullptr) {
                    return BAM_RESULT_NULL_POINTER;
                }

                ActorStatsView stats(stats_ptr, stats_vt_.raw());
                BehaviorModeView mode(mode_ptr, mode_vt_.raw());

                if (!stats || !mode) {
                    return BAM_RESULT_NULL_POINTER;
                }

                // Exercise `HostVecView` methods: reserve, push, pop, mutable range-for (`as_mut_slice`)
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

                for (uint32_t& val : scores) {
                    val += 5u;
                }

                res = stats.set_active(true);
                if (res != BAM_RESULT_OK) return res;
                res = stats.set_tag("cpp17_view_updated");
                if (res != BAM_RESULT_OK) return res;
                res = stats.set_bonus_some(777u);
                if (res != BAM_RESULT_OK) return res;

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

    BamResult run_has_hierarchy_and_batch(BamSystemCtx* raw_ctx) {
        bam::SystemContext ctx(api_, raw_ctx);
        BamIter* iter = nullptr;
        BamResult res = ctx.query(0u, iter);
        if (res != BAM_RESULT_OK) {
            return res;
        }

        auto* telemetry = ctx.res_mut_ptr<CppTelemetry>(iter, 3u);
        if (telemetry == nullptr) {
            return BAM_RESULT_NULL_POINTER;
        }

        uint32_t with_stats    = 0u;
        uint32_t without_stats = 0u;
        uint32_t children_seen = 0u;

        size_t chunk_len = 0u;
        while (ctx.next_chunk(iter, chunk_len)) {
            bam::Chunk chunk(api_, iter, chunk_len);
            const auto* has_stats = chunk.has_column(1u);
            const auto* child_ofs = chunk.column_readonly<BamChildOf>(2u);
            if (has_stats == nullptr) {
                return BAM_RESULT_NULL_POINTER;
            }
            for (size_t i = 0u; i < chunk.len(); ++i) {
                if (has_stats[i] != BAM_FALSE) {
                    with_stats += 1u;
                } else {
                    without_stats += 1u;
                }
                if (child_ofs != nullptr && child_ofs[i].parent != BAM_ENTITY_NULL) {
                    children_seen += 1u;
                }
            }
        }

        telemetry->observed_adds          = observed_adds_;
        telemetry->entities_with_stats    = with_stats;
        telemetry->entities_without_stats = without_stats;
        telemetry->child_entities_seen    = children_seen;

        if (telemetry->spawn_batch_count > 0u && telemetry->batch_spawned == BAM_FALSE) {
            BamResult cmd_res = BAM_RESULT_OK;
            bam::Commands cmds = ctx.commands(&cmd_res);
            if (cmd_res != BAM_RESULT_OK || !cmds.valid()) {
                return cmd_res != BAM_RESULT_OK ? cmd_res : BAM_RESULT_NULL_POINTER;
            }
            const size_t count = static_cast<size_t>(telemetry->spawn_batch_count);
            std::vector<Position> batch_positions(count);
            for (size_t i = 0u; i < count; ++i) {
                batch_positions[i] = Position{static_cast<float>(i) + 1.0f, 0.0f, 0.0f};
            }
            const BamComponent comps[1] = {pos_comp_};
            const uint8_t* cols[1] = {reinterpret_cast<const uint8_t*>(batch_positions.data())};
            res = cmds.spawn_batch(comps, cols, 1u, count, nullptr);
            if (res != BAM_RESULT_OK) {
                return res;
            }
            telemetry->batch_spawned = BAM_TRUE;
        }

        return BAM_RESULT_OK;
    }

    BamHostApi                            api_{};
    BamComponent                          pos_comp_{BAM_COMPONENT_INVALID};
    BamComponent                          vel_comp_{BAM_COMPONENT_INVALID};
    BamComponent                          cfg_res_{BAM_COMPONENT_INVALID};
    BamComponent                          stats_comp_{BAM_COMPONENT_INVALID};
    BamComponent                          mode_comp_{BAM_COMPONENT_INVALID};
    BamComponent                          child_of_comp_{BAM_COMPONENT_INVALID};
    BamComponent                          telemetry_res_{BAM_COMPONENT_INVALID};
    bam::VTableHandle<ActorStatsVTable>   stats_vt_{};
    bam::VTableHandle<BehaviorModeVTable> mode_vt_{};
    uint32_t                              observed_adds_{0u};
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
