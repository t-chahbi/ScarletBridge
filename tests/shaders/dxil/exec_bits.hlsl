// dxc: -T cs_6_0
RWStructuredBuffer<uint4> inb : register(u0);
RWStructuredBuffer<uint4> outb : register(u1);
[numthreads(64, 1, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    uint4 v = inb[id.x];
    int4 s = (int4)v;
    uint4 r;
    r.x = reversebits(v.x) ^ countbits(v.y);
    r.y = (uint)firstbitlow(v.z) + (uint)firstbithigh(v.w) * 3;
    r.z = (uint)(s.x >> 3) + (uint)(s.y / ((s.z & 255) + 1)) + (uint)(s.y % ((s.w & 255) + 1));
    r.w = (v.x << (v.y & 31)) ^ (v.z >> (v.w & 31)) ^ ((v.x >> 4) & 0xFF) ^ (uint)firstbithigh(s.z);
    outb[id.x] = r;
}
