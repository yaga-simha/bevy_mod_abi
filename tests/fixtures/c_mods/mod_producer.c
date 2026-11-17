#include <stddef.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include "bevy_abi_mod.h"

typedef struct OrderTraceRes {
    uint32_t steps[4];
    uint32_t len;
} OrderTraceRes;

struct BamInstance {
    BamHostApi   api;
    BamComponent trace_res;
};

static const BamManifest MANIFEST = {
    .struct_size = (uint32_t)sizeof(BamManifest),
    .abi_version = BAM_ABI_VERSION,
    .name = "mod_producer",
    .version = "0.2.0",
    .flags = 0u,
    .reserved0 = 0u
};

static BamResult produce_cb(BamSystemCtx* sys_ctx, void* user_data) {
    BamInstance* st = (BamInstance*)user_data;
    BamIter* iter = NULL;
    OrderTraceRes* r;
    if (st == NULL || st->api.sys_query(sys_ctx, 0u, &iter) != BAM_RESULT_OK) {
        return BAM_RESULT_INTERNAL_ERROR;
    }
    r = (OrderTraceRes*)(void*)st->api.res_mut_ptr(iter, 0u);
    if (r != NULL && r->len < 4u) {
        r->steps[r->len++] = 1u;
    }
    return BAM_RESULT_OK;
}

BAM_EXPORT const BamManifest* bevy_abi_mod_manifest(void) {
    return &MANIFEST;
}

BAM_EXPORT BamResult bevy_abi_mod_init(
    const BamHostApi* api,
    BamLoadCtx* load_ctx,
    BamInstance** instance_out
) {
    BamHostApi local_api;
    const BamHostCtx* host_ctx;
    BamInstance* st;
    BamTermDesc term;
    BamQueryDesc q_desc;
    BamSystemDesc desc;
    BamResult rc;

    if (load_ctx == NULL || instance_out == NULL) {
        return BAM_RESULT_NULL_POINTER;
    }
    *instance_out = NULL;

    rc = bam_host_api_copy_bounded(&local_api, sizeof(local_api), api);
    if (rc != BAM_RESULT_OK) {
        return rc;
    }
    host_ctx = local_api.load_host_ctx(load_ctx);
    if (host_ctx == NULL) {
        return BAM_RESULT_INVALID_STATE;
    }

    st = (BamInstance*)calloc(1u, sizeof(BamInstance));
    if (st == NULL) {
        return BAM_RESULT_INTERNAL_ERROR;
    }
    st->api = local_api;

    rc = st->api.resolve_component(host_ctx, "sched_reg::OrderTraceRes", NULL, &st->trace_res);
    if (rc != BAM_RESULT_OK) {
        rc = st->api.resolve_component(host_ctx, "OrderTraceRes", NULL, &st->trace_res);
    }
    if (rc != BAM_RESULT_OK) {
        free(st);
        return rc;
    }

    term = bam_term_desc(st->trace_res, BAM_ACCESS_RESOURCE_WRITE, 0u);
    memset(&q_desc, 0, sizeof(q_desc));
    q_desc.struct_size = (uint32_t)sizeof(BamQueryDesc);
    q_desc.terms = &term;
    q_desc.term_count = 1u;

    memset(&desc, 0, sizeof(desc));
    desc.struct_size = (uint32_t)sizeof(BamSystemDesc);
    desc.name = "produce";
    desc.queries = &q_desc;
    desc.query_count = 1u;
    desc.callback = produce_cb;
    desc.user_data = st;
    desc.schedule = BAM_SCHEDULE_UPDATE;
    rc = st->api.register_system(load_ctx, &desc);
    if (rc != BAM_RESULT_OK) {
        free(st);
        return rc;
    }

    *instance_out = st;
    return BAM_RESULT_OK;
}

BAM_EXPORT void bevy_abi_mod_shutdown(BamInstance* instance) {
    if (instance != NULL) {
        free(instance);
    }
}
