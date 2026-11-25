// dxc: -T vs_6_0
struct VSIn { float4 pos : POSITION; float4 col : COLOR; };
struct VSOut { float4 pos : SV_Position; float4 col : COLOR; };
VSOut main(VSIn i) { VSOut o; o.pos = i.pos; o.col = i.col; return o; }
