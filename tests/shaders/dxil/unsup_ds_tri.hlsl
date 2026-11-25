// dxc: -T ds_6_0
struct V { float4 pos : POSITION; float2 uv : TEXCOORD0; };
struct PC { float edges[3] : SV_TessFactor; float inside : SV_InsideTessFactor; };
struct O { float4 pos : SV_Position; float2 uv : TEXCOORD0; };
[domain("tri")]
O main(PC pc, float3 b : SV_DomainLocation, const OutputPatch<V, 3> p) {
    O o; o.pos = p[0].pos * b.x + p[1].pos * b.y + p[2].pos * b.z; o.uv = p[0].uv * b.x + p[1].uv * b.y + p[2].uv * b.z; return o;
}
