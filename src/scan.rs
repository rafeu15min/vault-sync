//! Varredura forense do sistema de arquivos (4.1) — port nativo de
//! `tools/steam_scan.py`. Mapeia bibliotecas Steam, classifica cada prefixo
//! em `compatdata/` (oficial/non-Steam/órfão) e produz `GameSource` reais em
//! vez de depender de heurísticas externas ao Rust.
//!
//! A única omissão deliberada em relação ao script original é a consulta
//! online à Steam Store API dentro de `investigate_orphan`: introduziria
//! I/O de rede não-determinístico (e uma dependência HTTP) num comando cujo
//! propósito é gerar um catálogo local rápido e reprodutível. As pistas
//! offline (registro do Wine, atalhos, pastas de save) continuam intactas.

use crate::domain::GameSource;
use crate::vdf::{self, VdfValue};
use regex::Regex;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

fn path_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#""path"\s+"([^"]+)""#).unwrap())
}

fn name_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"(?i)"name"\s+"([^"]+)""#).unwrap())
}

fn registry_key_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"(?i)^\[Software\\\\([^\]]+)\]"#).unwrap())
}

/// Localiza o diretório raiz de instalação da Steam no Linux, testando os
/// três caminhos convencionais nessa ordem de prioridade.
pub fn find_steam_root() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    [
        home.join(".local/share/Steam"),
        home.join(".steam/steam"),
        home.join(".steam/root"),
    ]
    .into_iter()
    .find(|p| p.is_dir())
}

/// Lê `libraryfolders.vdf` para descobrir as bibliotecas Steam conhecidas,
/// além da raiz (que é sempre uma biblioteca implícita).
pub fn parse_library_folders(steam_root: &Path) -> Vec<PathBuf> {
    let mut libraries = vec![steam_root.to_path_buf()];
    let vdf_path = steam_root.join("steamapps").join("libraryfolders.vdf");
    let Ok(content) = fs::read_to_string(&vdf_path) else {
        return libraries;
    };
    for cap in path_regex().captures_iter(&content) {
        let clean = cap[1].replace("\\\\", "/");
        let path = PathBuf::from(&clean);
        if path.is_dir() && !libraries.contains(&path) {
            libraries.push(path);
        }
    }
    libraries
}

/// Lê o nome de um jogo oficial a partir do `appmanifest_<id>.acf`
/// correspondente, se existir.
pub fn official_game_name(library: &Path, app_id: &str) -> Option<String> {
    let manifest = library
        .join("steamapps")
        .join(format!("appmanifest_{app_id}.acf"));
    let content = fs::read_to_string(manifest).ok()?;
    name_regex().captures(&content).map(|c| c[1].to_string())
}

/// Decodifica todos os `shortcuts.vdf` (um por usuário local da Steam) e
/// devolve o mapa app_id -> nome, usado para identificar jogos non-Steam
/// (GOG, homebrew, launchers de terceiros como o Hydra).
///
/// Também completa (nunca sobrescreve) com o que sobrar de
/// `shortcuts.vdf.vault-sync-bak` — a cópia de segurança que o próprio
/// `inject.rs` grava (`backup::atomic_backup`) antes de qualquer
/// modificação. Caso real que motivou isso: uma troca de conta Steam nesta
/// máquina abandonou o `userdata/<id_antigo>/` inteiro — o `shortcuts.vdf`
/// AO VIVO daquele usuário sumiu (conta nunca mais logada), levando junto
/// o nome de ~19 jogos non-Steam cujo `compatdata/` continuava órfão no
/// disco. Só afeta o texto exibido: a resolução real de caminho
/// (`find_shortcut_start_dir`) continua lendo exclusivamente o
/// `shortcuts.vdf` ao vivo, nunca o backup.
pub fn parse_shortcuts_map(steam_root: &Path) -> HashMap<u32, String> {
    let mut map = HashMap::new();
    let userdata = steam_root.join("userdata");
    let Ok(entries) = fs::read_dir(userdata) else {
        return map;
    };
    for user_dir in entries.flatten() {
        let config_dir = user_dir.path().join("config");
        if let Ok(root) = read_shortcuts_vdf(&config_dir.join("shortcuts.vdf")) {
            for (app_id, name) in shortcuts_from_tree(&root) {
                map.insert(app_id, name);
            }
        }
        if let Ok(root) = read_shortcuts_vdf(&config_dir.join("shortcuts.vdf.vault-sync-bak")) {
            for (app_id, name) in shortcuts_from_tree(&root) {
                map.entry(app_id).or_insert(name);
            }
        }
    }
    map
}

fn read_shortcuts_vdf(path: &Path) -> Result<vdf::VdfObject, ()> {
    let raw = fs::read(path).map_err(|_| ())?;
    vdf::parse_binary(&raw).map_err(|_| ())
}

/// Percorre as entradas de "shortcuts" de uma árvore de `shortcuts.vdf` já
/// decodificada, entregando os campos de cada atalho pra `f` extrair o que
/// precisar — base comum reaproveitada por todos os extratores
/// especializados abaixo, que só variam no campo extraído.
fn for_each_shortcut_entry(root: &vdf::VdfObject, mut f: impl FnMut(&vdf::VdfObject)) {
    let Some((_, VdfValue::Object(shortcuts))) = root
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("shortcuts"))
    else {
        return;
    };
    for (_, entry) in shortcuts {
        let VdfValue::Object(fields) = entry else {
            continue;
        };
        f(fields);
    }
}

/// Extrai os pares (appid, appname) da árvore de um `shortcuts.vdf` já
/// decodificado — usado tanto pelo catálogo quanto pelos testes do motor de
/// injeção.
pub(crate) fn shortcuts_from_tree(root: &vdf::VdfObject) -> Vec<(u32, String)> {
    let mut out = Vec::new();
    for_each_shortcut_entry(root, |fields| {
        let app_id = fields
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("appid"))
            .and_then(|(_, v)| match v {
                VdfValue::Int(i) => Some(*i as u32),
                _ => None,
            });
        let name = fields
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("appname"))
            .and_then(|(_, v)| match v {
                VdfValue::Str(s) => Some(s.clone()),
                _ => None,
            });
        if let (Some(app_id), Some(name)) = (app_id, name) {
            out.push((app_id, name));
        }
    });
    out
}

/// Procura, em todos os `shortcuts.vdf` locais (um por usuário Steam), o
/// `StartDir` do atalho non-Steam com o `app_id` informado — usado pela
/// Fase VII pra localizar a instalação completa de jogos piratas.
/// Deliberadamente devolve só a string gravada, sem verificar se o caminho
/// ainda existe: essa verificação é responsabilidade de quem chama
/// (`install_paths.rs`), já que aqui é só leitura/parsing puro.
pub fn find_shortcut_start_dir(steam_root: &Path, app_id: u32) -> Option<String> {
    find_shortcut_field(steam_root, app_id, "startdir")
}

/// Mesma coisa que `find_shortcut_start_dir`, mas pro campo `Exe` — usado
/// por `MenuLaunch` (main.rs) pra lançar um atalho non-Steam sem depender
/// do `-applaunch` da Steam (ver `session::run_session` direto). Incidente
/// real que motivou isso: `-applaunch`/`rungameid` devolvem "Game
/// configuration unavailable" pra atalhos non-Steam nesta versão do
/// cliente, mesmo com `shortcuts.vdf`/`CompatToolMapping` corretos — só o
/// `vault-sync run <id> --steam -- <exe>` chamado direto de fato roda o
/// jogo, então `MenuLaunch` passou a fazer exatamente isso, sem
/// intermediar pela Steam.
pub fn find_shortcut_exe(steam_root: &Path, app_id: u32) -> Option<String> {
    find_shortcut_field(steam_root, app_id, "exe")
}

/// Aplica `f` a cada `<steam_root>/userdata/*/config/<filename>` que existe
/// de verdade (um por usuário local da Steam), coletando o par (caminho,
/// resultado) — driver comum reaproveitado por qualquer operação que
/// precise varrer os perfis locais um por um (injeção, remoção, busca).
pub(crate) fn for_each_userdata_file<R>(
    steam_root: &Path,
    filename: &str,
    mut f: impl FnMut(&Path) -> R,
) -> Vec<(PathBuf, R)> {
    let userdata = steam_root.join("userdata");
    let Ok(entries) = fs::read_dir(userdata) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|e| e.path().join("config").join(filename))
        .filter(|p| p.is_file())
        .map(|p| {
            let result = f(&p);
            (p, result)
        })
        .collect()
}

/// Procura, em todos os `shortcuts.vdf` locais, o campo `field_name`
/// (comparação sem diferenciar maiúsculas) do atalho non-Steam que casa
/// com `app_id`.
fn find_shortcut_field(steam_root: &Path, app_id: u32, field_name: &str) -> Option<String> {
    for_each_userdata_file(steam_root, "shortcuts.vdf", |path| {
        let raw = fs::read(path).ok()?;
        let root = vdf::parse_binary(&raw).ok()?;
        shortcut_field_from_tree(&root, app_id, field_name)
    })
    .into_iter()
    .find_map(|(_, result)| result)
}

/// Mesma árvore que `shortcuts_from_tree` percorre, mas extraindo o campo
/// `field_name` do atalho que casa com `app_id`, em vez do mapa completo
/// appid→nome.
fn shortcut_field_from_tree(
    root: &vdf::VdfObject,
    app_id: u32,
    field_name: &str,
) -> Option<String> {
    let mut result = None;
    for_each_shortcut_entry(root, |fields| {
        if result.is_some() {
            return;
        }
        let matches_app_id = fields
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("appid"))
            .is_some_and(|(_, v)| matches!(v, VdfValue::Int(i) if *i as u32 == app_id));
        if !matches_app_id {
            return;
        }
        result = fields
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(field_name))
            .and_then(|(_, v)| match v {
                VdfValue::Str(s) => Some(s.trim_matches('"').to_string()),
                _ => None,
            });
    });
    result
}

/// Procura, em todos os `shortcuts.vdf` locais, uma entrada cujo `appname`
/// bata exatamente com `name` — devolve o caminho do `shortcuts.vdf` onde
/// achou e o `Exe` (sem aspas). Usado por `inject::stage_pirata_shortcut`
/// pra saber qual executável apontar dentro da pasta recém-consolidada em
/// `local_game_root_piratas`, sem tentar adivinhar entre vários `.exe`
/// possíveis: reaproveita o nome de arquivo de um atalho já existente,
/// mesmo que a pasta em si tenha ficado obsoleta por remanejo de disco (ver
/// `find_stale_shortcuts`, que detecta exatamente essas entradas obsoletas
/// depois que uma nova é criada).
pub fn find_shortcut_exe_by_name(steam_root: &Path, name: &str) -> Option<(PathBuf, String)> {
    for_each_userdata_file(steam_root, "shortcuts.vdf", |path| {
        let raw = fs::read(path).ok()?;
        let root = vdf::parse_binary(&raw).ok()?;
        exe_for_appname_from_tree(&root, name)
    })
    .into_iter()
    .find_map(|(path, result)| result.map(|exe| (path, exe)))
}

fn exe_for_appname_from_tree(root: &vdf::VdfObject, name: &str) -> Option<String> {
    let mut result = None;
    for_each_shortcut_entry(root, |fields| {
        if result.is_some() {
            return;
        }
        let matches_name = fields
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("appname"))
            .is_some_and(|(_, v)| matches!(v, VdfValue::Str(s) if s == name));
        if !matches_name {
            return;
        }
        result = fields
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("exe"))
            .and_then(|(_, v)| match v {
                VdfValue::Str(s) => Some(s.trim_matches('"').to_string()),
                _ => None,
            });
    });
    result
}

/// Uma entrada de `shortcuts.vdf` cujo `Exe` não existe mais em disco, mas
/// que compartilha `appname` com outra entrada (AppID diferente) cujo `Exe`
/// existe de verdade — sinal forte de resíduo de remanejo de disco: o
/// AppID da Steam é derivado do caminho do `Exe` (ver `steam_appid.rs`),
/// então mover um jogo de disco sempre gera um AppID novo, nunca atualiza
/// o antigo — a entrada velha fica pra sempre a menos que algo a remova.
/// Nunca reportado quando NENHUMA entrada daquele nome tem `Exe` vivo — aí
/// é só "jogo desinstalado em todo lugar", estado normal (mesmo princípio
/// já usado em `doctor.rs` pra ponto de montagem: ausência isolada não é
/// falha por si só). Só detecta e relata — nunca remove nada; ver
/// `Command::Doctor` (main.rs).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct StaleShortcut {
    pub name: String,
    pub dead_app_id: u32,
    pub dead_exe: String,
    pub live_app_id: u32,
}

pub fn find_stale_shortcuts(steam_root: &Path) -> Vec<StaleShortcut> {
    let mut by_name: HashMap<String, Vec<(u32, String)>> = HashMap::new();
    for (_, entries_in_file) in for_each_userdata_file(steam_root, "shortcuts.vdf", |path| {
        fs::read(path)
            .ok()
            .and_then(|raw| vdf::parse_binary(&raw).ok())
            .map(|root| shortcuts_with_exe_from_tree(&root))
            .unwrap_or_default()
    }) {
        for (app_id, name, exe) in entries_in_file {
            let bucket = by_name.entry(name).or_default();
            if !bucket.iter().any(|(id, _)| *id == app_id) {
                bucket.push((app_id, exe));
            }
        }
    }

    let mut stale = Vec::new();
    for (name, entries) in by_name {
        if entries.len() < 2 {
            continue;
        }
        let Some(&(live_app_id, _)) = entries.iter().find(|(_, exe)| Path::new(exe).is_file())
        else {
            continue;
        };
        for (app_id, exe) in &entries {
            if !Path::new(exe).is_file() {
                stale.push(StaleShortcut {
                    name: name.clone(),
                    dead_app_id: *app_id,
                    dead_exe: exe.clone(),
                    live_app_id,
                });
            }
        }
    }
    stale.sort_by(|a, b| (&a.name, a.dead_app_id).cmp(&(&b.name, b.dead_app_id)));
    stale
}

fn shortcuts_with_exe_from_tree(root: &vdf::VdfObject) -> Vec<(u32, String, String)> {
    let mut out = Vec::new();
    for_each_shortcut_entry(root, |fields| {
        let app_id = fields
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("appid"))
            .and_then(|(_, v)| match v {
                VdfValue::Int(i) => Some(*i as u32),
                _ => None,
            });
        let name = fields
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("appname"))
            .and_then(|(_, v)| match v {
                VdfValue::Str(s) => Some(s.clone()),
                _ => None,
            });
        let exe = fields
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("exe"))
            .and_then(|(_, v)| match v {
                VdfValue::Str(s) => Some(s.trim_matches('"').to_string()),
                _ => None,
            });
        if let (Some(app_id), Some(name), Some(exe)) = (app_id, name, exe) {
            out.push((app_id, name, exe));
        }
    });
    out
}

/// Resolve o diretório local de instalação completa de um jogo pirata —
/// usado tanto por `session::detect_full_install` (baixar) quanto por
/// `purge` (apagar), pra nunca deixar as duas resoluções divergirem.
/// Prioridade: override manual (`install_path_overrides.piratas`) > raiz
/// adotada (`local_game_root_piratas`, nomeada pelo `appname` gravado no
/// atalho — estável mesmo que o jogo troque de disco) > `StartDir` ao vivo
/// do `shortcuts.vdf` (legado, só quando nenhuma raiz adotada está
/// configurada). É o `StartDir` — livre, editável por fora — que fica
/// obsoleto quando o disco muda; incidente real: Phonopolis e Cities
/// Skylines II tiveram o AppID (CRC do `Exe`+nome, calculado pela própria
/// Steam) dessincronizado depois de uma troca de SSD, porque nada fixava o
/// atalho num destino estável. Uma vez adotado, o nome (não o disco de
/// origem) é que decide o caminho — troca de disco deixa de importar.
pub fn resolve_piratas_install_dir(
    override_path: Option<&Path>,
    adopted_root: Option<&Path>,
    steam_root: Option<&Path>,
    app_id: u32,
) -> Option<PathBuf> {
    if let Some(overridden) = override_path {
        return Some(overridden.to_path_buf());
    }
    if let Some(root) = adopted_root {
        let name = steam_root.and_then(|sroot| parse_shortcuts_map(sroot).remove(&app_id));
        if let Some(name) = name.filter(|n| !n.is_empty()) {
            return Some(root.join(name));
        }
    }
    let start_dir = steam_root.and_then(|sroot| find_shortcut_start_dir(sroot, app_id))?;
    Some(PathBuf::from(start_dir))
}

const IGNORE_DIRS: &[&str] = &[
    "microsoft",
    "temp",
    "openvr",
    "steam",
    "cef",
    "dxvk-cache",
    "gstreamer",
    "windows",
    "epicgames",
    "music",
    "downloads",
    "pictures",
    "videos",
    "desktop",
    "public",
    "templates",
    "appdata",
    "application data",
    "local settings",
    "my pictures",
    "my videos",
    "my music",
    "start menu",
    "nethood",
    "printhood",
    "recent",
    "sendto",
    "cookies",
    "history",
    "favorites",
    "logs",
    "nvidia",
    "amd",
    "epic games",
    "epic games launcher",
    "ubisoft",
    "ea desktop",
    "origin",
    "gog.com",
    "program data",
];

const IGNORE_REGISTRY_VENDORS: &[&str] = &[
    "wine",
    "microsoft",
    "valve",
    "classes",
    "clients",
    "policies",
    "google",
    "vulkan",
    "gstreamer",
    "bugsplat",
    "macromedia",
    "khronos",
    "epic games",
    "epic games inc",
    "gog.com",
    "ubisoft",
    "ea",
    "origin",
    "electronic arts",
    "rockstar games",
    "cd projekt red",
];

fn push_clue(clues: &mut Vec<String>, clue: String) {
    if !clues.contains(&clue) {
        clues.push(clue);
    }
}

/// Investiga um prefixo `compatdata/<app_id>` sem manifesto nem shortcut
/// correspondente, coletando pistas offline (pastas de save, atalhos e
/// registro do Wine) — réplica fiel da parte local de `investigate_orphan`
/// em `steam_scan.py` (sem a consulta à Steam Store API, ver módulo).
pub fn investigate_orphan(library: &Path, app_id: &str) -> Vec<String> {
    let pfx = library
        .join("steamapps")
        .join("compatdata")
        .join(app_id)
        .join("pfx");
    let users = pfx.join("drive_c").join("users").join("steamuser");
    let mut clues: Vec<String> = Vec::new();

    let search_dirs = [
        users.join("Documents"),
        users.join("Documents").join("My Games"),
        users.join("AppData").join("Local"),
        users.join("AppData").join("LocalLow"),
        users.join("AppData").join("Roaming"),
        users.join("Saved Games"),
        pfx.join("drive_c").join("ProgramData"),
    ];
    for dir in &search_dirs {
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if !entry.path().is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if !IGNORE_DIRS.contains(&name.to_lowercase().as_str()) && !name.starts_with('.') {
                push_clue(&mut clues, name);
            }
        }
    }

    let desktop_dirs = [
        users.join("Desktop"),
        pfx.join("drive_c")
            .join("users")
            .join("Public")
            .join("Desktop"),
    ];
    for dir in &desktop_dirs {
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let stem = name
                .strip_suffix(".lnk")
                .or_else(|| name.strip_suffix(".url"));
            if let Some(stem) = stem {
                if !IGNORE_DIRS.contains(&stem.to_lowercase().as_str()) {
                    push_clue(&mut clues, format!("Atalho: {stem}"));
                }
            }
        }
    }

    for reg_file in ["user.reg", "system.reg"] {
        let Ok(content) = fs::read_to_string(pfx.join(reg_file)) else {
            continue;
        };
        for line in content.lines() {
            let Some(cap) = registry_key_regex().captures(line) else {
                continue;
            };
            let mut parts = cap[1].split("\\\\");
            let Some(mut vendor) = parts.next() else {
                continue;
            };
            if vendor.eq_ignore_ascii_case("wow6432node") {
                match parts.next() {
                    Some(next) => vendor = next,
                    None => continue,
                }
            }
            let vendor = vendor.trim();
            if !vendor.is_empty()
                && !IGNORE_REGISTRY_VENDORS.contains(&vendor.to_lowercase().as_str())
            {
                push_clue(&mut clues, vendor.to_string());
                if let Some(game_name) = parts.next() {
                    let game_name = game_name.trim();
                    if !game_name.is_empty()
                        && !IGNORE_REGISTRY_VENDORS.contains(&game_name.to_lowercase().as_str())
                    {
                        push_clue(&mut clues, game_name.to_string());
                    }
                }
            }
        }
    }

    clues.retain(|c| c.trim().len() > 2);
    clues.truncate(5);
    clues
}

/// Uma entrada do catálogo: em qual biblioteca o prefixo foi encontrado e
/// como foi classificado.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ScannedEntry {
    pub library: PathBuf,
    #[serde(flatten)]
    pub source: GameSource,
}

/// Relatório completo de uma varredura — serializado como o
/// `steam_catalog.json` do comando `scan`.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ScanReport {
    pub entries: Vec<ScannedEntry>,
    pub official_count: usize,
    pub non_steam_count: usize,
    pub orphan_count: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum ScanError {
    #[error("diretório raiz da Steam não encontrado (~/.local/share/Steam, ~/.steam/steam ou ~/.steam/root)")]
    SteamRootNotFound,
}

/// Executa a varredura completa: descobre bibliotecas, decodifica os
/// `shortcuts.vdf` de todos os usuários locais e classifica cada prefixo em
/// `compatdata/` de cada biblioteca (raiz da Steam + `extra_libraries`,
/// tipicamente `config.local_libraries`).
pub fn run_scan(extra_libraries: &[PathBuf]) -> Result<ScanReport, ScanError> {
    let steam_root = find_steam_root().ok_or(ScanError::SteamRootNotFound)?;
    scan_from_root(&steam_root, extra_libraries)
}

fn scan_from_root(steam_root: &Path, extra_libraries: &[PathBuf]) -> Result<ScanReport, ScanError> {
    let mut libraries = parse_library_folders(steam_root);
    for extra in extra_libraries {
        if extra.is_dir() && !libraries.contains(extra) {
            libraries.push(extra.clone());
        }
    }

    let shortcuts_map = parse_shortcuts_map(steam_root);
    let mut report = ScanReport::default();

    for library in &libraries {
        let compatdata = library.join("steamapps").join("compatdata");
        let Ok(entries) = fs::read_dir(&compatdata) else {
            continue;
        };
        for entry in entries.flatten() {
            let app_id_str = entry.file_name().to_string_lossy().into_owned();
            if app_id_str.is_empty() || !app_id_str.chars().all(|c| c.is_ascii_digit()) {
                continue;
            }
            let numeric_app_id = app_id_str.parse::<u32>();

            let source = match numeric_app_id {
                Ok(id) if official_game_name(library, &app_id_str).is_some() => {
                    report.official_count += 1;
                    GameSource::Official { app_id: id }
                }
                Ok(id) if shortcuts_map.contains_key(&id) => {
                    report.non_steam_count += 1;
                    GameSource::NonSteamShortcut {
                        app_id: id,
                        shortcut_name: shortcuts_map[&id].clone(),
                    }
                }
                _ => {
                    report.orphan_count += 1;
                    GameSource::Orphan {
                        app_id: app_id_str.clone(),
                        clues: investigate_orphan(library, &app_id_str),
                    }
                }
            };

            report.entries.push(ScannedEntry {
                library: library.clone(),
                source,
            });
        }
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, File};
    use std::io::Write;

    fn test_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "vault-sync-scan-test-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn parse_library_folders_includes_root_and_extra_paths_from_vdf() {
        let root = test_dir("libs");
        let extra = test_dir("libs-extra");
        fs::create_dir_all(root.join("steamapps")).unwrap();
        let vdf_content = format!(
            "\"libraryfolders\"\n{{\n\t\"1\"\n\t{{\n\t\t\"path\"\t\t\"{}\"\n\t}}\n}}\n",
            extra.display().to_string().replace('/', "\\\\")
        );
        fs::write(
            root.join("steamapps").join("libraryfolders.vdf"),
            vdf_content,
        )
        .unwrap();

        let libs = parse_library_folders(&root);
        assert!(libs.contains(&root));
        assert!(libs.contains(&extra));
    }

    #[test]
    fn official_game_name_reads_appmanifest() {
        let lib = test_dir("official");
        fs::create_dir_all(lib.join("steamapps")).unwrap();
        fs::write(
            lib.join("steamapps").join("appmanifest_730.acf"),
            "\"AppState\"\n{\n\t\"appid\"\t\t\"730\"\n\t\"name\"\t\t\"Counter-Strike 2\"\n}\n",
        )
        .unwrap();

        assert_eq!(
            official_game_name(&lib, "730"),
            Some("Counter-Strike 2".to_string())
        );
        assert_eq!(official_game_name(&lib, "999"), None);
    }

    #[test]
    fn investigate_orphan_collects_registry_and_save_clues() {
        let lib = test_dir("orphan");
        let pfx = lib
            .join("steamapps")
            .join("compatdata")
            .join("999")
            .join("pfx");
        let saves = pfx
            .join("drive_c")
            .join("users")
            .join("steamuser")
            .join("Documents")
            .join("My Games")
            .join("MeuJogoPirata");
        fs::create_dir_all(&saves).unwrap();

        let mut reg = File::create(pfx.join("user.reg")).unwrap();
        writeln!(reg, "[Software\\\\MinhaEditora\\\\MeuJogoPirata]").unwrap();
        writeln!(reg, "\"Path\"=\"C:\\\\Games\"").unwrap();

        let clues = investigate_orphan(&lib, "999");
        assert!(clues.iter().any(|c| c == "MeuJogoPirata"));
        assert!(clues.iter().any(|c| c == "MinhaEditora"));
    }

    #[test]
    fn investigate_orphan_ignores_known_windows_noise() {
        let lib = test_dir("orphan-empty");
        let appdata = lib
            .join("steamapps")
            .join("compatdata")
            .join("1")
            .join("pfx")
            .join("drive_c")
            .join("users")
            .join("steamuser")
            .join("AppData")
            .join("Local");
        fs::create_dir_all(appdata.join("Microsoft")).unwrap();
        fs::create_dir_all(appdata.join("Temp")).unwrap();

        let clues = investigate_orphan(&lib, "1");
        assert!(clues.is_empty());
    }

    #[test]
    fn scan_from_root_classifies_official_non_steam_and_orphan_prefixes() {
        let root = test_dir("full-scan");
        fs::create_dir_all(root.join("steamapps").join("compatdata").join("730")).unwrap();
        fs::create_dir_all(root.join("steamapps").join("compatdata").join("1234567890")).unwrap();
        fs::create_dir_all(root.join("steamapps").join("compatdata").join("42")).unwrap();
        fs::write(
            root.join("steamapps").join("appmanifest_730.acf"),
            "\"AppState\"\n{\n\t\"name\"\t\t\"Half-Life\"\n}\n",
        )
        .unwrap();

        let userdata = root.join("userdata").join("1").join("config");
        fs::create_dir_all(&userdata).unwrap();
        let shortcuts_tree = vec![(
            "shortcuts".to_string(),
            VdfValue::Object(vec![(
                "0".to_string(),
                VdfValue::Object(vec![
                    ("appid".to_string(), VdfValue::Int(1234567890u32 as i32)),
                    (
                        "appname".to_string(),
                        VdfValue::Str("Jogo Non-Steam".to_string()),
                    ),
                ]),
            )]),
        )];
        fs::write(
            userdata.join("shortcuts.vdf"),
            vdf::write_binary(&shortcuts_tree),
        )
        .unwrap();

        let report = scan_from_root(&root, &[]).unwrap();

        assert_eq!(report.official_count, 1);
        assert_eq!(report.non_steam_count, 1);
        assert_eq!(report.orphan_count, 1);

        let has_official = report
            .entries
            .iter()
            .any(|e| matches!(&e.source, GameSource::Official { app_id: 730 }));
        let has_non_steam = report.entries.iter().any(|e| {
            matches!(&e.source, GameSource::NonSteamShortcut { app_id: 1234567890, shortcut_name } if shortcut_name == "Jogo Non-Steam")
        });
        let has_orphan = report
            .entries
            .iter()
            .any(|e| matches!(&e.source, GameSource::Orphan { app_id, .. } if app_id == "42"));

        assert!(has_official, "esperava um GameSource::Official para 730");
        assert!(
            has_non_steam,
            "esperava um GameSource::NonSteamShortcut para 1234567890"
        );
        assert!(has_orphan, "esperava um GameSource::Orphan para 42");
    }

    fn shortcuts_tree_with_name(app_id: u32, name: &str) -> vdf::VdfObject {
        vec![(
            "shortcuts".to_string(),
            VdfValue::Object(vec![(
                "0".to_string(),
                VdfValue::Object(vec![
                    ("appid".to_string(), VdfValue::Int(app_id as i32)),
                    ("appname".to_string(), VdfValue::Str(name.to_string())),
                ]),
            )]),
        )]
    }

    #[test]
    fn parse_shortcuts_map_falls_back_to_vault_sync_bak_for_names_missing_live() {
        // Reproduz o caso real: troca de conta Steam nesta máquina abandonou
        // o `userdata/<id_antigo>/`, apagando o `shortcuts.vdf` ao vivo
        // daquele usuário — só o backup de segurança do `inject.rs`
        // (`shortcuts.vdf.vault-sync-bak`) sobrou com os nomes antigos.
        let root = test_dir("shortcuts-map-bak-fallback");
        let config_dir = root.join("userdata").join("1").join("config");
        fs::create_dir_all(&config_dir).unwrap();
        fs::write(
            config_dir.join("shortcuts.vdf.vault-sync-bak"),
            vdf::write_binary(&shortcuts_tree_with_name(3410893582, "Disco Elysium")),
        )
        .unwrap();

        let map = parse_shortcuts_map(&root);

        assert_eq!(map.get(&3410893582), Some(&"Disco Elysium".to_string()));
    }

    #[test]
    fn parse_shortcuts_map_never_lets_backup_override_a_live_name() {
        let root = test_dir("shortcuts-map-live-wins");
        let config_dir = root.join("userdata").join("1").join("config");
        fs::create_dir_all(&config_dir).unwrap();
        fs::write(
            config_dir.join("shortcuts.vdf"),
            vdf::write_binary(&shortcuts_tree_with_name(100, "Nome Atual")),
        )
        .unwrap();
        fs::write(
            config_dir.join("shortcuts.vdf.vault-sync-bak"),
            vdf::write_binary(&shortcuts_tree_with_name(100, "Nome Antigo")),
        )
        .unwrap();

        let map = parse_shortcuts_map(&root);

        assert_eq!(map.get(&100), Some(&"Nome Atual".to_string()));
    }

    fn shortcuts_tree_with_start_dir(app_id: u32, start_dir: &str) -> vdf::VdfObject {
        vec![(
            "shortcuts".to_string(),
            VdfValue::Object(vec![(
                "0".to_string(),
                VdfValue::Object(vec![
                    ("appid".to_string(), VdfValue::Int(app_id as i32)),
                    ("appname".to_string(), VdfValue::Str("Jogo".to_string())),
                    (
                        "Exe".to_string(),
                        VdfValue::Str(format!("\"{start_dir}/Jogo.exe\"")),
                    ),
                    (
                        "StartDir".to_string(),
                        // Reproduz o formato real observado: Steam grava o
                        // valor com aspas literais embutidas na própria
                        // string, ao contrário de appname/appid.
                        VdfValue::Str(format!("\"{start_dir}\"")),
                    ),
                ]),
            )]),
        )]
    }

    #[test]
    fn find_shortcut_start_dir_extracts_and_unquotes_the_matching_entry() {
        let root = test_dir("shortcut-start-dir");
        let userdata = root.join("userdata").join("1").join("config");
        fs::create_dir_all(&userdata).unwrap();
        fs::write(
            userdata.join("shortcuts.vdf"),
            vdf::write_binary(&shortcuts_tree_with_start_dir(42, "/mnt/HDD1/Piratas/Jogo")),
        )
        .unwrap();

        let start_dir = find_shortcut_start_dir(&root, 42).unwrap();
        assert_eq!(start_dir, "/mnt/HDD1/Piratas/Jogo");
    }

    #[test]
    fn find_shortcut_exe_extracts_and_unquotes_the_matching_entry() {
        let root = test_dir("shortcut-exe");
        let userdata = root.join("userdata").join("1").join("config");
        fs::create_dir_all(&userdata).unwrap();
        fs::write(
            userdata.join("shortcuts.vdf"),
            vdf::write_binary(&shortcuts_tree_with_start_dir(42, "/mnt/HDD1/Piratas/Jogo")),
        )
        .unwrap();

        let exe = find_shortcut_exe(&root, 42).unwrap();
        assert_eq!(exe, "/mnt/HDD1/Piratas/Jogo/Jogo.exe");
    }

    #[test]
    fn find_shortcut_exe_is_none_for_unknown_app_id() {
        let root = test_dir("shortcut-exe-miss");
        let userdata = root.join("userdata").join("1").join("config");
        fs::create_dir_all(&userdata).unwrap();
        fs::write(
            userdata.join("shortcuts.vdf"),
            vdf::write_binary(&shortcuts_tree_with_start_dir(42, "/mnt/HDD1/Piratas/Jogo")),
        )
        .unwrap();

        assert!(find_shortcut_exe(&root, 999).is_none());
    }

    #[test]
    fn find_shortcut_start_dir_is_none_for_unknown_app_id() {
        let root = test_dir("shortcut-start-dir-miss");
        let userdata = root.join("userdata").join("1").join("config");
        fs::create_dir_all(&userdata).unwrap();
        fs::write(
            userdata.join("shortcuts.vdf"),
            vdf::write_binary(&shortcuts_tree_with_start_dir(42, "/mnt/HDD1/Piratas/Jogo")),
        )
        .unwrap();

        assert!(find_shortcut_start_dir(&root, 999).is_none());
    }

    #[test]
    fn resolve_piratas_install_dir_prefers_override_over_everything() {
        let root = test_dir("resolve-piratas-override");
        let userdata = root.join("userdata").join("1").join("config");
        fs::create_dir_all(&userdata).unwrap();
        fs::write(
            userdata.join("shortcuts.vdf"),
            vdf::write_binary(&shortcuts_tree_with_start_dir(42, "/mnt/HDD1/Piratas/Jogo")),
        )
        .unwrap();

        let resolved = resolve_piratas_install_dir(
            Some(Path::new("/override/caminho")),
            Some(Path::new("/home/rafeu/Games/piratas")),
            Some(&root),
            42,
        );
        assert_eq!(resolved, Some(PathBuf::from("/override/caminho")));
    }

    #[test]
    fn resolve_piratas_install_dir_uses_adopted_root_named_by_shortcut_appname() {
        let root = test_dir("resolve-piratas-adopted");
        let userdata = root.join("userdata").join("1").join("config");
        fs::create_dir_all(&userdata).unwrap();
        fs::write(
            userdata.join("shortcuts.vdf"),
            vdf::write_binary(&shortcuts_tree_with_start_dir(42, "/mnt/SSD1/Piratas/Jogo")),
        )
        .unwrap();

        // Sem override, mas com raiz adotada configurada — deve ignorar o
        // `StartDir` (que pode estar num disco que já nem existe mais) e
        // usar o `appname` gravado no atalho ("Jogo") como nome estável.
        let resolved = resolve_piratas_install_dir(
            None,
            Some(Path::new("/home/rafeu/Games/piratas")),
            Some(&root),
            42,
        );
        assert_eq!(
            resolved,
            Some(PathBuf::from("/home/rafeu/Games/piratas/Jogo"))
        );
    }

    #[test]
    fn resolve_piratas_install_dir_falls_back_to_start_dir_without_adopted_root() {
        let root = test_dir("resolve-piratas-legacy");
        let userdata = root.join("userdata").join("1").join("config");
        fs::create_dir_all(&userdata).unwrap();
        fs::write(
            userdata.join("shortcuts.vdf"),
            vdf::write_binary(&shortcuts_tree_with_start_dir(42, "/mnt/HDD1/Piratas/Jogo")),
        )
        .unwrap();

        let resolved = resolve_piratas_install_dir(None, None, Some(&root), 42);
        assert_eq!(resolved, Some(PathBuf::from("/mnt/HDD1/Piratas/Jogo")));
    }

    #[test]
    fn resolve_piratas_install_dir_is_none_when_nothing_resolves() {
        let root = test_dir("resolve-piratas-none");
        fs::create_dir_all(root.join("userdata")).unwrap();
        assert!(resolve_piratas_install_dir(None, None, Some(&root), 999).is_none());
    }

    #[test]
    fn scan_report_serializes_to_json() {
        let mut report = ScanReport::default();
        report.entries.push(ScannedEntry {
            library: PathBuf::from("/mnt/SSD2/SteamLibrary"),
            source: GameSource::Official { app_id: 730 },
        });
        let json = serde_json::to_string(&report).unwrap();
        assert!(json.contains("\"kind\":\"official\""));
        assert!(json.contains("\"app_id\":730"));
    }

    fn shortcuts_tree_with_exe(app_id: u32, name: &str, exe: &str) -> vdf::VdfObject {
        vec![(
            "shortcuts".to_string(),
            VdfValue::Object(vec![(
                "0".to_string(),
                VdfValue::Object(vec![
                    ("appid".to_string(), VdfValue::Int(app_id as i32)),
                    ("appname".to_string(), VdfValue::Str(name.to_string())),
                    ("Exe".to_string(), VdfValue::Str(format!("\"{exe}\""))),
                ]),
            )]),
        )]
    }

    #[test]
    fn find_shortcut_exe_by_name_finds_across_any_userdata_profile() {
        let root = test_dir("exe-by-name-hit");
        let userdata = root.join("userdata").join("340852084").join("config");
        fs::create_dir_all(&userdata).unwrap();
        fs::write(
            userdata.join("shortcuts.vdf"),
            vdf::write_binary(&shortcuts_tree_with_exe(
                2385817016,
                "Haste",
                "/mnt/SSD2/Piratas/HASTE.v1.5.c/HASTE.v1.5.c/Haste.exe",
            )),
        )
        .unwrap();

        let (found_path, exe) = find_shortcut_exe_by_name(&root, "Haste").unwrap();
        assert_eq!(found_path, userdata.join("shortcuts.vdf"));
        assert_eq!(exe, "/mnt/SSD2/Piratas/HASTE.v1.5.c/HASTE.v1.5.c/Haste.exe");
    }

    #[test]
    fn find_shortcut_exe_by_name_is_none_for_unknown_name() {
        let root = test_dir("exe-by-name-miss");
        let userdata = root.join("userdata").join("1").join("config");
        fs::create_dir_all(&userdata).unwrap();
        fs::write(
            userdata.join("shortcuts.vdf"),
            vdf::write_binary(&shortcuts_tree_with_exe(42, "Outro Jogo", "/bin/outro")),
        )
        .unwrap();

        assert!(find_shortcut_exe_by_name(&root, "Haste").is_none());
    }

    #[test]
    fn find_stale_shortcuts_flags_the_dead_duplicate_when_a_live_one_exists() {
        // Regressão do incidente real (2026-08-30): remanejo de disco
        // deixou o atalho antigo do Haste (2385817016, Exe morto)
        // convivendo com o atual (2892464705, Exe vivo) em dois
        // `userdata/` diferentes.
        let root = test_dir("stale-real-haste");
        let old_dir = root.join("userdata").join("340852084").join("config");
        let new_dir = root.join("userdata").join("986775062").join("config");
        fs::create_dir_all(&old_dir).unwrap();
        fs::create_dir_all(&new_dir).unwrap();
        fs::write(
            old_dir.join("shortcuts.vdf"),
            vdf::write_binary(&shortcuts_tree_with_exe(
                2385817016,
                "Haste",
                "/mnt/SSD2/Piratas/HASTE.v1.5.c/HASTE.v1.5.c/Haste.exe",
            )),
        )
        .unwrap();
        let live_exe = root.join("Games").join("piratas").join("Haste.exe");
        fs::create_dir_all(live_exe.parent().unwrap()).unwrap();
        fs::write(&live_exe, b"fake exe").unwrap();
        fs::write(
            new_dir.join("shortcuts.vdf"),
            vdf::write_binary(&shortcuts_tree_with_exe(
                2892464705,
                "Haste",
                live_exe.to_str().unwrap(),
            )),
        )
        .unwrap();

        let stale = find_stale_shortcuts(&root);
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].name, "Haste");
        assert_eq!(stale[0].dead_app_id, 2385817016);
        assert_eq!(stale[0].live_app_id, 2892464705);
    }

    #[test]
    fn find_stale_shortcuts_reports_nothing_when_every_entry_is_dead() {
        // Jogo desinstalado em todo lugar é estado normal, não um achado —
        // mesmo princípio já usado em doctor.rs pra ponto de montagem.
        let root = test_dir("stale-all-dead");
        let dir_a = root.join("userdata").join("1").join("config");
        let dir_b = root.join("userdata").join("2").join("config");
        fs::create_dir_all(&dir_a).unwrap();
        fs::create_dir_all(&dir_b).unwrap();
        fs::write(
            dir_a.join("shortcuts.vdf"),
            vdf::write_binary(&shortcuts_tree_with_exe(1, "Jogo Morto", "/nao/existe/a")),
        )
        .unwrap();
        fs::write(
            dir_b.join("shortcuts.vdf"),
            vdf::write_binary(&shortcuts_tree_with_exe(2, "Jogo Morto", "/nao/existe/b")),
        )
        .unwrap();

        assert!(find_stale_shortcuts(&root).is_empty());
    }

    #[test]
    fn find_stale_shortcuts_reports_nothing_for_a_single_entry() {
        let root = test_dir("stale-single");
        let userdata = root.join("userdata").join("1").join("config");
        fs::create_dir_all(&userdata).unwrap();
        fs::write(
            userdata.join("shortcuts.vdf"),
            vdf::write_binary(&shortcuts_tree_with_exe(1, "Jogo Solo", "/nao/existe")),
        )
        .unwrap();

        assert!(find_stale_shortcuts(&root).is_empty());
    }
}
