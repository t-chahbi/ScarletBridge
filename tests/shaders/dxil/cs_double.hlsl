// dxc: -T cs_6_0
RWStructuredBuffer<double> o : register(u0);
StructuredBuffer<double> i : register(t0);
RWStructuredBuffer<uint2> bits : register(u1);
[numthreads(16, 1, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    double a = i[id.x], b = i[id.x + 1];
    double c = a * b + a / (b + 1.0) - a;
    double d = sqrt(abs(c)) ;
    o[id.x] = c + d + (double)id.x + (double)(float)a;
    uint lo, hi;
    asuint(a, lo, hi);
    bits[id.x] = uint2(lo, hi);
    double e = asdouble(lo, hi);
    o[id.x + 16] = (a > b) ? e : floor(a);
    float cf = (float)c;
    int ci = (int)c;
    uint cu = (uint)c;
    o[id.x + 32] = cf + ci + cu;
}
