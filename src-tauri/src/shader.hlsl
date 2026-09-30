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
Texture2D prev : register(t1);
SamplerState samp : register(s0);

cbuffer Mitigation : register(b0) {
    float strength;
    float exposure;
    float knee;
    float temporal;
    float desat;
    float pad0;
    float pad1;
    float pad2;
};

float3 reinhard_knee(float3 c, float k) {
    float lum = dot(c, float3(0.2126, 0.7152, 0.0722));
    float kl = k * k;
    float num = lum * (1.0 + lum / kl);
    float denom = 1.0 + lum;
    float nl = num / max(denom, 1e-4);
    float s = nl / max(lum, 1e-4);
    return c * s;
}

// Downscale / detection blit (no cbuffer).
float4 PSBlit(VSOut i) : SV_TARGET {
    return src.Sample(samp, i.uv);
}

float4 PSMain(VSOut i) : SV_TARGET {
    float4 s = src.Sample(samp, i.uv);
    float3 c = s.rgb;
    float eff = saturate(strength);
    float3 mit = c * lerp(1.0, exposure, eff);
    mit = reinhard_knee(mit, lerp(1.0, knee, eff));
    float grey = dot(mit, float3(0.3333, 0.3333, 0.3333));
    mit = lerp(float3(grey, grey, grey), mit, saturate(1.0 - desat * strength));
    // Slightly more aggressive blend + extra luma pull when mitigating.
    float mixAmt = saturate(strength * 1.12);
    float3 outc = lerp(c, mit, mixAmt);
    outc *= lerp(1.0, 0.86, saturate(strength * eff));
    float3 p = prev.Sample(samp, i.uv).rgb;
    outc = lerp(outc, p, saturate(temporal * strength));
    return float4(outc, 1.0);
}
