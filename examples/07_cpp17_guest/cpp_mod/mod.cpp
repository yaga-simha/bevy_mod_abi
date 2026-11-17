// ============================================================================
// mod.cpp — Strict ISO C++17 Guest Mod for `bevy_abi_mod` Example 07 (`ABI 0.2.0`)
//
// Demonstrates:
// 1. Exporting `bevy_abi_mod_manifest`, `bevy_abi_mod_init`, and `bevy_abi_mod_shutdown`
//    with `BAM_EXPORT` (`extern "C"` linkage).
// 2. Per-`World` RAII `CppModInstance` allocated with `std::make_unique` and transferred
//    across `*instance_out` (`BamInstance*`) with zero process globals.
// 3. Catching all C++ exceptions at the `extern "C"` (`nounwind`) boundary via
//    `try { ... } catch (...) { return BAM_RESULT_PANIC_CAUGHT; }` (`spec.md` §3.4).
// 4. Zero-serialization contiguous SoA chunk iteration over `example::Position` and
//    `example::Velocity` alongside `example::SimConfig` singleton resource reads.
// ============================================================================

#include "../../../include/bevy_abi_mod.h"

#include <cmath>
#include <cstddef>
#include <cstdint>
#include <memory>
#include <stdexcept>

namespace {

// Mirror of the host's `#[repr(C)]` `AbiPod` `Position` (`"example::Position"`).
struct Position {
    float x;
    float y;
    float z;
};
static_assert(sizeof(Position) == 12u, "Position must be 12 bytes");
static_assert(alignof(Position) == 4u, "Position must be 4-byte aligned");

// Mirror of the host's `#[repr(C)]` `AbiPod` `Velocity` (`"example::Velocity"`).
struct Velocity {
    float x;
    float y;
    float z;
};
static_assert(sizeof(Velocity) == 12u, "Velocity must be 12 bytes");
static_assert(alignof(Velocity) == 4u, "Velocity must be 4-byte aligned");

// Mirror of the host's `#[repr(C)]` `AbiPod` `SimConfig` resource (`"example::SimConfig"`).
struct SimConfig {
    float    dt;
    float    damping;
    BamBool  trigger_exception;
    uint8_t  _pad[3];
};
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

        return api_.register_system(load_ctx, &sys_desc);
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

    BamHostApi   api_{};
    BamComponent pos_comp_{BAM_COMPONENT_INVALID};
    BamComponent vel_comp_{BAM_COMPONENT_INVALID};
    BamComponent cfg_res_{BAM_COMPONENT_INVALID};
};

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

        auto instance = std::make_unique<CppModInstance>(local_api);
        const BamResult init_res = instance->initialize(load_ctx);
        if (init_res != BAM_RESULT_OK) {
            // RAII `std::unique_ptr` frees `CppModInstance` on the C++ guest heap automatically.
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
        auto* typed = reinterpret_cast<CppModInstance*>(instance);
        delete typed;
    } catch (...) {
        // Destructors are `noexcept` in C++17, but guard unconditionally across `extern "C"`.
    }
}
