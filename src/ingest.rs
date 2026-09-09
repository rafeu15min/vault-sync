//! Ingestão em lote (5.1): compara os `compatdata/<app_id>` presentes nas
//! bibliotecas locais com o que já existe no FeuVault e envia, em segundo
//! plano, apenas as instâncias que só existem localmente (aquisições
//! recentes ainda não sincronizadas) — nunca re-envia o que já está lá.

use crate::config::Config;
use crate::domain::{GameSource, LibraryLocation};
use crate::install_paths;
use crate::paths::{PathSafetyError, RemoteVault};
use crate::progress::ProgressUpdate;
use crate::remote::{self, RemoteError};
use crate::rsync::{RsyncTransfer, TransferError};
use crate::scan;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct IngestSummary {
    pub inspected: usize,
    pub uploaded: usize,
    pub skipped_existing: usize,
    /// Só incrementa quando `and_purge_local` está ativo — quantos jogos
    /// tiveram o compatdata (+ manifesto, se oficial) local apagado depois
    /// de um upload confirmado.
    pub purged_local: usize,
    /// Só quando `--full`: quantas instalações completas novas foram
    /// enviadas (as já presentes remotamente não contam aqui).
    pub full_uploaded: usize,
    /// Só quando `--full`: nomes dos jogos cuja instalação completa não
    /// pôde ser verificada em disco (metadado obsoleto — ver
    /// `install_paths.rs`) e por isso ficou de fora, sem apagar nada local
    /// pra eles mesmo com `--and-purge-local`.
    pub full_skipped: Vec<String>,
    /// Só quando `--full` e `local_game_root_steam` configurados: quantos
    /// manifestos de jogos Steam oficiais foram migrados (copiados) de uma
    /// biblioteca antiga pra `local_game_root_steam` — a "adoção" que
    /// deixa esse jogo permanentemente rastreado ali dali em diante.
    pub official_manifests_migrated: usize,
    /// Só quando `--full` e `--and-purge-local`: rótulos (título Epic ou
    /// `app_id — nome` Steam) de cada registro obsoleto removido —
    /// `installed.json` do Legendary (Epic) ou `appmanifest_<id>.acf`
    /// (Steam oficial) — por já estar confirmado 100% no FeuVault e sem
    /// nenhum vestígio local (prefixo Wine/compatdata) pra ele. Guardado
    /// por nome, não só contagem, depois de uma rodada real (2026-08-29)
    /// limpar 13 entradas de uma vez sem nenhum jeito de saber quais —
    /// forçava confiar cegamente no gate de segurança em vez de conseguir
    /// auditar o que mudou.
    pub stale_metadata_cleaned: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum IngestError {
    #[error(transparent)]
    PathSafety(#[from] PathSafetyError),
    #[error("falha ao consultar o compatdata remoto: {0}")]
    Remote(#[from] RemoteError),
    #[error("falha ao enviar app_id '{app_id}' ao servidor: {source}")]
    Upload {
        app_id: String,
        #[source]
        source: TransferError,
    },
    #[error("falha ao enviar a instalação completa de '{app_id}' ao servidor: {source}")]
    FullInstallUpload {
        app_id: String,
        #[source]
        source: TransferError,
    },
    #[error("upload de '{app_id}' concluído, mas falhou ao limpar o local: {source}")]
    Cleanup {
        app_id: String,
        #[source]
        source: std::io::Error,
    },
    #[error("'--full' exige `remote_installs_base_path` configurado em config.json")]
    FullNotConfigured,
    #[error("falha ao migrar o manifesto de '{app_id}' pra `local_game_root_steam`: {source}")]
    ManifestMigration {
        app_id: String,
        #[source]
        source: std::io::Error,
    },
    #[error(
        "falha ao limpar a entrada obsoleta de '{title}' no installed.json do Legendary: {source}"
    )]
    StaleMetadataCleanup {
        title: String,
        #[source]
        source: std::io::Error,
    },
}

/// Copia o `appmanifest_<app_id>.acf` de `local_lib` pra
/// `local_game_root_steam`, se ainda não estiver lá — o único dado que
/// precisa mesmo "passar pelo NVMe": a instalação grande e o compatdata
/// são enviados direto de onde já estão e apagados de lá (o destino
/// remoto não depende de qual biblioteca local os originou), nunca
/// precisam visitar `local_game_root_steam` fisicamente. Só o manifesto
/// precisa persistir lá — é a única memória do mapeamento
/// app_id→instalação que `run` consulta depois de adotado (ver
/// `session.rs::detect_full_install`). Devolve `true` só quando de fato
/// copiou algo novo.
/// Devolve `(migrated_now, old_manifest_path)` — `migrated_now` só `true`
/// quando a cópia foi feita nesta chamada (pra contagem no summary);
/// `old_manifest_path` é `Some` sempre que `local_lib` difere de
/// `local_game_root_steam` e o manifesto antigo ainda existe lá, mesmo que
/// a migração já tenha acontecido numa chamada anterior — quem chama usa
/// isso pra decidir se apaga o manifesto antigo (só com
/// `--and-purge-local`, nunca aqui dentro).
fn adopt_official_manifest(
    local_lib: &Path,
    local_game_root_steam: &Path,
    app_id: &str,
) -> std::io::Result<(bool, Option<PathBuf>)> {
    if local_lib == local_game_root_steam {
        return Ok((false, None));
    }
    let source = local_lib
        .join("steamapps")
        .join(format!("appmanifest_{app_id}.acf"));
    if !source.exists() {
        // Resolvido via override sem manifesto nenhum (ex.: jogo oficial
        // cujo `appmanifest` nunca existiu ou já foi apagado por fora) —
        // nada pra adotar, não é erro.
        return Ok((false, None));
    }
    let dest = local_game_root_steam
        .join("steamapps")
        .join(format!("appmanifest_{app_id}.acf"));
    let migrated_now = if dest.exists() {
        false
    } else {
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(&source, &dest)?;
        true
    };
    let old_manifest = source.exists().then_some(source);
    Ok((migrated_now, old_manifest))
}

/// Resultado da tentativa de sincronizar a instalação completa de um único
/// jogo — devolve o diretório local (pra `--and-purge-local` apagar depois)
/// só quando a instalação já está confirmada no remoto (recém-enviada ou já
/// presente). `None` sempre que o caminho não pôde ser verificado — nunca
/// um erro fatal, `ingest` segue pros próximos jogos.
#[allow(clippy::too_many_arguments)]
fn sync_full_install(
    ssh_target: &str,
    rsync_bin: &str,
    category: &str,
    remote_installs_base_path: &Path,
    // Não decide mais se o upload roda (ver fix 2026-09-06 acima) — mantido
    // só porque os chamadores já tinham o `HashSet` calculado pra decidir o
    // que reportar (`already_remote`), sem custo real em manter o parâmetro.
    _remote_known_names: &HashSet<String>,
    local_install_dir: &Path,
    app_id: &str,
    on_progress: &mut dyn FnMut(ProgressUpdate),
) -> Result<PathBuf, IngestError> {
    let name = local_install_dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| app_id.to_string());
    let remote_install_dir = remote_installs_base_path.join(category).join(&name);

    // Mesmo bug/fix do compatdata em `run_ingest_with_options` (ver
    // comentário lá, incidente 2026-09-06 Kenshi): `remote_known_names` só
    // diz que o NOME já existiu remotamente algum dia — não que o conteúdo
    // atual bate. Pular o upload aqui achando "já está lá" deixaria um
    // update/patch local (arquivos mudados desde a primeira ingestão) nunca
    // sincronizado, e `--and-purge-local` apagaria a única cópia com esse
    // update. `remote_known_names` continua servindo só pro chamador decidir
    // o que reportar (`full_uploaded` vs já presente), nunca pra decidir se
    // a chamada em si acontece.
    RsyncTransfer::incremental_upload(ssh_target, local_install_dir, &remote_install_dir)
        .with_binary(rsync_bin)
        .execute_with_progress(|u| on_progress(u))
        .map_err(|source| IngestError::FullInstallUpload {
            app_id: app_id.to_string(),
            source,
        })?;

    Ok(local_install_dir.to_path_buf())
}

/// Decide "já remoto ou upload novo" (compara o nome de `install_dir`
/// contra `known_names`, o retrato já consultado antes de qualquer laço
/// começar) e chama [`sync_full_install`], contabilizando o resultado em
/// `summary.full_uploaded`. Núcleo repetido nas 5 passadas de
/// `run_ingest_with_options` (CLEANUP.md, Camada 4.2) sempre que uma
/// instalação completa acaba de ser resolvida — só a categoria, o
/// conjunto de nomes conhecidos e o rótulo mudam entre elas; a resolução
/// em si (de onde vem `install_dir`) e o que fazer depois (rastrear pra
/// purga mais tarde vs. apagar na hora) continuam em cada passada, por
/// serem genuinamente diferentes por categoria.
#[allow(clippy::too_many_arguments)]
fn sync_full_install_and_report(
    ssh_target: &str,
    rsync_bin: &str,
    category: &str,
    remote_installs_base_path: &Path,
    known_names: &HashSet<String>,
    install_dir: &Path,
    id_or_title: &str,
    label: &str,
    summary: &mut IngestSummary,
    on_uploading: &mut dyn FnMut(&str),
    on_progress: &mut dyn FnMut(ProgressUpdate),
) -> Result<PathBuf, IngestError> {
    let already_remote = install_dir
        .file_name()
        .map(|n| known_names.contains(n.to_string_lossy().as_ref()))
        .unwrap_or(false);
    if !already_remote {
        on_uploading(&format!("{label} (instalação completa)"));
    }
    let synced = sync_full_install(
        ssh_target,
        rsync_bin,
        category,
        remote_installs_base_path,
        known_names,
        install_dir,
        id_or_title,
        on_progress,
    )?;
    summary.full_uploaded += usize::from(!already_remote);
    Ok(synced)
}

/// Um jogo cuja instalação completa não resolveu no disco (pasta ausente)
/// pode já estar migrada com sucesso — manifesto/override/`installed.json`
/// ficam obsoletos sozinhos (histórico documentado em cada passada de
/// `run_ingest_with_options`) depois de um `--and-purge-local` anterior;
/// pasta ausente não é o mesmo que "nunca instalado". Compara cada nome
/// candidato (de fontes diferentes por categoria — cada par nome/conjunto
/// é independente, por isso não é um único `known_names` compartilhado)
/// contra o que já está confirmado no FeuVault. `None` num candidato
/// (fonte não disponível, ex.: sem manifesto local) simplesmente não
/// contribui, sem precisar de um caso especial pra isso.
fn already_migrated(candidates: &[(Option<&str>, &HashSet<String>)]) -> bool {
    candidates
        .iter()
        .any(|(name, known_names)| name.is_some_and(|n| known_names.contains(n)))
}

/// Remove `install_dir` do disco se ele existir de verdade — devolve se
/// removeu algo (`false` quando já não havia nada ali, ex.: purgado numa
/// execução anterior). Usado depois de uma instalação completa já
/// sincronizada, quando `--and-purge-local` está ativo. `id_for_error` só
/// identifica a falha no `IngestError::Cleanup` devolvido, nunca decide o
/// que apagar.
fn purge_full_install_dir(install_dir: &Path, id_for_error: &str) -> Result<bool, IngestError> {
    if !install_dir.exists() {
        return Ok(false);
    }
    std::fs::remove_dir_all(install_dir).map_err(|source| IngestError::Cleanup {
        app_id: id_for_error.to_string(),
        source,
    })?;
    Ok(true)
}

/// Remove um único arquivo de manifesto se ele ainda existir — reaproveitado
/// tanto pro `appmanifest_<id>.acf` obsoleto (sem instalação nem compatdata
/// local pra ele) quanto pro manifesto antigo que sobra em `local_lib`
/// depois de `adopt_official_manifest` copiar pra `local_game_root_steam`
/// (`old_manifest_to_purge`). Mesmo princípio de `purge_full_install_dir`,
/// sem o valor de retorno booleano porque nenhuma passada precisa
/// condicionar mais nada a isso.
fn purge_old_manifest(old_manifest: &Path, id_for_error: &str) -> Result<(), IngestError> {
    if old_manifest.exists() {
        std::fs::remove_file(old_manifest).map_err(|source| IngestError::Cleanup {
            app_id: id_for_error.to_string(),
            source,
        })?;
    }
    Ok(())
}

/// Copia `appmanifest_<app_id>.acf` de `local_lib` pro FeuVault, num
/// namespace próprio (`<remote_installs_base_path>/steam-manifests/<app_id>.acf`,
/// separado da árvore `steam/<nome>` — o nome da pasta só é conhecido
/// depois de ler o manifesto, então chavear por app_id evita essa
/// dependência circular). Retrato usado por
/// `session::restore_official_manifest_snapshot` pra recuperar sozinho um
/// jogo cujo manifesto local sumiu — incidente real (2026-08-30/31): a
/// própria Steam limpou sozinha o manifesto fantasma do Kenshi, que
/// `run`/`ingest` deliberadamente preservam, mas nada garantia que ela
/// preservaria também. Universal: roda pra qualquer jogo Steam oficial
/// com instalação completa rastreada, não um caso especial.
///
/// Melhor esforço deliberado: sem manifesto local pra copiar (alguns jogos
/// rastreados só por override nunca tiveram um — ver `install_paths.rs`,
/// caso real do Echoes of Aincrad), ou falha de rede, só registra um aviso
/// — nunca derruba o `ingest` inteiro por causa de uma rede de segurança
/// auxiliar à instalação completa (essa sim, um erro duro).
fn upload_manifest_snapshot(
    ssh_target: &str,
    rsync_bin: &str,
    remote_installs_base_path: &Path,
    local_lib: &Path,
    app_id: &str,
) {
    let local_manifest = local_lib
        .join("steamapps")
        .join(format!("appmanifest_{app_id}.acf"));
    if !local_manifest.is_file() {
        return;
    }
    let remote_manifest = remote_installs_base_path
        .join("steam-manifests")
        .join(format!("{app_id}.acf"));
    if let Err(e) = RsyncTransfer::upload_single_file(ssh_target, &local_manifest, &remote_manifest)
        .with_binary(rsync_bin)
        .execute_with_progress(|_| {})
    {
        tracing::warn!(
            app_id,
            "falha ao salvar retrato do manifesto no servidor: {e}"
        );
    }
}

/// Salva no FeuVault um retrato do `installed.json` inteiro do Legendary
/// (não por título/app_id, ao contrário de `upload_manifest_snapshot`
/// pra Steam) — o arquivo já é pequeno mesmo com muitos jogos, e cada
/// entrada é indexada por um hash interno do Legendary que só o próprio
/// arquivo sabe resolver por título; recortar uma entrada só faria
/// sentido depois de já ter lido o arquivo inteiro mesmo. Incidente real
/// (2026-09-07, "Nobody Wants to Die"): sem isto, um jogo Heroic cujos
/// arquivos completos são restaurados do FeuVault (`run`, depois de um
/// `--and-purge-local` ou numa máquina nova) nunca teria como
/// `legendary launch` reconhecer como instalado — o `installed.json`
/// nunca soube da instalação completa que o vault-sync acabou de trazer
/// de volta, mesmo com os arquivos certinhos no disco. Ver o restaurador
/// em `session::ensure_legendary_knows_about_install_at`.
///
/// Melhor esforço deliberado: sem `installed.json` local (Heroic nunca
/// usado nesta máquina) ou falha de rede, só um aviso — nunca derruba o
/// `ingest` por causa de uma rede de segurança auxiliar.
fn upload_legendary_installed_json_snapshot(
    ssh_target: &str,
    rsync_bin: &str,
    remote_installs_base_path: &Path,
) {
    let Some(installed_json) = install_paths::default_legendary_installed_json_path() else {
        return;
    };
    if !installed_json.is_file() {
        return;
    }
    let remote_snapshot = remote_installs_base_path
        .join("epic-manifests")
        .join("installed.json");
    if let Err(e) = RsyncTransfer::upload_single_file(ssh_target, &installed_json, &remote_snapshot)
        .with_binary(rsync_bin)
        .execute_with_progress(|_| {})
    {
        tracing::warn!("falha ao salvar retrato do installed.json do Legendary no servidor: {e}");
    }
}

/// Ferramentas de compatibilidade (Proton e variantes, Steam Linux Runtime,
/// redistribuíveis) criam seu próprio `compatdata/<app_id>` por bookkeeping
/// interno, mas não são jogos: sua "instalação completa" é infraestrutura
/// compartilhada entre todos os jogos que a usam. Subir/apagar isso pelo
/// ciclo de vida de um único item quebraria os outros jogos que dependem
/// dela. Detecção por nome do diretório (sem consultar a Steam Store API,
/// mesma decisão documentada na Fase IV) — mas o nome que importa aqui é o
/// `installdir` físico (`steamapps/common/<installdir>`), não o `name` de
/// exibição do manifesto: os dois divergem pra essas ferramentas (ex.:
/// manifesto diz "Steam Linux Runtime 2.0 (soldier)", o diretório real é
/// `SteamLinuxRuntime_soldier`). Confirmado direto em disco 2026-08-15 —
/// a versão anterior deste heurístico comparava contra o nome de exibição
/// e deixava passar `SteamLinuxRuntime*`/`Steamworks Shared` pro FeuVault.
fn is_compat_tool_install(install_dir: &Path) -> bool {
    let Some(name) = install_dir.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let name = name.to_ascii_lowercase();
    name.starts_with("proton")
        || name.starts_with("steamlinuxruntime")
        || name == "steamworks shared"
}

#[cfg(test)]
mod compat_tool_install_tests {
    use super::is_compat_tool_install;
    use std::path::Path;

    #[test]
    fn recognizes_every_known_compat_tool_installdir() {
        for installdir in [
            "Proton - Experimental",
            "Proton 8.0",
            "Proton 9.0 (Beta)",
            "Proton 10.0",
            "SteamLinuxRuntime",
            "SteamLinuxRuntime_soldier",
            "SteamLinuxRuntime_sniper",
            "SteamLinuxRuntime_4",
            "Steamworks Shared",
        ] {
            assert!(
                is_compat_tool_install(Path::new(installdir)),
                "deveria reconhecer {installdir:?} como ferramenta de compatibilidade"
            );
        }
    }

    #[test]
    fn does_not_flag_real_games() {
        for installdir in [
            "Kenshi",
            "hotline_miami",
            "Buckshot Roulette",
            "Cyberpunk 2077",
        ] {
            assert!(
                !is_compat_tool_install(Path::new(installdir)),
                "não deveria reconhecer {installdir:?} como ferramenta de compatibilidade"
            );
        }
    }
}

/// Rótulo pra UI/log: `app_id` sozinho não diz nada a olho nu. Resolve o
/// nome pelo `appmanifest_<id>.acf` da própria biblioteca (jogos oficiais)
/// e, se ausente, pelo `shortcuts.vdf` (jogos non-Steam/piratas) — mesmas
/// fontes que `scan.rs` já usa em outros comandos. Sem nenhuma das duas,
/// cai de volta pro `app_id` puro (comportamento anterior).
fn display_label(
    local_lib: &Path,
    shortcuts_names: &std::collections::HashMap<u32, String>,
    app_id: &str,
) -> String {
    let name = scan::official_game_name(local_lib, app_id).or_else(|| {
        let numeric: u32 = app_id.parse().ok()?;
        shortcuts_names.get(&numeric).cloned()
    });
    match name {
        Some(name) => format!("{app_id} — {name}"),
        None => app_id.to_string(),
    }
}

/// Pré-conta quantas entradas Steam (`compatdata/<app_id>`) vão ser
/// visitadas pelo laço de verdade, com os mesmos filtros (dígito-only,
/// `--only`) — permite reportar progresso real (X de Y) antes de começar,
/// sem restructurar o laço principal pra coletar tudo antecipadamente.
fn count_matching_steam_entries(
    local_libraries: &[PathBuf],
    only: Option<&str>,
    excluded: &HashSet<String>,
) -> usize {
    local_libraries
        .iter()
        .map(|lib| {
            let compatdata = lib.join("steamapps").join("compatdata");
            std::fs::read_dir(&compatdata)
                .map(|entries| {
                    entries
                        .flatten()
                        .filter(|entry| {
                            let app_id = entry.file_name().to_string_lossy().into_owned();
                            if app_id.is_empty() || !app_id.chars().all(|c| c.is_ascii_digit()) {
                                return false;
                            }
                            if excluded.contains(&app_id) {
                                return false;
                            }
                            only.is_none_or(|o| o == app_id)
                        })
                        .count()
                })
                .unwrap_or(0)
        })
        .sum()
}

/// App IDs Steam oficiais com `appmanifest_<id>.acf` mas SEM
/// `compatdata/<id>` correspondente — jogos nativos Linux (nunca passam
/// pelo Proton, logo nunca geram compatdata) ou qualquer instalação
/// oficial nunca lançada. Invisíveis ao laço principal, que descobre tudo
/// por `compatdata/`; só processados com `--full`, já que sem compatdata
/// não sobra nada além da instalação em si pra sincronizar.
fn manifest_only_entries(
    local_lib: &Path,
    only: Option<&str>,
    excluded: &HashSet<String>,
) -> Vec<String> {
    let compatdata = local_lib.join("steamapps").join("compatdata");
    let Ok(entries) = std::fs::read_dir(local_lib.join("steamapps")) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let app_id = name.strip_prefix("appmanifest_")?.strip_suffix(".acf")?;
            if app_id.is_empty() || !app_id.chars().all(|c| c.is_ascii_digit()) {
                return None;
            }
            if excluded.contains(app_id) {
                return None;
            }
            if compatdata.join(app_id).exists() {
                return None;
            }
            if only.is_some_and(|o| o != app_id) {
                return None;
            }
            Some(app_id.to_string())
        })
        .collect()
}

/// Idem, pros títulos Heroic sob `heroic_local_root`.
fn count_matching_heroic_entries(heroic_local_root: Option<&Path>, only: Option<&str>) -> usize {
    let Some(root) = heroic_local_root else {
        return 0;
    };
    std::fs::read_dir(root)
        .map(|entries| {
            entries
                .flatten()
                .filter(|entry| {
                    if !entry.path().is_dir() {
                        return false;
                    }
                    let title = entry.file_name().to_string_lossy().into_owned();
                    only.is_none_or(|o| o == title)
                })
                .count()
        })
        .unwrap_or(0)
}

/// App IDs de atalhos non-Steam (`shortcuts.vdf`) sem `compatdata/<id>`
/// correspondente em NENHUMA biblioteca local — jogo pirata nativo Linux,
/// ou qualquer atalho nunca lançado. `shortcuts.vdf` lista todo atalho
/// cadastrado independente de ter sido executado, então é fonte de
/// descoberta completa por si só (ao contrário de `compatdata/`, que só
/// existe depois do primeiro lançamento via Proton).
/// App_ids piratas com override configurado (`install_path_overrides.
/// piratas`) mas sem `compatdata` correspondente em nenhuma biblioteca
/// local — jogo nativo Linux, ou nunca lançado ainda. Deliberadamente NÃO
/// descoberto via `shortcuts.vdf` (ver comentário no laço que chama isto)
/// — só o override é fonte de descoberta aqui, então sem override
/// configurado o jogo é invisível a esta função de propósito.
fn pirata_manifest_only_entries(
    local_libraries: &[PathBuf],
    piratas_overrides: &std::collections::HashMap<String, PathBuf>,
    only: Option<&str>,
    excluded: &HashSet<String>,
) -> Vec<String> {
    piratas_overrides
        .keys()
        .filter(|app_id| {
            if excluded.contains(app_id.as_str()) {
                return false;
            }
            if only.is_some_and(|o| o != app_id.as_str()) {
                return false;
            }
            !local_libraries.iter().any(|lib| {
                lib.join("steamapps")
                    .join("compatdata")
                    .join(app_id)
                    .exists()
            })
        })
        .cloned()
        .collect()
}

/// Títulos do `installed.json` do Legendary sem prefixo Wine
/// correspondente em `heroic_local_root` — jogo Epic nativo Linux (nunca
/// precisa do Wine, logo nunca gera prefixo) ou qualquer título nunca
/// lançado. Mesmo princípio de `pirata_manifest_only_entries`: a fonte de
/// descoberta (`installed.json`) é completa por si só, o prefixo é que é
/// opcional.
fn epic_manifest_only_entries(
    heroic_local_root: &Path,
    installed_json: &Path,
    only: Option<&str>,
) -> Vec<String> {
    install_paths::list_legendary_installed_titles(installed_json)
        .into_iter()
        .filter(|title| {
            if only.is_some_and(|o| o != title) {
                return false;
            }
            !heroic_local_root.join(title).exists()
        })
        .collect()
}

/// Apaga o compatdata (+ manifesto, se presente) local de `targets`, só
/// depois de um upload já confirmado — mesma operação que `purge.rs` faz,
/// reaproveitada aqui pro `--and-purge-local` do `ingest`.
fn delete_local_targets(targets: &crate::paths::LocalDeletionTargets) -> std::io::Result<()> {
    if targets.compatdata_dir.exists() {
        std::fs::remove_dir_all(&targets.compatdata_dir)?;
    }
    if let Some(manifest) = &targets.manifest_file {
        if manifest.exists() {
            std::fs::remove_file(manifest)?;
        }
    }
    Ok(())
}

/// Varre `cfg.local_libraries`, compara com o `compatdata/` remoto (uma
/// única consulta SSH) e envia via `RsyncTransfer::incremental_upload` tudo
/// que só existe localmente. `on_uploading` é chamado antes de cada envio,
/// para o chamador (CLI) logar o app_id em progresso. `on_item` é chamado
/// uma vez por item VISITADO (enviado ou já presente) com
/// `(posição_1_based, total, nome)` — permite ao chamador (CLI) reportar
/// progresso real (ex.: "3 de 12", 25%) em vez de só "processando algo".
#[allow(clippy::too_many_arguments)]
pub fn run_ingest(
    cfg: &Config,
    full: bool,
    and_purge_local: bool,
    steam_root: Option<&Path>,
    only: Option<&str>,
    on_uploading: impl FnMut(&str),
    on_item: impl FnMut(usize, usize, &str),
    on_progress: impl FnMut(ProgressUpdate),
) -> Result<IngestSummary, IngestError> {
    run_ingest_with_options(
        cfg,
        full,
        and_purge_local,
        steam_root,
        only,
        "rsync",
        "ssh",
        on_uploading,
        on_item,
        on_progress,
    )
}

#[allow(clippy::too_many_arguments)]
fn run_ingest_with_options(
    cfg: &Config,
    full: bool,
    and_purge_local: bool,
    steam_root: Option<&Path>,
    only: Option<&str>,
    rsync_bin: &str,
    ssh_bin: &str,
    mut on_uploading: impl FnMut(&str),
    mut on_item: impl FnMut(usize, usize, &str),
    mut on_progress: impl FnMut(ProgressUpdate),
) -> Result<IngestSummary, IngestError> {
    // Resolvidos uma única vez (decodifica todo `shortcuts.vdf`/localiza o
    // `installed.json`) em vez de repetir a cada item dos laços abaixo —
    // também usados já aqui embaixo pra pré-contar os itens "só instalação,
    // sem save/prefixo local" descobertos independente de compatdata.
    let shortcuts_names: std::collections::HashMap<u32, String> = steam_root
        .map(scan::parse_shortcuts_map)
        .unwrap_or_default();
    let legendary_installed_json = install_paths::default_legendary_installed_json_path();
    // App_id "0" (bookkeeping compartilhado da própria Steam) e IDs de
    // ferramentas de compatibilidade não precisam entrar aqui — são
    // reconhecidos automaticamente via `AppData/Local` vazio (ver
    // `install_paths::looks_like_empty_compat_tool_prefix`), sem precisar
    // de lista nenhuma. Só o que o usuário listou explicitamente em
    // `ingest_exclude_steam` (ver doc do campo em config.rs — caso real:
    // Zenless Zone Zero via Lutris com `StartDir=$HOME`, que a guarda de
    // segurança nunca resolveria de forma segura mesmo com override).
    let excluded_steam: HashSet<String> = cfg.ingest_exclude_steam.clone();

    let total_items = count_matching_steam_entries(&cfg.local_libraries, only, &excluded_steam)
        + count_matching_heroic_entries(cfg.heroic_local_root.as_deref(), only)
        + if full {
            cfg.local_libraries
                .iter()
                .map(|lib| manifest_only_entries(lib, only, &excluded_steam).len())
                .sum::<usize>()
                + pirata_manifest_only_entries(
                    &cfg.local_libraries,
                    &cfg.install_path_overrides.piratas,
                    only,
                    &excluded_steam,
                )
                .len()
                + match (
                    cfg.heroic_local_root.as_deref(),
                    legendary_installed_json.as_deref(),
                ) {
                    (Some(root), Some(json)) => epic_manifest_only_entries(root, json, only).len(),
                    _ => 0,
                }
        } else {
            0
        };
    let mut processed = 0usize;
    let vault = RemoteVault {
        ssh_target: cfg.remote_ssh_target.clone(),
        base_path: cfg.remote_base_path.clone(),
    };
    let remote_compatdata_dir = vault.base_path.join("steamapps").join("compatdata");
    let remote_entries: HashSet<String> =
        remote::list_remote_dir_entries(ssh_bin, &vault.ssh_target, &remote_compatdata_dir)?
            .into_iter()
            .collect();

    // Só consultado quando `--full` está ativo — dois diretórios remotos
    // separados (oficiais vs. piratas) sob a mesma raiz, já que os dois
    // namespaces de nome podem colidir (ex.: dois jogos diferentes com o
    // mesmo `installdir`/pasta de atalho não é esperado, mas manter os
    // conjuntos separados custa uma consulta SSH a mais e evita qualquer
    // ambiguidade).
    let full_install_remote: Option<(&Path, HashSet<String>, HashSet<String>)> = if full {
        let base = cfg
            .remote_installs_base_path
            .as_deref()
            .ok_or(IngestError::FullNotConfigured)?;
        let steam_names: HashSet<String> =
            remote::list_remote_dir_entries(ssh_bin, &vault.ssh_target, &base.join("steam"))?
                .into_iter()
                .collect();
        let piratas_names: HashSet<String> =
            remote::list_remote_dir_entries(ssh_bin, &vault.ssh_target, &base.join("piratas"))?
                .into_iter()
                .collect();
        Some((base, steam_names, piratas_names))
    } else {
        None
    };

    let mut summary = IngestSummary::default();
    // App IDs Steam (oficiais e non-Steam) já visitados no primeiro laço
    // abaixo, nesta mesma execução — cobre tanto `manifest_only_entries`
    // quanto `pirata_manifest_only_entries` mais adiante. Necessário porque,
    // com `--and-purge-local`, o primeiro laço apaga `compatdata/<app_id>`
    // do jogo assim que termina de processá-lo; sem este registro, as
    // passadas "sem compatdata" mais abaixo releem o disco DEPOIS dessa
    // exclusão e enxergam o mesmo app_id como "nunca lançado" — como a
    // instalação completa já foi movida (e, sem override, o manifesto ainda
    // não migrado é a única fonte pra resolvê-la) pelo primeiro laço, a
    // resolução falha de novo e o jogo aparece falsamente em
    // `full_skipped`, mesmo tendo sido migrado com sucesso segundos antes.
    let processed_steam_app_ids = run_steam_compatdata_pass(
        cfg,
        full,
        and_purge_local,
        only,
        rsync_bin,
        &shortcuts_names,
        &excluded_steam,
        &vault,
        &full_install_remote,
        &remote_entries,
        total_items,
        &mut processed,
        &mut summary,
        &mut on_uploading,
        &mut on_item,
        &mut on_progress,
    )?;

    run_steam_manifest_only_pass(
        cfg,
        full,
        and_purge_local,
        only,
        rsync_bin,
        &shortcuts_names,
        &excluded_steam,
        &vault,
        &full_install_remote,
        &processed_steam_app_ids,
        total_items,
        &mut processed,
        &mut summary,
        &mut on_uploading,
        &mut on_item,
        &mut on_progress,
    )?;

    run_pirata_manifest_only_pass(
        cfg,
        full,
        and_purge_local,
        only,
        rsync_bin,
        &shortcuts_names,
        &excluded_steam,
        &vault,
        &full_install_remote,
        &processed_steam_app_ids,
        total_items,
        &mut processed,
        &mut summary,
        &mut on_uploading,
        &mut on_item,
        &mut on_progress,
    )?;

    run_epic_pass(
        cfg,
        full,
        and_purge_local,
        only,
        rsync_bin,
        ssh_bin,
        &vault,
        &full_install_remote,
        legendary_installed_json.as_deref(),
        total_items,
        &mut processed,
        &mut summary,
        &mut on_uploading,
        &mut on_item,
        &mut on_progress,
    )?;

    Ok(summary)
}

/// Primeira passada: percorre `compatdata/<app_id>` em cada biblioteca
/// local (Steam oficial e piratas juntos — a categoria só é decidida
/// dentro do laço, por qual override/manifesto resolve). Devolve os
/// app_ids já vistos aqui nesta execução, pra `run_steam_manifest_only_
/// pass`/`run_pirata_manifest_only_pass` não reprocessarem (com
/// `--and-purge-local`, este laço já pode ter apagado o compatdata e a
/// instalação completa de um app_id antes das passadas seguintes lerem o
/// disco de novo).
#[allow(clippy::too_many_arguments)]
fn run_steam_compatdata_pass(
    cfg: &Config,
    full: bool,
    and_purge_local: bool,
    only: Option<&str>,
    rsync_bin: &str,
    shortcuts_names: &std::collections::HashMap<u32, String>,
    excluded_steam: &HashSet<String>,
    vault: &RemoteVault,
    full_install_remote: &Option<(&Path, HashSet<String>, HashSet<String>)>,
    remote_entries: &HashSet<String>,
    total_items: usize,
    processed: &mut usize,
    summary: &mut IngestSummary,
    on_uploading: &mut dyn FnMut(&str),
    on_item: &mut dyn FnMut(usize, usize, &str),
    on_progress: &mut dyn FnMut(ProgressUpdate),
) -> Result<HashSet<String>, IngestError> {
    let mut processed_steam_app_ids: HashSet<String> = HashSet::new();

    for local_lib in &cfg.local_libraries {
        let library = LibraryLocation::new(local_lib.clone());
        let compatdata = local_lib.join("steamapps").join("compatdata");
        let Ok(entries) = std::fs::read_dir(&compatdata) else {
            continue;
        };
        for entry in entries.flatten() {
            let app_id = entry.file_name().to_string_lossy().into_owned();
            if app_id.is_empty() || !app_id.chars().all(|c| c.is_ascii_digit()) {
                continue;
            }
            if excluded_steam.contains(&app_id) {
                continue;
            }
            if only.is_some_and(|o| o != app_id) {
                continue;
            }
            processed_steam_app_ids.insert(app_id.clone());
            let label = display_label(local_lib, shortcuts_names, &app_id);
            *processed += 1;
            on_item(*processed, total_items, &label);
            summary.inspected += 1;

            // Incidente real (2026-09-06, Kenshi): antes, `compatdata_already_remote`
            // (mera checagem de existência da entrada remota) decidia se o
            // upload sequer era TENTADO — uma vez que o app_id apareceu no
            // FeuVault (mesmo que fosse a primeira ingestão, semanas atrás),
            // toda ingestão seguinte pulava o upload achando "já está lá",
            // sem nunca comparar conteúdo. `--and-purge-local` então apagava
            // o local confiando nessa bandeira congelada — resultado: 2
            // semanas de progresso real (autosaves mais novos que a cópia
            // remota) apagadas sem nunca terem sido sincronizadas de volta.
            // `RsyncTransfer::incremental_upload` já é incremental/idempotente
            // (não custa nada re-rodar quando não há nada novo) — não existe
            // motivo pra pular a chamada em si, só pra reportar diferente no
            // resumo (`uploaded` vs `skipped_existing`).
            let compatdata_already_remote = remote_entries.contains(&app_id);
            // GameSource usado só como veículo de caminho (mesmo truque
            // de scan.rs/paths.rs) — ingest não classifica a origem do
            // jogo pra fins de compatdata, só compara conjuntos de
            // app_ids locais x remotos.
            let source = GameSource::Orphan {
                app_id: app_id.clone(),
                clues: vec![],
            };
            let local_targets = library.deletion_targets(&source)?;
            let remote_paths = vault.paths_for(&source)?;

            on_uploading(&label);
            RsyncTransfer::incremental_upload(
                &vault.ssh_target,
                &local_targets.compatdata_dir,
                &remote_paths.compatdata_dir,
            )
            .with_binary(rsync_bin)
            .execute_with_progress(|u| on_progress(u))
            .map_err(|source| IngestError::Upload {
                app_id: app_id.clone(),
                source,
            })?;

            if compatdata_already_remote {
                summary.skipped_existing += 1;
            } else {
                summary.uploaded += 1;
            }

            // Resolução da instalação completa é independente de o
            // compatdata já estar remoto ou não — um jogo pode ter sido
            // ingerido (só compatdata) antes desta funcionalidade existir.
            let mut local_install_dir_for_purge: Option<PathBuf> = None;
            let mut old_manifest_to_purge: Option<PathBuf> = None;
            let full_install_ok = if !full {
                true
            } else {
                let steam_override = cfg.install_path_overrides.steam.get(&app_id);
                let piratas_override = cfg.install_path_overrides.piratas.get(&app_id);
                let resolved = install_paths::resolve_official_install_dir(
                    local_lib,
                    &app_id,
                    steam_override.map(|p| p.as_path()),
                )
                .map(|dir| ("steam", dir))
                .or_else(|| {
                    // Só via override explícito, nunca via `StartDir` do
                    // `shortcuts.vdf` (ver doc de `verified_override`) —
                    // sem override, este jogo pirata simplesmente não entra
                    // no rastreamento de instalação completa, silenciosamente.
                    install_paths::verified_override(piratas_override.map(|p| p.as_path()))
                        .map(|dir| ("piratas", dir))
                });

                match resolved {
                    Some((_, install_dir)) if is_compat_tool_install(&install_dir) => {
                        // Ferramenta de compatibilidade, não jogo: compatdata
                        // já foi sincronizado acima (bookkeeping do próprio
                        // Proton); nunca sobe/apaga a instalação completa —
                        // ver `is_compat_tool_install`.
                        true
                    }
                    Some((category, install_dir)) => {
                        if category == "steam" {
                            if let Some(root) = &cfg.local_game_root_steam {
                                let (migrated_now, old_manifest) = adopt_official_manifest(
                                    local_lib, root, &app_id,
                                )
                                .map_err(|source| IngestError::ManifestMigration {
                                    app_id: app_id.clone(),
                                    source,
                                })?;
                                summary.official_manifests_migrated += usize::from(migrated_now);
                                old_manifest_to_purge = old_manifest;
                            }
                        }
                        let (base, steam_names, piratas_names) = full_install_remote
                            .as_ref()
                            .expect("full=true garante Some");
                        let known_names = if category == "steam" {
                            steam_names
                        } else {
                            piratas_names
                        };
                        let synced = sync_full_install_and_report(
                            &vault.ssh_target,
                            rsync_bin,
                            category,
                            base,
                            known_names,
                            &install_dir,
                            &app_id,
                            &label,
                            summary,
                            on_uploading,
                            on_progress,
                        )?;
                        if category == "steam" {
                            upload_manifest_snapshot(
                                &vault.ssh_target,
                                rsync_bin,
                                base,
                                local_lib,
                                &app_id,
                            );
                        }
                        local_install_dir_for_purge = Some(synced);
                        true
                    }
                    None => {
                        // Bookkeeping da própria Steam (app_id "0") ou de
                        // ferramenta de compatibilidade (Proton, builds
                        // locais como CachyOS/UMU-Proton) nunca teve jogo
                        // nenhum rodando ali — reconhecido automaticamente
                        // (sem lista de exclusão, sem depender de nome de
                        // ferramenta) via `AppData/Local` vazio. Nem
                        // reportado.
                        if install_paths::looks_like_empty_compat_tool_prefix(
                            &compatdata.join(&app_id),
                        ) {
                            continue;
                        }
                        // Manifesto oficial e override continuam apontando
                        // pro nome certo mesmo depois da instalação já ter
                        // sido migrada e apagada (por este mesmo laço, numa
                        // execução anterior) — só a pasta em si some. Sem
                        // checar isso aqui também (não só nas passadas "sem
                        // compatdata"), um jogo com override configurado
                        // reaparecia como "pulado" pra sempre assim que
                        // `--and-purge-local` apagasse a pasta que o
                        // próprio override aponta.
                        let (_, steam_names, piratas_names) = full_install_remote
                            .as_ref()
                            .expect("full=true garante Some");
                        let steam_name = install_paths::manifest_installdir(local_lib, &app_id);
                        let piratas_override_name = piratas_override
                            .and_then(|p| p.file_name())
                            .map(|n| n.to_string_lossy().into_owned());
                        let already_migrated = already_migrated(&[
                            (steam_name.as_deref(), steam_names),
                            (piratas_override_name.as_deref(), piratas_names),
                        ]);
                        if !already_migrated {
                            summary.full_skipped.push(label.clone());
                        }
                        false
                    }
                }
            };

            if and_purge_local && full_install_ok {
                let source = GameSource::Orphan {
                    app_id: app_id.clone(),
                    clues: vec![],
                };
                let local_targets = library.deletion_targets(&source)?;
                delete_local_targets(&local_targets).map_err(|source| IngestError::Cleanup {
                    app_id: app_id.clone(),
                    source,
                })?;
                if let Some(install_dir) = &local_install_dir_for_purge {
                    purge_full_install_dir(install_dir, &app_id)?;
                }
                if let Some(old_manifest) = &old_manifest_to_purge {
                    purge_old_manifest(old_manifest, &app_id)?;
                }
                summary.purged_local += 1;
            }
        }
    }

    Ok(processed_steam_app_ids)
}

/// Segunda passada, só com `--full`: jogos oficiais instalados sem
/// compatdata (nativos Linux) — o laço acima nunca os visita porque
/// descobre tudo por `compatdata/`.
#[allow(clippy::too_many_arguments)]
fn run_steam_manifest_only_pass(
    cfg: &Config,
    full: bool,
    and_purge_local: bool,
    only: Option<&str>,
    rsync_bin: &str,
    shortcuts_names: &std::collections::HashMap<u32, String>,
    excluded_steam: &HashSet<String>,
    vault: &RemoteVault,
    full_install_remote: &Option<(&Path, HashSet<String>, HashSet<String>)>,
    processed_steam_app_ids: &HashSet<String>,
    total_items: usize,
    processed: &mut usize,
    summary: &mut IngestSummary,
    on_uploading: &mut dyn FnMut(&str),
    on_item: &mut dyn FnMut(usize, usize, &str),
    on_progress: &mut dyn FnMut(ProgressUpdate),
) -> Result<(), IngestError> {
    if full {
        let (base, steam_names, _piratas_names) = full_install_remote
            .as_ref()
            .expect("full=true garante Some");
        for local_lib in &cfg.local_libraries {
            for app_id in manifest_only_entries(local_lib, only, excluded_steam) {
                if processed_steam_app_ids.contains(&app_id) {
                    continue;
                }
                let label = display_label(local_lib, shortcuts_names, &app_id);
                *processed += 1;
                on_item(*processed, total_items, &label);
                summary.inspected += 1;

                let steam_override = cfg.install_path_overrides.steam.get(&app_id);
                let resolved = install_paths::resolve_official_install_dir(
                    local_lib,
                    &app_id,
                    steam_override.map(|p| p.as_path()),
                );

                let mut local_install_dir_for_purge: Option<PathBuf> = None;
                let mut old_manifest_to_purge: Option<PathBuf> = None;
                let full_install_ok = match resolved {
                    Some(install_dir) if is_compat_tool_install(&install_dir) => true,
                    Some(install_dir) => {
                        if let Some(root) = &cfg.local_game_root_steam {
                            let (migrated_now, old_manifest) = adopt_official_manifest(
                                local_lib, root, &app_id,
                            )
                            .map_err(|source| IngestError::ManifestMigration {
                                app_id: app_id.clone(),
                                source,
                            })?;
                            summary.official_manifests_migrated += usize::from(migrated_now);
                            old_manifest_to_purge = old_manifest;
                        }
                        let synced = sync_full_install_and_report(
                            &vault.ssh_target,
                            rsync_bin,
                            "steam",
                            base,
                            steam_names,
                            &install_dir,
                            &app_id,
                            &label,
                            summary,
                            on_uploading,
                            on_progress,
                        )?;
                        upload_manifest_snapshot(
                            &vault.ssh_target,
                            rsync_bin,
                            base,
                            local_lib,
                            &app_id,
                        );
                        local_install_dir_for_purge = Some(synced);
                        true
                    }
                    None => {
                        // Pasta ausente não é necessariamente "nunca
                        // lançado": o manifesto oficial retido em
                        // `local_game_root_steam` (bookkeeping deliberado
                        // da Fase VII — ver `install_paths::
                        // manifest_installdir`) reaparece aqui pra sempre
                        // depois de uma migração bem-sucedida, já que
                        // `compatdata/` some mas o manifesto não. Sem essa
                        // checagem, todo `ingest --full` seguinte reportava
                        // de novo (falsamente) qualquer jogo já migrado com
                        // sucesso em execuções anteriores.
                        let steam_name = install_paths::manifest_installdir(local_lib, &app_id);
                        let already_migrated =
                            already_migrated(&[(steam_name.as_deref(), steam_names)]);
                        if !already_migrated {
                            summary.full_skipped.push(label.clone());
                        } else if and_purge_local {
                            // Instalação completa já confirmada no FeuVault
                            // e sem `compatdata/` local pra este app_id (é
                            // assim que chegou aqui — ver
                            // `manifest_only_entries`): o
                            // `appmanifest_<id>.acf` que sobrou é só
                            // metadado obsoleto, sozinho ele deixa a Steam
                            // mostrando o jogo como instalado/corrompido
                            // por engano.
                            let manifest = local_lib
                                .join("steamapps")
                                .join(format!("appmanifest_{app_id}.acf"));
                            if manifest.exists() {
                                std::fs::remove_file(&manifest).map_err(|source| {
                                    IngestError::Cleanup {
                                        app_id: app_id.clone(),
                                        source,
                                    }
                                })?;
                                summary.stale_metadata_cleaned.push(label.clone());
                            }
                        }
                        false
                    }
                };

                if and_purge_local && full_install_ok {
                    if let Some(install_dir) = &local_install_dir_for_purge {
                        purge_full_install_dir(install_dir, &app_id)?;
                    }
                    let manifest = local_lib
                        .join("steamapps")
                        .join(format!("appmanifest_{app_id}.acf"));
                    purge_old_manifest(&manifest, &app_id)?;
                    if let Some(old_manifest) = &old_manifest_to_purge {
                        purge_old_manifest(old_manifest, &app_id)?;
                    }
                    summary.purged_local += 1;
                }
            }
        }
    }

    Ok(())
}

/// Terceira passada, só com `--full`: jogos piratas com override
/// configurado mas sem `compatdata` — nativo Linux, ou nunca lançado
/// ainda. Deliberadamente NUNCA descoberto via `shortcuts.vdf`: o
/// `StartDir` de um atalho non-Steam é só o que o usuário digitou uma
/// vez ao criar o atalho, nunca atualizado depois — não é uma fonte
/// confiável de "onde o jogo está" (diferente do manifesto Steam ou do
/// `installed.json` do Legendary, mantidos pelas próprias plataformas).
/// Só o override explícito do usuário é. Sem override, o jogo
/// simplesmente não entra aqui — nem tentado, nem reportado.
#[allow(clippy::too_many_arguments)]
fn run_pirata_manifest_only_pass(
    cfg: &Config,
    full: bool,
    and_purge_local: bool,
    only: Option<&str>,
    rsync_bin: &str,
    shortcuts_names: &std::collections::HashMap<u32, String>,
    excluded_steam: &HashSet<String>,
    vault: &RemoteVault,
    full_install_remote: &Option<(&Path, HashSet<String>, HashSet<String>)>,
    processed_steam_app_ids: &HashSet<String>,
    total_items: usize,
    processed: &mut usize,
    summary: &mut IngestSummary,
    on_uploading: &mut dyn FnMut(&str),
    on_item: &mut dyn FnMut(usize, usize, &str),
    on_progress: &mut dyn FnMut(ProgressUpdate),
) -> Result<(), IngestError> {
    if full {
        let (base, _steam_names, piratas_names) = full_install_remote
            .as_ref()
            .expect("full=true garante Some");
        for app_id in pirata_manifest_only_entries(
            &cfg.local_libraries,
            &cfg.install_path_overrides.piratas,
            only,
            excluded_steam,
        ) {
            if processed_steam_app_ids.contains(&app_id) {
                continue;
            }
            let label = app_id
                .parse::<u32>()
                .ok()
                .and_then(|numeric| shortcuts_names.get(&numeric))
                .map(|name| format!("{app_id} — {name}"))
                .unwrap_or_else(|| app_id.clone());
            *processed += 1;
            on_item(*processed, total_items, &label);
            summary.inspected += 1;

            let override_path = cfg.install_path_overrides.piratas.get(&app_id);
            let resolved = install_paths::verified_override(override_path.map(|p| p.as_path()));

            match resolved {
                Some(install_dir) if is_compat_tool_install(&install_dir) => {}
                Some(install_dir) => {
                    sync_full_install_and_report(
                        &vault.ssh_target,
                        rsync_bin,
                        "piratas",
                        base,
                        piratas_names,
                        &install_dir,
                        &app_id,
                        &label,
                        summary,
                        on_uploading,
                        on_progress,
                    )?;
                    if and_purge_local && install_dir.exists() {
                        std::fs::remove_dir_all(&install_dir).map_err(|source| {
                            IngestError::Cleanup {
                                app_id: app_id.clone(),
                                source,
                            }
                        })?;
                        summary.purged_local += 1;
                    }
                }
                None => {
                    // O override existe (senão o jogo nem chegaria aqui,
                    // ver `pirata_manifest_only_entries`) mas a pasta que
                    // ele aponta não existe mais — provavelmente já
                    // migrada e apagada por este mesmo `--and-purge-local`
                    // numa execução anterior. Checa o nome contra o que já
                    // está remoto antes de desistir.
                    let override_name = override_path
                        .and_then(|p| p.file_name())
                        .map(|n| n.to_string_lossy().into_owned());
                    let already_migrated =
                        already_migrated(&[(override_name.as_deref(), piratas_names)]);
                    if !already_migrated {
                        summary.full_skipped.push(label.clone());
                    }
                }
            }
        }
    }

    Ok(())
}

/// Quarta e quinta passadas (agrupadas: vivem sob a mesma configuração
/// `--heroic`): títulos Heroic com prefixo Wine (compatdata, mesmo
/// princípio de `run_steam_compatdata_pass`) e, só com `--full`, títulos
/// Epic sem prefixo (`installed.json` como fonte de descoberta completa,
/// mesmo princípio das passadas "manifest-only" de Steam/piratas).
#[allow(clippy::too_many_arguments)]
fn run_epic_pass(
    cfg: &Config,
    full: bool,
    and_purge_local: bool,
    only: Option<&str>,
    rsync_bin: &str,
    ssh_bin: &str,
    vault: &RemoteVault,
    full_install_remote: &Option<(&Path, HashSet<String>, HashSet<String>)>,
    legendary_installed_json: Option<&Path>,
    total_items: usize,
    processed: &mut usize,
    summary: &mut IngestSummary,
    on_uploading: &mut dyn FnMut(&str),
    on_item: &mut dyn FnMut(usize, usize, &str),
    on_progress: &mut dyn FnMut(ProgressUpdate),
) -> Result<(), IngestError> {
    if let (Some(heroic_local_root), Some(heroic_remote_base_path)) =
        (&cfg.heroic_local_root, &cfg.heroic_remote_base_path)
    {
        let heroic_vault = RemoteVault {
            ssh_target: cfg.remote_ssh_target.clone(),
            base_path: heroic_remote_base_path.clone(),
        };
        let heroic_remote_entries: HashSet<String> = remote::list_remote_dir_entries(
            ssh_bin,
            &heroic_vault.ssh_target,
            heroic_remote_base_path,
        )?
        .into_iter()
        .collect();

        let epic_remote: Option<(&Path, HashSet<String>)> = if full {
            let (base, _, _) = full_install_remote
                .as_ref()
                .expect("full=true garante Some");
            let epic_names: HashSet<String> =
                remote::list_remote_dir_entries(ssh_bin, &vault.ssh_target, &base.join("epic"))?
                    .into_iter()
                    .collect();
            Some((*base, epic_names))
        } else {
            None
        };

        // Retrato do `installed.json` inteiro do Legendary, uma vez por
        // `ingest` (não por título) — ver `upload_legendary_installed_json_snapshot`
        // e o restaurador em `session::ensure_legendary_knows_about_install_at`.
        if full {
            if let Some(remote_installs) = &cfg.remote_installs_base_path {
                upload_legendary_installed_json_snapshot(
                    &vault.ssh_target,
                    rsync_bin,
                    remote_installs,
                );
            }
        }

        let library = LibraryLocation::new(heroic_local_root.clone());
        // Mesmo motivo de `processed_steam_app_ids` acima: sem isso,
        // `epic_manifest_only_entries` mais adiante releria o disco depois
        // do laço abaixo apagar o prefixo Wine do título (`--and-purge-
        // local`) e o marcaria de novo como "nunca lançado", falseando
        // `full_skipped` pra qualquer título Heroic com prefixo existente.
        let mut processed_heroic_titles: HashSet<String> = HashSet::new();
        let Ok(entries) = std::fs::read_dir(heroic_local_root) else {
            return Ok(());
        };
        for entry in entries.flatten() {
            if !entry.path().is_dir() {
                continue;
            }
            let title = entry.file_name().to_string_lossy().into_owned();
            if only.is_some_and(|o| o != title) {
                continue;
            }
            processed_heroic_titles.insert(title.clone());
            *processed += 1;
            on_item(*processed, total_items, &title);
            summary.inspected += 1;

            // Mesmo fix do ramo Steam acima (ver comentário lá): nunca pular
            // o upload em si só por já existir uma entrada remota — o app
            // pode ter progresso novo desde a primeira ingestão.
            let compatdata_already_remote = heroic_remote_entries.contains(&title);
            let source = GameSource::HeroicGame {
                title: title.clone(),
            };
            let local_targets = library.deletion_targets(&source)?;
            let remote_paths = heroic_vault.paths_for(&source)?;

            on_uploading(&title);
            RsyncTransfer::incremental_upload(
                &heroic_vault.ssh_target,
                &local_targets.compatdata_dir,
                &remote_paths.compatdata_dir,
            )
            .with_binary(rsync_bin)
            .execute_with_progress(|u| on_progress(u))
            .map_err(|source| IngestError::Upload {
                app_id: title.clone(),
                source,
            })?;

            if compatdata_already_remote {
                summary.skipped_existing += 1;
            } else {
                summary.uploaded += 1;
            }

            let mut local_install_dir_for_purge: Option<PathBuf> = None;
            let full_install_ok = if !full {
                true
            } else {
                let epic_override = cfg.install_path_overrides.epic.get(&title);
                let resolved = legendary_installed_json.and_then(|json| {
                    install_paths::resolve_legendary_install_path(
                        json,
                        &title,
                        epic_override.map(|p| p.as_path()),
                    )
                });

                match resolved {
                    Some(install_dir) => {
                        let (base, epic_names) =
                            epic_remote.as_ref().expect("full=true garante Some");
                        let synced = sync_full_install_and_report(
                            &vault.ssh_target,
                            rsync_bin,
                            "epic",
                            base,
                            epic_names,
                            &install_dir,
                            &title,
                            &title,
                            summary,
                            on_uploading,
                            on_progress,
                        )?;
                        local_install_dir_for_purge = Some(synced);
                        true
                    }
                    None => {
                        // Mesmo raciocínio das passadas Steam/piratas: o
                        // `install_path` do Legendary (e o próprio
                        // override) ficam obsoletos sozinhos nesta máquina
                        // (histórico documentado) — pasta ausente não é
                        // "nunca instalado" se o nome já bate com o que
                        // está confirmado no FeuVault.
                        let (_, epic_names) = epic_remote.as_ref().expect("full=true garante Some");
                        let override_name = epic_override
                            .and_then(|p| p.file_name())
                            .map(|n| n.to_string_lossy().into_owned());
                        let installed_json_name = legendary_installed_json.and_then(|json| {
                            install_paths::legendary_installdir_name(json, &title)
                        });
                        let already_migrated = already_migrated(&[
                            (override_name.as_deref(), epic_names),
                            (installed_json_name.as_deref(), epic_names),
                        ]);
                        if !already_migrated {
                            summary.full_skipped.push(title.clone());
                        }
                        false
                    }
                }
            };

            if and_purge_local && full_install_ok {
                let source = GameSource::HeroicGame {
                    title: title.clone(),
                };
                let local_targets = library.deletion_targets(&source)?;
                delete_local_targets(&local_targets).map_err(|source| IngestError::Cleanup {
                    app_id: title.clone(),
                    source,
                })?;
                if let Some(install_dir) = &local_install_dir_for_purge {
                    purge_full_install_dir(install_dir, &title)?;
                    // Sem isto, o `installed.json` continua afirmando que
                    // `title` está instalado na pasta que acabamos de
                    // apagar — o catálogo (`local_heroic_titles`,
                    // deliberadamente sem verificar o disco) mostraria o
                    // jogo como "Cliente"/"Cliente+Servidor" pra sempre,
                    // mesmo 100% migrado. Mesmo princípio de Steam apagar o
                    // `appmanifest` no purge.
                    if let Some(json) = legendary_installed_json {
                        install_paths::remove_legendary_installed_entry(json, &title).map_err(
                            |source| IngestError::StaleMetadataCleanup {
                                title: title.clone(),
                                source,
                            },
                        )?;
                    }
                }
                summary.purged_local += 1;
            }
        }

        // Idem à passada de piratas acima, mas pra títulos Epic sem
        // prefixo Wine (`heroic_local_root/<título>`) — nativos Linux ou
        // nunca lançados. `installed.json` é a fonte de descoberta
        // completa; o prefixo, aqui, é opcional.
        if full {
            if let Some(json) = legendary_installed_json {
                let (base, epic_names) = epic_remote.as_ref().expect("full=true garante Some");
                for title in epic_manifest_only_entries(heroic_local_root, json, only) {
                    if processed_heroic_titles.contains(&title) {
                        continue;
                    }
                    *processed += 1;
                    on_item(*processed, total_items, &title);
                    summary.inspected += 1;

                    let epic_override = cfg.install_path_overrides.epic.get(&title);
                    let resolved = install_paths::resolve_legendary_install_path(
                        json,
                        &title,
                        epic_override.map(|p| p.as_path()),
                    );

                    match resolved {
                        Some(install_dir) => {
                            sync_full_install_and_report(
                                &vault.ssh_target,
                                rsync_bin,
                                "epic",
                                base,
                                epic_names,
                                &install_dir,
                                &title,
                                &title,
                                summary,
                                on_uploading,
                                on_progress,
                            )?;
                            if and_purge_local && install_dir.exists() {
                                std::fs::remove_dir_all(&install_dir).map_err(|source| {
                                    IngestError::Cleanup {
                                        app_id: title.clone(),
                                        source,
                                    }
                                })?;
                                // Mesmo motivo do ramo com prefixo Wine
                                // acima: sem isto, o `installed.json`
                                // continua achando esse título instalado
                                // depois de já ter apagado a pasta.
                                install_paths::remove_legendary_installed_entry(json, &title)
                                    .map_err(|source| IngestError::StaleMetadataCleanup {
                                        title: title.clone(),
                                        source,
                                    })?;
                                summary.purged_local += 1;
                            }
                        }
                        None => {
                            // Mesmo raciocínio de cima: `install_path` do
                            // Legendary (e o override) ficam obsoletos
                            // sozinhos — pasta ausente pode só significar
                            // "já migrado e apagado".
                            let override_name = epic_override
                                .and_then(|p| p.file_name())
                                .map(|n| n.to_string_lossy().into_owned());
                            let installed_json_name =
                                install_paths::legendary_installdir_name(json, &title);
                            let already_migrated = already_migrated(&[
                                (override_name.as_deref(), epic_names),
                                (installed_json_name.as_deref(), epic_names),
                            ]);
                            if !already_migrated {
                                summary.full_skipped.push(title.clone());
                            } else if and_purge_local {
                                // Instalação completa já confirmada no
                                // FeuVault e nenhum prefixo Wine local pra
                                // este título (é assim que ele chegou aqui
                                // — ver `epic_manifest_only_entries`): o
                                // registro no installed.json é só metadado
                                // obsoleto, sozinho ele deixa o Heroic
                                // mostrando "instalado" por engano pra
                                // sempre.
                                if install_paths::remove_legendary_installed_entry(json, &title)
                                    .map_err(|source| IngestError::StaleMetadataCleanup {
                                        title: title.clone(),
                                        source,
                                    })?
                                {
                                    summary.stale_metadata_cleaned.push(title.clone());
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vdf::VdfValue;
    use std::fs;

    fn test_dir(label: &str) -> std::path::PathBuf {
        crate::testutil::test_dir("ingest", label)
    }

    fn fake_bin(dir: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
        crate::testutil::write_script(dir, name, body)
    }

    fn cfg_with_lib(local_lib: &std::path::Path) -> Config {
        Config {
            local_libraries: vec![local_lib.to_path_buf()],
            ..Config::test_default()
        }
    }

    #[test]
    fn upload_manifest_snapshot_uploads_when_local_manifest_exists() {
        let dir = test_dir("manifest-snapshot-upload");
        let local_lib = dir.join("ssd");
        fs::create_dir_all(local_lib.join("steamapps")).unwrap();
        fs::write(
            local_lib.join("steamapps").join("appmanifest_730.acf"),
            "\"installdir\"\t\t\"Meu Jogo\"\n",
        )
        .unwrap();
        let remote_installs = dir.join("remote-installs");

        let args_log = dir.join("args.log");
        let rsync = fake_bin(
            &dir,
            "fake-rsync.sh",
            &format!("echo \"$@\" >> {}\nexit 0", args_log.display()),
        );

        upload_manifest_snapshot(
            "feuvault",
            rsync.to_str().unwrap(),
            &remote_installs,
            &local_lib,
            "730",
        );

        let recorded = fs::read_to_string(&args_log).unwrap();
        assert!(recorded.contains("appmanifest_730.acf"));
        assert!(recorded.contains("steam-manifests/730.acf"));
    }

    #[test]
    fn upload_manifest_snapshot_is_a_silent_no_op_without_a_local_manifest() {
        // Alguns jogos rastreados só por override nunca tiveram um
        // appmanifest local (ver install_paths.rs) — não é erro, só nada
        // pra salvar.
        let dir = test_dir("manifest-snapshot-missing");
        let local_lib = dir.join("ssd");
        fs::create_dir_all(local_lib.join("steamapps")).unwrap();
        let remote_installs = dir.join("remote-installs");

        let args_log = dir.join("args.log");
        let rsync = fake_bin(
            &dir,
            "fake-rsync.sh",
            &format!("echo called >> {}\nexit 0", args_log.display()),
        );

        upload_manifest_snapshot(
            "feuvault",
            rsync.to_str().unwrap(),
            &remote_installs,
            &local_lib,
            "730",
        );

        assert!(
            !args_log.exists(),
            "rsync não deveria ter sido chamado sem manifesto local"
        );
    }

    #[test]
    fn syncs_every_game_but_only_reports_new_ones_as_uploaded() {
        let dir = test_dir("upload-only-new");
        let local_lib = dir.join("ssd");
        fs::create_dir_all(local_lib.join("steamapps").join("compatdata").join("100")).unwrap();
        fs::create_dir_all(local_lib.join("steamapps").join("compatdata").join("200")).unwrap();

        // O FeuVault fake já conhece o 100, mas não o 200.
        let ssh = fake_bin(&dir, "fake-ssh.sh", "printf '100\\n'");
        let rsync = fake_bin(&dir, "fake-rsync.sh", "exit 0");

        let mut uploaded_ids = Vec::new();
        let summary = run_ingest_with_options(
            &cfg_with_lib(&local_lib),
            false,
            false,
            None,
            None,
            rsync.to_str().unwrap(),
            ssh.to_str().unwrap(),
            |id: &str| uploaded_ids.push(id.to_string()),
            |_, _, _| {},
            |_| {},
        )
        .unwrap();

        assert_eq!(summary.inspected, 2);
        assert_eq!(summary.uploaded, 1);
        assert_eq!(summary.skipped_existing, 1);
        // Incidente real (2026-09-06, Kenshi): "100" já apareceu remotamente
        // uma vez, mas isso não significa que o conteúdo local atual bate —
        // o rsync roda pros DOIS (idempotente/incremental, barato quando não
        // há nada novo), só o resumo acima (`uploaded`/`skipped_existing`)
        // distingue "nunca visto antes" de "já era conhecido".
        let mut sorted_ids = uploaded_ids.clone();
        sorted_ids.sort();
        assert_eq!(sorted_ids, vec!["100".to_string(), "200".to_string()]);
    }

    #[test]
    fn label_includes_the_game_name_when_a_manifest_is_present() {
        let dir = test_dir("label-with-name");
        let local_lib = dir.join("ssd");
        fs::create_dir_all(local_lib.join("steamapps").join("compatdata").join("730")).unwrap();
        fs::write(
            local_lib.join("steamapps").join("appmanifest_730.acf"),
            "\"name\"\t\t\"Counter-Strike 2\"\n\"installdir\"\t\t\"Counter-Strike Global Offensive\"\n",
        )
        .unwrap();

        let ssh = fake_bin(&dir, "fake-ssh.sh", "printf ''");
        let rsync = fake_bin(&dir, "fake-rsync.sh", "exit 0");

        let mut uploaded_labels = Vec::new();
        run_ingest_with_options(
            &cfg_with_lib(&local_lib),
            false,
            false,
            None,
            None,
            rsync.to_str().unwrap(),
            ssh.to_str().unwrap(),
            |label: &str| uploaded_labels.push(label.to_string()),
            |_, _, _| {},
            |_| {},
        )
        .unwrap();

        assert_eq!(uploaded_labels, vec!["730 — Counter-Strike 2".to_string()]);
    }

    #[test]
    fn label_falls_back_to_the_bare_app_id_without_a_manifest() {
        let dir = test_dir("label-without-name");
        let local_lib = dir.join("ssd");
        fs::create_dir_all(local_lib.join("steamapps").join("compatdata").join("999")).unwrap();

        let ssh = fake_bin(&dir, "fake-ssh.sh", "printf ''");
        let rsync = fake_bin(&dir, "fake-rsync.sh", "exit 0");

        let mut uploaded_labels = Vec::new();
        run_ingest_with_options(
            &cfg_with_lib(&local_lib),
            false,
            false,
            None,
            None,
            rsync.to_str().unwrap(),
            ssh.to_str().unwrap(),
            |label: &str| uploaded_labels.push(label.to_string()),
            |_, _, _| {},
            |_| {},
        )
        .unwrap();

        assert_eq!(uploaded_labels, vec!["999".to_string()]);
    }

    #[test]
    fn only_restricts_the_sweep_to_a_single_app_id() {
        let dir = test_dir("only-filter");
        let local_lib = dir.join("ssd");
        // "100" é um jogo grande que NÃO deve ser tocado — só "200" deve
        // ser processado, mesmo os dois sendo local-only.
        fs::create_dir_all(local_lib.join("steamapps").join("compatdata").join("100")).unwrap();
        fs::create_dir_all(local_lib.join("steamapps").join("compatdata").join("200")).unwrap();

        let ssh = fake_bin(&dir, "fake-ssh.sh", "printf ''");
        let rsync = fake_bin(&dir, "fake-rsync.sh", "exit 0");

        let mut uploaded_ids = Vec::new();
        let summary = run_ingest_with_options(
            &cfg_with_lib(&local_lib),
            false,
            false,
            None,
            Some("200"),
            rsync.to_str().unwrap(),
            ssh.to_str().unwrap(),
            |id: &str| uploaded_ids.push(id.to_string()),
            |_, _, _| {},
            |_| {},
        )
        .unwrap();

        assert_eq!(summary.inspected, 1, "só o app_id filtrado deve contar");
        assert_eq!(uploaded_ids, vec!["200".to_string()]);
    }

    #[test]
    fn and_purge_local_deletes_compatdata_after_successful_upload() {
        let dir = test_dir("purge-after-upload");
        let local_lib = dir.join("ssd");
        let compatdata = local_lib.join("steamapps").join("compatdata").join("200");
        fs::create_dir_all(&compatdata).unwrap();

        let ssh = fake_bin(&dir, "fake-ssh.sh", "printf ''");
        let rsync = fake_bin(&dir, "fake-rsync.sh", "exit 0");

        let summary = run_ingest_with_options(
            &cfg_with_lib(&local_lib),
            false,
            true,
            None,
            None,
            rsync.to_str().unwrap(),
            ssh.to_str().unwrap(),
            |_| {},
            |_, _, _| {},
            |_| {},
        )
        .unwrap();

        assert_eq!(summary.uploaded, 1);
        assert_eq!(summary.purged_local, 1);
        assert!(!compatdata.exists());
    }

    #[test]
    fn without_and_purge_local_flag_local_compatdata_survives_upload() {
        let dir = test_dir("no-purge-after-upload");
        let local_lib = dir.join("ssd");
        let compatdata = local_lib.join("steamapps").join("compatdata").join("200");
        fs::create_dir_all(&compatdata).unwrap();

        let ssh = fake_bin(&dir, "fake-ssh.sh", "printf ''");
        let rsync = fake_bin(&dir, "fake-rsync.sh", "exit 0");

        let summary = run_ingest_with_options(
            &cfg_with_lib(&local_lib),
            false,
            false,
            None,
            None,
            rsync.to_str().unwrap(),
            ssh.to_str().unwrap(),
            |_| {},
            |_, _, _| {},
            |_| {},
        )
        .unwrap();

        assert_eq!(summary.purged_local, 0);
        assert!(compatdata.exists());
    }

    #[test]
    fn skips_non_numeric_compatdata_entries() {
        let dir = test_dir("skip-non-numeric");
        let local_lib = dir.join("ssd");
        fs::create_dir_all(
            local_lib
                .join("steamapps")
                .join("compatdata")
                .join("nao-e-appid"),
        )
        .unwrap();

        let ssh = fake_bin(&dir, "fake-ssh.sh", "printf ''");
        let rsync = fake_bin(&dir, "fake-rsync.sh", "exit 0");

        let summary = run_ingest_with_options(
            &cfg_with_lib(&local_lib),
            false,
            false,
            None,
            None,
            rsync.to_str().unwrap(),
            ssh.to_str().unwrap(),
            |_| {},
            |_, _, _| {},
            |_| {},
        )
        .unwrap();

        assert_eq!(summary.inspected, 0);
        assert_eq!(summary.uploaded, 0);
    }

    #[test]
    fn upload_failure_is_reported_with_the_offending_app_id() {
        let dir = test_dir("upload-fail");
        let local_lib = dir.join("ssd");
        fs::create_dir_all(local_lib.join("steamapps").join("compatdata").join("300")).unwrap();

        let ssh = fake_bin(&dir, "fake-ssh.sh", "printf ''");
        let rsync = fake_bin(&dir, "fake-rsync.sh", "exit 23");

        let result = run_ingest_with_options(
            &cfg_with_lib(&local_lib),
            false,
            false,
            None,
            None,
            rsync.to_str().unwrap(),
            ssh.to_str().unwrap(),
            |_| {},
            |_, _, _| {},
            |_| {},
        );

        match result {
            Err(IngestError::Upload { app_id, .. }) => assert_eq!(app_id, "300"),
            other => panic!("esperava IngestError::Upload, obteve {other:?}"),
        }
    }

    #[test]
    fn empty_remote_and_local_is_a_no_op() {
        let dir = test_dir("empty");
        let local_lib = dir.join("ssd");
        fs::create_dir_all(&local_lib).unwrap();

        let ssh = fake_bin(&dir, "fake-ssh.sh", "printf ''");
        let rsync = fake_bin(&dir, "fake-rsync.sh", "exit 0");

        let summary = run_ingest_with_options(
            &cfg_with_lib(&local_lib),
            false,
            false,
            None,
            None,
            rsync.to_str().unwrap(),
            ssh.to_str().unwrap(),
            |_| {},
            |_, _, _| {},
            |_| {},
        )
        .unwrap();

        assert_eq!(summary, IngestSummary::default());
    }

    fn cfg_with_full(
        local_lib: &std::path::Path,
        remote_installs_base: &std::path::Path,
    ) -> Config {
        Config {
            remote_installs_base_path: Some(remote_installs_base.to_path_buf()),
            ..cfg_with_lib(local_lib)
        }
    }

    #[test]
    fn full_resolves_via_config_override_when_manifest_is_absent() {
        let dir = test_dir("full-override");
        let local_lib = dir.join("ssd");
        // Compatdata existe, mas NENHUM manifesto — reproduz o cenário real
        // que motivou este recurso (metadado do launcher indisponível ou
        // obsoleto). Sem override, isso cairia em `full_skipped`.
        fs::create_dir_all(local_lib.join("steamapps").join("compatdata").join("100")).unwrap();
        let real_install = dir.join("jogo-de-verdade");
        fs::create_dir_all(&real_install).unwrap();

        let ssh = fake_bin(&dir, "fake-ssh.sh", "printf ''");
        let rsync = fake_bin(&dir, "fake-rsync.sh", "exit 0");
        let remote_installs = dir.join("remote-installs");
        let mut cfg = cfg_with_full(&local_lib, &remote_installs);
        cfg.install_path_overrides
            .steam
            .insert("100".to_string(), real_install.clone());

        let summary = run_ingest_with_options(
            &cfg,
            true,
            false,
            None,
            None,
            rsync.to_str().unwrap(),
            ssh.to_str().unwrap(),
            |_| {},
            |_, _, _| {},
            |_| {},
        )
        .unwrap();

        assert_eq!(summary.full_uploaded, 1);
        assert!(
            summary.full_skipped.is_empty(),
            "o override deveria ter resolvido o jogo sem precisar de manifesto"
        );
    }

    #[test]
    fn full_override_without_manifest_never_fails_when_local_game_root_steam_is_set() {
        // Bug real: `adopt_official_manifest` presumia que o manifesto
        // sempre existia (fazia sentido antes, quando a resolução oficial
        // só tinha sucesso lendo o manifesto) — com `local_game_root_steam`
        // configurado E um override resolvendo sem manifesto nenhum (ex.:
        // Echoes of Aincrad Demo, appmanifest nunca existiu), o
        // `std::fs::copy` de um arquivo inexistente derrubava o `ingest`
        // inteiro com erro fatal, em vez de simplesmente não ter nada pra
        // adotar.
        let dir = test_dir("full-override-no-manifest-with-root");
        let local_lib = dir.join("ssd");
        fs::create_dir_all(local_lib.join("steamapps").join("compatdata").join("100")).unwrap();
        let real_install = dir.join("jogo-de-verdade");
        fs::create_dir_all(&real_install).unwrap();
        let local_game_root_steam = dir.join("games-steam");

        let ssh = fake_bin(&dir, "fake-ssh.sh", "printf ''");
        let rsync = fake_bin(&dir, "fake-rsync.sh", "exit 0");
        let remote_installs = dir.join("remote-installs");
        let mut cfg = cfg_with_full(&local_lib, &remote_installs);
        cfg.local_game_root_steam = Some(local_game_root_steam);
        cfg.install_path_overrides
            .steam
            .insert("100".to_string(), real_install);

        let summary = run_ingest_with_options(
            &cfg,
            true,
            false,
            None,
            None,
            rsync.to_str().unwrap(),
            ssh.to_str().unwrap(),
            |_| {},
            |_, _, _| {},
            |_| {},
        )
        .unwrap();

        assert_eq!(summary.full_uploaded, 1);
        assert_eq!(summary.official_manifests_migrated, 0);
    }

    #[test]
    fn full_manifest_only_entry_already_on_remote_is_not_reported_as_skipped() {
        // Reproduz o estado de uma segunda execução de `ingest --full
        // --and-purge-local` sobre um jogo já migrado com sucesso numa
        // execução anterior: `compatdata/` já sumiu, mas o manifesto
        // oficial (bookkeeping deliberado da Fase VII) persiste apontando
        // pra uma pasta que também já foi apagada. Sem checar o nome
        // resolvido contra o que já está remoto, isso caía em
        // `full_skipped` pra sempre, mesmo saudável.
        let dir = test_dir("full-manifest-already-remote");
        let local_lib = dir.join("ssd");
        let steamapps = local_lib.join("steamapps");
        fs::create_dir_all(&steamapps).unwrap();
        fs::write(
            steamapps.join("appmanifest_100.acf"),
            "\"installdir\"\t\t\"Jogo Migrado\"\n",
        )
        .unwrap();

        let ssh = fake_bin(&dir, "fake-ssh.sh", "printf 'Jogo Migrado\\n'");
        let rsync = fake_bin(&dir, "fake-rsync.sh", "exit 0");
        let remote_installs = dir.join("remote-installs");

        let summary = run_ingest_with_options(
            &cfg_with_full(&local_lib, &remote_installs),
            true,
            true,
            None,
            None,
            rsync.to_str().unwrap(),
            ssh.to_str().unwrap(),
            |_| {},
            |_, _, _| {},
            |_| {},
        )
        .unwrap();

        assert!(
            summary.full_skipped.is_empty(),
            "jogo já migrado (manifesto órfão apontando pra pasta já apagada, mas já \
             presente no remoto) não deveria aparecer em full_skipped: {:?}",
            summary.full_skipped
        );
    }

    #[test]
    fn pirata_with_compatdata_and_purged_override_is_not_reported_as_skipped() {
        // Reproduz um bug real: um jogo pirata com `compatdata/` (primeiro
        // laço, não a passada "sem compatdata") tinha seu override apagado
        // pelo próprio `--and-purge-local` na execução anterior — a pasta
        // que o override aponta não existe mais. O `None` deste primeiro
        // laço nunca checava nome nenhum contra o remoto (só as passadas
        // "sem compatdata" mais abaixo tinham essa checagem), então o jogo
        // reaparecia em `full_skipped` mesmo já estando são no FeuVault.
        let dir = test_dir("full-pirata-compatdata-override-purged");
        let local_lib = dir.join("ssd");
        fs::create_dir_all(local_lib.join("steamapps").join("compatdata").join("300")).unwrap();
        // Nunca recriada de propósito — reproduz "já migrada e apagada
        // pelo próprio purge da execução anterior".
        let purged_install_dir = dir.join("Piratas").join("Jogo Ja Migrado");

        let ssh = fake_bin(&dir, "fake-ssh.sh", "printf 'Jogo Ja Migrado\\n'");
        let rsync = fake_bin(&dir, "fake-rsync.sh", "exit 0");
        let remote_installs = dir.join("remote-installs");
        let mut cfg = cfg_with_full(&local_lib, &remote_installs);
        cfg.install_path_overrides
            .piratas
            .insert("300".to_string(), purged_install_dir);

        let summary = run_ingest_with_options(
            &cfg,
            true,
            true,
            None,
            None,
            rsync.to_str().unwrap(),
            ssh.to_str().unwrap(),
            |_| {},
            |_, _, _| {},
            |_| {},
        )
        .unwrap();

        assert!(
            summary.full_skipped.is_empty(),
            "jogo pirata já migrado (override apontando pra pasta já apagada pelo próprio \
             purge, mas já presente no remoto) não deveria aparecer em full_skipped: {:?}",
            summary.full_skipped
        );
    }

    #[test]
    fn full_resolves_and_uploads_new_official_install() {
        let dir = test_dir("full-official-new");
        let local_lib = dir.join("ssd");
        fs::create_dir_all(local_lib.join("steamapps").join("compatdata").join("100")).unwrap();
        fs::create_dir_all(local_lib.join("steamapps").join("common").join("Meu Jogo")).unwrap();
        fs::write(
            local_lib.join("steamapps").join("appmanifest_100.acf"),
            "\"installdir\"\t\t\"Meu Jogo\"\n",
        )
        .unwrap();

        let ssh = fake_bin(&dir, "fake-ssh.sh", "printf ''");
        let rsync = fake_bin(&dir, "fake-rsync.sh", "exit 0");
        let remote_installs = dir.join("remote-installs");

        let summary = run_ingest_with_options(
            &cfg_with_full(&local_lib, &remote_installs),
            true,
            false,
            None,
            None,
            rsync.to_str().unwrap(),
            ssh.to_str().unwrap(),
            |_| {},
            |_, _, _| {},
            |_| {},
        )
        .unwrap();

        assert_eq!(summary.uploaded, 1);
        assert_eq!(summary.full_uploaded, 1);
        assert!(summary.full_skipped.is_empty());
    }

    #[test]
    fn full_skips_game_whose_install_path_cannot_be_verified() {
        let dir = test_dir("full-unresolvable");
        let local_lib = dir.join("ssd");
        // Só compatdata — sem manifesto, sem shortcuts.vdf, sem
        // steam_root: nada pra resolver a instalação completa. Uma pasta
        // extra em `AppData/Local` (além do esqueleto padrão do Wine)
        // prova que um jogo de verdade já rodou ali — sem isso, o
        // `looks_like_empty_compat_tool_prefix` trataria como bookkeeping
        // vazio e nem reportaria (comportamento correto pra esse caso,
        // testado separadamente; aqui queremos o caso "jogo real, caminho
        // perdido").
        let appdata_local = local_lib
            .join("steamapps")
            .join("compatdata")
            .join("200")
            .join("pfx")
            .join("drive_c")
            .join("users")
            .join("steamuser")
            .join("AppData")
            .join("Local");
        fs::create_dir_all(appdata_local.join("MeuJogoDeVerdade")).unwrap();

        let ssh = fake_bin(&dir, "fake-ssh.sh", "printf ''");
        let rsync = fake_bin(&dir, "fake-rsync.sh", "exit 0");
        let remote_installs = dir.join("remote-installs");

        let summary = run_ingest_with_options(
            &cfg_with_full(&local_lib, &remote_installs),
            true,
            false,
            None,
            None,
            rsync.to_str().unwrap(),
            ssh.to_str().unwrap(),
            |_| {},
            |_, _, _| {},
            |_| {},
        )
        .unwrap();

        // Compatdata (pequeno, sempre seguro) ainda sobe normalmente —
        // só a parte da instalação completa é que não pôde ser resolvida.
        assert_eq!(summary.uploaded, 1);
        assert_eq!(summary.full_uploaded, 0);
        assert_eq!(summary.full_skipped, vec!["200".to_string()]);
    }

    #[test]
    fn ingest_exclude_steam_skips_configured_app_id_entirely() {
        // Reproduz o caso real (Zenless Zone Zero via Lutris, atalho
        // non-Steam com StartDir=$HOME): a guarda de segurança
        // (`install_paths::is_safe_install_dir`) nunca resolveria isso de
        // forma segura de qualquer jeito, então o jogo reapareceria em
        // `full_skipped` pra sempre sem uma forma explícita de dizer "não
        // tente, eu sei".
        let dir = test_dir("exclude-configured");
        let local_lib = dir.join("ssd");
        fs::create_dir_all(local_lib.join("steamapps").join("compatdata").join("999")).unwrap();

        let ssh = fake_bin(&dir, "fake-ssh.sh", "printf ''");
        let rsync = fake_bin(&dir, "fake-rsync.sh", "exit 0");
        let remote_installs = dir.join("remote-installs");
        let mut cfg = cfg_with_full(&local_lib, &remote_installs);
        cfg.ingest_exclude_steam.insert("999".to_string());

        let summary = run_ingest_with_options(
            &cfg,
            true,
            false,
            None,
            None,
            rsync.to_str().unwrap(),
            ssh.to_str().unwrap(),
            |_| {},
            |_, _, _| {},
            |_| {},
        )
        .unwrap();

        assert_eq!(
            summary.inspected, 0,
            "jogo excluído não deveria nem ser inspecionado"
        );
        assert!(summary.full_skipped.is_empty());
    }

    #[test]
    fn ingest_always_skips_app_id_zero_reserved_by_steam() {
        // "0" é bookkeeping compartilhado da própria Steam, nunca um jogo
        // de verdade — reconhecido automaticamente via `AppData/Local`
        // vazio (sem pfx nenhum criado aqui de propósito, o que já conta
        // como "vazio" pra `looks_like_empty_compat_tool_prefix`), nunca
        // aparece em `full_skipped`. O compatdata em si (pequeno, sempre
        // seguro) ainda é "inspecionado" normalmente — só não é reportado
        // como pulado.
        let dir = test_dir("exclude-zero");
        let local_lib = dir.join("ssd");
        fs::create_dir_all(local_lib.join("steamapps").join("compatdata").join("0")).unwrap();

        let ssh = fake_bin(&dir, "fake-ssh.sh", "printf ''");
        let rsync = fake_bin(&dir, "fake-rsync.sh", "exit 0");
        let remote_installs = dir.join("remote-installs");

        let summary = run_ingest_with_options(
            &cfg_with_full(&local_lib, &remote_installs),
            true,
            false,
            None,
            None,
            rsync.to_str().unwrap(),
            ssh.to_str().unwrap(),
            |_| {},
            |_, _, _| {},
            |_| {},
        )
        .unwrap();

        assert_eq!(summary.inspected, 1);
        assert!(summary.full_skipped.is_empty());
    }

    #[test]
    fn full_and_purge_local_deletes_both_compatdata_and_install_dir() {
        let dir = test_dir("full-purge-both");
        let local_lib = dir.join("ssd");
        let compatdata = local_lib.join("steamapps").join("compatdata").join("100");
        let install_dir = local_lib.join("steamapps").join("common").join("Meu Jogo");
        fs::create_dir_all(&compatdata).unwrap();
        fs::create_dir_all(&install_dir).unwrap();
        fs::write(
            local_lib.join("steamapps").join("appmanifest_100.acf"),
            "\"installdir\"\t\t\"Meu Jogo\"\n",
        )
        .unwrap();

        let ssh = fake_bin(&dir, "fake-ssh.sh", "printf ''");
        let rsync = fake_bin(&dir, "fake-rsync.sh", "exit 0");
        let remote_installs = dir.join("remote-installs");

        let summary = run_ingest_with_options(
            &cfg_with_full(&local_lib, &remote_installs),
            true,
            true,
            None,
            None,
            rsync.to_str().unwrap(),
            ssh.to_str().unwrap(),
            |_| {},
            |_, _, _| {},
            |_| {},
        )
        .unwrap();

        assert_eq!(summary.purged_local, 1);
        assert!(!compatdata.exists());
        assert!(!install_dir.exists());
        // Regressão: a passada "manifest_only" mais adiante releia o disco
        // DEPOIS do laço de compatdata já ter apagado tudo acima — sem
        // registrar este app_id como "já processado nesta execução", ela
        // enxergava "compatdata sumiu" como "nunca foi lançado", tentava
        // resolver a instalação de novo (que também já tinha sido movida e
        // apagada), falhava, e reportava o jogo em `full_skipped` mesmo
        // tendo sido migrado com sucesso segundos antes.
        assert!(
            summary.full_skipped.is_empty(),
            "jogo com compatdata+manifesto não deveria aparecer em full_skipped: {:?}",
            summary.full_skipped
        );
    }

    #[test]
    fn full_and_purge_local_never_touches_a_compat_tool_install() {
        let dir = test_dir("full-purge-proton");
        let local_lib = dir.join("ssd");
        let compatdata = local_lib
            .join("steamapps")
            .join("compatdata")
            .join("1493710");
        let install_dir = local_lib
            .join("steamapps")
            .join("common")
            .join("Proton - Experimental");
        fs::create_dir_all(&compatdata).unwrap();
        fs::create_dir_all(&install_dir).unwrap();
        fs::write(
            local_lib.join("steamapps").join("appmanifest_1493710.acf"),
            "\"installdir\"\t\t\"Proton - Experimental\"\n",
        )
        .unwrap();

        let ssh = fake_bin(&dir, "fake-ssh.sh", "printf ''");
        let rsync = fake_bin(&dir, "fake-rsync.sh", "exit 0");
        let remote_installs = dir.join("remote-installs");

        let summary = run_ingest_with_options(
            &cfg_with_full(&local_lib, &remote_installs),
            true,
            true,
            None,
            None,
            rsync.to_str().unwrap(),
            ssh.to_str().unwrap(),
            |_| {},
            |_, _, _| {},
            |_| {},
        )
        .unwrap();

        // compatdata (bookkeeping do próprio Proton) sobe/purga normalmente,
        // mas a instalação do Proton em si nunca é enviada nem apagada —
        // é infraestrutura compartilhada por todos os jogos que a usam.
        assert_eq!(summary.full_uploaded, 0);
        assert!(summary.full_skipped.is_empty());
        assert!(!compatdata.exists());
        assert!(install_dir.exists());
    }

    #[test]
    fn full_migrates_manifest_to_local_game_root_steam_and_purges_old_copy() {
        let dir = test_dir("full-adopt");
        // Biblioteca "antiga" (ex.: HDD1) — onde o jogo está instalado hoje.
        let old_lib = dir.join("hdd1");
        let compatdata = old_lib.join("steamapps").join("compatdata").join("100");
        let install_dir = old_lib.join("steamapps").join("common").join("Meu Jogo");
        fs::create_dir_all(&compatdata).unwrap();
        fs::create_dir_all(&install_dir).unwrap();
        let old_manifest = old_lib.join("steamapps").join("appmanifest_100.acf");
        fs::write(&old_manifest, "\"installdir\"\t\t\"Meu Jogo\"\n").unwrap();

        // Raiz nova (NVMe) — ainda vazia, é pra onde o manifesto deve
        // migrar (a instalação/compatdata em si nunca precisam visitar
        // esse caminho: sobem direto da biblioteca antiga e são apagadas
        // de lá).
        let steam_full_root = dir.join("nvme-steam");

        let ssh = fake_bin(&dir, "fake-ssh.sh", "printf ''");
        let rsync = fake_bin(&dir, "fake-rsync.sh", "exit 0");
        let remote_installs = dir.join("remote-installs");
        let cfg = Config {
            local_game_root_steam: Some(steam_full_root.clone()),
            ..cfg_with_full(&old_lib, &remote_installs)
        };

        let summary = run_ingest_with_options(
            &cfg,
            true,
            true,
            None,
            None,
            rsync.to_str().unwrap(),
            ssh.to_str().unwrap(),
            |_| {},
            |_, _, _| {},
            |_| {},
        )
        .unwrap();

        let new_manifest = steam_full_root
            .join("steamapps")
            .join("appmanifest_100.acf");
        assert_eq!(summary.official_manifests_migrated, 1);
        assert!(
            new_manifest.exists(),
            "manifesto deveria ter sido copiado pro NVMe"
        );
        assert!(
            !old_manifest.exists(),
            "manifesto antigo deveria ter sido apagado (purga completa do PC)"
        );
        assert!(!compatdata.exists());
        assert!(!install_dir.exists());
        assert!(
            !steam_full_root.join("steamapps").join("common").exists(),
            "a instalação nunca deveria visitar o NVMe fisicamente — sobe direto da biblioteca antiga"
        );
    }

    #[test]
    fn full_and_purge_local_never_deletes_compatdata_when_install_is_unresolvable() {
        let dir = test_dir("full-purge-unresolvable");
        let local_lib = dir.join("ssd");
        let compatdata = local_lib.join("steamapps").join("compatdata").join("200");
        fs::create_dir_all(&compatdata).unwrap();

        let ssh = fake_bin(&dir, "fake-ssh.sh", "printf ''");
        let rsync = fake_bin(&dir, "fake-rsync.sh", "exit 0");
        let remote_installs = dir.join("remote-installs");

        let summary = run_ingest_with_options(
            &cfg_with_full(&local_lib, &remote_installs),
            true,
            true,
            None,
            None,
            rsync.to_str().unwrap(),
            ssh.to_str().unwrap(),
            |_| {},
            |_, _, _| {},
            |_| {},
        )
        .unwrap();

        assert_eq!(summary.purged_local, 0);
        assert!(
            compatdata.exists(),
            "nunca apaga compatdata sem confirmar a instalação completa"
        );
    }

    #[test]
    fn full_without_remote_installs_base_path_is_a_hard_error() {
        let dir = test_dir("full-not-configured");
        let local_lib = dir.join("ssd");
        fs::create_dir_all(&local_lib).unwrap();

        let ssh = fake_bin(&dir, "fake-ssh.sh", "printf ''");
        let rsync = fake_bin(&dir, "fake-rsync.sh", "exit 0");

        let result = run_ingest_with_options(
            &cfg_with_lib(&local_lib),
            true,
            false,
            None,
            None,
            rsync.to_str().unwrap(),
            ssh.to_str().unwrap(),
            |_| {},
            |_, _, _| {},
            |_| {},
        );

        assert!(matches!(result, Err(IngestError::FullNotConfigured)));
    }

    #[test]
    fn full_resolves_pirata_install_via_override_never_via_shortcuts_vdf() {
        // `StartDir` do `shortcuts.vdf` é só o que o usuário digitou uma
        // vez ao criar o atalho, nunca atualizado depois — não é fonte
        // confiável de "onde o jogo está" (ao contrário do manifesto Steam
        // ou do `installed.json` do Legendary, mantidos pelas próprias
        // plataformas). Só o override explícito resolve pra piratas; por
        // isso o `shortcuts.vdf` abaixo aponta pra um caminho MORTO de
        // propósito — se o código ainda lesse `StartDir`, o teste passaria
        // por engano com o caminho errado.
        let dir = test_dir("full-piratas");
        let local_lib = dir.join("ssd");
        // Non-Steam shortcuts nunca têm appmanifest — só compatdata.
        fs::create_dir_all(
            local_lib
                .join("steamapps")
                .join("compatdata")
                .join("1234567890"),
        )
        .unwrap();

        let steam_root = dir.join("steam");
        let userdata = steam_root.join("userdata").join("1").join("config");
        fs::create_dir_all(&userdata).unwrap();
        let install_dir = dir.join("Piratas").join("Jogo Pirata");
        fs::create_dir_all(&install_dir).unwrap();
        let shortcuts_tree = vec![(
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
                        VdfValue::Str("\"/caminho/morto/de/proposito\"".to_string()),
                    ),
                ]),
            )]),
        )];
        fs::write(
            userdata.join("shortcuts.vdf"),
            crate::vdf::write_binary(&shortcuts_tree),
        )
        .unwrap();

        let ssh = fake_bin(&dir, "fake-ssh.sh", "printf ''");
        let rsync = fake_bin(&dir, "fake-rsync.sh", "exit 0");
        let remote_installs = dir.join("remote-installs");
        let mut cfg = cfg_with_full(&local_lib, &remote_installs);
        cfg.install_path_overrides
            .piratas
            .insert("1234567890".to_string(), install_dir);

        let summary = run_ingest_with_options(
            &cfg,
            true,
            false,
            Some(&steam_root),
            None,
            rsync.to_str().unwrap(),
            ssh.to_str().unwrap(),
            |_| {},
            |_, _, _| {},
            |_| {},
        )
        .unwrap();

        assert_eq!(summary.full_uploaded, 1);
        assert!(summary.full_skipped.is_empty());
    }

    #[test]
    fn full_never_attempts_pirata_without_override_even_with_shortcuts_vdf() {
        // Reflete o novo design: sem override, um atalho non-Steam nunca é
        // tentado pra rastreamento de instalação completa — nem enviado,
        // nem reportado como pulado. `shortcuts.vdf` sozinho não é mais
        // suficiente.
        let dir = test_dir("full-piratas-no-override");
        let local_lib = dir.join("ssd");
        fs::create_dir_all(
            local_lib
                .join("steamapps")
                .join("compatdata")
                .join("1234567890"),
        )
        .unwrap();

        let steam_root = dir.join("steam");
        let userdata = steam_root.join("userdata").join("1").join("config");
        fs::create_dir_all(&userdata).unwrap();
        let install_dir = dir.join("Piratas").join("Jogo Pirata");
        fs::create_dir_all(&install_dir).unwrap();
        let shortcuts_tree = vec![(
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
        fs::write(
            userdata.join("shortcuts.vdf"),
            crate::vdf::write_binary(&shortcuts_tree),
        )
        .unwrap();

        let ssh = fake_bin(&dir, "fake-ssh.sh", "printf ''");
        let rsync = fake_bin(&dir, "fake-rsync.sh", "exit 0");
        let remote_installs = dir.join("remote-installs");

        let summary = run_ingest_with_options(
            &cfg_with_full(&local_lib, &remote_installs),
            true,
            false,
            Some(&steam_root),
            None,
            rsync.to_str().unwrap(),
            ssh.to_str().unwrap(),
            |_| {},
            |_, _, _| {},
            |_| {},
        )
        .unwrap();

        assert_eq!(summary.full_uploaded, 0);
        assert!(
            summary.full_skipped.is_empty(),
            "sem override, o jogo não deveria nem aparecer no relatório: {:?}",
            summary.full_skipped
        );
    }

    #[test]
    fn full_pirata_manifest_only_entry_already_on_remote_is_not_reported_as_skipped() {
        // Mesmo raciocínio do equivalente oficial acima, mas pra atalhos
        // non-Steam: o `StartDir` continua no `shortcuts.vdf` mesmo depois
        // da pasta já ter sido migrada e apagada numa execução anterior —
        // só o atalho em si nunca é tocado pelo `ingest`.
        let dir = test_dir("full-pirata-already-remote");
        let local_lib = dir.join("ssd");
        fs::create_dir_all(&local_lib).unwrap();

        let steam_root = dir.join("steam");
        let userdata = steam_root.join("userdata").join("1").join("config");
        fs::create_dir_all(&userdata).unwrap();
        // Nunca recriada de propósito — reproduz "já migrada e apagada".
        let install_dir = dir.join("Piratas").join("Jogo Ja Migrado");
        let shortcuts_tree = vec![(
            "shortcuts".to_string(),
            VdfValue::Object(vec![(
                "0".to_string(),
                VdfValue::Object(vec![
                    ("appid".to_string(), VdfValue::Int(999999999u32 as i32)),
                    (
                        "appname".to_string(),
                        VdfValue::Str("Jogo Ja Migrado".to_string()),
                    ),
                    (
                        "StartDir".to_string(),
                        VdfValue::Str(format!("\"{}\"", install_dir.display())),
                    ),
                ]),
            )]),
        )];
        fs::write(
            userdata.join("shortcuts.vdf"),
            crate::vdf::write_binary(&shortcuts_tree),
        )
        .unwrap();

        let ssh = fake_bin(&dir, "fake-ssh.sh", "printf 'Jogo Ja Migrado\\n'");
        let rsync = fake_bin(&dir, "fake-rsync.sh", "exit 0");
        let remote_installs = dir.join("remote-installs");

        let summary = run_ingest_with_options(
            &cfg_with_full(&local_lib, &remote_installs),
            true,
            true,
            Some(&steam_root),
            None,
            rsync.to_str().unwrap(),
            ssh.to_str().unwrap(),
            |_| {},
            |_, _, _| {},
            |_| {},
        )
        .unwrap();

        assert!(
            summary.full_skipped.is_empty(),
            "jogo pirata já migrado (atalho apontando pra pasta já apagada, mas já \
             presente no remoto) não deveria aparecer em full_skipped: {:?}",
            summary.full_skipped
        );
    }

    #[test]
    fn full_discovers_and_uploads_a_pirata_override_with_no_compatdata() {
        let dir = test_dir("full-pirata-no-compatdata");
        let local_lib = dir.join("ssd");
        // Nenhum compatdata — o atalho nunca foi lançado (jogo pirata
        // nativo Linux, por exemplo). O override é a única fonte de
        // descoberta aqui — sem ele, o jogo não apareceria (ver o teste
        // `full_never_attempts_pirata_without_override_even_with_shortcuts_vdf`).
        fs::create_dir_all(&local_lib).unwrap();

        let install_dir = dir.join("Piratas").join("Jogo Nativo Pirata");
        fs::create_dir_all(&install_dir).unwrap();

        let ssh = fake_bin(&dir, "fake-ssh.sh", "printf ''");
        let rsync = fake_bin(&dir, "fake-rsync.sh", "exit 0");
        let remote_installs = dir.join("remote-installs");
        let mut cfg = cfg_with_full(&local_lib, &remote_installs);
        cfg.install_path_overrides
            .piratas
            .insert("555555555".to_string(), install_dir.clone());

        let summary = run_ingest_with_options(
            &cfg,
            true,
            true,
            None,
            None,
            rsync.to_str().unwrap(),
            ssh.to_str().unwrap(),
            |_| {},
            |_, _, _| {},
            |_| {},
        )
        .unwrap();

        assert_eq!(
            summary.full_uploaded, 1,
            "atalho pirata sem compatdata, mas com override, deveria ser descoberto e enviado"
        );
        assert_eq!(summary.purged_local, 1);
        assert!(!install_dir.exists());
    }

    #[test]
    fn full_discovers_and_uploads_a_native_game_with_no_compatdata() {
        let dir = test_dir("full-no-compatdata");
        let local_lib = dir.join("ssd");
        // Jogo nativo Linux: instalação completa e manifesto existem, mas
        // NUNCA passou pelo Proton — sem `compatdata/100`, invisível ao
        // laço principal.
        fs::create_dir_all(
            local_lib
                .join("steamapps")
                .join("common")
                .join("Jogo Nativo"),
        )
        .unwrap();
        fs::write(
            local_lib.join("steamapps").join("appmanifest_100.acf"),
            "\"installdir\"\t\t\"Jogo Nativo\"\n",
        )
        .unwrap();

        let ssh = fake_bin(&dir, "fake-ssh.sh", "printf ''");
        let rsync = fake_bin(&dir, "fake-rsync.sh", "exit 0");
        let remote_installs = dir.join("remote-installs");

        let summary = run_ingest_with_options(
            &cfg_with_full(&local_lib, &remote_installs),
            true,
            true,
            None,
            None,
            rsync.to_str().unwrap(),
            ssh.to_str().unwrap(),
            |_| {},
            |_, _, _| {},
            |_| {},
        )
        .unwrap();

        assert_eq!(
            summary.full_uploaded, 1,
            "jogo sem compatdata deveria ser descoberto e enviado via --full"
        );
        assert_eq!(summary.purged_local, 1);
        assert!(!local_lib
            .join("steamapps")
            .join("common")
            .join("Jogo Nativo")
            .exists());
        assert!(!local_lib
            .join("steamapps")
            .join("appmanifest_100.acf")
            .exists());
    }

    #[test]
    fn full_skips_a_manifest_only_entry_whose_install_dir_is_missing() {
        let dir = test_dir("full-no-compatdata-broken");
        let local_lib = dir.join("ssd");
        // Manifesto órfão: Steam registra como instalado, mas a pasta em
        // `common/` já não existe (apagada manualmente fora da Steam) —
        // reproduz o caso real do Hotline Miami/Buckshot Roulette.
        fs::create_dir_all(local_lib.join("steamapps")).unwrap();
        fs::write(
            local_lib.join("steamapps").join("appmanifest_200.acf"),
            "\"installdir\"\t\t\"Jogo Quebrado\"\n",
        )
        .unwrap();

        let ssh = fake_bin(&dir, "fake-ssh.sh", "printf ''");
        let rsync = fake_bin(&dir, "fake-rsync.sh", "exit 0");
        let remote_installs = dir.join("remote-installs");

        let summary = run_ingest_with_options(
            &cfg_with_full(&local_lib, &remote_installs),
            true,
            true,
            None,
            None,
            rsync.to_str().unwrap(),
            ssh.to_str().unwrap(),
            |_| {},
            |_, _, _| {},
            |_| {},
        )
        .unwrap();

        assert_eq!(summary.full_uploaded, 0);
        assert_eq!(summary.full_skipped, vec!["200".to_string()]);
        assert_eq!(
            summary.purged_local, 0,
            "nunca apaga o manifesto de uma instalação que não pôde verificar"
        );
        assert!(local_lib
            .join("steamapps")
            .join("appmanifest_200.acf")
            .exists());
    }

    // `epic_manifest_only_entries` é testada isoladamente (em vez de via
    // `run_ingest_with_options`) porque o caminho do `installed.json` real
    // usado pelo laço --full não é injetável (usa sempre
    // `install_paths::default_legendary_installed_json_path()`) — mesma
    // limitação documentada no teste `#[ignore]` de `install_paths.rs`.
    #[test]
    fn epic_manifest_only_entries_excludes_titles_that_already_have_a_prefix() {
        let dir = test_dir("epic-manifest-only");
        let heroic_root = dir.join("heroic-prefixes");
        fs::create_dir_all(heroic_root.join("Ja Tem Prefixo")).unwrap();
        let installed_json = dir.join("installed.json");
        fs::write(
            &installed_json,
            r#"{
                "a": {"title": "Ja Tem Prefixo", "install_path": "/qualquer"},
                "b": {"title": "Sem Prefixo Nenhum", "install_path": "/qualquer2"}
            }"#,
        )
        .unwrap();

        let found = epic_manifest_only_entries(&heroic_root, &installed_json, None);
        assert_eq!(found, vec!["Sem Prefixo Nenhum".to_string()]);
    }

    #[test]
    fn epic_manifest_only_entries_respects_only_filter() {
        let dir = test_dir("epic-manifest-only-filter");
        let heroic_root = dir.join("heroic-prefixes");
        fs::create_dir_all(&heroic_root).unwrap();
        let installed_json = dir.join("installed.json");
        fs::write(
            &installed_json,
            r#"{
                "a": {"title": "Jogo A", "install_path": "/qualquer"},
                "b": {"title": "Jogo B", "install_path": "/qualquer2"}
            }"#,
        )
        .unwrap();

        let found = epic_manifest_only_entries(&heroic_root, &installed_json, Some("Jogo B"));
        assert_eq!(found, vec!["Jogo B".to_string()]);
    }

    fn cfg_with_heroic(local_lib: &std::path::Path, heroic_local_root: &std::path::Path) -> Config {
        Config {
            heroic_local_root: Some(heroic_local_root.to_path_buf()),
            heroic_remote_base_path: Some(std::path::PathBuf::from("/fake/remote/heroic")),
            ..cfg_with_lib(local_lib)
        }
    }

    #[test]
    fn syncs_every_heroic_title_but_only_reports_new_ones_as_uploaded() {
        let dir = test_dir("heroic-upload-only-new");
        let local_lib = dir.join("ssd");
        fs::create_dir_all(&local_lib).unwrap();
        let heroic_root = dir.join("heroic-prefixes");
        fs::create_dir_all(heroic_root.join("Jogo Presente")).unwrap();
        fs::create_dir_all(heroic_root.join("Jogo Novo")).unwrap();

        // O FeuVault fake já conhece "Jogo Presente", mas não "Jogo Novo".
        let ssh = fake_bin(&dir, "fake-ssh.sh", "printf 'Jogo Presente\\n'");
        let rsync = fake_bin(&dir, "fake-rsync.sh", "exit 0");

        let mut uploaded_ids = Vec::new();
        let summary = run_ingest_with_options(
            &cfg_with_heroic(&local_lib, &heroic_root),
            false,
            false,
            None,
            None,
            rsync.to_str().unwrap(),
            ssh.to_str().unwrap(),
            |id: &str| uploaded_ids.push(id.to_string()),
            |_, _, _| {},
            |_| {},
        )
        .unwrap();

        assert_eq!(summary.inspected, 2);
        assert_eq!(summary.uploaded, 1);
        assert_eq!(summary.skipped_existing, 1);
        // Mesmo raciocínio do teste Steam equivalente: "Jogo Presente" já
        // apareceu remotamente antes, mas isso não garante que o conteúdo
        // local ainda bate — o sync roda pros dois, só o resumo distingue.
        let mut sorted_ids = uploaded_ids.clone();
        sorted_ids.sort();
        assert_eq!(
            sorted_ids,
            vec!["Jogo Novo".to_string(), "Jogo Presente".to_string()]
        );
    }

    #[test]
    fn and_purge_local_deletes_heroic_prefix_after_successful_upload() {
        let dir = test_dir("heroic-purge-after-upload");
        let local_lib = dir.join("ssd");
        fs::create_dir_all(&local_lib).unwrap();
        let heroic_root = dir.join("heroic-prefixes");
        let prefix = heroic_root.join("Jogo Novo");
        fs::create_dir_all(&prefix).unwrap();

        let ssh = fake_bin(&dir, "fake-ssh.sh", "printf ''");
        let rsync = fake_bin(&dir, "fake-rsync.sh", "exit 0");

        let summary = run_ingest_with_options(
            &cfg_with_heroic(&local_lib, &heroic_root),
            false,
            true,
            None,
            None,
            rsync.to_str().unwrap(),
            ssh.to_str().unwrap(),
            |_| {},
            |_, _, _| {},
            |_| {},
        )
        .unwrap();

        assert_eq!(summary.uploaded, 1);
        assert_eq!(summary.purged_local, 1);
        assert!(!prefix.exists());
    }

    #[test]
    fn heroic_ingest_is_skipped_entirely_when_unconfigured() {
        let dir = test_dir("heroic-unconfigured");
        let local_lib = dir.join("ssd");
        fs::create_dir_all(&local_lib).unwrap();

        let ssh = fake_bin(&dir, "fake-ssh.sh", "printf ''");
        let rsync = fake_bin(&dir, "fake-rsync.sh", "exit 0");

        let summary = run_ingest_with_options(
            &cfg_with_lib(&local_lib),
            false,
            false,
            None,
            None,
            rsync.to_str().unwrap(),
            ssh.to_str().unwrap(),
            |_| {},
            |_, _, _| {},
            |_| {},
        )
        .unwrap();

        assert_eq!(summary, IngestSummary::default());
    }
}
