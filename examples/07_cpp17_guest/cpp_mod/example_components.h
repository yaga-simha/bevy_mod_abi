/* AUTO-GENERATED FROM `ModComponent::C_DECL` — DO NOT EDIT BY HAND. */
#ifndef EXAMPLE_07_COMPONENTS_H
#define EXAMPLE_07_COMPONENTS_H

#include "../../../include/bevy_abi_mod.h"

#ifdef __cplusplus
extern "C" {
#endif

typedef struct Position {
    float x;
    float y;
    float z;
} Position;

typedef struct PositionVTable {
    BamVTableHeader header;
    float (*get_x)(const void* ptr);
    BamResult (*set_x)(void* ptr, float val);
    float (*get_y)(const void* ptr);
    BamResult (*set_y)(void* ptr, float val);
    float (*get_z)(const void* ptr);
    BamResult (*set_z)(void* ptr, float val);
} PositionVTable;
BAM_STATIC_ASSERT(offsetof(PositionVTable, header) == 0u, positionvtable_header_off);
#if UINTPTR_MAX == UINT64_MAX
BAM_STATIC_ASSERT(sizeof(PositionVTable) == 88u, positionvtable_size64);
#elif UINTPTR_MAX == UINT32_MAX
BAM_STATIC_ASSERT(sizeof(PositionVTable) == 56u, positionvtable_size32);
#endif
#ifdef __cplusplus
extern "C++" {
namespace bam {
#ifndef BAM_CPP17_SDK_HELPERS_DEFINED
#define BAM_CPP17_SDK_HELPERS_DEFINED

template <typename VT>
inline const VT* resolve_vtable(
    const BamHostApi& api,
    const BamHostCtx* host_ctx,
    const char* symbol,
    BamResult* out_err = nullptr
) noexcept {
    if (api.get_proc_address == nullptr || host_ctx == nullptr || symbol == nullptr) {
        if (out_err != nullptr) {
            *out_err = BAM_RESULT_NULL_POINTER;
        }
        return nullptr;
    }
    const void* raw = api.get_proc_address(host_ctx, symbol);
    if (raw == nullptr) {
        if (out_err != nullptr) {
            *out_err = BAM_RESULT_NOT_FOUND;
        }
        return nullptr;
    }
    const auto* hdr = static_cast<const BamVTableHeader*>(raw);
    if (hdr->struct_size < static_cast<uint32_t>(sizeof(VT)) ||
        bam_version_is_compatible(BAM_ABI_VERSION, hdr->abi_version) == BAM_FALSE) {
        if (out_err != nullptr) {
            *out_err = BAM_RESULT_INVALID_VERSION;
        }
        return nullptr;
    }
    if (out_err != nullptr) {
        *out_err = BAM_RESULT_OK;
    }
    return static_cast<const VT*>(raw);
}

template <typename VT>
class VTableHandle {
public:
    VTableHandle() noexcept = default;
    explicit VTableHandle(const VT* ptr) noexcept : ptr_(ptr) {}

    static VTableHandle resolve(
        const BamHostApi& api,
        const BamHostCtx* host_ctx,
        const char* symbol,
        BamResult* out_err = nullptr
    ) noexcept {
        return VTableHandle(resolve_vtable<VT>(api, host_ctx, symbol, out_err));
    }

    bool valid() const noexcept { return ptr_ != nullptr; }
    explicit operator bool() const noexcept { return valid(); }
    const VT* raw() const noexcept { return ptr_; }
    const VT* operator->() const noexcept { return ptr_; }

private:
    const VT* ptr_{nullptr};
};

template <typename InstanceT, void (*DestroyFn)(InstanceT*)>
class UniqueGuest {
public:
    UniqueGuest() noexcept = default;
    explicit UniqueGuest(InstanceT* ptr) noexcept : ptr_(ptr) {}
    ~UniqueGuest() noexcept { reset(); }

    UniqueGuest(const UniqueGuest&) = delete;
    UniqueGuest& operator=(const UniqueGuest&) = delete;

    UniqueGuest(UniqueGuest&& other) noexcept : ptr_(other.release()) {}
    UniqueGuest& operator=(UniqueGuest&& other) noexcept {
        if (this != &other) {
            reset(other.release());
        }
        return *this;
    }

    InstanceT* get() const noexcept { return ptr_; }
    InstanceT* operator->() const noexcept { return ptr_; }
    InstanceT& operator*() const noexcept { return *ptr_; }
    explicit operator bool() const noexcept { return ptr_ != nullptr; }

    InstanceT* release() noexcept {
        InstanceT* tmp = ptr_;
        ptr_ = nullptr;
        return tmp;
    }

    void reset(InstanceT* next = nullptr) noexcept {
        if constexpr (DestroyFn != nullptr) {
            if (ptr_ != nullptr) {
                DestroyFn(ptr_);
            }
        }
        ptr_ = next;
    }

private:
    InstanceT* ptr_{nullptr};
};

template <typename Elem, typename SliceTy, typename Ops>
class HostVecView {
public:
    HostVecView(void* ptr, const Ops& ops) noexcept : ptr_(ptr), ops_(ops) {}

    bool valid() const noexcept { return ptr_ != nullptr && ops_.valid(); }
    explicit operator bool() const noexcept { return valid(); }

    size_t len() const noexcept {
        return (ptr_ != nullptr && ops_.len != nullptr) ? ops_.len(ptr_) : 0u;
    }
    bool empty() const noexcept { return len() == 0u; }

    SliceTy as_slice() const noexcept {
        if (ptr_ == nullptr || ops_.get == nullptr) {
            return SliceTy{nullptr, 0u};
        }
        return ops_.get(ptr_);
    }

    const Elem* data() const noexcept { return as_slice().ptr; }

    Elem* data_mut(size_t* out_len = nullptr) noexcept {
        size_t local_len = 0u;
        size_t* len_ptr = out_len != nullptr ? out_len : &local_len;
        if (ptr_ == nullptr || ops_.as_mut_slice == nullptr) {
            *len_ptr = 0u;
            return nullptr;
        }
        return ops_.as_mut_slice(ptr_, len_ptr);
    }

    BamResult set(const Elem* ptr, size_t count) noexcept {
        if (ptr_ == nullptr || ops_.set == nullptr) {
            return BAM_RESULT_NULL_POINTER;
        }
        return ops_.set(ptr_, SliceTy{ptr, count});
    }

    BamResult clear() noexcept {
        return (ptr_ != nullptr && ops_.clear != nullptr) ? ops_.clear(ptr_) : BAM_RESULT_NULL_POINTER;
    }

    BamResult push(Elem val) noexcept {
        return (ptr_ != nullptr && ops_.push != nullptr) ? ops_.push(ptr_, val) : BAM_RESULT_NULL_POINTER;
    }

    bool pop(Elem* out_elem) noexcept {
        if (ptr_ == nullptr || ops_.pop == nullptr) {
            return false;
        }
        return ops_.pop(ptr_, out_elem) != BAM_FALSE;
    }

    BamResult remove(size_t index, Elem* out_elem) noexcept {
        return (ptr_ != nullptr && ops_.remove != nullptr)
            ? ops_.remove(ptr_, index, out_elem)
            : BAM_RESULT_NULL_POINTER;
    }

    BamResult swap_remove(size_t index, Elem* out_elem) noexcept {
        return (ptr_ != nullptr && ops_.swap_remove != nullptr)
            ? ops_.swap_remove(ptr_, index, out_elem)
            : BAM_RESULT_NULL_POINTER;
    }

    BamResult truncate(size_t new_len) noexcept {
        return (ptr_ != nullptr && ops_.truncate != nullptr)
            ? ops_.truncate(ptr_, new_len)
            : BAM_RESULT_NULL_POINTER;
    }

    BamResult reserve(size_t additional) noexcept {
        return (ptr_ != nullptr && ops_.reserve != nullptr)
            ? ops_.reserve(ptr_, additional)
            : BAM_RESULT_NULL_POINTER;
    }

    const Elem* begin() const noexcept { return as_slice().ptr; }
    const Elem* end() const noexcept {
        const SliceTy s = as_slice();
        return s.ptr != nullptr ? (s.ptr + s.len) : nullptr;
    }

    Elem* begin() noexcept { return data_mut(nullptr); }
    Elem* end() noexcept {
        size_t n = 0u;
        Elem* p = data_mut(&n);
        return p != nullptr ? (p + n) : nullptr;
    }

private:
    void* ptr_{nullptr};
    Ops   ops_{};
};

template <typename Elem, typename SliceTy, typename Ops>
class ConstHostVecView {
public:
    ConstHostVecView(const void* ptr, const Ops& ops) noexcept : ptr_(ptr), ops_(ops) {}

    bool valid() const noexcept { return ptr_ != nullptr && ops_.get != nullptr; }
    explicit operator bool() const noexcept { return valid(); }

    size_t len() const noexcept {
        return (ptr_ != nullptr && ops_.len != nullptr) ? ops_.len(ptr_) : 0u;
    }
    bool empty() const noexcept { return len() == 0u; }

    SliceTy as_slice() const noexcept {
        if (ptr_ == nullptr || ops_.get == nullptr) {
            return SliceTy{nullptr, 0u};
        }
        return ops_.get(ptr_);
    }

    const Elem* data() const noexcept { return as_slice().ptr; }
    const Elem* begin() const noexcept { return as_slice().ptr; }
    const Elem* end() const noexcept {
        const SliceTy s = as_slice();
        return s.ptr != nullptr ? (s.ptr + s.len) : nullptr;
    }

private:
    const void* ptr_{nullptr};
    Ops         ops_{};
};

#endif /* BAM_CPP17_SDK_HELPERS_DEFINED */
} // namespace bam
class PositionConstView {
public:
    PositionConstView() noexcept = default;
    PositionConstView(const void* ptr, const PositionVTable* vt) noexcept : ptr_(ptr), vt_(vt) {}
    bool valid() const noexcept { return ptr_ != nullptr && vt_ != nullptr; }
    explicit operator bool() const noexcept { return valid(); }
    const void* raw() const noexcept { return ptr_; }
    const PositionVTable* vtable() const noexcept { return vt_; }
    float get_x() const noexcept {
        return (ptr_ != nullptr && vt_ != nullptr && vt_->get_x != nullptr) ? vt_->get_x(ptr_) : float{};
    }
    float get_y() const noexcept {
        return (ptr_ != nullptr && vt_ != nullptr && vt_->get_y != nullptr) ? vt_->get_y(ptr_) : float{};
    }
    float get_z() const noexcept {
        return (ptr_ != nullptr && vt_ != nullptr && vt_->get_z != nullptr) ? vt_->get_z(ptr_) : float{};
    }
private:
    const void* ptr_{nullptr};
    const PositionVTable* vt_{nullptr};
};

class PositionView {
public:
    PositionView() noexcept = default;
    PositionView(void* ptr, const PositionVTable* vt) noexcept : ptr_(ptr), vt_(vt) {}
    bool valid() const noexcept { return ptr_ != nullptr && vt_ != nullptr; }
    explicit operator bool() const noexcept { return valid(); }
    void* raw() const noexcept { return ptr_; }
    const PositionVTable* vtable() const noexcept { return vt_; }
    PositionConstView as_const() const noexcept { return PositionConstView(ptr_, vt_); }
    float get_x() const noexcept {
        return (ptr_ != nullptr && vt_ != nullptr && vt_->get_x != nullptr) ? vt_->get_x(ptr_) : float{};
    }
    BamResult set_x(float val) noexcept {
        return (ptr_ != nullptr && vt_ != nullptr && vt_->set_x != nullptr) ? vt_->set_x(ptr_, val) : BAM_RESULT_NULL_POINTER;
    }
    float get_y() const noexcept {
        return (ptr_ != nullptr && vt_ != nullptr && vt_->get_y != nullptr) ? vt_->get_y(ptr_) : float{};
    }
    BamResult set_y(float val) noexcept {
        return (ptr_ != nullptr && vt_ != nullptr && vt_->set_y != nullptr) ? vt_->set_y(ptr_, val) : BAM_RESULT_NULL_POINTER;
    }
    float get_z() const noexcept {
        return (ptr_ != nullptr && vt_ != nullptr && vt_->get_z != nullptr) ? vt_->get_z(ptr_) : float{};
    }
    BamResult set_z(float val) noexcept {
        return (ptr_ != nullptr && vt_ != nullptr && vt_->set_z != nullptr) ? vt_->set_z(ptr_, val) : BAM_RESULT_NULL_POINTER;
    }
private:
    void* ptr_{nullptr};
    const PositionVTable* vt_{nullptr};
};

} // extern "C++"
#endif /* __cplusplus */

typedef struct Velocity {
    float x;
    float y;
    float z;
} Velocity;

typedef struct VelocityVTable {
    BamVTableHeader header;
    float (*get_x)(const void* ptr);
    BamResult (*set_x)(void* ptr, float val);
    float (*get_y)(const void* ptr);
    BamResult (*set_y)(void* ptr, float val);
    float (*get_z)(const void* ptr);
    BamResult (*set_z)(void* ptr, float val);
} VelocityVTable;
BAM_STATIC_ASSERT(offsetof(VelocityVTable, header) == 0u, velocityvtable_header_off);
#if UINTPTR_MAX == UINT64_MAX
BAM_STATIC_ASSERT(sizeof(VelocityVTable) == 88u, velocityvtable_size64);
#elif UINTPTR_MAX == UINT32_MAX
BAM_STATIC_ASSERT(sizeof(VelocityVTable) == 56u, velocityvtable_size32);
#endif
#ifdef __cplusplus
extern "C++" {
namespace bam {
#ifndef BAM_CPP17_SDK_HELPERS_DEFINED
#define BAM_CPP17_SDK_HELPERS_DEFINED

template <typename VT>
inline const VT* resolve_vtable(
    const BamHostApi& api,
    const BamHostCtx* host_ctx,
    const char* symbol,
    BamResult* out_err = nullptr
) noexcept {
    if (api.get_proc_address == nullptr || host_ctx == nullptr || symbol == nullptr) {
        if (out_err != nullptr) {
            *out_err = BAM_RESULT_NULL_POINTER;
        }
        return nullptr;
    }
    const void* raw = api.get_proc_address(host_ctx, symbol);
    if (raw == nullptr) {
        if (out_err != nullptr) {
            *out_err = BAM_RESULT_NOT_FOUND;
        }
        return nullptr;
    }
    const auto* hdr = static_cast<const BamVTableHeader*>(raw);
    if (hdr->struct_size < static_cast<uint32_t>(sizeof(VT)) ||
        bam_version_is_compatible(BAM_ABI_VERSION, hdr->abi_version) == BAM_FALSE) {
        if (out_err != nullptr) {
            *out_err = BAM_RESULT_INVALID_VERSION;
        }
        return nullptr;
    }
    if (out_err != nullptr) {
        *out_err = BAM_RESULT_OK;
    }
    return static_cast<const VT*>(raw);
}

template <typename VT>
class VTableHandle {
public:
    VTableHandle() noexcept = default;
    explicit VTableHandle(const VT* ptr) noexcept : ptr_(ptr) {}

    static VTableHandle resolve(
        const BamHostApi& api,
        const BamHostCtx* host_ctx,
        const char* symbol,
        BamResult* out_err = nullptr
    ) noexcept {
        return VTableHandle(resolve_vtable<VT>(api, host_ctx, symbol, out_err));
    }

    bool valid() const noexcept { return ptr_ != nullptr; }
    explicit operator bool() const noexcept { return valid(); }
    const VT* raw() const noexcept { return ptr_; }
    const VT* operator->() const noexcept { return ptr_; }

private:
    const VT* ptr_{nullptr};
};

template <typename InstanceT, void (*DestroyFn)(InstanceT*)>
class UniqueGuest {
public:
    UniqueGuest() noexcept = default;
    explicit UniqueGuest(InstanceT* ptr) noexcept : ptr_(ptr) {}
    ~UniqueGuest() noexcept { reset(); }

    UniqueGuest(const UniqueGuest&) = delete;
    UniqueGuest& operator=(const UniqueGuest&) = delete;

    UniqueGuest(UniqueGuest&& other) noexcept : ptr_(other.release()) {}
    UniqueGuest& operator=(UniqueGuest&& other) noexcept {
        if (this != &other) {
            reset(other.release());
        }
        return *this;
    }

    InstanceT* get() const noexcept { return ptr_; }
    InstanceT* operator->() const noexcept { return ptr_; }
    InstanceT& operator*() const noexcept { return *ptr_; }
    explicit operator bool() const noexcept { return ptr_ != nullptr; }

    InstanceT* release() noexcept {
        InstanceT* tmp = ptr_;
        ptr_ = nullptr;
        return tmp;
    }

    void reset(InstanceT* next = nullptr) noexcept {
        if constexpr (DestroyFn != nullptr) {
            if (ptr_ != nullptr) {
                DestroyFn(ptr_);
            }
        }
        ptr_ = next;
    }

private:
    InstanceT* ptr_{nullptr};
};

template <typename Elem, typename SliceTy, typename Ops>
class HostVecView {
public:
    HostVecView(void* ptr, const Ops& ops) noexcept : ptr_(ptr), ops_(ops) {}

    bool valid() const noexcept { return ptr_ != nullptr && ops_.valid(); }
    explicit operator bool() const noexcept { return valid(); }

    size_t len() const noexcept {
        return (ptr_ != nullptr && ops_.len != nullptr) ? ops_.len(ptr_) : 0u;
    }
    bool empty() const noexcept { return len() == 0u; }

    SliceTy as_slice() const noexcept {
        if (ptr_ == nullptr || ops_.get == nullptr) {
            return SliceTy{nullptr, 0u};
        }
        return ops_.get(ptr_);
    }

    const Elem* data() const noexcept { return as_slice().ptr; }

    Elem* data_mut(size_t* out_len = nullptr) noexcept {
        size_t local_len = 0u;
        size_t* len_ptr = out_len != nullptr ? out_len : &local_len;
        if (ptr_ == nullptr || ops_.as_mut_slice == nullptr) {
            *len_ptr = 0u;
            return nullptr;
        }
        return ops_.as_mut_slice(ptr_, len_ptr);
    }

    BamResult set(const Elem* ptr, size_t count) noexcept {
        if (ptr_ == nullptr || ops_.set == nullptr) {
            return BAM_RESULT_NULL_POINTER;
        }
        return ops_.set(ptr_, SliceTy{ptr, count});
    }

    BamResult clear() noexcept {
        return (ptr_ != nullptr && ops_.clear != nullptr) ? ops_.clear(ptr_) : BAM_RESULT_NULL_POINTER;
    }

    BamResult push(Elem val) noexcept {
        return (ptr_ != nullptr && ops_.push != nullptr) ? ops_.push(ptr_, val) : BAM_RESULT_NULL_POINTER;
    }

    bool pop(Elem* out_elem) noexcept {
        if (ptr_ == nullptr || ops_.pop == nullptr) {
            return false;
        }
        return ops_.pop(ptr_, out_elem) != BAM_FALSE;
    }

    BamResult remove(size_t index, Elem* out_elem) noexcept {
        return (ptr_ != nullptr && ops_.remove != nullptr)
            ? ops_.remove(ptr_, index, out_elem)
            : BAM_RESULT_NULL_POINTER;
    }

    BamResult swap_remove(size_t index, Elem* out_elem) noexcept {
        return (ptr_ != nullptr && ops_.swap_remove != nullptr)
            ? ops_.swap_remove(ptr_, index, out_elem)
            : BAM_RESULT_NULL_POINTER;
    }

    BamResult truncate(size_t new_len) noexcept {
        return (ptr_ != nullptr && ops_.truncate != nullptr)
            ? ops_.truncate(ptr_, new_len)
            : BAM_RESULT_NULL_POINTER;
    }

    BamResult reserve(size_t additional) noexcept {
        return (ptr_ != nullptr && ops_.reserve != nullptr)
            ? ops_.reserve(ptr_, additional)
            : BAM_RESULT_NULL_POINTER;
    }

    const Elem* begin() const noexcept { return as_slice().ptr; }
    const Elem* end() const noexcept {
        const SliceTy s = as_slice();
        return s.ptr != nullptr ? (s.ptr + s.len) : nullptr;
    }

    Elem* begin() noexcept { return data_mut(nullptr); }
    Elem* end() noexcept {
        size_t n = 0u;
        Elem* p = data_mut(&n);
        return p != nullptr ? (p + n) : nullptr;
    }

private:
    void* ptr_{nullptr};
    Ops   ops_{};
};

template <typename Elem, typename SliceTy, typename Ops>
class ConstHostVecView {
public:
    ConstHostVecView(const void* ptr, const Ops& ops) noexcept : ptr_(ptr), ops_(ops) {}

    bool valid() const noexcept { return ptr_ != nullptr && ops_.get != nullptr; }
    explicit operator bool() const noexcept { return valid(); }

    size_t len() const noexcept {
        return (ptr_ != nullptr && ops_.len != nullptr) ? ops_.len(ptr_) : 0u;
    }
    bool empty() const noexcept { return len() == 0u; }

    SliceTy as_slice() const noexcept {
        if (ptr_ == nullptr || ops_.get == nullptr) {
            return SliceTy{nullptr, 0u};
        }
        return ops_.get(ptr_);
    }

    const Elem* data() const noexcept { return as_slice().ptr; }
    const Elem* begin() const noexcept { return as_slice().ptr; }
    const Elem* end() const noexcept {
        const SliceTy s = as_slice();
        return s.ptr != nullptr ? (s.ptr + s.len) : nullptr;
    }

private:
    const void* ptr_{nullptr};
    Ops         ops_{};
};

#endif /* BAM_CPP17_SDK_HELPERS_DEFINED */
} // namespace bam
class VelocityConstView {
public:
    VelocityConstView() noexcept = default;
    VelocityConstView(const void* ptr, const VelocityVTable* vt) noexcept : ptr_(ptr), vt_(vt) {}
    bool valid() const noexcept { return ptr_ != nullptr && vt_ != nullptr; }
    explicit operator bool() const noexcept { return valid(); }
    const void* raw() const noexcept { return ptr_; }
    const VelocityVTable* vtable() const noexcept { return vt_; }
    float get_x() const noexcept {
        return (ptr_ != nullptr && vt_ != nullptr && vt_->get_x != nullptr) ? vt_->get_x(ptr_) : float{};
    }
    float get_y() const noexcept {
        return (ptr_ != nullptr && vt_ != nullptr && vt_->get_y != nullptr) ? vt_->get_y(ptr_) : float{};
    }
    float get_z() const noexcept {
        return (ptr_ != nullptr && vt_ != nullptr && vt_->get_z != nullptr) ? vt_->get_z(ptr_) : float{};
    }
private:
    const void* ptr_{nullptr};
    const VelocityVTable* vt_{nullptr};
};

class VelocityView {
public:
    VelocityView() noexcept = default;
    VelocityView(void* ptr, const VelocityVTable* vt) noexcept : ptr_(ptr), vt_(vt) {}
    bool valid() const noexcept { return ptr_ != nullptr && vt_ != nullptr; }
    explicit operator bool() const noexcept { return valid(); }
    void* raw() const noexcept { return ptr_; }
    const VelocityVTable* vtable() const noexcept { return vt_; }
    VelocityConstView as_const() const noexcept { return VelocityConstView(ptr_, vt_); }
    float get_x() const noexcept {
        return (ptr_ != nullptr && vt_ != nullptr && vt_->get_x != nullptr) ? vt_->get_x(ptr_) : float{};
    }
    BamResult set_x(float val) noexcept {
        return (ptr_ != nullptr && vt_ != nullptr && vt_->set_x != nullptr) ? vt_->set_x(ptr_, val) : BAM_RESULT_NULL_POINTER;
    }
    float get_y() const noexcept {
        return (ptr_ != nullptr && vt_ != nullptr && vt_->get_y != nullptr) ? vt_->get_y(ptr_) : float{};
    }
    BamResult set_y(float val) noexcept {
        return (ptr_ != nullptr && vt_ != nullptr && vt_->set_y != nullptr) ? vt_->set_y(ptr_, val) : BAM_RESULT_NULL_POINTER;
    }
    float get_z() const noexcept {
        return (ptr_ != nullptr && vt_ != nullptr && vt_->get_z != nullptr) ? vt_->get_z(ptr_) : float{};
    }
    BamResult set_z(float val) noexcept {
        return (ptr_ != nullptr && vt_ != nullptr && vt_->set_z != nullptr) ? vt_->set_z(ptr_, val) : BAM_RESULT_NULL_POINTER;
    }
private:
    void* ptr_{nullptr};
    const VelocityVTable* vt_{nullptr};
};

} // extern "C++"
#endif /* __cplusplus */

typedef struct SimConfig {
    float dt;
    float damping;
    BamBool trigger_exception;
    uint8_t pad[3];
} SimConfig;

typedef struct SimConfigVTable {
    BamVTableHeader header;
    float (*get_dt)(const void* ptr);
    BamResult (*set_dt)(void* ptr, float val);
    float (*get_damping)(const void* ptr);
    BamResult (*set_damping)(void* ptr, float val);
    BamBool (*get_trigger_exception)(const void* ptr);
    BamResult (*set_trigger_exception)(void* ptr, BamBool val);
    const void* (*ptr_pad)(const void* ptr);
    void*       (*ptr_mut_pad)(void* ptr);
} SimConfigVTable;
BAM_STATIC_ASSERT(offsetof(SimConfigVTable, header) == 0u, simconfigvtable_header_off);
#if UINTPTR_MAX == UINT64_MAX
BAM_STATIC_ASSERT(sizeof(SimConfigVTable) == 104u, simconfigvtable_size64);
#elif UINTPTR_MAX == UINT32_MAX
BAM_STATIC_ASSERT(sizeof(SimConfigVTable) == 64u, simconfigvtable_size32);
#endif
#ifdef __cplusplus
extern "C++" {
namespace bam {
#ifndef BAM_CPP17_SDK_HELPERS_DEFINED
#define BAM_CPP17_SDK_HELPERS_DEFINED

template <typename VT>
inline const VT* resolve_vtable(
    const BamHostApi& api,
    const BamHostCtx* host_ctx,
    const char* symbol,
    BamResult* out_err = nullptr
) noexcept {
    if (api.get_proc_address == nullptr || host_ctx == nullptr || symbol == nullptr) {
        if (out_err != nullptr) {
            *out_err = BAM_RESULT_NULL_POINTER;
        }
        return nullptr;
    }
    const void* raw = api.get_proc_address(host_ctx, symbol);
    if (raw == nullptr) {
        if (out_err != nullptr) {
            *out_err = BAM_RESULT_NOT_FOUND;
        }
        return nullptr;
    }
    const auto* hdr = static_cast<const BamVTableHeader*>(raw);
    if (hdr->struct_size < static_cast<uint32_t>(sizeof(VT)) ||
        bam_version_is_compatible(BAM_ABI_VERSION, hdr->abi_version) == BAM_FALSE) {
        if (out_err != nullptr) {
            *out_err = BAM_RESULT_INVALID_VERSION;
        }
        return nullptr;
    }
    if (out_err != nullptr) {
        *out_err = BAM_RESULT_OK;
    }
    return static_cast<const VT*>(raw);
}

template <typename VT>
class VTableHandle {
public:
    VTableHandle() noexcept = default;
    explicit VTableHandle(const VT* ptr) noexcept : ptr_(ptr) {}

    static VTableHandle resolve(
        const BamHostApi& api,
        const BamHostCtx* host_ctx,
        const char* symbol,
        BamResult* out_err = nullptr
    ) noexcept {
        return VTableHandle(resolve_vtable<VT>(api, host_ctx, symbol, out_err));
    }

    bool valid() const noexcept { return ptr_ != nullptr; }
    explicit operator bool() const noexcept { return valid(); }
    const VT* raw() const noexcept { return ptr_; }
    const VT* operator->() const noexcept { return ptr_; }

private:
    const VT* ptr_{nullptr};
};

template <typename InstanceT, void (*DestroyFn)(InstanceT*)>
class UniqueGuest {
public:
    UniqueGuest() noexcept = default;
    explicit UniqueGuest(InstanceT* ptr) noexcept : ptr_(ptr) {}
    ~UniqueGuest() noexcept { reset(); }

    UniqueGuest(const UniqueGuest&) = delete;
    UniqueGuest& operator=(const UniqueGuest&) = delete;

    UniqueGuest(UniqueGuest&& other) noexcept : ptr_(other.release()) {}
    UniqueGuest& operator=(UniqueGuest&& other) noexcept {
        if (this != &other) {
            reset(other.release());
        }
        return *this;
    }

    InstanceT* get() const noexcept { return ptr_; }
    InstanceT* operator->() const noexcept { return ptr_; }
    InstanceT& operator*() const noexcept { return *ptr_; }
    explicit operator bool() const noexcept { return ptr_ != nullptr; }

    InstanceT* release() noexcept {
        InstanceT* tmp = ptr_;
        ptr_ = nullptr;
        return tmp;
    }

    void reset(InstanceT* next = nullptr) noexcept {
        if constexpr (DestroyFn != nullptr) {
            if (ptr_ != nullptr) {
                DestroyFn(ptr_);
            }
        }
        ptr_ = next;
    }

private:
    InstanceT* ptr_{nullptr};
};

template <typename Elem, typename SliceTy, typename Ops>
class HostVecView {
public:
    HostVecView(void* ptr, const Ops& ops) noexcept : ptr_(ptr), ops_(ops) {}

    bool valid() const noexcept { return ptr_ != nullptr && ops_.valid(); }
    explicit operator bool() const noexcept { return valid(); }

    size_t len() const noexcept {
        return (ptr_ != nullptr && ops_.len != nullptr) ? ops_.len(ptr_) : 0u;
    }
    bool empty() const noexcept { return len() == 0u; }

    SliceTy as_slice() const noexcept {
        if (ptr_ == nullptr || ops_.get == nullptr) {
            return SliceTy{nullptr, 0u};
        }
        return ops_.get(ptr_);
    }

    const Elem* data() const noexcept { return as_slice().ptr; }

    Elem* data_mut(size_t* out_len = nullptr) noexcept {
        size_t local_len = 0u;
        size_t* len_ptr = out_len != nullptr ? out_len : &local_len;
        if (ptr_ == nullptr || ops_.as_mut_slice == nullptr) {
            *len_ptr = 0u;
            return nullptr;
        }
        return ops_.as_mut_slice(ptr_, len_ptr);
    }

    BamResult set(const Elem* ptr, size_t count) noexcept {
        if (ptr_ == nullptr || ops_.set == nullptr) {
            return BAM_RESULT_NULL_POINTER;
        }
        return ops_.set(ptr_, SliceTy{ptr, count});
    }

    BamResult clear() noexcept {
        return (ptr_ != nullptr && ops_.clear != nullptr) ? ops_.clear(ptr_) : BAM_RESULT_NULL_POINTER;
    }

    BamResult push(Elem val) noexcept {
        return (ptr_ != nullptr && ops_.push != nullptr) ? ops_.push(ptr_, val) : BAM_RESULT_NULL_POINTER;
    }

    bool pop(Elem* out_elem) noexcept {
        if (ptr_ == nullptr || ops_.pop == nullptr) {
            return false;
        }
        return ops_.pop(ptr_, out_elem) != BAM_FALSE;
    }

    BamResult remove(size_t index, Elem* out_elem) noexcept {
        return (ptr_ != nullptr && ops_.remove != nullptr)
            ? ops_.remove(ptr_, index, out_elem)
            : BAM_RESULT_NULL_POINTER;
    }

    BamResult swap_remove(size_t index, Elem* out_elem) noexcept {
        return (ptr_ != nullptr && ops_.swap_remove != nullptr)
            ? ops_.swap_remove(ptr_, index, out_elem)
            : BAM_RESULT_NULL_POINTER;
    }

    BamResult truncate(size_t new_len) noexcept {
        return (ptr_ != nullptr && ops_.truncate != nullptr)
            ? ops_.truncate(ptr_, new_len)
            : BAM_RESULT_NULL_POINTER;
    }

    BamResult reserve(size_t additional) noexcept {
        return (ptr_ != nullptr && ops_.reserve != nullptr)
            ? ops_.reserve(ptr_, additional)
            : BAM_RESULT_NULL_POINTER;
    }

    const Elem* begin() const noexcept { return as_slice().ptr; }
    const Elem* end() const noexcept {
        const SliceTy s = as_slice();
        return s.ptr != nullptr ? (s.ptr + s.len) : nullptr;
    }

    Elem* begin() noexcept { return data_mut(nullptr); }
    Elem* end() noexcept {
        size_t n = 0u;
        Elem* p = data_mut(&n);
        return p != nullptr ? (p + n) : nullptr;
    }

private:
    void* ptr_{nullptr};
    Ops   ops_{};
};

template <typename Elem, typename SliceTy, typename Ops>
class ConstHostVecView {
public:
    ConstHostVecView(const void* ptr, const Ops& ops) noexcept : ptr_(ptr), ops_(ops) {}

    bool valid() const noexcept { return ptr_ != nullptr && ops_.get != nullptr; }
    explicit operator bool() const noexcept { return valid(); }

    size_t len() const noexcept {
        return (ptr_ != nullptr && ops_.len != nullptr) ? ops_.len(ptr_) : 0u;
    }
    bool empty() const noexcept { return len() == 0u; }

    SliceTy as_slice() const noexcept {
        if (ptr_ == nullptr || ops_.get == nullptr) {
            return SliceTy{nullptr, 0u};
        }
        return ops_.get(ptr_);
    }

    const Elem* data() const noexcept { return as_slice().ptr; }
    const Elem* begin() const noexcept { return as_slice().ptr; }
    const Elem* end() const noexcept {
        const SliceTy s = as_slice();
        return s.ptr != nullptr ? (s.ptr + s.len) : nullptr;
    }

private:
    const void* ptr_{nullptr};
    Ops         ops_{};
};

#endif /* BAM_CPP17_SDK_HELPERS_DEFINED */
} // namespace bam
class SimConfigConstView {
public:
    SimConfigConstView() noexcept = default;
    SimConfigConstView(const void* ptr, const SimConfigVTable* vt) noexcept : ptr_(ptr), vt_(vt) {}
    bool valid() const noexcept { return ptr_ != nullptr && vt_ != nullptr; }
    explicit operator bool() const noexcept { return valid(); }
    const void* raw() const noexcept { return ptr_; }
    const SimConfigVTable* vtable() const noexcept { return vt_; }
    float get_dt() const noexcept {
        return (ptr_ != nullptr && vt_ != nullptr && vt_->get_dt != nullptr) ? vt_->get_dt(ptr_) : float{};
    }
    float get_damping() const noexcept {
        return (ptr_ != nullptr && vt_ != nullptr && vt_->get_damping != nullptr) ? vt_->get_damping(ptr_) : float{};
    }
    BamBool get_trigger_exception() const noexcept {
        return (ptr_ != nullptr && vt_ != nullptr && vt_->get_trigger_exception != nullptr) ? vt_->get_trigger_exception(ptr_) : BamBool{};
    }
    const void* ptr_pad() const noexcept {
        return (ptr_ != nullptr && vt_ != nullptr && vt_->ptr_pad != nullptr) ? vt_->ptr_pad(ptr_) : nullptr;
    }
private:
    const void* ptr_{nullptr};
    const SimConfigVTable* vt_{nullptr};
};

class SimConfigView {
public:
    SimConfigView() noexcept = default;
    SimConfigView(void* ptr, const SimConfigVTable* vt) noexcept : ptr_(ptr), vt_(vt) {}
    bool valid() const noexcept { return ptr_ != nullptr && vt_ != nullptr; }
    explicit operator bool() const noexcept { return valid(); }
    void* raw() const noexcept { return ptr_; }
    const SimConfigVTable* vtable() const noexcept { return vt_; }
    SimConfigConstView as_const() const noexcept { return SimConfigConstView(ptr_, vt_); }
    float get_dt() const noexcept {
        return (ptr_ != nullptr && vt_ != nullptr && vt_->get_dt != nullptr) ? vt_->get_dt(ptr_) : float{};
    }
    BamResult set_dt(float val) noexcept {
        return (ptr_ != nullptr && vt_ != nullptr && vt_->set_dt != nullptr) ? vt_->set_dt(ptr_, val) : BAM_RESULT_NULL_POINTER;
    }
    float get_damping() const noexcept {
        return (ptr_ != nullptr && vt_ != nullptr && vt_->get_damping != nullptr) ? vt_->get_damping(ptr_) : float{};
    }
    BamResult set_damping(float val) noexcept {
        return (ptr_ != nullptr && vt_ != nullptr && vt_->set_damping != nullptr) ? vt_->set_damping(ptr_, val) : BAM_RESULT_NULL_POINTER;
    }
    BamBool get_trigger_exception() const noexcept {
        return (ptr_ != nullptr && vt_ != nullptr && vt_->get_trigger_exception != nullptr) ? vt_->get_trigger_exception(ptr_) : BamBool{};
    }
    BamResult set_trigger_exception(BamBool val) noexcept {
        return (ptr_ != nullptr && vt_ != nullptr && vt_->set_trigger_exception != nullptr) ? vt_->set_trigger_exception(ptr_, val) : BAM_RESULT_NULL_POINTER;
    }
    const void* ptr_pad() const noexcept {
        return (ptr_ != nullptr && vt_ != nullptr && vt_->ptr_pad != nullptr) ? vt_->ptr_pad(ptr_) : nullptr;
    }
    void* ptr_mut_pad() noexcept {
        return (ptr_ != nullptr && vt_ != nullptr && vt_->ptr_mut_pad != nullptr) ? vt_->ptr_mut_pad(ptr_) : nullptr;
    }
private:
    void* ptr_{nullptr};
    const SimConfigVTable* vt_{nullptr};
};

} // extern "C++"
#endif /* __cplusplus */

typedef struct ActorStats_bonus_Option {
    BamBool is_some;
    uint32_t value;
} ActorStats_bonus_Option;

typedef struct ActorStats ActorStats;

typedef struct ActorStatsVTable_scores_Slice {
    const uint32_t* ptr;
    size_t len;
} ActorStatsVTable_scores_Slice;

typedef struct ActorStatsVTable {
    BamVTableHeader header;
    BamBool   (*get_active)(const void* ptr);
    BamResult (*set_active)(void* ptr, BamBool val);
    BamStrView (*get_tag)(const void* ptr);
    BamResult  (*set_tag)(void* ptr, BamStrView view);
    ActorStatsVTable_scores_Slice (*get_scores)(const void* ptr);
    BamResult (*set_scores)(void* ptr, ActorStatsVTable_scores_Slice slice);
    size_t (*len_scores)(const void* ptr);
    BamResult (*clear_scores)(void* ptr);
    BamResult (*push_scores)(void* ptr, uint32_t val);
    BamBool (*pop_scores)(void* ptr, uint32_t* out_elem);
    BamResult (*remove_scores)(void* ptr, size_t index, uint32_t* out_elem);
    BamResult (*swap_remove_scores)(void* ptr, size_t index, uint32_t* out_elem);
    BamResult (*truncate_scores)(void* ptr, size_t new_len);
    BamResult (*reserve_scores)(void* ptr, size_t additional);
    uint32_t* (*as_mut_slice_scores)(void* ptr, size_t* out_len);
    ActorStats_bonus_Option (*get_bonus)(const void* ptr);
    BamResult (*set_bonus)(void* ptr, ActorStats_bonus_Option val);
    BamResult (*clear_bonus)(void* ptr);
} ActorStatsVTable;
BAM_STATIC_ASSERT(offsetof(ActorStatsVTable, header) == 0u, actorstatsvtable_header_off);
#if UINTPTR_MAX == UINT64_MAX
BAM_STATIC_ASSERT(sizeof(ActorStatsVTable) == 184u, actorstatsvtable_size64);
#elif UINTPTR_MAX == UINT32_MAX
BAM_STATIC_ASSERT(sizeof(ActorStatsVTable) == 104u, actorstatsvtable_size32);
#endif
#ifdef __cplusplus
extern "C++" {
namespace bam {
#ifndef BAM_CPP17_SDK_HELPERS_DEFINED
#define BAM_CPP17_SDK_HELPERS_DEFINED

template <typename VT>
inline const VT* resolve_vtable(
    const BamHostApi& api,
    const BamHostCtx* host_ctx,
    const char* symbol,
    BamResult* out_err = nullptr
) noexcept {
    if (api.get_proc_address == nullptr || host_ctx == nullptr || symbol == nullptr) {
        if (out_err != nullptr) {
            *out_err = BAM_RESULT_NULL_POINTER;
        }
        return nullptr;
    }
    const void* raw = api.get_proc_address(host_ctx, symbol);
    if (raw == nullptr) {
        if (out_err != nullptr) {
            *out_err = BAM_RESULT_NOT_FOUND;
        }
        return nullptr;
    }
    const auto* hdr = static_cast<const BamVTableHeader*>(raw);
    if (hdr->struct_size < static_cast<uint32_t>(sizeof(VT)) ||
        bam_version_is_compatible(BAM_ABI_VERSION, hdr->abi_version) == BAM_FALSE) {
        if (out_err != nullptr) {
            *out_err = BAM_RESULT_INVALID_VERSION;
        }
        return nullptr;
    }
    if (out_err != nullptr) {
        *out_err = BAM_RESULT_OK;
    }
    return static_cast<const VT*>(raw);
}

template <typename VT>
class VTableHandle {
public:
    VTableHandle() noexcept = default;
    explicit VTableHandle(const VT* ptr) noexcept : ptr_(ptr) {}

    static VTableHandle resolve(
        const BamHostApi& api,
        const BamHostCtx* host_ctx,
        const char* symbol,
        BamResult* out_err = nullptr
    ) noexcept {
        return VTableHandle(resolve_vtable<VT>(api, host_ctx, symbol, out_err));
    }

    bool valid() const noexcept { return ptr_ != nullptr; }
    explicit operator bool() const noexcept { return valid(); }
    const VT* raw() const noexcept { return ptr_; }
    const VT* operator->() const noexcept { return ptr_; }

private:
    const VT* ptr_{nullptr};
};

template <typename InstanceT, void (*DestroyFn)(InstanceT*)>
class UniqueGuest {
public:
    UniqueGuest() noexcept = default;
    explicit UniqueGuest(InstanceT* ptr) noexcept : ptr_(ptr) {}
    ~UniqueGuest() noexcept { reset(); }

    UniqueGuest(const UniqueGuest&) = delete;
    UniqueGuest& operator=(const UniqueGuest&) = delete;

    UniqueGuest(UniqueGuest&& other) noexcept : ptr_(other.release()) {}
    UniqueGuest& operator=(UniqueGuest&& other) noexcept {
        if (this != &other) {
            reset(other.release());
        }
        return *this;
    }

    InstanceT* get() const noexcept { return ptr_; }
    InstanceT* operator->() const noexcept { return ptr_; }
    InstanceT& operator*() const noexcept { return *ptr_; }
    explicit operator bool() const noexcept { return ptr_ != nullptr; }

    InstanceT* release() noexcept {
        InstanceT* tmp = ptr_;
        ptr_ = nullptr;
        return tmp;
    }

    void reset(InstanceT* next = nullptr) noexcept {
        if constexpr (DestroyFn != nullptr) {
            if (ptr_ != nullptr) {
                DestroyFn(ptr_);
            }
        }
        ptr_ = next;
    }

private:
    InstanceT* ptr_{nullptr};
};

template <typename Elem, typename SliceTy, typename Ops>
class HostVecView {
public:
    HostVecView(void* ptr, const Ops& ops) noexcept : ptr_(ptr), ops_(ops) {}

    bool valid() const noexcept { return ptr_ != nullptr && ops_.valid(); }
    explicit operator bool() const noexcept { return valid(); }

    size_t len() const noexcept {
        return (ptr_ != nullptr && ops_.len != nullptr) ? ops_.len(ptr_) : 0u;
    }
    bool empty() const noexcept { return len() == 0u; }

    SliceTy as_slice() const noexcept {
        if (ptr_ == nullptr || ops_.get == nullptr) {
            return SliceTy{nullptr, 0u};
        }
        return ops_.get(ptr_);
    }

    const Elem* data() const noexcept { return as_slice().ptr; }

    Elem* data_mut(size_t* out_len = nullptr) noexcept {
        size_t local_len = 0u;
        size_t* len_ptr = out_len != nullptr ? out_len : &local_len;
        if (ptr_ == nullptr || ops_.as_mut_slice == nullptr) {
            *len_ptr = 0u;
            return nullptr;
        }
        return ops_.as_mut_slice(ptr_, len_ptr);
    }

    BamResult set(const Elem* ptr, size_t count) noexcept {
        if (ptr_ == nullptr || ops_.set == nullptr) {
            return BAM_RESULT_NULL_POINTER;
        }
        return ops_.set(ptr_, SliceTy{ptr, count});
    }

    BamResult clear() noexcept {
        return (ptr_ != nullptr && ops_.clear != nullptr) ? ops_.clear(ptr_) : BAM_RESULT_NULL_POINTER;
    }

    BamResult push(Elem val) noexcept {
        return (ptr_ != nullptr && ops_.push != nullptr) ? ops_.push(ptr_, val) : BAM_RESULT_NULL_POINTER;
    }

    bool pop(Elem* out_elem) noexcept {
        if (ptr_ == nullptr || ops_.pop == nullptr) {
            return false;
        }
        return ops_.pop(ptr_, out_elem) != BAM_FALSE;
    }

    BamResult remove(size_t index, Elem* out_elem) noexcept {
        return (ptr_ != nullptr && ops_.remove != nullptr)
            ? ops_.remove(ptr_, index, out_elem)
            : BAM_RESULT_NULL_POINTER;
    }

    BamResult swap_remove(size_t index, Elem* out_elem) noexcept {
        return (ptr_ != nullptr && ops_.swap_remove != nullptr)
            ? ops_.swap_remove(ptr_, index, out_elem)
            : BAM_RESULT_NULL_POINTER;
    }

    BamResult truncate(size_t new_len) noexcept {
        return (ptr_ != nullptr && ops_.truncate != nullptr)
            ? ops_.truncate(ptr_, new_len)
            : BAM_RESULT_NULL_POINTER;
    }

    BamResult reserve(size_t additional) noexcept {
        return (ptr_ != nullptr && ops_.reserve != nullptr)
            ? ops_.reserve(ptr_, additional)
            : BAM_RESULT_NULL_POINTER;
    }

    const Elem* begin() const noexcept { return as_slice().ptr; }
    const Elem* end() const noexcept {
        const SliceTy s = as_slice();
        return s.ptr != nullptr ? (s.ptr + s.len) : nullptr;
    }

    Elem* begin() noexcept { return data_mut(nullptr); }
    Elem* end() noexcept {
        size_t n = 0u;
        Elem* p = data_mut(&n);
        return p != nullptr ? (p + n) : nullptr;
    }

private:
    void* ptr_{nullptr};
    Ops   ops_{};
};

template <typename Elem, typename SliceTy, typename Ops>
class ConstHostVecView {
public:
    ConstHostVecView(const void* ptr, const Ops& ops) noexcept : ptr_(ptr), ops_(ops) {}

    bool valid() const noexcept { return ptr_ != nullptr && ops_.get != nullptr; }
    explicit operator bool() const noexcept { return valid(); }

    size_t len() const noexcept {
        return (ptr_ != nullptr && ops_.len != nullptr) ? ops_.len(ptr_) : 0u;
    }
    bool empty() const noexcept { return len() == 0u; }

    SliceTy as_slice() const noexcept {
        if (ptr_ == nullptr || ops_.get == nullptr) {
            return SliceTy{nullptr, 0u};
        }
        return ops_.get(ptr_);
    }

    const Elem* data() const noexcept { return as_slice().ptr; }
    const Elem* begin() const noexcept { return as_slice().ptr; }
    const Elem* end() const noexcept {
        const SliceTy s = as_slice();
        return s.ptr != nullptr ? (s.ptr + s.len) : nullptr;
    }

private:
    const void* ptr_{nullptr};
    Ops         ops_{};
};

#endif /* BAM_CPP17_SDK_HELPERS_DEFINED */
} // namespace bam
struct ActorStatsVTable_scores_ConstVecOps {
    ActorStatsVTable_scores_Slice (*get)(const void*) = nullptr;
    size_t (*len)(const void*) = nullptr;
    static ActorStatsVTable_scores_ConstVecOps from_vtable(const ActorStatsVTable* vt) noexcept {
        if (vt == nullptr) return ActorStatsVTable_scores_ConstVecOps{};
        return ActorStatsVTable_scores_ConstVecOps{vt->get_scores, vt->len_scores};
    }
};
struct ActorStatsVTable_scores_VecOps {
    ActorStatsVTable_scores_Slice (*get)(const void*) = nullptr;
    BamResult (*set)(void*, ActorStatsVTable_scores_Slice) = nullptr;
    size_t (*len)(const void*) = nullptr;
    BamResult (*clear)(void*) = nullptr;
    BamResult (*push)(void*, uint32_t) = nullptr;
    BamBool (*pop)(void*, uint32_t*) = nullptr;
    BamResult (*remove)(void*, size_t, uint32_t*) = nullptr;
    BamResult (*swap_remove)(void*, size_t, uint32_t*) = nullptr;
    BamResult (*truncate)(void*, size_t) = nullptr;
    BamResult (*reserve)(void*, size_t) = nullptr;
    uint32_t* (*as_mut_slice)(void*, size_t*) = nullptr;
    bool valid() const noexcept {
        return get != nullptr && set != nullptr && len != nullptr && clear != nullptr &&
               push != nullptr && pop != nullptr && remove != nullptr && swap_remove != nullptr &&
               truncate != nullptr && reserve != nullptr && as_mut_slice != nullptr;
    }
    static ActorStatsVTable_scores_VecOps from_vtable(const ActorStatsVTable* vt) noexcept {
        if (vt == nullptr) return ActorStatsVTable_scores_VecOps{};
        return ActorStatsVTable_scores_VecOps{
            vt->get_scores, vt->set_scores, vt->len_scores, vt->clear_scores,
            vt->push_scores, vt->pop_scores, vt->remove_scores, vt->swap_remove_scores,
            vt->truncate_scores, vt->reserve_scores, vt->as_mut_slice_scores
        };
    }
};
using ActorStatsVTable_scores_ConstVecView = bam::ConstHostVecView<uint32_t, ActorStatsVTable_scores_Slice, ActorStatsVTable_scores_ConstVecOps>;
using ActorStatsVTable_scores_VecView = bam::HostVecView<uint32_t, ActorStatsVTable_scores_Slice, ActorStatsVTable_scores_VecOps>;

class ActorStatsConstView {
public:
    ActorStatsConstView() noexcept = default;
    ActorStatsConstView(const void* ptr, const ActorStatsVTable* vt) noexcept : ptr_(ptr), vt_(vt) {}
    bool valid() const noexcept { return ptr_ != nullptr && vt_ != nullptr; }
    explicit operator bool() const noexcept { return valid(); }
    const void* raw() const noexcept { return ptr_; }
    const ActorStatsVTable* vtable() const noexcept { return vt_; }
    bool get_active() const noexcept {
        return (ptr_ != nullptr && vt_ != nullptr && vt_->get_active != nullptr) ? (vt_->get_active(ptr_) != BAM_FALSE) : false;
    }
    BamStrView get_tag() const noexcept {
        return (ptr_ != nullptr && vt_ != nullptr && vt_->get_tag != nullptr) ? vt_->get_tag(ptr_) : BamStrView{nullptr, 0u};
    }
    ActorStatsVTable_scores_ConstVecView scores() const noexcept {
        return ActorStatsVTable_scores_ConstVecView(ptr_, ActorStatsVTable_scores_ConstVecOps::from_vtable(vt_));
    }
    ActorStats_bonus_Option get_bonus() const noexcept {
        return (ptr_ != nullptr && vt_ != nullptr && vt_->get_bonus != nullptr) ? vt_->get_bonus(ptr_) : ActorStats_bonus_Option{BAM_FALSE, {}};
    }
private:
    const void* ptr_{nullptr};
    const ActorStatsVTable* vt_{nullptr};
};

class ActorStatsView {
public:
    ActorStatsView() noexcept = default;
    ActorStatsView(void* ptr, const ActorStatsVTable* vt) noexcept : ptr_(ptr), vt_(vt) {}
    bool valid() const noexcept { return ptr_ != nullptr && vt_ != nullptr; }
    explicit operator bool() const noexcept { return valid(); }
    void* raw() const noexcept { return ptr_; }
    const ActorStatsVTable* vtable() const noexcept { return vt_; }
    ActorStatsConstView as_const() const noexcept { return ActorStatsConstView(ptr_, vt_); }
    bool get_active() const noexcept {
        return (ptr_ != nullptr && vt_ != nullptr && vt_->get_active != nullptr) ? (vt_->get_active(ptr_) != BAM_FALSE) : false;
    }
    BamResult set_active(bool val) noexcept {
        return (ptr_ != nullptr && vt_ != nullptr && vt_->set_active != nullptr) ? vt_->set_active(ptr_, val ? BAM_TRUE : BAM_FALSE) : BAM_RESULT_NULL_POINTER;
    }
    BamStrView get_tag() const noexcept {
        return (ptr_ != nullptr && vt_ != nullptr && vt_->get_tag != nullptr) ? vt_->get_tag(ptr_) : BamStrView{nullptr, 0u};
    }
    BamResult set_tag(BamStrView view) noexcept {
        return (ptr_ != nullptr && vt_ != nullptr && vt_->set_tag != nullptr) ? vt_->set_tag(ptr_, view) : BAM_RESULT_NULL_POINTER;
    }
    BamResult set_tag(const char* cstr) noexcept {
        const BamStrView view{reinterpret_cast<const uint8_t*>(cstr), cstr != nullptr ? strlen(cstr) : 0u};
        return set_tag(view);
    }
    ActorStatsVTable_scores_VecView scores() noexcept {
        return ActorStatsVTable_scores_VecView(ptr_, ActorStatsVTable_scores_VecOps::from_vtable(vt_));
    }
    ActorStatsVTable_scores_ConstVecView scores() const noexcept {
        return ActorStatsVTable_scores_ConstVecView(ptr_, ActorStatsVTable_scores_ConstVecOps::from_vtable(vt_));
    }
    ActorStats_bonus_Option get_bonus() const noexcept {
        return (ptr_ != nullptr && vt_ != nullptr && vt_->get_bonus != nullptr) ? vt_->get_bonus(ptr_) : ActorStats_bonus_Option{BAM_FALSE, {}};
    }
    BamResult set_bonus(ActorStats_bonus_Option val) noexcept {
        return (ptr_ != nullptr && vt_ != nullptr && vt_->set_bonus != nullptr) ? vt_->set_bonus(ptr_, val) : BAM_RESULT_NULL_POINTER;
    }
    BamResult set_bonus_some(uint32_t val) noexcept {
        return set_bonus(ActorStats_bonus_Option{BAM_TRUE, val});
    }
    BamResult clear_bonus() noexcept {
        return (ptr_ != nullptr && vt_ != nullptr && vt_->clear_bonus != nullptr) ? vt_->clear_bonus(ptr_) : BAM_RESULT_NULL_POINTER;
    }
private:
    void* ptr_{nullptr};
    const ActorStatsVTable* vt_{nullptr};
};

} // extern "C++"
#endif /* __cplusplus */

typedef struct BehaviorMode BehaviorMode;
#define BehaviorMode_Tag_Idle ((uint8_t)0)
#define BehaviorMode_Tag_Patrol ((uint8_t)1)
#define BehaviorMode_Tag_Attack ((uint8_t)2)

typedef struct BehaviorModeVTable {
    BamVTableHeader header;
    uint8_t (*tag)(const void* ptr);
    BamResult (*set_idle)(void* ptr);
    BamResult (*set_patrol)(void* ptr, float speed, BamBool aggressive);
    BamResult (*try_get_patrol_speed)(const void* ptr, float* out_val);
    BamResult (*try_get_patrol_aggressive)(const void* ptr, BamBool* out_val);
    BamResult (*set_attack)(void* ptr, uint32_t _0);
    BamResult (*try_get_attack_0)(const void* ptr, uint32_t* out_val);
} BehaviorModeVTable;
BAM_STATIC_ASSERT(offsetof(BehaviorModeVTable, header) == 0u, behaviormodevtable_header_off);
#if UINTPTR_MAX == UINT64_MAX
BAM_STATIC_ASSERT(sizeof(BehaviorModeVTable) == 96u, behaviormodevtable_size64);
#elif UINTPTR_MAX == UINT32_MAX
BAM_STATIC_ASSERT(sizeof(BehaviorModeVTable) == 60u, behaviormodevtable_size32);
#endif

#ifdef __cplusplus
extern "C++" {
namespace bam {
#ifndef BAM_CPP17_SDK_HELPERS_DEFINED
#define BAM_CPP17_SDK_HELPERS_DEFINED

template <typename VT>
inline const VT* resolve_vtable(
    const BamHostApi& api,
    const BamHostCtx* host_ctx,
    const char* symbol,
    BamResult* out_err = nullptr
) noexcept {
    if (api.get_proc_address == nullptr || host_ctx == nullptr || symbol == nullptr) {
        if (out_err != nullptr) {
            *out_err = BAM_RESULT_NULL_POINTER;
        }
        return nullptr;
    }
    const void* raw = api.get_proc_address(host_ctx, symbol);
    if (raw == nullptr) {
        if (out_err != nullptr) {
            *out_err = BAM_RESULT_NOT_FOUND;
        }
        return nullptr;
    }
    const auto* hdr = static_cast<const BamVTableHeader*>(raw);
    if (hdr->struct_size < static_cast<uint32_t>(sizeof(VT)) ||
        bam_version_is_compatible(BAM_ABI_VERSION, hdr->abi_version) == BAM_FALSE) {
        if (out_err != nullptr) {
            *out_err = BAM_RESULT_INVALID_VERSION;
        }
        return nullptr;
    }
    if (out_err != nullptr) {
        *out_err = BAM_RESULT_OK;
    }
    return static_cast<const VT*>(raw);
}

template <typename VT>
class VTableHandle {
public:
    VTableHandle() noexcept = default;
    explicit VTableHandle(const VT* ptr) noexcept : ptr_(ptr) {}

    static VTableHandle resolve(
        const BamHostApi& api,
        const BamHostCtx* host_ctx,
        const char* symbol,
        BamResult* out_err = nullptr
    ) noexcept {
        return VTableHandle(resolve_vtable<VT>(api, host_ctx, symbol, out_err));
    }

    bool valid() const noexcept { return ptr_ != nullptr; }
    explicit operator bool() const noexcept { return valid(); }
    const VT* raw() const noexcept { return ptr_; }
    const VT* operator->() const noexcept { return ptr_; }

private:
    const VT* ptr_{nullptr};
};

template <typename InstanceT, void (*DestroyFn)(InstanceT*)>
class UniqueGuest {
public:
    UniqueGuest() noexcept = default;
    explicit UniqueGuest(InstanceT* ptr) noexcept : ptr_(ptr) {}
    ~UniqueGuest() noexcept { reset(); }

    UniqueGuest(const UniqueGuest&) = delete;
    UniqueGuest& operator=(const UniqueGuest&) = delete;

    UniqueGuest(UniqueGuest&& other) noexcept : ptr_(other.release()) {}
    UniqueGuest& operator=(UniqueGuest&& other) noexcept {
        if (this != &other) {
            reset(other.release());
        }
        return *this;
    }

    InstanceT* get() const noexcept { return ptr_; }
    InstanceT* operator->() const noexcept { return ptr_; }
    InstanceT& operator*() const noexcept { return *ptr_; }
    explicit operator bool() const noexcept { return ptr_ != nullptr; }

    InstanceT* release() noexcept {
        InstanceT* tmp = ptr_;
        ptr_ = nullptr;
        return tmp;
    }

    void reset(InstanceT* next = nullptr) noexcept {
        if constexpr (DestroyFn != nullptr) {
            if (ptr_ != nullptr) {
                DestroyFn(ptr_);
            }
        }
        ptr_ = next;
    }

private:
    InstanceT* ptr_{nullptr};
};

template <typename Elem, typename SliceTy, typename Ops>
class HostVecView {
public:
    HostVecView(void* ptr, const Ops& ops) noexcept : ptr_(ptr), ops_(ops) {}

    bool valid() const noexcept { return ptr_ != nullptr && ops_.valid(); }
    explicit operator bool() const noexcept { return valid(); }

    size_t len() const noexcept {
        return (ptr_ != nullptr && ops_.len != nullptr) ? ops_.len(ptr_) : 0u;
    }
    bool empty() const noexcept { return len() == 0u; }

    SliceTy as_slice() const noexcept {
        if (ptr_ == nullptr || ops_.get == nullptr) {
            return SliceTy{nullptr, 0u};
        }
        return ops_.get(ptr_);
    }

    const Elem* data() const noexcept { return as_slice().ptr; }

    Elem* data_mut(size_t* out_len = nullptr) noexcept {
        size_t local_len = 0u;
        size_t* len_ptr = out_len != nullptr ? out_len : &local_len;
        if (ptr_ == nullptr || ops_.as_mut_slice == nullptr) {
            *len_ptr = 0u;
            return nullptr;
        }
        return ops_.as_mut_slice(ptr_, len_ptr);
    }

    BamResult set(const Elem* ptr, size_t count) noexcept {
        if (ptr_ == nullptr || ops_.set == nullptr) {
            return BAM_RESULT_NULL_POINTER;
        }
        return ops_.set(ptr_, SliceTy{ptr, count});
    }

    BamResult clear() noexcept {
        return (ptr_ != nullptr && ops_.clear != nullptr) ? ops_.clear(ptr_) : BAM_RESULT_NULL_POINTER;
    }

    BamResult push(Elem val) noexcept {
        return (ptr_ != nullptr && ops_.push != nullptr) ? ops_.push(ptr_, val) : BAM_RESULT_NULL_POINTER;
    }

    bool pop(Elem* out_elem) noexcept {
        if (ptr_ == nullptr || ops_.pop == nullptr) {
            return false;
        }
        return ops_.pop(ptr_, out_elem) != BAM_FALSE;
    }

    BamResult remove(size_t index, Elem* out_elem) noexcept {
        return (ptr_ != nullptr && ops_.remove != nullptr)
            ? ops_.remove(ptr_, index, out_elem)
            : BAM_RESULT_NULL_POINTER;
    }

    BamResult swap_remove(size_t index, Elem* out_elem) noexcept {
        return (ptr_ != nullptr && ops_.swap_remove != nullptr)
            ? ops_.swap_remove(ptr_, index, out_elem)
            : BAM_RESULT_NULL_POINTER;
    }

    BamResult truncate(size_t new_len) noexcept {
        return (ptr_ != nullptr && ops_.truncate != nullptr)
            ? ops_.truncate(ptr_, new_len)
            : BAM_RESULT_NULL_POINTER;
    }

    BamResult reserve(size_t additional) noexcept {
        return (ptr_ != nullptr && ops_.reserve != nullptr)
            ? ops_.reserve(ptr_, additional)
            : BAM_RESULT_NULL_POINTER;
    }

    const Elem* begin() const noexcept { return as_slice().ptr; }
    const Elem* end() const noexcept {
        const SliceTy s = as_slice();
        return s.ptr != nullptr ? (s.ptr + s.len) : nullptr;
    }

    Elem* begin() noexcept { return data_mut(nullptr); }
    Elem* end() noexcept {
        size_t n = 0u;
        Elem* p = data_mut(&n);
        return p != nullptr ? (p + n) : nullptr;
    }

private:
    void* ptr_{nullptr};
    Ops   ops_{};
};

template <typename Elem, typename SliceTy, typename Ops>
class ConstHostVecView {
public:
    ConstHostVecView(const void* ptr, const Ops& ops) noexcept : ptr_(ptr), ops_(ops) {}

    bool valid() const noexcept { return ptr_ != nullptr && ops_.get != nullptr; }
    explicit operator bool() const noexcept { return valid(); }

    size_t len() const noexcept {
        return (ptr_ != nullptr && ops_.len != nullptr) ? ops_.len(ptr_) : 0u;
    }
    bool empty() const noexcept { return len() == 0u; }

    SliceTy as_slice() const noexcept {
        if (ptr_ == nullptr || ops_.get == nullptr) {
            return SliceTy{nullptr, 0u};
        }
        return ops_.get(ptr_);
    }

    const Elem* data() const noexcept { return as_slice().ptr; }
    const Elem* begin() const noexcept { return as_slice().ptr; }
    const Elem* end() const noexcept {
        const SliceTy s = as_slice();
        return s.ptr != nullptr ? (s.ptr + s.len) : nullptr;
    }

private:
    const void* ptr_{nullptr};
    Ops         ops_{};
};

#endif /* BAM_CPP17_SDK_HELPERS_DEFINED */
} // namespace bam
class BehaviorModeConstView {
public:
    BehaviorModeConstView() noexcept = default;
    BehaviorModeConstView(const void* ptr, const BehaviorModeVTable* vt) noexcept : ptr_(ptr), vt_(vt) {}
    bool valid() const noexcept { return ptr_ != nullptr && vt_ != nullptr; }
    explicit operator bool() const noexcept { return valid(); }
    const void* raw() const noexcept { return ptr_; }
    const BehaviorModeVTable* vtable() const noexcept { return vt_; }
    uint8_t tag() const noexcept {
        return (ptr_ != nullptr && vt_ != nullptr && vt_->tag != nullptr) ? vt_->tag(ptr_) : static_cast<uint8_t>(0);
    }
    BamResult try_get_patrol_speed(float* out_val) const noexcept {
        return (ptr_ != nullptr && vt_ != nullptr && vt_->try_get_patrol_speed != nullptr) ? vt_->try_get_patrol_speed(ptr_, out_val) : BAM_RESULT_NULL_POINTER;
    }
    BamResult try_get_patrol_aggressive(BamBool* out_val) const noexcept {
        return (ptr_ != nullptr && vt_ != nullptr && vt_->try_get_patrol_aggressive != nullptr) ? vt_->try_get_patrol_aggressive(ptr_, out_val) : BAM_RESULT_NULL_POINTER;
    }
    BamResult try_get_attack_0(uint32_t* out_val) const noexcept {
        return (ptr_ != nullptr && vt_ != nullptr && vt_->try_get_attack_0 != nullptr) ? vt_->try_get_attack_0(ptr_, out_val) : BAM_RESULT_NULL_POINTER;
    }
private:
    const void* ptr_{nullptr};
    const BehaviorModeVTable* vt_{nullptr};
};

class BehaviorModeView {
public:
    BehaviorModeView() noexcept = default;
    BehaviorModeView(void* ptr, const BehaviorModeVTable* vt) noexcept : ptr_(ptr), vt_(vt) {}
    bool valid() const noexcept { return ptr_ != nullptr && vt_ != nullptr; }
    explicit operator bool() const noexcept { return valid(); }
    void* raw() const noexcept { return ptr_; }
    const BehaviorModeVTable* vtable() const noexcept { return vt_; }
    BehaviorModeConstView as_const() const noexcept { return BehaviorModeConstView(ptr_, vt_); }
    uint8_t tag() const noexcept {
        return (ptr_ != nullptr && vt_ != nullptr && vt_->tag != nullptr) ? vt_->tag(ptr_) : static_cast<uint8_t>(0);
    }
    BamResult set_idle() noexcept {
        return (ptr_ != nullptr && vt_ != nullptr && vt_->set_idle != nullptr) ? vt_->set_idle(ptr_) : BAM_RESULT_NULL_POINTER;
    }
    BamResult set_patrol(float speed, bool aggressive) noexcept {
        return (ptr_ != nullptr && vt_ != nullptr && vt_->set_patrol != nullptr) ? vt_->set_patrol(ptr_, speed, aggressive ? BAM_TRUE : BAM_FALSE) : BAM_RESULT_NULL_POINTER;
    }
    BamResult try_get_patrol_speed(float* out_val) const noexcept {
        return (ptr_ != nullptr && vt_ != nullptr && vt_->try_get_patrol_speed != nullptr) ? vt_->try_get_patrol_speed(ptr_, out_val) : BAM_RESULT_NULL_POINTER;
    }
    BamResult try_get_patrol_aggressive(BamBool* out_val) const noexcept {
        return (ptr_ != nullptr && vt_ != nullptr && vt_->try_get_patrol_aggressive != nullptr) ? vt_->try_get_patrol_aggressive(ptr_, out_val) : BAM_RESULT_NULL_POINTER;
    }
    BamResult set_attack(uint32_t _0) noexcept {
        return (ptr_ != nullptr && vt_ != nullptr && vt_->set_attack != nullptr) ? vt_->set_attack(ptr_, _0) : BAM_RESULT_NULL_POINTER;
    }
    BamResult try_get_attack_0(uint32_t* out_val) const noexcept {
        return (ptr_ != nullptr && vt_ != nullptr && vt_->try_get_attack_0 != nullptr) ? vt_->try_get_attack_0(ptr_, out_val) : BAM_RESULT_NULL_POINTER;
    }
private:
    void* ptr_{nullptr};
    const BehaviorModeVTable* vt_{nullptr};
};

} // extern "C++"
#endif /* __cplusplus */

typedef struct CppTelemetry {
    uint32_t observed_adds;
    uint32_t entities_with_stats;
    uint32_t entities_without_stats;
    uint32_t child_entities_seen;
    uint32_t spawn_batch_count;
    BamBool batch_spawned;
    uint8_t pad[3];
} CppTelemetry;

typedef struct CppTelemetryVTable {
    BamVTableHeader header;
    uint32_t (*get_observed_adds)(const void* ptr);
    BamResult (*set_observed_adds)(void* ptr, uint32_t val);
    uint32_t (*get_entities_with_stats)(const void* ptr);
    BamResult (*set_entities_with_stats)(void* ptr, uint32_t val);
    uint32_t (*get_entities_without_stats)(const void* ptr);
    BamResult (*set_entities_without_stats)(void* ptr, uint32_t val);
    uint32_t (*get_child_entities_seen)(const void* ptr);
    BamResult (*set_child_entities_seen)(void* ptr, uint32_t val);
    uint32_t (*get_spawn_batch_count)(const void* ptr);
    BamResult (*set_spawn_batch_count)(void* ptr, uint32_t val);
    BamBool (*get_batch_spawned)(const void* ptr);
    BamResult (*set_batch_spawned)(void* ptr, BamBool val);
    const void* (*ptr_pad)(const void* ptr);
    void*       (*ptr_mut_pad)(void* ptr);
} CppTelemetryVTable;
BAM_STATIC_ASSERT(offsetof(CppTelemetryVTable, header) == 0u, cpptelemetryvtable_header_off);
#if UINTPTR_MAX == UINT64_MAX
BAM_STATIC_ASSERT(sizeof(CppTelemetryVTable) == 152u, cpptelemetryvtable_size64);
#elif UINTPTR_MAX == UINT32_MAX
BAM_STATIC_ASSERT(sizeof(CppTelemetryVTable) == 88u, cpptelemetryvtable_size32);
#endif
#ifdef __cplusplus
extern "C++" {
namespace bam {
#ifndef BAM_CPP17_SDK_HELPERS_DEFINED
#define BAM_CPP17_SDK_HELPERS_DEFINED

template <typename VT>
inline const VT* resolve_vtable(
    const BamHostApi& api,
    const BamHostCtx* host_ctx,
    const char* symbol,
    BamResult* out_err = nullptr
) noexcept {
    if (api.get_proc_address == nullptr || host_ctx == nullptr || symbol == nullptr) {
        if (out_err != nullptr) {
            *out_err = BAM_RESULT_NULL_POINTER;
        }
        return nullptr;
    }
    const void* raw = api.get_proc_address(host_ctx, symbol);
    if (raw == nullptr) {
        if (out_err != nullptr) {
            *out_err = BAM_RESULT_NOT_FOUND;
        }
        return nullptr;
    }
    const auto* hdr = static_cast<const BamVTableHeader*>(raw);
    if (hdr->struct_size < static_cast<uint32_t>(sizeof(VT)) ||
        bam_version_is_compatible(BAM_ABI_VERSION, hdr->abi_version) == BAM_FALSE) {
        if (out_err != nullptr) {
            *out_err = BAM_RESULT_INVALID_VERSION;
        }
        return nullptr;
    }
    if (out_err != nullptr) {
        *out_err = BAM_RESULT_OK;
    }
    return static_cast<const VT*>(raw);
}

template <typename VT>
class VTableHandle {
public:
    VTableHandle() noexcept = default;
    explicit VTableHandle(const VT* ptr) noexcept : ptr_(ptr) {}

    static VTableHandle resolve(
        const BamHostApi& api,
        const BamHostCtx* host_ctx,
        const char* symbol,
        BamResult* out_err = nullptr
    ) noexcept {
        return VTableHandle(resolve_vtable<VT>(api, host_ctx, symbol, out_err));
    }

    bool valid() const noexcept { return ptr_ != nullptr; }
    explicit operator bool() const noexcept { return valid(); }
    const VT* raw() const noexcept { return ptr_; }
    const VT* operator->() const noexcept { return ptr_; }

private:
    const VT* ptr_{nullptr};
};

template <typename InstanceT, void (*DestroyFn)(InstanceT*)>
class UniqueGuest {
public:
    UniqueGuest() noexcept = default;
    explicit UniqueGuest(InstanceT* ptr) noexcept : ptr_(ptr) {}
    ~UniqueGuest() noexcept { reset(); }

    UniqueGuest(const UniqueGuest&) = delete;
    UniqueGuest& operator=(const UniqueGuest&) = delete;

    UniqueGuest(UniqueGuest&& other) noexcept : ptr_(other.release()) {}
    UniqueGuest& operator=(UniqueGuest&& other) noexcept {
        if (this != &other) {
            reset(other.release());
        }
        return *this;
    }

    InstanceT* get() const noexcept { return ptr_; }
    InstanceT* operator->() const noexcept { return ptr_; }
    InstanceT& operator*() const noexcept { return *ptr_; }
    explicit operator bool() const noexcept { return ptr_ != nullptr; }

    InstanceT* release() noexcept {
        InstanceT* tmp = ptr_;
        ptr_ = nullptr;
        return tmp;
    }

    void reset(InstanceT* next = nullptr) noexcept {
        if constexpr (DestroyFn != nullptr) {
            if (ptr_ != nullptr) {
                DestroyFn(ptr_);
            }
        }
        ptr_ = next;
    }

private:
    InstanceT* ptr_{nullptr};
};

template <typename Elem, typename SliceTy, typename Ops>
class HostVecView {
public:
    HostVecView(void* ptr, const Ops& ops) noexcept : ptr_(ptr), ops_(ops) {}

    bool valid() const noexcept { return ptr_ != nullptr && ops_.valid(); }
    explicit operator bool() const noexcept { return valid(); }

    size_t len() const noexcept {
        return (ptr_ != nullptr && ops_.len != nullptr) ? ops_.len(ptr_) : 0u;
    }
    bool empty() const noexcept { return len() == 0u; }

    SliceTy as_slice() const noexcept {
        if (ptr_ == nullptr || ops_.get == nullptr) {
            return SliceTy{nullptr, 0u};
        }
        return ops_.get(ptr_);
    }

    const Elem* data() const noexcept { return as_slice().ptr; }

    Elem* data_mut(size_t* out_len = nullptr) noexcept {
        size_t local_len = 0u;
        size_t* len_ptr = out_len != nullptr ? out_len : &local_len;
        if (ptr_ == nullptr || ops_.as_mut_slice == nullptr) {
            *len_ptr = 0u;
            return nullptr;
        }
        return ops_.as_mut_slice(ptr_, len_ptr);
    }

    BamResult set(const Elem* ptr, size_t count) noexcept {
        if (ptr_ == nullptr || ops_.set == nullptr) {
            return BAM_RESULT_NULL_POINTER;
        }
        return ops_.set(ptr_, SliceTy{ptr, count});
    }

    BamResult clear() noexcept {
        return (ptr_ != nullptr && ops_.clear != nullptr) ? ops_.clear(ptr_) : BAM_RESULT_NULL_POINTER;
    }

    BamResult push(Elem val) noexcept {
        return (ptr_ != nullptr && ops_.push != nullptr) ? ops_.push(ptr_, val) : BAM_RESULT_NULL_POINTER;
    }

    bool pop(Elem* out_elem) noexcept {
        if (ptr_ == nullptr || ops_.pop == nullptr) {
            return false;
        }
        return ops_.pop(ptr_, out_elem) != BAM_FALSE;
    }

    BamResult remove(size_t index, Elem* out_elem) noexcept {
        return (ptr_ != nullptr && ops_.remove != nullptr)
            ? ops_.remove(ptr_, index, out_elem)
            : BAM_RESULT_NULL_POINTER;
    }

    BamResult swap_remove(size_t index, Elem* out_elem) noexcept {
        return (ptr_ != nullptr && ops_.swap_remove != nullptr)
            ? ops_.swap_remove(ptr_, index, out_elem)
            : BAM_RESULT_NULL_POINTER;
    }

    BamResult truncate(size_t new_len) noexcept {
        return (ptr_ != nullptr && ops_.truncate != nullptr)
            ? ops_.truncate(ptr_, new_len)
            : BAM_RESULT_NULL_POINTER;
    }

    BamResult reserve(size_t additional) noexcept {
        return (ptr_ != nullptr && ops_.reserve != nullptr)
            ? ops_.reserve(ptr_, additional)
            : BAM_RESULT_NULL_POINTER;
    }

    const Elem* begin() const noexcept { return as_slice().ptr; }
    const Elem* end() const noexcept {
        const SliceTy s = as_slice();
        return s.ptr != nullptr ? (s.ptr + s.len) : nullptr;
    }

    Elem* begin() noexcept { return data_mut(nullptr); }
    Elem* end() noexcept {
        size_t n = 0u;
        Elem* p = data_mut(&n);
        return p != nullptr ? (p + n) : nullptr;
    }

private:
    void* ptr_{nullptr};
    Ops   ops_{};
};

template <typename Elem, typename SliceTy, typename Ops>
class ConstHostVecView {
public:
    ConstHostVecView(const void* ptr, const Ops& ops) noexcept : ptr_(ptr), ops_(ops) {}

    bool valid() const noexcept { return ptr_ != nullptr && ops_.get != nullptr; }
    explicit operator bool() const noexcept { return valid(); }

    size_t len() const noexcept {
        return (ptr_ != nullptr && ops_.len != nullptr) ? ops_.len(ptr_) : 0u;
    }
    bool empty() const noexcept { return len() == 0u; }

    SliceTy as_slice() const noexcept {
        if (ptr_ == nullptr || ops_.get == nullptr) {
            return SliceTy{nullptr, 0u};
        }
        return ops_.get(ptr_);
    }

    const Elem* data() const noexcept { return as_slice().ptr; }
    const Elem* begin() const noexcept { return as_slice().ptr; }
    const Elem* end() const noexcept {
        const SliceTy s = as_slice();
        return s.ptr != nullptr ? (s.ptr + s.len) : nullptr;
    }

private:
    const void* ptr_{nullptr};
    Ops         ops_{};
};

#endif /* BAM_CPP17_SDK_HELPERS_DEFINED */
} // namespace bam
class CppTelemetryConstView {
public:
    CppTelemetryConstView() noexcept = default;
    CppTelemetryConstView(const void* ptr, const CppTelemetryVTable* vt) noexcept : ptr_(ptr), vt_(vt) {}
    bool valid() const noexcept { return ptr_ != nullptr && vt_ != nullptr; }
    explicit operator bool() const noexcept { return valid(); }
    const void* raw() const noexcept { return ptr_; }
    const CppTelemetryVTable* vtable() const noexcept { return vt_; }
    uint32_t get_observed_adds() const noexcept {
        return (ptr_ != nullptr && vt_ != nullptr && vt_->get_observed_adds != nullptr) ? vt_->get_observed_adds(ptr_) : uint32_t{};
    }
    uint32_t get_entities_with_stats() const noexcept {
        return (ptr_ != nullptr && vt_ != nullptr && vt_->get_entities_with_stats != nullptr) ? vt_->get_entities_with_stats(ptr_) : uint32_t{};
    }
    uint32_t get_entities_without_stats() const noexcept {
        return (ptr_ != nullptr && vt_ != nullptr && vt_->get_entities_without_stats != nullptr) ? vt_->get_entities_without_stats(ptr_) : uint32_t{};
    }
    uint32_t get_child_entities_seen() const noexcept {
        return (ptr_ != nullptr && vt_ != nullptr && vt_->get_child_entities_seen != nullptr) ? vt_->get_child_entities_seen(ptr_) : uint32_t{};
    }
    uint32_t get_spawn_batch_count() const noexcept {
        return (ptr_ != nullptr && vt_ != nullptr && vt_->get_spawn_batch_count != nullptr) ? vt_->get_spawn_batch_count(ptr_) : uint32_t{};
    }
    BamBool get_batch_spawned() const noexcept {
        return (ptr_ != nullptr && vt_ != nullptr && vt_->get_batch_spawned != nullptr) ? vt_->get_batch_spawned(ptr_) : BamBool{};
    }
    const void* ptr_pad() const noexcept {
        return (ptr_ != nullptr && vt_ != nullptr && vt_->ptr_pad != nullptr) ? vt_->ptr_pad(ptr_) : nullptr;
    }
private:
    const void* ptr_{nullptr};
    const CppTelemetryVTable* vt_{nullptr};
};

class CppTelemetryView {
public:
    CppTelemetryView() noexcept = default;
    CppTelemetryView(void* ptr, const CppTelemetryVTable* vt) noexcept : ptr_(ptr), vt_(vt) {}
    bool valid() const noexcept { return ptr_ != nullptr && vt_ != nullptr; }
    explicit operator bool() const noexcept { return valid(); }
    void* raw() const noexcept { return ptr_; }
    const CppTelemetryVTable* vtable() const noexcept { return vt_; }
    CppTelemetryConstView as_const() const noexcept { return CppTelemetryConstView(ptr_, vt_); }
    uint32_t get_observed_adds() const noexcept {
        return (ptr_ != nullptr && vt_ != nullptr && vt_->get_observed_adds != nullptr) ? vt_->get_observed_adds(ptr_) : uint32_t{};
    }
    BamResult set_observed_adds(uint32_t val) noexcept {
        return (ptr_ != nullptr && vt_ != nullptr && vt_->set_observed_adds != nullptr) ? vt_->set_observed_adds(ptr_, val) : BAM_RESULT_NULL_POINTER;
    }
    uint32_t get_entities_with_stats() const noexcept {
        return (ptr_ != nullptr && vt_ != nullptr && vt_->get_entities_with_stats != nullptr) ? vt_->get_entities_with_stats(ptr_) : uint32_t{};
    }
    BamResult set_entities_with_stats(uint32_t val) noexcept {
        return (ptr_ != nullptr && vt_ != nullptr && vt_->set_entities_with_stats != nullptr) ? vt_->set_entities_with_stats(ptr_, val) : BAM_RESULT_NULL_POINTER;
    }
    uint32_t get_entities_without_stats() const noexcept {
        return (ptr_ != nullptr && vt_ != nullptr && vt_->get_entities_without_stats != nullptr) ? vt_->get_entities_without_stats(ptr_) : uint32_t{};
    }
    BamResult set_entities_without_stats(uint32_t val) noexcept {
        return (ptr_ != nullptr && vt_ != nullptr && vt_->set_entities_without_stats != nullptr) ? vt_->set_entities_without_stats(ptr_, val) : BAM_RESULT_NULL_POINTER;
    }
    uint32_t get_child_entities_seen() const noexcept {
        return (ptr_ != nullptr && vt_ != nullptr && vt_->get_child_entities_seen != nullptr) ? vt_->get_child_entities_seen(ptr_) : uint32_t{};
    }
    BamResult set_child_entities_seen(uint32_t val) noexcept {
        return (ptr_ != nullptr && vt_ != nullptr && vt_->set_child_entities_seen != nullptr) ? vt_->set_child_entities_seen(ptr_, val) : BAM_RESULT_NULL_POINTER;
    }
    uint32_t get_spawn_batch_count() const noexcept {
        return (ptr_ != nullptr && vt_ != nullptr && vt_->get_spawn_batch_count != nullptr) ? vt_->get_spawn_batch_count(ptr_) : uint32_t{};
    }
    BamResult set_spawn_batch_count(uint32_t val) noexcept {
        return (ptr_ != nullptr && vt_ != nullptr && vt_->set_spawn_batch_count != nullptr) ? vt_->set_spawn_batch_count(ptr_, val) : BAM_RESULT_NULL_POINTER;
    }
    BamBool get_batch_spawned() const noexcept {
        return (ptr_ != nullptr && vt_ != nullptr && vt_->get_batch_spawned != nullptr) ? vt_->get_batch_spawned(ptr_) : BamBool{};
    }
    BamResult set_batch_spawned(BamBool val) noexcept {
        return (ptr_ != nullptr && vt_ != nullptr && vt_->set_batch_spawned != nullptr) ? vt_->set_batch_spawned(ptr_, val) : BAM_RESULT_NULL_POINTER;
    }
    const void* ptr_pad() const noexcept {
        return (ptr_ != nullptr && vt_ != nullptr && vt_->ptr_pad != nullptr) ? vt_->ptr_pad(ptr_) : nullptr;
    }
    void* ptr_mut_pad() noexcept {
        return (ptr_ != nullptr && vt_ != nullptr && vt_->ptr_mut_pad != nullptr) ? vt_->ptr_mut_pad(ptr_) : nullptr;
    }
private:
    void* ptr_{nullptr};
    const CppTelemetryVTable* vt_{nullptr};
};

} // extern "C++"
#endif /* __cplusplus */

#ifdef __cplusplus
}
#endif

#endif /* EXAMPLE_07_COMPONENTS_H */
