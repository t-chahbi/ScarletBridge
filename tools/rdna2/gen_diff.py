#!/usr/bin/env python3
"""Génère des encodages pseudo-aléatoires valides (au sens de llvm-mc) pour le test différentiel.

Sortie TSV : `wave<TAB>dwords hex séparés par des virgules<TAB>texte LLVM canonique`.
Consommé par le test Rust ignoré `rdna2_diff` (variable RDNA2_DIFF_TSV).

Usage : python3 -I tools/rdna2/gen_diff.py [graine] [n_par_opcode] > /tmp/rdna2_diff.tsv
"""
import os
import random
import re
import subprocess
import sys
from concurrent.futures import ThreadPoolExecutor

HERE = os.path.dirname(os.path.abspath(__file__))
TABLES = os.path.join(HERE, "..", "..", "crates", "sb-gpu", "src", "rdna2", "tables.rs")


def load_tables():
    cur, res = None, {}
    for line in open(TABLES):
        m = re.match(r"pub static (\w+): &\[OpDef\]", line)
        if m:
            cur = m.group(1)
            res[cur] = []
            continue
        m = re.match(r'\s*\(0x([0-9a-f]+), "([^"]*)", "([^"]*)"\)', line)
        if m and cur:
            res[cur].append((int(m.group(1), 16), m.group(2), m.group(3)))
    return res


def one(words, wave):
    attr = "-mattr=+wavefrontsize%d,-wavefrontsize%d" % (wave, 96 - wave)
    inp = ",".join("0x%02x" % ((w >> (8 * i)) & 255) for w in words for i in range(4)) + "\n"
    p = subprocess.run(["llvm-mc", "--disassemble", "-triple=amdgcn", "-mcpu=gfx1030", attr],
                       input=inp, capture_output=True, text=True)
    outs = [l.strip() for l in p.stdout.splitlines() if l.strip()]
    if p.stderr.strip() or len(outs) != 1 or "/*" in outs[0]:
        return None
    return outs[0]


def main():
    seed = int(sys.argv[1]) if len(sys.argv) > 1 else 1
    per = int(sys.argv[2]) if len(sys.argv) > 2 else 6
    rnd = random.Random(seed)
    T = load_tables()

    def rsrc9(vgpr_ok=True, lit=True):
        k = rnd.random()
        if k < 0.35:
            return rnd.randrange(0, 106) & ~1 if rnd.random() < .5 else rnd.randrange(0, 106)
        if k < 0.45:
            return rnd.choice([106, 107, 124, 125, 126, 127])
        if k < 0.6:
            return rnd.randrange(128, 209)
        if k < 0.67:
            return rnd.randrange(240, 249)
        if k < 0.7:
            return rnd.choice([251, 252, 253])
        if lit and k < 0.75:
            return 255
        if vgpr_ok:
            return 256 + rnd.randrange(0, 256)
        return rnd.randrange(0, 106)

    cands = []  # (words, comment)

    def lit(codes, extra=False):
        return [rnd.getrandbits(32)] if (255 in codes or extra) else []

    for op, name, shape in T["SOP2"]:
        for _ in range(per):
            s0, s1 = rsrc9(False), rsrc9(False)
            s0 &= 0xFF; s1 &= 0xFF
            w = 0x80000000 | op << 23 | rnd.randrange(0, 106) << 16 | s1 << 8 | s0
            cands.append([w] + lit([s0, s1]))
    for op, name, shape in T["SOP1"]:
        for _ in range(per):
            s0 = rsrc9(False) & 0xFF
            w = 0xBE800000 | rnd.randrange(0, 106) << 16 | op << 8 | s0
            cands.append([w] + lit([s0]))
    for op, name, shape in T["SOPC"]:
        for _ in range(per):
            s0, s1 = rsrc9(False) & 0xFF, rsrc9(False) & 0xFF
            cands.append([0xBF000000 | op << 16 | s1 << 8 | s0] + lit([s0, s1]))
    for op, name, shape in T["SOPK"]:
        for _ in range(per):
            cands.append([0xB0000000 | op << 23 | rnd.randrange(0, 106) << 16 | rnd.getrandbits(16)])
    for op, name, shape in T["SOPP"]:
        for _ in range(per):
            cands.append([0xBF800000 | op << 16 | rnd.getrandbits(16)])
    for op, name, shape in T["VOP2"]:
        for _ in range(per):
            s0 = rsrc9()
            cands.append([op << 25 | rnd.randrange(256) << 17 | rnd.randrange(256) << 9 | s0] + lit([s0], "K" in shape))
    for op, name, shape in T["VOP1"]:
        for _ in range(per):
            s0 = rsrc9()
            cands.append([0x7E000000 | rnd.randrange(256) << 17 | op << 9 | s0] + lit([s0]))
    for op, name, shape in T["VOPC"]:
        for _ in range(per):
            s0 = rsrc9()
            cands.append([0x7C000000 | op << 17 | rnd.randrange(256) << 9 | s0] + lit([s0]))
    for fam, base, mask in (("VOP3", 0xD4000000, 0x3FF), ("VOP3P", 0xCC000000, 0x7F)):
        for op, name, shape in T[fam]:
            for _ in range(per * 2):
                srcs = [rsrc9() for _ in range(3)]
                d0 = base | op << 16 | rnd.randrange(0, 106) if "d" in shape or "E" in shape else base | op << 16 | rnd.randrange(256)
                if rnd.random() < 0.5:
                    d0 |= rnd.getrandbits(8) << 8
                if rnd.random() < 0.3:
                    d0 |= 1 << 15
                d1 = srcs[0] | srcs[1] << 9 | srcs[2] << 18 | rnd.getrandbits(5) << 27
                cands.append([d0, d1] + lit(srcs))
    for op, name, shape in T["VINTRP"]:
        for _ in range(per):
            cands.append([0xC8000000 | op << 16 | rnd.getrandbits(16) | rnd.randrange(256) << 18])
    for op, name, shape in T["DS"]:
        for _ in range(per):
            cands.append([0xD8000000 | op << 18 | rnd.getrandbits(17), rnd.getrandbits(32)])
    for op, name, shape in T["MUBUF"]:
        for _ in range(per):
            cands.append([0xE0000000 | op << 18 | rnd.getrandbits(18), rnd.getrandbits(23) | rnd.choice([0x80, 0x81, 0x7c, 0x10, 0x41]) << 24])
    for op, name, shape in T["MTBUF"]:
        for _ in range(per * 3):
            cands.append([0xE8000000 | op << 16 | rnd.getrandbits(16) | rnd.randrange(128) << 19, rnd.getrandbits(24) | rnd.choice([0x80, 0x7c, 0x10]) << 24])
    for op, name, shape in T["SMEM"]:
        for _ in range(per):
            cands.append([0xF4000000 | op << 18 | rnd.getrandbits(7) << 6 | rnd.getrandbits(6) | rnd.getrandbits(2) << 14, rnd.getrandbits(21) | rnd.choice([0x7D, 0x7D, 0x10, 0x7e]) << 25])
    for seg, fam in ((0, "FLAT"), (1, "SCRATCH"), (2, "GLOBAL")):
        for op, name, shape in T[fam]:
            for _ in range(per):
                cands.append([0xDC000000 | op << 18 | seg << 14 | rnd.getrandbits(14) & 0x3FFF | rnd.getrandbits(2) << 16, rnd.getrandbits(16) | rnd.choice([0x7D, 0x7D, 0x04, 0x10]) << 16 | rnd.getrandbits(8) << 24])
    for op, name, shape in T["MIMG"]:
        for _ in range(per * 3):
            w0 = 0xF0000000 | (op & 0x7F) << 18 | (op >> 7) | rnd.getrandbits(18) & 0x3FFF8 | (rnd.getrandbits(1) << 25)
            w0 &= ~(3 << 1)  # nsa=0
            cands.append([w0, rnd.getrandbits(32)])
    for _ in range(per * 30):
        target = rnd.choice(list(range(0, 10)) + list(range(12, 16)) + list(range(32, 64)))
        cands.append([0xF8000000 | rnd.getrandbits(4) | target << 4 | rnd.getrandbits(1) << 10 | rnd.getrandbits(1) << 11 | rnd.getrandbits(1) << 12, rnd.getrandbits(32)])

    jobs = [(c, w) for c in cands for w in (32, 64)]
    with ThreadPoolExecutor(max_workers=16) as ex:
        res = list(ex.map(lambda j: one(j[0], j[1]), jobs))
    n = 0
    for (c, w), t in zip(jobs, res):
        if t:
            print("%d\t%s\t%s" % (w, ",".join("%08x" % x for x in c), t))
            n += 1
    sys.stderr.write("%d/%d valides\n" % (n, len(jobs)))


main()
