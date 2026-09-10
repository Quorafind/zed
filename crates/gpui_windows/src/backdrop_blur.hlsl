// Backdrop blur: the pane-of-glass pass.
//
// Samples a copy of what has already been drawn into the window and writes a
// blurred version of it back inside one rounded rectangle. Everything painted
// after this pass composites on top, so a caller gets the same effect as CSS
// `backdrop-filter: blur()` — the panel blurs what is *behind* it rather than
// blurring itself.
//
// A separable gaussian would be two passes and a scratch target; this takes a
// single pass with a fixed 13-tap kernel per axis instead. At the radii a glass
// panel asks for, one pass is the cheaper trade and the difference does not
// survive the translucent tint painted over it.

// b2, not b0: gpui binds GlobalParams to b0 and BatchParams to b1 once at
// startup and never rebinds them. A pass that runs mid-scene and takes b0 for
// itself leaves every later batch reading its parameters as globals, which puts
// the whole interface offscreen.
cbuffer BackdropBlurParams: register(b2) {
    // The rounded rect to fill, in pixels, top-left origin.
    float2 backdrop_origin;
    float2 backdrop_size;
    // Corner radius in pixels, already clamped to half the shorter side.
    float backdrop_corner_radius;
    // Gaussian sigma in pixels.
    float backdrop_sigma;
    float2 backdrop_viewport_size;
};

Texture2D<float4> t_window: register(t0);
SamplerState s_window: register(s0);

struct BackdropBlurVertexOutput {
    float4 position: SV_Position;
    float2 texture_coords: TEXCOORD0;
};

BackdropBlurVertexOutput backdrop_blur_vertex(uint vertex_id: SV_VertexID) {
    // Same triangle strip winding as the rest of the gpui pipelines, mapped to
    // the rect rather than the whole window: the pass only ever touches its own
    // bounds, so the cost is the panel's area and not the window's.
    float2 unit_vertex = float2(float(vertex_id & 1u), 0.5 * float(vertex_id & 2u));
    float2 pixel = backdrop_origin + unit_vertex * backdrop_size;
    float2 clip = pixel / backdrop_viewport_size * float2(2.0, -2.0) + float2(-1.0, 1.0);

    BackdropBlurVertexOutput output;
    output.position = float4(clip, 0.0, 1.0);
    output.texture_coords = pixel / backdrop_viewport_size;
    return output;
}

// Signed distance to a rounded rectangle, negative inside. The standard
// quad SDF gpui's own shaders use, so the edge lands on the same pixels a
// `rounded()` quad would cover.
float rounded_rect_sdf(float2 offset, float2 half_size, float radius) {
    float2 inner = half_size - float2(radius, radius);
    float2 d = abs(offset) - inner;
    return length(max(d, 0.0)) + min(max(d.x, d.y), 0.0) - radius;
}

float4 backdrop_blur_fragment(BackdropBlurVertexOutput input): SV_Target {
    float2 pixel = input.texture_coords * backdrop_viewport_size;
    float2 center = backdrop_origin + backdrop_size * 0.5;
    float distance = rounded_rect_sdf(
        pixel - center,
        backdrop_size * 0.5,
        backdrop_corner_radius
    );
    // One pixel of feather so the corner is antialiased rather than stepped.
    float coverage = saturate(0.5 - distance);
    if (coverage <= 0.0) {
        discard;
    }

    float sigma = max(backdrop_sigma, 0.0001);
    float2 texel = 1.0 / backdrop_viewport_size;
    // Taps are spread over +/- 2 sigma, which carries ~95% of the gaussian's
    // mass; past that the weights are below the precision of an 8-bit target.
    float step = sigma * 2.0 / 6.0;

    float4 total = float4(0.0, 0.0, 0.0, 0.0);
    float weight_total = 0.0;
    [unroll]
    for (int x = -6; x <= 6; x += 1) {
        [unroll]
        for (int y = -6; y <= 6; y += 1) {
            float2 offset = float2(float(x), float(y)) * step;
            float weight = exp(-dot(offset, offset) / (2.0 * sigma * sigma));
            float2 coords = input.texture_coords + offset * texel;
            total += t_window.SampleLevel(s_window, saturate(coords), 0.0) * weight;
            weight_total += weight;
        }
    }

    float4 blurred = total / max(weight_total, 0.0001);
    // The snapshot is premultiplied BGRA and the target is opaque here, so the
    // pass writes straight over its bounds; the caller's own tint is a separate
    // quad painted after this one.
    return float4(blurred.rgb, 1.0) * coverage;
}
