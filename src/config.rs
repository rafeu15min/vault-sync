use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Mapeamento manual e explícito de onde a instalação completa de um jogo
/// está de verdade, mantido pelo usuário direto no `config.json` — a fonte
/// de verdade definitiva quando o metadado do launcher (ACF/shortcuts.vdf/
/// installed.json) está obsoleto ou indisponível. Nunca é preciso mexer em
/// código pra corrigir um caminho que mudou: só editar a entrada
/// correspondente aqui. Chaves: `app_id` numérico (string) pra `steam`/
/// `piratas`, título de pasta pra `epic` — mesmas chaves que os comandos
/// já usam (`run <id> --steam`, `run <título> --heroic`). Ainda passa por
/// `Path::is_dir()` antes de ser aceito — um override desatualizado também
/// não é confiado cegamente, só reportado como "não verificável" como
/// qualquer outra fonte (ver `install_paths.rs`).
#[derive(Debug, Clone, Deserialize, Serialize, Default, PartialEq, Eq)]
pub struct InstallPathOverrides {
    #[serde(default)]
    pub steam: HashMap<String, PathBuf>,
    #[serde(default)]
    pub piratas: HashMap<String, PathBuf>,
    #[serde(default)]
    pub epic: HashMap<String, PathBuf>,
}

/// Configuração persistida em JSON (ex.: `~/.config/vault-sync/config.json`).
/// Majoritariamente só-leitura — a única escrita é `Config::save`, usada
/// por `vault-sync set-proton` (ver `proton_overrides` abaixo) pra gravar a
/// escolha de Proton feita no segundo Rofi.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    pub remote_ssh_target: String,
    pub remote_base_path: PathBuf,
    pub local_libraries: Vec<PathBuf>,
    /// Diretório local que contém os prefixos do Heroic Games Launcher, um
    /// por título — na prática `~/Games/Heroic/Prefixes/default`. Campo
    /// opcional: configs Steam-only existentes continuam válidas sem ele, e
    /// `doctor`/`ingest`/`run --heroic`/`purge --heroic` simplesmente ficam
    /// indisponíveis (com erro claro) se ele não estiver presente.
    #[serde(default)]
    pub heroic_local_root: Option<PathBuf>,
    /// Espelho remoto de `heroic_local_root` no FeuVault — deliberadamente
    /// separado de `remote_base_path` (que hoje é especificamente a raiz da
    /// biblioteca Steam) para não misturar os dois namespaces nem arriscar
    /// as configs Steam já em produção.
    #[serde(default)]
    pub heroic_remote_base_path: Option<PathBuf>,

    /// Raiz local unificada onde `run` baixa/instala a **instalação
    /// completa** (não só o compatdata) de jogos Steam oficiais, uma vez
    /// migrados — ex.: `/home/rafeu/Games/steam`. Distinto de
    /// `local_libraries` (que continua servindo só de fonte de descoberta
    /// pro `ingest`/`scan` sobre o que já está instalado hoje). Só é
    /// utilizável de fato depois que o usuário registrar esse caminho como
    /// Library Folder no cliente Steam (passo manual, fora do escopo deste
    /// binário — ver Fase VII no README).
    #[serde(default)]
    pub local_game_root_steam: Option<PathBuf>,
    /// Idem, pra jogos Steam non-Steam (piratas) — sem a restrição de
    /// registro na Steam, já que o `StartDir` do atalho é livre.
    #[serde(default)]
    pub local_game_root_piratas: Option<PathBuf>,
    /// Idem, pra jogos Epic/Heroic — separado de `heroic_local_root`, que
    /// continua sendo só o prefixo Wine (`Prefixes/default`), nunca a
    /// instalação em si.
    #[serde(default)]
    pub local_game_root_epic: Option<PathBuf>,
    /// Raiz remota única, no FeuVault, pra instalações completas dos três
    /// tipos — namespaced por subpasta (`steam/`, `piratas/`, `epic/`) em
    /// vez de um campo por tipo, já que do lado remoto é só um mount, sem a
    /// razão física que justifica 3 raízes locais separadas.
    #[serde(default)]
    pub remote_installs_base_path: Option<PathBuf>,
    /// Overrides manuais de caminho de instalação — ver
    /// `InstallPathOverrides`. Vazio por padrão (nenhum override), nunca
    /// `None` — mapas vazios já significam "nada sobrescrito" sem precisar
    /// de mais um nível de `Option`.
    #[serde(default)]
    pub install_path_overrides: InstallPathOverrides,
    /// Nome de conta (`AccountName`, não o `PersonaName`) usado pelo
    /// `update` pra logar no `steamcmd` — precisa já ter uma sessão salva
    /// nesta máquina (cliente gráfico ou `steamcmd` já logado uma vez
    /// antes) pra autenticar sem prompt. Opcional: configs que nunca usam
    /// `update` continuam válidas sem ele.
    #[serde(default)]
    pub steam_account: Option<String>,
    /// App_ids Steam (oficiais ou non-Steam) que o `ingest` deve ignorar
    /// completamente — nunca tenta resolver, nunca reporta em
    /// `full_skipped`. Pensado pra jogos que o vault-sync estruturalmente
    /// não consegue gerenciar: caso real que motivou isso, um atalho
    /// non-Steam do Zenless Zone Zero (instalado via Lutris) tinha
    /// `StartDir="/home/rafeu"` — a guarda de segurança
    /// (`install_paths::is_safe_install_dir`) corretamente recusa resolver
    /// isso pra sempre, então o jogo nunca teria como sair da lista de
    /// "pulados" por conta própria; listar aqui é a forma explícita de
    /// dizer "não tente, eu sei". O app_id "0" (bookkeeping compartilhado
    /// da própria Steam, nunca um jogo de verdade) é sempre ignorado,
    /// independente desta lista.
    #[serde(default)]
    pub ingest_exclude_steam: std::collections::HashSet<String>,
    /// Versão de Proton escolhida (nome do diretório em
    /// `compatibilitytools.d`, ver `proton.rs`) pra cada atalho non-Steam,
    /// chaveado por `app_id` (mesma convenção de string usada por
    /// `install_path_overrides`). A PRIMEIRA escolha feita no segundo Rofi
    /// (`tools/vault-sync-menu.sh`) grava aqui e vira o padrão dali em
    /// diante; trocas subsequentes (Alt+P no menu) sobrescrevem livremente,
    /// sem validação além de "a versão ainda está instalada"
    /// (`proton::resolve`). Sem entrada aqui pra um `app_id`, `MenuLaunch`
    /// recusa lançar o atalho non-Steam com um erro acionável — nunca
    /// adivinha: deixar o `binfmt_misc` do sistema decidir sozinho qual
    /// Wine/prefixo usar foi exatamente o bug que motivou este campo (o
    /// prefixo usado na execução podia não ser o `compatdata` sincronizado
    /// com o FeuVault).
    #[serde(default)]
    pub proton_overrides: HashMap<String, String>,
    /// Nome de exibição do servidor remoto em mensagens voltadas ao
    /// usuário (zenity, stdout, `tracing`) — "FeuVault" na máquina do
    /// desenvolvedor, mas nunca hardcoded no código pra a ferramenta
    /// continuar universal em qualquer instalação. `None`/vazio cai pro
    /// genérico "Servidor" — ver `Config::server_name`. Editável via
    /// `vault-sync set-server-name <nome>`.
    #[serde(default)]
    pub server_name: Option<String>,
    /// Idem, pro nome de exibição da máquina local — simétrico a
    /// `server_name`, cai pro genérico "Cliente" — ver
    /// `Config::client_name`. Editável via `vault-sync set-client-name
    /// <nome>`.
    #[serde(default)]
    pub client_name: Option<String>,
}

impl Config {
    /// Nome de exibição do servidor remoto pras mensagens voltadas ao
    /// usuário — `server_name` configurado, ou "Servidor" se ninguém
    /// nomeou (uma string em branco também cai no padrão, não é tratada
    /// como "nomeado com nada").
    pub fn server_name(&self) -> &str {
        match self.server_name.as_deref() {
            Some(name) if !name.trim().is_empty() => name,
            _ => "Servidor",
        }
    }

    /// Idem, pro nome de exibição da máquina local — `client_name`
    /// configurado, ou "Cliente" se ninguém nomeou.
    pub fn client_name(&self) -> &str {
        match self.client_name.as_deref() {
            Some(name) if !name.trim().is_empty() => name,
            _ => "Cliente",
        }
    }

    /// Caminho efetivo do config.json: `explicit_path` se informado, senão
    /// o padrão XDG. Extraído de `load` pra ser reaproveitado por
    /// `vault-sync set-proton` (main.rs), que precisa gravar de volta no
    /// mesmo arquivo que acabou de ler.
    pub fn resolve_path(explicit_path: Option<&Path>) -> Option<PathBuf> {
        match explicit_path {
            Some(p) => Some(p.to_path_buf()),
            None => dirs::config_dir().map(|d| d.join("vault-sync").join("config.json")),
        }
    }

    /// Carrega a config do caminho informado, ou do padrão XDG se `None`.
    /// Retorna `Ok(None)` se o arquivo simplesmente não existir ainda —
    /// isso não é um erro nesta fase, já que run/purge/ingest ainda não
    /// executam de fato (Fases III/V).
    pub fn load(explicit_path: Option<&Path>) -> anyhow::Result<Option<Self>> {
        let Some(path) = Self::resolve_path(explicit_path) else {
            return Ok(None);
        };

        if !path.is_file() {
            return Ok(None);
        }

        let raw = std::fs::read_to_string(&path)?;
        let cfg: Config = serde_json::from_str(&raw)?;
        Ok(Some(cfg))
    }

    /// Grava a config de volta em `path`, atomicamente (mesmo padrão de
    /// `backup::atomic_backup`: escreve num `.tmp` no mesmo diretório e só
    /// promove via `rename`) e com backup pregresso do conteúdo anterior —
    /// mesma garantia que `inject.rs`/`purge.rs` já dão pra `shortcuts.vdf`
    /// antes de qualquer mutação. Único ponto de escrita de `config.json`
    /// hoje: `vault-sync set-proton` (ver `Command::SetProton`, main.rs).
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if path.is_file() {
            crate::backup::atomic_backup(path)?;
        }
        let json = serde_json::to_string_pretty(self)?;
        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "config.json".to_string());
        let tmp_path = path.with_file_name(format!("{file_name}.tmp"));
        std::fs::write(&tmp_path, json)?;
        std::fs::rename(&tmp_path, path)?;
        Ok(())
    }
}

#[cfg(test)]
impl Config {
    /// Config mínima de teste: `remote_ssh_target`/`remote_base_path` fixos,
    /// `local_libraries` vazio, todo o resto em seu padrão "nada configurado".
    /// Construtores de `Config` de teste em outros módulos devem usar
    /// `..Config::test_default()` em vez de repetir os campos — sem isso,
    /// todo campo novo (ex.: `steam_account`) quebra a compilação de teste
    /// em cada um dos sites que ainda usa um literal completo (já aconteceu:
    /// 6 arquivos precisaram de correção manual na mesma sessão que
    /// adicionou `steam_account`).
    pub(crate) fn test_default() -> Self {
        Config {
            remote_ssh_target: "feuvault-teste".to_string(),
            remote_base_path: PathBuf::from("/fake/remote/base"),
            local_libraries: vec![],
            heroic_local_root: None,
            heroic_remote_base_path: None,
            local_game_root_steam: None,
            local_game_root_piratas: None,
            local_game_root_epic: None,
            remote_installs_base_path: None,
            install_path_overrides: InstallPathOverrides::default(),
            steam_account: None,
            ingest_exclude_steam: std::collections::HashSet::new(),
            proton_overrides: HashMap::new(),
            server_name: None,
            client_name: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_name_defaults_to_generic_servidor_when_unset() {
        let cfg = Config::test_default();
        assert_eq!(cfg.server_name(), "Servidor");
    }

    #[test]
    fn server_name_uses_configured_value() {
        let cfg = Config {
            server_name: Some("FeuVault".to_string()),
            ..Config::test_default()
        };
        assert_eq!(cfg.server_name(), "FeuVault");
    }

    #[test]
    fn server_name_falls_back_to_default_when_configured_as_blank() {
        // Uma string em branco não conta como "nomeado" — evita um
        // config.json com `"server_name": ""` (fácil de acontecer editando
        // à mão) silenciosamente virando um nome vazio nas mensagens.
        let cfg = Config {
            server_name: Some("   ".to_string()),
            ..Config::test_default()
        };
        assert_eq!(cfg.server_name(), "Servidor");
    }

    #[test]
    fn client_name_defaults_to_generic_cliente_when_unset() {
        let cfg = Config::test_default();
        assert_eq!(cfg.client_name(), "Cliente");
    }

    #[test]
    fn client_name_uses_configured_value() {
        let cfg = Config {
            client_name: Some("Desktop do Rafeu".to_string()),
            ..Config::test_default()
        };
        assert_eq!(cfg.client_name(), "Desktop do Rafeu");
    }

    #[test]
    fn load_returns_none_for_missing_file() {
        let result = Config::load(Some(Path::new("/tmp/vault-sync-inexistente.json"))).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn load_parses_valid_json() {
        let dir = std::env::temp_dir().join(format!("vault-sync-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        std::fs::write(
            &path,
            r#"{
                "remote_ssh_target": "feuvault",
                "remote_base_path": "/mnt/vault/steam-library",
                "local_libraries": ["/mnt/SSD2/SteamLibrary"]
            }"#,
        )
        .unwrap();

        let cfg = Config::load(Some(&path)).unwrap().unwrap();
        assert_eq!(cfg.remote_ssh_target, "feuvault");
        assert_eq!(cfg.local_libraries.len(), 1);
        assert_eq!(cfg.install_path_overrides, InstallPathOverrides::default());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_parses_install_path_overrides() {
        let dir =
            std::env::temp_dir().join(format!("vault-sync-test-overrides-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        std::fs::write(
            &path,
            r#"{
                "remote_ssh_target": "feuvault",
                "remote_base_path": "/mnt/vault/steam-library",
                "local_libraries": ["/mnt/SSD2/SteamLibrary"],
                "install_path_overrides": {
                    "steam": {"1449850": "/home/rafeu/Games/steam/YuGiOh"},
                    "epic": {"Disco Elysium - The Final Cut": "/mnt/HDD1/EPIC/DiscoElysium"}
                }
            }"#,
        )
        .unwrap();

        let cfg = Config::load(Some(&path)).unwrap().unwrap();
        assert_eq!(
            cfg.install_path_overrides.steam.get("1449850"),
            Some(&PathBuf::from("/home/rafeu/Games/steam/YuGiOh"))
        );
        assert_eq!(
            cfg.install_path_overrides
                .epic
                .get("Disco Elysium - The Final Cut"),
            Some(&PathBuf::from("/mnt/HDD1/EPIC/DiscoElysium"))
        );
        assert!(cfg.install_path_overrides.piratas.is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn save_then_load_round_trips_proton_overrides() {
        let dir = std::env::temp_dir().join(format!(
            "vault-sync-test-save-proton-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");

        let mut cfg = Config::test_default();
        cfg.proton_overrides
            .insert("2892464705".to_string(), "GE-Proton10-34".to_string());
        cfg.save(&path).unwrap();

        let reloaded = Config::load(Some(&path)).unwrap().unwrap();
        assert_eq!(
            reloaded.proton_overrides.get("2892464705"),
            Some(&"GE-Proton10-34".to_string())
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn save_leaves_no_temp_file_behind() {
        let dir = std::env::temp_dir().join(format!(
            "vault-sync-test-save-no-tmp-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");

        Config::test_default().save(&path).unwrap();

        let leftover_tmp = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .any(|e| e.file_name().to_string_lossy().ends_with(".tmp"));
        assert!(
            !leftover_tmp,
            "arquivo temporário de save não foi promovido"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn save_backs_up_previous_content_before_overwriting() {
        let dir = std::env::temp_dir().join(format!(
            "vault-sync-test-save-backup-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        Config::test_default().save(&path).unwrap();

        let mut cfg = Config::test_default();
        cfg.proton_overrides
            .insert("1".to_string(), "GE-Proton10-34".to_string());
        cfg.save(&path).unwrap();

        let has_backup = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .any(|e| e.file_name().to_string_lossy().contains(".vault-sync-bak-"));
        assert!(has_backup, "esperava um backup do config.json anterior");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resolve_path_prefers_explicit_path_over_xdg_default() {
        let explicit = Path::new("/tmp/algum-config-explicito.json");
        assert_eq!(
            Config::resolve_path(Some(explicit)),
            Some(explicit.to_path_buf())
        );
    }
}
