use crate::domain::{GameSource, LibraryLocation};
use std::path::{Path, PathBuf};

/// Referência ao servidor remoto (FeuVault) — alvo SSH + diretório base
/// onde a "biblioteca espelho" vive.
#[derive(Debug, Clone)]
pub struct RemoteVault {
    /// Alvo SSH, ex.: "feuvault" (via ~/.ssh/config) ou "user@host".
    pub ssh_target: String,
    pub base_path: PathBuf,
}

/// Coordenadas remotas para um jogo: onde buscar o manifesto de
/// redescoberta (se oficial) e onde ficam os dados de progressão/save.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemotePaths {
    pub compatdata_dir: PathBuf,
    pub manifest_file: Option<PathBuf>,
    pub progress_dir: PathBuf,
}

/// Coordenadas locais estritamente delimitadas para deleção pós-sync.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalDeletionTargets {
    pub compatdata_dir: PathBuf,
    pub manifest_file: Option<PathBuf>,
}

/// Erros de segurança de caminho. Qualquer app_id que não seja
/// estritamente numérico é rejeitado — na prática, tanto `steam_scan.py`
/// (`if not appid.isdigit(): continue`) quanto o Steam garantem que nomes
/// de pastas em `compatdata/` e app_ids são sempre dígitos. Isso elimina de
/// saída qualquer possibilidade de path traversal (`..`, `/`, `.`, etc.)
/// nos alvos de deleção, sem precisar normalizar/canonicalizar caminhos.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PathSafetyError {
    #[error("app_id vazio — recusando construir caminho de deleção")]
    EmptyAppId,
    #[error("app_id não-numérico ('{0}') — recusando construir caminho de deleção")]
    NonNumericAppId(String),
    #[error("título Heroic inseguro ('{0}') — recusando construir caminho de deleção")]
    UnsafeHeroicTitle(String),
}

fn validated_app_id(source: &GameSource) -> Result<String, PathSafetyError> {
    let raw = source.app_id_str();
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(PathSafetyError::EmptyAppId);
    }
    if !trimmed.chars().all(|c| c.is_ascii_digit()) {
        return Err(PathSafetyError::NonNumericAppId(raw));
    }
    Ok(trimmed.to_string())
}

/// Mesma finalidade de `validated_app_id` (blindagem contra path traversal
/// nos alvos de deleção/sincronização), mas para o título de pasta do
/// Heroic, que legitimamente contém espaços e pontuação — não dá pra exigir
/// "só dígitos" aqui. Em vez disso, rejeita qualquer coisa que quebraria a
/// suposição de "um único componente de caminho": `/`, `\`, byte nulo, e os
/// componentes especiais `.`/`..`.
fn validated_heroic_title(source: &GameSource) -> Result<String, PathSafetyError> {
    let raw = source.app_id_str();
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(PathSafetyError::EmptyAppId);
    }
    if trimmed == "." || trimmed == ".." || trimmed.contains(['/', '\\', '\0']) {
        return Err(PathSafetyError::UnsafeHeroicTitle(raw));
    }
    Ok(trimmed.to_string())
}

/// Detecta se `path` é a raiz de um sistema de arquivos distinto do seu
/// diretório pai, comparando `st_dev` — a mesma técnica do utilitário
/// `mountpoint`, sem depender dele estar instalado. Usado só pelo `doctor`,
/// nunca como barreira dentro de `run`/`ingest`/`purge`: um SSD "efêmero"
/// deveria ser um ponto de montagem separado, mas exigir isso à força
/// quebraria configurações legítimas de disco único, então a checagem é
/// puramente informativa. O caso perigoso que ela detecta: o diretório do
/// ponto de montagem existe (foi criado com `mkdir` para servir de alvo),
/// mas o dispositivo não está montado ali — nesse estado o diretório
/// aparece vazio e gravável normalmente, e dados acabam silenciosamente
/// escritos na partição raiz em vez do SSD/HDD esperado.
#[cfg(unix)]
pub fn is_mount_point(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    let Some(parent) = path.parent() else {
        // Caminho sem pai (a própria raiz "/") é trivialmente seu próprio
        // limite de sistema de arquivos.
        return true;
    };
    let Ok(parent_meta) = std::fs::metadata(parent) else {
        return false;
    };
    meta.dev() != parent_meta.dev()
}

#[cfg(not(unix))]
pub fn is_mount_point(_path: &Path) -> bool {
    true
}

impl RemoteVault {
    /// Constrói as coordenadas remotas para um jogo. Para as variantes Steam,
    /// inclui condicionalmente o manifesto (só jogos oficiais) e o diretório
    /// de progressão sob `pfx/drive_c/users/steamuser`. Para `HeroicGame`,
    /// `self.base_path` já é o "diretório de títulos" Heroic (equivalente ao
    /// `Prefixes/default` local) — o alvo remoto é só `<base_path>/<título>`,
    /// sem manifesto (Heroic não tem esse conceito) e com `progress_dir`
    /// apontando pro próprio prefixo, já que não há um save path confiável
    /// por jogo (ver `domain::GameSource::HeroicGame`).
    pub fn paths_for(&self, source: &GameSource) -> Result<RemotePaths, PathSafetyError> {
        if source.is_heroic() {
            let title = validated_heroic_title(source)?;
            let compatdata_dir = self.base_path.join(&title);
            return Ok(RemotePaths {
                compatdata_dir: compatdata_dir.clone(),
                manifest_file: None,
                progress_dir: compatdata_dir,
            });
        }

        let app_id = validated_app_id(source)?;
        let steamapps = self.base_path.join("steamapps");
        let compatdata_dir = steamapps.join("compatdata").join(&app_id);
        let manifest_file = source
            .is_official()
            .then(|| steamapps.join(format!("appmanifest_{app_id}.acf")));
        let progress_dir = compatdata_dir
            .join("pfx")
            .join("drive_c")
            .join("users")
            .join("steamuser");

        Ok(RemotePaths {
            compatdata_dir,
            manifest_file,
            progress_dir,
        })
    }
}

impl LibraryLocation {
    /// Constrói os alvos *exatos* de deleção local pós-sync bem-sucedido.
    /// Para as variantes Steam, deliberadamente estreito: só produz caminhos
    /// da forma `<root>/steamapps/compatdata/<app_id numérico>` (+ manifesto
    /// opcional) — nunca `<root>` em si e nunca o diretório `compatdata`
    /// compartilhado por todos os outros jogos instalados. Para
    /// `HeroicGame`, `self.root` já é o diretório `Prefixes/default` — o
    /// alvo é só `<root>/<título>`, igualmente nunca `<root>` em si.
    pub fn deletion_targets(
        &self,
        source: &GameSource,
    ) -> Result<LocalDeletionTargets, PathSafetyError> {
        if source.is_heroic() {
            let title = validated_heroic_title(source)?;
            return Ok(LocalDeletionTargets {
                compatdata_dir: self.root.join(&title),
                manifest_file: None,
            });
        }

        let app_id = validated_app_id(source)?;
        let steamapps = self.root.join("steamapps");
        let compatdata_dir = steamapps.join("compatdata").join(&app_id);
        let manifest_file = source
            .is_official()
            .then(|| steamapps.join(format!("appmanifest_{app_id}.acf")));

        Ok(LocalDeletionTargets {
            compatdata_dir,
            manifest_file,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::GameSource;

    fn lib() -> LibraryLocation {
        LibraryLocation::new("/mnt/SSD2/SteamLibrary")
    }

    fn vault() -> RemoteVault {
        RemoteVault {
            ssh_target: "feuvault".into(),
            base_path: PathBuf::from("/mnt/vault/steam-library"),
        }
    }

    #[test]
    fn official_game_targets_manifest_and_compatdata() {
        let source = GameSource::Official { app_id: 570 };
        let targets = lib().deletion_targets(&source).unwrap();
        assert_eq!(
            targets.compatdata_dir,
            PathBuf::from("/mnt/SSD2/SteamLibrary/steamapps/compatdata/570")
        );
        assert_eq!(
            targets.manifest_file,
            Some(PathBuf::from(
                "/mnt/SSD2/SteamLibrary/steamapps/appmanifest_570.acf"
            ))
        );
    }

    #[test]
    fn non_steam_shortcut_has_no_manifest_target() {
        let source = GameSource::NonSteamShortcut {
            app_id: 1234567890,
            shortcut_name: "Exemplo".into(),
        };
        let targets = lib().deletion_targets(&source).unwrap();
        assert!(targets.manifest_file.is_none());
        assert_eq!(
            targets.compatdata_dir,
            PathBuf::from("/mnt/SSD2/SteamLibrary/steamapps/compatdata/1234567890")
        );
    }

    #[test]
    fn orphan_with_numeric_id_resolves_normally() {
        let source = GameSource::Orphan {
            app_id: "42".into(),
            clues: vec!["ExemploClue".into()],
        };
        let targets = lib().deletion_targets(&source).unwrap();
        assert!(targets.manifest_file.is_none());
        assert_eq!(
            targets.compatdata_dir,
            PathBuf::from("/mnt/SSD2/SteamLibrary/steamapps/compatdata/42")
        );
    }

    #[test]
    fn empty_app_id_is_rejected() {
        let source = GameSource::Orphan {
            app_id: "".into(),
            clues: vec![],
        };
        assert_eq!(
            lib().deletion_targets(&source).unwrap_err(),
            PathSafetyError::EmptyAppId
        );
    }

    #[test]
    fn whitespace_only_app_id_is_rejected() {
        let source = GameSource::Orphan {
            app_id: "   ".into(),
            clues: vec![],
        };
        assert_eq!(
            lib().deletion_targets(&source).unwrap_err(),
            PathSafetyError::EmptyAppId
        );
    }

    #[test]
    fn path_traversal_app_id_is_rejected() {
        for malicious in ["../../..", "..", ".", "/etc", "570/../../../root"] {
            let source = GameSource::Orphan {
                app_id: malicious.into(),
                clues: vec![],
            };
            let err = lib().deletion_targets(&source).unwrap_err();
            assert!(
                matches!(err, PathSafetyError::NonNumericAppId(_)),
                "esperava rejeição de '{malicious}', obteve {err:?}"
            );
        }
    }

    #[test]
    fn deletion_target_is_always_nested_under_library_root() {
        let source = GameSource::Official { app_id: 730 };
        let targets = lib().deletion_targets(&source).unwrap();
        assert!(targets.compatdata_dir.starts_with(&lib().root));
        assert_ne!(targets.compatdata_dir, lib().root);
    }

    #[test]
    fn deletion_target_is_never_the_shared_compatdata_directory() {
        // Para qualquer app_id numérico válido, o alvo tem que estar UM
        // nível abaixo de compatdata/, nunca ser o próprio compatdata/
        // (que contém os prefixos de TODOS os outros jogos).
        let shared_compatdata = lib().root.join("steamapps").join("compatdata");
        for app_id in ["1", "570", "9999999999"] {
            let source = GameSource::Orphan {
                app_id: app_id.into(),
                clues: vec![],
            };
            let targets = lib().deletion_targets(&source).unwrap();
            assert_ne!(targets.compatdata_dir, shared_compatdata);
            assert_eq!(
                targets.compatdata_dir.parent(),
                Some(shared_compatdata.as_path())
            );
        }
    }

    #[test]
    fn remote_paths_include_progress_dir_under_correct_compatdata() {
        let source = GameSource::Official { app_id: 730 };
        let remote = vault().paths_for(&source).unwrap();
        assert_eq!(
            remote.compatdata_dir,
            PathBuf::from("/mnt/vault/steam-library/steamapps/compatdata/730")
        );
        assert!(remote.progress_dir.starts_with(&remote.compatdata_dir));
        assert_eq!(
            remote.manifest_file,
            Some(PathBuf::from(
                "/mnt/vault/steam-library/steamapps/appmanifest_730.acf"
            ))
        );
    }

    #[test]
    fn remote_paths_reject_unsafe_app_id_same_as_local() {
        let source = GameSource::Orphan {
            app_id: "../escape".into(),
            clues: vec![],
        };
        assert!(vault().paths_for(&source).is_err());
    }

    #[test]
    fn heroic_deletion_target_is_title_joined_directly_under_root() {
        let source = GameSource::HeroicGame {
            title: "Disco Elysium - The Final Cut".into(),
        };
        let targets = lib().deletion_targets(&source).unwrap();
        assert_eq!(
            targets.compatdata_dir,
            PathBuf::from("/mnt/SSD2/SteamLibrary/Disco Elysium - The Final Cut")
        );
        assert!(targets.manifest_file.is_none());
    }

    #[test]
    fn heroic_remote_paths_are_title_joined_directly_under_base_path() {
        let source = GameSource::HeroicGame {
            title: "Trine Enchanted Edition".into(),
        };
        let remote = vault().paths_for(&source).unwrap();
        assert_eq!(
            remote.compatdata_dir,
            PathBuf::from("/mnt/vault/steam-library/Trine Enchanted Edition")
        );
        assert!(remote.manifest_file.is_none());
        assert_eq!(remote.progress_dir, remote.compatdata_dir);
    }

    #[test]
    fn heroic_title_path_traversal_is_rejected() {
        for malicious in ["..", ".", "../../etc", "a/../../b", "a/b", "a\\b"] {
            let source = GameSource::HeroicGame {
                title: malicious.into(),
            };
            let err = lib().deletion_targets(&source).unwrap_err();
            assert!(
                matches!(err, PathSafetyError::UnsafeHeroicTitle(_)),
                "esperava rejeição de '{malicious}', obteve {err:?}"
            );
        }
    }

    #[test]
    fn heroic_empty_title_is_rejected() {
        let source = GameSource::HeroicGame { title: "".into() };
        assert_eq!(
            lib().deletion_targets(&source).unwrap_err(),
            PathSafetyError::EmptyAppId
        );
    }

    #[test]
    fn heroic_deletion_target_is_always_nested_under_library_root() {
        let source = GameSource::HeroicGame {
            title: "Hogwarts Legacy".into(),
        };
        let targets = lib().deletion_targets(&source).unwrap();
        assert!(targets.compatdata_dir.starts_with(&lib().root));
        assert_ne!(targets.compatdata_dir, lib().root);
    }

    #[test]
    fn is_mount_point_treats_root_as_its_own_filesystem_boundary() {
        assert!(is_mount_point(Path::new("/")));
    }

    #[test]
    fn is_mount_point_is_false_for_a_nonexistent_path() {
        assert!(!is_mount_point(Path::new(
            "/tmp/vault-sync-definitely-does-not-exist-xyz"
        )));
    }

    #[test]
    fn is_mount_point_is_false_for_a_plain_subdirectory() {
        // Um subdiretório recém-criado dentro de um diretório de teste está,
        // por construção, no mesmo dispositivo do seu pai — nunca um ponto
        // de montagem separado, independentemente do layout de discos da
        // máquina que roda o teste.
        let dir = std::env::temp_dir().join(format!(
            "vault-sync-mountpoint-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(!is_mount_point(&dir));
    }
}
