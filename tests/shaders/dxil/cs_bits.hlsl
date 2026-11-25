// dxc: -T cs_6_0
RWStructuredBuffer<uint4> o : register(u0);
StructuredBuffer<uint4> i : register(t0);
[numthreads(32, 1, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    uint4 v = i[id.x];
    uint4 r;
    r.x = reversebits(v.x) ^ countbits(v.y);
    r.y = (uint)firstbitlow(v.z) + (uint)firstbithigh(v.w);
    r.z = (v.x << 3) | (v.y >> 2) & ~v.z;
    int sv = (int)v.x;
    r.w = (uint)firstbithigh(sv) + (uint)(sv >> 4) + (v.x & 0xF0F0);
    o[id.x] = r;
    uint4 rr;
    rr.x = v.x * v.y + v.z / (v.w | 1) + v.x % (v.y | 1);
    rr.y = (uint)((int)v.x / ((int)v.y | 1));
    rr.z = (uint)((int)v.x % ((int)v.y | 1));
    rr.w = abs((int)v.x) + max((int)v.y, (int)v.z) + min(v.z, v.w) + max(v.x, v.y);
    o[id.x + 32] = rr;
    uint hi, lo;
    uint ex = (v.x >> 4) & 0xFF;
    o[id.x + 64] = uint4(ex, mad(v.x, v.y, v.z), clamp(v.x, 1u, 100u), (uint)sign((int)v.y));
}
