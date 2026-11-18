//! AUTO-GENERATED FROM `ModComponent::ZIG_DECL` — DO NOT EDIT BY HAND.

const std = @import("std");
pub const bam = @cImport({
    @cInclude("bevy_abi_mod.h");
});

pub const Position = extern struct {
    x: f32,
    y: f32,
    z: f32,
};

pub const PositionVTable = extern struct {
    header: bam.BamVTableHeader,
    get_x: ?*const fn (?*const anyopaque) callconv(.c) f32,
    set_x: ?*const fn (?*anyopaque, f32) callconv(.c) bam.BamResult,
    get_y: ?*const fn (?*const anyopaque) callconv(.c) f32,
    set_y: ?*const fn (?*anyopaque, f32) callconv(.c) bam.BamResult,
    get_z: ?*const fn (?*const anyopaque) callconv(.c) f32,
    set_z: ?*const fn (?*anyopaque, f32) callconv(.c) bam.BamResult,
};

pub const PositionView = struct {
    ptr: *anyopaque,
    vt: *const PositionVTable,

    comptime {
        if (@offsetOf(PositionVTable, "header") != 0) @compileError("PositionVTable header must be at offset 0");
    }

    pub fn resolveVTable(
        api: *const bam.BamHostApi,
        host_ctx: ?*const bam.BamHostCtx,
        symbol: [*:0]const u8,
    ) !*const PositionVTable {
        const get_proc = api.get_proc_address orelse return error.NullPointer;
        const raw = get_proc(host_ctx, symbol) orelse return error.NotFound;
        const hdr: *const bam.BamVTableHeader = @ptrCast(@alignCast(raw));
        if (hdr.struct_size < @sizeOf(PositionVTable) or bam.bam_version_is_compatible(bam.BAM_ABI_VERSION, hdr.abi_version) == bam.BAM_FALSE) {
            return error.InvalidVersion;
        }
        return @ptrCast(@alignCast(raw));
    }

    pub fn init(ptr: *anyopaque, vt: *const PositionVTable) @This() {
        return .{ .ptr = ptr, .vt = vt };
    }

    pub fn get_x(self: @This()) f32 {
        const f = self.vt.get_x orelse return std.mem.zeroes(f32);
        return f(self.ptr);
    }
    pub fn set_x(self: @This(), val: f32) bam.BamResult {
        const f = self.vt.set_x orelse return bam.BAM_RESULT_NULL_POINTER;
        return f(self.ptr, val);
    }
    pub fn get_y(self: @This()) f32 {
        const f = self.vt.get_y orelse return std.mem.zeroes(f32);
        return f(self.ptr);
    }
    pub fn set_y(self: @This(), val: f32) bam.BamResult {
        const f = self.vt.set_y orelse return bam.BAM_RESULT_NULL_POINTER;
        return f(self.ptr, val);
    }
    pub fn get_z(self: @This()) f32 {
        const f = self.vt.get_z orelse return std.mem.zeroes(f32);
        return f(self.ptr);
    }
    pub fn set_z(self: @This(), val: f32) bam.BamResult {
        const f = self.vt.set_z orelse return bam.BAM_RESULT_NULL_POINTER;
        return f(self.ptr, val);
    }
};


pub const Velocity = extern struct {
    x: f32,
    y: f32,
    z: f32,
};

pub const VelocityVTable = extern struct {
    header: bam.BamVTableHeader,
    get_x: ?*const fn (?*const anyopaque) callconv(.c) f32,
    set_x: ?*const fn (?*anyopaque, f32) callconv(.c) bam.BamResult,
    get_y: ?*const fn (?*const anyopaque) callconv(.c) f32,
    set_y: ?*const fn (?*anyopaque, f32) callconv(.c) bam.BamResult,
    get_z: ?*const fn (?*const anyopaque) callconv(.c) f32,
    set_z: ?*const fn (?*anyopaque, f32) callconv(.c) bam.BamResult,
};

pub const VelocityView = struct {
    ptr: *anyopaque,
    vt: *const VelocityVTable,

    comptime {
        if (@offsetOf(VelocityVTable, "header") != 0) @compileError("VelocityVTable header must be at offset 0");
    }

    pub fn resolveVTable(
        api: *const bam.BamHostApi,
        host_ctx: ?*const bam.BamHostCtx,
        symbol: [*:0]const u8,
    ) !*const VelocityVTable {
        const get_proc = api.get_proc_address orelse return error.NullPointer;
        const raw = get_proc(host_ctx, symbol) orelse return error.NotFound;
        const hdr: *const bam.BamVTableHeader = @ptrCast(@alignCast(raw));
        if (hdr.struct_size < @sizeOf(VelocityVTable) or bam.bam_version_is_compatible(bam.BAM_ABI_VERSION, hdr.abi_version) == bam.BAM_FALSE) {
            return error.InvalidVersion;
        }
        return @ptrCast(@alignCast(raw));
    }

    pub fn init(ptr: *anyopaque, vt: *const VelocityVTable) @This() {
        return .{ .ptr = ptr, .vt = vt };
    }

    pub fn get_x(self: @This()) f32 {
        const f = self.vt.get_x orelse return std.mem.zeroes(f32);
        return f(self.ptr);
    }
    pub fn set_x(self: @This(), val: f32) bam.BamResult {
        const f = self.vt.set_x orelse return bam.BAM_RESULT_NULL_POINTER;
        return f(self.ptr, val);
    }
    pub fn get_y(self: @This()) f32 {
        const f = self.vt.get_y orelse return std.mem.zeroes(f32);
        return f(self.ptr);
    }
    pub fn set_y(self: @This(), val: f32) bam.BamResult {
        const f = self.vt.set_y orelse return bam.BAM_RESULT_NULL_POINTER;
        return f(self.ptr, val);
    }
    pub fn get_z(self: @This()) f32 {
        const f = self.vt.get_z orelse return std.mem.zeroes(f32);
        return f(self.ptr);
    }
    pub fn set_z(self: @This(), val: f32) bam.BamResult {
        const f = self.vt.set_z orelse return bam.BAM_RESULT_NULL_POINTER;
        return f(self.ptr, val);
    }
};


pub const ZigSimConfig = extern struct {
    dt: f32,
    trigger_error: bam.BamBool,
    pad: [3]u8,
};

pub const ZigSimConfigVTable = extern struct {
    header: bam.BamVTableHeader,
    get_dt: ?*const fn (?*const anyopaque) callconv(.c) f32,
    set_dt: ?*const fn (?*anyopaque, f32) callconv(.c) bam.BamResult,
    get_trigger_error: ?*const fn (?*const anyopaque) callconv(.c) bam.BamBool,
    set_trigger_error: ?*const fn (?*anyopaque, bam.BamBool) callconv(.c) bam.BamResult,
    ptr_pad: ?*const fn (?*const anyopaque) callconv(.c) ?*const anyopaque,
    ptr_mut_pad: ?*const fn (?*anyopaque) callconv(.c) ?*anyopaque,
};

pub const ZigSimConfigView = struct {
    ptr: *anyopaque,
    vt: *const ZigSimConfigVTable,

    comptime {
        if (@offsetOf(ZigSimConfigVTable, "header") != 0) @compileError("ZigSimConfigVTable header must be at offset 0");
    }

    pub fn resolveVTable(
        api: *const bam.BamHostApi,
        host_ctx: ?*const bam.BamHostCtx,
        symbol: [*:0]const u8,
    ) !*const ZigSimConfigVTable {
        const get_proc = api.get_proc_address orelse return error.NullPointer;
        const raw = get_proc(host_ctx, symbol) orelse return error.NotFound;
        const hdr: *const bam.BamVTableHeader = @ptrCast(@alignCast(raw));
        if (hdr.struct_size < @sizeOf(ZigSimConfigVTable) or bam.bam_version_is_compatible(bam.BAM_ABI_VERSION, hdr.abi_version) == bam.BAM_FALSE) {
            return error.InvalidVersion;
        }
        return @ptrCast(@alignCast(raw));
    }

    pub fn init(ptr: *anyopaque, vt: *const ZigSimConfigVTable) @This() {
        return .{ .ptr = ptr, .vt = vt };
    }

    pub fn get_dt(self: @This()) f32 {
        const f = self.vt.get_dt orelse return std.mem.zeroes(f32);
        return f(self.ptr);
    }
    pub fn set_dt(self: @This(), val: f32) bam.BamResult {
        const f = self.vt.set_dt orelse return bam.BAM_RESULT_NULL_POINTER;
        return f(self.ptr, val);
    }
    pub fn get_trigger_error(self: @This()) bam.BamBool {
        const f = self.vt.get_trigger_error orelse return std.mem.zeroes(bam.BamBool);
        return f(self.ptr);
    }
    pub fn set_trigger_error(self: @This(), val: bam.BamBool) bam.BamResult {
        const f = self.vt.set_trigger_error orelse return bam.BAM_RESULT_NULL_POINTER;
        return f(self.ptr, val);
    }
    pub fn ptr_pad(self: @This()) ?*const anyopaque {
        const f = self.vt.ptr_pad orelse return null;
        return f(self.ptr);
    }
    pub fn ptr_mut_pad(self: @This()) ?*anyopaque {
        const f = self.vt.ptr_mut_pad orelse return null;
        return f(self.ptr);
    }
};


pub const ZigActorStats_bonus_Option = extern struct {
    is_some: bam.BamBool,
    value: u32,
};

pub const ZigActorStats = opaque {};

pub const ZigActorStatsVTable_scores_Slice = extern struct {
    ptr: ?[*]const u32,
    len: usize,
};

pub const ZigActorStatsVTable = extern struct {
    header: bam.BamVTableHeader,
    get_active: ?*const fn (?*const anyopaque) callconv(.c) bam.BamBool,
    set_active: ?*const fn (?*anyopaque, bam.BamBool) callconv(.c) bam.BamResult,
    get_label: ?*const fn (?*const anyopaque) callconv(.c) bam.BamStrView,
    set_label: ?*const fn (?*anyopaque, bam.BamStrView) callconv(.c) bam.BamResult,
    get_scores: ?*const fn (?*const anyopaque) callconv(.c) ZigActorStatsVTable_scores_Slice,
    set_scores: ?*const fn (?*anyopaque, ZigActorStatsVTable_scores_Slice) callconv(.c) bam.BamResult,
    len_scores: ?*const fn (?*const anyopaque) callconv(.c) usize,
    clear_scores: ?*const fn (?*anyopaque) callconv(.c) bam.BamResult,
    push_scores: ?*const fn (?*anyopaque, u32) callconv(.c) bam.BamResult,
    pop_scores: ?*const fn (?*anyopaque, ?*u32) callconv(.c) bam.BamBool,
    remove_scores: ?*const fn (?*anyopaque, usize, ?*u32) callconv(.c) bam.BamResult,
    swap_remove_scores: ?*const fn (?*anyopaque, usize, ?*u32) callconv(.c) bam.BamResult,
    truncate_scores: ?*const fn (?*anyopaque, usize) callconv(.c) bam.BamResult,
    reserve_scores: ?*const fn (?*anyopaque, usize) callconv(.c) bam.BamResult,
    as_mut_slice_scores: ?*const fn (?*anyopaque, ?*usize) callconv(.c) ?[*]u32,
    get_bonus: ?*const fn (?*const anyopaque) callconv(.c) ZigActorStats_bonus_Option,
    set_bonus: ?*const fn (?*anyopaque, ZigActorStats_bonus_Option) callconv(.c) bam.BamResult,
    clear_bonus: ?*const fn (?*anyopaque) callconv(.c) bam.BamResult,
};

pub const ZigActorStatsVTable_scores_VecView = struct {
    ptr: *anyopaque,
    vt: *const ZigActorStatsVTable,

    pub fn len(self: @This()) usize {
        const f = self.vt.len_scores orelse return 0;
        return f(self.ptr);
    }
    pub fn isEmpty(self: @This()) bool {
        return self.len() == 0;
    }
    pub fn slice(self: @This()) []const u32 {
        const f = self.vt.get_scores orelse return &[_]u32{};
        const s = f(self.ptr);
        const p = s.ptr orelse return &[_]u32{};
        return p[0..s.len];
    }
    pub fn mutSlice(self: @This()) []u32 {
        const f = self.vt.as_mut_slice_scores orelse return &[_]u32{};
        var n: usize = 0;
        const p = f(self.ptr, &n) orelse return &[_]u32{};
        return p[0..n];
    }
    pub fn set(self: @This(), items: []const u32) bam.BamResult {
        const f = self.vt.set_scores orelse return bam.BAM_RESULT_NULL_POINTER;
        return f(self.ptr, ZigActorStatsVTable_scores_Slice{ .ptr = if (items.len == 0) null else items.ptr, .len = items.len });
    }
    pub fn clear(self: @This()) bam.BamResult {
        const f = self.vt.clear_scores orelse return bam.BAM_RESULT_NULL_POINTER;
        return f(self.ptr);
    }
    pub fn push(self: @This(), val: u32) bam.BamResult {
        const f = self.vt.push_scores orelse return bam.BAM_RESULT_NULL_POINTER;
        return f(self.ptr, val);
    }
    pub fn pop(self: @This(), out_elem: *u32) bool {
        const f = self.vt.pop_scores orelse return false;
        return f(self.ptr, out_elem) != bam.BAM_FALSE;
    }
    pub fn remove(self: @This(), index: usize, out_elem: *u32) bam.BamResult {
        const f = self.vt.remove_scores orelse return bam.BAM_RESULT_NULL_POINTER;
        return f(self.ptr, index, out_elem);
    }
    pub fn swapRemove(self: @This(), index: usize, out_elem: *u32) bam.BamResult {
        const f = self.vt.swap_remove_scores orelse return bam.BAM_RESULT_NULL_POINTER;
        return f(self.ptr, index, out_elem);
    }
    pub fn truncate(self: @This(), new_len: usize) bam.BamResult {
        const f = self.vt.truncate_scores orelse return bam.BAM_RESULT_NULL_POINTER;
        return f(self.ptr, new_len);
    }
    pub fn reserve(self: @This(), additional: usize) bam.BamResult {
        const f = self.vt.reserve_scores orelse return bam.BAM_RESULT_NULL_POINTER;
        return f(self.ptr, additional);
    }
};

pub const ZigActorStatsView = struct {
    ptr: *anyopaque,
    vt: *const ZigActorStatsVTable,

    comptime {
        if (@offsetOf(ZigActorStatsVTable, "header") != 0) @compileError("ZigActorStatsVTable header must be at offset 0");
    }

    pub fn resolveVTable(
        api: *const bam.BamHostApi,
        host_ctx: ?*const bam.BamHostCtx,
        symbol: [*:0]const u8,
    ) !*const ZigActorStatsVTable {
        const get_proc = api.get_proc_address orelse return error.NullPointer;
        const raw = get_proc(host_ctx, symbol) orelse return error.NotFound;
        const hdr: *const bam.BamVTableHeader = @ptrCast(@alignCast(raw));
        if (hdr.struct_size < @sizeOf(ZigActorStatsVTable) or bam.bam_version_is_compatible(bam.BAM_ABI_VERSION, hdr.abi_version) == bam.BAM_FALSE) {
            return error.InvalidVersion;
        }
        return @ptrCast(@alignCast(raw));
    }

    pub fn init(ptr: *anyopaque, vt: *const ZigActorStatsVTable) @This() {
        return .{ .ptr = ptr, .vt = vt };
    }

    pub fn get_active(self: @This()) bool {
        const f = self.vt.get_active orelse return false;
        return f(self.ptr) != bam.BAM_FALSE;
    }
    pub fn set_active(self: @This(), val: bool) bam.BamResult {
        const f = self.vt.set_active orelse return bam.BAM_RESULT_NULL_POINTER;
        return f(self.ptr, if (val) bam.BAM_TRUE else bam.BAM_FALSE);
    }
    pub fn get_label(self: @This()) []const u8 {
        const f = self.vt.get_label orelse return &[_]u8{};
        const v = f(self.ptr);
        const p = v.ptr orelse return &[_]u8{};
        return p[0..v.len];
    }
    pub fn set_label(self: @This(), text: []const u8) bam.BamResult {
        const f = self.vt.set_label orelse return bam.BAM_RESULT_NULL_POINTER;
        return f(self.ptr, .{ .ptr = if (text.len == 0) null else text.ptr, .len = text.len });
    }
    pub fn scores(self: @This()) ZigActorStatsVTable_scores_VecView {
        return .{ .ptr = self.ptr, .vt = self.vt };
    }
    pub fn get_bonus(self: @This()) ZigActorStats_bonus_Option {
        const f = self.vt.get_bonus orelse return std.mem.zeroes(ZigActorStats_bonus_Option);
        return f(self.ptr);
    }
    pub fn set_bonus(self: @This(), val: ZigActorStats_bonus_Option) bam.BamResult {
        const f = self.vt.set_bonus orelse return bam.BAM_RESULT_NULL_POINTER;
        return f(self.ptr, val);
    }
    pub fn clear_bonus(self: @This()) bam.BamResult {
        const f = self.vt.clear_bonus orelse return bam.BAM_RESULT_NULL_POINTER;
        return f(self.ptr);
    }
};


pub const ZigBehaviorMode = opaque {};
pub const ZigBehaviorModeTag = enum(u8) {
    Idle = 0,
    Patrol = 1,
    Attack = 2,
};

pub const ZigBehaviorModeVTable = extern struct {
    header: bam.BamVTableHeader,
    tag: ?*const fn (?*const anyopaque) callconv(.c) u8,
    set_idle: ?*const fn (?*anyopaque) callconv(.c) bam.BamResult,
    set_patrol: ?*const fn (?*anyopaque, f32, bam.BamBool) callconv(.c) bam.BamResult,
    try_get_patrol_speed: ?*const fn (?*const anyopaque, ?*f32) callconv(.c) bam.BamResult,
    try_get_patrol_aggressive: ?*const fn (?*const anyopaque, ?*bam.BamBool) callconv(.c) bam.BamResult,
    set_attack: ?*const fn (?*anyopaque, u32) callconv(.c) bam.BamResult,
    try_get_attack_0: ?*const fn (?*const anyopaque, ?*u32) callconv(.c) bam.BamResult,
};

pub const ZigBehaviorModeView = struct {
    ptr: *anyopaque,
    vt: *const ZigBehaviorModeVTable,

    comptime {
        if (@offsetOf(ZigBehaviorModeVTable, "header") != 0) @compileError("ZigBehaviorModeVTable header must be at offset 0");
    }

    pub fn resolveVTable(
        api: *const bam.BamHostApi,
        host_ctx: ?*const bam.BamHostCtx,
        symbol: [*:0]const u8,
    ) !*const ZigBehaviorModeVTable {
        const get_proc = api.get_proc_address orelse return error.NullPointer;
        const raw = get_proc(host_ctx, symbol) orelse return error.NotFound;
        const hdr: *const bam.BamVTableHeader = @ptrCast(@alignCast(raw));
        if (hdr.struct_size < @sizeOf(ZigBehaviorModeVTable) or bam.bam_version_is_compatible(bam.BAM_ABI_VERSION, hdr.abi_version) == bam.BAM_FALSE) {
            return error.InvalidVersion;
        }
        return @ptrCast(@alignCast(raw));
    }

    pub fn init(ptr: *anyopaque, vt: *const ZigBehaviorModeVTable) @This() {
        return .{ .ptr = ptr, .vt = vt };
    }

    pub fn tag(self: @This()) u8 {
        const f = self.vt.tag orelse return 0;
        return f(self.ptr);
    }
    pub fn set_idle(self: @This()) bam.BamResult {
        const f = self.vt.set_idle orelse return bam.BAM_RESULT_NULL_POINTER;
        return f(self.ptr);
    }
    pub fn set_patrol(self: @This(), speed: f32, aggressive: bam.BamBool) bam.BamResult {
        const f = self.vt.set_patrol orelse return bam.BAM_RESULT_NULL_POINTER;
        return f(self.ptr, speed, aggressive);
    }
    pub fn try_get_patrol_speed(self: @This(), out_val: *f32) bam.BamResult {
        const f = self.vt.try_get_patrol_speed orelse return bam.BAM_RESULT_NULL_POINTER;
        return f(self.ptr, out_val);
    }
    pub fn try_get_patrol_aggressive(self: @This(), out_val: *bam.BamBool) bam.BamResult {
        const f = self.vt.try_get_patrol_aggressive orelse return bam.BAM_RESULT_NULL_POINTER;
        return f(self.ptr, out_val);
    }
    pub fn set_attack(self: @This(), _0: u32) bam.BamResult {
        const f = self.vt.set_attack orelse return bam.BAM_RESULT_NULL_POINTER;
        return f(self.ptr, _0);
    }
    pub fn try_get_attack_0(self: @This(), out_val: *u32) bam.BamResult {
        const f = self.vt.try_get_attack_0 orelse return bam.BAM_RESULT_NULL_POINTER;
        return f(self.ptr, out_val);
    }
};


