# Pipelines GPU natifs RDNA 2 (`Rdna2Native`) — modèle de liaison

Tranche verticale : un programme GPU **RDNA 2 brut** (VS + PS ou CS), écrit à la main, est traduit
(`sb-gpu::rdna2`, voir `docs/rdna2.md`) puis exécuté par le bridge via `d3d12_xs`, **sans aucun jeu
et sans format de conteneur commercial** : le conteneur `SBR2` est le nôtre et est versionné.

Code : `crates/sb-gpu/src/rdna2_native.rs` (conteneur + traduction), `crates/sb-gpu/src/renderer/native.rs`
(pipelines Vulkan, images, descripteurs), `crates/sb-hle/src/d3d12_native.rs` (arguments racine, lecture
des V#/T#/S#, résolution). Harnais : `tests/harness/rdna2-native` (PE32+).

## 1. Conteneur `SBR2` v1 (`ShaderFormat::Rdna2Native`)

Dwords little-endian : en-tête de 16 dwords (`magic "SBR2"`, `version = 1`, `header_dwords`, `stage`
0 VS / 1 PS / 2 CS, `wave` 32|64, `local_size[3]`, `lds_dwords`, `num_user_sgprs` ≤ 32,
`num_input_vgprs`, `sys_sgpr_flags`, `n_resources` ≤ 32, `code_dwords`, `total_dwords`, 0), puis
`n_resources` enregistrements de 5 dwords `(kind, loc_kind, loc_a, loc_b, stride)`, puis le code.
La disposition exacte est dans l'en-tête de module de `rdna2_native.rs` ; le lecteur valide toutes les
tailles et refuse une version inconnue (`NativeError::Version`).

| Champ | Valeurs |
|---|---|
| `kind` | 0 = V# (tampon ; `stride` pour `idxen`), 1 = T# (image), 2 = S# (échantillonneur) |
| `loc_kind` | 0 = **Table** (`loc_a` = SGPR de base de la paire pointeur, `loc_b` = décalage en octets), 1 = **Sgprs** (descripteur dans les user-SGPR à partir de `loc_a`), 2 = **Pointer** (paire de SGPR = adresse d'un tampon, cible de `s_load_*`) |

Registres d'entrée : `s0..s(N-1)` = user data ; CS : identifiants de groupe x/y/z (selon
`sys_sgpr_flags`) dans les SGPR suivants. VS : `v0` = `VertexIndex`, `v1` = `InstanceIndex`. PS : `v0,v1` = 0
(le traducteur utilise la valeur interpolée complète de Vulkan), `v2..v5` = `FragCoord`. CS : `v0..v2` =
identifiant local. `translate_shader` (`shader.rs`) reconnaît la magie `SBR2` sans changer DXBC/DXIL ;
`ShaderContainer::parse` renvoie `ShaderFormat::Rdna2Native`.

## 2. User-SGPR et arguments racine

La signature racine (`D3D12SerializeRootSignature` du bridge, ou un blob `DXBC`/`RTS0` complet)
donne la nature de chaque paramètre ; `root_signature::pack_user_sgprs` les compacte dans les 32
user-SGPR (une signature qui déborde est refusée : le spill n'est pas modélisé) :

| Paramètre racine | User-SGPR | Valeur écrite |
|---|---|---|
| Table de descripteurs | 2 dwords | adresse GPU du premier descripteur du tas (= handle ; l'adresse mémoire réelle) |
| Constantes 32 bits (N) | N dwords | les valeurs (`SetXRoot32BitConstant[s]`, avec `DestOffset`) |
| CBV / SRV / UAV racine | 2 dwords | adresse virtuelle du tampon |

Les arguments sont suivis par la liste de commandes ; au `Draw` / `Dispatch` ils sont capturés dans les 32
mots de user data de la commande. **Chaque étage reçoit le même tableau** (pas de visibilité par étage) et lit
ses `num_user_sgprs` premiers mots. Côté Vulkan : SSBO `(set 0, binding 0)`, chargé par le prologue du shader
(`SgprInit::UserData(i)`). Un shader dont les `s_load` visent `s[0:1] + offset` voit ainsi le contenu de la table.

### 2 bis. Tables de descripteurs découvertes par le traducteur (compute natif)

Pour un compute dont la signature racine est connue (`RootSigLayout`), les paramètres **table**
(user-SGPR `first_sgpr[i]`, paire 64 bits) sont déclarés au traducteur (`TranslateConfig::tables`, voir
`docs/rdna2.md`). `d3d12_native::discover_root_tables` traduit le programme une première fois et en
tire la liste des entrées `(table, offset)` réellement utilisées (V#, T#, S#, y compris via
`s_add_u32/s_addc_u32 + s_load_dwordx16`) ; chacune devient un `NativeResource` `Table` (liaison
`base + indice`, comme avant) et remplace les ressources `Table`/`Pointer` heuristiques de ces SGPR.
Les paramètres CBV/SRV/UAV racine restent des `Pointer` (données). Sans signature racine connue,
l'ancienne inférence par motifs `s_load` est conservée (aucune information n'est inventée).
**Plan user-SGPR déduit de l'usage.** `pack_user_sgprs` fournit le plan *réel* (`first_sgpr`) ; les
tables du plan sont déclarées d'abord. Si le shader charge ses descripteurs depuis une autre paire de
user-SGPR (hors CBV/SRV/UAV du plan), cette paire est une table **inférée par l'usage**
(`TranslateConfig::infer_tables`). La glue l'associe, dans l'ordre de première utilisation, aux
paramètres **table** de la signature que le shader n'utilise pas à leur emplacement du plan
(usage ↔ ordre des paramètres table), journalise `FSPX Compute : plan user-SGPR déduit de l'usage
… paramètre racine #0 (plan réel s0) -> s[4:5]` (`sb_warn`) et mémorise le plan `(paramètre, user-SGPR)`
dans `NativePso::user_plan` ; `NativeListState::note_pso` l'installe dans `RootBinding::plan_override`
(`user_data()` y place alors le pointeur de table, sans toucher `d3d12_xs.rs`). Rien n'est inventé :
plus de tables utilisées que de paramètres table libres → `plan user-SGPR : le shader utilise N
table(s) … mais la signature n'a que M paramètre(s) table libre(s)` ; paire déduite qui recouvre des
constantes racine → erreur nommant les deux paramètres. L'ordre est déterministe (ordre du programme).
Les pointeurs hérités de l'heuristique pour des bases hors CBV/SRV/UAV du plan sont supprimés (ils
provoquaient les confusions de natures et la cascade `image_load` après `s_load_dwordx16`).

Échecs : le message du traducteur est renvoyé tel quel avec la liste des user-SGPR de table
(`tables de descripteurs (user-SGPR [0, 4]) : s_load_dwordx4 depuis s[8:9] (…)`) ; au-delà de 32
entrées, `n entrées de table + m autres ressources > 32`. Une table dont les mots sont lus comme
données (`s_load_dword` utilisé arithmétiquement) n'est pas supportée : ces mots valent 0.

## 3. V#, T#, S# : mots matériels → liaisons Vulkan

Au `ExecuteCommandLists`, pour chaque ressource déclarée, le bridge lit les mots à l'adresse
`table + décalage` (ou dans les user-SGPR, ou suit le pointeur racine). **Seules les adresses contenues dans un
tas de descripteurs du bridge (`DESC_HEAP_RANGES`) ou dans une ressource du bridge (`ALLOCATED_BUFFER_SIZES`)
sont lues.** Les mots sont la source de vérité (format, dimensions, adresse) :

* **V#** (`decode_buffer`) → copie des octets `[adresse, adresse + num_records·max(stride,1))` bornés à la
  ressource propriétaire → SSBO à la liaison de la ressource (copie entrante ; après un *dispatch*, les tampons
  modifiés sont recopiés en mémoire). Un pointeur racine désigne le reste de la ressource (≤ 16 Mio).
* **Images de stockage** : un T# lié par `image_store`/`image_load`/`image_atomic_*` (liaison
  `storage_image`) devient une image `STORAGE` (si le format le permet) en layout `GENERAL` pendant le
  dispatch : transition `SHADER_READ_ONLY -> GENERAL`, dispatch, barrière, copie image → tampon hôte,
  retour en `SHADER_READ_ONLY_OPTIMAL`. `VulkanRenderer::dispatch_native_ex` renvoie les texels
  linéaires serrés (`NativeDispatchOutput::images`) ; le bridge les recopie en mémoire invitée si la
  ressource est **linéaire** (aucun re-tiling connu : pour une ressource tuilée, l'image reste côté GPU
  et un avertissement est journalisé). Formats : GFX10 56/57/60/61 (RGBA8 UNORM/SNORM/UINT/SINT), 130
  (SRGB, échantillonnage seul), 71, 77, 29, 22, 21, 20, 64, 13 ; dimensions 2D, 2D array
  (`create_native_image_dim`, `NativeImageDim`) et 3D, mip 0, couche de base 0.
* **T#** (`decode_image`) : adresse (>>8) + format GFX10 + dimensions → ressource du bridge. Résolution : (1) la
  table `descriptors` (handle → SRV) donne la ressource, recoupée avec l'adresse du T# ; (2) sinon plage dont
  la base tronquée à 256 octets ou le contenu inclut l'adresse (ambiguïté signalée). Puis table
  `(adresse, largeur, hauteur, VkFormat) → image Vulkan` ; le contenu (mémoire de la ressource, détuilée si
  marquée tuilée) est téléversé et **re-téléversé quand son hachage change**. Le swizzle `DST_SEL` du T#
  devient le `VkComponentMapping` de la vue.
* **S#** (`decode_sampler`) → `VkSampler` (filtres, adressage, LOD, anisotropie, comparaison, bordure).

Un descripteur illisible ou non supporté lie une ressource de repli (zéros / texture blanche /
échantillonneur par défaut) avec un avertissement : jamais de lecture hors mémoire du bridge.

## 4. Pipelines Vulkan

`VulkanRenderer::create_native_graphics_pipeline` / `create_native_compute_pipeline` : layout de set dérivé
des liaisons **réellement utilisées** par les modules (user data = binding 0 ; ressources `1..=32` VS/CS,
`33..=64` PS ; V# → `STORAGE_BUFFER`, T# → `SAMPLED_IMAGE` (ou `STORAGE_IMAGE` si le programme écrit/atomise l'image), S# →
`SAMPLER`). Pas d'attributs de sommet (le VS
lit ses sommets par V#). Les ressources voyagent dans `RenderCommand::NativeDraw` / `dispatch_native`. Un CS
qui modifie EXEC impose la taille de sous-groupe (`subgroupSizeControl`) ou est refusé.

## 5. Limites

* T# : 2D, 2D array, 3D (mip 0, couche de base 0) ; formats GFX10 56/57/60/61/130 (RGBA8), 71, 77, 29, 22,
  21, 20, 64, 13 seulement (le layout console explicite des textures n'est pas documenté : rien n'est
  deviné). Images de stockage : **compute seulement** (une image de stockage dans un étage graphique est
  refusée à la création du pipeline : la transition `GENERAL` est impossible dans une passe de rendu) ;
  recopie sortante pour les ressources linéaires seulement. Le dimensionnement des textures committed du
  bridge (`compute_resource_byte_len`, `d3d12_xs.rs`, hors périmètre) ignore la profondeur / les couches :
  un T# 2D array ou 3D qui dépasse la ressource est refusé (`dépasse la mémoire de la ressource`) ; les
  tests GPU du renderer couvrent 2D array et 3D, le harnais PE32+ la 2D.
* Pas de `DrawIndexedInstanced` avec un pipeline natif ; pas de MRT ; état raster/blend/depth du PSO D3D12.
* Copie entrante/sortante (pas de zéro-copie) ; recopie sortante pour les *dispatch* seulement.
* Lecture des descripteurs à l'exécution de la liste (pas à l'enregistrement).
* Les programmes suivent l'hypothèse « taille de sous-groupe = taille de vague » de `docs/rdna2.md` §5.
* Non validé avec les couches de validation Khronos (non installées sur la machine de test).

## 6. Programmes d'exemple (`rdna2_native::samples`, assembleur `llvm-mc -triple=amdgcn -mcpu=gfx1030 -mattr=+wavefrontsize32`)

PS « teinte » (T# = table s[0:1]+0x20, S# = table s[2:3], teinte = `s4..s7`) :
```
s_load_dwordx8 s[8:15], s[0:1], 0x20   ; s_load_dwordx4 s[16:19], s[2:3], 0x0
v_interp_p1/p2_f32 …                   ; image_sample v[4:7], v[2:3], s[8:15], s[16:19] dmask:0xf dim:SQ_RSRC_IMG_2D
v_mul_f32 v4..v7, s4..s7, v4..v7       ; v_cvt_pkrtz_f16_f32 ; exp mrt0 … done compr vm
```
CS affine (`out[i] = f32(in[i])·s4 + biais`, V# entrée/sortie = table s[0:1]+0/+0x20, biais lu par `s_load_dword s6, s[2:3]`
dans le CBV racine, `s5` = groupe x) :
```
s_load_dwordx4 s[8:11], s[0:1], 0x0 ; s_load_dwordx4 s[12:15], s[0:1], 0x20 ; s_load_dword s6, s[2:3], 0x0
s_lshl_b32 s7, s5, 6 ; v_add_nc_u32 v0, s7, v0 ; v_lshlrev_b32 v1, 2, v0
buffer_load_dword v2, v1, s[8:11], 0 offen ; v_cvt_f32_u32 v3, v2 ; v_fma_f32 v3, v3, s4, s6
buffer_store_dword v3, v1, s[12:15], 0 offen
```
Le VS est `vs_uv_passthrough` de `docs/rdna2.md` (sommets de 32 octets : position, uv).
