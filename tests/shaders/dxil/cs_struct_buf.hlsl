// dxc: -T cs_6_0
struct P { float3 pos; uint flags; float4x4 mat; int arr[3]; };
StructuredBuffer<P> src : register(t0);
RWStructuredBuffer<P> dst : register(u0);
[numthreads(16, 1, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    P p = src[id.x];
    p.pos *= 2;
    p.flags |= 1;
    p.arr[1] += 5;
    dst[id.x] = p;
    dst[id.x].mat[1][2] = 3;
}
