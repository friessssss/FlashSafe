// FlashSafe per-pixel filter. Mirrors `flashsafe_core::filter::apply_pixel`;
// see crates/flashsafe-core/src/filter.rs for the model.

struct VSOut {
    float4 pos : SV_POSITION;
    float2 uv : TEXCOORD0;
};

VSOut VSMain(uint vid : SV_VertexID) {
    VSOut o;
    float2 uv = float2((vid == 2) ? 2.0 : 0.0, (vid == 1) ? 2.0 : 0.0);
    o.pos = float4(uv * float2(2.0, -2.0) + float2(-1.0, 1.0), 0.0, 1.0);
    o.uv = uv;
    return o;
}

cbuffer Params : register(b0) {
    float min_gain;
    float stats_level;
    float fade_weight; // 0 = hold current image brighter, 1 = crossfade (screen-wide fades)
    float pad;
};

// Mirrors flashsafe_core::filter::MAX_FALL_GAIN.
static const float MAX_FALL_GAIN = 3.0;

// t0: captured client area (BGRA8, sRGB-encoded values) — or the stats
//     texture in PSDownsample.
// t1: colour displayed last frame (linear RGB).
// t2: per-tile (rise scale, fall scale), TILE_COLS x TILE_ROWS; the linear
//     clamp sampler interpolates between tile centres, matching
//     `flashsafe_core::filter::sample_tile`.
Texture2D src : register(t0);
Texture2D hist : register(t1);
Texture2D<float2> scales : register(t2);
SamplerState samp : register(s0);

static const float3 LUMA = float3(0.2126, 0.7152, 0.0722);

float3 srgb_to_linear(float3 c) {
    return (c <= 0.04045) ? c / 12.92 : pow((c + 0.055) / 1.055, 2.4);
}

float3 linear_to_srgb(float3 c) {
    return (c <= 0.0031308) ? c * 12.92 : 1.055 * pow(c, 1.0 / 2.4) - 0.055;
}

float3 frame_linear(float4 pos) {
    return srgb_to_linear(src.Load(int3(pos.xy, 0)).rgb);
}

// Seed the display history with the current frame.
float4 PSPrime(VSOut i) : SV_TARGET {
    return float4(frame_linear(i.pos), 1.0);
}

// Per-pixel (L, P, max(0, L - P), max(0, P - L)); tile means come from the mip chain.
float4 PSStats(VSOut i) : SV_TARGET {
    float l = dot(frame_linear(i.pos), LUMA);
    float p = dot(hist.Load(int3(i.pos.xy, 0)).rgb, LUMA);
    return float4(l, p, max(0.0, l - p), max(0.0, p - l));
}

// Area-averaged downsample of the stats texture for CPU readback.
float4 PSDownsample(VSOut i) : SV_TARGET {
    return src.SampleLevel(samp, i.uv, stats_level);
}

struct ApplyOut {
    float4 color : SV_TARGET0; // mirror swapchain (sRGB-encoded)
    float4 hist : SV_TARGET1;  // next display history (linear)
};

ApplyOut PSMain(VSOut i) {
    float3 x = frame_linear(i.pos);
    float3 prev = hist.Load(int3(i.pos.xy, 0)).rgb;
    float2 s = scales.Sample(samp, i.uv);
    float l = dot(x, LUMA);
    float p = dot(prev, LUMA);
    float3 o;
    if (l > p) {
        // Brightening: the current picture, dimmed to the allowed luminance.
        float shown = max(p + s.x * (l - p), min_gain * l);
        o = x * (shown / l);
    } else {
        // Darkening: keep the current image, held brighter by a gain, so
        // moving content never leaves trails of the previous frame. Only
        // screen-wide fades crossfade from what was on screen.
        float target = p + s.y * (l - p);
        float max_c = max(x.r, max(x.g, x.b));
        float g = l > 1e-5 ? max(min(min(target / l, MAX_FALL_GAIN), 1.0 / max(max_c, 1e-5)), 1.0) : 1.0;
        float3 held = x * g;
        float3 faded = lerp(prev, x, s.y);
        o = lerp(held, faded, fade_weight);
    }
    ApplyOut r;
    r.color = float4(linear_to_srgb(saturate(o)), 1.0);
    r.hist = float4(o, 1.0);
    return r;
}
