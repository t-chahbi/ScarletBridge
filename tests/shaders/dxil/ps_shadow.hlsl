// dxc: -T ps_6_0
Texture2D<float> shadow : register(t0);
Texture2DArray<float> shadowArr : register(t1);
SamplerComparisonState cmp : register(s0);
float4 main(float4 pos : SV_Position, float3 uvw : TEXCOORD0) : SV_Target {
    float a = shadow.SampleCmp(cmp, uvw.xy, uvw.z);
    float b = shadow.SampleCmpLevelZero(cmp, uvw.xy, uvw.z);
    float c = shadowArr.SampleCmp(cmp, float3(uvw.xy, 1.0), uvw.z);
    return float4(a, b, c, 1.0);
}
