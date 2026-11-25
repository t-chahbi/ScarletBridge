# Module `sb-gpu::rdna2` — décodeur ISA AMD RDNA 2 et traducteur SPIR-V

Décodeur / désassembleur de l'ISA **AMD RDNA 2 (gfx10.3), wave32 et wave64**, et traducteur de
programmes GPU console (VS, PS, CS) vers **SPIR-V 1.3 / Vulkan 1.1**. Le module est général :
il ne contient aucune connaissance d'un jeu ou d'un binaire commercial.

Code : `crates/sb-gpu/src/rdna2/` (déclaré dans `lib.rs` par `pub mod rdna2;`).

## 1. Méthode et sources (clean-room)

* **Encodages, opcodes, noms, formes d'opérandes** : générés à partir de l'assembleur /
  désassembleur public **LLVM** (`llvm-mc -triple=amdgcn -mcpu=gfx1030`, LLVM 23.1) par
  `tools/rdna2/gen_tables.py`. Le script « sonde » chaque opcode avec des champs de registres
  magiques distincts pour retrouver quel champ binaire correspond à quel opérande textuel. La
  sortie est versionnée (`tables.rs`) ; les tests Rust n'appellent jamais `llvm-mc`.
* **Sémantique** : guide ISA public AMD « RDNA 2 Instruction Set Architecture Reference Guide » et
  doc LLVM AMDGPU. Le PDF n'a pas été re-téléchargé pendant ce travail : la sémantique a été écrite
  depuis la connaissance de ces documents publics puis **validée empiriquement** (§6).
* Aucun code, document ni binaire des projets « Leonida », aucun binaire de jeu : tous les
  programmes de test sont écrits à la main (`tools/rdna2/gen_programs.py`).

## 2. Décodeur (`decode.rs`, `inst.rs`)

`decode(&[u32], WaveSize) -> Result<Instruction, DecodeError>` et `decode_stream`.

`Instruction { encoding, opcode, name, words, n_dst, operands: Vec<Operand>, mods: Mods }` :

* `Operand { src: Src, width, neg, abs }` ; `Src` = `Sgpr`, `Ttmp`, `Vgpr`, `VccLo/Hi`, `M0`,
  `Null`, `ExecLo/Hi`, `InlineInt`, `InlineFloat`, `Literal`, `Scc`, `Vccz`, `Execz`, `LdsDirect`,
  `Special`, `Off`. Les registres multi-dwords sont alignés comme le matériel (paire sur 2, quad
  sur 4). Les masques de lanes (`VCC`, sdst de `v_cmp`, source de `v_cndmask`…) ont une largeur de
  1 dword en wave32 et 2 en wave64.
* `Mods` : `clamp`, `omod`, `op_sel`, `op_sel_hi`, `neg_lo/hi`, `simm16`, offsets
  (`offset`, `offset0/1`), `glc/slc/dlc/tfe/lds/gds`, `offen/idxen`, `format`, champs `exp`
  (`target/en/compr/done/vm`), champs MIMG (`dmask/dim/unorm/lwe/d16/a16/r128/nsa`), `attr`/`attr_chan`.
* Littéraux (dword suivant) pour SOP*, VOP1/2/C, VOP3 (3 dwords), `v_fmamk/fmaak`.

### Couverture décodeur

Familles : SOP1, SOP2, SOPK, SOPC, SOPP, SMEM, VOP1, VOP2, VOPC, VOP3, VOP3P, VINTRP, DS, MUBUF,
MTBUF, MIMG (y compris NSA), EXP, FLAT, GLOBAL, SCRATCH — **toutes implémentées**.

Opcodes définis dans les tables (1338, ceux que `llvm-mc` gfx1030 accepte) :
SOP2 51, SOP1 65, SOPC 18, SOPK 26, SOPP 37, VOP2 47, VOP1 81, VOPC 190, VOP3 325, VOP3P 29,
DS 116, MUBUF 75, MTBUF 16, SMEM 16, FLAT 54, SCRATCH 22, GLOBAL 57, MIMG 110, VINTRP 3, + `exp`.

**Non décodés en détail** : extensions **SDWA** et **DPP8/DPP16** (reconnues, longueur correcte,
second dword exposé dans `Mods::ext_raw`, mais pas de décodage des champs ni de désassemblage
exact). Les instructions VOP3 « interp » f16 (`v_interp_p1ll_f16`…) ne sont pas décodées avec leurs
opérandes `attr`.

### Désassembleur (`disasm.rs`)

`disassemble(&Instruction, WaveSize) -> String`, syntaxe LLVM.

Mesures (test différentiel `tests/rdna2_diff.rs`, ignoré par défaut, voir §7) sur ~16 000
encodages pseudo-aléatoires acceptés par `llvm-mc`, wave32 + wave64 :

* **93,9 % de textes identiques** (15 057 / 16 027, graine 7).
* Les écarts restants (≈ 970) sont presque tous des combinaisons exotiques de **MIMG**
  (`a16`/`d16`/`tfe`/`lwe` aléatoires : taille du `vaddr`/`vdata` non modélisée exactement par
  LLVM dans ces cas), plus quelques instructions rares : `s_waitcnt_depctr` (champs symboliques),
  `v_interp_*_f16` en VOP3, `v_cvt_pknorm_*` avec `op_sel`, `s_sendmsg(halt)`/`s_version`
  symboliques.
* Les 268 vecteurs de test figés (`test_vectors.rs`, assemblés par `llvm-mc`, toutes familles,
  wave32 et wave64) sont **tous** restitués à l'identique.

## 3. IR de flot de contrôle (`cfg.rs`)

Blocs de base + terminateurs (`Fallthrough`, `Branch`, `Cond{scc0/scc1/vccz/vccnz/execz/execnz}`,
`End`). Les branches RDNA sont **uniformes sur la vague** : seul `EXEC` crée de la divergence.
`s_setpc/s_swappc/s_getpc/s_call` et `s_cbranch_cdbg*` sont refusés (`CfgError::Indirect`).
Les cibles de branchement doivent tomber sur le début d'une instruction.

## 4. Traduction SPIR-V (`translate.rs`, `trans_alu.rs`, `trans_mem.rs`)

`translate_program(&[u32], &TranslateConfig) -> Result<TranslatedProgram, TranslateError>`.
Réutilise le writer `crate::dxbc_to_spirv::spv::Spv` **sans le modifier** (dépendance en lecture
seule : toute évolution incompatible de ce writer par le chantier DXIL casserait ce module).

### Modèle

* Une invocation SPIR-V = une lane. Registres = variables `Function` `uint` créées à la demande
  (128 codes scalaires, 256 VGPR).
* **Masques de lanes réels** : `EXEC`, `VCC` et tout SGPR de masque sont des entiers. Un `v_cmp`
  écrit `ballot(cmp && exec)` (`OpGroupNonUniformBallot`) ; `v_cndmask`, `v_addc`… et les écritures
  VALU extraient le bit de la lane via `SubgroupLocalInvocationId`. Les opérations scalaires sur
  masques (`s_and_b64`, `s_and_saveexec_b32`, `s_or exec, exec, sN`, `s_cbranch_execz`…) sont donc
  **exactes** (ce sont de simples opérations entières uniformes).
* Écritures VALU : `vgpr = select(exec_bit, nouveau, ancien)`. Mémoire / exports / LDS / atomiques :
  région `OpSelectionMerge` gardée par le bit EXEC.
* **Optimisation** : si le programme n'écrit jamais EXEC, la prédication disparaît (VS/PS/CS
  linéaires sans capacité de sous-groupe — c'est le cas des exemples VS passe-plat et PS couleur).
* **Flot de contrôle** : programme linéaire → code direct ; sinon machine à `pc` (boucle
  structurée + `OpSwitch` sur le bloc), toujours valide, y compris CFG irréductible.
* PS : si EXEC est modifié, `OpKill` en fin de programme pour les lanes tuées.

### Configuration (`TranslateConfig`)

* `stage` (VS/PS/CS), `wave`, `local_size`.
* `sgprs: [(sgpr, SgprInit)]` : `UserData(i)` (mot du buffer « user data »), `WorkgroupId(axe)`,
  `Constant`.
* `vgprs: [(vgpr, VgprInit)]` : `VertexId`, `InstanceId`, `LocalInvocationId(axe)`,
  `FragCoord(axe)`, `Constant`.
* `resources` : liaison Vulkan `(set, binding)` de chaque descripteur, avec son emplacement :
  `Sgprs(n)` (V#/T#/S# directement dans des SGPR), `Table{table_sgpr, byte_offset}` (chargé par
  `s_load_*` depuis une table pointée par une paire de SGPR) ou `Pointer(n)` (pointeur vers les
  données d'un buffer, cible de `s_load_*`).
  **Les descripteurs ne sont jamais interprétés** : le traducteur ne lit pas les mots du V#/T#/S#.
* `lds_dwords` pour `ds_*`.

Entrées/sorties : VS `exp posN` → `Position` (pos1..3 ignorés), `exp paramN` → `Location N` ;
PS `exp mrtN` → `Location N`, `exp mrtz` → `FragDepth`, `v_interp_*` → entrées `Location attr`.

### Couverture traducteur (mesurée)

Mesure par `translator_coverage_report` (traduit une instruction synthétique par opcode des
tables ; « traduit » = pas d'erreur `Unsupported`) : **471 / 1176** opcodes des tables
SOP*/VOP1/2/C/VOP3/DS/MUBUF/MTBUF/SMEM/MIMG/VINTRP (hors VOP3P, FLAT/GLOBAL/SCRATCH, EXP).

| Famille | traduits / décodables | Détail |
|---|---|---|
| SOP2 | 41/51 | add/sub/addc/subb/min/max/cselect, logique 32+64 bits, shifts 32, mul, mul_hi, bfe_u32/i32, bfm_b32, absdiff, pack. Non : shifts/bfe/bfm 64 bits, `lshl[1-4]_add` |
| SOP1 | 33/65 | mov/not/brev/bcnt1/ff1/flbit (32), abs, sext, cmov, `*_saveexec_b32/b64` (and, or, xor, andn2, orn2, nand, nor, xnor). Non : wqm, 64 bits (brev/ff/flbit/bitset), quadmask, movrel*, getpc/setpc |
| SOPC | 16/18 | s_cmp_*_i32/u32, `_u64` eq/lg, bitcmp_b32. Non : bitcmp_b64 |
| SOPK | 21/26 | movk/cmovk/addk/mulk/cmpk_*/waitcnt_*. Non : getreg/setreg/call/subvector |
| SOPP | 25/37 | nop, waitcnt, sleep, setprio, barrier (CS), branches (via CFG), endpgm… Non : sendmsg, trap, halt… |
| VOP1 | 29/81 | mov, cvt f32↔i32/u32, f16↔f32, ubyteN, rcp/rsq/sqrt/exp/log/sin/cos, floor/ceil/trunc/fract/rndne, not/bfrev/ffbh, readfirstlane. Non : f64, f16 (hors cvt), frexp, movrel |
| VOP2 | 31/47 | add/sub/subrev/mul/min/max f32, fmac/fmamk/fmaak, ldexp, cndmask, entiers (add/sub nc, logique, shifts, u24, min/max), `*_co_ci_u32`, pkrtz. Non : f16, mul_hi_*24, dot |
| VOPC | 64/190 | `v_cmp[x]_*_{f32,i32,u32}` (16 relations f32, 8 relations int). Non : f16, f64, i16/u16, i64/u64, `class` |
| VOP3 | 119/325 | formes e64 de ce qui précède + fma/med3/min3/max3, bfe, bfi, alignbit/byte, mul_lo/hi, mad_u32_u24/i32_i24, lshl_add, add3, and_or, or3, xad, readlane, `*_co_u32`. Modificateurs `abs/neg/clamp/omod` pour f32. Non : f16, f64, cube*, sad*, div_*, mad_u64… |
| DS | 8/116 | `ds_read_b32/64/96/128`, `ds_write_b32/64/96/128` (LDS en `Workgroup`). Non : atomiques, `*2*`, u8/u16, swizzle/permute |
| MUBUF | 43/75 | load/store dword×1..4, `format_*` (hypothèse f32 par canal), ubyte/sbyte/ushort/sshort (loads), atomiques 32 bits (add, sub, min, max, and, or, xor, swap, cmpswap), `offen/idxen/offset/soffset`. Non : stores 8/16 bits, `d16`, atomiques 64 bits/inc/dec/csub/float, `tfe`, `lds` |
| MTBUF | 16/16 | formats **32 bits par canal** uniquement (les autres : `Unsupported` explicite) |
| SMEM | 12/16 | `s_load_dword*`, `s_buffer_load_dword*` (+ no-ops de cache). Non : memtime, atc_probe |
| MIMG | 10/110 | `image_sample`, `_l`, `_lz`, `_b`, `_c`, `_c_l`, `_c_lz`, `image_load`, `image_load_mip` ; dims 1D/2D/3D/Cube/1D-array/2D-array ; dmask. Non : offsets, dérivées, clamp, gather4, store, atomiques, resinfo, MSAA, a16/d16/tfe/lwe |
| VINTRP | 3/3 | `v_interp_p1` (valeur déjà interpolée par Vulkan), `p2` (no-op), `mov` |
| EXP | oui | pos0, param0..31, mrt0..7, mrtz, null ; masque `en`, mode `compr`, `vm`/`done` ignorés |
| VOP3P | 0/29 | **non traduit** (math packée f16, dot, fma_mix) |
| FLAT/GLOBAL/SCRATCH | 0/133 | **non traduit** (adresses 64 bits : nécessiterait `PhysicalStorageBuffer64`) |

Toute instruction non traduite → `TranslateError::Unsupported(mnémonique)` ; descripteur non
configuré → `TranslateError::Unresolved` ; jamais de devinette silencieuse.

## 5. Hypothèses et limites connues

1. **Taille de sous-groupe = taille de vague** (32 ou 64) pour tout programme qui modifie EXEC ou
   écrit un masque (ballot). Sur GPU AMD : `VK_EXT_subgroup_size_control` avec
   `requiredSubgroupSize` (fait par les tests CS). Pour un étage graphique, le pilote choisit :
   wave64 avec un shader wave32 donne des résultats faux si des masques sont utilisés.
2. Capacités requises quand EXEC/VCC sont utilisés : `GroupNonUniform`, `GroupNonUniformBallot`
   (+ `GroupNonUniformShuffle` pour `v_readlane_b32`) ; fonctionnalités de sous-groupe Vulkan 1.1
   (ballot) sur l'étage concerné.
3. Les arrondis et dénormaux suivent SPIR-V/le pilote (le mode `s_round_mode`/`s_denorm_mode` est
   ignoré) ; `v_sin/cos` = `sin/cos(2πx)` ; `v_rcp/rsq/exp/log` = précision du pilote.
   `v_cvt_pkrtz_f16_f32` = `PackHalf2x16` (arrondi du pilote, pas forcément RTZ).
4. **Pas de contrôle de bornes** matériel sur les buffers (V# `num_records`) : on compte sur
   `robustBufferAccess`. Formats `buffer_load_format_*` : f32 par canal supposé.
5. `v_interp_p1/p2` : Vulkan fournit la valeur interpolée complète ; les VGPR i/j (barycentriques)
   ne sont pas utilisés. Un attribut lu par `v_interp_mov_f32` n'est pas décoré `Flat`.
6. `s_cbranch_vccz/execz…` sont évalués sur les **mots entiers** de VCC/EXEC (exact en
   uniformité de vague) ; les helper lanes / `s_wqm` / `exec` de quad ne sont pas modélisés.
7. Les SGPR de descripteurs sont résolus **statiquement, dans l'ordre du programme** (pas de
   sensibilité au chemin) ; une écriture qui recouvre un descripteur l'invalide.
8. Pas de DPP/SDWA, pas de géométrie/tessellation (LS/HS/ES/GS/NGG), pas de ray tracing
   (`image_bvh_*`), pas de f64/i64, pas de f16 arithmétique.
9. Le VS ne corrige pas la convention Y/Z de Vulkan : à faire côté pipeline.
10. Pas de `OpSource`/noms de debug dans le module produit.

## 6. Vérifications effectuées

* `spirv-val --target-env vulkan1.1` valide les 17 programmes de test (VS, PS, CS ; wave32/64).
* **Exécution réelle sur GPU** (AMD Radeon RX 5700 XT, RADV, Vulkan 1.4 — NB : RDNA 1 / gfx1010,
  mêmes instructions ALU/mémoire que gfx10.3 pour le sous-ensemble testé ; non testé sur RDNA 2) :
  * `rdna2::tests_gpu` : CS (écriture de buffer, branche divergente EXEC wave32 **et** wave64,
    boucle scalaire, boucle divergente avec `s_cbranch_execnz`, LDS + barrière, modificateurs
    VOP3 / carry, atomiques) ; **batterie ALU de 84 segments × 64 lanes × 2 tailles de vague**
    comparée à un oracle CPU indépendant (5 376 valeurs par taille de vague).
  * `rdna2::tests_gpu_gfx` : rendu 4×4 d'un triangle plein écran : VS (fetch par `s_load` +
    `buffer_load_dwordx4`) + PS couleur constante ; VS (UV) + PS texturé (`v_interp_*`,
    `image_sample`, export `compr`) : 16 pixels corrects dans chaque cas.
* Non exécuté sur GPU : `ps_kill` (`v_cmpx` + `OpKill`, validé seulement par `spirv-val`),
  tbuffer/idxen, `s_buffer_load` (validés par `spirv-val` + tests de structure).

## 7. Tests et outils

```
cargo test -p sb-gpu --lib rdna2                          # décodage, CFG, traduction, spirv-val
cargo test -p sb-gpu --lib rdna2 -- --ignored --nocapture  # + exécution GPU Vulkan
python3 -I tools/rdna2/gen_diff.py 7 8 > /tmp/d.tsv        # encodages aléatoires valides (llvm-mc)
RDNA2_DIFF_TSV=/tmp/d.tsv cargo test -p sb-gpu --test rdna2_diff -- --ignored --nocapture
```

Outils de génération (développement uniquement, Python + `llvm-mc`, sortie versionnée) :

| Script | Produit |
|---|---|
| `tools/rdna2/gen_tables.py` | `src/rdna2/tables.rs` (opcodes, formes, formats MTBUF, hwreg) |
| `tools/rdna2/gen_vectors.py` | `src/rdna2/test_vectors.rs` (268 instructions, wave32/64) |
| `tools/rdna2/gen_programs.py` | `src/rdna2/test_programs.rs` (programmes VS/PS/CS écrits à la main) |
| `tools/rdna2/gen_diff.py` | TSV aléatoire pour le test différentiel |

`spirv-val` absent : les tests de traduction affichent un message et ignorent la validation.
