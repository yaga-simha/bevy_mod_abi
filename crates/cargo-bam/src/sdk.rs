//! SDK header export (`bevy_abi_mod.h`, `bevy_abi_mod.hpp`, `bevy_abi_mod.zig`) for `cargo-bam`.

use std::fs;
use std::path::{Path, PathBuf};

/// Canonical strict ISO C99 ABI header embedded directly from `include/bevy_abi_mod.h`.
pub const C99_HEADER: &str = include_str!("../../../include/bevy_abi_mod.h");

/// Ergonomic ISO C++17 RAII wrapper header over `bevy_abi_mod.h`.
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

private:
    const BamHostApi& api_;
    BamSystemCtx* ctx_;
};

} // namespace bam

#endif // BEVY_ABI_MOD_HPP
"#;

/// Zig SDK wrapper over `bevy_abi_mod.h` (`@cImport`).
pub const ZIG_SDK: &str = r#"//! Zig SDK bindings for `bevy_abi_mod` (`ABI 0.2.0`).

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
pub fn export_sdk(out_dir: &Path) -> Result<Vec<PathBuf>, std::io::Error> {
    fs::create_dir_all(out_dir)?;
    let files: [(&str, &str); 3] = [
        ("bevy_abi_mod.h", C99_HEADER),
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
