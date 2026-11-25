// dxc: -T cs_6_0
groupshared uint sh[64];
RWStructuredBuffer<uint> inb : register(u0);
RWStructuredBuffer<uint> outb : register(u1);
[numthreads(64, 1, 1)]
void main(uint3 gid : SV_GroupID, uint gi : SV_GroupIndex, uint3 id : SV_DispatchThreadID) {
    sh[gi] = inb[id.x];
    GroupMemoryBarrierWithGroupSync();
    for (uint s = 32; s > 0; s >>= 1) {
        if (gi < s) sh[gi] += sh[gi + s];
        GroupMemoryBarrierWithGroupSync();
    }
    if (gi == 0) {
        outb[gid.x] = sh[0];
        InterlockedAdd(outb[255], sh[0]);
    }
}
