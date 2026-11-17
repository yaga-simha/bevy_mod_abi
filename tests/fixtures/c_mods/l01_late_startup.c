#include <stddef.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include "bevy_abi_mod.h"

typedef struct CounterRes {
    uint32_t startup_hits;
    uint32_t update_hits;
} CounterRes;

struct BamInstance {
    BamHostApi   api;
    BamComponent counter_res;
};

static const BamManifest MANIFEST = {
    (uint32_t)sizeof(BamManifest),
    BAM_ABI_VERSION,
    "l01_late_startup",
    "0.2.0",
    0u,
    0u
};

static BamResult on_startup(BamSystemCtx* sys_ctx, void* user_data) {
    BamInstance* st = (BamInstance*)user_data;
    BamIter* iter = NULL;
    CounterRes* c;
    if (st == NULL) {
        return BAM_RESULT_INTERNAL_ERROR;
    }
    if (st->api.sys_query(sys_ctx, 0u, &iter) != BAM_RESULT_OK) {
        return BAM_RESULT_INTERNAL_ERROR;
    }
    c = (CounterRes*)(void*)st->api.res_mut_ptr(iter, 0u);
    if (c != NULL) {
        c->startup_hits += 1u;
    }
    return BAM_RESULT_OK;
}

static BamResult on_update(BamSystemCtx* sys_ctx, void* user_data) {
    BamInstance* st = (BamInstance*)user_data;
    BamIter* iter = NULL;
    CounterRes* c;
    if (st == NULL) {
        return BAM_RESULT_INTERNAL_ERROR;
    }
    if (st->api.sys_query(sys_ctx, 0u, &iter) != BAM_RESULT_OK) {
        return BAM_RESULT_INTERNAL_ERROR;
    }
    c = (CounterRes*)(void*)st->api.res_mut_ptr(iter, 0u);
    if (c != NULL) {
        c->update_hits += 1u;
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
    BamSystemDesc d1;
    BamSystemDesc d2;
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

    rc = st->api.resolve_component(host_ctx, "sched_reg::CounterRes", NULL, &st->counter_res);
    if (rc != BAM_RESULT_OK) {
        rc = st->api.resolve_component(host_ctx, "CounterRes", NULL, &st->counter_res);
    }
    if (rc != BAM_RESULT_OK) {
        free(st);
        return rc;
    }

    term = bam_term_desc(st->counter_res, BAM_ACCESS_RESOURCE_WRITE, 0u);
    memset(&q_desc, 0, sizeof(q_desc));
    q_desc.struct_size = (uint32_t)sizeof(BamQueryDesc);
    q_desc.terms = &term;
    q_desc.term_count = 1u;

    memset(&d1, 0, sizeof(d1));
    d1.struct_size = (uint32_t)sizeof(BamSystemDesc);
    d1.name = "late_startup_sys";
    d1.queries = &q_desc;
    d1.query_count = 1u;
    d1.callback = on_startup;
    d1.user_data = st;
    d1.schedule = BAM_SCHEDULE_STARTUP;
    rc = st->api.register_system(load_ctx, &d1);
    if (rc != BAM_RESULT_OK) {
        free(st);
        return rc;
    }

    memset(&d2, 0, sizeof(d2));
    d2.struct_size = (uint32_t)sizeof(BamSystemDesc);
    d2.name = "regular_update_sys";
    d2.queries = &q_desc;
    d2.query_count = 1u;
    d2.callback = on_update;
    d2.user_data = st;
    d2.schedule = BAM_SCHEDULE_UPDATE;
    rc = st->api.register_system(load_ctx, &d2);
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
