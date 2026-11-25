// dxc: -T cs_6_0
RWStructuredBuffer<float4> o : register(u0);
StructuredBuffer<float4> i : register(t0);
[numthreads(32, 1, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    float4 v = i[id.x];
    float a = exp(v.x) + exp2(v.y) + log(abs(v.z) + 1) + log2(abs(v.w) + 1) + log10(abs(v.x) + 1);
    float b = sqrt(abs(v.x)) + rsqrt(abs(v.y) + 1) + rcp(v.z + 2) + abs(v.w) + saturate(v.x);
    float c = sin(v.x) + cos(v.y) + tan(v.z) + asin(saturate(v.w)) + acos(saturate(v.x)) + atan(v.y) + atan2(v.z, v.w);
    float d = sinh(v.x) + cosh(v.y) + tanh(v.z);
    float e = floor(v.x) + ceil(v.y) + round(v.z) + trunc(v.w) + frac(v.x);
    float f = fmod(v.x, v.y + 5) + pow(abs(v.x), v.y) + max(v.x, v.y) + min(v.z, v.w);
    float g = mad(v.x, v.y, v.z) + lerp(v.x, v.y, 0.3) + smoothstep(0, 1, v.x) + step(v.x, v.y) + clamp(v.z, -1, 1);
    float h = dot(v, v) + length(v.xyz) + distance(v.xy, v.zw);
    float3 n = normalize(v.xyz) + cross(v.xyz, v.yzx) + reflect(v.xyz, normalize(v.yzx));
    float s = sign(v.x) + degrees(v.y) + radians(v.z);
    bool isn = isnan(v.x) || isinf(v.y) || isfinite(v.z);
    uint u = (uint)v.x + (uint)isn;
    int ii = (int)v.y;
    float k = (float)u + (float)ii + (float)(u >> 1);
    o[id.x] = float4(a + b, c + d + e, f + g + h + k, n.x + n.y + n.z + s);
    float2 sc; sincos(v.x, sc.x, sc.y);
    o[id.x + 32] = float4(sc, f16tof32(u), asfloat(f32tof16(v.x)));
}
