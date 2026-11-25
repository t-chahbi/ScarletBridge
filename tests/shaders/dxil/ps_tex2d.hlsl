// dxc: -T ps_6_0
Texture2D<float4> t0 : register(t0);
Texture2D<float4> t1 : register(t1);
SamplerState s0 : register(s0);
SamplerState s1 : register(s1);
float4 main(float4 pos : SV_Position, float2 uv : TEXCOORD0) : SV_Target {
    float4 a = t0.Sample(s0, uv);
    float4 b = t0.SampleLevel(s1, uv, 2.0);
    float4 c = t1.SampleBias(s0, uv, 1.5);
    float4 d = t1.SampleGrad(s1, uv, float2(0.01, 0.0), float2(0.0, 0.01));
    float4 e = t0.Gather(s0, uv);
    float4 f = t0.Load(int3(int2(pos.xy), 0));
    uint w, h, m;
    t0.GetDimensions(0, w, h, m);
    float4 g = t1.Sample(s0, uv, int2(1, -1));
    return a + b + c + d + e + f + g + float4(w, h, m, 0);
}
