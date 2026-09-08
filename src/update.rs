//! `vault-sync update <app_id>`: atualiza um jogo Steam oficial já instalado
//! localmente via `steamcmd`, direto do CDN da Valve — nunca toca o
//! FeuVault, propositalmente separado de `run`/`ingest` (ver discussão de
//! design: uma falha de autenticação/Steam Guard aqui nunca deve travar uma
//! sessão de jogo em andamento nem um `ingest` em lote).
//!
//! Confirmado manualmente em 2026-08-15: `steamcmd` reaproveita a mesma
//! `~/.local/share/Steam/config/config.vdf` do cliente gráfico já logado —
//! com `stdin` fechado, `steamcmd +login <conta> +quit` autentica sozinho
//! ("Logging in using cached credentials"), sem pedir senha nem Steam
//! Guard de novo. `app_update ... validate` testado de ponta a ponta com o
//! AppID de teste público da Valve (480, Spacewar).

use crate::config::Config;
use crate::install_paths;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    #[error("'update' exige `steam_account` configurado em config.json (AccountName da Steam, não o nome de exibição)")]
    NoSteamAccount,
    #[error(
        "não foi possível resolver o diretório de instalação local do AppID '{0}' — rode 'ingest --full' ou 'run --steam' primeiro, ou confira install_path_overrides.steam"
    )]
    UnresolvedInstallDir(String),
    #[error("falha ao executar '{bin}': {source}")]
    Spawn {
        bin: String,
        #[source]
        source: std::io::Error,
    },
    #[error("steamcmd terminou sem confirmar sucesso — saída:\n{0}")]
    SteamCmdFailed(String),
}

/// Resultado de uma atualização bem-sucedida — só o necessário pra UI
/// decidir o que imprimir, sem forçar quem chama a re-parsear a saída
/// crua do `steamcmd`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateReport {
    pub app_id: String,
    pub install_dir: PathBuf,
}

pub fn run_update(cfg: &Config, app_id: &str) -> Result<UpdateReport, UpdateError> {
    run_update_with_options(cfg, app_id, "steamcmd")
}

fn run_update_with_options(
    cfg: &Config,
    app_id: &str,
    steamcmd_bin: &str,
) -> Result<UpdateReport, UpdateError> {
    let account = cfg
        .steam_account
        .as_deref()
        .ok_or(UpdateError::NoSteamAccount)?;
    let install_dir = resolve_install_dir(cfg, app_id)
        .ok_or_else(|| UpdateError::UnresolvedInstallDir(app_id.to_string()))?;

    run_steamcmd_update(steamcmd_bin, account, app_id, &install_dir)?;

    Ok(UpdateReport {
        app_id: app_id.to_string(),
        install_dir,
    })
}

/// Mesma fonte de verdade que `ingest --full`/`session.rs` usam pra
/// instalações oficiais: overrides de config primeiro, senão o
/// `appmanifest_<id>.acf` de cada `local_libraries` — a primeira biblioteca
/// que resolver um caminho verificável em disco vence. Nunca considera um
/// download em andamento como "instalado" (`resolve_official_install_dir`
/// já retorna `None` nesse caso — ver `install_paths.rs`), então `update`
/// nunca briga com um download que o próprio cliente Steam ainda está
/// terminando.
fn resolve_install_dir(cfg: &Config, app_id: &str) -> Option<PathBuf> {
    let steam_override = cfg.install_path_overrides.steam.get(app_id);
    cfg.local_libraries.iter().find_map(|lib| {
        install_paths::resolve_official_install_dir(
            lib,
            app_id,
            steam_override.map(|p| p.as_path()),
        )
    })
}

/// `+force_install_dir` precisa vir *antes* de `+login` na linha de
/// comando do steamcmd — confirmado manualmente: invertido ainda funciona
/// (steamcmd só imprime um aviso e segue), mas a ordem certa evita
/// depender desse comportamento não documentado.
fn run_steamcmd_update(
    steamcmd_bin: &str,
    account: &str,
    app_id: &str,
    install_dir: &Path,
) -> Result<(), UpdateError> {
    let output = Command::new(steamcmd_bin)
        .arg("+force_install_dir")
        .arg(install_dir)
        .arg("+login")
        .arg(account)
        .arg("+app_update")
        .arg(app_id)
        .arg("validate")
        .arg("+quit")
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|source| UpdateError::Spawn {
            bin: steamcmd_bin.to_string(),
            source,
        })?;

    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    // O código de saída do steamcmd é notoriamente pouco confiável (às
    // vezes 0 mesmo em falha) — a string de sucesso documentada pela
    // própria Valve é o sinal real, mesma checagem usada por scripts de CI
    // que automatizam steamcmd.
    if combined.contains(&format!("Success! App '{app_id}'")) {
        Ok(())
    } else {
        Err(UpdateError::SteamCmdFailed(combined))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::InstallPathOverrides;
    use std::collections::HashMap;
    use std::fs;

    fn test_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "vault-sync-update-test-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn fake_bin(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }

    fn base_config(local_libraries: Vec<PathBuf>) -> Config {
        Config {
            local_libraries,
            steam_account: Some("conta-de-teste".to_string()),
            ..Config::test_default()
        }
    }

    #[test]
    fn missing_steam_account_is_a_clear_error() {
        let dir = test_dir("no-account");
        let mut cfg = base_config(vec![dir]);
        cfg.steam_account = None;

        let err = run_update_with_options(&cfg, "730", "irrelevante").unwrap_err();
        assert!(matches!(err, UpdateError::NoSteamAccount));
    }

    #[test]
    fn unresolvable_install_dir_is_a_clear_error() {
        let dir = test_dir("unresolvable");
        let cfg = base_config(vec![dir]);

        let err = run_update_with_options(&cfg, "730", "irrelevante").unwrap_err();
        assert!(matches!(err, UpdateError::UnresolvedInstallDir(id) if id == "730"));
    }

    #[test]
    fn resolves_install_dir_via_manifest_before_calling_steamcmd() {
        let dir = test_dir("via-manifest");
        let local_lib = dir.join("steam-lib");
        let install_dir = local_lib.join("steamapps").join("common").join("Meu Jogo");
        fs::create_dir_all(&install_dir).unwrap();
        fs::write(
            local_lib.join("steamapps").join("appmanifest_730.acf"),
            "\"installdir\"\t\t\"Meu Jogo\"\n",
        )
        .unwrap();

        let cfg = base_config(vec![local_lib]);
        let steamcmd = fake_bin(
            &dir,
            "fake-steamcmd.sh",
            "echo \"Success! App '730' fully installed.\"",
        );

        let report = run_update_with_options(&cfg, "730", steamcmd.to_str().unwrap()).unwrap();
        assert_eq!(report.app_id, "730");
        assert_eq!(report.install_dir, install_dir);
    }

    #[test]
    fn override_path_wins_over_manifest_installdir() {
        let dir = test_dir("override-wins");
        let local_lib = dir.join("steam-lib");
        fs::create_dir_all(local_lib.join("steamapps")).unwrap();
        let override_dir = dir.join("caminho-manual");
        fs::create_dir_all(&override_dir).unwrap();

        let mut overrides = InstallPathOverrides::default();
        let mut steam = HashMap::new();
        steam.insert("730".to_string(), override_dir.clone());
        overrides.steam = steam;

        let mut cfg = base_config(vec![local_lib]);
        cfg.install_path_overrides = overrides;
        let steamcmd = fake_bin(
            &dir,
            "fake-steamcmd.sh",
            "echo \"Success! App '730' fully installed.\"",
        );

        let report = run_update_with_options(&cfg, "730", steamcmd.to_str().unwrap()).unwrap();
        assert_eq!(report.install_dir, override_dir);
    }

    #[test]
    fn steamcmd_output_without_success_marker_is_an_error() {
        let dir = test_dir("no-success-marker");
        let local_lib = dir.join("steam-lib");
        fs::create_dir_all(local_lib.join("steamapps").join("common").join("Meu Jogo")).unwrap();
        fs::write(
            local_lib.join("steamapps").join("appmanifest_730.acf"),
            "\"installdir\"\t\t\"Meu Jogo\"\n",
        )
        .unwrap();

        let cfg = base_config(vec![local_lib]);
        let steamcmd = fake_bin(
            &dir,
            "fake-steamcmd.sh",
            "echo 'ERROR! Failed to install app \\'730\\' (No subscription)'",
        );

        let err = run_update_with_options(&cfg, "730", steamcmd.to_str().unwrap()).unwrap_err();
        assert!(matches!(err, UpdateError::SteamCmdFailed(_)));
    }

    #[test]
    fn passes_force_install_dir_before_login_and_app_update_with_validate() {
        let dir = test_dir("arg-order");
        let local_lib = dir.join("steam-lib");
        fs::create_dir_all(local_lib.join("steamapps").join("common").join("Meu Jogo")).unwrap();
        fs::write(
            local_lib.join("steamapps").join("appmanifest_730.acf"),
            "\"installdir\"\t\t\"Meu Jogo\"\n",
        )
        .unwrap();
        let cfg = base_config(vec![local_lib]);

        // Grava os args recebidos num arquivo pra inspecionar depois — mais
        // simples e portável do que tentar capturar argv de um teste Rust
        // via variável de ambiente.
        let args_log = dir.join("args.log");
        let steamcmd = fake_bin(
            &dir,
            "fake-steamcmd.sh",
            &format!(
                "echo \"$@\" > {}\necho \"Success! App '730' fully installed.\"",
                args_log.display()
            ),
        );

        run_update_with_options(&cfg, "730", steamcmd.to_str().unwrap()).unwrap();

        let recorded = fs::read_to_string(&args_log).unwrap();
        let force_pos = recorded.find("+force_install_dir").unwrap();
        let login_pos = recorded.find("+login").unwrap();
        let app_update_pos = recorded.find("+app_update").unwrap();
        assert!(force_pos < login_pos, "força o install dir antes de logar");
        assert!(
            login_pos < app_update_pos,
            "loga antes de pedir o app_update"
        );
        assert!(recorded.contains("validate"));
        assert!(recorded.contains("+quit"));
    }
}
