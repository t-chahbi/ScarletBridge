// dxc: -T ps_6_0
struct Out { float4 c : SV_Target; float d : SV_Depth; };
Out main(float4 pos : SV_Position, float2 uv : TEXCOORD0, bool front : SV_IsFrontFace) {
    Out o;
    if (uv.x < 0.0) discard;
    clip(uv.y - 0.1);
    o.c = front ? float4(1, 0, 0, 1) : float4(0, 0, 1, 1);
    o.d = pos.z * 0.5;
    return o;
}
