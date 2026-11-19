//! Multi-language SDK exporter (`bevy_abi_mod.h`, `bevy_abi_mod.hpp`, `bevy_abi_mod.zig`).

use std::fs;
use std::path::{Path, PathBuf};

/// Generates the canonical strict ISO C99 header directly from `bevy_abi_mod_sys`.
///
/// # Panics
/// Panics only if internal `bevy_abi_mod_sys` wire layout invariants are violated.
#[must_use]
pub fn c99_header() -> String {
    crate::render_c99_header().expect("valid bevy_abi_mod_sys wire layouts")
}

/// Ergonomic ISO C++17 RAII wrapper header over `bevy_abi_mod.h` (`ABI 0.3.0`).
pub const CPP17_HEADER: &str = r#"#ifndef BEVY_ABI_MOD_HPP
#define BEVY_ABI_MOD_HPP

#include "bevy_abi_mod.h"

#include <cstddef>
#include <cstdint>
#include <exception>

#ifndef BAM_RESTRICT
#  if defined(_MSC_VER)
#    define BAM_RESTRICT __restrict
#  else
#    define BAM_RESTRICT
#  endif
#endif

namespace bam {

namespace offsets {

inline constexpr std::size_t SLOT_COUNT = BAM_HOST_API_SLOT_COUNT;
inline constexpr std::size_t STRUCT_SIZE = offsetof(BamHostApi, struct_size);
inline constexpr std::size_t ABI_VERSION = offsetof(BamHostApi, abi_version);

inline constexpr std::size_t GET_PROC_ADDRESS = BAM_HOST_API_OFFSET_GET_PROC_ADDRESS;
inline constexpr std::size_t LAST_ERROR = BAM_HOST_API_OFFSET_LAST_ERROR;
inline constexpr std::size_t LOAD_HOST_CTX = BAM_HOST_API_OFFSET_LOAD_HOST_CTX;
inline constexpr std::size_t REGISTER_COMPONENT = BAM_HOST_API_OFFSET_REGISTER_COMPONENT;
inline constexpr std::size_t RESOLVE_COMPONENT = BAM_HOST_API_OFFSET_RESOLVE_COMPONENT;
inline constexpr std::size_t REGISTER_SYSTEM = BAM_HOST_API_OFFSET_REGISTER_SYSTEM;
inline constexpr std::size_t SYS_HOST_CTX = BAM_HOST_API_OFFSET_SYS_HOST_CTX;
inline constexpr std::size_t SYS_QUERY = BAM_HOST_API_OFFSET_SYS_QUERY;
inline constexpr std::size_t SYS_COMMANDS = BAM_HOST_API_OFFSET_SYS_COMMANDS;
inline constexpr std::size_t ITER_NEXT_CHUNK = BAM_HOST_API_OFFSET_ITER_NEXT_CHUNK;
inline constexpr std::size_t ITER_RESET = BAM_HOST_API_OFFSET_ITER_RESET;
inline constexpr std::size_t ITER_ENTITIES = BAM_HOST_API_OFFSET_ITER_ENTITIES;
inline constexpr std::size_t ITER_COLUMN_READONLY = BAM_HOST_API_OFFSET_ITER_COLUMN_READONLY;
inline constexpr std::size_t ITER_COLUMN_MUTABLE = BAM_HOST_API_OFFSET_ITER_COLUMN_MUTABLE;
inline constexpr std::size_t ITER_COLUMN_MUTABLE_UNTRACKED = BAM_HOST_API_OFFSET_ITER_COLUMN_MUTABLE_UNTRACKED;
inline constexpr std::size_t ITER_MARK_CHANGED_RANGE = BAM_HOST_API_OFFSET_ITER_MARK_CHANGED_RANGE;
inline constexpr std::size_t ITER_COLUMN_ROWS_READONLY = BAM_HOST_API_OFFSET_ITER_COLUMN_ROWS_READONLY;
inline constexpr std::size_t ITER_COLUMN_ROWS_MUTABLE = BAM_HOST_API_OFFSET_ITER_COLUMN_ROWS_MUTABLE;
inline constexpr std::size_t ITER_COLUMN_INFO = BAM_HOST_API_OFFSET_ITER_COLUMN_INFO;
inline constexpr std::size_t RES_PTR = BAM_HOST_API_OFFSET_RES_PTR;
inline constexpr std::size_t RES_MUT_PTR = BAM_HOST_API_OFFSET_RES_MUT_PTR;
inline constexpr std::size_t CMD_SPAWN_EMPTY = BAM_HOST_API_OFFSET_CMD_SPAWN_EMPTY;
inline constexpr std::size_t CMD_SPAWN = BAM_HOST_API_OFFSET_CMD_SPAWN;
inline constexpr std::size_t CMD_DESPAWN = BAM_HOST_API_OFFSET_CMD_DESPAWN;
inline constexpr std::size_t CMD_INSERT = BAM_HOST_API_OFFSET_CMD_INSERT;
inline constexpr std::size_t CMD_INSERT_DEFAULT = BAM_HOST_API_OFFSET_CMD_INSERT_DEFAULT;
inline constexpr std::size_t CMD_REMOVE = BAM_HOST_API_OFFSET_CMD_REMOVE;
inline constexpr std::size_t PAR_FOR_EACH_CHUNK = BAM_HOST_API_OFFSET_PAR_FOR_EACH_CHUNK;
inline constexpr std::size_t SYS_ENTITY_GET = BAM_HOST_API_OFFSET_SYS_ENTITY_GET;
inline constexpr std::size_t SYS_ENTITY_GET_MUT = BAM_HOST_API_OFFSET_SYS_ENTITY_GET_MUT;
inline constexpr std::size_t MSG_READ_BATCH = BAM_HOST_API_OFFSET_MSG_READ_BATCH;
inline constexpr std::size_t MSG_SEND = BAM_HOST_API_OFFSET_MSG_SEND;
inline constexpr std::size_t MSG_WRITE_BATCH = BAM_HOST_API_OFFSET_MSG_WRITE_BATCH;
inline constexpr std::size_t RES_TICKS = BAM_HOST_API_OFFSET_RES_TICKS;
inline constexpr std::size_t SYS_RES_PTR = BAM_HOST_API_OFFSET_SYS_RES_PTR;
inline constexpr std::size_t SYS_RES_MUT_PTR = BAM_HOST_API_OFFSET_SYS_RES_MUT_PTR;
inline constexpr std::size_t REGISTER_OBSERVER = BAM_HOST_API_OFFSET_REGISTER_OBSERVER;
inline constexpr std::size_t CMD_TRIGGER = BAM_HOST_API_OFFSET_CMD_TRIGGER;
inline constexpr std::size_t CMD_SPAWN_PERSISTENT = BAM_HOST_API_OFFSET_CMD_SPAWN_PERSISTENT;
inline constexpr std::size_t CMD_SPAWN_BATCH = BAM_HOST_API_OFFSET_CMD_SPAWN_BATCH;
inline constexpr std::size_t CMD_DESPAWN_ALL = BAM_HOST_API_OFFSET_CMD_DESPAWN_ALL;
inline constexpr std::size_t CMD_INSERT_IF_NEW = BAM_HOST_API_OFFSET_CMD_INSERT_IF_NEW;
inline constexpr std::size_t CMD_TRIGGER_EX = BAM_HOST_API_OFFSET_CMD_TRIGGER_EX;
inline constexpr std::size_t CMD_OBSERVE = BAM_HOST_API_OFFSET_CMD_OBSERVE;
inline constexpr std::size_t ITER_READ_REMOVED = BAM_HOST_API_OFFSET_ITER_READ_REMOVED;
inline constexpr std::size_t SYS_ENTITY_CHILDREN = BAM_HOST_API_OFFSET_SYS_ENTITY_CHILDREN;
inline constexpr std::size_t ASSET_LOAD = BAM_HOST_API_OFFSET_ASSET_LOAD;
inline constexpr std::size_t ASSET_GET_LOAD_STATE = BAM_HOST_API_OFFSET_ASSET_GET_LOAD_STATE;
inline constexpr std::size_t ASSET_RELEASE = BAM_HOST_API_OFFSET_ASSET_RELEASE;
inline constexpr std::size_t CMD_SPAWN_SCENE = BAM_HOST_API_OFFSET_CMD_SPAWN_SCENE;
inline constexpr std::size_t LOG_DIAGNOSTIC = BAM_HOST_API_OFFSET_LOG_DIAGNOSTIC;
inline constexpr std::size_t REPORT_GUEST_FAULT = BAM_HOST_API_OFFSET_REPORT_GUEST_FAULT;
inline constexpr std::size_t RESOLVE_COMPONENT_BY_UUID = BAM_HOST_API_OFFSET_RESOLVE_COMPONENT_BY_UUID;
inline constexpr std::size_t COMPONENT_LAYOUT_INFO = BAM_HOST_API_OFFSET_COMPONENT_LAYOUT_INFO;
inline constexpr std::size_t REGISTER_RELATION = BAM_HOST_API_OFFSET_REGISTER_RELATION;
inline constexpr std::size_t INTERN_SYMBOL = BAM_HOST_API_OFFSET_INTERN_SYMBOL;
inline constexpr std::size_t BIND_NAMED_OBJECT = BAM_HOST_API_OFFSET_BIND_NAMED_OBJECT;
inline constexpr std::size_t ITER_COUNT = BAM_HOST_API_OFFSET_ITER_COUNT;
inline constexpr std::size_t ITER_UP_CASCADE = BAM_HOST_API_OFFSET_ITER_UP_CASCADE;
inline constexpr std::size_t PAR_FOR_EACH_CHUNK_READONLY = BAM_HOST_API_OFFSET_PAR_FOR_EACH_CHUNK_READONLY;
inline constexpr std::size_t REGISTER_SYSTEM_HANDLE = BAM_HOST_API_OFFSET_REGISTER_SYSTEM_HANDLE;
inline constexpr std::size_t CONFIGURE_SCHEDULE_SET = BAM_HOST_API_OFFSET_CONFIGURE_SCHEDULE_SET;
inline constexpr std::size_t MOUNT_SYSTEM_IN_SCHEDULE = BAM_HOST_API_OFFSET_MOUNT_SYSTEM_IN_SCHEDULE;
inline constexpr std::size_t CMD_CHECKPOINT = BAM_HOST_API_OFFSET_CMD_CHECKPOINT;
inline constexpr std::size_t CMD_ROLLBACK = BAM_HOST_API_OFFSET_CMD_ROLLBACK;
inline constexpr std::size_t CMD_INSERT_RELATION = BAM_HOST_API_OFFSET_CMD_INSERT_RELATION;
inline constexpr std::size_t CMD_REMOVE_RELATION = BAM_HOST_API_OFFSET_CMD_REMOVE_RELATION;
inline constexpr std::size_t MIGRATE_COMPONENT_COLUMN = BAM_HOST_API_OFFSET_MIGRATE_COMPONENT_COLUMN;

static_assert(STRUCT_SIZE == 0u, "BamHostApi::struct_size must be at offset 0");
static_assert(ABI_VERSION == 4u, "BamHostApi::abi_version must be at offset 4");
static_assert(GET_PROC_ADDRESS == 8u, "BamHostApi slot 0 must be at offset 8");
static_assert(
    MIGRATE_COMPONENT_COLUMN + sizeof(BamMigrateComponentColumnFn) == sizeof(BamHostApi),
    "BamHostApi tail slot must end at sizeof(BamHostApi)"
);

} // namespace offsets

inline bool is_ok(BamResult res) noexcept {
    return res == BAM_RESULT_OK;
}

inline bool host_api_covers_slot(
    const BamHostApi& api,
    std::size_t slot_offset,
    std::size_t slot_size
) noexcept {
    return bam_host_api_covers_slot(&api, slot_offset, slot_size) != 0;
}

template <typename T>
inline void copy_column_restricted(
    T* BAM_RESTRICT dst,
    const T* BAM_RESTRICT src,
    std::size_t count
) noexcept {
    for (std::size_t i = 0u; i < count; ++i) {
        dst[i] = src[i];
    }
}

template <typename Fn>
inline BamResult guard_boundary(Fn&& fn) noexcept {
    try {
        return fn();
    } catch (const std::exception&) {
        return BAM_RESULT_PANIC_CAUGHT;
    } catch (...) {
        return BAM_RESULT_PANIC_CAUGHT;
    }
}

class HostContextView {
public:
    explicit HostContextView(const BamHostApi& api, const BamHostCtx* ctx) noexcept
        : api_(api), ctx_(ctx) {}

    bool valid() const noexcept { return ctx_ != nullptr; }
    const BamHostCtx* raw() const noexcept { return ctx_; }

    BamResult resolve_component(const char* name, BamComponent& out) const noexcept {
        if (ctx_ == nullptr || api_.resolve_component == nullptr) {
            return BAM_RESULT_NULL_POINTER;
        }
        return api_.resolve_component(ctx_, name, nullptr, &out);
    }

    BamResult resolve_component_by_uuid(const BamComponentUuid& uuid, BamComponent& out) const noexcept {
        if (ctx_ == nullptr ||
            !bam_host_api_covers_slot(&api_, BAM_HOST_API_OFFSET_RESOLVE_COMPONENT_BY_UUID, sizeof(BamResolveComponentByUuidFn)) ||
            api_.resolve_component_by_uuid == nullptr) {
            return BAM_RESULT_UNSUPPORTED;
        }
        return api_.resolve_component_by_uuid(ctx_, &uuid, &out);
    }

    BamResult component_layout_info(BamComponent component, BamLayoutInfo& out) const noexcept {
        if (ctx_ == nullptr ||
            !bam_host_api_covers_slot(&api_, BAM_HOST_API_OFFSET_COMPONENT_LAYOUT_INFO, sizeof(BamComponentLayoutInfoFn)) ||
            api_.component_layout_info == nullptr) {
            return BAM_RESULT_UNSUPPORTED;
        }
        return api_.component_layout_info(ctx_, component, &out);
    }

    BamResult log_diagnostic(uint32_t level, const char* msg) const noexcept {
        if (ctx_ == nullptr ||
            !bam_host_api_covers_slot(&api_, BAM_HOST_API_OFFSET_LOG_DIAGNOSTIC, sizeof(BamLogDiagnosticFn)) ||
            api_.log_diagnostic == nullptr) {
            return BAM_RESULT_UNSUPPORTED;
        }
        return api_.log_diagnostic(ctx_, level, bam_str_view(msg));
    }

    BamResult report_guest_fault(const char* msg) const noexcept {
        if (ctx_ == nullptr ||
            !bam_host_api_covers_slot(&api_, BAM_HOST_API_OFFSET_REPORT_GUEST_FAULT, sizeof(BamReportGuestFaultFn)) ||
            api_.report_guest_fault == nullptr) {
            return BAM_RESULT_UNSUPPORTED;
        }
        return api_.report_guest_fault(ctx_, bam_str_view(msg));
    }

    size_t last_error(char* buf, size_t cap) const noexcept {
        if (ctx_ == nullptr || api_.last_error == nullptr) {
            if (buf != nullptr && cap > 0u) {
                buf[0] = '\0';
            }
            return 0u;
        }
        return api_.last_error(ctx_, buf, cap);
    }

private:
    const BamHostApi& api_;
    const BamHostCtx* ctx_;
};

class LoadContextView {
public:
    explicit LoadContextView(const BamHostApi& api, BamLoadCtx* ctx) noexcept
        : api_(api), ctx_(ctx) {}

    HostContextView host_ctx() const noexcept {
        const BamHostCtx* h = (ctx_ != nullptr && api_.load_host_ctx != nullptr)
            ? api_.load_host_ctx(ctx_)
            : nullptr;
        return HostContextView(api_, h);
    }

    BamResult register_component(const BamComponentDesc& desc, BamComponent& out) const noexcept {
        if (ctx_ == nullptr || api_.register_component == nullptr) {
            return BAM_RESULT_NULL_POINTER;
        }
        return api_.register_component(ctx_, &desc, &out);
    }

    BamResult register_system(const BamSystemDesc& desc) const noexcept {
        if (ctx_ == nullptr || api_.register_system == nullptr) {
            return BAM_RESULT_NULL_POINTER;
        }
        return api_.register_system(ctx_, &desc);
    }

    BamResult register_system_handle(const BamSystemDesc& desc, BamSystem& out) const noexcept {
        if (ctx_ == nullptr ||
            !bam_host_api_covers_slot(&api_, BAM_HOST_API_OFFSET_REGISTER_SYSTEM_HANDLE, sizeof(BamRegisterSystemHandleFn)) ||
            api_.register_system_handle == nullptr) {
            return BAM_RESULT_UNSUPPORTED;
        }
        return api_.register_system_handle(ctx_, &desc, &out);
    }

    BamResult register_observer(const BamObserverDesc& desc) const noexcept {
        if (ctx_ == nullptr || api_.register_observer == nullptr) {
            return BAM_RESULT_NULL_POINTER;
        }
        return api_.register_observer(ctx_, &desc);
    }

private:
    const BamHostApi& api_;
    BamLoadCtx* ctx_;
};

class ChunkView {
public:
    ChunkView(const BamHostApi& api, BamIter* iter, size_t count) noexcept
        : api_(api), iter_(iter), count_(count) {}

    size_t len() const noexcept { return count_; }

    BamResult total_count(size_t& out_count) const noexcept {
        if (iter_ == nullptr ||
            !bam_host_api_covers_slot(&api_, BAM_HOST_API_OFFSET_ITER_COUNT, sizeof(BamIterCountFn)) ||
            api_.iter_count == nullptr) {
            return BAM_RESULT_UNSUPPORTED;
        }
        return api_.iter_count(iter_, &out_count);
    }

    const BamEntity* entities() const noexcept {
        return (iter_ != nullptr && api_.iter_entities != nullptr)
            ? api_.iter_entities(iter_)
            : nullptr;
    }

    template <typename T>
    const T* column_readonly(size_t term_index) const noexcept {
        if (iter_ == nullptr || api_.iter_column_readonly == nullptr) {
            return nullptr;
        }
        return reinterpret_cast<const T*>(api_.iter_column_readonly(iter_, term_index));
    }

    template <typename T>
    T* column_mutable(size_t term_index) const noexcept {
        if (iter_ == nullptr || api_.iter_column_mutable == nullptr) {
            return nullptr;
        }
        return reinterpret_cast<T*>(api_.iter_column_mutable(iter_, term_index));
    }

private:
    const BamHostApi& api_;
    BamIter* iter_;
    size_t count_;
};

class CommandsView {
public:
    CommandsView() noexcept = default;
    CommandsView(const BamHostApi& api, BamCommands* cmds) noexcept
        : api_(&api), cmds_(cmds) {}

    BamCommands* raw() const noexcept { return cmds_; }

    BamResult checkpoint(size_t& out_token) const noexcept {
        if (api_ == nullptr || cmds_ == nullptr ||
            !bam_host_api_covers_slot(api_, BAM_HOST_API_OFFSET_CMD_CHECKPOINT, sizeof(BamCmdCheckpointFn)) ||
            api_->cmd_checkpoint == nullptr) {
            return BAM_RESULT_UNSUPPORTED;
        }
        return api_->cmd_checkpoint(cmds_, &out_token);
    }

    BamResult rollback(size_t token) const noexcept {
        if (api_ == nullptr || cmds_ == nullptr ||
            !bam_host_api_covers_slot(api_, BAM_HOST_API_OFFSET_CMD_ROLLBACK, sizeof(BamCmdRollbackFn)) ||
            api_->cmd_rollback == nullptr) {
            return BAM_RESULT_UNSUPPORTED;
        }
        return api_->cmd_rollback(cmds_, token);
    }

private:
    const BamHostApi* api_ = nullptr;
    BamCommands* cmds_ = nullptr;
};

class SystemContextView {
public:
    SystemContextView(const BamHostApi& api, BamSystemCtx* ctx) noexcept
        : api_(api), ctx_(ctx) {}

    BamResult query(size_t index, BamIter*& out_iter) const noexcept {
        if (ctx_ == nullptr || api_.sys_query == nullptr) {
            return BAM_RESULT_NULL_POINTER;
        }
        return api_.sys_query(ctx_, index, &out_iter);
    }

    template <typename T>
    const T* res_ptr(BamComponent comp, BamChangeTicks* out_ticks = nullptr) const noexcept {
        if (ctx_ == nullptr || api_.sys_res_ptr == nullptr) {
            return nullptr;
        }
        return reinterpret_cast<const T*>(api_.sys_res_ptr(ctx_, comp, out_ticks));
    }

    template <typename T>
    T* res_mut_ptr(BamComponent comp, BamChangeTicks* out_ticks = nullptr) const noexcept {
        if (ctx_ == nullptr || api_.sys_res_mut_ptr == nullptr) {
            return nullptr;
        }
        return reinterpret_cast<T*>(api_.sys_res_mut_ptr(ctx_, comp, out_ticks));
    }

    BamResult commands(BamCommands*& out_cmds) const noexcept {
        if (ctx_ == nullptr || api_.sys_commands == nullptr) {
            return BAM_RESULT_NULL_POINTER;
        }
        return api_.sys_commands(ctx_, &out_cmds);
    }

    BamResult commands_view(CommandsView& out_view) const noexcept {
        BamCommands* cmds = nullptr;
        BamResult res = commands(cmds);
        if (!is_ok(res)) {
            return res;
        }
        out_view = CommandsView(api_, cmds);
        return BAM_RESULT_OK;
    }

private:
    const BamHostApi& api_;
    BamSystemCtx* ctx_;
};

} // namespace bam

#endif // BEVY_ABI_MOD_HPP
"#;

/// Zig SDK wrapper over `bevy_abi_mod.h` (`ABI 0.3.0` `@cImport`).
pub const ZIG_SDK: &str = r#"//! Zig SDK bindings for `bevy_abi_mod` (`ABI 0.3.0`).

pub const c = @cImport({
    @cInclude("bevy_abi_mod.h");
});

/// Zig expresses C99/MSVC `BAM_RESTRICT` non-aliasing guarantees on SoA column pointers via the
/// built-in `noalias` parameter qualifier (for example:
/// `fn integrate(noalias pos: [*]Pos, noalias vel: [*]const Vel, len: usize) void`).
pub const BAM_RESTRICT_QUALIFIER = "noalias";

/// Compile-time byte offsets for `c.BamHostApi` header fields and all 68 function-pointer slots,
/// mirroring `bevy_abi_mod_sys::offsets` and `BAM_HOST_API_OFFSET_*` via `@offsetOf`.
pub const offsets = struct {
    pub const SLOT_COUNT: usize = 68;
    pub const STRUCT_SIZE: usize = @offsetOf(c.BamHostApi, "struct_size");
    pub const ABI_VERSION: usize = @offsetOf(c.BamHostApi, "abi_version");

    pub const GET_PROC_ADDRESS: usize = @offsetOf(c.BamHostApi, "get_proc_address");
    pub const LAST_ERROR: usize = @offsetOf(c.BamHostApi, "last_error");
    pub const LOAD_HOST_CTX: usize = @offsetOf(c.BamHostApi, "load_host_ctx");
    pub const REGISTER_COMPONENT: usize = @offsetOf(c.BamHostApi, "register_component");
    pub const RESOLVE_COMPONENT: usize = @offsetOf(c.BamHostApi, "resolve_component");
    pub const REGISTER_SYSTEM: usize = @offsetOf(c.BamHostApi, "register_system");
    pub const SYS_HOST_CTX: usize = @offsetOf(c.BamHostApi, "sys_host_ctx");
    pub const SYS_QUERY: usize = @offsetOf(c.BamHostApi, "sys_query");
    pub const SYS_COMMANDS: usize = @offsetOf(c.BamHostApi, "sys_commands");
    pub const ITER_NEXT_CHUNK: usize = @offsetOf(c.BamHostApi, "iter_next_chunk");
    pub const ITER_RESET: usize = @offsetOf(c.BamHostApi, "iter_reset");
    pub const ITER_ENTITIES: usize = @offsetOf(c.BamHostApi, "iter_entities");
    pub const ITER_COLUMN_READONLY: usize = @offsetOf(c.BamHostApi, "iter_column_readonly");
    pub const ITER_COLUMN_MUTABLE: usize = @offsetOf(c.BamHostApi, "iter_column_mutable");
    pub const ITER_COLUMN_MUTABLE_UNTRACKED: usize = @offsetOf(c.BamHostApi, "iter_column_mutable_untracked");
    pub const ITER_MARK_CHANGED_RANGE: usize = @offsetOf(c.BamHostApi, "iter_mark_changed_range");
    pub const ITER_COLUMN_ROWS_READONLY: usize = @offsetOf(c.BamHostApi, "iter_column_rows_readonly");
    pub const ITER_COLUMN_ROWS_MUTABLE: usize = @offsetOf(c.BamHostApi, "iter_column_rows_mutable");
    pub const ITER_COLUMN_INFO: usize = @offsetOf(c.BamHostApi, "iter_column_info");
    pub const RES_PTR: usize = @offsetOf(c.BamHostApi, "res_ptr");
    pub const RES_MUT_PTR: usize = @offsetOf(c.BamHostApi, "res_mut_ptr");
    pub const CMD_SPAWN_EMPTY: usize = @offsetOf(c.BamHostApi, "cmd_spawn_empty");
    pub const CMD_SPAWN: usize = @offsetOf(c.BamHostApi, "cmd_spawn");
    pub const CMD_DESPAWN: usize = @offsetOf(c.BamHostApi, "cmd_despawn");
    pub const CMD_INSERT: usize = @offsetOf(c.BamHostApi, "cmd_insert");
    pub const CMD_INSERT_DEFAULT: usize = @offsetOf(c.BamHostApi, "cmd_insert_default");
    pub const CMD_REMOVE: usize = @offsetOf(c.BamHostApi, "cmd_remove");
    pub const PAR_FOR_EACH_CHUNK: usize = @offsetOf(c.BamHostApi, "par_for_each_chunk");
    pub const SYS_ENTITY_GET: usize = @offsetOf(c.BamHostApi, "sys_entity_get");
    pub const SYS_ENTITY_GET_MUT: usize = @offsetOf(c.BamHostApi, "sys_entity_get_mut");
    pub const MSG_READ_BATCH: usize = @offsetOf(c.BamHostApi, "msg_read_batch");
    pub const MSG_SEND: usize = @offsetOf(c.BamHostApi, "msg_send");
    pub const MSG_WRITE_BATCH: usize = @offsetOf(c.BamHostApi, "msg_write_batch");
    pub const RES_TICKS: usize = @offsetOf(c.BamHostApi, "res_ticks");
    pub const SYS_RES_PTR: usize = @offsetOf(c.BamHostApi, "sys_res_ptr");
    pub const SYS_RES_MUT_PTR: usize = @offsetOf(c.BamHostApi, "sys_res_mut_ptr");
    pub const REGISTER_OBSERVER: usize = @offsetOf(c.BamHostApi, "register_observer");
    pub const CMD_TRIGGER: usize = @offsetOf(c.BamHostApi, "cmd_trigger");
    pub const CMD_SPAWN_PERSISTENT: usize = @offsetOf(c.BamHostApi, "cmd_spawn_persistent");
    pub const CMD_SPAWN_BATCH: usize = @offsetOf(c.BamHostApi, "cmd_spawn_batch");
    pub const CMD_DESPAWN_ALL: usize = @offsetOf(c.BamHostApi, "cmd_despawn_all");
    pub const CMD_INSERT_IF_NEW: usize = @offsetOf(c.BamHostApi, "cmd_insert_if_new");
    pub const CMD_TRIGGER_EX: usize = @offsetOf(c.BamHostApi, "cmd_trigger_ex");
    pub const CMD_OBSERVE: usize = @offsetOf(c.BamHostApi, "cmd_observe");
    pub const ITER_READ_REMOVED: usize = @offsetOf(c.BamHostApi, "iter_read_removed");
    pub const SYS_ENTITY_CHILDREN: usize = @offsetOf(c.BamHostApi, "sys_entity_children");
    pub const ASSET_LOAD: usize = @offsetOf(c.BamHostApi, "asset_load");
    pub const ASSET_GET_LOAD_STATE: usize = @offsetOf(c.BamHostApi, "asset_get_load_state");
    pub const ASSET_RELEASE: usize = @offsetOf(c.BamHostApi, "asset_release");
    pub const CMD_SPAWN_SCENE: usize = @offsetOf(c.BamHostApi, "cmd_spawn_scene");
    pub const LOG_DIAGNOSTIC: usize = @offsetOf(c.BamHostApi, "log_diagnostic");
    pub const REPORT_GUEST_FAULT: usize = @offsetOf(c.BamHostApi, "report_guest_fault");
    pub const RESOLVE_COMPONENT_BY_UUID: usize = @offsetOf(c.BamHostApi, "resolve_component_by_uuid");
    pub const COMPONENT_LAYOUT_INFO: usize = @offsetOf(c.BamHostApi, "component_layout_info");
    pub const REGISTER_RELATION: usize = @offsetOf(c.BamHostApi, "register_relation");
    pub const INTERN_SYMBOL: usize = @offsetOf(c.BamHostApi, "intern_symbol");
    pub const BIND_NAMED_OBJECT: usize = @offsetOf(c.BamHostApi, "bind_named_object");
    pub const ITER_COUNT: usize = @offsetOf(c.BamHostApi, "iter_count");
    pub const ITER_UP_CASCADE: usize = @offsetOf(c.BamHostApi, "iter_up_cascade");
    pub const PAR_FOR_EACH_CHUNK_READONLY: usize = @offsetOf(c.BamHostApi, "par_for_each_chunk_readonly");
    pub const REGISTER_SYSTEM_HANDLE: usize = @offsetOf(c.BamHostApi, "register_system_handle");
    pub const CONFIGURE_SCHEDULE_SET: usize = @offsetOf(c.BamHostApi, "configure_schedule_set");
    pub const MOUNT_SYSTEM_IN_SCHEDULE: usize = @offsetOf(c.BamHostApi, "mount_system_in_schedule");
    pub const CMD_CHECKPOINT: usize = @offsetOf(c.BamHostApi, "cmd_checkpoint");
    pub const CMD_ROLLBACK: usize = @offsetOf(c.BamHostApi, "cmd_rollback");
    pub const CMD_INSERT_RELATION: usize = @offsetOf(c.BamHostApi, "cmd_insert_relation");
    pub const CMD_REMOVE_RELATION: usize = @offsetOf(c.BamHostApi, "cmd_remove_relation");
    pub const MIGRATE_COMPONENT_COLUMN: usize = @offsetOf(c.BamHostApi, "migrate_component_column");
};

pub const BAM_HOST_API_OFFSET_GET_PROC_ADDRESS: usize = offsets.GET_PROC_ADDRESS;
pub const BAM_HOST_API_OFFSET_LAST_ERROR: usize = offsets.LAST_ERROR;
pub const BAM_HOST_API_OFFSET_LOAD_HOST_CTX: usize = offsets.LOAD_HOST_CTX;
pub const BAM_HOST_API_OFFSET_REGISTER_COMPONENT: usize = offsets.REGISTER_COMPONENT;
pub const BAM_HOST_API_OFFSET_RESOLVE_COMPONENT: usize = offsets.RESOLVE_COMPONENT;
pub const BAM_HOST_API_OFFSET_REGISTER_SYSTEM: usize = offsets.REGISTER_SYSTEM;
pub const BAM_HOST_API_OFFSET_SYS_HOST_CTX: usize = offsets.SYS_HOST_CTX;
pub const BAM_HOST_API_OFFSET_SYS_QUERY: usize = offsets.SYS_QUERY;
pub const BAM_HOST_API_OFFSET_SYS_COMMANDS: usize = offsets.SYS_COMMANDS;
pub const BAM_HOST_API_OFFSET_ITER_NEXT_CHUNK: usize = offsets.ITER_NEXT_CHUNK;
pub const BAM_HOST_API_OFFSET_ITER_RESET: usize = offsets.ITER_RESET;
pub const BAM_HOST_API_OFFSET_ITER_ENTITIES: usize = offsets.ITER_ENTITIES;
pub const BAM_HOST_API_OFFSET_ITER_COLUMN_READONLY: usize = offsets.ITER_COLUMN_READONLY;
pub const BAM_HOST_API_OFFSET_ITER_COLUMN_MUTABLE: usize = offsets.ITER_COLUMN_MUTABLE;
pub const BAM_HOST_API_OFFSET_ITER_COLUMN_MUTABLE_UNTRACKED: usize = offsets.ITER_COLUMN_MUTABLE_UNTRACKED;
pub const BAM_HOST_API_OFFSET_ITER_MARK_CHANGED_RANGE: usize = offsets.ITER_MARK_CHANGED_RANGE;
pub const BAM_HOST_API_OFFSET_ITER_COLUMN_ROWS_READONLY: usize = offsets.ITER_COLUMN_ROWS_READONLY;
pub const BAM_HOST_API_OFFSET_ITER_COLUMN_ROWS_MUTABLE: usize = offsets.ITER_COLUMN_ROWS_MUTABLE;
pub const BAM_HOST_API_OFFSET_ITER_COLUMN_INFO: usize = offsets.ITER_COLUMN_INFO;
pub const BAM_HOST_API_OFFSET_RES_PTR: usize = offsets.RES_PTR;
pub const BAM_HOST_API_OFFSET_RES_MUT_PTR: usize = offsets.RES_MUT_PTR;
pub const BAM_HOST_API_OFFSET_CMD_SPAWN_EMPTY: usize = offsets.CMD_SPAWN_EMPTY;
pub const BAM_HOST_API_OFFSET_CMD_SPAWN: usize = offsets.CMD_SPAWN;
pub const BAM_HOST_API_OFFSET_CMD_DESPAWN: usize = offsets.CMD_DESPAWN;
pub const BAM_HOST_API_OFFSET_CMD_INSERT: usize = offsets.CMD_INSERT;
pub const BAM_HOST_API_OFFSET_CMD_INSERT_DEFAULT: usize = offsets.CMD_INSERT_DEFAULT;
pub const BAM_HOST_API_OFFSET_CMD_REMOVE: usize = offsets.CMD_REMOVE;
pub const BAM_HOST_API_OFFSET_PAR_FOR_EACH_CHUNK: usize = offsets.PAR_FOR_EACH_CHUNK;
pub const BAM_HOST_API_OFFSET_SYS_ENTITY_GET: usize = offsets.SYS_ENTITY_GET;
pub const BAM_HOST_API_OFFSET_SYS_ENTITY_GET_MUT: usize = offsets.SYS_ENTITY_GET_MUT;
pub const BAM_HOST_API_OFFSET_MSG_READ_BATCH: usize = offsets.MSG_READ_BATCH;
pub const BAM_HOST_API_OFFSET_MSG_SEND: usize = offsets.MSG_SEND;
pub const BAM_HOST_API_OFFSET_MSG_WRITE_BATCH: usize = offsets.MSG_WRITE_BATCH;
pub const BAM_HOST_API_OFFSET_RES_TICKS: usize = offsets.RES_TICKS;
pub const BAM_HOST_API_OFFSET_SYS_RES_PTR: usize = offsets.SYS_RES_PTR;
pub const BAM_HOST_API_OFFSET_SYS_RES_MUT_PTR: usize = offsets.SYS_RES_MUT_PTR;
pub const BAM_HOST_API_OFFSET_REGISTER_OBSERVER: usize = offsets.REGISTER_OBSERVER;
pub const BAM_HOST_API_OFFSET_CMD_TRIGGER: usize = offsets.CMD_TRIGGER;
pub const BAM_HOST_API_OFFSET_CMD_SPAWN_PERSISTENT: usize = offsets.CMD_SPAWN_PERSISTENT;
pub const BAM_HOST_API_OFFSET_CMD_SPAWN_BATCH: usize = offsets.CMD_SPAWN_BATCH;
pub const BAM_HOST_API_OFFSET_CMD_DESPAWN_ALL: usize = offsets.CMD_DESPAWN_ALL;
pub const BAM_HOST_API_OFFSET_CMD_INSERT_IF_NEW: usize = offsets.CMD_INSERT_IF_NEW;
pub const BAM_HOST_API_OFFSET_CMD_TRIGGER_EX: usize = offsets.CMD_TRIGGER_EX;
pub const BAM_HOST_API_OFFSET_CMD_OBSERVE: usize = offsets.CMD_OBSERVE;
pub const BAM_HOST_API_OFFSET_ITER_READ_REMOVED: usize = offsets.ITER_READ_REMOVED;
pub const BAM_HOST_API_OFFSET_SYS_ENTITY_CHILDREN: usize = offsets.SYS_ENTITY_CHILDREN;
pub const BAM_HOST_API_OFFSET_ASSET_LOAD: usize = offsets.ASSET_LOAD;
pub const BAM_HOST_API_OFFSET_ASSET_GET_LOAD_STATE: usize = offsets.ASSET_GET_LOAD_STATE;
pub const BAM_HOST_API_OFFSET_ASSET_RELEASE: usize = offsets.ASSET_RELEASE;
pub const BAM_HOST_API_OFFSET_CMD_SPAWN_SCENE: usize = offsets.CMD_SPAWN_SCENE;
pub const BAM_HOST_API_OFFSET_LOG_DIAGNOSTIC: usize = offsets.LOG_DIAGNOSTIC;
pub const BAM_HOST_API_OFFSET_REPORT_GUEST_FAULT: usize = offsets.REPORT_GUEST_FAULT;
pub const BAM_HOST_API_OFFSET_RESOLVE_COMPONENT_BY_UUID: usize = offsets.RESOLVE_COMPONENT_BY_UUID;
pub const BAM_HOST_API_OFFSET_COMPONENT_LAYOUT_INFO: usize = offsets.COMPONENT_LAYOUT_INFO;
pub const BAM_HOST_API_OFFSET_REGISTER_RELATION: usize = offsets.REGISTER_RELATION;
pub const BAM_HOST_API_OFFSET_INTERN_SYMBOL: usize = offsets.INTERN_SYMBOL;
pub const BAM_HOST_API_OFFSET_BIND_NAMED_OBJECT: usize = offsets.BIND_NAMED_OBJECT;
pub const BAM_HOST_API_OFFSET_ITER_COUNT: usize = offsets.ITER_COUNT;
pub const BAM_HOST_API_OFFSET_ITER_UP_CASCADE: usize = offsets.ITER_UP_CASCADE;
pub const BAM_HOST_API_OFFSET_PAR_FOR_EACH_CHUNK_READONLY: usize = offsets.PAR_FOR_EACH_CHUNK_READONLY;
pub const BAM_HOST_API_OFFSET_REGISTER_SYSTEM_HANDLE: usize = offsets.REGISTER_SYSTEM_HANDLE;
pub const BAM_HOST_API_OFFSET_CONFIGURE_SCHEDULE_SET: usize = offsets.CONFIGURE_SCHEDULE_SET;
pub const BAM_HOST_API_OFFSET_MOUNT_SYSTEM_IN_SCHEDULE: usize = offsets.MOUNT_SYSTEM_IN_SCHEDULE;
pub const BAM_HOST_API_OFFSET_CMD_CHECKPOINT: usize = offsets.CMD_CHECKPOINT;
pub const BAM_HOST_API_OFFSET_CMD_ROLLBACK: usize = offsets.CMD_ROLLBACK;
pub const BAM_HOST_API_OFFSET_CMD_INSERT_RELATION: usize = offsets.CMD_INSERT_RELATION;
pub const BAM_HOST_API_OFFSET_CMD_REMOVE_RELATION: usize = offsets.CMD_REMOVE_RELATION;
pub const BAM_HOST_API_OFFSET_MIGRATE_COMPONENT_COLUMN: usize = offsets.MIGRATE_COMPONENT_COLUMN;

comptime {
    if (offsets.STRUCT_SIZE != 0 or offsets.ABI_VERSION != 4 or offsets.GET_PROC_ADDRESS != 8) {
        @compileError("BamHostApi header offsets mismatch");
    }
    if (offsets.MIGRATE_COMPONENT_COLUMN + @sizeOf(usize) != @sizeOf(c.BamHostApi)) {
        @compileError("BamHostApi tail slot offset mismatch");
    }
}

/// Returns `true` when `api.struct_size` covers a slot of `slot_size` bytes at `slot_offset`.
pub inline fn coversHostApiSlot(api: *const c.BamHostApi, slot_offset: usize, slot_size: usize) bool {
    const sz: usize = @as(usize, api.struct_size);
    return slot_offset <= sz and slot_size <= (sz - slot_offset);
}

/// Copies `len` elements between non-overlapping SoA columns using Zig's `noalias`
/// parameter qualifier (the Zig equivalent of C99/MSVC `BAM_RESTRICT`).
pub fn copyColumnNoAlias(
    comptime T: type,
    noalias dst: [*]T,
    noalias src: [*]const T,
    len: usize,
) void {
    @memcpy(dst[0..len], src[0..len]);
}

pub const BamError = error{
    NullPointer,
    InvalidVersion,
    InvalidComponent,
    AccessConflict,
    InvalidArgument,
    OutOfRange,
    NotFound,
    InvalidState,
    TypeMismatch,
    OutOfMemory,
    Unsupported,
    InternalError,
};

pub fn checkResult(res: c.BamResult) BamError!void {
    return switch (res) {
        c.BAM_RESULT_OK => {},
        c.BAM_RESULT_NULL_POINTER => error.NullPointer,
        c.BAM_RESULT_INVALID_VERSION => error.InvalidVersion,
        c.BAM_RESULT_INVALID_COMPONENT => error.InvalidComponent,
        c.BAM_RESULT_ACCESS_CONFLICT => error.AccessConflict,
        c.BAM_RESULT_INVALID_ARGUMENT => error.InvalidArgument,
        c.BAM_RESULT_OUT_OF_RANGE => error.OutOfRange,
        c.BAM_RESULT_NOT_FOUND => error.NotFound,
        c.BAM_RESULT_INVALID_STATE => error.InvalidState,
        c.BAM_RESULT_TYPE_MISMATCH => error.TypeMismatch,
        c.BAM_RESULT_UNSUPPORTED => error.Unsupported,
        else => error.InternalError,
    };
}

pub fn mapError(err: BamError) c.BamResult {
    return switch (err) {
        error.NullPointer => c.BAM_RESULT_NULL_POINTER,
        error.InvalidVersion => c.BAM_RESULT_INVALID_VERSION,
        error.InvalidComponent => c.BAM_RESULT_INVALID_COMPONENT,
        error.AccessConflict => c.BAM_RESULT_ACCESS_CONFLICT,
        error.InvalidArgument => c.BAM_RESULT_INVALID_ARGUMENT,
        error.OutOfRange => c.BAM_RESULT_OUT_OF_RANGE,
        error.NotFound => c.BAM_RESULT_NOT_FOUND,
        error.InvalidState => c.BAM_RESULT_INVALID_STATE,
        error.TypeMismatch => c.BAM_RESULT_TYPE_MISMATCH,
        error.Unsupported => c.BAM_RESULT_UNSUPPORTED,
        error.OutOfMemory, error.InternalError => c.BAM_RESULT_INTERNAL_ERROR,
    };
}
"#;

/// Exports `bevy_abi_mod.h`, `bevy_abi_mod.hpp`, and `bevy_abi_mod.zig` into `out_dir`.
///
/// # Errors
/// Returns an `io::Error` if writing any SDK artifact fails or C header layout checks fail.
pub fn export_sdk(out_dir: &Path) -> Result<Vec<PathBuf>, std::io::Error> {
    fs::create_dir_all(out_dir)?;
    let c_hdr = crate::render_c99_header().map_err(std::io::Error::other)?;
    let files: [(&str, &str); 3] = [
        ("bevy_abi_mod.h", &c_hdr),
        ("bevy_abi_mod.hpp", CPP17_HEADER),
        ("bevy_abi_mod.zig", ZIG_SDK),
    ];
    let mut written = Vec::with_capacity(files.len());
    for (filename, content) in files {
        let dest = out_dir.join(filename);
        fs::write(&dest, content.as_bytes())?;
        written.push(dest);
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use bevy_abi_mod_sys::BAM_HOST_API_SLOT_COUNT;

    use super::*;

    #[test]
    fn export_sdk_writes_all_three_abi_0_3_headers() {
        let tmp = std::env::temp_dir().join(format!("bam_sdk_gen_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        let written = export_sdk(&tmp).expect("export_sdk must succeed");
        assert_eq!(written.len(), 3);
        let h_content = fs::read_to_string(tmp.join("bevy_abi_mod.h")).unwrap();
        let hpp_content = fs::read_to_string(tmp.join("bevy_abi_mod.hpp")).unwrap();
        let zig_content = fs::read_to_string(tmp.join("bevy_abi_mod.zig")).unwrap();
        assert!(h_content.contains("#define BAM_ABI_VERSION 0x00030000u"));
        assert!(h_content.contains("#  define BAM_RESTRICT restrict"));
        assert!(hpp_content.contains("BAM_HOST_API_OFFSET_CMD_CHECKPOINT"));
        assert!(hpp_content.contains("BAM_RESTRICT"));
        assert!(zig_content.contains("ABI 0.3.0"));
        assert!(zig_content.contains("BAM_RESTRICT_QUALIFIER"));
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn all_68_host_api_offsets_and_restrict_mirrored_across_c_cpp_and_zig() {
        let h_content = c99_header();
        let slots = crate::header_gen::host_api_slot_names();
        assert_eq!(slots.len(), BAM_HOST_API_SLOT_COUNT);

        assert!(h_content.contains("#if defined(_MSC_VER)\n#  define BAM_RESTRICT __restrict\n"));
        assert!(h_content.contains(
            "#elif !defined(__cplusplus) && defined(__STDC_VERSION__) && (__STDC_VERSION__ >= 199901L)\n#  define BAM_RESTRICT restrict\n"
        ));
        assert!(CPP17_HEADER.contains("#ifndef BAM_RESTRICT"));
        assert!(CPP17_HEADER.contains("T* BAM_RESTRICT dst"));
        assert!(CPP17_HEADER.contains(
            "inline constexpr std::size_t STRUCT_SIZE = offsetof(BamHostApi, struct_size);"
        ));
        assert!(CPP17_HEADER.contains(
            "inline constexpr std::size_t ABI_VERSION = offsetof(BamHostApi, abi_version);"
        ));
        assert!(
            ZIG_SDK.contains(
                "pub const STRUCT_SIZE: usize = @offsetOf(c.BamHostApi, \"struct_size\");"
            )
        );
        assert!(
            ZIG_SDK.contains(
                "pub const ABI_VERSION: usize = @offsetOf(c.BamHostApi, \"abi_version\");"
            )
        );
        assert!(ZIG_SDK.contains("noalias dst: [*]T"));

        for slot in slots {
            let upper = slot.to_ascii_uppercase();
            let c_line =
                format!("#define BAM_HOST_API_OFFSET_{upper} offsetof(BamHostApi, {slot})");
            let cpp_line =
                format!("inline constexpr std::size_t {upper} = BAM_HOST_API_OFFSET_{upper};");
            let zig_offset_line =
                format!("pub const {upper}: usize = @offsetOf(c.BamHostApi, \"{slot}\");");
            let zig_alias_line =
                format!("pub const BAM_HOST_API_OFFSET_{upper}: usize = offsets.{upper};");

            assert!(h_content.contains(&c_line), "C99 header missing: {c_line}");
            assert!(
                CPP17_HEADER.contains(&cpp_line),
                "CPP17_HEADER missing: {cpp_line}"
            );
            assert!(
                ZIG_SDK.contains(&zig_offset_line),
                "ZIG_SDK missing: {zig_offset_line}"
            );
            assert!(
                ZIG_SDK.contains(&zig_alias_line),
                "ZIG_SDK missing: {zig_alias_line}"
            );
        }
    }
}
