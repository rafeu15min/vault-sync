//! Catálogo unificado de jogos: compõe `scan.rs` (Steam local),
//! `install_paths.rs` (Heroic/Epic local) e `list.rs` (o que existe no
//! FeuVault) num inventário só-leitura, cross-launcher, que sabe dizer se
//! cada jogo já está aqui, só no FeuVault, ou nos dois — a peça que faltava
//! pro `vault-sync menu` (rofi) mostrar até jogos nunca baixados nesta
//! máquina. Não reimplementa nenhuma descoberta: só junta o que
//! scan/list/install_paths já sabem, por app_id (Steam) ou título (Heroic).

use crate::config::Config;
use crate::domain::GameSource;
use crate::install_paths;
use crate::list::{self, RemoteListing};
use crate::remote::RemoteError;
use crate::scan::{self, ScanError};
use crate::steam_catalog;
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GameLocation {
    LocalOnly,
    RemoteOnly,
    Both,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct CatalogEntry {
    #[serde(flatten)]
    pub source: GameSource,
    pub display_name: String,
    /// `false` quando `display_name` é só o fallback genérico de
    /// `GameSource::display_name()` (nenhuma fonte real — API oficial da
    /// Steam, override, atalho — soube dizer o que é isso). Usado por
    /// `is_launchable` pra tirar do rofi entradas `Official` que a própria
    /// Steam não reconhece: na prática, `compatdata` de ferramenta de
    /// compatibilidade (Proton, Steam Linux Runtime — instalam sem página
    /// de loja pública) que vazou pro FeuVault, não jogo de verdade. Sinal
    /// estrutural (a Steam Store API não sabe o AppID), não uma lista fixa
    /// de IDs — vale em qualquer máquina, sem precisar editar código toda
    /// vez que aparecer uma ferramenta nova.
    pub identity_known: bool,
    pub location: GameLocation,
}

impl CatalogEntry {
    /// Linha determinística pro modo `--format=rofi`: round-trip por
    /// string, sem ID escondido — mesmo princípio já usado em
    /// `~/.config/waybar/scripts/minimized.sh` (reconstrói a linha e
    /// procura de volta em vez de tentar parsear um índice implícito).
    /// `server_name`/`client_name` são os nomes de exibição do servidor
    /// remoto e desta máquina local (`Config::server_name`/
    /// `Config::client_name`, "Servidor"/"Cliente" por padrão) — nunca
    /// hardcoded aqui, pra o catálogo continuar universal em qualquer
    /// instalação. Quem chama precisa passar os MESMOS nomes usados quando
    /// a linha foi gerada (`catalog --format=rofi`) e quando ela é
    /// resolvida de volta (`menu-launch`/`set-proton`/`get-proton`) — como
    /// os dois vêm do mesmo `cfg` na mesma invocação, isso é automático.
    pub fn rofi_line(&self, server_name: &str, client_name: &str) -> String {
        let platform = match self.source {
            GameSource::HeroicGame { .. } => "Epic",
            GameSource::Official { .. } => "Steam",
            GameSource::NonSteamShortcut { .. } => "Steam (non-Steam)",
            GameSource::Orphan { .. } => "Órfão",
        };
        let where_ = match self.location {
            GameLocation::LocalOnly => client_name.to_string(),
            GameLocation::RemoteOnly => server_name.to_string(),
            GameLocation::Both => format!("{client_name}+{server_name}"),
        };
        format!("[{platform}] {} ({where_})", self.display_name)
    }

    /// `false` pra `Orphan` (sem manifesto/atalho conhecido, não há o que o
    /// `MenuLaunch` sequer tente rodar) e pra `Official` sem identidade
    /// conhecida (ver `identity_known` — ferramenta de compatibilidade, não
    /// jogo). `NonSteamShortcut` é launchable como qualquer outro app
    /// Steam — `MenuLaunch` dispara `steam -applaunch`, e o wrapper já
    /// injetado em `LaunchOptions` (`inject.rs`) cuida da sincronização do
    /// lado de dentro. Usado pra não listar no modo rofi opções que sempre
    /// terminam em erro ou que só existem por vazamento de metadado
    /// interno da Steam; o modo `Human`/os dados continuam mostrando tudo.
    pub fn is_launchable(&self) -> bool {
        match self.source {
            GameSource::Orphan { .. } => false,
            GameSource::Official { .. } => self.identity_known,
            GameSource::NonSteamShortcut { .. } | GameSource::HeroicGame { .. } => true,
        }
    }
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Catalog {
    pub entries: Vec<CatalogEntry>,
}

impl Catalog {
    pub fn find_by_rofi_line(
        &self,
        line: &str,
        server_name: &str,
        client_name: &str,
    ) -> Option<&CatalogEntry> {
        self.entries
            .iter()
            .find(|e| e.rofi_line(server_name, client_name) == line)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    #[error("varredura local falhou: {0}")]
    Scan(#[from] ScanError),
    #[error("listagem remota falhou: {0}")]
    Remote(#[from] RemoteError),
}

/// Monta o catálogo completo: varre local (Steam + Heroic/Epic), lista o
/// FeuVault via SSH, e funde os dois. Faz chamadas de rede/E-S — para a
/// lógica de fusão pura e testável sem I/O, ver `merge_catalog`. Propaga
/// erro se a listagem remota falhar — use quando a acessibilidade do
/// FeuVault já foi garantida (ex.: `MenuLaunch`, depois de
/// `remote::ensure_reachable`), e um catálogo incompleto seria pior que um
/// erro explícito.
pub fn build_catalog(cfg: &Config) -> Result<Catalog, CatalogError> {
    let remote = list::run_list(cfg)?;
    build_catalog_from(cfg, remote)
}

/// Como `build_catalog`, mas nunca liga nem espera o FeuVault: uma única
/// tentativa de listagem remota (mesmo `ConnectTimeout=10` de sempre, sem
/// wake, sem retry). Pensada pro menu rofi (`vault-sync-menu.sh`/`Command::
/// Catalog`), que precisa aparecer na hora — esperar o ciclo de wake
/// completo (~2min) ali faz o SUPER+G parecer travado, sem nenhuma saída no
/// meio tempo.
///
/// Se a listagem ao vivo falhar, cai pra `list::read_cache()` — a última
/// listagem remota bem-sucedida, gravada em disco por qualquer chamada
/// anterior de `list::run_list` (outro `catalog`/`menu-launch` com o
/// servidor acordado, `ingest`, `vault-sync list`). Isso é o que permite um
/// jogo 100% remoto (nunca instalado localmente, ou já migrado via `ingest
/// --and-purge-local`) continuar aparecendo no menu com o FeuVault
/// dormindo — sem essa cache, `--and-purge-local` de tudo deixaria o menu
/// vazio sempre que o servidor estivesse desligado, o que anularia o
/// próprio propósito de não bloquear aqui. Só degrada pra catálogo
/// puramente local (nenhum jogo remoto visível) na primeiríssima execução
/// de sempre, antes de qualquer listagem bem-sucedida ter acontecido.
/// Selecionar uma entrada vinda do cache funciona normalmente: `MenuLaunch`
/// liga o servidor e reconstrói o catálogo (ao vivo, sem cache) antes de
/// despachar, então uma entrada cacheada desatualizada (ex.: um jogo já
/// apagado do FeuVault por fora) só falha nesse ponto, nunca silenciosamente.
pub fn build_catalog_best_effort(cfg: &Config) -> Result<Catalog, CatalogError> {
    let remote = list::run_list(cfg).unwrap_or_else(|e| match list::read_cache() {
        Some(cached) => {
            tracing::warn!(
                "{} inacessível ({e}) — catálogo usando a última listagem remota \
                 conhecida (pode estar desatualizada)",
                cfg.server_name()
            );
            cached
        }
        None => {
            tracing::warn!(
                "{} inacessível ({e}) e nenhuma listagem remota cacheada ainda — \
                 catálogo mostrando só itens locais",
                cfg.server_name()
            );
            list::RemoteListing::default()
        }
    });
    build_catalog_from(cfg, remote)
}

fn build_catalog_from(cfg: &Config, remote: list::RemoteListing) -> Result<Catalog, CatalogError> {
    let scan_report = scan::run_scan(&cfg.local_libraries)?;

    let mut local_sources: Vec<GameSource> =
        scan_report.entries.into_iter().map(|e| e.source).collect();
    local_sources.extend(
        local_heroic_titles(cfg)
            .into_iter()
            .map(|title| GameSource::HeroicGame { title }),
    );

    let remote = augment_remote_with_full_install_only_epic_titles(cfg, remote);
    let remote = augment_remote_with_full_install_only_steam_titles(cfg, remote);

    Ok(merge_catalog(local_sources, &remote, |source| {
        resolve_display_name(cfg, source)
    }))
}

/// Reconhece jogos Epic cuja única presença remota é a instalação completa
/// (`remote.epic_installs`, ver `list::RemoteListing`) — nativos Linux, ou
/// qualquer título que nunca criou prefixo Wine. Sem isto, `heroic_titles`
/// (que só cobre `heroic_remote_base_path`) nunca teria como saber que
/// esses jogos existem no servidor, e eles ficariam pra sempre invisíveis
/// no catálogo assim que o `installed.json` do Legendary esquecesse deles
/// (ver `ingest::remove_legendary_installed_entry` — incidente real,
/// 2026-09-07: Chivalry 2 sumiu do SUPER+G depois de confirmado 100% no
/// servidor). `install_path_overrides.epic` (título -> pasta local) é o
/// único mapeamento título<->nome-de-pasta que sobrevive ao Legendary
/// esquecer o jogo; sem override configurado pra um título assim, não tem
/// como saber o nome bonito, e ele continua invisível (mesmo comportamento
/// de antes, sem regressão pra quem não configurou nada).
fn augment_remote_with_full_install_only_epic_titles(
    cfg: &Config,
    mut remote: list::RemoteListing,
) -> list::RemoteListing {
    let Some(epic_installs) = &remote.epic_installs else {
        return remote;
    };
    let already_known = remote.heroic_titles.clone().unwrap_or_default();
    let extra: Vec<String> = cfg
        .install_path_overrides
        .epic
        .iter()
        .filter(|(title, _)| !already_known.contains(title))
        .filter(|(_, path)| {
            path.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .is_some_and(|n| epic_installs.contains(&n))
        })
        .map(|(title, _)| title.clone())
        .collect();
    if extra.is_empty() {
        return remote;
    }
    let mut titles = remote.heroic_titles.take().unwrap_or_default();
    titles.extend(extra);
    titles.sort();
    remote.heroic_titles = Some(titles);
    remote
}

/// Mesmo raciocínio de `augment_remote_with_full_install_only_epic_titles`,
/// do lado Steam oficial: reconhece jogos cuja única presença remota é a
/// instalação completa (`remote.steam_installs`) quando o `compatdata`
/// nunca foi sincronizado de volta (ou se perdeu — `steam_compatdata_ids`
/// não sabe nada sobre eles). Sem isto, um jogo assim fica invisível no
/// catálogo mesmo 100% presente no servidor (incidente real, 2026-09-09:
/// Kenshi — compatdata só local, nunca existiu remoto; instalação completa
/// só remota, nunca local — nenhum dos dois lados bastava sozinho pro
/// catálogo antigo reconhecer o jogo). `install_path_overrides.steam`
/// (app_id -> pasta local) é o único mapeamento app_id<->nome-de-pasta que
/// sobrevive a essa lacuna dupla; sem override configurado pra esse app_id,
/// não tem como saber o nome da pasta remota, e ele continua invisível
/// (mesmo comportamento de antes, sem regressão pra quem não configurou
/// nada).
fn augment_remote_with_full_install_only_steam_titles(
    cfg: &Config,
    mut remote: list::RemoteListing,
) -> list::RemoteListing {
    let Some(steam_installs) = &remote.steam_installs else {
        return remote;
    };
    let already_known: HashSet<&str> = remote
        .steam_compatdata_ids
        .iter()
        .map(String::as_str)
        .collect();
    let extra: Vec<String> = cfg
        .install_path_overrides
        .steam
        .iter()
        .filter(|(app_id, _)| !already_known.contains(app_id.as_str()))
        .filter(|(_, path)| {
            path.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .is_some_and(|n| steam_installs.contains(&n))
        })
        .map(|(app_id, _)| app_id.clone())
        .collect();
    if extra.is_empty() {
        return remote;
    }
    remote.steam_compatdata_ids.extend(extra);
    remote.steam_compatdata_ids.sort();
    remote
}

/// Nomes de exibição: resolução best-effort via `steam_catalog` pras
/// variantes Steam (nunca bloqueia — cai pro `GameSource::display_name()`
/// genérico se a rede falhar ou o ID for desconhecido); título direto pro
/// Heroic, que já é legível. O segundo valor (`identity_known`) diz se essa
/// resolução veio de uma fonte real — `false` só quando nada soube dizer o
/// que é esse AppID, ver `CatalogEntry::identity_known`.
fn resolve_display_name(cfg: &Config, source: &GameSource) -> (String, bool) {
    match source {
        GameSource::Official { .. } | GameSource::NonSteamShortcut { .. } => {
            let app_id = source.app_id_str();
            match steam_catalog::resolve_name(
                &app_id,
                &cfg.install_path_overrides.piratas,
                steam_catalog::fetch_official_name,
            )
            .or_else(|| pirata_shortcut_name(source, &app_id))
            {
                Some(name) => (name, true),
                None => (source.display_name(), false),
            }
        }
        GameSource::HeroicGame { title } => (title.clone(), true),
        GameSource::Orphan { .. } => (source.display_name(), true),
    }
}

/// Fallback de nome pra jogo pirata sem override configurado (adotado em
/// `local_game_root_piratas`) — lê o `appname` do atalho ao vivo, mesma
/// fonte que `scan::resolve_piratas_install_dir` usa pra nomear a pasta
/// adotada. Sem isso, um pirata remoto-only (nunca rodado nesta máquina,
/// só listado via `RemoteListing`) fica sem nome nenhum: `placeholder_steam_source`
/// deixa `shortcut_name` vazio de propósito (não há entrada local de
/// `shortcuts.vdf` pra ler naquele ponto), e sem override o
/// `steam_catalog::resolve_name` também não tem de onde tirar um nome.
fn pirata_shortcut_name(source: &GameSource, app_id: &str) -> Option<String> {
    if !matches!(source, GameSource::NonSteamShortcut { .. }) {
        return None;
    }
    let numeric_id: u32 = app_id.parse().ok()?;
    let steam_root = scan::find_steam_root()?;
    scan::parse_shortcuts_map(&steam_root).remove(&numeric_id)
}

/// União dos títulos Heroic/Epic conhecidos localmente: pastas de prefixo
/// Wine sob `heroic_local_root` (jogos que passaram pelo Wine) + títulos do
/// `installed.json` do Legendary (cobre também nativos Linux, que nunca
/// criam prefixo — mesmo motivo de `install_paths::list_legendary_installed_titles`
/// existir). Só enumera candidatos, sem verificar se o `install_path`
/// gravado ainda existe — mesmo princípio dessa função.
fn local_heroic_titles(cfg: &Config) -> Vec<String> {
    let mut titles: HashSet<String> = HashSet::new();

    if let Some(root) = &cfg.heroic_local_root {
        if let Ok(entries) = std::fs::read_dir(root) {
            for entry in entries.flatten() {
                if entry.path().is_dir() {
                    titles.insert(entry.file_name().to_string_lossy().into_owned());
                }
            }
        }
    }

    if let Some(installed_json) = install_paths::default_legendary_installed_json_path() {
        if installed_json.is_file() {
            titles.extend(install_paths::list_legendary_installed_titles(
                &installed_json,
            ));
        }
    }

    let mut titles: Vec<String> = titles.into_iter().collect();
    titles.sort();
    titles
}

/// Chave de deduplicação/junção: Steam (app_id) e Heroic (título) vivem em
/// namespaces separados, prefixados pra nunca colidir entre si.
fn dedup_key(source: &GameSource) -> String {
    match source {
        GameSource::HeroicGame { title } => format!("heroic:{title}"),
        _ => format!("steam:{}", source.app_id_str()),
    }
}

/// Um app_id remoto sem manifesto/shortcut local pra classificar: usa a
/// mesma faixa numérica que `steam_catalog::is_official_app_id` já usa pra
/// distinguir oficial de non-Steam. `shortcut_name` fica vazio de propósito
/// nesse caso — não existe entrada local em `shortcuts.vdf` pra ler um nome
/// de verdade, e o `display_name` resolvido (campo separado) é o que
/// realmente aparece pro usuário.
fn placeholder_steam_source(app_id: &str) -> GameSource {
    let id: u32 = app_id.parse().unwrap_or(0);
    if steam_catalog::is_official_app_id(app_id) {
        GameSource::Official { app_id: id }
    } else {
        GameSource::NonSteamShortcut {
            app_id: id,
            shortcut_name: String::new(),
        }
    }
}

/// Fusão pura (sem I/O) de fontes locais + listagem remota — separada de
/// `build_catalog` pra ser testável com dados fabricados, mesmo espírito de
/// `steam_catalog::parse_appdetails` vs `fetch_official_name`.
pub(crate) fn merge_catalog(
    local_sources: Vec<GameSource>,
    remote: &RemoteListing,
    mut display_name: impl FnMut(&GameSource) -> (String, bool),
) -> Catalog {
    let mut sources: HashMap<String, GameSource> = HashMap::new();
    let mut local_keys: HashSet<String> = HashSet::new();
    for source in local_sources {
        let key = dedup_key(&source);
        local_keys.insert(key.clone());
        sources.insert(key, source);
    }

    let mut remote_keys: HashSet<String> = HashSet::new();
    for app_id in &remote.steam_compatdata_ids {
        let key = format!("steam:{app_id}");
        remote_keys.insert(key.clone());
        let entry = sources
            .entry(key)
            .or_insert_with(|| placeholder_steam_source(app_id));
        // `Orphan` é só "não deu pra classificar com o que via local" — o
        // FeuVault confirmando esse mesmo app_id assume identidade real
        // (Official/NonSteamShortcut), então substitui em vez de deixar
        // `or_insert_with` (que só age quando a chave está livre) manter um
        // orfão pra sempre invisível no menu (`is_launchable` == false pra
        // Orphan). Incidente real, 2026-09-09: Kenshi tinha `compatdata`
        // local sem manifesto (Orphan) E `compatdata` remoto confirmado — o
        // app_id ocupava a mesma chave `dedup_key` dos dois lados, então o
        // orfão local vencia e o jogo sumia do SUPER+G mesmo com o servidor
        // sabendo exatamente o que ele é.
        if matches!(entry, GameSource::Orphan { .. }) {
            *entry = placeholder_steam_source(app_id);
        }
    }
    for title in remote.heroic_titles.iter().flatten() {
        let key = format!("heroic:{title}");
        remote_keys.insert(key.clone());
        sources
            .entry(key)
            .or_insert_with(|| GameSource::HeroicGame {
                title: title.clone(),
            });
    }

    let mut entries: Vec<CatalogEntry> = sources
        .into_iter()
        .map(|(key, source)| {
            let location = match (local_keys.contains(&key), remote_keys.contains(&key)) {
                (true, true) => GameLocation::Both,
                (true, false) => GameLocation::LocalOnly,
                (false, true) => GameLocation::RemoteOnly,
                (false, false) => unreachable!("toda entrada vem de local ou remoto"),
            };
            let (display_name, identity_known) = display_name(&source);
            CatalogEntry {
                source,
                display_name,
                identity_known,
                location,
            }
        })
        .collect();

    entries.sort_by(|a, b| a.display_name.cmp(&b.display_name));
    Catalog { entries }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn official(app_id: u32) -> GameSource {
        GameSource::Official { app_id }
    }

    fn heroic(title: &str) -> GameSource {
        GameSource::HeroicGame {
            title: title.to_string(),
        }
    }

    #[test]
    fn resolve_display_name_uses_bare_title_for_heroic_not_the_generic_prefix() {
        // Regressão: `GameSource::display_name()` genérico prefixa "Heroic:
        // " (pensado pra logs), o que duplicava a plataforma já mostrada
        // pelo `rofi_line` (ex.: "[Epic] Heroic: Chivalry 2").
        let cfg = Config::test_default();
        let (name, known) = resolve_display_name(&cfg, &heroic("Chivalry 2"));
        assert_eq!(name, "Chivalry 2");
        assert!(known);
    }

    #[test]
    fn orphan_is_not_launchable() {
        let orphan = GameSource::Orphan {
            app_id: "123".to_string(),
            clues: vec![],
        };
        let entry = CatalogEntry {
            source: orphan,
            display_name: "X".to_string(),
            identity_known: true,
            location: GameLocation::LocalOnly,
        };
        assert!(!entry.is_launchable());
    }

    #[test]
    fn official_heroic_and_non_steam_shortcut_are_launchable_when_identity_known() {
        let shortcut = GameSource::NonSteamShortcut {
            app_id: 123,
            shortcut_name: "X".to_string(),
        };
        for source in [official(730), heroic("X"), shortcut] {
            let entry = CatalogEntry {
                source,
                display_name: "X".to_string(),
                identity_known: true,
                location: GameLocation::LocalOnly,
            };
            assert!(entry.is_launchable());
        }
    }

    #[test]
    fn official_with_unknown_identity_is_not_launchable() {
        // Sinal real: AppID Official que a Steam Store não reconhece (ex.:
        // ferramenta de compatibilidade — Proton, Steam Linux Runtime —
        // que vazou pro FeuVault como `compatdata`, não um jogo).
        let entry = CatalogEntry {
            source: official(2348590),
            display_name: "Steam AppID 2348590".to_string(),
            identity_known: false,
            location: GameLocation::RemoteOnly,
        };
        assert!(!entry.is_launchable());
    }

    #[test]
    fn non_steam_shortcut_with_unknown_identity_stays_launchable() {
        // `identity_known` só afeta `Official` — um atalho pirata sem nome
        // resolvido ainda é um app Steam de verdade, lançável via
        // `-applaunch`.
        let shortcut = GameSource::NonSteamShortcut {
            app_id: 123,
            shortcut_name: String::new(),
        };
        let entry = CatalogEntry {
            source: shortcut,
            display_name: "Steam AppID 123".to_string(),
            identity_known: false,
            location: GameLocation::RemoteOnly,
        };
        assert!(entry.is_launchable());
    }

    fn remote(steam_ids: &[&str], heroic_titles: Option<&[&str]>) -> RemoteListing {
        RemoteListing {
            steam_compatdata_ids: steam_ids.iter().map(|s| s.to_string()).collect(),
            heroic_titles: heroic_titles
                .map(|titles| titles.iter().map(|s| s.to_string()).collect()),
            piratas_installs: None,
            epic_installs: None,
            steam_installs: None,
        }
    }

    #[test]
    fn local_only_game_is_classified_correctly() {
        let catalog = merge_catalog(vec![official(730)], &remote(&[], None), |s| {
            (s.display_name(), true)
        });
        assert_eq!(catalog.entries.len(), 1);
        assert_eq!(catalog.entries[0].location, GameLocation::LocalOnly);
    }

    #[test]
    fn remote_only_official_game_gets_a_placeholder_source() {
        let catalog = merge_catalog(vec![], &remote(&["730"], None), |s| {
            (s.display_name(), true)
        });
        assert_eq!(catalog.entries.len(), 1);
        assert_eq!(catalog.entries[0].location, GameLocation::RemoteOnly);
        assert!(matches!(
            catalog.entries[0].source,
            GameSource::Official { app_id: 730 }
        ));
    }

    #[test]
    fn remote_only_non_steam_id_is_classified_as_shortcut() {
        // Acima do threshold de 2^31 usado por steam_catalog::is_official_app_id.
        let catalog = merge_catalog(vec![], &remote(&["2385817016"], None), |s| {
            (s.display_name(), true)
        });
        assert!(matches!(
            catalog.entries[0].source,
            GameSource::NonSteamShortcut {
                app_id: 2385817016,
                ..
            }
        ));
    }

    #[test]
    fn game_present_locally_and_remotely_is_both() {
        let catalog = merge_catalog(vec![official(730)], &remote(&["730"], None), |s| {
            (s.display_name(), true)
        });
        assert_eq!(catalog.entries.len(), 1);
        assert_eq!(catalog.entries[0].location, GameLocation::Both);
    }

    #[test]
    fn remote_confirmation_upgrades_a_local_orphan_into_a_launchable_official_game() {
        // Regressão real (2026-09-09): Kenshi tinha `compatdata` local sem
        // `appmanifest` (por isso classificado `Orphan` por `scan.rs`) E
        // `compatdata` confirmado no FeuVault — antes deste fix, o `Orphan`
        // local ocupava a chave e o app_id ficava invisível no menu mesmo
        // com o servidor confirmando de que jogo se trata.
        let orphan = GameSource::Orphan {
            app_id: "233860".to_string(),
            clues: vec![],
        };
        let catalog = merge_catalog(vec![orphan], &remote(&["233860"], None), |s| {
            (s.display_name(), true)
        });
        assert_eq!(catalog.entries.len(), 1);
        assert_eq!(catalog.entries[0].location, GameLocation::Both);
        assert!(matches!(
            catalog.entries[0].source,
            GameSource::Official { app_id: 233860 }
        ));
        assert!(catalog.entries[0].is_launchable());
    }

    #[test]
    fn heroic_and_steam_namespaces_never_collide() {
        // Título Heroic igual ao app_id Steam (como string) não deve fundir.
        let catalog = merge_catalog(
            vec![official(730), heroic("730")],
            &remote(&[], None),
            |s| (s.display_name(), true),
        );
        assert_eq!(catalog.entries.len(), 2);
    }

    #[test]
    fn heroic_remote_only_title_is_classified_correctly() {
        let catalog = merge_catalog(vec![], &remote(&[], Some(&["Hogwarts Legacy"])), |s| {
            (s.display_name(), true)
        });
        assert_eq!(catalog.entries.len(), 1);
        assert_eq!(catalog.entries[0].location, GameLocation::RemoteOnly);
        assert!(matches!(
            &catalog.entries[0].source,
            GameSource::HeroicGame { title } if title == "Hogwarts Legacy"
        ));
    }

    #[test]
    fn entries_are_sorted_by_display_name() {
        let catalog = merge_catalog(
            vec![heroic("Zelda"), heroic("Alan Wake")],
            &remote(&[], None),
            |s| (s.display_name(), true),
        );
        let names: Vec<&str> = catalog
            .entries
            .iter()
            .map(|e| e.display_name.as_str())
            .collect();
        assert_eq!(names, vec!["Heroic: Alan Wake", "Heroic: Zelda"]);
    }

    #[test]
    fn rofi_line_round_trips_back_to_the_same_entry() {
        let catalog = merge_catalog(vec![official(730)], &remote(&["730"], None), |s| {
            (s.display_name(), true)
        });
        let line = catalog.entries[0].rofi_line("Servidor", "Cliente");
        let found = catalog.find_by_rofi_line(&line, "Servidor", "Cliente");
        assert_eq!(found, Some(&catalog.entries[0]));
    }

    #[test]
    fn unknown_rofi_line_resolves_to_none() {
        let catalog = merge_catalog(vec![official(730)], &remote(&[], None), |s| {
            (s.display_name(), true)
        });
        assert_eq!(
            catalog.find_by_rofi_line("linha que não existe", "Servidor", "Cliente"),
            None
        );
    }

    #[test]
    fn rofi_line_uses_the_configured_server_name_instead_of_a_hardcoded_one() {
        // A ferramenta precisa continuar universal em qualquer instalação
        // — nada de "FeuVault" cravado no código; o nome vem de
        // `Config::server_name`, repassado por quem monta a linha.
        let catalog = merge_catalog(vec![official(730)], &remote(&["730"], None), |s| {
            (s.display_name(), true)
        });
        let line = catalog.entries[0].rofi_line("Meu Servidor", "Cliente");
        assert!(line.ends_with("Meu Servidor)"), "linha: {line}");
    }

    #[test]
    fn rofi_line_uses_the_configured_client_name_instead_of_a_hardcoded_one() {
        let catalog = merge_catalog(vec![heroic("Zelda")], &remote(&[], None), |s| {
            (s.display_name(), true)
        });
        let line = catalog.entries[0].rofi_line("Servidor", "Minha Máquina");
        assert!(line.ends_with("(Minha Máquina)"), "linha: {line}");
    }

    fn cfg_with_epic_override(title: &str, basename: &str) -> Config {
        let mut cfg = Config::test_default();
        cfg.install_path_overrides.epic.insert(
            title.to_string(),
            std::path::PathBuf::from(format!("/qualquer/{basename}")),
        );
        cfg
    }

    #[test]
    fn augment_recognizes_epic_game_present_only_as_a_full_install() {
        // Regressão real (2026-09-07): Chivalry 2 é nativo Linux, nunca
        // criou prefixo Wine, então `heroic_titles` nunca soube dele — só
        // aparecia no catálogo por acidente, via uma entrada obsoleta do
        // `installed.json` do Legendary. Assim que essa entrada foi limpa
        // (comportamento correto), o jogo sumiu do SUPER+G mesmo 100%
        // migrado. `install_path_overrides.epic` é o que permite reconhecer
        // esse caso sem depender do Legendary.
        let cfg = cfg_with_epic_override("Chivalry 2", "Chivalry2");
        let mut remote = remote(&[], None);
        remote.epic_installs = Some(vec!["Chivalry2".to_string()]);

        let augmented = augment_remote_with_full_install_only_epic_titles(&cfg, remote);

        assert_eq!(
            augmented.heroic_titles,
            Some(vec!["Chivalry 2".to_string()])
        );
    }

    #[test]
    fn augment_does_not_duplicate_a_title_already_known_via_prefix() {
        let cfg = cfg_with_epic_override("Disco Elysium - The Final Cut", "DiscoElysium");
        let mut remote = remote(&[], Some(&["Disco Elysium - The Final Cut"]));
        remote.epic_installs = Some(vec!["DiscoElysium".to_string()]);

        let augmented = augment_remote_with_full_install_only_epic_titles(&cfg, remote);

        assert_eq!(
            augmented.heroic_titles,
            Some(vec!["Disco Elysium - The Final Cut".to_string()])
        );
    }

    #[test]
    fn augment_ignores_override_whose_basename_is_not_confirmed_remotely() {
        let cfg = cfg_with_epic_override("Jogo Nunca Enviado", "JogoNuncaEnviado");
        let mut remote = remote(&[], None);
        remote.epic_installs = Some(vec!["OutroJogo".to_string()]);

        let augmented = augment_remote_with_full_install_only_epic_titles(&cfg, remote);

        assert_eq!(augmented.heroic_titles, None);
    }

    #[test]
    fn augment_is_a_noop_when_remote_installs_base_path_is_not_configured() {
        let cfg = cfg_with_epic_override("Chivalry 2", "Chivalry2");
        let remote = remote(&[], None); // epic_installs: None

        let augmented = augment_remote_with_full_install_only_epic_titles(&cfg, remote);

        assert_eq!(augmented.heroic_titles, None);
    }

    fn cfg_with_steam_override(app_id: &str, basename: &str) -> Config {
        let mut cfg = Config::test_default();
        cfg.install_path_overrides.steam.insert(
            app_id.to_string(),
            std::path::PathBuf::from(format!("/qualquer/{basename}")),
        );
        cfg
    }

    #[test]
    fn augment_recognizes_steam_game_present_only_as_a_full_install() {
        // Regressão real (2026-09-09): Kenshi tinha `compatdata` só local
        // (nunca sincronizado de volta) e instalação completa só remota —
        // nenhum dos dois lados sozinho bastava pro catálogo antigo
        // reconhecer o jogo, que sumiu do SUPER+G. `install_path_overrides
        // .steam` é o que permite reconhecer esse caso sem depender de
        // `compatdata` remoto existir.
        let cfg = cfg_with_steam_override("233860", "Kenshi");
        let mut remote = remote(&[], None);
        remote.steam_installs = Some(vec!["Kenshi".to_string()]);

        let augmented = augment_remote_with_full_install_only_steam_titles(&cfg, remote);

        assert_eq!(augmented.steam_compatdata_ids, vec!["233860".to_string()]);
    }

    #[test]
    fn augment_does_not_duplicate_an_app_id_already_known_via_compatdata() {
        let cfg = cfg_with_steam_override("233860", "Kenshi");
        let mut remote = remote(&["233860"], None);
        remote.steam_installs = Some(vec!["Kenshi".to_string()]);

        let augmented = augment_remote_with_full_install_only_steam_titles(&cfg, remote);

        assert_eq!(augmented.steam_compatdata_ids, vec!["233860".to_string()]);
    }

    #[test]
    fn augment_ignores_steam_override_whose_basename_is_not_confirmed_remotely() {
        let cfg = cfg_with_steam_override("999999", "JogoNuncaEnviado");
        let mut remote = remote(&[], None);
        remote.steam_installs = Some(vec!["OutroJogo".to_string()]);

        let augmented = augment_remote_with_full_install_only_steam_titles(&cfg, remote);

        assert_eq!(augmented.steam_compatdata_ids, Vec::<String>::new());
    }

    #[test]
    fn augment_steam_is_a_noop_when_remote_installs_base_path_is_not_configured() {
        let cfg = cfg_with_steam_override("233860", "Kenshi");
        let remote = remote(&[], None); // steam_installs: None

        let augmented = augment_remote_with_full_install_only_steam_titles(&cfg, remote);

        assert_eq!(augmented.steam_compatdata_ids, Vec::<String>::new());
    }
}
