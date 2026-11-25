// dxc: -T cs_6_0
RWByteAddressBuffer bab : register(u0);
RWStructuredBuffer<uint> sb : register(u1);
RWStructuredBuffer<int> sbi : register(u2);
groupshared uint gs[64];
groupshared int gsi;
[numthreads(64, 1, 1)]
void main(uint3 id : SV_DispatchThreadID, uint gi : SV_GroupIndex) {
    uint old;
    bab.InterlockedAdd(0, 1, old);
    bab.InterlockedMax(4, id.x);
    bab.InterlockedMin(8, id.x);
    bab.InterlockedAnd(12, 0xff);
    bab.InterlockedOr(16, 1);
    bab.InterlockedXor(20, id.x);
    bab.InterlockedExchange(24, id.x, old);
    bab.InterlockedCompareExchange(28, 5, 6, old);
    InterlockedAdd(sb[id.x], old);
    InterlockedMax(sbi[0], (int)id.x);
    InterlockedMin(sbi[1], -(int)id.x);
    gs[gi] = 0;
    GroupMemoryBarrierWithGroupSync();
    InterlockedAdd(gs[gi & 3], 1);
    InterlockedAdd(gsi, 2);
    GroupMemoryBarrierWithGroupSync();
    sb[gi] = gs[gi] + gsi;
}
