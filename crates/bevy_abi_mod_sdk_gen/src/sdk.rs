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

namespace bam {

inline bool is_ok(BamResult res) noexcept {
    return res == BAM_RESULT_OK;
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
        return static_cast<const T*>(api_.iter_column_readonly(iter_, term_index));
    }

    template <typename T>
    T* column_mutable(size_t term_index) const noexcept {
        if (iter_ == nullptr || api_.iter_column_mutable == nullptr) {
            return nullptr;
        }
        return static_cast<T*>(api_.iter_column_mutable(iter_, term_index));
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
        return static_cast<const T*>(api_.sys_res_ptr(ctx_, comp, out_ticks));
    }

    template <typename T>
    T* res_mut_ptr(BamComponent comp, BamChangeTicks* out_ticks = nullptr) const noexcept {
        if (ctx_ == nullptr || api_.sys_res_mut_ptr == nullptr) {
            return nullptr;
        }
        return static_cast<T*>(api_.sys_res_mut_ptr(ctx_, comp, out_ticks));
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
        assert!(hpp_content.contains("BAM_HOST_API_OFFSET_CMD_CHECKPOINT"));
        assert!(zig_content.contains("ABI 0.3.0"));
        let _ = fs::remove_dir_all(&tmp);
    }
}
