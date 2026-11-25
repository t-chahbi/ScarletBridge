#!/usr/bin/env python3
"""Génère `crates/sb-gpu/src/rdna2/tables.rs` : opcodes + formes d'opérandes RDNA2 (gfx10.3).

Source de vérité : le désassembleur LLVM (`llvm-mc -triple=amdgcn -mcpu=gfx1030`), sondé avec des
champs de registres « magiques » distincts pour retrouver quel champ binaire correspond à quel
opérande textuel. Aucun code ni document tiers n'est lu. Outil de développement uniquement :
la sortie est versionnée, les tests Rust ne dépendent pas de llvm-mc.

Usage : python3 -I tools/rdna2/gen_tables.py > crates/sb-gpu/src/rdna2/tables.rs
"""
import os
import re
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from probe import probe_family, parse  # noqa: E402
from llvm_dis import dis  # noqa: E402

LIT = 0x0ABC1234
OUT = []


def emit(name, doc, table):
    OUT.append("/// %s" % doc)
    OUT.append("pub static %s: &[OpDef] = &[" % name)
    for op in sorted(table):
        mn, shape = table[op][0], table[op][1]
        OUT.append('    (0x%x, "%s", "%s"),' % (op, mn, shape or ""))
    OUT.append("];\n")


def family(name, doc, mk, ops, fields, tries):
    f = dict(fields)
    res, pend = probe_family(name, mk, ops, f, tries)
    sys.stderr.write("%s: %d ops non décodables\n" % (name, len(pend)))
    emit(name, doc, res)
    sys.stderr.write("%s: %d opcodes\n" % (name, len(res)))
    return res


def main():
    fl = {8: "D", 16: "A", 24: "B", 32: "C", 0: "E", ("imm", 0x1234): "I", ("imm", LIT): "K"}
    OUT.append("//! Tables d'opcodes RDNA2 (gfx10.3) — GÉNÉRÉ par tools/rdna2/gen_tables.py. Ne pas éditer.")
    OUT.append("//!")
    OUT.append("//! Format de `shape` : slots séparés par des blancs, lettre de champ + largeur en dwords")
    OUT.append("//! (`M` = masque de lanes : 1 dword en wave32, 2 en wave64). Champs : D=dst vectoriel, d=dst scalaire, E=sdst VOP3,")
    OUT.append("//! A/B/C=src0/1/2, I=simm16, K=littéral, V=VCC implicite, Y=adresse, P/Q=data0/1,")
    OUT.append("//! R=srsrc, O=soffset.\n")
    OUT.append("/// (opcode, mnémonique LLVM, forme)")
    OUT.append("pub type OpDef = (u16, &'static str, &'static str);\n")

    family("SOP2", "SOP2 : opcodes 0..95", lambda op, v: [0x80000000 | op << 23 | 8 << 16 | 24 << 8 | 16], range(0x60), fl, [0])
    family("SOP1", "SOP1 : opcodes 0..255", lambda op, v: [0xBE800000 | 8 << 16 | op << 8 | 16], range(256), fl, [0])
    family("SOPC", "SOPC : opcodes 0..127", lambda op, v: [0xBF000000 | op << 16 | 24 << 8 | 16], range(128), fl, [0])
    family("SOPK", "SOPK : opcodes 0..28", lambda op, v: [0xB0000000 | op << 23 | 8 << 16 | 0x1234], range(0x1D), fl, [0])

    # SOPP : seulement présence d'un opérande
    sopp = {}
    # simm16 non nul d'abord ; certains opcodes (s_barrier...) exigent simm16 = 0.
    lines = [[0xBF800000 | op << 16 | 0x12] for op in range(128)]
    lines0 = [[0xBF800000 | op << 16] for op in range(128)]
    for op, (t, t0) in enumerate(zip(dis(lines, 32), dis(lines0, 32))):
        if t:
            mn, ops = parse(t)
            sopp[op] = (mn, "I0" if ops else "")
        elif t0:
            sopp[op] = (parse(t0)[0], "")
    emit("SOPP", "SOPP : opcodes 0..127 (`I0` = a un simm16)", sopp)

    def vsrc(v, k):
        return (256 + k) if v == "v" else k

    vop_tries = [("v", 1), ("s", 1), ("v", 2), ("s", 2)]

    def mk_vop2(op, v):
        w = [op << 25 | 8 << 17 | 24 << 9 | vsrc(v[0], 16)]
        return w + ([LIT] if v[1] == 2 else [])

    family("VOP2", "VOP2 (e32) : opcodes 0..62", mk_vop2, range(0x3E), fl, vop_tries)

    def mk_vop1(op, v):
        if v[0] == "z":  # v_nop et consorts : tous les champs à zéro
            return [0x7E000000 | op << 9]
        w = [0x7E000000 | 8 << 17 | op << 9 | vsrc(v[0], 16)]
        return w + ([LIT] if v[1] == 2 else [])

    family("VOP1", "VOP1 (e32) : opcodes 0..255", mk_vop1, range(256), fl, vop_tries + [("z", 1)])

    def mk_vopc(op, v):
        w = [0x7C000000 | op << 17 | 24 << 9 | vsrc(v[0], 16)]
        return w + ([LIT] if v[1] == 2 else [])

    family("VOPC", "VOPC (e32) : opcodes 0..255", mk_vopc, range(256), fl, vop_tries)

    srcs = []
    for nsrc in (3, 2, 1, 0):
        for pat in ("vvv", "sss", "vss"):
            srcs.append((pat, nsrc))

    def vop3_src(v):
        pat, n = v
        codes = []
        for i, c in enumerate(pat):
            k = (16, 24, 32)[i]
            codes.append((256 + k if c == "v" else k) if i < n else 0)
        return codes[0] | codes[1] << 9 | codes[2] << 18

    family("VOP3", "VOP3 : opcodes 0..1023 (0..255 = VOPC, 256.. = VOP2/VOP1/natifs)",
           lambda op, v: [0xD4000000 | op << 16 | 8, vop3_src(v)], range(1024), fl, srcs)
    family("VOP3P", "VOP3P : opcodes 0..127",
           lambda op, v: [0xCC000000 | op << 16 | 8, vop3_src(v)], range(128), fl, srcs)

    # DS : combinaisons de champs présents
    dfl = {8: "D", 16: "Y", 24: "P", 32: "Q"}
    combos = []
    for mask in range(15, -1, -1):
        combos.append(mask)
    combos.sort(key=lambda m: -bin(m).count("1"))

    def mk_ds(op, m):
        a = 16 if m & 1 else 0
        d0 = 24 if m & 2 else 0
        d1 = 32 if m & 4 else 0
        d = 8 if m & 8 else 0
        return [0xD8000000 | op << 18, a | d0 << 8 | d1 << 16 | d << 24]

    family("DS", "DS : opcodes 0..255", mk_ds, range(256), dfl, combos)

    mfl = {8: "D", 16: "Y", 24: "R", 32: "O"}

    def mk_mubuf(op, v):
        return [0xE0000000 | op << 18 | (0x1000 if v else 0), 16 | 8 << 8 | (24 >> 2) << 16 | 32 << 24]

    family("MUBUF", "MUBUF : opcodes 0..255", mk_mubuf, range(256), mfl, [1, 0])

    def mk_mtbuf(op, v):
        # op[2:0] dans dword0[18:16], op[3] (variantes d16) dans dword1[21]
        return [0xE8000000 | (op & 7) << 16 | (0x1000 if v else 0),
                16 | 8 << 8 | (24 >> 2) << 16 | (op >> 3) << 21 | 32 << 24]

    family("MTBUF", "MTBUF : opcodes 0..15 (op[3] = dword1[21] ; format 7 bits dans dword0[25:19])", mk_mtbuf, range(16), mfl, [1, 0])

    # Noms symboliques des formats de données/numériques unifiés (MTBUF.format).
    fmts = []
    lines = [[0xE8000000 | f << 19, 0x80010000] for f in range(128)]
    for f, t in enumerate(dis(lines, 32)):
        m = re.search(r"format:\[(\w+)\]", t or "")
        fmts.append(m.group(1) if m else "")
    OUT.append("/// Noms symboliques des formats MTBUF (index = champ format 0..127 ; vide = affichage numérique).")
    OUT.append("pub static TBUF_FORMATS: [&str; 128] = [")
    for f in fmts:
        OUT.append('    "%s",' % f)
    OUT.append("];\n")

    sfl = {8: "D", 16: "B", ("imm", 0x1234): "I"}

    def mk_smem(op, v):
        return [0xF4000000 | op << 18 | 8 << 6 | (16 >> 1), 0x1234 | 0x7D << 25]

    family("SMEM", "SMEM : opcodes 0..255", mk_smem, range(256), sfl, [0])

    ffl = {8: "D", 16: "Y", 24: "P"}
    for seg, nm in ((0, "FLAT"), (1, "SCRATCH"), (2, "GLOBAL")):
        def mk_flat(op, v, seg=seg):
            return [0xDC000000 | op << 18 | seg << 14 | 1 << 16, 16 | 24 << 8 | 0x7D << 16 | 8 << 24]
        family(nm, "%s : opcodes 0..127 (glc=1 : atomiques avec retour)" % nm, mk_flat, range(128), ffl, [0])

    # MIMG : noms seulement (largeurs calculées par le décodeur selon dim/dmask)
    mimg = {}
    lines = [[0xF0000000 | ((op & 0x7F) << 18) | (op >> 7), 0, 0] for op in range(256)]
    for op, t in enumerate(dis(lines, 32)):
        if t and not t.startswith("v_illegal"):
            mn, _ = parse(t)
            mimg[op] = (mn, "")
    emit("MIMG", "MIMG : opcodes 0..255 (noms seuls)", mimg)

    # Noms symboliques des registres matériels (s_getreg/s_setreg, id sur 6 bits).
    hw = []
    lines = [[0xB0000000 | 0x12 << 23 | 8 << 16 | (31 << 11) | i] for i in range(64)]
    for i, t in enumerate(dis(lines, 32)):
        m = re.search(r"hwreg\((\w+),", t or "") or re.search(r"hwreg\((\w+)\)", t or "")
        hw.append(m.group(1) if m and not m.group(1).isdigit() else "")
    OUT.append("/// Noms symboliques des registres matériels `hwreg` (index = id 0..63 ; vide = numérique).")
    OUT.append("pub static HWREG_NAMES: [&str; 64] = [")
    for h in hw:
        OUT.append('    "%s",' % h)
    OUT.append("];\n")

    vintrp = {}
    lines = [[0xC8000000 | op << 16 | 8 << 18] for op in range(4)]
    for op, t in enumerate(dis(lines, 32)):
        if t:
            mn, _ = parse(t)
            vintrp[op] = (mn, "")
    emit("VINTRP", "VINTRP : opcodes 0..3", vintrp)
    print("\n".join(OUT))


main()
