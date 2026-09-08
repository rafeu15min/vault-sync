use serde::Serialize;
use std::path::PathBuf;

/// Classifica a origem de um jogo, replicando a heurística já validada em
/// `steam_scan.py`:
///
/// - `Official`  -> existe `appmanifest_<app_id>.acf` em alguma biblioteca Steam.
/// - `NonSteamShortcut` -> entrada em `shortcuts.vdf` de algum usuário local
///   (jogos "non-Steam": GOG, homebrew, launchers de terceiros como o Hydra, etc.).
/// - `Orphan` -> prefixo de compatdata sem manifesto e sem shortcut correspondente;
///   a origem só pode ser reconstruída por investigação forense (registro, saves).
/// - `HeroicGame` -> prefixo Wine do Heroic Games Launcher (hoje: títulos Epic
///   rodados via Legendary/Heroic sobre Windows), identificado pelo título
///   exibido — que é literalmente o nome da pasta em
///   `~/Games/Heroic/Prefixes/default/<título>`, não um app_id numérico. Ao
///   contrário do `compatdata/<app_id>` da Steam, o Heroic não expõe um save
///   path confiável para todo jogo (`installed.json` do Legendary tem
///   `save_path: null` em vários títulos), então o prefixo inteiro é tratado
///   como unidade de sincronização — mesma estratégia usada para o
///   `compatdata_dir` da Steam.
///
/// A tipagem forte é proposital: cada variante só carrega os dados que
/// realmente possui, então o resto do código não consegue pedir, por
/// exemplo, o caminho de um `appmanifest` para um `Orphan` — isso vira erro
/// de compilação em vez de bug em tempo de execução.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GameSource {
    Official {
        app_id: u32,
    },
    NonSteamShortcut {
        app_id: u32,
        shortcut_name: String,
    },
    Orphan {
        /// Nome bruto da pasta em `compatdata/` (sempre numérico na prática,
        /// mas mantido como String pois vem diretamente da listagem do
        /// diretório e ainda não foi validado).
        app_id: String,
        /// Pistas coletadas pela varredura forense (registro do Wine,
        /// atalhos .lnk, pastas de save) — vazio se nada foi encontrado.
        clues: Vec<String>,
    },
    HeroicGame {
        /// Nome da pasta em `Prefixes/default/` — igual ao campo `title` do
        /// `installed.json` do Legendary, ainda não validado contra path
        /// traversal (isso é responsabilidade de `paths.rs`).
        title: String,
    },
}

impl GameSource {
    /// Representação textual do identificador, independente da variante —
    /// app_id numérico para as variantes Steam, título de pasta para o
    /// Heroic. `paths.rs` decide, por variante, quais regras de segurança de
    /// caminho validam esse valor antes de usá-lo para montar caminhos.
    pub fn app_id_str(&self) -> String {
        match self {
            GameSource::Official { app_id } => app_id.to_string(),
            GameSource::NonSteamShortcut { app_id, .. } => app_id.to_string(),
            GameSource::Orphan { app_id, .. } => app_id.clone(),
            GameSource::HeroicGame { title } => title.clone(),
        }
    }

    /// Nome amigável para logs e para os diálogos zenity/kdialog (Fase II).
    pub fn display_name(&self) -> String {
        match self {
            GameSource::Official { app_id } => format!("Steam AppID {app_id}"),
            GameSource::NonSteamShortcut { shortcut_name, .. } => shortcut_name.clone(),
            GameSource::Orphan { app_id, clues } if !clues.is_empty() => {
                format!("Órfão {app_id} (pistas: {})", clues.join(" | "))
            }
            GameSource::Orphan { app_id, .. } => format!("Órfão {app_id} (sem pistas)"),
            GameSource::HeroicGame { title } => format!("Heroic: {title}"),
        }
    }

    /// Só jogos oficiais têm um `appmanifest_*.acf` canônico para ler/mover.
    pub fn is_official(&self) -> bool {
        matches!(self, GameSource::Official { .. })
    }

    /// Só jogos Heroic vivem sob `Prefixes/default/<título>` em vez de
    /// `steamapps/compatdata/<app_id>`.
    pub fn is_heroic(&self) -> bool {
        matches!(self, GameSource::HeroicGame { .. })
    }
}

/// Qual launcher um comando (`run`/`purge`) deve tratar `app_id` como sendo
/// de — decide tanto a construção do `GameSource` quanto quais campos de
/// `Config` (Steam vs `heroic_*`) fornecem os diretórios local/remoto.
/// Escolhido explicitamente via `--steam`/`--heroic` na CLI: um título
/// Heroic poderia coincidentemente ser só dígitos, então não dá pra inferir
/// com segurança a partir do valor de `app_id` sozinho.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Launcher {
    Steam,
    Heroic,
}

/// Uma biblioteca Steam local (um dos diretórios listados em
/// `libraryfolders.vdf`, incluindo a raiz e bibliotecas manuais como as
/// injetadas em `steam_scan.py`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryLocation {
    pub root: PathBuf,
}

impl LibraryLocation {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
}

/// Estado de uma sessão de jogo gerenciada pelo vault-sync: do download a
/// partir do FeuVault até a limpeza local pós-execução. Preenchido
/// incrementalmente pelas fases seguintes (o motor de processos é Fase III).
#[derive(Debug, Clone)]
pub struct GameSession {
    pub source: GameSource,
    pub child_pid: Option<u32>,
}

impl GameSession {
    pub fn new(source: GameSource) -> Self {
        Self {
            source,
            child_pid: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn official_is_official_only_for_official_variant() {
        assert!(GameSource::Official { app_id: 730 }.is_official());
        assert!(!GameSource::NonSteamShortcut {
            app_id: 1,
            shortcut_name: "X".into()
        }
        .is_official());
        assert!(!GameSource::Orphan {
            app_id: "1".into(),
            clues: vec![]
        }
        .is_official());
    }

    #[test]
    fn app_id_str_matches_variant() {
        assert_eq!(GameSource::Official { app_id: 730 }.app_id_str(), "730");
        assert_eq!(
            GameSource::Orphan {
                app_id: "999".into(),
                clues: vec![]
            }
            .app_id_str(),
            "999"
        );
    }
}
