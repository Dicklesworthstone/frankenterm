// Window chrome quads (ft-yccm0.4.7.1): the WebGpu renderer's quad shader
// (frankenterm-gui shader.wgsl, vs_main and fs_main), so the tab bar, modal
// overlays and every other box-model element are drawn by the same math on
// both front ends. Quads are instanced, in paint order; each is a pixel
// rectangle with texture coordinates in the UI atlas, an RGBA8 sRGB texture
// holding the bytes WebGpu's glyph atlas holds.

#include <metal_stdlib>
using namespace metal;

// shader.wgsl's quad kinds.
constant float IS_GLYPH = 0.0;
constant float IS_COLOR_EMOJI = 1.0;
constant float IS_BG_IMAGE = 2.0;
constant float IS_SOLID_COLOR = 3.0;
constant float IS_GRAY_SCALE = 4.0;

struct UiUniforms {
    float2 viewport;            // 0: drawable pixels
    float2 reserved;            // 8
    float4 foreground_text_hsb; // 16: xyz
};

// 96 bytes, as ui_quads.rs writes it.
struct UiQuad {
    float4 rect;     // 0: left, top, right, bottom in drawable pixels
    float4 uv;       // 16: left, top, right, bottom in the atlas
    float4 fg;       // 32: linear light, straight alpha
    float4 alt;      // 48
    float4 hsv_kind; // 64: hue, saturation, brightness, kind
    float4 mix_pad;  // 80: x is the fg/alt mix
};

struct UiVertex {
    float4 position [[position]];
    float2 uv;
    float4 fg;
    float3 hsv;
    float kind;
};

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
    return c.z * mix(K.xxx, clamp(p - K.xxx, float3(0.0), float3(1.0)), c.y);
}

static float4 apply_hsv(float4 c, float3 transform) {
    float3 hsv = rgb2hsv(c.rgb) * transform;
    return float4(hsv2rgb(hsv).rgb, c.a);
}

vertex UiVertex ui_vertex(uint vertex_id [[vertex_id]],
                          uint instance_id [[instance_id]],
                          constant UiUniforms &u [[buffer(0)]],
                          device const UiQuad *quads [[buffer(1)]]) {
    UiQuad quad = quads[instance_id];
    // Strip order: top left, top right, bottom left, bottom right.
    bool right = vertex_id == 1 || vertex_id == 3;
    bool bottom = vertex_id >= 2;
    float2 position = float2(right ? quad.rect.z : quad.rect.x,
                             bottom ? quad.rect.w : quad.rect.y);
    UiVertex out;
    out.position = float4(position.x / u.viewport.x * 2.0 - 1.0,
                          1.0 - position.y / u.viewport.y * 2.0, 0.0, 1.0);
    out.uv = float2(right ? quad.uv.z : quad.uv.x, bottom ? quad.uv.w : quad.uv.y);
    out.fg = mix(quad.fg, quad.alt, quad.mix_pad.x);
    out.hsv = quad.hsv_kind.xyz;
    out.kind = quad.hsv_kind.w;
    return out;
}

constexpr sampler atlas_nearest(filter::nearest, address::clamp_to_edge);
constexpr sampler atlas_linear(filter::linear, address::clamp_to_edge);

// fs_main: straight alpha, blended with source alpha like WebGpu's
// BlendState::ALPHA_BLENDING.
fragment float4 ui_fragment(UiVertex in [[stage_in]],
                            constant UiUniforms &u [[buffer(0)]],
                            texture2d<float> atlas [[texture(0)]]) {
    float4 linear_tex = atlas.sample(atlas_linear, in.uv);
    float4 nearest_tex = atlas.sample(atlas_nearest, in.uv);
    float3 hsv = in.hsv;
    float4 color;
    if (in.kind == IS_SOLID_COLOR) {
        color = in.fg;
    } else if (in.kind == IS_BG_IMAGE) {
        color = linear_tex;
        color.a *= in.fg.a;
    } else if (in.kind == IS_COLOR_EMOJI) {
        color = nearest_tex;
    } else if (in.kind == IS_GRAY_SCALE) {
        color = in.fg;
        color.a *= nearest_tex.a;
    } else {
        // IS_GLYPH: the texture is the coverage, tinted with fg.
        color = in.fg;
        color.a = nearest_tex.a;
        hsv *= u.foreground_text_hsb.xyz;
    }
    return apply_hsv(color, hsv);
}
