// dxc: -T ms_6_5
struct V { float4 pos : SV_Position; };
[numthreads(32, 1, 1)] [outputtopology("triangle")]
void main(uint tid : SV_GroupIndex, out vertices V verts[3], out indices uint3 tris[1]) {
    SetMeshOutputCounts(3, 1);
    verts[tid % 3].pos = float4(tid, 0, 0, 1);
    tris[0] = uint3(0, 1, 2);
}
