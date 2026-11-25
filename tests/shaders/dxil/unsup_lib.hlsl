// dxc: -T lib_6_3
RaytracingAccelerationStructure scene : register(t0);
RWTexture2D<float4> outImg : register(u0);
struct Payload { float4 c; };
[shader("raygeneration")]
void rg() {
    RayDesc r; r.Origin = float3(0,0,0); r.Direction = float3(0,0,1); r.TMin = 0; r.TMax = 100;
    Payload p; p.c = 0;
    TraceRay(scene, 0, 0xff, 0, 1, 0, r, p);
    outImg[DispatchRaysIndex().xy] = p.c;
}
[shader("miss")] void miss(inout Payload p) { p.c = float4(0, 0, 1, 1); }
