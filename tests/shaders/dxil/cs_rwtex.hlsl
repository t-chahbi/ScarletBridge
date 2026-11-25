// dxc: -T cs_6_0
RWTexture2D<float4> img : register(u0);
RWTexture3D<uint> vol : register(u1);
RWTexture1D<float> line1 : register(u2);
RWTexture2DArray<float2> arr : register(u3);
Texture2D<float4> src : register(t0);
[numthreads(8, 8, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    float4 c = img[id.xy];
    img[id.xy] = c * 2 + src.Load(int3(id.xy, 0));
    vol[id] = vol[id] + 1;
    line1[id.x] = c.x;
    arr[id] = c.xy;
    uint w, h;
    img.GetDimensions(w, h);
    InterlockedAdd(vol[id], 1);
}
