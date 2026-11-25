// dxc: -T cs_6_0
ByteAddressBuffer src : register(t0);
RWByteAddressBuffer dst : register(u0);
Buffer<float4> tb : register(t1);
RWBuffer<uint> rb : register(u1);
[numthreads(8, 8, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    uint i = id.x + id.y * 8;
    uint a = src.Load(i * 4);
    uint2 b = src.Load2(i * 8);
    uint4 c = src.Load4(i * 16);
    float f = asfloat(src.Load(64));
    dst.Store(i * 4, a + b.x);
    dst.Store4(i * 16, c);
    dst.Store2(i * 8, b);
    float4 t = tb[i];
    rb[i] = asuint(t.x) + a;
    uint n; dst.GetDimensions(n);
    dst.Store(0, n);
    uint m; tb.GetDimensions(m);
    dst.Store(4, m);
}
