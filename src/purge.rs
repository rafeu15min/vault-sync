//! Remoção remota irrevogável (5.3): obliteração estrita e coordenada dos
//! dados de um jogo no FeuVault, seguida da limpeza de qualquer resquício
//! local — compatdata/manifest ainda presentes no SSD e, crucialmente, as
//! entradas correspondentes em `shortcuts.vdf` (sem isso, jogos non-Steam
//! removidos do FeuVault continuariam aparecendo normalmente na UI da
//! Steam, "evidência remanescente" que a spec pede para eliminar).

use crate::backup::atomic_backup;
use crate::config::Config;
use crate::domain::{GameSource, Launcher, LibraryLocation};
use crate::install_paths;
use crate::paths::{PathSafetyError, RemoteVault};
use crate::remote::{self, RemoteError};
use crate::scan;
use crate::vdf::{self, VdfError, VdfValue};
use std::path::{Path, PathBuf};

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PurgeSummary {
    pub remote_removed: bool,
    pub local_compatdata_removed: bool,
    pub local_manifest_removed: bool,
    pub shortcut_entries_removed: usize,
    /// Fase VII, melhor esforço: só `true` quando a instalação completa
    /// pôde ser resolvida (manifesto/`StartDir`/raiz Epic configurada) —
    /// `false` não é erro, só significa que não havia nada rastreado pra
    /// remover além do compatdata já tratado acima.
    pub full_install_removed_local: bool,
    pub full_install_removed_remote: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum PurgeError {
    #[error(transparent)]
    PathSafety(#[from] PathSafetyError),
    #[error("falha ao remover dados remotos no servidor: {0}")]
    Remote(#[from] RemoteError),
    #[error("falha de E/S ao limpar armazenamento local: {0}")]
    Io(#[source] std::io::Error),
    #[error(
        "'--heroic' exige `heroic_local_root` e `heroic_remote_base_path` configurados em config.json"
    )]
    HeroicNotConfigured,
}

/// Remove definitivamente `app_id` do FeuVault e de qualquer registro
/// local correspondente. `steam_root`, quando fornecido, também limpa as
/// entradas de `shortcuts.vdf` — omitido nos testes/chamadores que não
/// precisam tocar em Steam de verdade. Ignorado quando `launcher` é
/// `Heroic`, já que jogos Heroic nunca são registrados em `shortcuts.vdf`.
pub fn run_purge(
    cfg: &Config,
    launcher: Launcher,
    app_id: &str,
    steam_root: Option<&Path>,
) -> Result<PurgeSummary, PurgeError> {
    run_purge_with_options(
        cfg,
        launcher,
        app_id,
        steam_root,
        "ssh",
        install_paths::default_legendary_installed_json_path().as_deref(),
    )
}

fn run_purge_with_options(
    cfg: &Config,
    launcher: Launcher,
    app_id: &str,
    steam_root: Option<&Path>,
    ssh_bin: &str,
    legendary_installed_json_path: Option<&Path>,
) -> Result<PurgeSummary, PurgeError> {
    // GameSource usado só como veículo de caminho (mesmo truque de
    // scan.rs/ingest.rs) — purge não classifica a origem, só precisa dos
    // mesmos caminhos de compatdata/manifest que o resto do código já usa.
    // `validated_app_id`/`validated_heroic_title` (via `paths_for`) rejeitam
    // qualquer coisa insegura antes de qualquer operação destrutiva rodar.
    let (source, local_roots, remote_base_path) = match launcher {
        Launcher::Steam => {
            let source = GameSource::Orphan {
                app_id: app_id.to_string(),
                clues: vec![],
            };
            (
                source,
                cfg.local_libraries.clone(),
                cfg.remote_base_path.clone(),
            )
        }
        Launcher::Heroic => {
            let source = GameSource::HeroicGame {
                title: app_id.to_string(),
            };
            let local_root = cfg
                .heroic_local_root
                .clone()
                .ok_or(PurgeError::HeroicNotConfigured)?;
            let remote_base_path = cfg
                .heroic_remote_base_path
                .clone()
                .ok_or(PurgeError::HeroicNotConfigured)?;
            (source, vec![local_root], remote_base_path)
        }
    };

    let vault = RemoteVault {
        ssh_target: cfg.remote_ssh_target.clone(),
        base_path: remote_base_path,
    };
    let remote_paths = vault.paths_for(&source)?;

    let mut summary = PurgeSummary::default();

    remote::remove_remote_path(ssh_bin, &vault.ssh_target, &remote_paths.compatdata_dir)?;
    summary.remote_removed = true;
    if let Some(manifest) = &remote_paths.manifest_file {
        remote::remove_remote_path(ssh_bin, &vault.ssh_target, manifest)?;
    }

    for local_lib in &local_roots {
        let library = LibraryLocation::new(local_lib.clone());
        let targets = library.deletion_targets(&source)?;
        if targets.compatdata_dir.exists() {
            std::fs::remove_dir_all(&targets.compatdata_dir).map_err(PurgeError::Io)?;
            summary.local_compatdata_removed = true;
        }
        if let Some(manifest) = &targets.manifest_file {
            if manifest.exists() {
                std::fs::remove_file(manifest).map_err(PurgeError::Io)?;
                summary.local_manifest_removed = true;
            }
        }
    }

    // Fase VII, melhor esforço: instalação completa (Steam oficial via
    // manifesto, pirata via `scan::resolve_piratas_install_dir`, Epic via
    // raiz configurada — nunca via `installed.json` do Legendary, que já
    // provou ficar obsoleto sozinho; o caminho canônico pra jogos
    // rastreados é sempre `local_game_root_epic/<título>` ou
    // `local_game_root_piratas/<nome>`, ver `session.rs`). Não resolver não
    // é erro — só significa que não há nada de instalação completa pra
    // remover além do compatdata já tratado acima.
    if let Some(remote_installs_base) = &cfg.remote_installs_base_path {
        let full_install: Option<(&'static str, PathBuf, String)> = match launcher {
            Launcher::Steam => {
                let official = cfg.local_game_root_steam.as_ref().and_then(|root| {
                    install_paths::manifest_installdir(root, app_id).map(|name| {
                        let local_dir = root.join("steamapps").join("common").join(&name);
                        ("steam", local_dir, name)
                    })
                });
                official.or_else(|| {
                    // Mesma prioridade do `session.rs` — ver
                    // `scan::resolve_piratas_install_dir`.
                    let numeric_app_id = app_id.trim().parse::<u32>().ok()?;
                    let local_dir = scan::resolve_piratas_install_dir(
                        cfg.install_path_overrides
                            .piratas
                            .get(app_id)
                            .map(PathBuf::as_path),
                        cfg.local_game_root_piratas.as_deref(),
                        steam_root,
                        numeric_app_id,
                    )?;
                    let name = local_dir.file_name()?.to_string_lossy().into_owned();
                    Some(("piratas", local_dir, name))
                })
            }
            Launcher::Heroic => cfg
                .local_game_root_epic
                .as_ref()
                .map(|root| ("epic", root.join(app_id), app_id.to_string())),
        };

        if let Some((category, local_dir, name)) = full_install {
            // Guarda central contra caminho raso demais/`$HOME` vindo de
            // metadado de launcher externo (StartDir do Lutris, override
            // stale...) — mesma checagem de `install_paths::verified_candidate`.
            // Incidente real: StartDir="/home/rafeu" quase apagou a home
            // inteira num `remove_dir_all` sem essa guarda.
            if local_dir.exists() {
                if install_paths::is_safe_install_dir(&local_dir, dirs::home_dir().as_deref()) {
                    std::fs::remove_dir_all(&local_dir).map_err(PurgeError::Io)?;
                    summary.full_install_removed_local = true;
                    // Sem isto, o `installed.json` do Legendary continua
                    // afirmando que `app_id` (o título, pra Heroic) está
                    // instalado na pasta que acabamos de apagar — mesmo gap
                    // que existia em `session.rs::upload_and_cleanup` antes
                    // de ser corrigido (ver doc ali).
                    if launcher == Launcher::Heroic {
                        if let Some(installed_json) = legendary_installed_json_path {
                            crate::session::clean_stale_legendary_entry_at(app_id, installed_json);
                        }
                    }
                } else {
                    tracing::warn!(
                        ?local_dir,
                        "recusando remove_dir_all: caminho raso demais ou dentro de $HOME"
                    );
                }
            }
            let remote_dir = remote_installs_base.join(category).join(&name);
            remote::remove_remote_path(ssh_bin, &vault.ssh_target, &remote_dir)?;
            summary.full_install_removed_remote = true;
        }
    }

    // Heroic nunca é registrado em shortcuts.vdf — só a Steam precisa dessa
    // limpeza de "evidência remanescente".
    if launcher == Launcher::Steam {
        if let (Some(steam_root), Ok(numeric_app_id)) = (steam_root, app_id.trim().parse::<u32>()) {
            for (path, result) in remove_shortcut_entries_everywhere(steam_root, numeric_app_id) {
                match result {
                    Ok(removed) => summary.shortcut_entries_removed += removed,
                    Err(e) => {
                        tracing::warn!(?path, "falha ao limpar entrada de shortcuts.vdf: {e}");
                    }
                }
            }
        }
    }

    Ok(summary)
}

#[derive(Debug, thiserror::Error)]
pub enum ShortcutCleanupError {
    #[error("falha de E/S ao processar '{0}': {1}")]
    Io(PathBuf, #[source] std::io::Error),
    #[error("falha ao decodificar VDF binário '{0}': {1}")]
    Vdf(PathBuf, #[source] VdfError),
}

/// Remove, de todos os `shortcuts.vdf` locais (um por usuário Steam), a
/// entrada cujo `appid` bate com `app_id` — preserva o resto da árvore
/// intacto, igual ao motor de injeção (4.3), e só reescreve o arquivo (com
/// backup atômico prévio) se algo de fato foi removido. `pub(crate)` porque
/// `Command::PruneShortcuts` (main.rs) reaproveita este mesmo motor pra
/// remover atalhos obsoletos detectados por `scan::find_stale_shortcuts` —
/// mesma garantia de segurança, só muda de onde o `app_id` alvo vem.
pub(crate) fn remove_shortcut_entries_everywhere(
    steam_root: &Path,
    app_id: u32,
) -> Vec<(PathBuf, Result<usize, ShortcutCleanupError>)> {
    let userdata = steam_root.join("userdata");
    let Ok(entries) = std::fs::read_dir(userdata) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|e| e.path().join("config").join("shortcuts.vdf"))
        .filter(|p| p.is_file())
        .map(|p| {
            let result = remove_shortcut_entries_in_file(&p, app_id);
            (p, result)
        })
        .collect()
}

fn remove_shortcut_entries_in_file(
    path: &Path,
    app_id: u32,
) -> Result<usize, ShortcutCleanupError> {
    let raw = std::fs::read(path).map_err(|e| ShortcutCleanupError::Io(path.to_path_buf(), e))?;
    let mut root =
        vdf::parse_binary(&raw).map_err(|e| ShortcutCleanupError::Vdf(path.to_path_buf(), e))?;

    let mut removed = 0usize;
    if let Some((_, VdfValue::Object(shortcuts))) = root
        .iter_mut()
        .find(|(k, _)| k.eq_ignore_ascii_case("shortcuts"))
    {
        let before = shortcuts.len();
        shortcuts.retain(|(_, entry)| {
            let VdfValue::Object(fields) = entry else {
                return true;
            };
            let entry_app_id = fields
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("appid"))
                .and_then(|(_, v)| match v {
                    VdfValue::Int(i) => Some(*i as u32),
                    _ => None,
                });
            entry_app_id != Some(app_id)
        });
        removed = before - shortcuts.len();
    }

    if removed > 0 {
        atomic_backup(path).map_err(|e| ShortcutCleanupError::Io(path.to_path_buf(), e))?;
        std::fs::write(path, vdf::write_binary(&root))
            .map_err(|e| ShortcutCleanupError::Io(path.to_path_buf(), e))?;
    }

    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vdf::VdfObject;
    use std::fs::{self, File};
    use std::io::Write;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    fn test_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "vault-sync-purge-test-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn fake_ssh(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join("fake-ssh.sh");
        let mut f = File::create(&path).unwrap();
        writeln!(f, "#!/bin/sh\n{body}").unwrap();
        #[cfg(unix)]
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn cfg_with_lib(local_lib: &Path) -> Config {
        Config {
            local_libraries: vec![local_lib.to_path_buf()],
            ..Config::test_default()
        }
    }

    fn shortcut_tree(app_id: u32, name: &str) -> (String, VdfValue) {
        (
            app_id.to_string(),
            VdfValue::Object(vec![
                ("appid".to_string(), VdfValue::Int(app_id as i32)),
                ("appname".to_string(), VdfValue::Str(name.to_string())),
            ]),
        )
    }

    #[test]
    fn removes_remote_and_local_compatdata() {
        let dir = test_dir("remote-and-local");
        let local_lib = dir.join("ssd");
        let compatdata = local_lib.join("steamapps").join("compatdata").join("42");
        fs::create_dir_all(&compatdata).unwrap();

        let ssh = fake_ssh(&dir, "exit 0");

        let summary = run_purge_with_options(
            &cfg_with_lib(&local_lib),
            Launcher::Steam,
            "42",
            None,
            ssh.to_str().unwrap(),
            None,
        )
        .unwrap();

        assert!(summary.remote_removed);
        assert!(summary.local_compatdata_removed);
        assert!(!compatdata.exists());
    }

    #[test]
    fn rejects_non_numeric_app_id_before_touching_anything() {
        let dir = test_dir("bad-appid");
        let local_lib = dir.join("ssd");
        fs::create_dir_all(&local_lib).unwrap();
        let ssh = fake_ssh(&dir, "exit 0");

        let result = run_purge_with_options(
            &cfg_with_lib(&local_lib),
            Launcher::Steam,
            "../escape",
            None,
            ssh.to_str().unwrap(),
            None,
        );

        assert!(matches!(result, Err(PurgeError::PathSafety(_))));
    }

    #[test]
    fn propagates_remote_removal_failure() {
        let dir = test_dir("remote-fail");
        let local_lib = dir.join("ssd");
        fs::create_dir_all(&local_lib).unwrap();
        let ssh = fake_ssh(&dir, "exit 1");

        let result = run_purge_with_options(
            &cfg_with_lib(&local_lib),
            Launcher::Steam,
            "42",
            None,
            ssh.to_str().unwrap(),
            None,
        );

        assert!(matches!(result, Err(PurgeError::Remote(_))));
    }

    #[test]
    fn removes_matching_shortcut_entry_and_preserves_others() {
        let dir = test_dir("shortcut-removal");
        let local_lib = dir.join("ssd");
        fs::create_dir_all(&local_lib).unwrap();
        let ssh = fake_ssh(&dir, "exit 0");

        let steam_root = dir.join("steam");
        let config_dir = steam_root.join("userdata").join("1").join("config");
        fs::create_dir_all(&config_dir).unwrap();
        let shortcuts_path = config_dir.join("shortcuts.vdf");
        let tree: VdfObject = vec![(
            "shortcuts".to_string(),
            VdfValue::Object(vec![
                shortcut_tree(1234567890, "Jogo A"),
                shortcut_tree(999, "Jogo B"),
            ]),
        )];
        fs::write(&shortcuts_path, vdf::write_binary(&tree)).unwrap();

        let summary = run_purge_with_options(
            &cfg_with_lib(&local_lib),
            Launcher::Steam,
            "1234567890",
            Some(&steam_root),
            ssh.to_str().unwrap(),
            None,
        )
        .unwrap();

        assert_eq!(summary.shortcut_entries_removed, 1);

        let rewritten = vdf::parse_binary(&fs::read(&shortcuts_path).unwrap()).unwrap();
        let VdfValue::Object(shortcuts) = &rewritten[0].1 else {
            panic!("esperava objeto shortcuts")
        };
        assert_eq!(shortcuts.len(), 1);
        assert_eq!(shortcuts[0].0, "999");

        let has_backup = fs::read_dir(&config_dir)
            .unwrap()
            .flatten()
            .any(|e| e.file_name().to_string_lossy().contains("vault-sync-bak"));
        assert!(has_backup, "esperava backup atômico antes da reescrita");
    }

    #[test]
    fn leaves_shortcuts_untouched_when_app_id_does_not_match() {
        let dir = test_dir("shortcut-no-match");
        let local_lib = dir.join("ssd");
        fs::create_dir_all(&local_lib).unwrap();
        let ssh = fake_ssh(&dir, "exit 0");

        let steam_root = dir.join("steam");
        let config_dir = steam_root.join("userdata").join("1").join("config");
        fs::create_dir_all(&config_dir).unwrap();
        let shortcuts_path = config_dir.join("shortcuts.vdf");
        let tree: VdfObject = vec![(
            "shortcuts".to_string(),
            VdfValue::Object(vec![shortcut_tree(999, "Jogo B")]),
        )];
        fs::write(&shortcuts_path, vdf::write_binary(&tree)).unwrap();

        let summary = run_purge_with_options(
            &cfg_with_lib(&local_lib),
            Launcher::Steam,
            "1234567890",
            Some(&steam_root),
            ssh.to_str().unwrap(),
            None,
        )
        .unwrap();

        assert_eq!(summary.shortcut_entries_removed, 0);
        let has_backup = fs::read_dir(&config_dir)
            .unwrap()
            .flatten()
            .any(|e| e.file_name().to_string_lossy().contains("vault-sync-bak"));
        assert!(!has_backup, "não deveria gerar backup sem remoção alguma");
    }

    fn cfg_with_full_steam(local_lib: &Path, remote_installs: &Path) -> Config {
        Config {
            local_game_root_steam: Some(local_lib.to_path_buf()),
            remote_installs_base_path: Some(remote_installs.to_path_buf()),
            ..cfg_with_lib(local_lib)
        }
    }

    #[test]
    fn full_install_official_removed_both_sides_when_manifest_resolves() {
        let dir = test_dir("full-official");
        let local_lib = dir.join("ssd");
        let install_dir = local_lib.join("steamapps").join("common").join("Meu Jogo");
        fs::create_dir_all(&install_dir).unwrap();
        fs::write(
            local_lib.join("steamapps").join("appmanifest_42.acf"),
            "\"installdir\"\t\t\"Meu Jogo\"\n",
        )
        .unwrap();

        let ssh = fake_ssh(&dir, "exit 0");
        let remote_installs = dir.join("remote-installs");

        let summary = run_purge_with_options(
            &cfg_with_full_steam(&local_lib, &remote_installs),
            Launcher::Steam,
            "42",
            None,
            ssh.to_str().unwrap(),
            None,
        )
        .unwrap();

        assert!(summary.full_install_removed_local);
        assert!(summary.full_install_removed_remote);
        assert!(!install_dir.exists());
    }

    #[test]
    fn full_install_epic_always_attempted_when_configured_even_if_never_present() {
        let dir = test_dir("full-epic");
        let local_lib = dir.join("ssd");
        fs::create_dir_all(&local_lib).unwrap();
        let heroic_root = dir.join("heroic-prefixes");
        fs::create_dir_all(&heroic_root).unwrap();
        let epic_root = dir.join("epic-installs");
        // O jogo nunca chegou a ser adotado (nada em epic_root/<título>) —
        // deve ser um no-op seguro, não erro.

        let ssh = fake_ssh(&dir, "exit 0");
        let remote_installs = dir.join("remote-installs");
        let cfg = Config {
            heroic_local_root: Some(heroic_root),
            heroic_remote_base_path: Some(PathBuf::from("/fake/remote/heroic")),
            local_game_root_epic: Some(epic_root),
            remote_installs_base_path: Some(remote_installs),
            ..cfg_with_lib(&local_lib)
        };

        let summary = run_purge_with_options(
            &cfg,
            Launcher::Heroic,
            "Jogo Nunca Adotado",
            None,
            ssh.to_str().unwrap(),
            None,
        )
        .unwrap();

        assert!(!summary.full_install_removed_local);
        assert!(
            summary.full_install_removed_remote,
            "remoto é sempre tentado (idempotente via rm -rf) quando local_game_root_epic está configurado"
        );
    }

    #[test]
    fn full_install_untouched_when_remote_installs_base_path_not_configured() {
        let dir = test_dir("full-not-configured");
        let local_lib = dir.join("ssd");
        fs::create_dir_all(local_lib.join("steamapps").join("compatdata").join("42")).unwrap();
        let ssh = fake_ssh(&dir, "exit 0");

        let summary = run_purge_with_options(
            &cfg_with_lib(&local_lib),
            Launcher::Steam,
            "42",
            None,
            ssh.to_str().unwrap(),
            None,
        )
        .unwrap();

        assert!(!summary.full_install_removed_local);
        assert!(!summary.full_install_removed_remote);
    }

    #[test]
    fn full_install_pirated_resolves_via_shortcut_start_dir() {
        let dir = test_dir("full-piratas");
        let local_lib = dir.join("ssd");
        fs::create_dir_all(&local_lib).unwrap();

        let steam_root = dir.join("steam");
        let config_dir = steam_root.join("userdata").join("1").join("config");
        fs::create_dir_all(&config_dir).unwrap();
        let install_dir = dir.join("Piratas").join("Jogo Pirata");
        fs::create_dir_all(&install_dir).unwrap();
        let tree: VdfObject = vec![(
            "shortcuts".to_string(),
            VdfValue::Object(vec![(
                "0".to_string(),
                VdfValue::Object(vec![
                    ("appid".to_string(), VdfValue::Int(1234567890u32 as i32)),
                    (
                        "appname".to_string(),
                        VdfValue::Str("Jogo Pirata".to_string()),
                    ),
                    (
                        "StartDir".to_string(),
                        VdfValue::Str(format!("\"{}\"", install_dir.display())),
                    ),
                ]),
            )]),
        )];
        fs::write(config_dir.join("shortcuts.vdf"), vdf::write_binary(&tree)).unwrap();

        let ssh = fake_ssh(&dir, "exit 0");
        let remote_installs = dir.join("remote-installs");
        let cfg = Config {
            remote_installs_base_path: Some(remote_installs),
            ..cfg_with_lib(&local_lib)
        };

        let summary = run_purge_with_options(
            &cfg,
            Launcher::Steam,
            "1234567890",
            Some(&steam_root),
            ssh.to_str().unwrap(),
            None,
        )
        .unwrap();

        assert!(summary.full_install_removed_local);
        assert!(summary.full_install_removed_remote);
        assert!(!install_dir.exists());
    }

    #[test]
    fn full_install_pirated_config_override_wins_over_stale_shortcut() {
        let dir = test_dir("full-piratas-override");
        let local_lib = dir.join("ssd");
        fs::create_dir_all(&local_lib).unwrap();

        let steam_root = dir.join("steam");
        let config_dir = steam_root.join("userdata").join("1").join("config");
        fs::create_dir_all(&config_dir).unwrap();
        // StartDir aponta pra um caminho morto — o override deve ganhar.
        let tree: VdfObject = vec![(
            "shortcuts".to_string(),
            VdfValue::Object(vec![(
                "0".to_string(),
                VdfValue::Object(vec![
                    ("appid".to_string(), VdfValue::Int(1234567890u32 as i32)),
                    (
                        "appname".to_string(),
                        VdfValue::Str("Jogo Pirata".to_string()),
                    ),
                    (
                        "StartDir".to_string(),
                        VdfValue::Str("\"/run/media/rafeu/SSD/Piratas/Jogo\"".to_string()),
                    ),
                ]),
            )]),
        )];
        fs::write(config_dir.join("shortcuts.vdf"), vdf::write_binary(&tree)).unwrap();

        let real_install_dir = dir.join("Piratas").join("Jogo Real");
        fs::create_dir_all(&real_install_dir).unwrap();

        let ssh = fake_ssh(&dir, "exit 0");
        let remote_installs = dir.join("remote-installs");
        let mut cfg = Config {
            remote_installs_base_path: Some(remote_installs),
            ..cfg_with_lib(&local_lib)
        };
        cfg.install_path_overrides
            .piratas
            .insert("1234567890".to_string(), real_install_dir.clone());

        let summary = run_purge_with_options(
            &cfg,
            Launcher::Steam,
            "1234567890",
            Some(&steam_root),
            ssh.to_str().unwrap(),
            None,
        )
        .unwrap();

        assert!(summary.full_install_removed_local);
        assert!(summary.full_install_removed_remote);
        assert!(!real_install_dir.exists());
    }
}
