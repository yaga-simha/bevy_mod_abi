/*
 * test_abi_layout.c
 * Empirical Cross-Language C-ABI v0.2 Verification Test.
 *
 * Compiles with: cc -std=c99 -Wall -Wextra -Werror -I../../include -o test_abi_layout test_abi_layout.c
 */

#include "../../include/bevy_abi_mod.h"
#include <assert.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static int g_custom_drop_called = 0;
static void* g_dropped_ptr = NULL;

static void custom_drop_callback(void* ptr) {
    g_custom_drop_called++;
    g_dropped_ptr = ptr;
}

static BamResult mock_register_component(
    BamLoadCtx* load_ctx,
    const BamComponentDesc* desc,
    BamComponent* out_component
) {
    if (load_ctx == NULL || desc == NULL || out_component == NULL) {
        return BAM_RESULT_NULL_POINTER;
    }
    if (desc->struct_size < BAM_COMPONENT_DESC_MIN_V0_2_SIZE) {
        return BAM_RESULT_INVALID_VERSION;
    }
    if ((desc->flags & ~BAM_COMPONENT_KNOWN_FLAGS) != 0u) {
        return BAM_RESULT_UNSUPPORTED;
    }
    if (desc->name == NULL) {
        return BAM_RESULT_NULL_POINTER;
    }
    if (desc->align == 0u || (desc->align & (desc->align - 1u)) != 0u) {
        return BAM_RESULT_INVALID_ARGUMENT;
    }
    if (desc->size % desc->align != 0u) {
        return BAM_RESULT_INVALID_ARGUMENT;
    }
    *out_component = 1u;
    return BAM_RESULT_OK;
}

int main(void) {
    BamComponentDesc desc;
    BamComponentDesc bad_desc;
    BamHostApi api;
    BamLoadCtx* mock_load_ctx = (BamLoadCtx*)0xCAFE;
    BamComponent out_comp = BAM_COMPONENT_INVALID;
    int dummy_target = 42;

    printf("[C99 ABI v0.2 Layout Verification]\n");

    assert(BAM_COMPONENT_DESC_MIN_V0_2_SIZE == 64u);
    assert(sizeof(BamComponentDesc) == 136u);
    assert(BAM_ALIGNOF(BamComponentDesc) == 8u);
    assert(offsetof(BamComponentDesc, struct_size) == 0u);
    assert(offsetof(BamComponentDesc, flags) == 4u);
    assert(offsetof(BamComponentDesc, name) == 8u);
    assert(offsetof(BamComponentDesc, uuid) == 16u);
    assert(offsetof(BamComponentDesc, size) == 32u);
    assert(offsetof(BamComponentDesc, align) == 40u);
    assert(offsetof(BamComponentDesc, drop_fn) == 48u);
    assert(offsetof(BamComponentDesc, vtable_ptr) == 56u);
    assert(offsetof(BamComponentDesc, on_add) == 64u);
    assert(offsetof(BamComponentDesc, on_remove) == 72u);
    assert(offsetof(BamComponentDesc, hook_user_data) == 80u);
    assert(offsetof(BamComponentDesc, on_insert) == 88u);
    assert(offsetof(BamComponentDesc, on_discard) == 96u);
    assert(offsetof(BamComponentDesc, on_despawn) == 104u);
    assert(offsetof(BamComponentDesc, clone_fn) == 112u);
    assert(offsetof(BamComponentDesc, required_components) == 120u);
    assert(offsetof(BamComponentDesc, required_component_count) == 128u);

    assert(BAM_HOST_API_MIN_V0_2_SIZE == 224u);
    assert(sizeof(BamHostApi) == 408u);
    assert(BAM_ALIGNOF(BamHostApi) == 8u);

    memset(&desc, 0, sizeof(desc));
    desc.struct_size = (uint32_t)sizeof(BamComponentDesc);
    desc.flags = BAM_COMPONENT_FLAG_SPARSE_SET;
    desc.name = "c99_mod::DynamicComponent";
    desc.uuid = bam_uuid_from_parts("c99_mod::DynamicComponent", 128u, 16u, 0ull);
    desc.size = 128u;
    desc.align = 16u;
    desc.drop_fn = custom_drop_callback;
    desc.vtable_ptr = (const void*)0xDEADBEEF;

    desc.drop_fn(&dummy_target);
    assert(g_custom_drop_called == 1);
    assert(g_dropped_ptr == &dummy_target);

    memset(&api, 0, sizeof(api));
    api.struct_size = (uint32_t)sizeof(BamHostApi);
    api.abi_version = BAM_ABI_VERSION;
    api.register_component = mock_register_component;

    assert(api.register_component(mock_load_ctx, &desc, &out_comp) == BAM_RESULT_OK);
    assert(out_comp == 1u);

    assert(api.register_component(NULL, &desc, &out_comp) == BAM_RESULT_NULL_POINTER);
    assert(api.register_component(mock_load_ctx, NULL, &out_comp) == BAM_RESULT_NULL_POINTER);
    assert(api.register_component(mock_load_ctx, &desc, NULL) == BAM_RESULT_NULL_POINTER);

    bad_desc = desc;
    bad_desc.struct_size = 32u;
    assert(api.register_component(mock_load_ctx, &bad_desc, &out_comp) == BAM_RESULT_INVALID_VERSION);

    bad_desc = desc;
    bad_desc.flags = 0x8000u;
    assert(api.register_component(mock_load_ctx, &bad_desc, &out_comp) == BAM_RESULT_UNSUPPORTED);

    bad_desc = desc;
    bad_desc.size = 15u;
    bad_desc.align = 8u;
    assert(api.register_component(mock_load_ctx, &bad_desc, &out_comp) == BAM_RESULT_INVALID_ARGUMENT);

    bad_desc = desc;
    bad_desc.align = 7u;
    assert(api.register_component(mock_load_ctx, &bad_desc, &out_comp) == BAM_RESULT_INVALID_ARGUMENT);

    printf("[SUCCESS] All C99 ABI v0.2 layout and dispatch tests passed!\n");
    return 0;
}
