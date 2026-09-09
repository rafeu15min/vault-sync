//! Diagnóstico pré-voo (`vault-sync doctor`): lê a config e confere, sem
//! mutar nada, se os caminhos de cada máquina — bibliotecas locais no
//! Rafeu-PC e `remote_base_path` no FeuVault — e a conectividade SSH estão
//! prontos antes de `run`/`ingest`/`purge` tentarem uma operação real (a
//! primeira das quais pode ser destrutiva ou, no caso de `purge`,
//! irrevogável). Responde diretamente à lacuna que o projeto tinha até
//! aqui: nenhum comando validava esses pré-requisitos antes de agir.

use crate::config::Config;
use crate::paths::is_mount_point;
use crate::remote::{self, RemotePathStatus};
use crate::scan::{self, StaleShortcut};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryCheck {
    pub path: PathBuf,
    pub exists: bool,
    pub is_mount_point: bool,
}

impl LibraryCheck {
    fn for_path(path: &Path) -> Self {
        Self {
            path: path.to_path_buf(),
            exists: path.is_dir(),
            is_mount_point: is_mount_point(path),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteCheck {
    pub ssh_target: String,
    pub reachable: bool,
    pub base_path: PathBuf,
    /// `None` só quando `reachable` é falso — não faz sentido consultar um
    /// caminho num alvo que não respondeu.
    pub base_path_status: Option<RemotePathStatus>,
}

/// Checagem de `--heroic`, só presente quando `heroic_local_root` e
/// `heroic_remote_base_path` estão ambos configurados — igual à regra que
/// `session.rs`/`purge.rs` já aplicam (`HeroicNotConfigured`), só que
/// verificada aqui antes de qualquer tentativa real de `run`/`purge`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeroicCheck {
    pub local_root: LibraryCheck,
    pub remote_base_path: PathBuf,
    /// `None` só quando o FeuVault está inalcançável — nesse caso o
    /// `RemoteCheck` principal já reporta a falha dura.
    pub remote_base_path_status: Option<RemotePathStatus>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoctorReport {
    pub libraries: Vec<LibraryCheck>,
    pub remote: RemoteCheck,
    /// `None` quando o usuário não configurou suporte a Heroic — nesse caso
    /// não é reportado como falha, só omitido.
    pub heroic: Option<HeroicCheck>,
    /// Atalhos non-Steam com `Exe` morto que compartilham nome com um
    /// atalho vivo — resíduo de remanejo de disco (ver `scan.rs`). Nunca
    /// uma falha dura (`has_hard_failures` deliberadamente não olha pra
    /// isso, mesmo princípio de ponto de montagem): é informativo, e a
    /// remoção é sempre um passo separado e explícito, nunca automático
    /// aqui.
    pub stale_shortcuts: Vec<StaleShortcut>,
    /// App_ids com override de pirata configurado (`install_path_overrides.
    /// piratas`) mas sem nenhum campo `Exe` correspondente em nenhum
    /// `shortcuts.vdf` local — normalmente porque o arquivo inteiro sumiu
    /// ou foi resetado pela própria Steam (incidente real, 2026-09-09:
    /// Phonopolis parou de lançar assim, `menu-launch` falhava sem
    /// nenhuma pista visível). Nunca uma falha dura — só um aviso; `doctor`
    /// nunca escreve em `shortcuts.vdf`.
    pub missing_pirata_shortcuts: Vec<MissingPirataShortcut>,
    /// `true` quando existe pelo menos um backup automático do vault-sync
    /// (`shortcuts.vdf.vault-sync-bak*`, criado por `atomic_backup` antes
    /// de qualquer escrita) em algum perfil local — só relevante (e só
    /// computado) quando `missing_pirata_shortcuts` não está vazio, pra
    /// dizer ao usuário que uma recuperação sem digitar tudo de novo é
    /// possível.
    pub shortcuts_vdf_backup_available: bool,
}

/// Um pirata rastreado em `config.json` que `doctor` não conseguiu achar
/// no `shortcuts.vdf` ao vivo desta máquina.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingPirataShortcut {
    pub app_id: String,
    pub override_path: PathBuf,
}

impl DoctorReport {
    /// Falhas que de fato impedem `run`/`ingest`/`purge` de funcionar.
    /// Deliberadamente não inclui `is_mount_point == false`: um SSD que não
    /// é um ponto de montagem separado pode ser uma configuração legítima
    /// (disco único), então isso é reportado como aviso, não como falha —
    /// ver `paths::is_mount_point`.
    pub fn has_hard_failures(&self) -> bool {
        if !self.remote.reachable {
            return true;
        }
        if !matches!(
            self.remote.base_path_status,
            Some(RemotePathStatus::Directory | RemotePathStatus::MountPoint)
        ) {
            return true;
        }
        if self.libraries.iter().any(|lib| !lib.exists) {
            return true;
        }
        // Chegar aqui garante `self.remote.reachable`, então um `None` em
        // `remote_base_path_status` só pode significar caminho ausente.
        if let Some(heroic) = &self.heroic {
            if !heroic.local_root.exists {
                return true;
            }
            if !matches!(
                heroic.remote_base_path_status,
                Some(RemotePathStatus::Directory | RemotePathStatus::MountPoint)
            ) {
                return true;
            }
        }
        false
    }
}

/// Roda o diagnóstico completo contra o `ssh` real do sistema e a raiz da
/// Steam real desta máquina.
pub fn run_doctor(cfg: &Config) -> DoctorReport {
    run_doctor_with_options(cfg, "ssh", scan::find_steam_root().as_deref())
}

fn missing_pirata_shortcuts(cfg: &Config, steam_root: &Path) -> Vec<MissingPirataShortcut> {
    cfg.install_path_overrides
        .piratas
        .iter()
        .filter_map(|(app_id, override_path)| {
            let numeric = app_id.parse::<u32>().ok()?;
            if scan::find_shortcut_exe(steam_root, numeric).is_some() {
                return None;
            }
            Some(MissingPirataShortcut {
                app_id: app_id.clone(),
                override_path: override_path.clone(),
            })
        })
        .collect()
}

/// `shortcuts.vdf.vault-sync-bak*` em qualquer perfil local — os mesmos
/// backups que `backup::atomic_backup` já grava sozinho antes de qualquer
/// escrita em `shortcuts.vdf` (ver `inject.rs`/`purge.rs`), aqui só
/// checados pra existência, nunca lidos ou restaurados por `doctor`.
fn any_shortcuts_vdf_backup_exists(steam_root: &Path) -> bool {
    let Ok(profiles) = std::fs::read_dir(steam_root.join("userdata")) else {
        return false;
    };
    profiles.flatten().any(|profile| {
        let Ok(config_entries) = std::fs::read_dir(profile.path().join("config")) else {
            return false;
        };
        config_entries.flatten().any(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("shortcuts.vdf.vault-sync-bak")
        })
    })
}

fn run_doctor_with_options(cfg: &Config, ssh_bin: &str, steam_root: Option<&Path>) -> DoctorReport {
    let libraries = cfg
        .local_libraries
        .iter()
        .map(|path| LibraryCheck::for_path(path))
        .collect();

    let reachable = remote::check_connectivity(ssh_bin, &cfg.remote_ssh_target).is_ok();
    let base_path_status = reachable
        .then(|| remote::check_remote_path(ssh_bin, &cfg.remote_ssh_target, &cfg.remote_base_path))
        .and_then(Result::ok);

    let heroic = match (&cfg.heroic_local_root, &cfg.heroic_remote_base_path) {
        (Some(local_root), Some(remote_base_path)) => {
            let remote_base_path_status = reachable
                .then(|| {
                    remote::check_remote_path(ssh_bin, &cfg.remote_ssh_target, remote_base_path)
                })
                .and_then(Result::ok);
            Some(HeroicCheck {
                local_root: LibraryCheck::for_path(local_root),
                remote_base_path: remote_base_path.clone(),
                remote_base_path_status,
            })
        }
        _ => None,
    };

    let stale_shortcuts = steam_root
        .map(scan::find_stale_shortcuts)
        .unwrap_or_default();

    let missing_pirata_shortcuts = steam_root
        .map(|root| missing_pirata_shortcuts(cfg, root))
        .unwrap_or_default();
    let shortcuts_vdf_backup_available = !missing_pirata_shortcuts.is_empty()
        && steam_root.is_some_and(any_shortcuts_vdf_backup_exists);

    DoctorReport {
        libraries,
        remote: RemoteCheck {
            ssh_target: cfg.remote_ssh_target.clone(),
            reachable,
            base_path: cfg.remote_base_path.clone(),
            base_path_status,
        },
        heroic,
        stale_shortcuts,
        missing_pirata_shortcuts,
        shortcuts_vdf_backup_available,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, File};
    use std::io::Write;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    fn test_dir(label: &str) -> PathBuf {
        crate::testutil::test_dir("doctor", label)
    }

    fn fake_ssh(dir: &std::path::Path, body: &str) -> PathBuf {
        let path = dir.join("fake-ssh.sh");
        let mut f = File::create(&path).unwrap();
        writeln!(f, "#!/bin/sh\n{body}").unwrap();
        #[cfg(unix)]
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn cfg_with_libs(local_libraries: Vec<PathBuf>) -> Config {
        Config {
            local_libraries,
            ..Config::test_default()
        }
    }

    fn cfg_with_heroic(local_libraries: Vec<PathBuf>, heroic_local_root: PathBuf) -> Config {
        Config {
            heroic_local_root: Some(heroic_local_root),
            heroic_remote_base_path: Some(PathBuf::from("/vault/heroic-prefixes")),
            ..cfg_with_libs(local_libraries)
        }
    }

    #[test]
    fn healthy_setup_reports_no_hard_failures() {
        let dir = test_dir("healthy");
        let lib = dir.join("ssd");
        fs::create_dir_all(&lib).unwrap();
        let ssh = fake_ssh(
            &dir,
            "case \"$*\" in *true*) exit 0;; *) printf 'dir\\n';; esac",
        );

        let report = run_doctor_with_options(
            &cfg_with_libs(vec![lib.clone()]),
            ssh.to_str().unwrap(),
            None,
        );

        assert!(!report.has_hard_failures());
        assert!(report.remote.reachable);
        assert_eq!(
            report.remote.base_path_status,
            Some(RemotePathStatus::Directory)
        );
        assert_eq!(report.libraries.len(), 1);
        assert!(report.libraries[0].exists);
    }

    #[test]
    fn missing_local_library_is_a_hard_failure() {
        let dir = test_dir("missing-lib");
        let missing = dir.join("nao-existe");
        let ssh = fake_ssh(
            &dir,
            "case \"$*\" in *true*) exit 0;; *) printf 'dir\\n';; esac",
        );

        let report = run_doctor_with_options(
            &cfg_with_libs(vec![missing.clone()]),
            ssh.to_str().unwrap(),
            None,
        );

        assert!(report.has_hard_failures());
        assert!(!report.libraries[0].exists);
    }

    #[test]
    fn unreachable_remote_is_a_hard_failure_and_skips_path_check() {
        let dir = test_dir("unreachable");
        let lib = dir.join("ssd");
        fs::create_dir_all(&lib).unwrap();
        let ssh = fake_ssh(&dir, "exit 255");

        let report =
            run_doctor_with_options(&cfg_with_libs(vec![lib]), ssh.to_str().unwrap(), None);

        assert!(report.has_hard_failures());
        assert!(!report.remote.reachable);
        assert_eq!(report.remote.base_path_status, None);
    }

    #[test]
    fn missing_remote_base_path_is_a_hard_failure() {
        let dir = test_dir("missing-remote-path");
        let lib = dir.join("ssd");
        fs::create_dir_all(&lib).unwrap();
        let ssh = fake_ssh(
            &dir,
            "case \"$*\" in *true*) exit 0;; *) printf 'missing\\n';; esac",
        );

        let report =
            run_doctor_with_options(&cfg_with_libs(vec![lib]), ssh.to_str().unwrap(), None);

        assert!(report.has_hard_failures());
        assert_eq!(
            report.remote.base_path_status,
            Some(RemotePathStatus::Missing)
        );
    }

    #[test]
    fn library_not_being_a_mount_point_is_only_a_warning() {
        let dir = test_dir("not-a-mountpoint");
        let lib = dir.join("ssd"); // subdiretório comum, não um mount real
        fs::create_dir_all(&lib).unwrap();
        let ssh = fake_ssh(
            &dir,
            "case \"$*\" in *true*) exit 0;; *) printf 'mount\\n';; esac",
        );

        let report =
            run_doctor_with_options(&cfg_with_libs(vec![lib]), ssh.to_str().unwrap(), None);

        assert!(
            !report.has_hard_failures(),
            "não ser ponto de montagem não deveria ser uma falha dura"
        );
        assert!(!report.libraries[0].is_mount_point);
    }

    #[test]
    fn unconfigured_heroic_is_omitted_and_not_a_failure() {
        let dir = test_dir("heroic-unconfigured");
        let lib = dir.join("ssd");
        fs::create_dir_all(&lib).unwrap();
        let ssh = fake_ssh(
            &dir,
            "case \"$*\" in *true*) exit 0;; *) printf 'dir\\n';; esac",
        );

        let report =
            run_doctor_with_options(&cfg_with_libs(vec![lib]), ssh.to_str().unwrap(), None);

        assert!(report.heroic.is_none());
        assert!(!report.has_hard_failures());
    }

    #[test]
    fn healthy_heroic_setup_reports_no_hard_failures() {
        let dir = test_dir("heroic-healthy");
        let lib = dir.join("ssd");
        let heroic_root = dir.join("heroic-prefixes");
        fs::create_dir_all(&lib).unwrap();
        fs::create_dir_all(&heroic_root).unwrap();
        let ssh = fake_ssh(
            &dir,
            "case \"$*\" in *true*) exit 0;; *) printf 'dir\\n';; esac",
        );

        let report = run_doctor_with_options(
            &cfg_with_heroic(vec![lib], heroic_root.clone()),
            ssh.to_str().unwrap(),
            None,
        );

        let heroic = report
            .heroic
            .as_ref()
            .expect("heroic deveria estar presente");
        assert!(heroic.local_root.exists);
        assert_eq!(
            heroic.remote_base_path_status,
            Some(RemotePathStatus::Directory)
        );
        assert!(!report.has_hard_failures());
    }

    #[test]
    fn missing_heroic_local_root_is_a_hard_failure() {
        let dir = test_dir("heroic-missing-local");
        let lib = dir.join("ssd");
        let heroic_root = dir.join("nao-existe");
        fs::create_dir_all(&lib).unwrap();
        let ssh = fake_ssh(
            &dir,
            "case \"$*\" in *true*) exit 0;; *) printf 'dir\\n';; esac",
        );

        let report = run_doctor_with_options(
            &cfg_with_heroic(vec![lib], heroic_root),
            ssh.to_str().unwrap(),
            None,
        );

        assert!(!report.heroic.as_ref().unwrap().local_root.exists);
        assert!(report.has_hard_failures());
    }

    #[test]
    fn missing_heroic_remote_base_path_is_a_hard_failure() {
        let dir = test_dir("heroic-missing-remote");
        let lib = dir.join("ssd");
        let heroic_root = dir.join("heroic-prefixes");
        fs::create_dir_all(&lib).unwrap();
        fs::create_dir_all(&heroic_root).unwrap();
        let ssh = fake_ssh(
            &dir,
            "case \"$*\" in \
                *heroic-prefixes*) printf 'missing\\n';; \
                *true*) exit 0;; \
                *) printf 'dir\\n';; \
             esac",
        );

        let report = run_doctor_with_options(
            &cfg_with_heroic(vec![lib], heroic_root),
            ssh.to_str().unwrap(),
            None,
        );

        assert_eq!(
            report.heroic.as_ref().unwrap().remote_base_path_status,
            Some(RemotePathStatus::Missing)
        );
        assert!(report.has_hard_failures());
    }

    #[test]
    fn stale_shortcuts_are_reported_but_never_a_hard_failure() {
        let dir = test_dir("stale-shortcuts");
        let lib = dir.join("ssd");
        fs::create_dir_all(&lib).unwrap();
        let ssh = fake_ssh(
            &dir,
            "case \"$*\" in *true*) exit 0;; *) printf 'dir\\n';; esac",
        );

        let steam_root = dir.join("steam");
        let dead_dir = steam_root.join("userdata").join("1").join("config");
        let live_dir = steam_root.join("userdata").join("2").join("config");
        fs::create_dir_all(&dead_dir).unwrap();
        fs::create_dir_all(&live_dir).unwrap();
        let live_exe = dir.join("Games").join("piratas").join("Jogo.exe");
        fs::create_dir_all(live_exe.parent().unwrap()).unwrap();
        fs::write(&live_exe, b"fake exe").unwrap();
        fs::write(
            dead_dir.join("shortcuts.vdf"),
            crate::vdf::write_binary(&vec![(
                "shortcuts".to_string(),
                crate::vdf::VdfValue::Object(vec![(
                    "0".to_string(),
                    crate::vdf::VdfValue::Object(vec![
                        ("appid".to_string(), crate::vdf::VdfValue::Int(1)),
                        (
                            "appname".to_string(),
                            crate::vdf::VdfValue::Str("Jogo".to_string()),
                        ),
                        (
                            "Exe".to_string(),
                            crate::vdf::VdfValue::Str("\"/disco/antigo/Jogo.exe\"".to_string()),
                        ),
                    ]),
                )]),
            )]),
        )
        .unwrap();
        fs::write(
            live_dir.join("shortcuts.vdf"),
            crate::vdf::write_binary(&vec![(
                "shortcuts".to_string(),
                crate::vdf::VdfValue::Object(vec![(
                    "0".to_string(),
                    crate::vdf::VdfValue::Object(vec![
                        ("appid".to_string(), crate::vdf::VdfValue::Int(2)),
                        (
                            "appname".to_string(),
                            crate::vdf::VdfValue::Str("Jogo".to_string()),
                        ),
                        (
                            "Exe".to_string(),
                            crate::vdf::VdfValue::Str(format!(
                                "\"{}\"",
                                live_exe.to_str().unwrap()
                            )),
                        ),
                    ]),
                )]),
            )]),
        )
        .unwrap();

        let report = run_doctor_with_options(
            &cfg_with_libs(vec![lib]),
            ssh.to_str().unwrap(),
            Some(&steam_root),
        );

        assert_eq!(report.stale_shortcuts.len(), 1);
        assert_eq!(report.stale_shortcuts[0].dead_app_id, 1);
        assert_eq!(report.stale_shortcuts[0].live_app_id, 2);
        assert!(
            !report.has_hard_failures(),
            "atalho obsoleto nunca deve ser tratado como falha dura"
        );
    }

    #[test]
    fn missing_pirata_shortcuts_flags_overrides_without_a_live_exe_entry() {
        // Regressão do incidente real (2026-09-09, Phonopolis): o
        // shortcuts.vdf da conta principal sumiu por completo, e
        // `menu-launch` falhava sem nenhuma pista visível de por quê.
        let dir = test_dir("missing-pirata-shortcuts");
        let steam_root = dir.join("steam");
        let userdata = steam_root.join("userdata").join("1").join("config");
        fs::create_dir_all(&userdata).unwrap();
        fs::write(
            userdata.join("shortcuts.vdf"),
            crate::vdf::write_binary(&vec![(
                "shortcuts".to_string(),
                crate::vdf::VdfValue::Object(vec![(
                    "0".to_string(),
                    crate::vdf::VdfValue::Object(vec![
                        ("appid".to_string(), crate::vdf::VdfValue::Int(111)),
                        (
                            "appname".to_string(),
                            crate::vdf::VdfValue::Str("Presente".to_string()),
                        ),
                        (
                            "Exe".to_string(),
                            crate::vdf::VdfValue::Str("\"/jogos/presente.exe\"".to_string()),
                        ),
                    ]),
                )]),
            )]),
        )
        .unwrap();

        let mut cfg = cfg_with_libs(vec![dir.join("ssd")]);
        cfg.install_path_overrides
            .piratas
            .insert("111".to_string(), PathBuf::from("/jogos/presente"));
        cfg.install_path_overrides
            .piratas
            .insert("222".to_string(), PathBuf::from("/jogos/ausente"));

        let missing = missing_pirata_shortcuts(&cfg, &steam_root);

        assert_eq!(missing.len(), 1, "só o 222 deveria faltar: {missing:?}");
        assert_eq!(missing[0].app_id, "222");
    }

    #[test]
    fn any_shortcuts_vdf_backup_exists_detects_a_backup_in_any_profile() {
        let dir = test_dir("shortcuts-backup-present");
        let steam_root = dir.join("steam");
        let config_dir = steam_root.join("userdata").join("1").join("config");
        fs::create_dir_all(&config_dir).unwrap();
        fs::write(config_dir.join("shortcuts.vdf.vault-sync-bak-12345"), b"").unwrap();

        assert!(any_shortcuts_vdf_backup_exists(&steam_root));
    }

    #[test]
    fn any_shortcuts_vdf_backup_exists_is_false_without_any_backup() {
        let dir = test_dir("shortcuts-backup-absent");
        let steam_root = dir.join("steam");
        let config_dir = steam_root.join("userdata").join("1").join("config");
        fs::create_dir_all(&config_dir).unwrap();

        assert!(!any_shortcuts_vdf_backup_exists(&steam_root));
    }
}
