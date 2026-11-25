// dxc: -T cs_6_0
cbuffer C : register(b0) { uint n; uint mode; };
RWStructuredBuffer<uint> inb : register(u0);
RWStructuredBuffer<uint> outb : register(u1);
uint collatz(uint x) {
    uint steps = 0;
    [loop] while (x != 1 && steps < 200) { x = (x & 1) ? 3 * x + 1 : (x >> 1); steps++; }
    return steps;
}
[numthreads(64, 1, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    uint x = inb[id.x] % 97 + 1;
    uint r = collatz(x);
    switch (mode) {
        case 0: r += 1000; break;
        case 1: r += 2000;
        case 2: r += 3000; break;
        default: r = 7; break;
    }
    for (uint a = 0; a < 5; ++a) {
        if (a == (x & 3)) continue;
        if (a > 3 && (x & 1)) break;
        r += a * x;
    }
    uint k = 0;
    do { k++; if (k == 3) continue; r ^= k; } while (k < n);
    [loop] for (uint i = 0; i < 4; ++i) {
        for (uint j = 0; j < 4; ++j) {
            if (i + j == (x & 7)) break;
            r += 1;
        }
    }
    outb[id.x] = r;
}
