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

Extensions **SDWA**, **DPP16** et **DPP8** : champs décodés (`Mods::sdwa()`, `dpp16()`, `dpp8()`,
`neg/abs` portés par les opérandes) et désassemblés en syntaxe LLVM (vecteurs figés identiques).
**Non décodées avec leurs opérandes** : les instructions VOP3 « interp » f16 (`v_interp_p1ll_f16`…).

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
* Les 279 vecteurs de test figés (`test_vectors.rs`, assemblés par `llvm-mc`, toutes familles,
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

Mesure par `translator_coverage_report` (une instruction synthétique par opcode des tables ;
« traduit » = pas d'erreur `Unsupported`) :

* **Avant** (commit eaedd64) : **471 / 1338** opcodes (VOP3P, FLAT, GLOBAL, SCRATCH : 0).
* **Après** : **1023 / 1338** opcodes.

| Famille | avant | après | Détail |
|---|---|---|---|
| SOP2 | 41/51 | 48/51 | + `s_lshl/lshr_b64`, `s_ashr_i64`, `s_lshl[1-4]_add_u32`. Non : `s_bfe_*64`, `s_bfm_b64` |
| SOP1 | 33/65 | 47/65 | + `s_wqm`, `s_quadmask_b32`, `s_bcnt0`, `s_ff0_b32`, `s_flbit_i32[_b64]`, `s_bitset0/1`, `s_brev_b64`, `s_ff1_b64`. Non : movrel*, rfe, andn1/orn1 saveexec, bitreplicate |
| SOPC | 16/18 | 16/18 | Non : bitcmp_b64 |
| SOPK | 21/26 | 21/26 | Non : getreg/setreg/call/subvector |
| SOPP | 25/37 | 25/37 | Non : sendmsg, trap, halt… |
| VOP1 | 29/81 | 67/81 | + f16 (rcp, sqrt, rsq, log, exp, floor, ceil, trunc, rndne, fract, sin, cos, cvt u16/i16), f64 (sqrt, rsq, rcp, floor…, frexp, cvt), `v_frexp_*_f32`, `v_ffbl/ffbh_i32`, `v_cvt_rpi/flr`. Non : movrel, swap, norm/sat_pk, frexp f16 |
| VOP2 | 31/47 | 43/47 | + f16 (add/sub/mul/min/max/fmac/fmamk/fmaak/ldexp), `mul_hi_*24`. Non : dot2c/dot4c, pk_fmac, fmac_legacy |
| VOPC | 64/190 | 188/190 | + f16/i16/u16/f64/i64/u64, `class_f32/f16`. Non : `class_f64` |
| VOP3 | 119/325 | 282/325 | + tout ce qui précède en e64, `v_mad_u64_u32`/`i64_i32`, `v_perm_b32`, `v_writelane`, `v_mbcnt`, 64-bit shifts, `v_xor3`, `v_add_lshl`, min3/max3/med3 16 bits, `cvt_pk_*`, `pkrtz`, `pknorm`. Non : cube*, sad/msad/qsad, div_scale/fmas/fixup, lerp, mullit, permlane*, f16 `div_fixup` |
| VOP3P | 0/29 | **29/29** | `v_pk_*` (f16/i16/u16), `v_dot2/4/8`, `v_fma_mix*` avec `op_sel/op_sel_hi/neg_lo/neg_hi` |
| FLAT | 0/54 | 33/54 | via `PhysicalStorageBuffer64` (voir ci-dessous) |
| GLOBAL | 0/57 | 33/57 | load/store ubyte..dwordx4 (+ d16), atomiques 32 bits. Non : atomiques 64 bits, inc/dec/csub/float, `*_addtid` |
| SCRATCH | 0/22 | 0/22 | **non supporté** (mémoire privée par lane) |
| DS | 8/116 | 57/116 | read/write b8/b16/b32/b64/b96/b128, u8/i8/u16/i16 (+d16), `read2/write2[st64]` b32/b64, atomiques 32 bits (add, sub, min, max, and, or, xor, wrxchg, cmpst + rtn), `ds_swizzle` (bitmask et quad_perm), `ds_permute`/`bpermute`, `ds_append/consume`. Non : atomiques 64 bits/float, inc/dec/rsub/mskor, `*_addtid`, GWS/ordered count |
| MUBUF | 43/75 | 53/75 | + `store_byte/short` (RMW atomique), contrôle de bornes. Non : `d16` loads, atomiques 64 bits/inc/dec/float, `tfe`, `lds` |
| MTBUF | 16/16 | 16/16 | formats 32 bits par canal |
| SMEM | 12/16 | 12/16 | + bornes optionnelles |
| MIMG | 10/110 | 60/110 | sample `_c _b _l _lz _cl _o _d _cd` et combinaisons, `gather4[_c][_o]`, `get_lod`, `get_resinfo`, `load[_mip]` (y compris MSAA), `store`, `atomic_*` (r32ui), 1D/2D/3D/cube/arrays, `d16`, `a16`, `tfe/lwe`. Non : `gather4_{l,lz,b,cl}`, `gather4h`, `_g16`, `*_pck`, `store_mip`, `atomic_inc/dec/float`, `msaa_load`, bvh |
| VINTRP | 3/3 | 3/3 | |
| EXP | oui | oui | |
| DPP/SDWA | non | oui | DPP16 (quad_perm, row_shl/shr/ror, mirror, half_mirror, row_share, row_xmask, masks, bound_ctrl), DPP8, SDWA (sélecteurs, sext, dst_sel/unused, clamp/omod, neg/abs). Non : `wave_*`, `row_bcast` |

Toute instruction non traduite → `TranslateError::Unsupported(mnémonique)` ; descripteur non
configuré → `TranslateError::Unresolved` ; jamais de devinette silencieuse.

### Prérequis Vulkan des nouvelles fonctions

| Fonction | Prérequis |
|---|---|
| FLAT/GLOBAL | `bufferDeviceAddress` (Vulkan 1.2), `shaderInt64` ; SPIR-V : `Int64`, `PhysicalStorageBufferAddresses`, extension `SPV_KHR_physical_storage_buffer`, modèle d'adressage `PhysicalStorageBuffer64` (patché dans l'en-tête, le writer SPIR-V partagé ne l'expose pas). Les adresses des buffers sont fournies par SGPR/VGPR (ex. `SgprInit::UserData`). FLAT est traité comme GLOBAL. |
| f64 | `shaderFloat64` (capacité `Float64`) ; sinon le module est refusé par le pilote — la traduction n'échoue pas. `clamp`/`omod` sur f64 : `Unsupported`. |
| f16 / packed | **aucun** : émulé en f32 (`PackHalf2x16`/`UnpackHalf2x16`), exact pour add/sub/mul ; fma/mad peuvent différer d'1 ulp f16. |
| DS `swizzle/permute/bpermute`, DPP, `v_readlane`, `v_mbcnt` | sous-groupe `GroupNonUniform{,Ballot,Shuffle}` (Vulkan 1.1) avec taille de sous-groupe = taille de vague. |
| MIMG | `ImageQuery` (`get_lod`, `get_resinfo`, offsets émulés), `MinLod` (`_cl`), `ImageGatherExtended` (gather `_o`), `StorageImageWriteWithoutFormat` + `Image1D`/`Sampled1D` selon usage. |
| Bornes | `TranslateConfig::buffer_sizes` ; sans entrée, aucun contrôle. |

## 5. Hypothèses et limites connues

1. **Taille de sous-groupe = taille de vague** (32 ou 64) pour tout programme qui modifie EXEC,
   écrit un masque, ou utilise DPP / `ds_swizzle` / `permute` / `readlane`. Sur GPU AMD :
   `VK_EXT_subgroup_size_control` avec `requiredSubgroupSize` (fait par les tests CS). Pour un
   étage graphique, le pilote choisit : wave64 avec un shader wave32 donne des résultats faux.
2. Arrondis et dénormaux suivent SPIR-V/le pilote (`s_round_mode`/`s_denorm_mode` ignorés) ;
   `v_sin/cos` = `sin/cos(2πx)` ; `v_rcp/rsq/exp/log` à la précision du pilote ;
   `v_cvt_pkrtz_f16_f32` = `PackHalf2x16`.
3. f16 : les écritures VOP3 `op_sel[3]` placent le résultat dans la moitié haute en préservant la
   basse ; sinon la moitié haute est mise à zéro.
4. **Contrôle de bornes** : uniquement si `buffer_sizes` fournit `(set, binding, octets)` ;
   alors lectures hors bornes = 0, écritures/atomiques ignorés (granularité dword, MUBUF et
   `s_buffer_load`). Sinon on compte sur `robustBufferAccess`. `num_records`/`stride` du V# ne
   sont jamais lus depuis les registres. Formats `buffer_load_format_*` : f32 par canal supposé.
5. `v_interp_p1/p2` : Vulkan fournit la valeur interpolée complète ; les VGPR i/j sont ignorés.
6. Offsets `image_sample*_o` : Vulkan interdit un `Offset` dynamique hors gather
   (VUID-RuntimeSpirv-Offset-10213) ; ils sont émulés par `coord += offset / taille(mip 0)` —
   exact pour le mip 0 seulement. `gather4` n'accepte ni Lod, ni Bias, ni MinLod en SPIR-V :
   `gather4_{l,lz,b,cl}` sont refusés ; `gather4` hors fragment utilise un LOD implicite
   (indéfini hors PS sauf dérivées de calcul) — à n'utiliser qu'en PS ou avec `ComputeDerivativeGroup`.
7. `image_sample` en CS/VS utilise `Lod 0` ; `_d` utilise `Grad` ; `d16` empaquette 2 composantes
   par VGPR ; `tfe/lwe` écrivent 0 dans le registre de statut (toujours « résident »).
8. SDWA/DPP : lectures lanes invalides = `bound_ctrl ? 0 : écriture sautée` ; `fi` ignoré ;
   sémantique **non vérifiable sur matériel** (voir §6).
9. Pas de géométrie/tessellation (LS/HS/ES/GS/NGG), pas de ray tracing (`image_bvh_*`), pas de
   SCRATCH, pas de GWS/GDS, pas d'atomiques 64 bits, pas d'`MSAA` en échantillonnage.
10. Les SGPR de descripteurs sont résolus statiquement dans l'ordre du programme.
11. Le VS ne corrige pas la convention Y/Z de Vulkan ; pas de `OpSource`/noms de debug.

## 6. Vérifications effectuées

* `spirv-val --target-env vulkan1.1` valide les 28 programmes de test (VS, PS, CS ; wave32/64).
* **Exécution réelle sur GPU** (AMD Radeon RX 5700 XT, RADV, Vulkan 1.4 — NB : RDNA 1 / gfx1010,
  mêmes instructions ALU/mémoire que gfx10.3 pour le sous-ensemble testé ; non testé sur RDNA 2) :
  * `rdna2::tests_gpu` : CS (écriture de buffer, branche divergente EXEC wave32 **et** wave64,
    boucle scalaire, boucle divergente avec `s_cbranch_execnz`, LDS + barrière, modificateurs
    VOP3 / carry, atomiques) ; **batterie ALU de 84 segments × 64 lanes × 2 tailles de vague**
    comparée à un oracle CPU indépendant (5 376 valeurs par taille de vague).
  * `rdna2::tests_gpu_gfx` : rendu 4×4 d'un triangle plein écran : VS (fetch par `s_load` +
    `buffer_load_dwordx4`) + PS couleur constante ; VS (UV) + PS texturé (`v_interp_*`,
    `image_sample`, export `compr`) : 16 pixels corrects dans chaque cas.
* **Extensions (commit courant)** : `rdna2::tests_gpu_ext` exécute sur la RX 5700 XT :
  * mémoire 64 bits (`global_*` load/store 8/32 bits, adresse en VGPR et en SGPR+offset, atomique) ;
  * batterie f16 / packed / dot / fma_mix / i16 / `class` / frexp / `mad_u64` / `perm` / f64 : 50
    segments × 64 lanes × wave32 **et** wave64 contre un oracle CPU ;
  * DS (u8/i8/u16, `read2`, `write_b8`, atomique `rtn`, `append`, `bpermute`, `permute`,
    `swizzle`) + sous-groupes (`v_mbcnt`, `v_writelane`, `s_wqm`, décalages 64 bits…) : 26
    segments, wave32 et wave64 ;
  * MIMG en CS (échantillonnage `lz/l/o/d`, d16, gather4 (4 texels), load, tfe, resinfo, store,
    atomiques r32ui) sur une texture 2×2 ;
  * DPP / SDWA (16 segments, wave32 et wave64, **oracle = lecture du guide ISA**, pas le matériel) ;
  * contrôle de bornes, stores octet/short de buffer.
* `ps_mimg_ext` (PS : `_b _c _cl _d_cl _o _c_l`, gather `_c_o`, `get_lod`, a16, tfe, d16, 3D, cube,
  1D array, MSAA load) n'est validé que par `spirv-val` (pas rendu sur GPU).
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
| `tools/rdna2/gen_vectors.py` | `src/rdna2/test_vectors.rs` (279 instructions, wave32/64, DPP/SDWA inclus) |
| `tools/rdna2/gen_programs.py` | `src/rdna2/test_programs.rs` (28 programmes VS/PS/CS écrits à la main) |
| `tools/rdna2/gen_diff.py` | TSV aléatoire pour le test différentiel |

`spirv-val` absent : les tests de traduction affichent un message et ignorent la validation.
