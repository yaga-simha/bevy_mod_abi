const std = @import("std");

pub fn build(b: *std.Build) void {
    const target = b.standardTargetOptions(.{});
    const optimize = b.standardOptimizeOption(.{});

    const mod = b.createModule(.{
        .root_source_file = b.path("src/root.zig"),
        .target = target,
        .optimize = optimize,
        .link_libc = true,
    });
    mod.addIncludePath(b.path("../../../include"));

    const lib = b.addLibrary(.{
        .linkage = .dynamic,
        .name = "zig_mod",
        .root_module = mod,
    });

    b.installArtifact(lib);
}
