//! Parseur minimal de fichiers `.def` (Module-Definition Files) pour générer des stubs sans DLL source.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

use crate::pe::ExportedSymbol;

#[derive(Debug, Clone)]
pub struct DefFile {
    pub library_name: Option<String>,
    pub exports: Vec<ExportedSymbol>,
}

pub fn parse_def_file(path: &Path) -> Result<DefFile> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("impossible de lire le fichier .def : {}", path.display()))?;
    Ok(parse_def_str(&text))
}

pub fn parse_def_str(text: &str) -> DefFile {
    let mut library_name = None;
    let mut in_exports = false;
    let mut exports = Vec::new();

    for raw_line in text.lines() {
        let line = raw_line.split(';').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }

        let upper = line.to_ascii_uppercase();
        if upper.starts_with("LIBRARY") {
            let rest = line[7..].trim().trim_matches('"');
            if !rest.is_empty() {
                library_name = Some(rest.to_string());
            }
            in_exports = false;
            continue;
        }
        if upper == "EXPORTS" {
            in_exports = true;
            continue;
        }
        if upper.starts_with("SECTIONS")
            || upper.starts_with("DESCRIPTION")
            || upper.starts_with("VERSION")
        {
            in_exports = false;
            continue;
        }

        if in_exports {
            let mut tokens = line.split_whitespace();
            let Some(first) = tokens.next() else {
                continue;
            };
            let name = first.split('=').next().unwrap_or(first).to_string();
            let mut ordinal = None;
            for tok in tokens {
                if let Some(ord_str) = tok.strip_prefix('@') {
                    ordinal = ord_str.parse::<u16>().ok();
                }
            }
            exports.push(ExportedSymbol {
                name,
                ordinal,
                rva: 0,
                forwarder: None,
            });
        }
    }

    DefFile {
        library_name,
        exports,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_standard_def() {
        let content = r#"
; Commentaire
LIBRARY "sample_xbox.dll"
EXPORTS
    SampleInit @1
    SampleShutdown @2
    ?CppFunc@@YAXXZ
"#;
        let def = parse_def_str(content);
        assert_eq!(def.library_name.as_deref(), Some("sample_xbox.dll"));
        assert_eq!(def.exports.len(), 3);
        assert_eq!(def.exports[0].name, "SampleInit");
        assert_eq!(def.exports[0].ordinal, Some(1));
        assert_eq!(def.exports[2].name, "?CppFunc@@YAXXZ");
    }
}
