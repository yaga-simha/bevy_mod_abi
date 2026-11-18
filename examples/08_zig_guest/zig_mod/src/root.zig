//! Zig Guest Mod for `bevy_abi_mod` Example 08 (`ABI 0.2.0`).
//!
//! Demonstrates:
//! 1. Importing `generated_components.zig` (`ModComponent::ZIG_DECL`) and `include/bevy_abi_mod.h`.
//! 2. Exporting `bevy_abi_mod_manifest`, `bevy_abi_mod_init`, and `bevy_abi_mod_shutdown`
//!    with `callconv(.c)`.
//! 3. Mapping typed Zig error unions (`BamError!void`) cleanly to `c.BamResult` status codes
//!    without unwinding across `extern "C"` (`spec.md` §3.4).
//! 4. Per-`World` `ZigModInstance` allocated on `std.heap.c_allocator` (`*instance_out`)
//!    performing zero-copy SoA chunk iteration over `example::Position` and `example::Velocity`.
//! 5. Using Zig `comptime` VTable/view wrappers (`ZigActorStatsView`, `VecView`, `ZigBehaviorModeView`)
//!    when `example::ZigActorStats` and `example::ZigBehaviorMode` are registered by the host.

const std = @import("std");
const gen = @import("generated_components.zig");
const c = gen.bam;

pub const Position = gen.Position;
pub const Velocity = gen.Velocity;
pub const ZigSimConfig = gen.ZigSimConfig;

comptime {
    if (@sizeOf(Position) != 12 or @alignOf(Position) != 4) {
        @compileError("Position layout mismatch");
    }
    if (@sizeOf(Velocity) != 12 or @alignOf(Velocity) != 4) {
        @compileError("Velocity layout mismatch");
    }
    if (@sizeOf(ZigSimConfig) != 8 or @alignOf(ZigSimConfig) != 4) {
        @compileError("ZigSimConfig layout mismatch");
    }
    if (@offsetOf(gen.ZigActorStatsVTable, "header") != 0) {
        @compileError("ZigActorStatsVTable header must sit at offset 0");
    }
    if (@offsetOf(gen.ZigBehaviorModeVTable, "header") != 0) {
        @compileError("ZigBehaviorModeVTable header must sit at offset 0");
    }
}

const BamError = error{
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
    InternalError,
};

fn mapErrorToBamResult(err: BamError) c.BamResult {
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
        error.OutOfMemory, error.InternalError => c.BAM_RESULT_INTERNAL_ERROR,
    };
}

fn checkBamResult(res: c.BamResult) BamError!void {
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
        else => error.InternalError,
    };
}

const ZigModInstance = struct {
    api: c.BamHostApi,
    pos_comp: c.BamComponent,
    vel_comp: c.BamComponent,
    cfg_res: c.BamComponent,
    stats_comp: c.BamComponent,
    mode_comp: c.BamComponent,
    stats_vt: ?*const gen.ZigActorStatsVTable,
    mode_vt: ?*const gen.ZigBehaviorModeVTable,

    fn initAndRegister(self: *ZigModInstance, load_ctx: *c.BamLoadCtx) BamError!void {
        const load_host_ctx = self.api.load_host_ctx orelse return error.NullPointer;
        const resolve_component = self.api.resolve_component orelse return error.NullPointer;
        const register_system = self.api.register_system orelse return error.NullPointer;

        const host_ctx = load_host_ctx(load_ctx) orelse return error.NullPointer;

        try checkBamResult(resolve_component(host_ctx, "example::Position", null, &self.pos_comp));
        try checkBamResult(resolve_component(host_ctx, "example::Velocity", null, &self.vel_comp));
        try checkBamResult(resolve_component(host_ctx, "example::ZigSimConfig", null, &self.cfg_res));

        const terms = [_]c.BamTermDesc{
            c.bam_term_desc(self.pos_comp, c.BAM_ACCESS_WRITE, 0),
            c.bam_term_desc(self.vel_comp, c.BAM_ACCESS_READ, 0),
            c.bam_term_desc(self.cfg_res, c.BAM_ACCESS_RESOURCE_READ, 0),
        };

        var query = std.mem.zeroes(c.BamQueryDesc);
        query.struct_size = @intCast(@sizeOf(c.BamQueryDesc));
        query.terms = &terms[0];
        query.term_count = terms.len;

        var sys_desc = std.mem.zeroes(c.BamSystemDesc);
        sys_desc.struct_size = @intCast(@sizeOf(c.BamSystemDesc));
        sys_desc.name = "zig_orbital_step";
        sys_desc.queries = &query;
        sys_desc.query_count = 1;
        sys_desc.callback = &zigSystemCallback;
        sys_desc.user_data = @ptrCast(self);
        sys_desc.schedule = c.BAM_SCHEDULE_UPDATE;

        try checkBamResult(register_system(load_ctx, &sys_desc));

        // Optionally bind `ZigActorStats` + `ZigBehaviorMode` via `comptime` generated Views.
        if (resolve_component(host_ctx, "example::ZigActorStats", null, &self.stats_comp) == c.BAM_RESULT_OK and
            resolve_component(host_ctx, "example::ZigBehaviorMode", null, &self.mode_comp) == c.BAM_RESULT_OK)
        {
            self.stats_vt = try gen.ZigActorStatsView.resolveVTable(
                &self.api,
                host_ctx,
                "bam.v0.example::ZigActorStats.vtable",
            );
            self.mode_vt = try gen.ZigBehaviorModeView.resolveVTable(
                &self.api,
                host_ctx,
                "bam.v0.example::ZigBehaviorMode.vtable",
            );

            const view_terms = [_]c.BamTermDesc{
                c.bam_term_desc(self.stats_comp, c.BAM_ACCESS_WRITE, 0),
                c.bam_term_desc(self.mode_comp, c.BAM_ACCESS_WRITE, 0),
            };
            var view_query = std.mem.zeroes(c.BamQueryDesc);
            view_query.struct_size = @intCast(@sizeOf(c.BamQueryDesc));
            view_query.terms = &view_terms[0];
            view_query.term_count = view_terms.len;

            var view_sys = std.mem.zeroes(c.BamSystemDesc);
            view_sys.struct_size = @intCast(@sizeOf(c.BamSystemDesc));
            view_sys.name = "zig_actor_views";
            view_sys.queries = &view_query;
            view_sys.query_count = 1;
            view_sys.callback = &zigActorViewsCallback;
            view_sys.user_data = @ptrCast(self);
            view_sys.schedule = c.BAM_SCHEDULE_UPDATE;

            try checkBamResult(register_system(load_ctx, &view_sys));
        }
    }

    fn runStep(self: *ZigModInstance, sys_ctx: *c.BamSystemCtx) BamError!void {
        const sys_query = self.api.sys_query orelse return error.NullPointer;
        const iter_next_chunk = self.api.iter_next_chunk orelse return error.NullPointer;
        const iter_col_mut = self.api.iter_column_mutable orelse return error.NullPointer;
        const iter_col_ro = self.api.iter_column_readonly orelse return error.NullPointer;
        const iter_col_info = self.api.iter_column_info orelse return error.NullPointer;
        const res_ptr = self.api.res_ptr orelse return error.NullPointer;

        var iter_opt: ?*c.BamIter = null;
        try checkBamResult(sys_query(sys_ctx, 0, &iter_opt));
        const iter = iter_opt orelse return error.NullPointer;

        const raw_cfg = res_ptr(iter, 2) orelse return error.NullPointer;
        const cfg: *const ZigSimConfig = @ptrCast(@alignCast(raw_cfg));
        if (cfg.trigger_error != c.BAM_FALSE) {
            return error.InvalidArgument;
        }

        var pos_info = std.mem.zeroes(c.BamColumnInfo);
        var vel_info = std.mem.zeroes(c.BamColumnInfo);
        try checkBamResult(iter_col_info(iter, 0, &pos_info));
        try checkBamResult(iter_col_info(iter, 1, &vel_info));
        if (pos_info.size != @sizeOf(Position) or vel_info.size != @sizeOf(Velocity)) {
            return error.TypeMismatch;
        }

        var chunk_len: usize = 0;
        while (iter_next_chunk(iter, &chunk_len) != c.BAM_FALSE) {
            const pos_bytes = iter_col_mut(iter, 0) orelse return error.NullPointer;
            const vel_bytes = iter_col_ro(iter, 1) orelse return error.NullPointer;

            const pos_ptr: [*]Position = @ptrCast(@alignCast(pos_bytes));
            const vel_ptr: [*]const Velocity = @ptrCast(@alignCast(vel_bytes));

            const positions = pos_ptr[0..chunk_len];
            const velocities = vel_ptr[0..chunk_len];

            for (positions, velocities) |*pos, vel| {
                pos.x += vel.x * cfg.dt;
                pos.y += vel.y * cfg.dt;
                pos.z += vel.z * cfg.dt;
            }
        }
    }

    fn runActorViews(self: *ZigModInstance, sys_ctx: *c.BamSystemCtx) BamError!void {
        const stats_vt = self.stats_vt orelse return error.NullPointer;
        const mode_vt = self.mode_vt orelse return error.NullPointer;
        const sys_query = self.api.sys_query orelse return error.NullPointer;
        const iter_next_chunk = self.api.iter_next_chunk orelse return error.NullPointer;
        const iter_col_mut = self.api.iter_column_mutable orelse return error.NullPointer;
        const iter_col_info = self.api.iter_column_info orelse return error.NullPointer;

        var iter_opt: ?*c.BamIter = null;
        try checkBamResult(sys_query(sys_ctx, 0, &iter_opt));
        const iter = iter_opt orelse return error.NullPointer;

        var stats_info = std.mem.zeroes(c.BamColumnInfo);
        var mode_info = std.mem.zeroes(c.BamColumnInfo);
        try checkBamResult(iter_col_info(iter, 0, &stats_info));
        try checkBamResult(iter_col_info(iter, 1, &mode_info));

        var chunk_len: usize = 0;
        while (iter_next_chunk(iter, &chunk_len) != c.BAM_FALSE) {
            const stats_base = iter_col_mut(iter, 0) orelse return error.NullPointer;
            const mode_base = iter_col_mut(iter, 1) orelse return error.NullPointer;

            var i: usize = 0;
            while (i < chunk_len) : (i += 1) {
                const stats_ptr: *anyopaque = @ptrCast(stats_base + i * stats_info.size);
                const mode_ptr: *anyopaque = @ptrCast(mode_base + i * mode_info.size);

                const stats = gen.ZigActorStatsView.init(stats_ptr, stats_vt);
                const mode = gen.ZigBehaviorModeView.init(mode_ptr, mode_vt);

                const scores = stats.scores();
                try checkBamResult(scores.reserve(4));
                try checkBamResult(scores.push(42));
                for (scores.mutSlice()) |*item| {
                    item.* += 1;
                }
                try checkBamResult(stats.set_active(true));
                try checkBamResult(stats.set_label("zig_view_updated"));
                try checkBamResult(stats.set_bonus(.{ .is_some = c.BAM_TRUE, .value = 321 }));

                if (mode.tag() == @intFromEnum(gen.ZigBehaviorModeTag.Idle)) {
                    try checkBamResult(mode.set_patrol(9.5, c.BAM_TRUE));
                } else {
                    try checkBamResult(mode.set_attack(77));
                }
            }
        }
    }
};

fn zigSystemCallback(sys_ctx_opt: ?*c.BamSystemCtx, user_data: ?*anyopaque) callconv(.c) c.BamResult {
    const sys_ctx = sys_ctx_opt orelse return c.BAM_RESULT_NULL_POINTER;
    const raw_ud = user_data orelse return c.BAM_RESULT_NULL_POINTER;
    const self: *ZigModInstance = @ptrCast(@alignCast(raw_ud));
    self.runStep(sys_ctx) catch |err| return mapErrorToBamResult(err);
    return c.BAM_RESULT_OK;
}

fn zigActorViewsCallback(sys_ctx_opt: ?*c.BamSystemCtx, user_data: ?*anyopaque) callconv(.c) c.BamResult {
    const sys_ctx = sys_ctx_opt orelse return c.BAM_RESULT_NULL_POINTER;
    const raw_ud = user_data orelse return c.BAM_RESULT_NULL_POINTER;
    const self: *ZigModInstance = @ptrCast(@alignCast(raw_ud));
    self.runActorViews(sys_ctx) catch |err| return mapErrorToBamResult(err);
    return c.BAM_RESULT_OK;
}

const MANIFEST: c.BamManifest = blk: {
    var m = std.mem.zeroes(c.BamManifest);
    m.struct_size = @intCast(@sizeOf(c.BamManifest));
    m.abi_version = c.BAM_ABI_VERSION;
    m.name = "example_08_zig_mod";
    m.version = "0.2.0";
    break :blk m;
};

export fn bevy_abi_mod_manifest() callconv(.c) *const c.BamManifest {
    return &MANIFEST;
}

fn initImpl(
    host_api_opt: ?*const c.BamHostApi,
    load_ctx_opt: ?*c.BamLoadCtx,
    instance_out_opt: ?*?*c.BamInstance,
) BamError!void {
    const host_api = host_api_opt orelse return error.NullPointer;
    const load_ctx = load_ctx_opt orelse return error.NullPointer;
    const instance_out = instance_out_opt orelse return error.NullPointer;
    instance_out.* = null;

    var local_api = std.mem.zeroes(c.BamHostApi);
    try checkBamResult(c.bam_host_api_copy_bounded(&local_api, @sizeOf(c.BamHostApi), host_api));

    const instance = std.heap.c_allocator.create(ZigModInstance) catch return error.OutOfMemory;
    errdefer std.heap.c_allocator.destroy(instance);

    instance.* = .{
        .api = local_api,
        .pos_comp = c.BAM_COMPONENT_INVALID,
        .vel_comp = c.BAM_COMPONENT_INVALID,
        .cfg_res = c.BAM_COMPONENT_INVALID,
        .stats_comp = c.BAM_COMPONENT_INVALID,
        .mode_comp = c.BAM_COMPONENT_INVALID,
        .stats_vt = null,
        .mode_vt = null,
    };

    try instance.initAndRegister(load_ctx);
    instance_out.* = @ptrCast(instance);
}

export fn bevy_abi_mod_init(
    host_api: ?*const c.BamHostApi,
    load_ctx: ?*c.BamLoadCtx,
    instance_out: ?*?*c.BamInstance,
) callconv(.c) c.BamResult {
    initImpl(host_api, load_ctx, instance_out) catch |err| return mapErrorToBamResult(err);
    return c.BAM_RESULT_OK;
}

export fn bevy_abi_mod_shutdown(instance_opt: ?*c.BamInstance) callconv(.c) void {
    const raw = instance_opt orelse return;
    const instance: *ZigModInstance = @ptrCast(@alignCast(raw));
    std.heap.c_allocator.destroy(instance);
}
