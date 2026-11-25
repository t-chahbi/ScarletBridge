// dxc: -T cs_6_6
cbuffer C : register(b0) { uint texIdx; uint bufIdx; uint smpIdx; };
[numthreads(8, 8, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    Texture2D<float4> t = ResourceDescriptorHeap[texIdx];
    SamplerState s = SamplerDescriptorHeap[smpIdx];
    RWStructuredBuffer<float4> ob = ResourceDescriptorHeap[bufIdx];
    ob[id.x] = t.SampleLevel(s, float2(id.xy) / 8.0, 0);
}
