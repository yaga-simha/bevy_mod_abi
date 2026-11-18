#ifndef BAM_CPP17_SDK_HPP
#define BAM_CPP17_SDK_HPP

#include "../../../include/bevy_abi_mod.h"

#include <cstddef>
#include <cstdint>
#include <vector>

namespace bam {

class Commands {
public:
    Commands(const BamHostApi& api, BamCommands* cmds) noexcept : api_(api), cmds_(cmds) {}

    bool valid() const noexcept { return cmds_ != nullptr; }

    BamResult spawn_batch(
        const BamComponent* components,
        const uint8_t* const* column_ptrs,
        size_t component_count,
        size_t entity_count,
        BamEntity* out_entities = nullptr
    ) const noexcept {
        if (cmds_ == nullptr || api_.cmd_spawn_batch == nullptr) {
            return BAM_RESULT_NULL_POINTER;
        }
        return api_.cmd_spawn_batch(
            cmds_, components, column_ptrs, component_count, entity_count, out_entities);
    }

private:
    const BamHostApi& api_;
    BamCommands*      cmds_{nullptr};
};

class Chunk {
public:
    Chunk(const BamHostApi& api, BamIter* iter, size_t len) noexcept
        : api_(api), iter_(iter), len_(len) {}

    size_t len() const noexcept { return len_; }

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

    const BamBool* has_column(size_t term_index) const noexcept {
        return column_readonly<BamBool>(term_index);
    }

    uint8_t* rows_mutable(size_t term_index, size_t row_index, size_t elem_size) const noexcept {
        uint8_t* base = column_mutable<uint8_t>(term_index);
        return base != nullptr ? (base + row_index * elem_size) : nullptr;
    }

private:
    const BamHostApi& api_;
    BamIter*          iter_{nullptr};
    size_t            len_{0u};
};

class SystemContext {
public:
    SystemContext(const BamHostApi& api, BamSystemCtx* ctx) noexcept : api_(api), ctx_(ctx) {}

    BamResult query(size_t query_index, BamIter*& out_iter) const noexcept {
        if (ctx_ == nullptr || api_.sys_query == nullptr) {
            return BAM_RESULT_NULL_POINTER;
        }
        return api_.sys_query(ctx_, query_index, &out_iter);
    }

    BamResult column_info(BamIter* iter, size_t term_index, BamColumnInfo& out_info) const noexcept {
        if (iter == nullptr || api_.iter_column_info == nullptr) {
            return BAM_RESULT_NULL_POINTER;
        }
        return api_.iter_column_info(iter, term_index, &out_info);
    }

    bool next_chunk(BamIter* iter, size_t& out_len) const noexcept {
        if (iter == nullptr || api_.iter_next_chunk == nullptr) {
            out_len = 0u;
            return false;
        }
        return api_.iter_next_chunk(iter, &out_len) != BAM_FALSE;
    }

    template <typename T>
    const T* res_ptr(BamIter* iter, size_t term_index) const noexcept {
        if (iter == nullptr || api_.res_ptr == nullptr) {
            return nullptr;
        }
        return reinterpret_cast<const T*>(api_.res_ptr(iter, term_index));
    }

    template <typename T>
    T* res_mut_ptr(BamIter* iter, size_t term_index) const noexcept {
        if (iter == nullptr || api_.res_mut_ptr == nullptr) {
            return nullptr;
        }
        return reinterpret_cast<T*>(api_.res_mut_ptr(iter, term_index));
    }

    Commands commands(BamResult* out_err = nullptr) const noexcept {
        BamCommands* raw = nullptr;
        BamResult rc = BAM_RESULT_NULL_POINTER;
        if (ctx_ != nullptr && api_.sys_commands != nullptr) {
            rc = api_.sys_commands(ctx_, &raw);
        }
        if (out_err != nullptr) {
            *out_err = rc;
        }
        return Commands(api_, raw);
    }

private:
    const BamHostApi& api_;
    BamSystemCtx*     ctx_{nullptr};
};

class GuestApp {
public:
    GuestApp(const BamHostApi& api, BamLoadCtx* load_ctx) noexcept
        : api_(api), load_ctx_(load_ctx) {
        if (load_ctx_ != nullptr && api_.load_host_ctx != nullptr) {
            host_ctx_ = api_.load_host_ctx(load_ctx_);
        }
    }

    bool valid() const noexcept { return load_ctx_ != nullptr && host_ctx_ != nullptr; }
    const BamHostCtx* host_ctx() const noexcept { return host_ctx_; }

    BamResult resolve_component(const char* name, BamComponent& out_comp) const noexcept {
        if (host_ctx_ == nullptr || api_.resolve_component == nullptr) {
            return BAM_RESULT_NULL_POINTER;
        }
        return api_.resolve_component(host_ctx_, name, nullptr, &out_comp);
    }

    bool try_resolve_component(const char* name, BamComponent& out_comp) const noexcept {
        return resolve_component(name, out_comp) == BAM_RESULT_OK &&
               bam_component_is_valid(out_comp) != BAM_FALSE;
    }

    BamResult register_system(
        const char* name,
        const BamTermDesc* terms,
        size_t term_count,
        BamSystemFn callback,
        void* user_data,
        BamScheduleId schedule = BAM_SCHEDULE_UPDATE
    ) const noexcept {
        if (load_ctx_ == nullptr || api_.register_system == nullptr) {
            return BAM_RESULT_NULL_POINTER;
        }
        BamQueryDesc query{};
        query.struct_size = static_cast<uint32_t>(sizeof(BamQueryDesc));
        query.terms       = terms;
        query.term_count  = term_count;

        BamSystemDesc sys_desc{};
        sys_desc.struct_size = static_cast<uint32_t>(sizeof(BamSystemDesc));
        sys_desc.name        = name;
        sys_desc.queries     = (term_count > 0u && terms != nullptr) ? &query : nullptr;
        sys_desc.query_count = (term_count > 0u && terms != nullptr) ? 1u : 0u;
        sys_desc.callback    = callback;
        sys_desc.user_data   = user_data;
        sys_desc.schedule    = schedule;
        return api_.register_system(load_ctx_, &sys_desc);
    }

    BamResult register_lifecycle_observer(
        const char* name,
        BamEventKind event_kind,
        BamComponent watched_component,
        BamObserverFn callback,
        void* user_data
    ) const noexcept {
        if (load_ctx_ == nullptr || api_.register_observer == nullptr) {
            return BAM_RESULT_NULL_POINTER;
        }
        BamObserverDesc desc{};
        desc.struct_size             = static_cast<uint32_t>(sizeof(BamObserverDesc));
        desc.name                    = name;
        desc.event                   = BAM_COMPONENT_INVALID;
        desc.callback                = callback;
        desc.user_data               = user_data;
        desc.event_kind              = event_kind;
        desc.watched_components      = &watched_component;
        desc.watched_component_count = 1u;
        return api_.register_observer(load_ctx_, &desc);
    }

private:
    const BamHostApi& api_;
    BamLoadCtx*       load_ctx_{nullptr};
    const BamHostCtx* host_ctx_{nullptr};
};

} // namespace bam

#endif // BAM_CPP17_SDK_HPP
