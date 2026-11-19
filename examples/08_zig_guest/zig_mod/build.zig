const std = @import("std");

pub fn build(b: *std.Build) void {
    const target = b.standardTargetOptions(.{});
    const optimize = b.standardOptimizeOption(.{});

    const translate_c = b.addTranslateC(.{
        .root_source_file = b.path("../../../include/bevy_abi_mod.h"),
        .target = target,
        .optimize = optimize,
        .link_libc = true,
    });
    translate_c.addIncludePath(b.path("../../../include"));
    const bam_c_mod = translate_c.createModule();

    const mod = b.createModule(.{
        .root_source_file = b.path("src/root.zig"),
        .target = target,
        .optimize = optimize,
        .link_libc = true,
    });
    mod.addImport("bam_c", bam_c_mod);

    const lib = b.addLibrary(.{
        .linkage = .dynamic,
        .name = "zig_mod",
        .root_module = mod,
    });

    b.installArtifact(lib);
}
