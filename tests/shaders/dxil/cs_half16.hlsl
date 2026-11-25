// dxc: -T cs_6_2 -enable-16bit-types
RWStructuredBuffer<half4> o : register(u0);
StructuredBuffer<half4> i : register(t0);
RWStructuredBuffer<uint16_t> ou : register(u1);
cbuffer C : register(b0) { half h; uint16_t u; int16_t s; float f; };
[numthreads(32, 1, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    half4 v = i[id.x];
    half a = v.x * v.y + h;
    half b = sqrt(abs(v.z)) + (half)f;
    uint16_t x = u + (uint16_t)id.x;
    int16_t y = s - (int16_t)id.x;
    o[id.x] = half4(a, b, (half)x, (half)y);
    ou[id.x] = x * 3 + (uint16_t)y;
}
