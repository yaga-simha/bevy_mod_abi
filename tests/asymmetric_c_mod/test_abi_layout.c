/*
 * test_abi_layout.c
 * Empirical Cross-Language C-ABI Verification Test for Milestone 1.
 *
 * Compiles with: cc -std=c99 -Wall -Wextra -Werror -I../../include -o test_abi_layout test_abi_layout.c
 */

#include "../../include/bevy_abi_mod.h"
#include <stdio.h>
#include <stddef.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <assert.h>

static int g_custom_drop_called = 0;
static void* g_dropped_ptr = NULL;

static void custom_drop_callback(void* ptr) {
    g_custom_drop_called++;
    g_dropped_ptr = ptr;
}

static FfiResult mock_register_component(void* host_context, const ModComponentDescriptor* desc) {
    if (!host_context || !desc) {
        return FFI_RESULT_NULL_POINTER;
    }
    if (desc->struct_size < sizeof(ModComponentDescriptor)) {
        return FFI_RESULT_INVALID_VERSION;
    }
    if (!desc->name) {
        return FFI_RESULT_NULL_POINTER;
    }
    if (desc->align == 0 || (desc->align & (desc->align - 1)) != 0) {
        return FFI_RESULT_INTERNAL_ERROR;
    }
    if (desc->size % desc->align != 0) {
        return FFI_RESULT_INTERNAL_ERROR;
    }
    return FFI_RESULT_OK;
}

int main(void) {
    printf("[C99 ABI Layout Verification]\n");

    /* 1. ModComponentDescriptor Layout & Alignment */
    printf("Checking ModComponentDescriptor layout...\n");
    assert(sizeof(ModComponentDescriptor) >= 64);
    assert(BEVY_ABI_ALIGNOF(ModComponentDescriptor) == 8);

    assert(offsetof(ModComponentDescriptor, struct_size) == 0);
    assert(offsetof(ModComponentDescriptor, name) == 8);
    assert(offsetof(ModComponentDescriptor, uuid) == 16);
    assert(offsetof(ModComponentDescriptor, size) == 32);
    assert(offsetof(ModComponentDescriptor, align) == 40);
    assert(offsetof(ModComponentDescriptor, drop_fn) == 48);
    assert(offsetof(ModComponentDescriptor, vtable_ptr) == 56);

    /* 2. HostApi Layout & Alignment */
    printf("Checking HostApi layout...\n");
    assert(sizeof(HostApi) >= 72);
    assert(BEVY_ABI_ALIGNOF(HostApi) == 8);

    assert(offsetof(HostApi, struct_size) == 0);
    assert(offsetof(HostApi, abi_version) == 4);
    assert(offsetof(HostApi, register_system) == 8);
    assert(offsetof(HostApi, iter_next_chunk) == 16);
    assert(offsetof(HostApi, iter_column_readonly) == 24);
    assert(offsetof(HostApi, iter_column_mutable) == 32);
    assert(offsetof(HostApi, iter_mark_changed) == 40);
    assert(offsetof(HostApi, iter_entities) == 48);
    assert(offsetof(HostApi, iter_column_stride) == 56);
    assert(offsetof(HostApi, register_component) == 64);

    /* 3. Field isolation and non-corruption test */
    printf("Checking field isolation and memory mutations...\n");
    ModComponentDescriptor desc;
    memset(&desc, 0xAA, sizeof(desc));

    desc.struct_size = (uint32_t)sizeof(ModComponentDescriptor);
    desc.name = "C99DynamicComponent";
    desc.uuid.bytes[0] = 0x12;
    desc.uuid.bytes[15] = 0x34;
    desc.size = 128;
    desc.align = 16;
    desc.drop_fn = custom_drop_callback;
    desc.vtable_ptr = (const void*)0xDEADBEEF;

    assert(desc.struct_size == sizeof(ModComponentDescriptor));
    assert(strcmp(desc.name, "C99DynamicComponent") == 0);
    assert(desc.uuid.bytes[0] == 0x12);
    assert(desc.uuid.bytes[15] == 0x34);
    assert(desc.size == 128);
    assert(desc.align == 16);
    assert(desc.drop_fn == custom_drop_callback);
    assert(desc.vtable_ptr == (const void*)0xDEADBEEF);

    /* 4. Drop callback execution */
    printf("Checking drop callback execution...\n");
    int dummy_target = 42;
    desc.drop_fn(&dummy_target);
    assert(g_custom_drop_called == 1);
    assert(g_dropped_ptr == &dummy_target);

    /* 5. HostApi dispatch execution */
    printf("Checking HostApi dispatch...\n");
    HostApi api;
    memset(&api, 0, sizeof(api));
    api.struct_size = (uint32_t)sizeof(HostApi);
    api.abi_version = 0x00010000;
    api.register_component = mock_register_component;

    void* mock_host_ctx = (void*)0xCAFE;
    FfiResult res = api.register_component(mock_host_ctx, &desc);
    assert(res == FFI_RESULT_OK);

    /* 6. Test defensive validation through HostApi callback */
    /* Null context */
    assert(api.register_component(NULL, &desc) == FFI_RESULT_NULL_POINTER);
    /* Null desc */
    assert(api.register_component(mock_host_ctx, NULL) == FFI_RESULT_NULL_POINTER);

    /* Truncated struct size */
    ModComponentDescriptor bad_desc = desc;
    bad_desc.struct_size = 32;
    assert(api.register_component(mock_host_ctx, &bad_desc) == FFI_RESULT_INVALID_VERSION);

    /* Misaligned size */
    bad_desc = desc;
    bad_desc.size = 15;
    bad_desc.align = 8;
    assert(api.register_component(mock_host_ctx, &bad_desc) == FFI_RESULT_INTERNAL_ERROR);

    /* Non-power-of-two align */
    bad_desc = desc;
    bad_desc.align = 7;
    assert(api.register_component(mock_host_ctx, &bad_desc) == FFI_RESULT_INTERNAL_ERROR);

    printf("[SUCCESS] All C99 ABI layout and dispatch tests passed!\n");
    return 0;
}
