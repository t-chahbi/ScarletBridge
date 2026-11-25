"""Petit pont vers `llvm-mc` (cible AMDGPU gfx1030) pour les outils RDNA2.

Utilisé uniquement par les scripts de génération (tables d'opcodes / vecteurs de test) :
jamais à l'exécution des tests Rust. Clean-room : la seule source d'information est
l'assembleur/désassembleur LLVM public.
"""
import re
import subprocess
from concurrent.futures import ThreadPoolExecutor


def _attr(wave):
    return "-mattr=+wavefrontsize%d,-wavefrontsize%d" % (wave, 96 - wave)


def dis(lines, wave=32):
    """Désassemble une liste d'instructions (listes de dwords). Retourne texte ou None."""
    if not lines:
        return []
    inp = "\n".join(
        ",".join("0x%02x" % ((w >> (8 * i)) & 255) for w in ws for i in range(4))
        for ws in lines
    ) + "\n"
    p = subprocess.run(
        ["llvm-mc", "--disassemble", "-triple=amdgcn", "-mcpu=gfx1030", _attr(wave)],
        input=inp, capture_output=True, text=True,
    )
    bad = set(int(m.group(1)) for m in re.finditer(r"<stdin>:(\d+):", p.stderr))
    outs = [l.strip() for l in p.stdout.splitlines() if l.strip()]
    if len(outs) != len(lines) - len(bad):
        # sortie multi-instructions sur une ligne : repli ligne par ligne (lent mais sûr)
        if len(lines) == 1:
            return [outs[0] if outs and not bad and '/*' not in outs[0] else None]
        with ThreadPoolExecutor(max_workers=16) as ex:
            return list(ex.map(lambda l: dis([l], wave)[0], lines))
    res, k = [], 0
    for i in range(len(lines)):
        if i + 1 in bad:
            res.append(None)
        else:
            res.append(None if "/*" in outs[k] else outs[k])
            k += 1
    return res


def asm(lines, wave=32):
    """Assemble des lignes asm. Retourne (texte canonique, [dwords]) ou None par ligne."""
    res = []
    for line in lines:
        p = subprocess.run(
            ["llvm-mc", "-triple=amdgcn", "-mcpu=gfx1030", _attr(wave), "-show-encoding"],
            input=line + "\n", capture_output=True, text=True,
        )
        m = re.search(r"^\s*(.*?)\s*; encoding: \[(.*?)\]", p.stdout, re.M)
        if p.returncode != 0 or not m:
            res.append(None)
            continue
        bs = [int(b, 16) for b in m.group(2).split(",")]
        ws = [sum(bs[i + j] << (8 * j) for j in range(4)) for i in range(0, len(bs), 4)]
        res.append((re.sub(r"\s+", " ", m.group(1)), ws))
    return res
