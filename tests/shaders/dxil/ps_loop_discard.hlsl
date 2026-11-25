// dxc: -T ps_6_0
Texture2D<float4> t : register(t0);
SamplerState s : register(s0);
cbuffer C : register(b0) { int iters; };
float4 main(float4 pos : SV_Position, float2 uv : TEXCOORD0) : SV_Target {
    float4 acc = 0;
    [loop] for (int i = 0; i < iters; ++i) {
        float4 v = t.SampleLevel(s, uv + i * 0.01, 0);
        if (v.a < 0.01) discard;
        acc += v;
    }
    return acc;
}
