//! Lightweight Zig SDK wrapper around the `BamHostApi` C-ABI table.
//!
//! Mirrors `bam::GuestApp`, `bam::SystemContext`, `bam::Chunk`, and
//! `bam::Commands` from the C++17 example SDK using Zig `comptime` generics.

const std = @import("std");
pub const c = @import("bam_c");

pub const Commands = struct {
    api: *const c.BamHostApi,
    cmds: ?*c.BamCommands,

    pub fn valid(self: Commands) bool {
        return self.cmds != null;
    }

    pub fn spawnBatch(
        self: Commands,
        components: []const c.BamComponent,
        column_ptrs: []const ?[*]const u8,
        entity_count: usize,
        out_entities: ?[*]c.BamEntity,
    ) c.BamResult {
        if (components.len != column_ptrs.len) {
            return c.BAM_RESULT_INVALID_ARGUMENT;
        }
        const raw_cmds = self.cmds orelse return c.BAM_RESULT_NULL_POINTER;
        const func = self.api.cmd_spawn_batch orelse return c.BAM_RESULT_NULL_POINTER;
        return func(
            raw_cmds,
            components.ptr,
            @ptrCast(column_ptrs.ptr),
            components.len,
            entity_count,
            out_entities,
        );
    }
};

pub const Chunk = struct {
    api: *const c.BamHostApi,
    iter: *c.BamIter,
    len: usize,

    pub fn entities(self: Chunk) ?[*]const c.BamEntity {
        const func = self.api.iter_entities orelse return null;
        const ptr = func(self.iter) orelse return null;
        return @ptrCast(@alignCast(ptr));
    }

    pub fn columnReadonly(self: Chunk, comptime T: type, term_index: usize) ?[*]const T {
        const func = self.api.iter_column_readonly orelse return null;
        const ptr = func(self.iter, term_index) orelse return null;
        return @ptrCast(@alignCast(ptr));
    }

    pub fn columnMutable(self: Chunk, comptime T: type, term_index: usize) ?[*]T {
        const func = self.api.iter_column_mutable orelse return null;
        const ptr = func(self.iter, term_index) orelse return null;
        return @ptrCast(@alignCast(ptr));
    }

    pub fn hasColumn(self: Chunk, term_index: usize) ?[*]const c.BamBool {
        return self.columnReadonly(c.BamBool, term_index);
    }

    pub fn rowsMutable(self: Chunk, term_index: usize, row_index: usize, elem_size: usize) ?*anyopaque {
        const base = self.columnMutable(u8, term_index) orelse return null;
        return @ptrCast(base + row_index * elem_size);
    }
};

pub const SystemContext = struct {
    api: *const c.BamHostApi,
    ctx: ?*c.BamSystemCtx,

    pub fn init(api: *const c.BamHostApi, ctx: ?*c.BamSystemCtx) SystemContext {
        return .{ .api = api, .ctx = ctx };
    }

    pub fn query(self: SystemContext, query_index: usize, out_iter: *?*c.BamIter) c.BamResult {
        const raw_ctx = self.ctx orelse return c.BAM_RESULT_NULL_POINTER;
        const func = self.api.sys_query orelse return c.BAM_RESULT_NULL_POINTER;
        return func(raw_ctx, query_index, out_iter);
    }

    pub fn columnInfo(self: SystemContext, iter: *c.BamIter, term_index: usize, out_info: *c.BamColumnInfo) c.BamResult {
        const func = self.api.iter_column_info orelse return c.BAM_RESULT_NULL_POINTER;
        return func(iter, term_index, out_info);
    }

    pub fn nextChunk(self: SystemContext, iter: *c.BamIter, out_len: *usize) bool {
        const func = self.api.iter_next_chunk orelse {
            out_len.* = 0;
            return false;
        };
        return func(iter, out_len) != c.BAM_FALSE;
    }

    pub fn resPtr(self: SystemContext, comptime T: type, iter: *c.BamIter, term_index: usize) ?*const T {
        const func = self.api.res_ptr orelse return null;
        const ptr = func(iter, term_index) orelse return null;
        return @ptrCast(@alignCast(ptr));
    }

    pub fn resMutPtr(self: SystemContext, comptime T: type, iter: *c.BamIter, term_index: usize) ?*T {
        const func = self.api.res_mut_ptr orelse return null;
        const ptr = func(iter, term_index) orelse return null;
        return @ptrCast(@alignCast(ptr));
    }

    pub fn commands(self: SystemContext, out_err: ?*c.BamResult) Commands {
        var raw_cmds: ?*c.BamCommands = null;
        var rc: c.BamResult = c.BAM_RESULT_NULL_POINTER;
        if (self.ctx) |raw_ctx| {
            if (self.api.sys_commands) |func| {
                rc = func(raw_ctx, &raw_cmds);
            }
        }
        if (out_err) |err_ptr| {
            err_ptr.* = rc;
        }
        return .{ .api = self.api, .cmds = raw_cmds };
    }
};

pub const GuestApp = struct {
    api: *const c.BamHostApi,
    load_ctx: ?*c.BamLoadCtx,
    host_ctx: ?*const c.BamHostCtx,

    pub fn init(api: *const c.BamHostApi, load_ctx: ?*c.BamLoadCtx) GuestApp {
        var host_ctx: ?*const c.BamHostCtx = null;
        if (load_ctx) |raw_load| {
            if (api.load_host_ctx) |func| {
                host_ctx = func(raw_load);
            }
        }
        return .{
            .api = api,
            .load_ctx = load_ctx,
            .host_ctx = host_ctx,
        };
    }

    pub fn valid(self: GuestApp) bool {
        return self.load_ctx != null and self.host_ctx != null;
    }

    pub fn resolveComponent(self: GuestApp, name: [*:0]const u8, out_comp: *c.BamComponent) c.BamResult {
        const raw_host = self.host_ctx orelse return c.BAM_RESULT_NULL_POINTER;
        const func = self.api.resolve_component orelse return c.BAM_RESULT_NULL_POINTER;
        return func(raw_host, name, null, out_comp);
    }

    pub fn tryResolveComponent(self: GuestApp, name: [*:0]const u8, out_comp: *c.BamComponent) bool {
        return self.resolveComponent(name, out_comp) == c.BAM_RESULT_OK and
            c.bam_component_is_valid(out_comp.*) != c.BAM_FALSE;
    }

    pub fn registerSystem(
        self: GuestApp,
        name: [*:0]const u8,
        terms: []const c.BamTermDesc,
        callback: c.BamSystemFn,
        user_data: ?*anyopaque,
        schedule: c.BamScheduleId,
    ) c.BamResult {
        const raw_load = self.load_ctx orelse return c.BAM_RESULT_NULL_POINTER;
        const func = self.api.register_system orelse return c.BAM_RESULT_NULL_POINTER;

        var query = std.mem.zeroes(c.BamQueryDesc);
        query.struct_size = @sizeOf(c.BamQueryDesc);
        query.terms = if (terms.len > 0) terms.ptr else null;
        query.term_count = terms.len;

        var sys_desc = std.mem.zeroes(c.BamSystemDesc);
        sys_desc.struct_size = @sizeOf(c.BamSystemDesc);
        sys_desc.name = name;
        sys_desc.queries = if (terms.len > 0) &query else null;
        sys_desc.query_count = if (terms.len > 0) 1 else 0;
        sys_desc.callback = callback;
        sys_desc.user_data = user_data;
        sys_desc.schedule = schedule;

        return func(raw_load, &sys_desc);
    }

    pub fn registerLifecycleObserver(
        self: GuestApp,
        name: [*:0]const u8,
        event_kind: c.BamEventKind,
        watched_component: c.BamComponent,
        callback: c.BamObserverFn,
        user_data: ?*anyopaque,
    ) c.BamResult {
        const raw_load = self.load_ctx orelse return c.BAM_RESULT_NULL_POINTER;
        const func = self.api.register_observer orelse return c.BAM_RESULT_NULL_POINTER;

        var watched = [_]c.BamComponent{watched_component};
        var desc = std.mem.zeroes(c.BamObserverDesc);
        desc.struct_size = @sizeOf(c.BamObserverDesc);
        desc.name = name;
        desc.event = c.BAM_COMPONENT_INVALID;
        desc.callback = callback;
        desc.user_data = user_data;
        desc.event_kind = event_kind;
        desc.watched_components = &watched;
        desc.watched_component_count = 1;

        return func(raw_load, &desc);
    }
};
