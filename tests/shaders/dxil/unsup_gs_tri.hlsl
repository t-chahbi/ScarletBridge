// dxc: -T gs_6_0
struct V { float4 pos : SV_Position; float2 uv : TEXCOORD0; };
[maxvertexcount(3)]
void main(triangle V i[3], inout TriangleStream<V> s) {
    for (uint k = 0; k < 3; ++k) { V o = i[k]; o.pos.z += 0.1; s.Append(o); }
    s.RestartStrip();
}
