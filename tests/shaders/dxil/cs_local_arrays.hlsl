// dxc: -T cs_6_0
RWStructuredBuffer<float> o : register(u0);
cbuffer C : register(b0) { uint sel; };
[numthreads(16, 1, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    float t[8];
    for (uint k = 0; k < 8; ++k) t[k] = (float)(k * id.x);
    float acc = 0;
    for (uint j = 0; j < 8; ++j) acc += t[(j + sel) & 7];
    o[id.x] = acc;
}
