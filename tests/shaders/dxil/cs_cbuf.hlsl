// dxc: -T cs_6_0
cbuffer A : register(b0) { float4 a4; float3 a3; float a1; int2 ai; uint4 au[3]; float4x4 m; };
cbuffer B : register(b3) { float4 b4[10]; };
struct CB5 { float4 v; };
ConstantBuffer<CB5> cb : register(b5);
RWStructuredBuffer<float4> o : register(u0);
[numthreads(16, 1, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    float4 r = a4 + float4(a3, a1) + float4(ai, 0, 0) + (float4)au[id.x % 3] + mul(m, a4) + b4[id.x % 10] + cb.v;
    o[id.x] = r;
}
