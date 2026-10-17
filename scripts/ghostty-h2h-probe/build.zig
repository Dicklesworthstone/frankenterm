//! Builds ghostty-h2h-probe against the pinned Ghostty checkout's ghostty-vt
//! module (ft-yccm0.1.3). scripts/ghostty-headless-h2h.sh copies this
//! directory into its scratch tree, renders build.zig.zon from
//! build.zig.zon.in with the relative path to the checkout, and runs
//! `zig build -Doptimize=ReleaseFast --prefix ... --cache-dir ...` there, so
//! the Ghostty checkout is only ever read.
const std = @import("std");

pub fn build(b: *std.Build) void {
    const target = b.standardTargetOptions(.{});
    const optimize = b.standardOptimizeOption(.{});

    const exe_mod = b.createModule(.{
        .root_source_file = b.path("src/main.zig"),
        .target = target,
        .optimize = optimize,
    });

    if (b.lazyDependency("ghostty", .{
        .target = target,
        .optimize = optimize,
    })) |dep| {
        exe_mod.addImport("ghostty-vt", dep.module("ghostty-vt"));
    }

    const exe = b.addExecutable(.{
        .name = "ghostty-h2h-probe",
        .root_module = exe_mod,
    });
    b.installArtifact(exe);
}
