// dxc: -T cs_6_0
RWStructuredBuffer<float4> inb : register(u0);
RWStructuredBuffer<float4> outb : register(u1);
cbuffer C : register(b0) { float4 scale; float4 bias; };
[numthreads(64, 1, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    float4 v = inb[id.x];
    float4 r = v * scale + bias;
    r.x = sqrt(abs(r.x)) + floor(v.y);
    r.y = max(v.x, v.z) - min(v.y, v.w);
    r.z = lerp(v.x, v.z, 0.25) + (float)(uint)v.w;
    r.w = saturate(v.w * 0.1) + frac(v.x) + exp2(v.y * 0.01) + rsqrt(v.z + 1.0);
    outb[id.x] = r;
}
