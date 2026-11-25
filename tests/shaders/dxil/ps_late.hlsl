// dxc: -T ps_6_0
Texture2D<uint4> ti : register(t0);
Texture2D<int> tsi : register(t1);
Texture2D<float> tf : register(t2);
SamplerState s : register(s0);
float4 main(float4 pos : SV_Position, float2 uv : TEXCOORD0) : SV_Target {
    uint4 a = ti.Load(int3(pos.xy, 0));
    int b = tsi.Load(int3(pos.xy, 1));
    float4 g = tf.GatherRed(s, uv) + tf.GatherGreen(s, uv, int2(1, 1));
    return float4(a) + float(b) + g;
}
