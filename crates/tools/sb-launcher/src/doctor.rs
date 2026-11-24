//! `scarletbridge doctor` : vérifie que la machine remplit les conditions dont le bridge dépend,
//! indépendamment de tout titre (limites noyau, adresses libres, mémoire, CPU, Vulkan, SDL3,
//! affichage, audio, droits d'écriture) et indique quoi corriger.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Ok,
    Info,
    Warn,
    Fail,
}

#[derive(Debug, Clone)]
pub struct Finding {
    pub level: Level,
    pub name: &'static str,
    pub detail: String,
    pub hint: Option<String>,
}

fn f(level: Level, name: &'static str, detail: impl Into<String>, hint: Option<&str>) -> Finding {
    Finding {
        level,
        name,
        detail: detail.into(),
        hint: hint.map(str::to_string),
    }
}

fn read_trim(path: &str) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
}

/// `(majeur, mineur)` d'une version de noyau « 6.8.0-arch1 ».
pub fn parse_kernel_version(release: &str) -> Option<(u32, u32)> {
    let mut it = release.split(|c: char| !c.is_ascii_digit());
    let major = it.next()?.parse().ok()?;
    let minor = it.next()?.parse().ok()?;
    Some((major, minor))
}

/// Valeur `MemAvailable` de `/proc/meminfo` en Kio.
pub fn mem_available_kib(meminfo: &str) -> Option<u64> {
    meminfo
        .lines()
        .find(|l| l.starts_with("MemAvailable:"))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|v| v.parse().ok())
}

fn check_kernel() -> Finding {
    let release = read_trim("/proc/sys/kernel/osrelease").unwrap_or_default();
    match parse_kernel_version(&release) {
        Some((major, minor)) if (major, minor) >= (4, 17) => f(
            Level::Ok,
            "noyau",
            format!("Linux {release} (MAP_FIXED_NOREPLACE disponible)"),
            None,
        ),
        Some(_) => f(
            Level::Warn,
            "noyau",
            format!("Linux {release} : antérieur à 4.17"),
            Some(
                "MAP_FIXED_NOREPLACE manque : les chargements d'image peuvent écraser des mappings",
            ),
        ),
        None => f(
            Level::Info,
            "noyau",
            format!("version illisible ({release:?})"),
            None,
        ),
    }
}

fn check_map_count() -> Finding {
    match read_trim("/proc/sys/vm/max_map_count").and_then(|v| v.parse::<u64>().ok()) {
        Some(n) if n >= 262_144 => f(Level::Ok, "vm.max_map_count", n.to_string(), None),
        Some(n) => f(
            Level::Warn,
            "vm.max_map_count",
            format!("{n} (faible)"),
            Some(
                "un titre qui fait beaucoup de mappings échouera : sudo sysctl -w vm.max_map_count=1048576",
            ),
        ),
        None => f(Level::Info, "vm.max_map_count", "illisible", None),
    }
}

fn check_address_space() -> Vec<Finding> {
    let mut out = Vec::new();
    for base in [0x1_4000_0000usize, 0x1_8000_0000usize] {
        let len = 64 << 20;
        // SAFETY: sonde `PROT_NONE` sans remplacement (`MAP_FIXED_NOREPLACE`), libérée aussitôt.
        let p = unsafe {
            libc::mmap(
                base as *mut libc::c_void,
                len,
                libc::PROT_NONE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_FIXED_NOREPLACE,
                -1,
                0,
            )
        };
        if p == libc::MAP_FAILED {
            out.push(f(
                Level::Warn,
                "adresse de chargement",
                format!("{base:#x} occupée"),
                Some("une image sans relocations ne pourra pas se charger : vérifiez LD_PRELOAD et les bibliothèques préchargées"),
            ));
        } else {
            // SAFETY: libération de la sonde que nous venons de créer.
            unsafe { libc::munmap(p, len) };
            out.push(f(
                Level::Ok,
                "adresse de chargement",
                format!("{base:#x} libre"),
                None,
            ));
        }
    }
    out
}

fn check_memory() -> Finding {
    match std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|m| mem_available_kib(&m))
    {
        Some(kib) => {
            let gib = kib as f64 / 1_048_576.0;
            if gib >= 8.0 {
                f(
                    Level::Ok,
                    "mémoire disponible",
                    format!("{gib:.1} Gio"),
                    None,
                )
            } else {
                f(
                    Level::Warn,
                    "mémoire disponible",
                    format!("{gib:.1} Gio"),
                    Some(
                        "un titre Series X peut demander jusqu'à 13 Gio : fermez d'autres applications",
                    ),
                )
            }
        }
        None => f(Level::Info, "mémoire disponible", "illisible", None),
    }
}

fn rlimit(resource: libc::__rlimit_resource_t) -> Option<(u64, u64)> {
    let mut lim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `lim` est une structure de sortie valide.
    (unsafe { libc::getrlimit(resource, &mut lim) } == 0).then_some((lim.rlim_cur, lim.rlim_max))
}

fn check_limits() -> Vec<Finding> {
    let mut out = Vec::new();
    match rlimit(libc::RLIMIT_AS) {
        Some((cur, _)) if cur == libc::RLIM_INFINITY => out.push(f(
            Level::Ok,
            "limite d'espace d'adressage",
            "illimitée",
            None,
        )),
        Some((cur, _)) => out.push(f(
            Level::Fail,
            "limite d'espace d'adressage",
            format!("{} Gio", cur >> 30),
            Some("les réservations de mémoire virtuelle échoueront : ulimit -v unlimited"),
        )),
        None => {}
    }
    if let Some((cur, _)) = rlimit(libc::RLIMIT_NOFILE) {
        if cur >= 4096 {
            out.push(f(Level::Ok, "fichiers ouverts", cur.to_string(), None));
        } else {
            out.push(f(
                Level::Warn,
                "fichiers ouverts",
                cur.to_string(),
                Some("un titre qui ouvre beaucoup d'archives peut échouer : ulimit -n 65536"),
            ));
        }
    }
    out
}

fn check_cpu() -> Finding {
    use std::arch::is_x86_feature_detected as has;
    let wanted: [(&str, bool); 8] = [
        ("sse4.2", has!("sse4.2")),
        ("popcnt", has!("popcnt")),
        ("avx", has!("avx")),
        ("avx2", has!("avx2")),
        ("fma", has!("fma")),
        ("bmi1", has!("bmi1")),
        ("bmi2", has!("bmi2")),
        ("f16c", has!("f16c")),
    ];
    let missing: Vec<&str> = wanted
        .iter()
        .filter(|(_, ok)| !ok)
        .map(|(n, _)| *n)
        .collect();
    if missing.is_empty() {
        f(
            Level::Ok,
            "processeur",
            "AVX2/FMA/BMI2/F16C présents (niveau Zen 2)",
            None,
        )
    } else {
        f(
            Level::Warn,
            "processeur",
            format!("manque : {}", missing.join(", ")),
            Some(
                "les binaires Series X supposent un CPU Zen 2 : une instruction manquante donne SIGILL",
            ),
        )
    }
}

fn check_vulkan() -> Vec<Finding> {
    match sb_gpu::vulkan::GpuContext::new(false) {
        Ok(ctx) => {
            let c = &ctx.caps;
            let mut out = vec![f(
                Level::Ok,
                "Vulkan",
                format!(
                    "{} (Vulkan {}.{}.{})",
                    c.device_name, c.api_version_major, c.api_version_minor, c.api_version_patch
                ),
                None,
            )];
            out.push(if c.has_external_memory_host {
                f(Level::Ok, "VK_EXT_external_memory_host", "disponible (mémoire unifiée)", None)
            } else {
                f(
                    Level::Warn,
                    "VK_EXT_external_memory_host",
                    "absente",
                    Some("la mémoire GPU unifiée ne sera pas partagée sans copie : pilote Mesa/NVIDIA récent requis"),
                )
            });
            out.push(if c.has_bc_compression {
                f(Level::Ok, "textures BC1-BC7", "supportées", None)
            } else {
                f(
                    Level::Warn,
                    "textures BC1-BC7",
                    "non supportées",
                    Some("les textures compressées seront incorrectes"),
                )
            });
            out.push(if c.has_ray_tracing {
                f(
                    Level::Info,
                    "lancer de rayons",
                    "disponible (non utilisé par le bridge)",
                    None,
                )
            } else {
                f(
                    Level::Info,
                    "lancer de rayons",
                    "indisponible (BVH Xbox sans effet)",
                    None,
                )
            });
            out
        }
        Err(e) => vec![f(
            Level::Fail,
            "Vulkan",
            format!("initialisation impossible : {e}"),
            Some(
                "installez un pilote Vulkan (mesa-vulkan-drivers / vulkan-radeon / nvidia-utils) ; testez avec vulkaninfo",
            ),
        )],
    }
}

fn check_sdl3() -> Finding {
    if sb_gpu::sdl3::Sdl3Lib::load().is_some() {
        f(Level::Ok, "SDL3", "libSDL3.so.0 chargeable", None)
    } else {
        f(
            Level::Warn,
            "SDL3",
            "libSDL3.so.0 introuvable",
            Some(
                "sans SDL3 : pas de fenêtre ni de manette (mode headless uniquement) ; installez sdl3",
            ),
        )
    }
}

fn check_display() -> Finding {
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
    let x11 = std::env::var_os("DISPLAY").is_some();
    match (wayland, x11) {
        (true, _) => f(Level::Ok, "affichage", "Wayland", None),
        (false, true) => f(Level::Ok, "affichage", "X11", None),
        _ => f(
            Level::Warn,
            "affichage",
            "aucun serveur graphique détecté",
            Some("lancez avec --headless, ou depuis une session graphique"),
        ),
    }
}

fn check_audio() -> Finding {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
    let found = runtime.as_deref().is_some_and(|d| {
        ["pipewire-0", "pulse/native"]
            .iter()
            .any(|s| d.join(s).exists())
    });
    if found {
        f(Level::Ok, "audio", "PipeWire/PulseAudio détecté", None)
    } else {
        f(
            Level::Warn,
            "audio",
            "aucun serveur audio détecté",
            Some("le son sera muet ; démarrez PipeWire (pipewire, wireplumber)"),
        )
    }
}

/// Dossier de données du bridge (`~/.local/share/scarletbridge` ou `$XDG_DATA_HOME/scarletbridge`).
pub fn data_dir() -> Option<PathBuf> {
    if let Some(x) = std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(x).join("scarletbridge"));
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share/scarletbridge"))
}

fn free_bytes(path: &Path) -> Option<u64> {
    let mut probe = path.to_path_buf();
    while !probe.exists() {
        if !probe.pop() {
            return None;
        }
    }
    let c = std::ffi::CString::new(probe.to_string_lossy().as_bytes()).ok()?;
    // SAFETY: `statvfs` remplit une structure de sortie valide à partir d'un chemin C valide.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    (unsafe { libc::statvfs(c.as_ptr(), &mut st) } == 0)
        .then(|| st.f_bavail as u64 * st.f_frsize as u64)
}

fn check_data_dir() -> Finding {
    let Some(dir) = data_dir() else {
        return f(
            Level::Warn,
            "données",
            "HOME non défini",
            Some("définissez HOME ou XDG_DATA_HOME"),
        );
    };
    let writable =
        std::fs::create_dir_all(&dir).is_ok() && std::fs::write(dir.join(".doctor"), b"ok").is_ok();
    let _ = std::fs::remove_file(dir.join(".doctor"));
    if !writable {
        return f(
            Level::Fail,
            "données",
            format!("{} non inscriptible", dir.display()),
            Some("les sauvegardes et le cache échoueront : vérifiez les droits"),
        );
    }
    match free_bytes(&dir) {
        Some(b) if b >= 2 << 30 => f(
            Level::Ok,
            "données",
            format!("{} ({} Gio libres)", dir.display(), b >> 30),
            None,
        ),
        Some(b) => f(
            Level::Warn,
            "données",
            format!("{} ({} Mio libres)", dir.display(), b >> 20),
            Some("espace faible pour les sauvegardes et le cache de pipelines"),
        ),
        None => f(Level::Ok, "données", dir.display().to_string(), None),
    }
}

fn check_teb() -> Finding {
    // `arch_prctl(ARCH_SET_GS)` sur un thread jetable : indispensable au TEB/PEB Windows.
    let ok = std::thread::spawn(|| sb_loader::init_thread_teb(0).is_ok())
        .join()
        .unwrap_or(false);
    if ok {
        f(Level::Ok, "TEB/PEB (arch_prctl)", "GS configurable", None)
    } else {
        f(
            Level::Fail,
            "TEB/PEB (arch_prctl)",
            "arch_prctl(ARCH_SET_GS) refusé",
            Some(
                "un bac à sable (seccomp, gVisor, WSL1) bloque arch_prctl : exécutez hors de ce conteneur",
            ),
        )
    }
}

fn check_offline() -> Finding {
    let v = std::env::var("SB_OFFLINE_MODE").unwrap_or_else(|_| "1".into());
    if v == "0" {
        f(
            Level::Info,
            "mode hors ligne",
            "désactivé (SB_OFFLINE_MODE=0)",
            None,
        )
    } else {
        f(Level::Ok, "mode hors ligne", "strict (défaut)", None)
    }
}

/// Exécute toutes les vérifications.
pub fn collect() -> Vec<Finding> {
    let mut v = vec![check_kernel(), check_map_count()];
    v.extend(check_address_space());
    v.push(check_memory());
    v.extend(check_limits());
    v.push(check_cpu());
    v.push(check_teb());
    v.extend(check_vulkan());
    v.push(check_sdl3());
    v.push(check_display());
    v.push(check_audio());
    v.push(check_data_dir());
    v.push(check_offline());
    v
}

/// Affiche le rapport et retourne le code de sortie (1 s'il y a au moins un échec).
pub fn run() -> i32 {
    println!("ScarletBridge doctor — vérification de l'environnement\n");
    let findings = collect();
    for x in &findings {
        let tag = match x.level {
            Level::Ok => " OK ",
            Level::Info => "INFO",
            Level::Warn => "WARN",
            Level::Fail => "FAIL",
        };
        println!("[{tag}] {:<28} {}", x.name, x.detail);
        if let Some(h) = &x.hint {
            println!("       → {h}");
        }
    }
    let fails = findings.iter().filter(|x| x.level == Level::Fail).count();
    let warns = findings.iter().filter(|x| x.level == Level::Warn).count();
    println!("\n{fails} échec(s), {warns} avertissement(s).");
    i32::from(fails > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kernel_version_parsing() {
        assert_eq!(parse_kernel_version("6.8.0-arch1-1"), Some((6, 8)));
        assert_eq!(parse_kernel_version("4.17.0"), Some((4, 17)));
        assert_eq!(parse_kernel_version("abc"), None);
    }

    #[test]
    fn meminfo_parsing() {
        let sample = "MemTotal: 16000000 kB\nMemAvailable:    8388608 kB\nBuffers: 1 kB\n";
        assert_eq!(mem_available_kib(sample), Some(8_388_608));
        assert_eq!(mem_available_kib("MemTotal: 1 kB"), None);
    }

    #[test]
    fn collect_never_panics_and_reports_the_essentials() {
        let findings = collect();
        let names: Vec<&str> = findings.iter().map(|x| x.name).collect();
        for needed in [
            "noyau",
            "mémoire disponible",
            "processeur",
            "Vulkan",
            "données",
        ] {
            assert!(names.contains(&needed), "{needed} manquant : {names:?}");
        }
        // Chaque avertissement ou échec propose une action.
        for x in findings
            .iter()
            .filter(|x| matches!(x.level, Level::Warn | Level::Fail))
        {
            assert!(x.hint.is_some(), "{} sans indication", x.name);
        }
    }
}
