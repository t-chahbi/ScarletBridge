// dxc: -T ps_6_0
struct PSIn { float4 pos : SV_Position; float2 uv : TEXCOORD0; float4 col : COLOR0; nointerpolation uint id : TEXCOORD1; };
float4 main(PSIn i) : SV_Target {
    float4 c = i.col * float4(i.uv, 0.5, 1.0);
    c.x += float(i.id) * 0.01;
    return saturate(c);
}
