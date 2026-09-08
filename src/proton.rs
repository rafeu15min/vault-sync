//! Enumeração e resolução de versões de Proton instaladas — usado pelo
//! segundo Rofi de seleção de Proton (`tools/vault-sync-menu.sh`) e por
//! `MenuLaunch` (main.rs) pra invocar o script `proton` diretamente em vez
//! de deixar o `binfmt_misc` do sistema decidir sozinho qual Wine/prefixo
//! usar pra um `.exe`. Sem isso, nada garantia que a execução usava o
//! `compatdata/<id>/pfx` que o vault-sync de fato sincroniza com o FeuVault
//! — incidente real (2026-08-30): o bypass do `-applaunch` da Steam pra
//! atalhos non-Steam (ver `scan::find_shortcut_exe`) rodava o `.exe` cru, e
//! o Wine do sistema assumia um prefixo próprio, não o sincronizado. Ver
//! `session::run_session_with_options`, que passa a setar
//! `STEAM_COMPAT_DATA_PATH` a partir do mesmo `compatdata_dir` — é esse par
//! (script `proton` certo + `STEAM_COMPAT_DATA_PATH` certo) que faz trocar
//! de versão de Proton nunca arriscar o save: o prefixo-alvo nunca muda,
//! só o binário que o interpreta.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtonTool {
    /// Nome do diretório em `compatibilitytools.d` — identificador estável
    /// gravado em `Config::proton_overrides` (ex.: "GE-Proton10-34").
    pub name: String,
    pub dir: PathBuf,
}

impl ProtonTool {
    pub fn proton_script(&self) -> PathBuf {
        self.dir.join("proton")
    }
}

/// Diretórios convencionais de `compatibilitytools.d`: um dentro da própria
/// instalação Steam (compat tools baixados pela própria Steam ou colocados
/// manualmente pelo usuário) e um do sistema (pacotes como
/// `proton-cachyos-native`/`-slr` no Arch/CachyOS, que instalam em
/// `/usr/share/steam/compatibilitytools.d`). Nenhuma das duas é obrigatória
/// — uma máquina sem nenhum compat tool customizado só não tem nada pra
/// listar aqui.
fn compat_tool_dirs(steam_root: Option<&Path>) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(root) = steam_root {
        dirs.push(root.join("compatibilitytools.d"));
    }
    dirs.push(PathBuf::from("/usr/share/steam/compatibilitytools.d"));
    dirs
}

/// Lista todas as versões de Proton instaladas — qualquer subdiretório de
/// `compatibilitytools.d` que contenha um script `proton`. Ordenado por
/// nome pra saída determinística (rofi, testes). Um nome que aparece nas
/// duas raízes (usuário + sistema) só é listado uma vez, priorizando a
/// primeira ocorrência (raiz da Steam antes da raiz do sistema).
pub fn list_installed(steam_root: Option<&Path>) -> Vec<ProtonTool> {
    list_installed_in(&compat_tool_dirs(steam_root))
}

/// Núcleo testável de `list_installed`: recebe as raízes já resolvidas em
/// vez de montá-las internamente — sem isso, testes chamando `list_installed`
/// sempre acabariam varrendo `/usr/share/steam/compatibilitytools.d` de
/// verdade (raiz de sistema fixa), poluindo o resultado esperado com
/// qualquer compat tool que a máquina de teste tiver instalado de fato.
fn list_installed_in(dirs: &[PathBuf]) -> Vec<ProtonTool> {
    let mut tools = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() || !path.join("proton").is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if seen.insert(name.clone()) {
                tools.push(ProtonTool { name, dir: path });
            }
        }
    }
    tools.sort_by(|a, b| a.name.cmp(&b.name));
    tools
}

/// Resolve um nome salvo em `Config::proton_overrides` pra sua instalação
/// real — `None` se a versão configurada foi desinstalada desde a última
/// escolha. Nunca inventa um fallback silencioso: quem chama (`MenuLaunch`)
/// trata isso como erro acionável, pedindo pra escolher outra versão.
pub fn resolve(steam_root: Option<&Path>, name: &str) -> Option<ProtonTool> {
    list_installed(steam_root)
        .into_iter()
        .find(|t| t.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn test_dir(label: &str) -> PathBuf {
        crate::testutil::test_dir("proton", label)
    }

    fn fake_tool(root: &Path, name: &str) {
        let dir = root.join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("proton"), b"#!/bin/sh\n").unwrap();
    }

    #[test]
    fn list_installed_in_finds_tools_with_a_proton_script() {
        let root = test_dir("list-basic");
        let compat_dir = root.join("compatibilitytools.d");
        fs::create_dir_all(&compat_dir).unwrap();
        fake_tool(&compat_dir, "GE-Proton10-34");
        fake_tool(&compat_dir, "GE-Proton9-20");

        let tools = list_installed_in(&[compat_dir]);
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["GE-Proton10-34", "GE-Proton9-20"]);
    }

    #[test]
    fn list_installed_in_ignores_directories_without_a_proton_script() {
        let root = test_dir("list-ignores-noise");
        let compat_dir = root.join("compatibilitytools.d");
        fs::create_dir_all(compat_dir.join("nao-e-um-compat-tool")).unwrap();

        let tools = list_installed_in(&[compat_dir]);
        assert!(tools.is_empty());
    }

    #[test]
    fn list_installed_in_dedups_a_name_present_in_both_roots() {
        let root = test_dir("list-dedup");
        let user_dir = root.join("user").join("compatibilitytools.d");
        let system_dir = root.join("system").join("compatibilitytools.d");
        fs::create_dir_all(&user_dir).unwrap();
        fs::create_dir_all(&system_dir).unwrap();
        fake_tool(&user_dir, "proton-cachyos-slr");
        fake_tool(&system_dir, "proton-cachyos-slr");

        let tools = list_installed_in(&[user_dir, system_dir]);
        assert_eq!(tools.len(), 1);
    }

    #[test]
    fn list_installed_in_is_empty_when_no_directories_exist() {
        let root = test_dir("list-empty");
        assert!(list_installed_in(&[root.join("compatibilitytools.d-inexistente")]).is_empty());
    }

    #[test]
    fn compat_tool_dirs_includes_steam_root_and_the_fixed_system_path() {
        let steam_root = Path::new("/fake/steam/root");
        let dirs = compat_tool_dirs(Some(steam_root));
        assert_eq!(
            dirs,
            vec![
                steam_root.join("compatibilitytools.d"),
                PathBuf::from("/usr/share/steam/compatibilitytools.d"),
            ]
        );
    }

    #[test]
    fn resolve_finds_an_installed_tool_by_name() {
        let root = test_dir("resolve-hit");
        let steam_root = root.join("steam");
        let compat_dir = steam_root.join("compatibilitytools.d");
        fs::create_dir_all(&compat_dir).unwrap();
        fake_tool(&compat_dir, "GE-Proton10-34");

        let tool = resolve(Some(&steam_root), "GE-Proton10-34").unwrap();
        assert_eq!(
            tool.proton_script(),
            compat_dir.join("GE-Proton10-34").join("proton")
        );
    }

    #[test]
    fn resolve_is_none_for_an_uninstalled_name() {
        let root = test_dir("resolve-miss");
        let steam_root = root.join("steam");
        fs::create_dir_all(steam_root.join("compatibilitytools.d")).unwrap();

        assert!(resolve(
            Some(&steam_root),
            "versao-completamente-inventada-pro-teste"
        )
        .is_none());
    }
}
