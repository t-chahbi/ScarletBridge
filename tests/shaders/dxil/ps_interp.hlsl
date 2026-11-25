// dxc: -T ps_6_1
float4 main(float4 pos : SV_Position, float2 uv : TEXCOORD0, float4 col : COLOR0, nointerpolation float4 flat: COLOR1) : SV_Target {
    float4 a = EvaluateAttributeAtSample(col, 1);
    float4 b = EvaluateAttributeSnapped(col, int2(1, 1));
    float4 c = EvaluateAttributeCentroid(col);
    float4 d = GetAttributeAtVertex(flat, 1);
    return a + b + c + d + float4(uv, 0, 0);
}
