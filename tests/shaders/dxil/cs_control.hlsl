// dxc: -T cs_6_0
RWStructuredBuffer<uint> o : register(u0);
StructuredBuffer<uint> i : register(t0);
cbuffer C : register(b0) { uint n; uint mode; };
uint fib(uint x) {
    uint a = 0, b = 1;
    for (uint k = 0; k < x; ++k) { uint t = a + b; a = b; b = t; }
    return a;
}
uint early(uint x) {
    if (x == 0) return 100;
    if (x == 1) { return 200; }
    [loop] for (uint k = 0; k < 16; ++k) {
        if (k == x) return k * 3;
    }
    return 7;
}
[numthreads(32, 1, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    uint x = i[id.x];
    uint r = 0;
    if (x > 10) r = 1; else if (x > 5) r = 2; else r = 3;
    uint k = 0;
    while (k < n) {
        k++;
        if (k == 3) continue;
        if (k > 20) break;
        r += k;
    }
    do { r ^= k; k >>= 1; } while (k > 0);
    for (uint a = 0; a < 4; ++a) {
        for (uint b = 0; b < 4; ++b) {
            if (b == a) continue;
            if (a + b > 5) break;
            r += a * b;
        }
    }
    switch (mode) {
        case 0: r += 10; break;
        case 1: r += 20;
        case 2: r += 30; break;
        case 5: { r *= 2; break; }
        default: r = 0; break;
    }
    [unroll] for (int u = 0; u < 4; ++u) r += u;
    r += fib(x & 7) + early(x & 15);
    float f = (r > 3) ? 1.5 : 2.5;
    r += (uint)f;
    while (true) { r++; if (r > 1000) break; }
    o[id.x] = r;
}
