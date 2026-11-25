// dxc: -T vs_6_0
cbuffer Frame : register(b0) {
    float4x4 mvp;
    float4 tint;
};
struct VSIn { float3 pos : POSITION; float2 uv : TEXCOORD0; float4 col : COLOR0; };
struct VSOut {
    float4 pos : SV_Position;
    float2 uv : TEXCOORD0;
    float4 col : COLOR0;
    nointerpolation uint id : TEXCOORD1;
};
VSOut main(VSIn i, uint vid : SV_VertexID, uint iid : SV_InstanceID) {
    VSOut o;
    o.pos = mul(mvp, float4(i.pos, 1.0));
    o.uv = i.uv;
    o.col = i.col * tint;
    o.id = vid + iid * 3u;
    return o;
}
