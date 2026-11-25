// dxc: -T ps_6_0
Texture1D<float4> t1d : register(t0);
Texture2DArray<float4> t2a : register(t1);
Texture3D<float4> t3d : register(t2);
TextureCube<float4> tcb : register(t3);
TextureCubeArray<float4> tca : register(t4);
Texture1DArray<float4> t1a : register(t5);
SamplerState s0 : register(s0);
float4 main(float4 pos : SV_Position, float3 uvw : TEXCOORD0) : SV_Target {
    float4 r = t1d.Sample(s0, uvw.x);
    r += t2a.Sample(s0, float3(uvw.xy, 2.0));
    r += t3d.Sample(s0, uvw);
    r += tcb.Sample(s0, uvw);
    r += tca.Sample(s0, float4(uvw, 1.0));
    r += t1a.SampleLevel(s0, float2(uvw.x, 1.0), 0.0);
    uint w, h, d, l;
    t3d.GetDimensions(1, w, h, d, l);
    r.x += w + h + d + l;
    return r;
}
