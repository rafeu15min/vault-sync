use clap::{ArgGroup, Parser, Subcommand};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    name = "vault-sync",
    version,
    about = "Mantém o SSD local como zona de alocação efêmera, sincronizando jogos com o servidor."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,

    /// Caminho do arquivo de configuração (padrão: ~/.config/vault-sync/config.json)
    #[arg(long, global = true)]
    pub config: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum CatalogFormat {
    /// Tabela legível, uma linha por jogo (mesmo formato usado no modo rofi).
    Human,
    /// Uma linha por jogo, sem cabeçalho — pronta pra `rofi -dmenu`.
    Rofi,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Baixa um jogo do servidor, aguarda a execução e, ao encerrar,
    /// sincroniza de volta + limpa o SSD local. (Motor: Fase III; Heroic:
    /// Fase VI)
    #[command(group(ArgGroup::new("launcher_run").required(true).args(["steam", "heroic"])))]
    Run {
        /// AppID do jogo (Steam) ou título da pasta em Prefixes/default (Heroic)
        app_id: String,

        /// Matriz de comando do jogo, injetada pela plataforma de
        /// distribuição (ex.: Steam via "%command%" nas opções de
        /// lançamento). Tudo após `--` é repassado tal qual ao processo
        /// filho.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        exec: Vec<String>,

        /// `app_id` é um AppID numérico da Steam
        #[arg(long)]
        steam: bool,

        /// `app_id` é o título de um prefixo Heroic (Prefixes/default/<título>)
        #[arg(long)]
        heroic: bool,
    },

    /// Remove definitivamente um jogo do servidor e apaga os registros
    /// locais correspondentes. (Motor: Fase V; Heroic: Fase VI)
    #[command(group(ArgGroup::new("launcher_purge").required(true).args(["steam", "heroic"])))]
    Purge {
        app_id: String,
        /// Confirma a remoção irrevogável sem pedir confirmação interativa
        #[arg(long)]
        yes: bool,

        /// `app_id` é um AppID numérico da Steam
        #[arg(long)]
        steam: bool,

        /// `app_id` é o título de um prefixo Heroic (Prefixes/default/<título>)
        #[arg(long)]
        heroic: bool,
    },

    /// Atualiza um jogo Steam oficial já instalado localmente via
    /// `steamcmd`, direto do CDN da Valve — nunca toca o servidor,
    /// propositalmente separado de `run`/`ingest` (ver update.rs).
    Update {
        /// AppID do jogo (Steam)
        app_id: String,
    },

    /// Emite o catálogo unificado (Steam+Heroic+pirata, local+servidor) —
    /// `--format=rofi` gera uma linha por jogo pra alimentar `rofi -dmenu`
    /// (ver tools/vault-sync-menu.sh). Nunca muta nada.
    Catalog {
        #[arg(long, value_enum, default_value_t = CatalogFormat::Human)]
        format: CatalogFormat,
    },

    /// Uso interno de `tools/vault-sync-menu.sh`: recebe de volta a linha
    /// exata que `catalog --format=rofi` gerou e despacha pro `run`
    /// correspondente. Não é pensado pra digitar na mão.
    MenuLaunch { selection: String },

    /// Compara a biblioteca local com o servidor e envia a ele os
    /// jogos novos encontrados apenas localmente. (Motor: Fase V)
    Ingest {
        /// Roda em segundo plano sem bloquear o terminal
        #[arg(long)]
        background: bool,

        /// Além do compatdata, também resolve e envia a instalação
        /// completa de cada jogo local-only (Fase VII) — exige
        /// `remote_installs_base_path` configurado. Jogos cujo caminho de
        /// instalação não puder ser verificado em disco são pulados (não é
        /// erro fatal), listados ao final.
        #[arg(long)]
        full: bool,

        /// Apaga o compatdata (+ manifesto, se oficial) local de cada jogo
        /// logo após o upload correspondente ser confirmado — sem isso,
        /// `ingest` nunca apaga nada local (comportamento padrão desde a
        /// Fase V). Com `--full`, também apaga a instalação completa — mas
        /// só para jogos cuja instalação foi de fato verificada/enviada.
        #[arg(long)]
        and_purge_local: bool,

        /// Restringe a varredura a um único app_id (Steam) ou título
        /// (Heroic) — todo o resto é ignorado. Pensado pra testar
        /// `--full`/`--and-purge-local` com segurança em um jogo por vez
        /// antes de rodar sem filtro.
        #[arg(long)]
        only: Option<String>,
    },

    /// Executa a varredura forense (equivalente nativo do steam_scan.py) e
    /// gera o catálogo local de jogos oficiais/non-Steam/órfãos.
    /// (Motor: Fase IV — port do script Python)
    Scan {
        /// Caminho de saída do relatório JSON
        #[arg(long, default_value = "steam_catalog.json")]
        output: PathBuf,
    },

    /// Diagnóstico pré-voo: confere bibliotecas locais, conectividade SSH e
    /// `remote_base_path` no servidor sem mutar nada. Recomendado antes do
    /// primeiro uso de `run`/`ingest`/`purge`.
    Doctor,

    /// Lista o que já existe no servidor (compatdata Steam, prefixos
    /// Heroic, instalações completas de pirata) sem mutar nada — útil pra
    /// saber o que já foi migrado ou o que dá pra `stage`.
    List,

    /// Traz a instalação completa de um jogo pirata do servidor direto pra
    /// `local_game_root_piratas/<nome>`, sem precisar de app_id nem atalho
    /// existente — resolve o problema de ovo-e-galinha de adicionar um
    /// pirata novo pela Steam: "Add a Non-Steam Game" exige navegar até um
    /// `.exe` que já existe em disco, e um jogo nunca baixado nesta máquina
    /// não tem onde a Steam olhar até isso rodar uma vez. `<nome>` é o nome
    /// da pasta em `game-installs/piratas/<nome>` no servidor — ver
    /// `vault-sync list` pros nomes disponíveis. Depois do `stage`, aponte
    /// o diálogo da Steam pro `.exe` dentro da pasta impressa; o resto do
    /// ciclo (download/upload/limpeza a cada sessão) já funciona sozinho
    /// via `local_game_root_piratas`, ver `scan::resolve_piratas_install_dir`.
    Stage {
        /// Nome da pasta em `game-installs/piratas/<nome>` no servidor
        name: String,
    },

    /// Uso interno de `tools/vault-sync-menu.sh`: versões de Proton
    /// instaladas nesta máquina (`compatibilitytools.d`, raiz da Steam +
    /// raiz do sistema), uma por linha — alimenta o segundo Rofi de
    /// seleção (ver `proton.rs`). Nunca muta nada, não exige config.json.
    ListProton,

    /// Uso interno de `tools/vault-sync-menu.sh`: versão de Proton já
    /// definida (`Config::proton_overrides`) pra o atalho non-Steam
    /// correspondente a `selection` (a mesma linha que `catalog
    /// --format=rofi` gerou). Imprime o nome e sai com código 0 se houver
    /// um padrão definido; sai com código != 0 (sem saída em stdout) caso
    /// contrário — é assim que o wrapper decide se abre o segundo Rofi
    /// antes de lançar.
    GetProton { selection: String },

    /// Uso interno de `tools/vault-sync-menu.sh`: grava `name` (precisa
    /// estar entre as versões instaladas — ver `list-proton`) como Proton
    /// padrão pro atalho non-Steam correspondente a `selection`, em
    /// `Config::proton_overrides`. A PRIMEIRA escolha feita no segundo Rofi
    /// vira o padrão; trocas subsequentes (Alt+P no menu) sobrescrevem sem
    /// pedir confirmação extra.
    SetProton { selection: String, name: String },

    /// Remove os atalhos Steam obsoletos que `vault-sync doctor` reporta
    /// (resíduo de remanejo de disco — `Exe` morto, mas com um AppID vivo
    /// do mesmo jogo em outro lugar). Nunca remove um AppID que `doctor`
    /// não tenha primeiro classificado como morto (ver
    /// `scan::find_stale_shortcuts`) — só apaga a entrada de
    /// `shortcuts.vdf`, nunca dados no servidor. Backup atômico automático
    /// (mesmo motor de `purge`) antes de qualquer escrita.
    PruneShortcuts {
        /// Remove sem pedir confirmação interativa
        #[arg(long)]
        yes: bool,
    },

    /// Define o nome de exibição do servidor remoto (usado no catálogo,
    /// progresso de sincronização e demais mensagens). Sem isto, o padrão é
    /// "Servidor" — universal, não amarrado a nenhum apelido específico de
    /// máquina.
    SetServerName {
        /// Nome a exibir para o servidor remoto
        name: String,
    },

    /// Define o nome de exibição desta máquina local (usado no catálogo e
    /// demais mensagens). Sem isto, o padrão é "Cliente".
    SetClientName {
        /// Nome a exibir para esta máquina local
        name: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_requires_app_id() {
        let err = Cli::try_parse_from(["vault-sync", "run"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn run_parses_app_id() {
        let cli = Cli::try_parse_from(["vault-sync", "run", "730", "--steam"]).unwrap();
        match cli.command {
            Command::Run {
                app_id,
                exec,
                steam,
                heroic,
            } => {
                assert_eq!(app_id, "730");
                assert!(exec.is_empty());
                assert!(steam);
                assert!(!heroic);
            }
            other => panic!("comando inesperado: {other:?}"),
        }
    }

    #[test]
    fn run_captures_trailing_exec_command() {
        let cli = Cli::try_parse_from([
            "vault-sync",
            "run",
            "730",
            "--steam",
            "--",
            "/path/to/game",
            "--fullscreen",
            "-w",
            "1920",
        ])
        .unwrap();
        match cli.command {
            Command::Run { app_id, exec, .. } => {
                assert_eq!(app_id, "730");
                assert_eq!(exec, vec!["/path/to/game", "--fullscreen", "-w", "1920"]);
            }
            other => panic!("comando inesperado: {other:?}"),
        }
    }

    #[test]
    fn run_parses_heroic_title() {
        let cli =
            Cli::try_parse_from(["vault-sync", "run", "Hogwarts Legacy", "--heroic"]).unwrap();
        match cli.command {
            Command::Run {
                app_id,
                steam,
                heroic,
                ..
            } => {
                assert_eq!(app_id, "Hogwarts Legacy");
                assert!(!steam);
                assert!(heroic);
            }
            other => panic!("comando inesperado: {other:?}"),
        }
    }

    #[test]
    fn run_requires_exactly_one_launcher_flag() {
        let missing = Cli::try_parse_from(["vault-sync", "run", "730"]).unwrap_err();
        assert_eq!(
            missing.kind(),
            clap::error::ErrorKind::MissingRequiredArgument
        );

        let both =
            Cli::try_parse_from(["vault-sync", "run", "730", "--steam", "--heroic"]).unwrap_err();
        assert_eq!(both.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn purge_defaults_yes_to_false() {
        let cli = Cli::try_parse_from(["vault-sync", "purge", "730", "--steam"]).unwrap();
        match cli.command {
            Command::Purge { app_id, yes, .. } => {
                assert_eq!(app_id, "730");
                assert!(!yes);
            }
            other => panic!("comando inesperado: {other:?}"),
        }
    }

    #[test]
    fn purge_requires_exactly_one_launcher_flag() {
        let missing = Cli::try_parse_from(["vault-sync", "purge", "730"]).unwrap_err();
        assert_eq!(
            missing.kind(),
            clap::error::ErrorKind::MissingRequiredArgument
        );

        let both =
            Cli::try_parse_from(["vault-sync", "purge", "730", "--steam", "--heroic"]).unwrap_err();
        assert_eq!(both.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn update_parses_app_id() {
        let cli = Cli::try_parse_from(["vault-sync", "update", "730"]).unwrap();
        match cli.command {
            Command::Update { app_id } => assert_eq!(app_id, "730"),
            other => panic!("comando inesperado: {other:?}"),
        }
    }

    #[test]
    fn catalog_defaults_to_human_format() {
        let cli = Cli::try_parse_from(["vault-sync", "catalog"]).unwrap();
        match cli.command {
            Command::Catalog { format } => assert_eq!(format, CatalogFormat::Human),
            other => panic!("comando inesperado: {other:?}"),
        }
    }

    #[test]
    fn catalog_parses_rofi_format() {
        let cli = Cli::try_parse_from(["vault-sync", "catalog", "--format", "rofi"]).unwrap();
        match cli.command {
            Command::Catalog { format } => assert_eq!(format, CatalogFormat::Rofi),
            other => panic!("comando inesperado: {other:?}"),
        }
    }

    #[test]
    fn menu_launch_parses_selection() {
        let cli =
            Cli::try_parse_from(["vault-sync", "menu-launch", "[Steam] Portal 2 (local)"]).unwrap();
        match cli.command {
            Command::MenuLaunch { selection } => assert_eq!(selection, "[Steam] Portal 2 (local)"),
            other => panic!("comando inesperado: {other:?}"),
        }
    }

    #[test]
    fn scan_defaults_output_filename() {
        let cli = Cli::try_parse_from(["vault-sync", "scan"]).unwrap();
        match cli.command {
            Command::Scan { output } => assert_eq!(output, PathBuf::from("steam_catalog.json")),
            other => panic!("comando inesperado: {other:?}"),
        }
    }

    #[test]
    fn doctor_parses_with_no_args() {
        let cli = Cli::try_parse_from(["vault-sync", "doctor"]).unwrap();
        assert!(matches!(cli.command, Command::Doctor));
    }

    #[test]
    fn stage_parses_name() {
        let cli = Cli::try_parse_from(["vault-sync", "stage", "Haste"]).unwrap();
        match cli.command {
            Command::Stage { name } => assert_eq!(name, "Haste"),
            other => panic!("comando inesperado: {other:?}"),
        }
    }

    #[test]
    fn stage_requires_name() {
        let err = Cli::try_parse_from(["vault-sync", "stage"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn list_proton_parses_with_no_args() {
        let cli = Cli::try_parse_from(["vault-sync", "list-proton"]).unwrap();
        assert!(matches!(cli.command, Command::ListProton));
    }

    #[test]
    fn get_proton_parses_selection() {
        let cli = Cli::try_parse_from([
            "vault-sync",
            "get-proton",
            "[Steam (non-Steam)] Haste (local)",
        ])
        .unwrap();
        match cli.command {
            Command::GetProton { selection } => {
                assert_eq!(selection, "[Steam (non-Steam)] Haste (local)")
            }
            other => panic!("comando inesperado: {other:?}"),
        }
    }

    #[test]
    fn set_proton_parses_selection_and_name() {
        let cli = Cli::try_parse_from([
            "vault-sync",
            "set-proton",
            "[Steam (non-Steam)] Haste (local)",
            "GE-Proton10-34",
        ])
        .unwrap();
        match cli.command {
            Command::SetProton { selection, name } => {
                assert_eq!(selection, "[Steam (non-Steam)] Haste (local)");
                assert_eq!(name, "GE-Proton10-34");
            }
            other => panic!("comando inesperado: {other:?}"),
        }
    }

    #[test]
    fn set_proton_requires_both_args() {
        let err = Cli::try_parse_from(["vault-sync", "set-proton", "so-a-selecao"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn prune_shortcuts_defaults_yes_to_false() {
        let cli = Cli::try_parse_from(["vault-sync", "prune-shortcuts"]).unwrap();
        match cli.command {
            Command::PruneShortcuts { yes } => assert!(!yes),
            other => panic!("comando inesperado: {other:?}"),
        }
    }

    #[test]
    fn prune_shortcuts_parses_yes_flag() {
        let cli = Cli::try_parse_from(["vault-sync", "prune-shortcuts", "--yes"]).unwrap();
        match cli.command {
            Command::PruneShortcuts { yes } => assert!(yes),
            other => panic!("comando inesperado: {other:?}"),
        }
    }

    #[test]
    fn set_server_name_parses_name() {
        let cli = Cli::try_parse_from(["vault-sync", "set-server-name", "Meu Servidor"]).unwrap();
        match cli.command {
            Command::SetServerName { name } => assert_eq!(name, "Meu Servidor"),
            other => panic!("comando inesperado: {other:?}"),
        }
    }

    #[test]
    fn set_server_name_requires_name() {
        let err = Cli::try_parse_from(["vault-sync", "set-server-name"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn set_client_name_parses_name() {
        let cli = Cli::try_parse_from(["vault-sync", "set-client-name", "Minha Máquina"]).unwrap();
        match cli.command {
            Command::SetClientName { name } => assert_eq!(name, "Minha Máquina"),
            other => panic!("comando inesperado: {other:?}"),
        }
    }

    #[test]
    fn set_client_name_requires_name() {
        let err = Cli::try_parse_from(["vault-sync", "set-client-name"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }
}
