"""Sonde les encodages RDNA2 via llvm-mc pour en déduire noms et formes d'opérandes."""
import re
from llvm_dis import dis

REG = re.compile(r"^([sv])(?:(\d+)|\[(\d+):(\d+)\])$")


def parse(text):
    """-> (mnemonic, [operand tokens])"""
    text = text.split(";")[0].strip()
    parts = text.split(None, 1)
    mn = parts[0]
    ops = []
    if len(parts) > 1:
        depth = 0
        cur = ""
        for ch in parts[1]:
            if ch in "[(":
                depth += 1
            if ch in "])":
                depth -= 1
            if ch == "," and depth == 0:
                ops.append(cur.strip())
                cur = ""
            else:
                cur += ch
        ops.append(cur.strip())
        # les modificateurs suivent le dernier opérande, séparés par un blanc
        ops[-1] = ops[-1].split()[0] if ops[-1] else ops[-1]
    return mn, ops


def slots(text, fields):
    """fields: {numéro_magique: lettre}. Retourne (mnemonic, [(lettre, largeur)])."""
    mn, ops = parse(text)
    out = []
    for o in ops:
        m = REG.match(o)
        if m:
            n = int(m.group(2) if m.group(2) else m.group(3))
            w = 1 if m.group(2) else int(m.group(4)) - int(m.group(3)) + 1
            f = fields.get(n, "?")
            if f == "E" and len(out) != 1:
                f = "?"  # un « s0 » hors position 1 est un champ source nul, pas le sdst
            if f == "D" and m.group(1) == "s":
                f = "d"  # destination scalaire (sdst) dans une instruction vectorielle
            out.append((f, w))
        elif o in ("vcc", "vcc_lo"):
            out.append(("V", 1 if o == "vcc_lo" else 2))
        elif o.startswith("0x"):
            v = int(o, 16)
            out.append((fields.get(("imm", v), "?imm"), 0))
    return mn, out


def merge(s32, s64):
    """Fusionne les formes wave32/wave64 : largeur 'M' si (1,2) sur un registre masque."""
    if s32 is None:
        s32 = s64
    if s64 is None:
        s64 = s32
    if s32 is None:
        return None
    if [f for f, _ in s32] != [f for f, _ in s64]:
        return None
    res = []
    for (f, w32), (_, w64) in zip(s32, s64):
        if w32 == w64:
            res.append(f + str(w32))
        elif (w32, w64) == (1, 2):
            res.append(f + "M")
        else:
            res.append(f + "?")
    return " ".join(res)


def probe_family(name, mk, ops, fields, tries, tryfirst_ndw=None):
    """mk(op, variant) -> liste de dwords ; tries = liste de variantes à essayer dans l'ordre.

    Retourne {op: (mnemonic, shape)}.
    """
    result = {}
    pending = list(ops)
    for variant in tries:
        if not pending:
            break
        for wave_pair in [None]:
            lines = [mk(op, variant) for op in pending]
            t32 = dis(lines, 32)
            t64 = dis(lines, 64)
            still = []
            for op, a, b in zip(pending, t32, t64):
                if a is None and b is None:
                    still.append(op)
                    continue
                m32, s32 = slots(a, fields) if a else (None, None)
                m64, s64 = slots(b, fields) if b else (None, None)
                result[op] = (m32 or m64, merge(s32, s64), a or b)
            pending = still
    return result, pending
