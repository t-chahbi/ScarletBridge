// dxc: -T cs_6_0
RWStructuredBuffer<uint64_t> o : register(u0);
StructuredBuffer<uint64_t> i : register(t0);
[numthreads(16, 1, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    uint64_t a = i[id.x];
    int64_t b = (int64_t)a - 5;
    o[id.x] = a * 3 + (a >> 7) + (uint64_t)b + (a & 0xFFFF0000FFFFull) + (a / 7);
}
