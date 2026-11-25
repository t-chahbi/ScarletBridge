// dxc: -T vs_6_0
cbuffer Bones : register(b1) { float4x3 bones[32]; };
struct VSIn { float3 pos : POSITION; uint4 idx : BLENDINDICES; float4 w : BLENDWEIGHT; float3 n : NORMAL; };
struct VSOut { float4 pos : SV_Position; float3 n : NORMAL; float clip0 : SV_ClipDistance0; float cull : SV_CullDistance0; uint rt : SV_RenderTargetArrayIndex; };
VSOut main(VSIn i) {
    float3 p = 0, n = 0;
    for (int k = 0; k < 4; ++k) {
        float4x3 b = bones[i.idx[k]];
        p += mul(float4(i.pos, 1), b) * i.w[k];
        n += mul(i.n, (float3x3)b) * i.w[k];
    }
    VSOut o;
    o.pos = float4(p, 1);
    o.n = n;
    o.clip0 = p.x;
    o.cull = p.y;
    o.rt = 1;
    return o;
}
