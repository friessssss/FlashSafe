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

Texture2D src : register(t0);
// Per-tile gain from the FlashSafe filter (TILE_COLS x TILE_ROWS, R32_FLOAT).
// The linear clamp sampler interpolates between tile centres, matching
// `flashsafe_core::filter::sample_gain`.
Texture2D<float> gains : register(t1);
SamplerState samp : register(s0);

float3 srgb_to_linear(float3 c) {
    return (c <= 0.04045) ? c / 12.92 : pow((c + 0.055) / 1.055, 2.4);
}

float3 linear_to_srgb(float3 c) {
    return (c <= 0.0031308) ? c * 12.92 : 1.055 * pow(c, 1.0 / 2.4) - 0.055;
}

// Downscale blit for the detection texture.
float4 PSBlit(VSOut i) : SV_TARGET {
    return src.Sample(samp, i.uv);
}

// Mirror output: scale linear light by the tile gain (never brightens).
float4 PSMain(VSOut i) : SV_TARGET {
    float3 c = src.Sample(samp, i.uv).rgb;
    float g = saturate(gains.Sample(samp, i.uv));
    if (g < 0.999) {
        c = linear_to_srgb(srgb_to_linear(c) * g);
    }
    return float4(c, 1.0);
}
