// dxc: -T cs_6_0
RWStructuredBuffer<uint> o : register(u0);
StructuredBuffer<uint> i : register(t0);
RWStructuredBuffer<float> of : register(u1);
[numthreads(32, 1, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    uint x = i[id.x];
    float fx = (float)x;
    uint lane = WaveGetLaneIndex();
    uint cnt = WaveGetLaneCount();
    uint r = lane + cnt;
    if (WaveIsFirstLane()) r += 1000;
    r += WaveActiveSum(x) + WaveActiveProduct(x) + WaveActiveMin(x) + WaveActiveMax(x);
    r += WaveActiveBitAnd(x) | WaveActiveBitOr(x) | WaveActiveBitXor(x);
    r += WaveActiveCountBits(x > 3);
    r += WavePrefixSum(x) + WavePrefixCountBits(x > 3) + WavePrefixProduct(x);
    r += WaveReadLaneAt(x, 3) + WaveReadLaneFirst(x);
    if (WaveActiveAllTrue(x > 0)) r += 1;
    if (WaveActiveAnyTrue(x > 100)) r += 2;
    if (WaveActiveAllEqual(x)) r += 4;
    uint4 bal = WaveActiveBallot(x > 5);
    r += bal.x;
    of[id.x] = WaveActiveSum(fx) + WaveActiveMax(fx) + WavePrefixSum(fx) + QuadReadAcrossX(fx);
    r += (uint)QuadReadAcrossY(fx) + (uint)QuadReadAcrossDiagonal(fx) + QuadReadLaneAt(x, 1);
    o[id.x] = r;
}
