// dxc: -T cs_6_0
groupshared float4 tile[128];
RWStructuredBuffer<float4> outb : register(u0);
StructuredBuffer<float4> inb : register(t0);
[numthreads(128, 1, 1)]
void main(uint3 id : SV_DispatchThreadID, uint gi : SV_GroupIndex) {
    tile[gi] = inb[id.x];
    GroupMemoryBarrierWithGroupSync();
    float4 acc = 0;
    for (uint s = 1; s < 128; s <<= 1) {
        acc += tile[(gi + s) & 127];
        GroupMemoryBarrierWithGroupSync();
    }
    AllMemoryBarrier();
    DeviceMemoryBarrier();
    outb[id.x] = acc;
}
