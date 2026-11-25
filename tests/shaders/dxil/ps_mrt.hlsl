// dxc: -T ps_6_0
struct Out { float4 a : SV_Target0; float2 b : SV_Target1; uint c : SV_Target3; float4 d : SV_Target2; };
Out main(float4 pos : SV_Position, float3 n : NORMAL, uint cov : SV_Coverage) {
    Out o;
    o.a = float4(normalize(n), 1);
    o.b = pos.xy;
    o.c = cov;
    o.d = float4(1, 2, 3, 4);
    return o;
}
