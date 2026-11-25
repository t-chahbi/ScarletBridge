// dxc: -T ps_6_0
float4 main(float4 pos : SV_Position, uint pid : SV_PrimitiveID, nointerpolation float4 flat : COLOR0,
            centroid float2 cuv : TEXCOORD0, noperspective float2 npuv : TEXCOORD1, uint rt : SV_RenderTargetArrayIndex) : SV_Target {
    return float4(pid, rt, cuv + npuv) + flat + pos;
}
