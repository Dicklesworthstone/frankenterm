//! ghostty-h2h-probe (ft-yccm0.1.3): feeds a file through libghostty-vt's
//! readonly terminal stream exactly the way `ghostty-bench +terminal-stream`
//! does (fresh Terminal.init + fullReset, default modes, 64 KiB slices) and
//! prints the final grid geometry as one JSON line.
//!
//! scripts/ghostty-headless-h2h.sh compares that line with FrankenTerm's
//! ingest_throughput JSON on the emoji corpus: if the two engines assign
//! different widths to the same characters, they wrap a different number of
//! rows and stop at a different column, which changes the work each one does.
//!
//! Two terminals see the same bytes:
//! - `unlimited` keeps every row (max_scrollback_bytes = null), so its total
//!   row count is every row the input produced;
//! - `bench` uses Terminal.init's defaults, as ghostty-bench does, so its
//!   total row count is what the timed Ghostty arm actually retains.

const std = @import("std");
const vt = @import("ghostty-vt");

/// PageList logs every page-capacity adjustment at info level; the probe's
/// stderr should carry only real problems.
pub const std_options: std.Options = .{ .log_level = .warn };

const usage =
    \\usage: ghostty-h2h-probe --data=FILE [--terminal-rows=N] [--terminal-cols=N]
    \\
;

/// The read size of ghostty-bench's TerminalStream step (and of the real IO
/// thread's buffer), so parser state crosses slice boundaries at the same
/// offsets as in the timed arm.
const read_chunk = 64 * 1024;

pub fn main(init: std.process.Init) !void {
    const io = init.io;
    const gpa = init.gpa;

    var data_path: ?[]const u8 = null;
    var rows: u16 = 80;
    var cols: u16 = 120;

    var args = try init.minimal.args.iterateAllocator(gpa);
    defer args.deinit();
    _ = args.next();
    while (args.next()) |arg| {
        if (std.mem.startsWith(u8, arg, "--data=")) {
            data_path = arg["--data=".len..];
        } else if (std.mem.startsWith(u8, arg, "--terminal-rows=")) {
            rows = parseDimension(arg["--terminal-rows=".len..]);
        } else if (std.mem.startsWith(u8, arg, "--terminal-cols=")) {
            cols = parseDimension(arg["--terminal-cols=".len..]);
        } else {
            std.debug.print("ghostty-h2h-probe: unknown argument {s}\n" ++ usage, .{arg});
            std.process.exit(2);
        }
    }
    const path = data_path orelse {
        std.debug.print("ghostty-h2h-probe: --data is required\n" ++ usage, .{});
        std.process.exit(2);
    };

    var unlimited: vt.Terminal = try .init(io, gpa, .{
        .cols = cols,
        .rows = rows,
        .max_scrollback_bytes = null,
    });
    defer unlimited.deinit(gpa);
    unlimited.fullReset();

    var bench: vt.Terminal = try .init(io, gpa, .{ .cols = cols, .rows = rows });
    defer bench.deinit(gpa);
    bench.fullReset();

    var unlimited_stream = unlimited.vtStream();
    defer unlimited_stream.deinit();
    var bench_stream = bench.vtStream();
    defer bench_stream.deinit();

    const file = try std.Io.Dir.cwd().openFile(io, path, .{});
    defer file.close(io);
    var file_reader = file.reader(io, &.{});
    const reader = &file_reader.interface;

    var buf: [read_chunk]u8 = undefined;
    var bytes: u64 = 0;
    while (true) {
        const n = try reader.readSliceShort(&buf);
        if (n == 0) break;
        unlimited_stream.nextSlice(buf[0..n]);
        bench_stream.nextSlice(buf[0..n]);
        bytes += n;
    }

    const screen = unlimited.screens.active;
    var wrapped_rows: usize = 0;
    var rows_it = screen.pages.rowIterator(.right_down, .{ .screen = .{} }, null);
    while (rows_it.next()) |pin| {
        if (pin.rowAndCell().row.wrap) wrapped_rows += 1;
    }

    // Streaming, not positional: `File.writer` pwrites at offset 0, which
    // overwrites earlier output when stdout is a regular file opened for
    // appending (the runner's `>>` redirects).
    var out_buf: [1024]u8 = undefined;
    var stdout_writer = std.Io.File.stdout().writerStreaming(io, &out_buf);
    const out = &stdout_writer.interface;
    try out.print(
        "{{\"schema\":\"ft.bench.ghostty-h2h-probe.v1\",\"bytes\":{d},\"rows\":{d},\"cols\":{d}," ++
            "\"read_chunk\":{d},\"cursor_x\":{d},\"cursor_y\":{d},\"pending_wrap\":{s}," ++
            "\"total_rows\":{d},\"wrapped_rows\":{d},\"bench_equivalent_total_rows\":{d}}}\n",
        .{
            bytes,
            rows,
            cols,
            read_chunk,
            screen.cursor.x,
            screen.cursor.y,
            if (screen.cursor.pending_wrap) "true" else "false",
            screen.pages.total_rows,
            wrapped_rows,
            bench.screens.active.pages.total_rows,
        },
    );
    try out.flush();
}

fn parseDimension(text: []const u8) u16 {
    const value = std.fmt.parseInt(u16, text, 10) catch 0;
    if (value == 0) {
        std.debug.print("ghostty-h2h-probe: bad terminal dimension {s}\n" ++ usage, .{text});
        std.process.exit(2);
    }
    return value;
}
