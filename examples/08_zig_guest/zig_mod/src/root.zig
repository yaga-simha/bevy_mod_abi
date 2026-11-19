//! Zig Guest Mod for `bevy_abi_mod` Example 08 (`ABI 0.3.0`).
//!
//! Demonstrates:
//! 1. Importing `generated_components.zig` (`ModComponent::ZIG_DECL`) and `bam_zig_sdk.zig`
//!    (`GuestApp`, `SystemContext`, `Chunk`, `Commands`).
//! 2. Exporting `bevy_abi_mod_manifest`, `bevy_abi_mod_init`, and `bevy_abi_mod_shutdown`
//!    with `callconv(.c)`.
//! 3. Mapping typed Zig error unions (`BamError!void`) cleanly to `c.BamResult` status codes
//!    without unwinding across `extern "C"` (`spec.md` §3.4).
//! 4. Per-`World` `ZigModInstance` allocated on `std.heap.c_allocator` (`*instance_out`)
//!    performing zero-copy SoA chunk iteration over `example::Position` and `example::Velocity`.
//! 5. Using Zig `comptime` VTable/view wrappers (`ZigActorStatsView`, `VecView`, `ZigBehaviorModeView`)
//!    when `example::ZigActorStats` and `example::ZigBehaviorMode` are registered by the host.
//! 6. Optionally exercising `BAM_EVENT_KIND_ADD` lifecycle observers, `BAM_ACCESS_HAS`,
//!    `BAM_CHILD_OF_NAME` optional hierarchy reads, and `cmd_spawn_batch` SoA bulk spawning.

const std = @import("std");
const gen = @import("generated_components.zig");
const sdk = @import("bam_zig_sdk.zig");
const c = gen.bam;

pub const Position = gen.Position;
pub const Velocity = gen.Velocity;
pub const ZigSimConfig = gen.ZigSimConfig;
pub const ZigTelemetry = gen.ZigTelemetry;

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
    if (@sizeOf(ZigTelemetry) != 24 or @alignOf(ZigTelemetry) != 4) {
        @compileError("ZigTelemetry layout mismatch");
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
    child_of_comp: c.BamComponent,
    telemetry_res: c.BamComponent,
    stats_vt: ?*const gen.ZigActorStatsVTable,
    mode_vt: ?*const gen.ZigBehaviorModeVTable,
    observed_adds: u32,

    fn initAndRegister(self: *ZigModInstance, load_ctx: *c.BamLoadCtx) BamError!void {
        const app = sdk.GuestApp.init(&self.api, load_ctx);
        if (!app.valid()) {
            return error.NullPointer;
        }
        const host_ctx = app.host_ctx orelse return error.NullPointer;

        try checkBamResult(app.resolveComponent("example::Position", &self.pos_comp));
        try checkBamResult(app.resolveComponent("example::Velocity", &self.vel_comp));
        try checkBamResult(app.resolveComponent("example::ZigSimConfig", &self.cfg_res));

        const terms = [_]c.BamTermDesc{
            c.bam_term_desc(self.pos_comp, c.BAM_ACCESS_WRITE, 0),
            c.bam_term_desc(self.vel_comp, c.BAM_ACCESS_READ, 0),
            c.bam_term_desc(self.cfg_res, c.BAM_ACCESS_RESOURCE_READ, 0),
        };
        try checkBamResult(app.registerSystem(
            "zig_orbital_step",
            &terms,
            &zigSystemCallback,
            @ptrCast(self),
            c.BAM_SCHEDULE_UPDATE,
        ));

        // Optionally bind `ZigActorStats` + `ZigBehaviorMode` via `comptime` generated Views.
        if (app.tryResolveComponent("example::ZigActorStats", &self.stats_comp) and
            app.tryResolveComponent("example::ZigBehaviorMode", &self.mode_comp))
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
            try checkBamResult(app.registerSystem(
                "zig_actor_views",
                &view_terms,
                &zigActorViewsCallback,
                @ptrCast(self),
                c.BAM_SCHEDULE_UPDATE,
            ));

            // Bind `BAM_EVENT_KIND_ADD` lifecycle observer,
            // `BAM_ACCESS_HAS`, `BamChildOf` optional hierarchy read, and `cmd_spawn_batch`.
            if (app.tryResolveComponent("example::ZigTelemetry", &self.telemetry_res) and
                app.tryResolveComponent(c.BAM_CHILD_OF_NAME, &self.child_of_comp))
            {
                try checkBamResult(app.registerLifecycleObserver(
                    "zig_on_actor_added",
                    c.BAM_EVENT_KIND_ADD,
                    self.stats_comp,
                    &zigOnActorAddedCallback,
                    @ptrCast(self),
                ));

                const has_terms = [_]c.BamTermDesc{
                    c.bam_term_desc(self.pos_comp, c.BAM_ACCESS_READ, 0),
                    c.bam_term_desc(self.stats_comp, c.BAM_ACCESS_HAS, 0),
                    c.bam_term_desc(self.child_of_comp, c.BAM_ACCESS_OPTIONAL_READ, 0),
                    c.bam_term_desc(self.telemetry_res, c.BAM_ACCESS_RESOURCE_WRITE, 0),
                };
                try checkBamResult(app.registerSystem(
                    "zig_has_hierarchy_and_batch",
                    &has_terms,
                    &zigHasHierarchyCallback,
                    @ptrCast(self),
                    c.BAM_SCHEDULE_UPDATE,
                ));
            }
        }
    }

    fn runStep(self: *ZigModInstance, raw_ctx: *c.BamSystemCtx) BamError!void {
        const ctx = sdk.SystemContext.init(&self.api, raw_ctx);
        var iter_opt: ?*c.BamIter = null;
        try checkBamResult(ctx.query(0, &iter_opt));
        const iter = iter_opt orelse return error.NullPointer;

        const cfg = ctx.resPtr(ZigSimConfig, iter, 2) orelse return error.NullPointer;
        if (cfg.trigger_error != c.BAM_FALSE) {
            return error.InvalidArgument;
        }

        var pos_info = std.mem.zeroes(c.BamColumnInfo);
        var vel_info = std.mem.zeroes(c.BamColumnInfo);
        try checkBamResult(ctx.columnInfo(iter, 0, &pos_info));
        try checkBamResult(ctx.columnInfo(iter, 1, &vel_info));
        if (pos_info.size != @sizeOf(Position) or vel_info.size != @sizeOf(Velocity)) {
            return error.TypeMismatch;
        }

        var chunk_len: usize = 0;
        while (ctx.nextChunk(iter, &chunk_len)) {
            const chunk = sdk.Chunk{ .api = &self.api, .iter = iter, .len = chunk_len };
            const pos_ptr = chunk.columnMutable(Position, 0) orelse return error.NullPointer;
            const vel_ptr = chunk.columnReadonly(Velocity, 1) orelse return error.NullPointer;

            const positions = pos_ptr[0..chunk.len];
            const velocities = vel_ptr[0..chunk.len];

            for (positions, velocities) |*pos, vel| {
                pos.x += vel.x * cfg.dt;
                pos.y += vel.y * cfg.dt;
                pos.z += vel.z * cfg.dt;
            }
        }
    }

    fn runActorViews(self: *ZigModInstance, raw_ctx: *c.BamSystemCtx) BamError!void {
        const stats_vt = self.stats_vt orelse return error.NullPointer;
        const mode_vt = self.mode_vt orelse return error.NullPointer;
        const ctx = sdk.SystemContext.init(&self.api, raw_ctx);

        var iter_opt: ?*c.BamIter = null;
        try checkBamResult(ctx.query(0, &iter_opt));
        const iter = iter_opt orelse return error.NullPointer;

        var stats_info = std.mem.zeroes(c.BamColumnInfo);
        var mode_info = std.mem.zeroes(c.BamColumnInfo);
        try checkBamResult(ctx.columnInfo(iter, 0, &stats_info));
        try checkBamResult(ctx.columnInfo(iter, 1, &mode_info));

        var chunk_len: usize = 0;
        while (ctx.nextChunk(iter, &chunk_len)) {
            const chunk = sdk.Chunk{ .api = &self.api, .iter = iter, .len = chunk_len };
            var i: usize = 0;
            while (i < chunk.len) : (i += 1) {
                const stats_ptr = chunk.rowsMutable(0, i, stats_info.size) orelse return error.NullPointer;
                const mode_ptr = chunk.rowsMutable(1, i, mode_info.size) orelse return error.NullPointer;

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

    fn runHasHierarchyAndBatch(self: *ZigModInstance, raw_ctx: *c.BamSystemCtx) BamError!void {
        const ctx = sdk.SystemContext.init(&self.api, raw_ctx);
        var iter_opt: ?*c.BamIter = null;
        try checkBamResult(ctx.query(0, &iter_opt));
        const iter = iter_opt orelse return error.NullPointer;

        const telemetry = ctx.resMutPtr(ZigTelemetry, iter, 3) orelse return error.NullPointer;

        var with_stats: u32 = 0;
        var without_stats: u32 = 0;
        var children_seen: u32 = 0;

        var chunk_len: usize = 0;
        while (ctx.nextChunk(iter, &chunk_len)) {
            const chunk = sdk.Chunk{ .api = &self.api, .iter = iter, .len = chunk_len };
            const has_stats = chunk.hasColumn(1) orelse return error.NullPointer;
            const child_ofs = chunk.columnReadonly(c.BamChildOf, 2);

            var i: usize = 0;
            while (i < chunk.len) : (i += 1) {
                if (has_stats[i] != c.BAM_FALSE) {
                    with_stats += 1;
                } else {
                    without_stats += 1;
                }
                if (child_ofs) |parents| {
                    if (parents[i].parent != c.BAM_ENTITY_NULL) {
                        children_seen += 1;
                    }
                }
            }
        }

        telemetry.observed_adds = self.observed_adds;
        telemetry.entities_with_stats = with_stats;
        telemetry.entities_without_stats = without_stats;
        telemetry.child_entities_seen = children_seen;

        if (telemetry.spawn_batch_count > 0 and telemetry.batch_spawned == c.BAM_FALSE) {
            var cmd_res: c.BamResult = c.BAM_RESULT_OK;
            const cmds = ctx.commands(&cmd_res);
            try checkBamResult(cmd_res);
            if (!cmds.valid()) {
                return error.NullPointer;
            }

            const count: usize = @intCast(telemetry.spawn_batch_count);
            const batch_positions = std.heap.c_allocator.alloc(Position, count) catch return error.OutOfMemory;
            defer std.heap.c_allocator.free(batch_positions);

            for (batch_positions, 0..) |*pos, idx| {
                pos.* = .{
                    .x = @as(f32, @floatFromInt(idx)) + 1.0,
                    .y = 0.0,
                    .z = 0.0,
                };
            }

            const comps = [_]c.BamComponent{self.pos_comp};
            const cols = [_]?[*]const u8{@ptrCast(batch_positions.ptr)};
            try checkBamResult(cmds.spawnBatch(&comps, &cols, count, null));
            telemetry.batch_spawned = c.BAM_TRUE;
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

fn zigHasHierarchyCallback(sys_ctx_opt: ?*c.BamSystemCtx, user_data: ?*anyopaque) callconv(.c) c.BamResult {
    const sys_ctx = sys_ctx_opt orelse return c.BAM_RESULT_NULL_POINTER;
    const raw_ud = user_data orelse return c.BAM_RESULT_NULL_POINTER;
    const self: *ZigModInstance = @ptrCast(@alignCast(raw_ud));
    self.runHasHierarchyAndBatch(sys_ctx) catch |err| return mapErrorToBamResult(err);
    return c.BAM_RESULT_OK;
}

fn zigOnActorAddedCallback(
    target: c.BamEntity,
    event_data: ?*const anyopaque,
    commands: ?*c.BamCommands,
    user_data: ?*anyopaque,
) callconv(.c) c.BamResult {
    _ = target;
    _ = event_data;
    _ = commands;
    const raw_ud = user_data orelse return c.BAM_RESULT_NULL_POINTER;
    const self: *ZigModInstance = @ptrCast(@alignCast(raw_ud));
    self.observed_adds += 1;
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
        .child_of_comp = c.BAM_COMPONENT_INVALID,
        .telemetry_res = c.BAM_COMPONENT_INVALID,
        .stats_vt = null,
        .mode_vt = null,
        .observed_adds = 0,
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
