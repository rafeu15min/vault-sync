//! Listagem só-leitura do que já existe no FeuVault (`compatdata` Steam e
//! prefixos Heroic) — nunca muta nada, mesmo espírito do `doctor.rs`. Serve
//! pra responder "o que já foi migrado?" sem precisar inspecionar o
//! servidor manualmente via `ssh`.

use crate::config::Config;
use crate::remote::{self, RemoteError};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteListing {
    pub steam_compatdata_ids: Vec<String>,
    /// `None` quando `heroic_remote_base_path` não está configurado —
    /// mesma convenção de "omitido, não é falha" que `doctor::HeroicCheck`
    /// já usa.
    pub heroic_titles: Option<Vec<String>>,
    /// Nomes de pasta sob `remote_installs_base_path/piratas` — as
    /// instalações completas de pirata que já existem no FeuVault, prontas
    /// pra `stage` trazer de volta. `None` quando `remote_installs_base_path`
    /// não está configurado, mesma convenção de `heroic_titles`. Existe
    /// porque `steam_compatdata_ids` só cobre `compatdata` (save/prefixo),
    /// nunca a instalação completa em si — sem isso não dá pra saber, sem
    /// entrar no servidor na mão, o que existe pra adotar via `stage`.
    pub piratas_installs: Option<Vec<String>>,
    /// Nomes de pasta sob `remote_installs_base_path/epic` — instalações
    /// completas Epic/Heroic já confirmadas no servidor. Mesma convenção de
    /// `piratas_installs` (`None` quando `remote_installs_base_path` não
    /// está configurado). Existe porque `heroic_titles` só cobre o prefixo
    /// Wine/compatdata (`heroic_remote_base_path`) — um jogo Epic nativo
    /// Linux, que nunca cria prefixo, não tem como aparecer ali mesmo
    /// depois de 100% migrado. `catalog::build_catalog_from` usa este campo
    /// pra reconhecer esse caso via `install_path_overrides.epic`.
    pub epic_installs: Option<Vec<String>>,
}

/// Consulta o FeuVault de verdade e, se bem-sucedida, grava a listagem em
/// `cache_path()` — sobrevive a reinicializações e alimenta
/// `read_cache()` pra quando o servidor estiver inacessível (ver
/// `catalog::build_catalog_best_effort`). A escrita do cache é puramente
/// best-effort: uma falha ali (disco cheio, permissão) nunca derruba uma
/// listagem que já funcionou de verdade.
pub fn run_list(cfg: &Config) -> Result<RemoteListing, RemoteError> {
    let listing = run_list_with_options(cfg, "ssh")?;
    write_cache(&listing);
    Ok(listing)
}

fn cache_path() -> Option<PathBuf> {
    Some(
        dirs::config_dir()?
            .join("vault-sync")
            .join("remote_catalog_cache.json"),
    )
}

fn write_cache(listing: &RemoteListing) {
    if let Some(path) = cache_path() {
        write_cache_to(&path, listing);
    }
}

fn write_cache_to(path: &std::path::Path, listing: &RemoteListing) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string(listing) {
        let _ = std::fs::write(path, json);
    }
}

/// Última listagem remota bem-sucedida conhecida, gravada por `run_list`.
/// `None` tanto na primeira execução de sempre (nunca houve uma listagem
/// bem-sucedida) quanto se o arquivo estiver corrompido/ilegível — os dois
/// casos são "não dá pra confiar nisso", quem chama decide como degradar.
pub fn read_cache() -> Option<RemoteListing> {
    read_cache_from(&cache_path()?)
}

fn read_cache_from(path: &std::path::Path) -> Option<RemoteListing> {
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

fn run_list_with_options(cfg: &Config, ssh_bin: &str) -> Result<RemoteListing, RemoteError> {
    let compatdata_dir = cfg.remote_base_path.join("steamapps").join("compatdata");
    let mut steam_compatdata_ids =
        remote::list_remote_dir_entries(ssh_bin, &cfg.remote_ssh_target, &compatdata_dir)?;
    steam_compatdata_ids.sort();

    let heroic_titles = match &cfg.heroic_remote_base_path {
        Some(base) => {
            let mut titles =
                remote::list_remote_dir_entries(ssh_bin, &cfg.remote_ssh_target, base)?;
            titles.sort();
            Some(titles)
        }
        None => None,
    };

    let piratas_installs = match &cfg.remote_installs_base_path {
        Some(base) => {
            let mut names = remote::list_remote_dir_entries(
                ssh_bin,
                &cfg.remote_ssh_target,
                &base.join("piratas"),
            )?;
            names.sort();
            Some(names)
        }
        None => None,
    };

    let epic_installs = match &cfg.remote_installs_base_path {
        Some(base) => {
            let mut names = remote::list_remote_dir_entries(
                ssh_bin,
                &cfg.remote_ssh_target,
                &base.join("epic"),
            )?;
            names.sort();
            Some(names)
        }
        None => None,
    };

    Ok(RemoteListing {
        steam_compatdata_ids,
        heroic_titles,
        piratas_installs,
        epic_installs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, File};
    use std::io::Write;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    fn test_dir(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "vault-sync-list-test-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn fake_ssh(dir: &std::path::Path, body: &str) -> std::path::PathBuf {
        let path = dir.join("fake-ssh.sh");
        let mut f = File::create(&path).unwrap();
        writeln!(f, "#!/bin/sh\n{body}").unwrap();
        #[cfg(unix)]
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn cfg_steam_only() -> Config {
        Config::test_default()
    }

    fn cfg_with_heroic() -> Config {
        Config {
            heroic_local_root: Some(std::path::PathBuf::from("/fake/heroic/prefixes")),
            heroic_remote_base_path: Some(std::path::PathBuf::from("/fake/remote/heroic")),
            ..cfg_steam_only()
        }
    }

    #[test]
    fn lists_and_sorts_remote_steam_compatdata_ids() {
        let dir = test_dir("steam-sorted");
        let ssh = fake_ssh(&dir, "printf '200\\n100\\n'");

        let listing = run_list_with_options(&cfg_steam_only(), ssh.to_str().unwrap()).unwrap();

        assert_eq!(listing.steam_compatdata_ids, vec!["100", "200"]);
        assert_eq!(listing.heroic_titles, None);
    }

    #[test]
    fn omits_heroic_titles_when_unconfigured() {
        let dir = test_dir("heroic-unconfigured");
        let ssh = fake_ssh(&dir, "printf ''");

        let listing = run_list_with_options(&cfg_steam_only(), ssh.to_str().unwrap()).unwrap();

        assert_eq!(listing.heroic_titles, None);
    }

    #[test]
    fn lists_heroic_titles_when_configured() {
        let dir = test_dir("heroic-configured");
        // O caminho remoto passado difere entre a consulta Steam e a
        // Heroic (steamapps/compatdata vs. heroic_remote_base_path direto)
        // — o fake responde igual pras duas, só o conteúdo importa aqui.
        let ssh = fake_ssh(&dir, "printf 'Hogwarts Legacy\\nDisco Elysium\\n'");

        let listing = run_list_with_options(&cfg_with_heroic(), ssh.to_str().unwrap()).unwrap();

        assert_eq!(
            listing.heroic_titles,
            Some(vec![
                "Disco Elysium".to_string(),
                "Hogwarts Legacy".to_string()
            ])
        );
    }

    #[test]
    fn omits_piratas_installs_when_remote_installs_base_path_unconfigured() {
        let dir = test_dir("piratas-unconfigured");
        let ssh = fake_ssh(&dir, "printf ''");

        let listing = run_list_with_options(&cfg_steam_only(), ssh.to_str().unwrap()).unwrap();

        assert_eq!(listing.piratas_installs, None);
    }

    #[test]
    fn lists_piratas_installs_when_configured() {
        let dir = test_dir("piratas-configured");
        let ssh = fake_ssh(&dir, "printf 'Phonopolis\\nHaste\\n'");
        let cfg = Config {
            remote_installs_base_path: Some(std::path::PathBuf::from("/vault/game-installs")),
            ..cfg_steam_only()
        };

        let listing = run_list_with_options(&cfg, ssh.to_str().unwrap()).unwrap();

        assert_eq!(
            listing.piratas_installs,
            Some(vec!["Haste".to_string(), "Phonopolis".to_string()])
        );
    }

    #[test]
    fn omits_epic_installs_when_remote_installs_base_path_unconfigured() {
        let dir = test_dir("epic-installs-unconfigured");
        let ssh = fake_ssh(&dir, "printf ''");

        let listing = run_list_with_options(&cfg_steam_only(), ssh.to_str().unwrap()).unwrap();

        assert_eq!(listing.epic_installs, None);
    }

    #[test]
    fn lists_epic_installs_when_configured() {
        let dir = test_dir("epic-installs-configured");
        let ssh = fake_ssh(&dir, "printf 'Chivalry2\\nDiscoElysium\\n'");
        let cfg = Config {
            remote_installs_base_path: Some(std::path::PathBuf::from("/vault/game-installs")),
            ..cfg_steam_only()
        };

        let listing = run_list_with_options(&cfg, ssh.to_str().unwrap()).unwrap();

        assert_eq!(
            listing.epic_installs,
            Some(vec!["Chivalry2".to_string(), "DiscoElysium".to_string()])
        );
    }

    #[test]
    fn cache_round_trips_a_written_listing() {
        let dir = test_dir("cache-round-trip");
        let path = dir.join("remote_catalog_cache.json");
        let listing = RemoteListing {
            steam_compatdata_ids: vec!["100".to_string(), "200".to_string()],
            heroic_titles: Some(vec!["Disco Elysium".to_string()]),
            piratas_installs: Some(vec!["Haste".to_string()]),
            epic_installs: Some(vec!["DiscoElysium".to_string()]),
        };

        write_cache_to(&path, &listing);

        assert_eq!(read_cache_from(&path), Some(listing));
    }

    #[test]
    fn cache_read_is_none_when_file_is_missing() {
        let dir = test_dir("cache-missing");
        assert_eq!(read_cache_from(&dir.join("nao-existe.json")), None);
    }

    #[test]
    fn cache_read_is_none_when_file_is_corrupted() {
        let dir = test_dir("cache-corrupt");
        let path = dir.join("remote_catalog_cache.json");
        fs::write(&path, "isto nao e json valido").unwrap();

        assert_eq!(read_cache_from(&path), None);
    }

    #[test]
    fn cache_write_creates_missing_parent_directories() {
        let dir = test_dir("cache-parent-missing");
        let path = dir.join("nested").join("remote_catalog_cache.json");
        let listing = RemoteListing::default();

        write_cache_to(&path, &listing);

        assert_eq!(read_cache_from(&path), Some(listing));
    }
}
