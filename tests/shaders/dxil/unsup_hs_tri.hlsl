// dxc: -T hs_6_0 -E main
struct V { float4 pos : POSITION; float2 uv : TEXCOORD0; };
struct PC { float edges[3] : SV_TessFactor; float inside : SV_InsideTessFactor; };
PC patchf(InputPatch<V, 3> p) { PC o; o.edges[0] = 2; o.edges[1] = 3; o.edges[2] = 4; o.inside = 3; return o; }
[domain("tri")] [partitioning("integer")] [outputtopology("triangle_cw")] [outputcontrolpoints(3)] [patchconstantfunc("patchf")]
V main(InputPatch<V, 3> p, uint id : SV_OutputControlPointID) { return p[id]; }
