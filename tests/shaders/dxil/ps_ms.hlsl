// dxc: -T ps_6_0
Texture2DMS<float4, 4> tms : register(t0);
float4 main(float4 pos : SV_Position, uint sid : SV_SampleIndex) : SV_Target {
    uint w, h, n;
    tms.GetDimensions(w, h, n);
    return tms.Load(int2(pos.xy), sid) + float4(w, h, n, 0);
}
