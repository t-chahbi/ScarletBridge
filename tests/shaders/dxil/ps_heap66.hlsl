// dxc: -T ps_6_6
cbuffer C : register(b0) { uint texIdx; uint smpIdx; };
float4 main(float4 pos : SV_Position, float2 uv : TEXCOORD0) : SV_Target {
    Texture2D<float4> t = ResourceDescriptorHeap[texIdx];
    SamplerState s = SamplerDescriptorHeap[smpIdx];
    return t.Sample(s, uv);
}
