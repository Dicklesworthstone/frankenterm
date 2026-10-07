// The background pass (ft-yccm0.4.2.2): one full-screen triangle whose
// fragment shader finds the cell under each pixel in the CellBg ring buffer
// and composites its color, the selection and search tints and the cursor.
// No per-cell geometry. `shade_background` in src/cell_bg.rs is the CPU
// reference of this shader; keep the two identical.

#include <metal_stdlib>
using namespace metal;

// Byte layout: FrameUniforms::to_bytes in src/frame.rs.
struct FrameUniforms {
    ulong frame;             // 0
    uint2 viewport;          // 8
    uint2 grid;              // 16: rows, cols
    uint2 reserved0;         // 24
    float4 clear;            // 32: premultiplied default background
    float2 cell_size;        // 48: pixels
    float2 grid_origin;      // 56: pixels (left and top padding)
    uint row_offset;         // 64: CellBg ring offset
    uint cursor_shape;       // 68: 0 hidden, 1 block, 2 hollow, 3 underline, 4 bar
    uint2 cursor_cell;       // 72: col, row
    float4 cursor_color;     // 80: premultiplied
    float cursor_thickness;  // 96: pixels
    uint cursor_width;       // 100: cells
    uint2 reserved1;         // 104
    float4 selection_tint;   // 112: premultiplied overlays
    float4 search_tint;      // 128
    float4 current_tint;     // 144
    float4 reserved2;        // 160: the text pass's decoration geometry
    float4 hsb;              // 176: inactive-pane dimming when w != 0
};

// CellBg flag bits (src/cell_bg.rs `flags`).
constant uchar FLAG_COLOR = 1;
constant uchar FLAG_SELECTED = 2;
constant uchar FLAG_SEARCH_MATCH = 4;
constant uchar FLAG_CURRENT_MATCH = 8;

struct BackgroundVertex {
    float4 position [[position]];
};

// Vertices (-1, -1), (3, -1), (-1, 3): one triangle covering the viewport.
vertex BackgroundVertex bg_vertex(uint vertex_id [[vertex_id]]) {
    float2 corner = float2(float((vertex_id << 1) & 2), float(vertex_id & 2));
    BackgroundVertex out;
    out.position = float4(corner * 2.0 - 1.0, 0.0, 1.0);
    return out;
}

static float4 over(float4 top, float4 bottom) {
    return top + bottom * (1.0 - top.a);
}

// Inactive-pane dimming (ft-yccm0.4.6): the WebGpu renderer's apply_hsv,
// on a premultiplied color. `apply_hsb` in src/cell_bg.rs is the CPU
// reference; keep the three identical.
static float3 rgb2hsv(float3 c) {
    float4 K = float4(0.0, -1.0 / 3.0, 2.0 / 3.0, -1.0);
    float4 p = mix(float4(c.bg, K.wz), float4(c.gb, K.xy), step(c.b, c.g));
    float4 q = mix(float4(p.xyw, c.r), float4(c.r, p.yzx), step(p.x, c.r));
    float d = q.x - min(q.w, q.y);
    float e = 1.0e-10;
    return float3(abs(q.z + (q.w - q.y) / (6.0 * d + e)), d / (q.x + e), q.x);
}

static float3 hsv2rgb(float3 c) {
    float4 K = float4(1.0, 2.0 / 3.0, 1.0 / 3.0, 3.0);
    float3 p = abs(fract(c.xxx + K.xyz) * 6.0 - K.www);
    return c.z * mix(K.xxx, clamp(p - K.xxx, 0.0, 1.0), c.y);
}

static float4 apply_hsb(float4 color, float4 hsb) {
    if (hsb.w == 0.0 || color.a <= 0.0) {
        return color;
    }
    float3 hsv = rgb2hsv(color.rgb / color.a) * hsb.xyz;
    return float4(hsv2rgb(hsv) * color.a, color.a);
}

fragment float4 bg_fragment(BackgroundVertex in [[stage_in]],
                            constant FrameUniforms &u [[buffer(0)]],
                            device const uchar4 *cells [[buffer(1)]]) {
    float2 local = in.position.xy - u.grid_origin;
    if (local.x < 0.0 || local.y < 0.0) {
        discard_fragment();
    }
    uint col = uint(local.x / u.cell_size.x);
    uint row = uint(local.y / u.cell_size.y);
    uint rows = u.grid.x;
    uint cols = u.grid.y;
    // Padding right of and below the grid, and the scrollbar, keep the
    // cleared default background.
    if (col >= cols || row >= rows) {
        discard_fragment();
    }
    uint ring_row = (row + u.row_offset) % rows;
    uchar4 cell = cells[ring_row * cols + col];
    float4 color = (cell.a & FLAG_COLOR) != 0
        ? float4(float3(cell.rgb) / 255.0, 1.0)
        : u.clear;
    if ((cell.a & FLAG_SELECTED) != 0) {
        color = over(u.selection_tint, color);
    }
    if ((cell.a & FLAG_CURRENT_MATCH) != 0) {
        color = over(u.current_tint, color);
    } else if ((cell.a & FLAG_SEARCH_MATCH) != 0) {
        color = over(u.search_tint, color);
    }
    if (u.cursor_shape != 0 && row == u.cursor_cell.y && col >= u.cursor_cell.x
        && col < u.cursor_cell.x + u.cursor_width) {
        float2 inner = local - float2(float(u.cursor_cell.x) * u.cell_size.x,
                                      float(row) * u.cell_size.y);
        float width = float(u.cursor_width) * u.cell_size.x;
        float height = u.cell_size.y;
        float thickness = u.cursor_thickness;
        bool inside = u.cursor_shape == 1
            || (u.cursor_shape == 2
                && (inner.x < thickness || inner.y < thickness
                    || inner.x >= width - thickness || inner.y >= height - thickness))
            || (u.cursor_shape == 3 && inner.y >= height - thickness)
            || (u.cursor_shape == 4 && inner.x < thickness);
        if (inside) {
            color = over(u.cursor_color, color);
        }
    }
    return apply_hsb(color, u.hsb);
}
