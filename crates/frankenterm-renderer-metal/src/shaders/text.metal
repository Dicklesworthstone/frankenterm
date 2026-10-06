// The text pass (ft-yccm0.4.2.3): one instanced draw of every glyph. Each
// CellText instance becomes one quad covering its glyph and its cell span;
// the fragment shader samples the grayscale (tinted with the foreground) or
// the color atlas and draws the underline, overline and strikethrough itself,
// so decorations need no extra quads. `shade_text` in src/cell_text.rs is the
// CPU reference of this shader; keep the two identical.

#include <metal_stdlib>
using namespace metal;

// Byte layout: FrameUniforms::to_bytes in src/frame.rs.
struct FrameUniforms {
    ulong frame;                    // 0
    uint2 viewport;                 // 8
    uint2 grid;                     // 16: rows, cols
    uint2 reserved0;                // 24
    float4 clear;                   // 32: premultiplied default background
    float2 cell_size;               // 48: pixels
    float2 grid_origin;             // 56: pixels (left and top padding)
    uint row_offset;                // 64: ring offset of every row
    uint cursor_shape;              // 68
    uint2 cursor_cell;              // 72
    float4 cursor_color;            // 80
    float cursor_thickness;         // 96
    uint cursor_width;              // 100
    uint2 reserved1;                // 104
    float4 selection_tint;          // 112
    float4 search_tint;             // 128
    float4 current_tint;            // 144
    float underline_position;       // 160: pixels from the cell top
    float line_thickness;           // 164: pixels, every decoration
    float strikethrough_position;   // 168: pixels from the cell top
};

// Byte layout: CellText in src/cell_text.rs (24 bytes).
struct CellText {
    ushort2 grid;        // 0: column, ring row
    ushort2 atlas;       // 4: glyph's top-left texel
    ushort2 size;        // 8: glyph width, height in pixels
    short2 offset;       // 12: glyph top-left from the cell's top-left
    uchar4 fg;           // 16: straight RGBA
    uchar4 decoration;   // 20: underline RGB, flags
};

// CellText flag bits (src/cell_text.rs `flags`).
constant uchar FLAG_ATLAS_MASK = 3;
constant uchar FLAG_ATLAS_GRAY = 1;
constant uchar FLAG_ATLAS_COLOR = 2;
constant uchar FLAG_WIDE = 4;
constant uchar FLAG_UNDERLINE_SHIFT = 3;
constant uchar FLAG_UNDERLINE_MASK = 7;
constant uchar FLAG_STRIKETHROUGH = 64;
constant uchar FLAG_OVERLINE = 128;

// Underline styles (src/cell_text.rs `UnderlineStyle`).
constant uint UNDERLINE_SINGLE = 1;
constant uint UNDERLINE_DOUBLE = 2;
constant uint UNDERLINE_CURLY = 3;
constant uint UNDERLINE_DOTTED = 4;
constant uint UNDERLINE_DASHED = 5;

constant float TAU = 6.28318530717958647692;

struct TextVertex {
    float4 position [[position]];
    float2 span_origin [[flat]];
    float2 span_size [[flat]];
    float2 glyph_origin [[flat]];
    float2 glyph_size [[flat]];
    float2 atlas_origin [[flat]];
    float4 fg [[flat]];
    float3 decoration [[flat]];
    uint flags [[flat]];
};

// Vertices 0..4 of a triangle strip: the corners (0, 0), (1, 0), (0, 1), (1, 1).
vertex TextVertex text_vertex(uint vertex_id [[vertex_id]],
                              uint instance_id [[instance_id]],
                              constant FrameUniforms &u [[buffer(0)]],
                              device const CellText *instances [[buffer(2)]]) {
    CellText cell = instances[instance_id];
    uint rows = max(u.grid.x, 1u);
    uint logical_row = (uint(cell.grid.y) + rows - u.row_offset % rows) % rows;
    uint flags = cell.decoration.a;
    float span_cells = (flags & FLAG_WIDE) != 0 ? 2.0 : 1.0;
    float2 span_origin =
        u.grid_origin + float2(float(cell.grid.x), float(logical_row)) * u.cell_size;
    float2 span_size = float2(span_cells * u.cell_size.x, u.cell_size.y);
    float2 glyph_origin = floor(span_origin) + float2(cell.offset);
    float2 glyph_size = float2(cell.size);
    bool has_glyph = (flags & FLAG_ATLAS_MASK) != 0;
    bool has_decoration = (flags & ~(FLAG_ATLAS_MASK | FLAG_WIDE)) != 0;
    float2 low = has_decoration ? span_origin : glyph_origin;
    float2 high = has_decoration ? span_origin + span_size : glyph_origin + glyph_size;
    if (has_glyph && has_decoration) {
        low = min(low, glyph_origin);
        high = max(high, glyph_origin + glyph_size);
    }
    float2 corner = float2(float(vertex_id & 1), float(vertex_id >> 1));
    float2 pixel = mix(low, high, corner);
    float2 viewport = float2(u.viewport);
    TextVertex out;
    out.position = float4(pixel.x / viewport.x * 2.0 - 1.0, 1.0 - pixel.y / viewport.y * 2.0, 0.0, 1.0);
    out.span_origin = span_origin;
    out.span_size = span_size;
    out.glyph_origin = glyph_origin;
    out.glyph_size = glyph_size;
    out.atlas_origin = float2(cell.atlas);
    out.fg = float4(cell.fg) / 255.0;
    out.decoration = float3(cell.decoration.rgb) / 255.0;
    out.flags = flags;
    return out;
}

static float4 over(float4 top, float4 bottom) {
    return top + bottom * (1.0 - top.a);
}

static bool in_band(float y, float top, float thickness) {
    return y >= top && y < top + thickness;
}

// Whether the underline of `style` covers the point `local` of the cell span
// (pixels from the span's top-left); `x` is the pixel's distance from the
// grid's left edge, so dotted, dashed and curly patterns run on across cells.
static bool underline_covers(uint style, float2 local, float x, constant FrameUniforms &u) {
    float top = u.underline_position;
    float thickness = u.line_thickness;
    float unit = max(thickness, 1.0);
    if (style == UNDERLINE_SINGLE) {
        return in_band(local.y, top, thickness);
    }
    if (style == UNDERLINE_DOUBLE) {
        return in_band(local.y, top, thickness) || in_band(local.y, top - 2.0 * thickness, thickness);
    }
    if (style == UNDERLINE_CURLY) {
        float wave = precise::sin(TAU * x / max(u.cell_size.x, 1.0));
        float center = top + 0.5 * thickness + thickness * wave;
        return abs(local.y - center) < 0.5 * thickness + 0.5;
    }
    if (style == UNDERLINE_DOTTED) {
        return in_band(local.y, top, thickness) && uint(floor(x / unit)) % 2 == 0;
    }
    if (style == UNDERLINE_DASHED) {
        return in_band(local.y, top, thickness) && uint(floor(x / unit)) % 5 < 3;
    }
    return false;
}

fragment float4 text_fragment(TextVertex in [[stage_in]],
                              constant FrameUniforms &u [[buffer(0)]],
                              texture2d<float> gray [[texture(0)]],
                              texture2d<float> color [[texture(1)]]) {
    constexpr sampler texel(coord::pixel, filter::nearest, address::clamp_to_edge);
    float2 p = in.position.xy;
    float alpha = in.fg.a;
    // The underline has its own color (SGR 58); the overline and the
    // strikethrough take the foreground's.
    float4 underline_color = float4(in.decoration * alpha, alpha);
    float4 line_color = float4(in.fg.rgb * alpha, alpha);
    float4 out = float4(0.0);

    float2 local = p - in.span_origin;
    bool in_span = local.x >= 0.0 && local.y >= 0.0
        && local.x < in.span_size.x && local.y < in.span_size.y;
    float x = p.x - u.grid_origin.x;
    uint underline = (in.flags >> FLAG_UNDERLINE_SHIFT) & FLAG_UNDERLINE_MASK;
    if (in_span && underline_covers(underline, local, x, u)) {
        out = underline_color;
    }
    if (in_span && (in.flags & FLAG_OVERLINE) != 0 && in_band(local.y, 0.0, u.line_thickness)) {
        out = over(line_color, out);
    }

    float2 inner = p - in.glyph_origin;
    uint atlas = in.flags & FLAG_ATLAS_MASK;
    if ((atlas == FLAG_ATLAS_GRAY || atlas == FLAG_ATLAS_COLOR)
        && inner.x >= 0.0 && inner.y >= 0.0
        && inner.x < in.glyph_size.x && inner.y < in.glyph_size.y) {
        float2 at = in.atlas_origin + floor(inner) + 0.5;
        float4 glyph;
        if (atlas == FLAG_ATLAS_COLOR) {
            // Color glyphs are premultiplied and keep their own colors.
            glyph = color.sample(texel, at) * alpha;
        } else {
            float coverage = gray.sample(texel, at).r * alpha;
            glyph = float4(in.fg.rgb * coverage, coverage);
        }
        out = over(glyph, out);
    }

    if (in_span && (in.flags & FLAG_STRIKETHROUGH) != 0
        && in_band(local.y, u.strikethrough_position, u.line_thickness)) {
        out = over(line_color, out);
    }
    if (out.a <= 0.0) {
        discard_fragment();
    }
    return out;
}
