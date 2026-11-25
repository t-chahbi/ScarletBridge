// dxc: -T cs_6_0
RWStructuredBuffer<float4> outbuf : register(u0);
StructuredBuffer<float4> inbuf : register(t0);
cbuffer C : register(b0) { uint count; float scale; };
[numthreads(64, 1, 1)]
void main(uint3 id : SV_DispatchThreadID, uint3 gid : SV_GroupID, uint3 tid : SV_GroupThreadID, uint idx : SV_GroupIndex) {
    if (id.x >= count) return;
    float4 v = inbuf[id.x];
    outbuf[id.x] = v * scale + float4(gid.x, tid.x, idx, 0);
}
