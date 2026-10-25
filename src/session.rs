use crate::config::Config;
use crate::domain::{GameSession, GameSource, Launcher, LibraryLocation};
use crate::install_paths;
use crate::lock::{LockError, LockGuard};
use crate::paths::{LocalDeletionTargets, PathSafetyError, RemoteVault};
use crate::progress::ProgressUpdate;
use crate::rsync::{RsyncTransfer, TransferError};
use crate::scan;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// Alvos de sincronização da *instalação completa* de um jogo (Fase VII) —
/// paralelo a `local_targets`/`remote_paths` (que continuam só sobre
/// `compatdata`). `None` sempre que o rastreamento não está configurado ou
/// não pôde ser detectado pra este `app_id` — nesse caso `run` se comporta
/// exatamente como antes da Fase VII (só compatdata).
#[derive(Debug, PartialEq, Eq)]
struct FullInstallTarget {
    local_dir: PathBuf,
    remote_dir: PathBuf,
}

/// Estágio da sessão em andamento — permite ao chamador (CLI) rotular a UI
/// ("Baixando..."/"Jogando..."/"Enviando...") sem `session.rs` depender de
/// zenity diretamente. `Running` é reportado uma única vez, logo após o
/// processo do jogo nascer com sucesso — sem isso, a UI ficaria "parada"
/// no rótulo de download durante toda a sessão de jogo, que normalmente é
/// a fase mais longa do ciclo inteiro.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Download,
    Running,
    Upload,
}

#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error("nenhuma biblioteca local configurada em `local_libraries`")]
    NoLocalLibrary,
    #[error(
        "'--heroic' exige `heroic_local_root` e `heroic_remote_base_path` configurados em config.json"
    )]
    HeroicNotConfigured,
    #[error("app_id inválido: '{0}' — deve ser estritamente numérico")]
    InvalidAppId(String),
    #[error("nenhum comando de execução informado após `--`")]
    EmptyExecCommand,
    #[error(transparent)]
    PathSafety(#[from] PathSafetyError),
    #[error("instância concorrente do vault-sync detectada: {0}")]
    Lock(#[from] LockError),
    #[error("falha ao baixar a instância do servidor antes de iniciar: {0}")]
    Download(#[source] TransferError),
    #[error("falha ao iniciar o processo do jogo: {0}")]
    GameSpawn(#[source] std::io::Error),
    #[error("falha ao sincronizar alterações de volta ao servidor: {0}")]
    Upload(#[source] TransferError),
    #[error("falha ao limpar armazenamento local pós-sync: {0}")]
    Cleanup(#[source] std::io::Error),
    #[error(
        "instalação completa detectada para '{0}', mas `remote_installs_base_path` não está configurado em config.json"
    )]
    FullInstallNotConfigured(String),
    #[error(
        "'steam -applaunch {0}' não iniciou uma sessão visível (processo 'reaper' da Steam Linux Runtime) em tempo — a Steam pode ter mostrado um diálogo de erro; nada foi sincronizado de volta"
    )]
    SteamLaunchTimeout(String),
    #[error(
        "o processo encerrou em menos de {elapsed_secs}s com status de erro ({status}) — \
         provavelmente nunca chegou a rodar de verdade (launcher recusando o lançamento, \
         dependência ausente, config quebrada); nada foi sincronizado de volta. Rode o comando \
         manualmente num terminal pra ver a mensagem de erro real"
    )]
    GameExitedImmediately { status: String, elapsed_secs: u64 },
    #[error(
        "o prefixo Wine de '{app_id}' ({dir}) continuava em uso depois de {timeout_secs}s por: \
         {processes} — {outcome}. Feche esses processos (ex.: EA app ou outro launcher ainda \
         aberto em segundo plano) e depois envie com 'vault-sync ingest --only \"{app_id}\"'"
    )]
    PrefixStillActive {
        app_id: String,
        dir: String,
        timeout_secs: u64,
        processes: String,
        outcome: &'static str,
    },
}

/// Ciclo de vida completo de uma sessão (3.1 → 3.4): aquisição integral,
/// interrupção voluntária até o jogo encerrar, sincronização diferencial de
/// volta e, só em caso de êxito, exclusão categórica do armazenamento local.
///
/// A detecção real de `GameSource` (oficial/non-Steam/órfão) é escopo da
/// Fase IV (port do `steam_scan.py`); por ora todo `app_id` numérico Steam é
/// tratado como `Official`, e a primeira biblioteca configurada é usada
/// como destino local. `launcher` (Fase VI) decide se `app_id` é um AppID
/// Steam ou um título de prefixo Heroic, e qual par de diretórios
/// local/remoto usar.
pub fn run_session(
    cfg: &Config,
    launcher: Launcher,
    app_id: &str,
    exec: &[String],
    steam_root: Option<&Path>,
    on_progress: impl FnMut(Stage, ProgressUpdate),
) -> Result<GameSession, RunError> {
    run_session_with_options(
        cfg,
        launcher,
        app_id,
        exec,
        steam_root,
        "rsync",
        "ssh",
        &LockGuard::default_path(),
        install_paths::default_legendary_installed_json_path().as_deref(),
        on_progress,
    )
}

/// Detecta, só pra `Launcher::Steam`, se `app_id` já está sob rastreamento
/// de instalação completa (Fase VII) — e, se sim, devolve também a
/// biblioteca local que deve ser usada (em vez de `local_libraries.first()`)
/// pra manter `compatdata`/manifesto/instalação sempre co-localizados,
/// exigência real do Proton (não é só organização). Tenta oficial primeiro
/// (`local_game_root_steam` + manifesto — reenviado ao FeuVault e só então
/// apagado por `run` ao fim de cada sessão, ver `upload_and_cleanup`;
/// restaurado de lá sozinho na próxima vez que faltar, logo abaixo), depois
/// pirata (ver `scan::resolve_piratas_install_dir` pra prioridade completa
/// — esse nunca é apagado por `run`, não tem retrato remoto equivalente).
/// Sem nenhum dos dois configurados/resolvíveis: `None`, comportamento
/// idêntico ao pré-Fase VII.
fn detect_full_install(
    cfg: &Config,
    app_id: &str,
    steam_root: Option<&Path>,
    ssh_bin: &str,
) -> Result<(Option<PathBuf>, Option<FullInstallTarget>), RunError> {
    if let Some(root) = &cfg.local_game_root_steam {
        // Sem manifesto local, tenta restaurar o retrato salvo no FeuVault
        // no momento do upload (ver `ingest.rs`/`steam-manifests/<app_id>.acf`)
        // antes de desistir — incidente real (2026-08-30): a própria Steam
        // pode "esquecer" um jogo migrado ao limpar sozinha o manifesto
        // fantasma que `run`/`ingest` deliberadamente preservam (ver
        // `cleanup_local`). Sem essa restauração, o jogo simplesmente
        // desaparece do vault-sync até alguém notar e restaurar na mão.
        let installdir = install_paths::manifest_installdir(root, app_id)
            .or_else(|| restore_official_manifest_snapshot(cfg, app_id, root, ssh_bin));
        if let Some(installdir) = installdir {
            let remote_base = cfg
                .remote_installs_base_path
                .clone()
                .ok_or_else(|| RunError::FullInstallNotConfigured(app_id.to_string()))?;
            let local_dir = root.join("steamapps").join("common").join(&installdir);
            // Mesma guarda de `install_paths::verified_candidate` — `root`
            // vem de config.json, mas mesmo assim nunca confiamos num
            // caminho que vá alimentar `remove_dir_all` sem checar.
            if install_paths::is_safe_install_dir(&local_dir, dirs::home_dir().as_deref()) {
                let target = FullInstallTarget {
                    local_dir,
                    remote_dir: remote_base.join("steam").join(installdir),
                };
                return Ok((Some(root.clone()), Some(target)));
            }
        }

        // Nem manifesto local nem retrato remoto sobreviveram (incidente
        // real, 2026-09-09: Kenshi — `compatdata` só local, `appmanifest`
        // nunca foi enviado ao FeuVault, então não há retrato pra
        // restaurar). `install_path_overrides.steam` é o único mapeamento
        // app_id<->pasta que sobrevive a essa lacuna dupla, mesmo raciocínio
        // de `install_path_overrides.piratas` logo abaixo — sem ele, um
        // jogo oficial nessa situação nunca teria como saber onde baixar a
        // instalação completa, mesmo já reconhecido no catálogo (ver
        // `catalog::augment_remote_with_full_install_only_steam_titles`).
        if let Some(local_dir) = cfg.install_path_overrides.steam.get(app_id) {
            if install_paths::is_safe_install_dir(local_dir, dirs::home_dir().as_deref()) {
                let remote_base = cfg
                    .remote_installs_base_path
                    .clone()
                    .ok_or_else(|| RunError::FullInstallNotConfigured(app_id.to_string()))?;
                let name = local_dir
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| app_id.to_string());
                let target = FullInstallTarget {
                    local_dir: local_dir.clone(),
                    remote_dir: remote_base.join("steam").join(name),
                };
                return Ok((Some(root.clone()), Some(target)));
            }
        }
    }

    // Override manual > raiz adotada (`local_game_root_piratas`) > `StartDir`
    // ao vivo do atalho (legado) — ver `scan::resolve_piratas_install_dir`.
    // Não exige existir no disco agora: é um alvo de download válido mesmo
    // vazio (jogo "fora", só no FeuVault).
    let numeric_app_id = app_id.parse::<u32>().ok();
    let local_dir = numeric_app_id.and_then(|id| {
        scan::resolve_piratas_install_dir(
            cfg.install_path_overrides
                .piratas
                .get(app_id)
                .map(PathBuf::as_path),
            cfg.local_game_root_piratas.as_deref(),
            steam_root,
            id,
        )
    });
    if let Some(local_dir) = local_dir {
        // `StartDir`/override vêm de metadado de launcher externo (Lutris/
        // atalho non-Steam) — não confiável sem checar. Incidente real: um
        // atalho Lutris tinha StartDir=$HOME, isso quase apagou a home
        // inteira via o `remove_dir_all` mais abaixo (ver
        // `install_paths::is_safe_install_dir`).
        if install_paths::is_safe_install_dir(&local_dir, dirs::home_dir().as_deref()) {
            let remote_base = cfg
                .remote_installs_base_path
                .clone()
                .ok_or_else(|| RunError::FullInstallNotConfigured(app_id.to_string()))?;
            let name = local_dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| app_id.to_string());
            let target = FullInstallTarget {
                local_dir,
                remote_dir: remote_base.join("piratas").join(name),
            };
            // Piratas nunca muda qual biblioteca hospeda o compatdata — só a
            // instalação em si é rastreada separadamente (StartDir é livre).
            return Ok((None, Some(target)));
        }
    }

    Ok((None, None))
}

/// Devolve `Some(&full)` quando `app_id` tem instalação completa rastreada
/// (`full_install`) mas nenhum `appmanifest_<app_id>.acf` existe em
/// `local_game_root_steam` agora — o gatilho exato pra rodar `vault-sync
/// update` (steamcmd validate) antes do `-applaunch` (ver chamador em
/// `run_via_steam_applaunch_with_options`). `None` em qualquer outro caso
/// (sem instalação completa rastreada, sem `local_game_root_steam`
/// configurado, ou manifesto já presente) — nesses casos o `-applaunch`
/// sozinho já funciona normalmente, sem custo extra de rede.
fn full_install_missing_steam_manifest<'a>(
    cfg: &Config,
    app_id: &str,
    full_install: &'a Option<FullInstallTarget>,
) -> Option<&'a FullInstallTarget> {
    let full = full_install.as_ref()?;
    let root = cfg.local_game_root_steam.as_ref()?;
    let manifest_path = root
        .join("steamapps")
        .join(format!("appmanifest_{app_id}.acf"));
    if manifest_path.exists() {
        return None;
    }
    Some(full)
}

/// Busca, no FeuVault, o `appmanifest_<app_id>.acf` salvo por `ingest.rs`
/// junto com a instalação completa (`steam-manifests/<app_id>.acf`, ver
/// `ingest::upload_manifest_snapshot`) e escreve de volta em
/// `<local_game_root_steam>/steamapps/`, devolvendo o `installdir` lido —
/// universal pra qualquer jogo Steam oficial com instalação completa
/// rastreada, não um caso especial de nenhum jogo específico. `None` em
/// qualquer ponto (sem `remote_installs_base_path` configurado, sem
/// retrato salvo pra este `app_id`, falha de rede, escrita local falha) só
/// significa "nada pra restaurar aqui" — best-effort, nunca propaga erro:
/// o chamador (`detect_full_install`) já lida bem com "nenhuma instalação
/// completa rastreada", que é exatamente o estado pré-existente sem esta
/// função.
fn restore_official_manifest_snapshot(
    cfg: &Config,
    app_id: &str,
    local_game_root_steam: &Path,
    ssh_bin: &str,
) -> Option<String> {
    let remote_installs = cfg.remote_installs_base_path.as_ref()?;
    let remote_manifest = remote_installs
        .join("steam-manifests")
        .join(format!("{app_id}.acf"));
    let content =
        crate::remote::read_remote_file(ssh_bin, &cfg.remote_ssh_target, &remote_manifest)
            .ok()
            .flatten()?;
    let installdir = install_paths::parse_installdir_from_acf(&content)?;

    let local_manifest = local_game_root_steam
        .join("steamapps")
        .join(format!("appmanifest_{app_id}.acf"));
    std::fs::create_dir_all(local_manifest.parent()?).ok()?;
    std::fs::write(&local_manifest, &content).ok()?;

    tracing::info!(
        app_id,
        installdir,
        server = cfg.server_name(),
        "manifesto restaurado do servidor — a Steam tinha esquecido este jogo"
    );
    Some(installdir)
}

/// Garante que o `installed.json` do Legendary tenha uma entrada pra
/// `title` cujo `install_path` bate com `local_dir` — o caminho que o
/// vault-sync acabou de resolver/baixar pra essa sessão. Sem isto,
/// `legendary launch` reporta "not currently installed" mesmo com os
/// arquivos certinhos no disco, porque ele confia cegamente no que está
/// gravado no JSON, nunca verifica o disco por conta própria (incidente
/// real, 2026-09-07, "Nobody Wants to Die"). Dois casos:
/// - Já existe uma entrada pra esse título, só com `install_path`
///   desatualizado (jogo migrado de disco, ou restaurado do FeuVault num
///   caminho diferente de onde o Legendary lembra): corrige só esse
///   campo — mais barato e mais robusto que mover arquivos pra bater com
///   o que o `installed.json` "lembra".
/// - Não existe entrada nenhuma pra esse título ainda: sem os metadados
///   reais da Epic (`app_name`, executável, versão — só a Epic sabe
///   gerar isso) não dá pra inventar uma entrada do zero. Restaura de um
///   retrato do `installed.json` salvo no FeuVault no momento do
///   `ingest` (`ingest::upload_legendary_installed_json_snapshot`) —
///   mesmo princípio de `restore_official_manifest_snapshot` acima, só
///   que pro Legendary.
///
/// Melhor esforço em tudo (sem `installed.json` local nem remoto, JSON
/// corrompido, sem permissão de escrita): nunca derruba a sessão, só
/// registra um aviso — o `legendary launch` que vem a seguir reporta seu
/// próprio erro claro se isso não bastar pra resolver.
///
/// `installed_json_path` injetado em vez de sempre resolver o caminho
/// real do sistema (`dirs::config_dir()`, não redirecionável em teste) —
/// chamadores reais resolvem via `install_paths::
/// default_legendary_installed_json_path()`.
fn ensure_legendary_knows_about_install_at(
    cfg: &Config,
    title: &str,
    local_dir: &Path,
    ssh_bin: &str,
    installed_json_path: &Path,
) {
    let mut map: serde_json::Map<String, serde_json::Value> =
        std::fs::read_to_string(&installed_json_path)
            .ok()
            .and_then(|content| serde_json::from_str(&content).ok())
            .unwrap_or_default();

    let local_dir_value = serde_json::Value::String(local_dir.display().to_string());
    let existing_key = map
        .iter()
        .find(|(_, v)| v.get("title").and_then(|t| t.as_str()) == Some(title))
        .map(|(k, _)| k.clone());

    if let Some(key) = existing_key {
        let needs_fix = map
            .get(&key)
            .and_then(|v| v.get("install_path"))
            .is_none_or(|p| p != &local_dir_value);
        if !needs_fix {
            return;
        }
        if let Some(obj) = map.get_mut(&key).and_then(|v| v.as_object_mut()) {
            obj.insert("install_path".to_string(), local_dir_value);
        }
        if write_legendary_installed_json(&installed_json_path, &map).is_ok() {
            tracing::info!(
                title,
                "install_path do Legendary corrigido pra bater com o que o vault-sync resolveu"
            );
        }
        return;
    }

    let Some(remote_installs) = &cfg.remote_installs_base_path else {
        return;
    };
    let remote_snapshot = remote_installs
        .join("epic-manifests")
        .join("installed.json");
    let Ok(Some(content)) =
        crate::remote::read_remote_file(ssh_bin, &cfg.remote_ssh_target, &remote_snapshot)
    else {
        return;
    };
    let Ok(remote_map) =
        serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&content)
    else {
        return;
    };
    let Some((snapshot_key, mut snapshot_entry)) = remote_map
        .into_iter()
        .find(|(_, v)| v.get("title").and_then(|t| t.as_str()) == Some(title))
    else {
        return;
    };
    if let Some(obj) = snapshot_entry.as_object_mut() {
        obj.insert("install_path".to_string(), local_dir_value);
    }
    map.insert(snapshot_key, snapshot_entry);
    if write_legendary_installed_json(&installed_json_path, &map).is_ok() {
        tracing::info!(
            title,
            server = cfg.server_name(),
            "entrada do Legendary restaurada do retrato salvo no servidor"
        );
    }
}

fn write_legendary_installed_json(
    path: &Path,
    map: &serde_json::Map<String, serde_json::Value>,
) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(map)?;
    std::fs::write(path, text)
}

/// Contexto resolvido de uma sessão: quem/onde, já com compatdata +
/// instalação completa (se rastreada) garantidos localmente — baixados do
/// FeuVault quando faltando. Compartilhado pelas duas variantes de ciclo de
/// vida que precisam desse "traz de volta se faltar": `run_session_with_
/// options` (spawna e supervisiona o processo direto) e
/// `run_via_steam_applaunch_with_options` (jogos Steam oficiais lançados
/// via `steam -applaunch` pelo menu, onde não dá pra supervisionar o
/// processo real — ver `dispatch_run`/`MenuLaunch` em `main.rs`).
struct ResolvedSession {
    source: GameSource,
    vault: RemoteVault,
    remote_paths: crate::paths::RemotePaths,
    local_targets: LocalDeletionTargets,
    full_install: Option<FullInstallTarget>,
    // Mantido vivo pelo chamador enquanto a sessão estiver em andamento —
    // liberado automaticamente no Drop.
    _lock: LockGuard,
}

/// Verdadeiro se `dir` (ou qualquer subpasta dele, recursivamente) contém
/// pelo menos um arquivo comum. Usado pra distinguir uma instalação real
/// (mesmo parcial) de uma árvore de pastas vazia que a Steam cria sozinha
/// antes de começar a baixar algo — sem depender do tamanho esperado de
/// nenhum jogo específico, então funciona igual pra qualquer AppID.
fn dir_has_any_file(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_file() {
            return true;
        }
        if file_type.is_dir() && dir_has_any_file(&entry.path()) {
            return true;
        }
    }
    false
}

#[allow(clippy::too_many_arguments)]
fn resolve_and_download(
    cfg: &Config,
    launcher: Launcher,
    app_id: &str,
    steam_root: Option<&Path>,
    rsync_bin: &str,
    ssh_bin: &str,
    lock_path: &Path,
    mut on_progress: impl FnMut(Stage, ProgressUpdate),
) -> Result<ResolvedSession, RunError> {
    let (source, library_root, remote_base_path, full_install) = match launcher {
        Launcher::Steam => {
            let numeric_app_id: u32 = app_id
                .trim()
                .parse()
                .map_err(|_| RunError::InvalidAppId(app_id.to_string()))?;
            let source = GameSource::Official {
                app_id: numeric_app_id,
            };
            let (library_override, full_install) =
                detect_full_install(cfg, app_id, steam_root, ssh_bin)?;
            let library_root = match library_override {
                Some(root) => root,
                None => cfg
                    .local_libraries
                    .first()
                    .ok_or(RunError::NoLocalLibrary)?
                    .clone(),
            };
            (
                source,
                library_root,
                cfg.remote_base_path.clone(),
                full_install,
            )
        }
        Launcher::Heroic => {
            let source = GameSource::HeroicGame {
                title: app_id.to_string(),
            };
            let library_root = cfg
                .heroic_local_root
                .clone()
                .ok_or(RunError::HeroicNotConfigured)?;
            let remote_base_path = cfg
                .heroic_remote_base_path
                .clone()
                .ok_or(RunError::HeroicNotConfigured)?;
            // Incidente real (2026-09-07, "Nobody Wants to Die"): antes,
            // `local_dir` sempre era `root.join(app_id)` — o título do
            // Heroic ao pé da letra, com espaços — ignorando
            // `install_path_overrides.epic` por completo (diferente do
            // ramo Steam acima, que sempre confere o override primeiro).
            // `ingest`/`sync_full_install` já respeitava o override e
            // nomeava a pasta remota a partir do BASENAME do `local_dir`
            // resolvido (`NobodyWantsToDie`, sem espaço, o nome real da
            // pasta configurada) — resultado: `run` tentava baixar de
            // `.../epic/Nobody Wants to Die` (nunca existiu) enquanto os
            // dados de verdade estavam em `.../epic/NobodyWantsToDie`,
            // falhando sempre com o mesmo erro de rsync, determinístico,
            // não uma instabilidade de rede como pareceu à primeira vista.
            let full_install = match (&cfg.local_game_root_epic, &cfg.remote_installs_base_path) {
                (Some(root), Some(remote_base)) => {
                    let local_dir = cfg
                        .install_path_overrides
                        .epic
                        .get(app_id)
                        .cloned()
                        .unwrap_or_else(|| root.join(app_id));
                    let name = local_dir
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| app_id.to_string());
                    Some(FullInstallTarget {
                        local_dir,
                        remote_dir: remote_base.join("epic").join(name),
                    })
                }
                _ => None,
            };
            (source, library_root, remote_base_path, full_install)
        }
    };

    let library = LibraryLocation::new(library_root);
    let vault = RemoteVault {
        ssh_target: cfg.remote_ssh_target.clone(),
        base_path: remote_base_path,
    };

    let remote_paths = vault.paths_for(&source)?;
    let local_targets = library.deletion_targets(&source)?;

    // 2.3: barreira anti-concorrência. Mantida viva (via `_lock`, devolvida
    // ao chamador) durante o download aqui, e por todo o resto da sessão
    // quando quem chamou é `run_session_with_options` — liberada
    // automaticamente no Drop, inclusive em qualquer caminho de erro.
    let _lock = LockGuard::acquire(lock_path)?;

    // 3.1 (aquisição integral) + barreira da Fase II: uma falha de rede aqui
    // bloqueia imediatamente a inicialização do jogo, nunca é silenciada.
    //
    // Incidente real (2026-09-07, "Nobody Wants to Die"): isto rodava
    // incondicionalmente, mesmo quando o jogo nunca tinha sido aberto nem
    // uma vez — sem prefixo Wine local NEM remoto (nada pra `ingest` ter
    // subido ainda). `rsync` tentando entrar numa pasta remota que nunca
    // existiu falha com "No such file or directory" (código 23), idêntico
    // ao código de saída de uma instabilidade de rede real — mas
    // `is_missing_source_dir` (rsync.rs) reconhece esse caso específico
    // pelo texto de erro do próprio rsync ("change_dir ... failed: No such
    // file or directory"), sem round-trip extra de rede: só esse caso
    // exato é tratado como "nada sincronizado ainda" (não é erro nenhum);
    // qualquer outra causa de falha continua bloqueando igual antes.
    if let Err(err) = RsyncTransfer::full_download(
        &vault.ssh_target,
        &remote_paths.compatdata_dir,
        &local_targets.compatdata_dir,
    )
    .with_binary(rsync_bin)
    .execute_with_progress(|u| on_progress(Stage::Download, u))
    {
        if !err.is_missing_source_dir() {
            return Err(RunError::Download(err));
        }
    }

    // Fase VII: instalação completa, só quando rastreada — mesma barreira
    // (falha de rede bloqueia antes do jogo abrir). Pulado quando o
    // diretório já existe local E tem pelo menos um arquivo de verdade
    // dentro (jogo "em casa" de uma sessão anterior que não chegou a
    // fechar, ou primeira adoção onde a Steam/Heroic acabaram de instalar
    // ali direto — nada a baixar). Também é o que "traz de volta" uma
    // instalação fantasma (manifesto presente, pasta ausente — ver
    // incidente do Cyberpunk 2077 em 2026-09-12: `full.local_dir.exists()`
    // é sempre checado no disco de verdade, nunca confia no manifesto da
    // Steam).
    //
    // Incidente real (2026-09-20, Cyberpunk 2077 de novo): a Steam já tinha
    // criado a árvore de pastas do jogo (bin/, engine/, r6/, archive/...)
    // sozinha ao começar um download próprio pela CDN antes de `run` rodar
    // — pasta existe, mas vazia (zero arquivos). `local_dir.exists() ==
    // true` sozinho bastava pra pular o download do servidor inteiro,
    // deixando a instalação eternamente vazia (loop de `-applaunch`
    // falhando por nunca achar o jogo de verdade — ver
    // `SteamLaunchTimeout`). `dir_has_any_file` cobre isso sem precisar
    // saber o tamanho esperado de nenhum jogo específico: qualquer
    // instalação real tem arquivo, uma árvore de pastas fantasma da Steam
    // não tem nenhum.
    if let Some(full) = &full_install {
        let already_present = full.local_dir.exists() && dir_has_any_file(&full.local_dir);
        if !already_present {
            if full.local_dir.exists() {
                let _ = std::fs::remove_dir_all(&full.local_dir);
            }
            if let Err(err) =
                RsyncTransfer::full_download(&vault.ssh_target, &full.remote_dir, &full.local_dir)
                    .with_binary(rsync_bin)
                    .execute_with_progress(|u| on_progress(Stage::Download, u))
            {
                // Incidente real (2026-09-06, Phonopolis): rsync interrompido
                // no meio (rede caiu, `run` chamado sem contexto completo,
                // etc.) deixa uma pasta parcial em `full.local_dir` — sem
                // esta limpeza, a PRÓXIMA sessão veria `local_dir.exists() ==
                // true` acima e pularia o download inteiro, achando que o
                // jogo já está todo aqui. Resultado real: `run` "concluído"
                // sem nunca ter baixado nada, e o `upload_and_cleanup` do
                // fim do ciclo teria sincronizado esse lixo parcial de volta
                // pro FeuVault como se fosse a cópia boa. Melhor esforço —
                // nunca mascara o erro original de download por causa de uma
                // falha secundária ao limpar.
                let _ = std::fs::remove_dir_all(&full.local_dir);
                return Err(RunError::Download(err));
            }
        }
    }

    Ok(ResolvedSession {
        source,
        vault,
        remote_paths,
        local_targets,
        full_install,
        _lock,
    })
}

/// Um processo que sai com erro em menos deste tempo quase certamente nunca
/// chegou a abrir de verdade (launcher recusando o lançamento, dependência
/// ausente, config quebrada) — folga generosa acima do que qualquer engine
/// leva só pra inicializar gráficos/áudio, pra não confundir com uma sessão
/// de verdade que o Proton só encerra com um status não-zero (comportamento
/// legítimo já documentado onde este limiar é checado).
const GAME_EXIT_TOO_FAST_THRESHOLD: Duration = Duration::from_secs(5);

#[allow(clippy::too_many_arguments)]
fn run_session_with_options(
    cfg: &Config,
    launcher: Launcher,
    app_id: &str,
    exec: &[String],
    steam_root: Option<&Path>,
    rsync_bin: &str,
    ssh_bin: &str,
    lock_path: &Path,
    legendary_installed_json_path: Option<&Path>,
    mut on_progress: impl FnMut(Stage, ProgressUpdate),
) -> Result<GameSession, RunError> {
    let ResolvedSession {
        source,
        vault,
        remote_paths,
        local_targets,
        full_install,
        _lock,
    } = resolve_and_download(
        cfg,
        launcher,
        app_id,
        steam_root,
        rsync_bin,
        ssh_bin,
        lock_path,
        &mut on_progress,
    )?;

    let (program, args) = exec.split_first().ok_or(RunError::EmptyExecCommand)?;

    let mut session = GameSession::new(source);

    // 3.1: `Command` herda todo o ambiente do processo pai por padrão (sem
    // `.env_clear()`), garantindo que DISPLAY/WAYLAND_DISPLAY/XAUTHORITY e
    // demais variáveis da infraestrutura gráfica cheguem ao processo filho.
    // stdin/stdout/stderr também são herdados por padrão, preservando a
    // interação normal do jogo com o terminal/compositor.
    //
    // Sempre expõe `STEAM_COMPAT_DATA_PATH` apontando pro mesmo
    // `local_targets.compatdata_dir` que acabou de ser baixado (e vai ser
    // sincronizado de volta ao fim da sessão) — é isso que faz `proton run
    // <exe>` (invocado por `MenuLaunch`/main.rs pra atalhos non-Steam, ver
    // `proton.rs`) usar o prefixo (`$STEAM_COMPAT_DATA_PATH/pfx`) certo, em
    // vez de deixar o `binfmt_misc` do sistema decidir sozinho um prefixo
    // que nunca seria sincronizado com o FeuVault — incidente real
    // (2026-08-30) que motivou isso. Inócuo pros outros `program` possíveis
    // (Heroic usa `WINEPREFIX` próprio via `legendary`; os testes deste
    // módulo usam `/bin/sh`) — só passa a importar de fato quando `program`
    // é um script `proton`.
    let mut command = Command::new(program);
    command.args(args);
    command.env("STEAM_COMPAT_DATA_PATH", &local_targets.compatdata_dir);
    if let Some(root) = steam_root {
        command.env("STEAM_COMPAT_CLIENT_INSTALL_PATH", root);
    }
    // Incidente real (2026-09-07, "Nobody Wants to Die"): sem isto,
    // `legendary launch` (o `program` real pra `Launcher::Heroic`, ver
    // `GameSource::HeroicGame` em main.rs) fala com o `~/.config/legendary`
    // padrão do sistema — vazio, sem login, sem `installed.json` — em vez
    // da config de verdade que o próprio Heroic mantém separada.
    // `default_legendary_installed_json_path` já resolve esse caminho
    // padrão sem precisar de config manual (mesmo princípio de
    // `scan::find_steam_root`, já usado por `ingest.rs` pra descoberta —
    // só faltava usar isso também aqui, na hora de lançar). Resultado sem
    // isto: "not currently installed" pra QUALQUER jogo Heroic, mesmo um
    // que o Heroic gráfico reconhece e abre normalmente. Inócuo pros
    // outros `program` possíveis (Proton/piratas ignoram essa variável).
    if let Some(installed_json) = install_paths::default_legendary_installed_json_path() {
        if let Some(legendary_config) = installed_json.parent() {
            command.env("LEGENDARY_CONFIG_PATH", legendary_config);
        }
    }
    // Só pra Heroic, e só quando há instalação completa rastreada (sem
    // isso não sabemos qual `install_path` o Legendary deveria ter) —
    // ver `ensure_legendary_knows_about_install_at` pra por quê.
    if launcher == Launcher::Heroic {
        if let (Some(full), Some(installed_json)) = (&full_install, legendary_installed_json_path) {
            ensure_legendary_knows_about_install_at(
                cfg,
                app_id,
                &full.local_dir,
                ssh_bin,
                installed_json,
            );
        }
    }
    let mut child = command.spawn().map_err(RunError::GameSpawn)?;
    session.child_pid = Some(child.id());

    on_progress(
        Stage::Running,
        ProgressUpdate {
            percent: 0,
            rate: String::new(),
        },
    );

    // 3.2/3.3: `.wait()` bloqueia via waitpid — custo de CPU zero na thread
    // orquestradora até o kernel notificar o término do processo filho.
    let spawned_at = Instant::now();
    let status = child.wait().map_err(RunError::GameSpawn)?;
    if !status.success() {
        // Incidente real (2026-09-09, Nobody Wants to Die): um Legendary
        // que recusa o lançamento ("not currently installed" — installed.json
        // sem a entrada, ver `ensure_legendary_knows_about_install_at`) sai
        // com erro quase instantaneamente. Sem esta checagem, o ciclo seguia
        // pra "sincronizado com sucesso" como se o jogo tivesse rodado —
        // mascarando uma falha real atrás de um log que parecia êxito.
        // Proton legitimamente às vezes sai com status estranho numa sessão
        // de verdade (por isso o limiar generoso, não qualquer código != 0
        // vira falha dura) — só trata como falha quando as duas coisas
        // batem: saiu rápido demais E com erro.
        if spawned_at.elapsed() < GAME_EXIT_TOO_FAST_THRESHOLD {
            return Err(RunError::GameExitedImmediately {
                status: status.to_string(),
                elapsed_secs: spawned_at.elapsed().as_secs(),
            });
        }
        tracing::warn!(
            "processo do jogo encerrou com status {status:?} — sincronizando de volta mesmo assim"
        );
    }

    upload_and_cleanup(
        cfg,
        &session.source,
        &vault,
        &local_targets,
        &remote_paths,
        &full_install,
        rsync_bin,
        legendary_installed_json_path,
        PREFIX_QUIET_TIMEOUT,
        &mut on_progress,
    )?;

    Ok(session)
}

/// 3.3/3.4: sincronização diferencial de volta (estritamente sobre
/// compatdata + instalação completa rastreada, nunca a biblioteca inteira)
/// seguida de limpeza local — só chega a limpar com o(s) upload(s)
/// confirmado(s) (exit code 0).
///
/// Incidente real (2026-09-20/21, Cyberpunk 2077): antes, o manifesto
/// (Steam oficial) de uma instalação completa rastreada era sempre
/// preservado localmente após a limpeza — única memória persistente do
/// mapeamento app_id→pasta entre sessões (ver `detect_full_install`), pra
/// evitar o incidente de 2026-08-30 onde a própria Steam "esquecia"
/// sozinha o jogo. Só que isso tem um efeito colateral pior: um manifesto
/// presente sem os arquivos do jogo (que `run` acabou de apagar de
/// propósito) faz a Steam achar que a instalação está corrompida/faltando
/// e disparar um download completo pela CDN da Valve — ignorando
/// completamente que os dados certos estão no servidor. Agora o manifesto
/// é enviado pro FeuVault (mesmo retrato que `ingest::upload_manifest_snapshot`
/// já mantinha, só que aqui, dentro do próprio ciclo do `run`, sem
/// depender de quando um `ingest --full` rodou por último) e só então
/// apagado local — `detect_full_install`/`restore_official_manifest_snapshot`
/// já sabiam restaurar esse retrato quando o manifesto local está ausente,
/// então a próxima sessão recupera sozinha, sem a Steam nunca ver um
/// manifesto órfão apontando pro nada. Se o upload falhar (rede instável),
/// o manifesto local é preservado como estava antes — sem os dois lados
/// nunca fica de propósito.
///
/// Extraído pra ser reaproveitado tanto por `run_session_with_options`
/// (jogo controlado diretamente) quanto por `run_via_steam_applaunch_with_options`
/// (jogo controlado via `steam -applaunch` + supervisão do `reaper`) — a
/// garantia de "só sincroniza depois que o jogo de verdade fechou" é
/// idêntica nos dois casos, só o jeito de detectar o fechamento muda.
#[allow(clippy::too_many_arguments)]
fn upload_and_cleanup(
    cfg: &Config,
    source: &GameSource,
    vault: &RemoteVault,
    local_targets: &LocalDeletionTargets,
    remote_paths: &crate::paths::RemotePaths,
    full_install: &Option<FullInstallTarget>,
    rsync_bin: &str,
    legendary_installed_json_path: Option<&Path>,
    prefix_quiet_timeout: Duration,
    mut on_progress: impl FnMut(Stage, ProgressUpdate),
) -> Result<(), RunError> {
    // O jogo fechou, mas o prefixo pode continuar vivo (serviços do EA app,
    // wineserver ainda gravando o registro) — ver `PREFIX_QUIET_TIMEOUT`.
    let app_id = source.app_id_str();
    wait_for_prefix_quiet(
        &app_id,
        &local_targets.compatdata_dir,
        prefix_quiet_timeout,
        "nada foi enviado ao servidor nem apagado localmente",
    )?;

    // Incidente real (2026-09-07, "Nobody Wants to Die", segundo capítulo):
    // depois de corrigir o lado do download, o mesmo erro apareceu no
    // upload — o jogo abriu mas crashou antes de escrever qualquer coisa
    // no prefixo Wine local, então não havia `local_targets.compatdata_dir`
    // nenhum pra sincronizar de volta. `rsync` reportava isso com o mesmo
    // código 23 de sempre. Só esse caso específico (nada local pra enviar)
    // é seguro pular — a limpeza abaixo já lida bem com "nada aqui"
    // (`cleanup_local`/`delete_local_targets` já checam `.exists()` antes
    // de apagar).
    if let Err(err) = RsyncTransfer::incremental_upload(
        &vault.ssh_target,
        &local_targets.compatdata_dir,
        &remote_paths.compatdata_dir,
    )
    .with_binary(rsync_bin)
    .execute_with_progress(|u| on_progress(Stage::Upload, u))
    {
        if !err.is_missing_source_dir() {
            return Err(RunError::Upload(err));
        }
    }

    if let Some(full) = full_install {
        if let Err(err) =
            RsyncTransfer::incremental_upload(&vault.ssh_target, &full.local_dir, &full.remote_dir)
                .with_binary(rsync_bin)
                .execute_with_progress(|u| on_progress(Stage::Upload, u))
        {
            if !err.is_missing_source_dir() {
                return Err(RunError::Upload(err));
            }
        }
    }

    // Só jogos Steam oficiais têm manifesto pra proteger — `GameSource::HeroicGame`
    // e piratas não passam por `appmanifest_*.acf` nenhum.
    let manifest_safe_to_drop = match (source, full_install, &cfg.local_game_root_steam) {
        (GameSource::Official { app_id }, Some(_), Some(local_game_root_steam)) => {
            match &cfg.remote_installs_base_path {
                Some(remote_installs_base_path) => crate::ingest::upload_manifest_snapshot(
                    &vault.ssh_target,
                    rsync_bin,
                    remote_installs_base_path,
                    local_game_root_steam,
                    &app_id.to_string(),
                ),
                // Sem `remote_installs_base_path` configurado, o resto da
                // Fase VII (instalação completa) já nem se aplicaria aqui
                // (`full_install` só vem `Some` quando esse campo existe —
                // ver `detect_full_install`) — mantido só por exaustividade.
                None => true,
            }
        }
        _ => true,
    };
    // Segunda checagem, logo antes de apagar: algo pode ter voltado a usar
    // o prefixo durante o envio (ex.: "Jogar" clicado direto na Steam, fora
    // do vault-sync). Normalmente volta na hora.
    wait_for_prefix_quiet(
        &app_id,
        &local_targets.compatdata_dir,
        prefix_quiet_timeout,
        "o envio ao servidor terminou, mas a cópia local NÃO foi apagada",
    )?;
    cleanup_local(local_targets, full_install.is_some() && !manifest_safe_to_drop)?;
    if let Some(full) = full_install {
        if full.local_dir.exists() {
            std::fs::remove_dir_all(&full.local_dir).map_err(RunError::Cleanup)?;
            // Sem isto, o `installed.json` do Legendary continua afirmando
            // que o jogo está instalado na pasta que acabamos de apagar —
            // o catálogo mostraria o jogo como "Cliente"/"Cliente+Servidor"
            // até o próximo `ingest --full` limpar, mesmo já 100%
            // sincronizado de volta (incidente real, 2026-09-07). Melhor
            // esforço deliberado (só aviso, nunca falha o `run`): o próximo
            // `run` já repara sozinho via `ensure_legendary_knows_about_install_at`,
            // então uma falha aqui não compromete o próximo lançamento, só
            // a exibição do catálogo até lá.
            if let (GameSource::HeroicGame { title }, Some(installed_json)) =
                (source, legendary_installed_json_path)
            {
                clean_stale_legendary_entry_at(title, installed_json);
            }
        }
    }

    Ok(())
}

/// Limpa a entrada de `title` no `installed.json` do Legendary depois que
/// `upload_and_cleanup` (ou `purge::run_purge_with_options`) já apagou a
/// pasta de instalação completa correspondente. `installed_json_path`
/// injetado em vez de sempre resolver o caminho real do sistema
/// (`dirs::config_dir()`, não redirecionável em teste). Melhor esforço
/// deliberado (só aviso, nunca propaga erro): o próximo `run` já repara
/// sozinho via `ensure_legendary_knows_about_install_at`.
pub(crate) fn clean_stale_legendary_entry_at(title: &str, installed_json_path: &Path) {
    if !installed_json_path.is_file() {
        return;
    }
    if let Err(e) = install_paths::remove_legendary_installed_entry(installed_json_path, title) {
        tracing::warn!(
            title,
            "falha ao limpar entrada obsoleta do installed.json do Legendary: {e}"
        );
    }
}

/// PID do processo `reaper` da Steam Linux Runtime pra `app_id`, se
/// alguma sessão estiver em andamento agora. `reaper` supervisiona a
/// árvore de processos inteira de UM lançamento (launcher + Proton + jogo
/// real) e só termina quando ela termina de verdade — inclusive quando
/// nada disso foi spawnado diretamente por nós (`steam -applaunch`
/// retorna quase na hora, ver `run_via_steam_applaunch`). Varre `/proc`
/// diretamente (sem crate extra) — mesmo espírito de baixo-nível já usado
/// no resto do projeto (`install_paths.rs`, `scan.rs`).
fn find_reaper_pid(app_id: &str) -> Option<u32> {
    find_reaper_pid_in("/proc", app_id)
}

fn find_reaper_pid_in(proc_dir: &str, app_id: &str) -> Option<u32> {
    let marker = format!("AppId={app_id}");
    let entries = std::fs::read_dir(proc_dir).ok()?;
    for entry in entries.flatten() {
        let pid_str = entry.file_name().to_string_lossy().into_owned();
        if !pid_str.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let Ok(comm) = std::fs::read_to_string(entry.path().join("comm")) else {
            continue;
        };
        if comm.trim() != "reaper" {
            continue;
        }
        let Ok(cmdline) = std::fs::read_to_string(entry.path().join("cmdline")) else {
            continue;
        };
        if cmdline_has_marker(&cmdline, &marker) {
            if let Ok(pid) = pid_str.parse() {
                return Some(pid);
            }
        }
    }
    None
}

/// `cmdline` em `/proc/<pid>/cmdline` é uma lista de argumentos separados
/// por byte nulo (não espaço) — extraído como função pura pra ser
/// testável sem tocar em `/proc` de verdade.
fn cmdline_has_marker(cmdline: &str, marker: &str) -> bool {
    cmdline.split('\0').any(|arg| arg == marker)
}

/// Quanto o fim de sessão espera o prefixo Wine do jogo ficar quieto
/// (nenhum processo com `WINEPREFIX` apontando pra ele) antes de enviar ao
/// servidor e apagar a cópia local. O registro do Wine (`system.reg`,
/// `user.reg`) é gravado pelo wineserver, que só encerra — e grava a última
/// versão — depois que o último processo do prefixo sai; o EA app deixa
/// serviços próprios (EABackgroundService, EADesktop, EALocalHostSvc)
/// rodando no prefixo depois que o jogo fecha. Hipótese ainda não provada
/// da investigação do The Sims 4 (2026-10-23, segundo download já veio com
/// o prefixo quebrado): enviar/apagar com o prefixo ainda ativo grava no
/// servidor um retrato com registro e arquivos fora de sincronia — e apaga
/// a cópia local a partir dele. Estourar o tempo nunca apaga nada (ver
/// `RunError::PrefixStillActive`); por isso pode ser generoso.
const PREFIX_QUIET_TIMEOUT: Duration = Duration::from_secs(180);
const PREFIX_QUIET_POLL_INTERVAL: Duration = Duration::from_secs(2);
const PREFIX_QUIET_LOG_INTERVAL: Duration = Duration::from_secs(30);

/// Identidade (dispositivo, inode) de um diretório — o mesmo critério que o
/// próprio wineserver usa pra decidir qual servidor atende qual prefixo.
/// Casa o mesmo diretório escrito de jeitos diferentes (barra final que o
/// Proton põe, `~/.steam/steam` vs. `~/.local/share/Steam`, biblioteca por
/// symlink), coisa que comparar o texto do caminho não faria.
fn dir_identity(path: &Path) -> Option<(u64, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.dev(), meta.ino()))
}

/// Identidade do `WINEPREFIX` de um processo (`proc_entry` = `/proc/<pid>`).
/// Todo processo Wine precisa dessa variável pra achar o próprio wineserver
/// — inclusive o wineserver em si, que a herda do primeiro cliente.
/// Resolve primeiro pela raiz do próprio processo (`/proc/<pid>/root`), que
/// enxerga o caminho do jeito que o contêiner da Steam Linux Runtime
/// enxerga; sem acesso a ela, cai pro caminho visto do host.
fn wineprefix_identity(proc_entry: &Path) -> Option<(u64, u64)> {
    let environ = std::fs::read(proc_entry.join("environ")).ok()?;
    let value = environ
        .split(|&b| b == 0)
        .find_map(|var| var.strip_prefix(b"WINEPREFIX="))?;
    let prefix = Path::new(OsStr::from_bytes(value));
    let seen_by_process = proc_entry
        .join("root")
        .join(prefix.strip_prefix("/").unwrap_or(prefix));
    dir_identity(&seen_by_process).or_else(|| dir_identity(prefix))
}

fn process_name(proc_entry: &Path) -> String {
    std::fs::read_to_string(proc_entry.join("comm"))
        .map(|c| c.trim().to_string())
        .unwrap_or_default()
}

/// `(pid, nome)` de cada processo em `proc_dir` usando `compatdata_dir`
/// como prefixo Wine: o Proton aponta `WINEPREFIX` pra `<compatdata>/pfx`, o
/// Wine puro (Heroic sem Proton) pro próprio diretório. Vazio quando
/// `compatdata_dir` nem existe — não há cópia local pra proteger. O próprio
/// vault-sync fica de fora: se alguém o lançar com `WINEPREFIX` no
/// ambiente, ele esperaria por si mesmo até o tempo acabar.
fn prefix_users_in(proc_dir: &Path, compatdata_dir: &Path) -> Vec<(u32, String)> {
    let targets: Vec<(u64, u64)> = [compatdata_dir.join("pfx"), compatdata_dir.to_path_buf()]
        .iter()
        .filter_map(|p| dir_identity(p))
        .collect();
    if targets.is_empty() {
        return Vec::new();
    }
    let Ok(entries) = std::fs::read_dir(proc_dir) else {
        return Vec::new();
    };
    let own_pid = std::process::id();
    entries
        .flatten()
        .filter_map(|entry| {
            Some((
                entry.file_name().to_str()?.parse::<u32>().ok()?,
                entry.path(),
            ))
        })
        .filter(|(pid, _)| *pid != own_pid)
        // `targets` tem no máximo 2 itens: o `contains` não vira custo quadrático.
        .filter(|(_, path)| wineprefix_identity(path).is_some_and(|id| targets.contains(&id)))
        .map(|(pid, path)| (pid, process_name(&path)))
        .collect()
}

/// Processos usando agora o prefixo Wine de `compatdata_dir` — também usado
/// por `ingest` pra não enviar/apagar um prefixo vivo.
pub(crate) fn prefix_users(compatdata_dir: &Path) -> Vec<(u32, String)> {
    prefix_users_in(Path::new("/proc"), compatdata_dir)
}

pub(crate) fn describe_processes(users: &[(u32, String)]) -> String {
    users
        .iter()
        .map(|(pid, name)| format!("{name} (pid {pid})"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Espera o prefixo de `compatdata_dir` ficar sem nenhum processo, até
/// `timeout`. Estourou: `RunError::PrefixStillActive`, com `outcome`
/// dizendo o que já tinha acontecido (quem chama nunca apaga nada depois
/// de um erro aqui).
fn wait_for_prefix_quiet(
    app_id: &str,
    compatdata_dir: &Path,
    timeout: Duration,
    outcome: &'static str,
) -> Result<(), RunError> {
    wait_for_prefix_quiet_with(
        timeout,
        PREFIX_QUIET_POLL_INTERVAL,
        PREFIX_QUIET_LOG_INTERVAL,
        || prefix_users(compatdata_dir),
    )
    .map_err(|busy| RunError::PrefixStillActive {
        app_id: app_id.to_string(),
        dir: compatdata_dir.display().to_string(),
        timeout_secs: timeout.as_secs(),
        processes: describe_processes(&busy),
        outcome,
    })
}

/// Núcleo testável de `wait_for_prefix_quiet`: `users` injetado em vez de
/// varrer `/proc`. `Err` traz quem ainda usava o prefixo quando o tempo
/// acabou; o progresso vai pro log a cada `log_interval` (a primeira vez
/// logo na primeira checagem ocupada).
fn wait_for_prefix_quiet_with(
    timeout: Duration,
    poll_interval: Duration,
    log_interval: Duration,
    mut users: impl FnMut() -> Vec<(u32, String)>,
) -> Result<(), Vec<(u32, String)>> {
    let started = Instant::now();
    let mut next_log = started;
    loop {
        let busy = users();
        if busy.is_empty() {
            return Ok(());
        }
        if started.elapsed() >= timeout {
            return Err(busy);
        }
        if Instant::now() >= next_log {
            tracing::info!(
                "prefixo Wine ainda em uso por {} — esperando ficar quieto antes de enviar e \
                 limpar ({}s de no máximo {}s)",
                describe_processes(&busy),
                started.elapsed().as_secs(),
                timeout.as_secs()
            );
            next_log += log_interval;
        }
        std::thread::sleep(poll_interval);
    }
}

/// Margem de segurança entre um `reaper` sumir e considerar a sessão
/// encerrada de vez — incidente real (2026-08-16): a Steam lança **mais de
/// um** `reaper` sequencial pro mesmo AppID (uma passada rápida de
/// verificação/`Install=1` antes, só depois o `reaper` de verdade da
/// sessão de jogo) — sem essa margem, o primeiro `reaper` (o de
/// verificação) some, a "sessão" é dada como encerrada, e o upload+limpeza
/// dispara com o jogo de verdade subindo por baixo, apagando os arquivos
/// que ele ainda estava tentando abrir. `pid_is_alive`/PID único não
/// bastam — a checagem precisa ser "existe ALGUM reaper pra esse AppID
/// agora", reavaliada continuamente, não "o PID que eu vi primeiro ainda
/// existe".
///
/// Segundo incidente real (2026-09-06, Kenshi): 10s não é margem
/// suficiente — o reaper de verificação some, a compilação de shaders do
/// jogo de verdade demora mais que isso pra terminar e só então o reaper
/// real aparece, e os 10s estouram no meio do caminho: `run` concluiu
/// "sincronizado e limpo" com `pid=None` em 27s (Kenshi nunca chegou a
/// abrir), apagando os 12GB da instalação local enquanto a Steam ainda
/// tentava subir o jogo de verdade por baixo — exatamente o cenário que
/// este mecanismo existe pra evitar, só que com uma janela grande demais
/// pra shader compile em jogos maiores. Custo de errar pra mais aqui é só
/// atrasar em segundos quando o jogo fecha de verdade; custo de errar pra
/// menos é apagar um jogo que ainda estava abrindo — a assimetria manda
/// pecar por excesso de margem.
///
/// Terceiro incidente real (2026-09-12, Cyberpunk 2077): 90s de novo não
/// bastou — a compilação de shaders de um jogo AAA grande (pior ainda logo
/// após a Steam ter acabado de validar/reparar a instalação inteira, cache
/// de shader zerado) passa fácil de um minuto e meio. Resultado idêntico ao
/// incidente do Kenshi: upload+limpeza disparou com o jogo ainda no meio da
/// compilação, apagando os 67GB da instalação completa local (o compatdata
/// já tinha sincronizado a tempo, então nenhum dado de save/registro se
/// perdeu — só a instalação, que precisa ser baixada de novo do FeuVault).
/// Mesma assimetria de custo do incidente anterior, só que agora pesando
/// mais pro lado de "espera mais": subiu de 90s pra 10 minutos.
const REAPER_GRACE_PERIOD: Duration = Duration::from_secs(600);
const REAPER_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// `comm` do processo real da Steam — `/usr/bin/steam` é só um wrapper de
/// shell que executa `/usr/lib/steam/steam` (confirmado nesta máquina via
/// `pacman -Qo`/leitura do script), então o nome que aparece em `/proc`
/// reflete o binário final, não o wrapper.
const STEAM_PROCESS_COMM: &str = "steam";
/// Tempo de graça pra Steam terminar de inicializar depois que o processo
/// aparece — só o processo existir não significa que a API/IPC interna já
/// está pronta pra receber um `-applaunch` (ver `ensure_steam_running_
/// silently`). Incidente real (2026-10-08, Red Dead Redemption 2): com os
/// 3s antigos, o `-applaunch` chegou enquanto a Steam ainda carregava
/// (nenhum `GameAction` no console_log) e se perdeu em silêncio — 10
/// minutos depois, `SteamLaunchTimeout`. Só é pago num boot frio da Steam
/// (já rodando, `ensure_steam_running_silently_with` retorna na hora),
/// então errar pra mais custa segundos uma vez; errar pra menos perde o
/// lançamento inteiro.
const STEAM_WARMUP_GRACE: Duration = Duration::from_secs(30);
/// Tempo máximo de espera pro processo da Steam aparecer num boot frio —
/// bem mais generoso que o de sessão de jogo (`startup_timeout` de
/// `wait_for_steam_session`), já que carregar a própria Steam do zero é
/// mais pesado que só despachar um `-applaunch` pra uma instância já de pé.
const STEAM_COLD_START_TIMEOUT: Duration = Duration::from_secs(30);
const STEAM_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Tempo máximo de espera pro `reaper` aparecer pela primeira vez depois de
/// um `-applaunch` pelo menu (`run_via_steam_applaunch`) — separado de
/// `REAPER_GRACE_PERIOD`, que só conta depois que o reaper já apareceu ao
/// menos uma vez (ver comentário de `wait_for_steam_session`). Incidente
/// real (2026-09-18, Risk of Rain 2): a Steam decidiu baixar uma atualização
/// pendente antes de lançar o jogo — nenhum `reaper` chega a existir
/// enquanto o download roda, então os 60s antigos estouravam bem antes do
/// download terminar, abortando o lançamento (`SteamLaunchTimeout`) com o
/// jogo nem chegando a abrir. Subiu pra 10 minutos, mesma generosidade de
/// `REAPER_GRACE_PERIOD` — mesma assimetria de custo dos outros incidentes
/// deste arquivo: esperar demais só atrasa por segundos quando o
/// lançamento falhou de verdade, esperar de menos aborta um download
/// legítimo no meio.
const STEAM_APPLAUNCH_STARTUP_TIMEOUT: Duration = Duration::from_secs(600);

fn is_steam_process_running_in(proc_dir: &str) -> bool {
    let Ok(entries) = std::fs::read_dir(proc_dir) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let pid_str = entry.file_name().to_string_lossy().into_owned();
        if !pid_str.chars().all(|c| c.is_ascii_digit()) {
            return false;
        }
        std::fs::read_to_string(entry.path().join("comm"))
            .map(|c| c.trim() == STEAM_PROCESS_COMM)
            .unwrap_or(false)
    })
}

fn is_steam_process_running() -> bool {
    is_steam_process_running_in("/proc")
}

/// Garante que a Steam já está rodando (mesmo escondida, via `-silent`)
/// antes de qualquer `-applaunch` — sem isso, cada lançamento cold-starta a
/// Steam inteira, o que (a) sempre mostra a janela principal por alguns
/// segundos, mesmo pra quem só queria abrir direto o jogo, e (b)
/// frequentemente estoura o tempo de espera do `reaper` num boot frio,
/// fazendo o vault-sync reportar falha mesmo quando a Steam eventualmente
/// teria conseguido (incidente real, 2026-09-09: Kenshi e Haste — dois
/// jogos distintos, mesma falha, mesmo dia, porque nenhum dos dois
/// lançamentos tinha uma Steam já de pé pra responder rápido). Se a Steam
/// já estiver rodando, retorna na hora — só paga o custo de boot quando é
/// mesmo necessário. Melhor esforço deliberado: mesmo sem confirmar que a
/// Steam subiu a tempo, segue em frente — o `-applaunch` mais adiante
/// ainda pode dar certo, e `wait_for_steam_session` é quem de fato decide
/// se foi rápido o bastante.
fn ensure_steam_running_silently(steam_bin: &str) {
    ensure_steam_running_silently_with(
        steam_bin,
        STEAM_COLD_START_TIMEOUT,
        STEAM_WARMUP_GRACE,
        STEAM_POLL_INTERVAL,
        is_steam_process_running,
    );
}

/// Núcleo testável de `ensure_steam_running_silently`: `is_running`
/// injetado em vez de tocar `/proc` diretamente — o caminho "já está
/// rodando" (o único que os testes exercitam sem precisar de um binário
/// fake de longa duração) nunca chega a spawnar `steam_bin`.
fn ensure_steam_running_silently_with(
    steam_bin: &str,
    cold_start_timeout: Duration,
    warmup_grace: Duration,
    poll_interval: Duration,
    mut is_running: impl FnMut() -> bool,
) {
    if is_running() {
        return;
    }
    if std::process::Command::new(steam_bin)
        .arg("-silent")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .is_err()
    {
        return;
    }
    let deadline = Instant::now() + cold_start_timeout;
    while Instant::now() < deadline {
        if is_running() {
            std::thread::sleep(warmup_grace);
            return;
        }
        std::thread::sleep(poll_interval);
    }
}

/// Espera a sessão real da Steam pra `app_id` aparecer (jogo realmente
/// começou a subir sob o Steam Linux Runtime — cobre qualquer checagem/
/// atualização que a Steam decida fazer antes, inclusive baixar um update
/// pendente do jogo em si, ver `STEAM_APPLAUNCH_STARTUP_TIMEOUT`) e depois
/// desaparecer de vez (jogo fechou de verdade — com `REAPER_GRACE_PERIOD` de
/// tolerância pra trocas de `reaper` sequenciais, ver a constante acima).
/// Sem timeout na segunda metade: nenhum tempo de jogo é "longo demais".
/// `startup_timeout` existe só pro primeiro lado — se nenhum `reaper`
/// aparecer dentro dele, algo deu errado no lançamento (ex.: diálogo de
/// erro da Steam) e não tem o que esperar.
fn wait_for_steam_session(app_id: &str, startup_timeout: Duration) -> Result<(), RunError> {
    wait_for_steam_session_with(
        startup_timeout,
        REAPER_GRACE_PERIOD,
        REAPER_POLL_INTERVAL,
        || find_reaper_pid(app_id).is_some(),
    )
    .ok_or_else(|| RunError::SteamLaunchTimeout(app_id.to_string()))
}

/// Núcleo testável de `wait_for_steam_session`: `reaper_present` injetado
/// em vez de tocar `/proc` diretamente. `None` só quando nenhuma sessão
/// jamais apareceu dentro de `startup_timeout` — uma vez que apareceu ao
/// menos uma vez, sempre acaba retornando `Some(())` (só depois que
/// `reaper_present()` ficar falso por `grace_period` contínuo).
fn wait_for_steam_session_with(
    startup_timeout: Duration,
    grace_period: Duration,
    poll_interval: Duration,
    mut reaper_present: impl FnMut() -> bool,
) -> Option<()> {
    let deadline = Instant::now() + startup_timeout;
    loop {
        if reaper_present() {
            break;
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(poll_interval);
    }

    let mut last_seen = Instant::now();
    loop {
        std::thread::sleep(poll_interval);
        if reaper_present() {
            last_seen = Instant::now();
        } else if last_seen.elapsed() >= grace_period {
            return Some(());
        }
    }
}

/// Ciclo de vida completo pra jogos Steam oficiais lançados via `steam
/// -applaunch` (o processo que disparamos retorna quase na hora — não dá
/// pra `child.wait()` nele como `run_session` faz pro caso onde o processo
/// do jogo é filho direto nosso, ver `MenuLaunch` em `main.rs`). Baixa se
/// faltar (idêntico a `run_session`, inclusive "traz de volta" uma
/// instalação fantasma — manifesto presente, pasta ausente), dispara o
/// `-applaunch`, espera a sessão real da Steam via `reaper` (não o
/// processo que disparamos) começar e depois terminar, só então
/// sincroniza de volta e limpa — mesma garantia de segurança que
/// `run_session` já dá, só o jeito de saber que o jogo fechou que muda.
pub fn run_via_steam_applaunch(
    cfg: &Config,
    app_id: &str,
    steam_root: Option<&Path>,
    on_progress: impl FnMut(Stage, ProgressUpdate),
) -> Result<GameSession, RunError> {
    run_via_steam_applaunch_with_options(
        cfg,
        app_id,
        steam_root,
        "rsync",
        "ssh",
        &LockGuard::default_path(),
        STEAM_APPLAUNCH_STARTUP_TIMEOUT,
        on_progress,
    )
}

#[allow(clippy::too_many_arguments)]
fn run_via_steam_applaunch_with_options(
    cfg: &Config,
    app_id: &str,
    steam_root: Option<&Path>,
    rsync_bin: &str,
    ssh_bin: &str,
    lock_path: &Path,
    startup_timeout: Duration,
    mut on_progress: impl FnMut(Stage, ProgressUpdate),
) -> Result<GameSession, RunError> {
    let ResolvedSession {
        source,
        vault,
        remote_paths,
        local_targets,
        full_install,
        _lock,
    } = resolve_and_download(
        cfg,
        Launcher::Steam,
        app_id,
        steam_root,
        rsync_bin,
        ssh_bin,
        lock_path,
        &mut on_progress,
    )?;

    let session = GameSession::new(source);

    // Incidente real (2026-09-12, Cyberpunk 2077): instalação completa
    // rastreada só via `install_path_overrides.steam` (sem manifesto local
    // nem retrato salvo no FeuVault — a Steam nunca chegou a saber desse
    // jogo) baixa os arquivos certinho acima, mas `-applaunch` sozinho não
    // basta: sem `appmanifest_<app_id>.acf`, a Steam trata o jogo como "não
    // instalado" e abre o diálogo da loja pedindo instalação do zero (61GB
    // de download desnecessário — os arquivos já estão certos no disco).
    // `vault-sync update` (steamcmd `+app_update ... validate`, credenciais
    // em cache do próprio cliente Steam, sem senha) já resolve exatamente
    // isso — só falta rodar antes do `-applaunch` quando o manifesto
    // realmente está faltando. Best-effort: falha aqui (sem `steam_account`
    // configurado, steamcmd indisponível, sem rede) nunca cancela a sessão —
    // só significa que o `-applaunch` mais adiante corre o mesmo risco de
    // sempre, comportamento idêntico ao pré-existente antes desta checagem.
    if let Some(full) = full_install_missing_steam_manifest(cfg, app_id, &full_install) {
        tracing::info!(
            app_id,
            local_dir = %full.local_dir.display(),
            "manifesto da Steam ausente pra uma instalação completa rastreada — validando via steamcmd antes do -applaunch"
        );
        if let Err(err) = crate::update::run_update(cfg, app_id) {
            tracing::warn!(app_id, %err, "steamcmd validate falhou — tentando -applaunch mesmo assim");
        }
    }

    ensure_steam_running_silently("steam");
    Command::new("steam")
        .arg("-applaunch")
        .arg(app_id)
        .spawn()
        .map_err(RunError::GameSpawn)?;

    on_progress(
        Stage::Running,
        ProgressUpdate {
            percent: 0,
            rate: String::new(),
        },
    );

    wait_for_steam_session(app_id, startup_timeout)?;

    // Sempre Launcher::Steam (hardcoded acima) — `upload_and_cleanup` só usa
    // `legendary_installed_json_path` pra fontes `GameSource::HeroicGame`,
    // que nunca ocorrem aqui.
    upload_and_cleanup(
        cfg,
        &session.source,
        &vault,
        &local_targets,
        &remote_paths,
        &full_install,
        rsync_bin,
        None,
        PREFIX_QUIET_TIMEOUT,
        &mut on_progress,
    )?;

    Ok(session)
}

/// Baixa a instalação completa (+ compatdata) de `app_id` do servidor pro
/// SSD local sem lançar nada — par do `run`/`menu-launch` pensado pra
/// resolver o caso em que a Steam decide sozinha (ex.: botão "Atualizar" na
/// biblioteca) que precisa reinstalar via CDN da Valve mesmo com o jogo já
/// pronto no servidor (`vault-sync pull`, ver cli.rs). Reaproveita a mesma
/// checagem de manifesto ausente que `run_via_steam_applaunch` já fazia
/// antes do `-applaunch`: sem ela, a Steam trataria a instalação recém-
/// chegada como "nunca instalada" e ofereceria reinstalar do zero de novo,
/// anulando o propósito inteiro deste comando. Devolve o diretório local da
/// instalação completa quando havia uma rastreada (`None` quando só
/// compatdata se aplica a este `app_id`/launcher).
pub fn pull_full_install(
    cfg: &Config,
    launcher: Launcher,
    app_id: &str,
    steam_root: Option<&Path>,
    mut on_progress: impl FnMut(Stage, ProgressUpdate),
) -> Result<Option<PathBuf>, RunError> {
    let ResolvedSession { full_install, .. } = resolve_and_download(
        cfg,
        launcher,
        app_id,
        steam_root,
        "rsync",
        "ssh",
        &LockGuard::default_path(),
        &mut on_progress,
    )?;

    if launcher == Launcher::Steam {
        if let Some(full) = full_install_missing_steam_manifest(cfg, app_id, &full_install) {
            tracing::info!(
                app_id,
                local_dir = %full.local_dir.display(),
                "manifesto da Steam ausente pra uma instalação completa rastreada — validando via steamcmd"
            );
            if let Err(err) = crate::update::run_update(cfg, app_id) {
                tracing::warn!(
                    app_id,
                    %err,
                    "steamcmd validate falhou — a Steam pode não reconhecer a instalação até rodar 'vault-sync update' manualmente"
                );
            }
        }
    }

    Ok(full_install.map(|f| f.local_dir))
}

fn cleanup_local(targets: &LocalDeletionTargets, keep_manifest: bool) -> Result<(), RunError> {
    if targets.compatdata_dir.exists() {
        std::fs::remove_dir_all(&targets.compatdata_dir).map_err(RunError::Cleanup)?;
    }
    if !keep_manifest {
        if let Some(manifest) = &targets.manifest_file {
            if manifest.exists() {
                std::fs::remove_file(manifest).map_err(RunError::Cleanup)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn test_dir(label: &str) -> std::path::PathBuf {
        crate::testutil::test_dir("session", label)
    }

    fn write_script(dir: &Path, name: &str, body: &str) -> std::path::PathBuf {
        crate::testutil::write_script(dir, name, body)
    }

    /// rsync sempre bem-sucedido, sem copiar nada de fato — usado nos testes
    /// que só verificam a *orquestração* (ordem das chamadas), já que a
    /// montagem de argumentos e o parsing de progresso têm cobertura própria
    /// em `rsync.rs`/`progress.rs`.
    fn always_succeeds_rsync(dir: &Path) -> std::path::PathBuf {
        write_script(dir, "fake-rsync-ok.sh", "exit 0")
    }

    /// rsync que falha sempre — usado pra simular indisponibilidade de rede
    /// já na etapa de download.
    fn always_fails_rsync(dir: &Path) -> std::path::PathBuf {
        write_script(dir, "fake-rsync-fail.sh", "exit 23")
    }

    /// Falha só na PRIMEIRA chamada, com o texto exato de "diretório
    /// remoto não existe", depois sucede sempre — modela o download de
    /// compatdata de um jogo nunca aberto (nada remoto ainda) seguido do
    /// upload normal ao fim da sessão, que deve funcionar sem restrição.
    fn rsync_missing_remote_dir_on_first_call_then_succeeds(dir: &Path) -> std::path::PathBuf {
        let counter = dir.join("rsync-call-count");
        write_script(
            dir,
            "fake-rsync-missing-once.sh",
            &format!(
                "COUNT_FILE='{}'\n\
                 N=$(cat \"$COUNT_FILE\" 2>/dev/null || echo 0)\n\
                 N=$((N + 1))\n\
                 echo \"$N\" > \"$COUNT_FILE\"\n\
                 if [ \"$N\" = \"1\" ]; then\n\
                 echo 'rsync: [sender] change_dir \"/vault/x\" failed: No such file or directory (2)' >&2\n\
                 exit 23\n\
                 fi\n\
                 exit 0\n",
                counter.display()
            ),
        )
    }

    /// rsync que sempre sucede, mas registra `$@` (os argumentos recebidos,
    /// incluindo os caminhos local/remoto) numa linha de `log_path` antes de
    /// sair — usado quando o teste precisa verificar QUAL caminho foi
    /// pedido, não só se a chamada aconteceu (`always_succeeds_rsync` não
    /// serve pra isso: não deixa rastro nenhum dos argumentos).
    fn logging_rsync(dir: &Path, log_path: &Path) -> std::path::PathBuf {
        write_script(
            dir,
            "fake-rsync-log.sh",
            &format!("echo \"$@\" >> '{}'\nexit 0", log_path.display()),
        )
    }

    /// Fabrica um `<fake_proc>/<pid>/{comm,cmdline}` — mesmo formato real de
    /// `/proc`, sem precisar de um processo de verdade chamado "reaper"
    /// (o kernel deriva `comm` do binário efetivamente execado, não de um
    /// `argv[0]` forjado — não dá pra simular isso barato sem compilar um
    /// binário próprio só pro teste).
    fn fake_proc_entry(proc_dir: &Path, pid: u32, comm: &str, cmdline_args: &[&str]) {
        let entry_dir = proc_dir.join(pid.to_string());
        fs::create_dir_all(&entry_dir).unwrap();
        fs::write(entry_dir.join("comm"), format!("{comm}\n")).unwrap();
        // /proc/<pid>/cmdline: argumentos separados por byte nulo, sem
        // delimitador ao final de cada um (nem newline).
        let cmdline = cmdline_args.join("\0");
        fs::write(entry_dir.join("cmdline"), cmdline.as_bytes()).unwrap();
    }

    #[test]
    fn cmdline_has_marker_matches_exact_argument_only() {
        assert!(cmdline_has_marker(
            "reaper\0SteamLaunch\0AppId=1091500\0--\0foo",
            "AppId=1091500"
        ));
        // Não é substring solta — "AppId=10915000" não deveria bater com
        // "AppId=1091500" só por conter o prefixo.
        assert!(!cmdline_has_marker(
            "reaper\0SteamLaunch\0AppId=10915000\0--\0foo",
            "AppId=1091500"
        ));
    }

    #[test]
    fn find_reaper_pid_matches_the_right_appid_among_several_processes() {
        let dir = test_dir("proc-find-reaper");
        // Um reaper de outro AppID (não deveria bater).
        fake_proc_entry(
            &dir,
            111,
            "reaper",
            &["reaper", "SteamLaunch", "AppId=730", "--", "x"],
        );
        // Um processo qualquer com "reaper" na cmdline, mas comm diferente
        // (não deveria bater — comm é o critério primário).
        fake_proc_entry(
            &dir,
            222,
            "bash",
            &["bash", "-c", "echo reaper AppId=1091500"],
        );
        // Uma entrada não-numérica (deve ser ignorada silenciosamente, ex.:
        // "self", "version" em /proc de verdade).
        fs::create_dir_all(dir.join("self")).unwrap();
        // O reaper certo.
        fake_proc_entry(
            &dir,
            333,
            "reaper",
            &["reaper", "SteamLaunch", "AppId=1091500", "--", "x"],
        );

        let found = find_reaper_pid_in(dir.to_str().unwrap(), "1091500");
        assert_eq!(found, Some(333));
    }

    #[test]
    fn find_reaper_pid_returns_none_when_no_session_is_running() {
        let dir = test_dir("proc-find-reaper-empty");
        assert_eq!(find_reaper_pid_in(dir.to_str().unwrap(), "1091500"), None);
    }

    #[test]
    fn is_steam_process_running_in_finds_a_process_named_steam() {
        let dir = test_dir("proc-steam-running");
        fake_proc_entry(&dir, 555, "steam", &["/usr/lib/steam/steam"]);
        assert!(is_steam_process_running_in(dir.to_str().unwrap()));
    }

    #[test]
    fn is_steam_process_running_in_is_false_without_a_steam_process() {
        let dir = test_dir("proc-steam-absent");
        fake_proc_entry(&dir, 555, "bash", &["bash"]);
        assert!(!is_steam_process_running_in(dir.to_str().unwrap()));
    }

    #[test]
    fn ensure_steam_running_silently_with_never_spawns_when_already_running() {
        // `steam_bin` aponta pra um caminho inexistente de propósito — se a
        // função tentasse spawnar mesmo com a Steam já rodando, isso seria
        // trabalho desperdiçado (o ponto do cache: só paga o custo de boot
        // quando é mesmo necessário). Conta quantas vezes `is_running` foi
        // chamado pra confirmar que o caminho "já rodando" é só uma checagem.
        let mut calls = 0;
        ensure_steam_running_silently_with(
            "/caminho/que/nao/existe/de-jeito-nenhum",
            Duration::from_millis(50),
            Duration::from_millis(1),
            Duration::from_millis(1),
            || {
                calls += 1;
                true
            },
        );
        assert_eq!(calls, 1, "só deveria checar uma vez quando já está rodando");
    }

    #[test]
    fn ensure_steam_running_silently_with_gives_up_after_cold_start_timeout() {
        // `is_running` nunca fica `true` — simula uma Steam que nunca sobe.
        // `steam_bin` = "true" (sempre sai com sucesso sem fazer nada) só
        // pra confirmar que a função não trava além do timeout configurado.
        let start = Instant::now();
        ensure_steam_running_silently_with(
            "true",
            Duration::from_millis(100),
            Duration::from_millis(1),
            Duration::from_millis(10),
            || false,
        );
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "não deveria esperar muito além do cold_start_timeout configurado"
        );
    }

    #[test]
    fn wait_for_steam_session_times_out_when_reaper_never_appears() {
        // AppID que nenhuma sessão real vai ter — contra o /proc de
        // verdade, então isso testa a função pública ponta a ponta pro
        // caminho de timeout. O caminho de sucesso/carência é testado via
        // `wait_for_steam_session_with` (injetável) logo abaixo.
        let result =
            wait_for_steam_session("999999999-teste-inexistente", Duration::from_millis(50));
        assert!(matches!(result, Err(RunError::SteamLaunchTimeout(_))));
    }

    #[test]
    fn wait_for_steam_session_with_survives_a_gap_between_sequential_reapers() {
        // Regressão do incidente real (2026-08-16): a Steam lança um
        // primeiro `reaper` de verificação/instalação que some rápido,
        // *depois* o `reaper` de verdade da sessão de jogo aparece — sem
        // margem de carência, a primeira sumida já era tratada como "jogo
        // fechou", disparando upload+limpeza com o jogo de verdade ainda
        // subindo. Sequência simulada: ausente -> presente (reaper #1) ->
        // ausente (troca) -> presente (reaper #2) -> ausente (fim de
        // verdade, mantido até estourar a carência).
        let sequence = [
            false, true, true, // reaper #1 vivo
            false, false, // troca — gap curto, dentro da carência
            true, true, true, // reaper #2 vivo (sessão real)
            false, false, false, false, false, false, // acabou de vez
        ];
        let i = std::cell::Cell::new(0);
        let result = wait_for_steam_session_with(
            Duration::from_millis(200),
            Duration::from_millis(15), // carência bem menor que os testes reais, só pra ser rápido
            Duration::from_millis(5),
            || {
                let idx = i.get();
                i.set((idx + 1).min(sequence.len() - 1));
                sequence[idx]
            },
        );
        assert_eq!(result, Some(()));
    }

    #[test]
    fn wait_for_steam_session_with_returns_none_when_startup_never_happens() {
        let result = wait_for_steam_session_with(
            Duration::from_millis(20),
            Duration::from_millis(10),
            Duration::from_millis(5),
            || false,
        );
        assert_eq!(result, None);
    }

    /// Fabrica `<fake_proc>/<pid>/{comm,environ}` — `environ` no formato
    /// real de `/proc` (variáveis separadas por byte nulo).
    fn fake_proc_env(proc_dir: &Path, pid: u32, comm: &str, env: &[&str]) {
        let entry_dir = proc_dir.join(pid.to_string());
        fs::create_dir_all(&entry_dir).unwrap();
        fs::write(entry_dir.join("comm"), format!("{comm}\n")).unwrap();
        fs::write(entry_dir.join("environ"), env.join("\0")).unwrap();
    }

    #[test]
    fn prefix_users_in_finds_every_process_whose_wineprefix_is_this_prefix() {
        let work = test_dir("prefix-users");
        let compatdata = work.join("compatdata").join("1222670");
        let pfx = compatdata.join("pfx");
        let other_pfx = work.join("compatdata").join("730").join("pfx");
        fs::create_dir_all(&pfx).unwrap();
        fs::create_dir_all(&other_pfx).unwrap();
        // Mesma biblioteca vista por outro caminho, como `~/.steam/steam`.
        let alias = work.join("alias");
        std::os::unix::fs::symlink(&compatdata, &alias).unwrap();

        let proc_dir = work.join("proc");
        // Barra final: é assim que o Proton monta o `WINEPREFIX`.
        let with_slash = format!("WINEPREFIX={}/", pfx.display());
        let via_alias = format!("WINEPREFIX={}", alias.join("pfx").display());
        let other = format!("WINEPREFIX={}", other_pfx.display());
        let steam_only = format!("STEAM_COMPAT_DATA_PATH={}", compatdata.display());
        fake_proc_env(&proc_dir, 100, "wineserver", &[&with_slash]);
        fake_proc_env(
            &proc_dir,
            101,
            "EADesktop.exe",
            &["HOME=/home/u", &via_alias],
        );
        fake_proc_env(&proc_dir, 102, "outro-jogo.exe", &[&other]);
        fake_proc_env(&proc_dir, 103, "bash", &["HOME=/home/u"]);
        // Script `proton`/`reaper`: só o ambiente da Steam, sem
        // `WINEPREFIX` — esses o `reaper`/`child.wait()` já acompanham.
        fake_proc_env(&proc_dir, 104, "python3", &[&steam_only]);
        fs::create_dir_all(proc_dir.join("self")).unwrap();

        let mut users = prefix_users_in(&proc_dir, &compatdata);
        users.sort();
        assert_eq!(
            users,
            vec![
                (100, "wineserver".to_string()),
                (101, "EADesktop.exe".to_string())
            ]
        );
    }

    #[test]
    fn prefix_users_in_matches_a_plain_wine_prefix_and_ignores_a_missing_one() {
        // Heroic sem Proton: o `WINEPREFIX` é a própria pasta do título.
        let work = test_dir("prefix-users-plain-wine");
        let prefix = work.join("Prefixes").join("default").join("Jogo");
        fs::create_dir_all(&prefix).unwrap();
        let proc_dir = work.join("proc");
        let env = format!("WINEPREFIX={}", prefix.display());
        fake_proc_env(&proc_dir, 200, "Jogo.exe", &[&env]);

        assert_eq!(
            prefix_users_in(&proc_dir, &prefix),
            vec![(200, "Jogo.exe".to_string())]
        );
        assert!(prefix_users_in(&proc_dir, &work.join("nao-existe")).is_empty());
    }

    #[test]
    fn prefix_users_in_resolves_wineprefix_from_inside_the_process_root() {
        // Processo num contêiner (Steam Linux Runtime) que monta o prefixo
        // num caminho que nem existe no host: só `/proc/<pid>/root` resolve.
        let work = test_dir("prefix-users-container");
        let compatdata = work.join("compatdata").join("1222670");
        fs::create_dir_all(compatdata.join("pfx")).unwrap();
        let container_root = work.join("container-root");
        fs::create_dir_all(container_root.join("run")).unwrap();
        std::os::unix::fs::symlink(
            compatdata.join("pfx"),
            container_root.join("run").join("vault-sync-teste-pfx"),
        )
        .unwrap();

        let proc_dir = work.join("proc");
        fake_proc_env(
            &proc_dir,
            300,
            "wineserver",
            &["WINEPREFIX=/run/vault-sync-teste-pfx"],
        );
        std::os::unix::fs::symlink(&container_root, proc_dir.join("300").join("root")).unwrap();

        assert_eq!(
            prefix_users_in(&proc_dir, &compatdata),
            vec![(300, "wineserver".to_string())]
        );
    }

    #[test]
    fn wait_for_prefix_quiet_with_returns_at_once_when_nothing_uses_the_prefix() {
        let mut calls = 0;
        let result = wait_for_prefix_quiet_with(
            Duration::from_secs(60),
            Duration::from_millis(1),
            Duration::from_secs(30),
            || {
                calls += 1;
                Vec::new()
            },
        );
        assert_eq!(result, Ok(()));
        assert_eq!(calls, 1);
    }

    #[test]
    fn wait_for_prefix_quiet_with_waits_for_lingering_processes_to_exit() {
        // Jogo já fechou; serviço do EA app e wineserver saem devagar.
        let mut checks = vec![
            vec![
                (1, "EADesktop.exe".to_string()),
                (2, "wineserver".to_string()),
            ],
            vec![(2, "wineserver".to_string())],
            vec![],
        ]
        .into_iter();
        let result = wait_for_prefix_quiet_with(
            Duration::from_secs(60),
            Duration::from_millis(1),
            Duration::from_secs(30),
            || checks.next().unwrap_or_default(),
        );
        assert_eq!(result, Ok(()));
        assert!(checks.next().is_none(), "deveria ter feito as 3 checagens");
    }

    #[test]
    fn wait_for_prefix_quiet_with_gives_up_after_the_timeout_and_says_who_is_left() {
        let result = wait_for_prefix_quiet_with(
            Duration::from_millis(30),
            Duration::from_millis(5),
            Duration::from_secs(30),
            || vec![(42, "EABackgroundSe".to_string())],
        );
        assert_eq!(result, Err(vec![(42, "EABackgroundSe".to_string())]));
    }

    #[test]
    fn active_prefix_at_session_end_is_never_uploaded_nor_deleted() {
        // Serviço do EA app ainda vivo depois do jogo fechar — modelado por
        // um processo de verdade com `WINEPREFIX` apontando pro prefixo.
        let work = test_dir("prefix-still-active");
        let local_lib = work.join("ssd");
        let pfx = local_lib
            .join("steamapps")
            .join("compatdata")
            .join("1222670")
            .join("pfx");
        fs::create_dir_all(&pfx).unwrap();
        fs::write(pfx.join("user.reg"), b"registro").unwrap();
        let rsync_log = work.join("rsync.log");
        let rsync = logging_rsync(&work, &rsync_log);
        let mut lingering = Command::new("sleep")
            .arg("30")
            .env("WINEPREFIX", &pfx)
            .spawn()
            .unwrap();

        let cfg = base_config(&local_lib);
        let source = GameSource::Official { app_id: 1222670 };
        let vault = RemoteVault {
            ssh_target: cfg.remote_ssh_target.clone(),
            base_path: cfg.remote_base_path.clone(),
        };
        let result = upload_and_cleanup(
            &cfg,
            &source,
            &vault,
            &LibraryLocation::new(local_lib.clone())
                .deletion_targets(&source)
                .unwrap(),
            &vault.paths_for(&source).unwrap(),
            &None,
            rsync.to_str().unwrap(),
            None,
            Duration::ZERO,
            |_, _| {},
        );
        lingering.kill().ok();
        lingering.wait().ok();

        let err = result.unwrap_err();
        assert!(
            matches!(err, RunError::PrefixStillActive { .. }),
            "esperava PrefixStillActive, obteve {err:?}"
        );
        assert!(err.to_string().contains("ingest --only \"1222670\""));
        assert!(
            !rsync_log.exists(),
            "nada pode ser enviado com o prefixo ativo"
        );
        assert!(
            pfx.join("user.reg").exists(),
            "a cópia local nunca pode ser apagada com o prefixo ativo"
        );
    }

    /// rsync que só falha quando invocado como upload (destino remoto, com
    /// ':'). Olha o último argumento (`for dest; do :; done` deixa o último
    /// em `$dest`): o download tem mais opções que o upload (`--backup
    /// --backup-dir=...`), então nenhuma posição fixa serve pros dois.
    fn fails_only_on_upload_rsync(dir: &Path) -> std::path::PathBuf {
        write_script(
            dir,
            "fake-rsync-upload-fails.sh",
            "for dest; do :; done\ncase \"$dest\" in\n  *:*) exit 1 ;;\n  *) exit 0 ;;\nesac",
        )
    }

    /// Download (destino local puro) sempre sucede; upload (destino no
    /// formato `ssh_target:caminho`) falha sempre com o texto exato de
    /// "diretório de origem local não existe" — modela um jogo que abriu
    /// mas crashou antes de escrever qualquer coisa no prefixo local, então
    /// não há nada de verdade pra sincronizar de volta ao fim da sessão.
    fn upload_fails_with_missing_local_dir_rsync(dir: &Path) -> std::path::PathBuf {
        write_script(
            dir,
            "fake-rsync-upload-missing.sh",
            "for dest; do :; done\ncase \"$dest\" in\n  *:*) echo 'rsync: [sender] change_dir \
             \"/local/x\" failed: No such file or directory (2)' >&2; exit 23 ;;\n  *) exit 0 ;;\nesac",
        )
    }

    fn base_config(local_lib: &Path) -> Config {
        Config {
            local_libraries: vec![local_lib.to_path_buf()],
            ..Config::test_default()
        }
    }

    #[test]
    fn successful_run_spawns_game_uploads_and_cleans_up_local_storage() {
        let work = test_dir("success");
        let local_lib = work.join("ssd");
        let compatdata = local_lib.join("steamapps").join("compatdata").join("730");
        fs::create_dir_all(&compatdata).unwrap();
        fs::write(compatdata.join("save.dat"), b"progresso").unwrap();

        let marker = work.join("game-ran.marker");
        let rsync = always_succeeds_rsync(&work);
        let lock_path = work.join("session.lock");

        let cfg = base_config(&local_lib);
        let exec = vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            format!("touch {}", marker.display()),
        ];

        let result = run_session_with_options(
            &cfg,
            Launcher::Steam,
            "730",
            &exec,
            None,
            rsync.to_str().unwrap(),
            "ssh",
            &lock_path,
            None,
            |_, _| {},
        );

        assert!(result.is_ok(), "esperava sucesso, obteve {result:?}");
        assert!(marker.exists(), "o comando do jogo deveria ter rodado");
        assert!(
            !compatdata.exists(),
            "o diretório de compatdata deveria ter sido removido após o upload bem-sucedido"
        );
        assert!(
            !lock_path.exists(),
            "o lockfile deveria ter sido liberado no fim da sessão"
        );
    }

    #[test]
    fn game_exiting_immediately_with_an_error_is_a_hard_failure_not_a_silent_sync() {
        // Regressão do incidente real (2026-09-09, Nobody Wants to Die): o
        // Legendary recusou o lançamento ("not currently installed" — ver
        // `ensure_legendary_knows_about_install_at`) e saiu com erro em
        // menos de um segundo, mas o código de então tratava isso como uma
        // sessão normal — sincronizava de volta e limpava o local como se o
        // jogo tivesse rodado, mascarando a falha real atrás de um log que
        // parecia êxito.
        let work = test_dir("exit-too-fast");
        let local_lib = work.join("ssd");
        let compatdata = local_lib.join("steamapps").join("compatdata").join("730");
        fs::create_dir_all(&compatdata).unwrap();
        fs::write(compatdata.join("save.dat"), b"progresso").unwrap();

        let rsync = always_succeeds_rsync(&work);
        let lock_path = work.join("session.lock");

        let cfg = base_config(&local_lib);
        let exec = vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "exit 7".to_string(),
        ];

        let result = run_session_with_options(
            &cfg,
            Launcher::Steam,
            "730",
            &exec,
            None,
            rsync.to_str().unwrap(),
            "ssh",
            &lock_path,
            None,
            |_, _| {},
        );

        assert!(
            matches!(result, Err(RunError::GameExitedImmediately { .. })),
            "esperava GameExitedImmediately, obteve {result:?}"
        );
        assert!(
            compatdata.exists(),
            "nada deveria ter sido sincronizado/limpo numa falha de lançamento"
        );
        assert!(
            !lock_path.exists(),
            "o lockfile deveria ter sido liberado mesmo numa falha (Drop do LockGuard)"
        );
    }

    #[test]
    fn download_failure_aborts_before_spawning_the_game() {
        let work = test_dir("download-fail");
        let local_lib = work.join("ssd");
        fs::create_dir_all(&local_lib).unwrap();

        let marker = work.join("game-ran.marker");
        let rsync = always_fails_rsync(&work);
        let lock_path = work.join("session.lock");

        let cfg = base_config(&local_lib);
        let exec = vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            format!("touch {}", marker.display()),
        ];

        let result = run_session_with_options(
            &cfg,
            Launcher::Steam,
            "730",
            &exec,
            None,
            rsync.to_str().unwrap(),
            "ssh",
            &lock_path,
            None,
            |_, _| {},
        );

        assert!(matches!(result, Err(RunError::Download(_))));
        assert!(!marker.exists(), "o jogo nunca deveria ter sido iniciado");
        assert!(
            !lock_path.exists(),
            "o lockfile deveria ter sido liberado mesmo em erro"
        );
    }

    #[test]
    fn missing_remote_compatdata_on_first_ever_launch_does_not_block_the_game() {
        // Regressão do incidente real (2026-09-07, "Nobody Wants to Die"):
        // jogo baixado mas nunca aberto nem uma vez — sem prefixo Wine
        // local NEM remoto, nada pra `ingest` ter subido ainda. Antes,
        // `run` bloqueava aqui com o mesmo erro de uma queda de rede de
        // verdade (código 23), indistinguível pro usuário — mesmo o jogo
        // não tendo nenhum dado de sessão anterior pra realmente perder.
        let work = test_dir("missing-compatdata-first-launch");
        let local_lib = work.join("ssd");
        fs::create_dir_all(&local_lib).unwrap();
        // Propositalmente SEM compatdata/730 local — é exatamente esse o
        // cenário: nada rodou aqui ainda, em nenhum dos dois lados.

        let marker = work.join("game-ran.marker");
        let rsync = rsync_missing_remote_dir_on_first_call_then_succeeds(&work);
        let lock_path = work.join("session.lock");
        let cfg = base_config(&local_lib);
        let exec = vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            format!("touch {}", marker.display()),
        ];

        let result = run_session_with_options(
            &cfg,
            Launcher::Steam,
            "730",
            &exec,
            None,
            rsync.to_str().unwrap(),
            "ssh",
            &lock_path,
            None,
            |_, _| {},
        );

        assert!(result.is_ok(), "esperava sucesso, obteve {result:?}");
        assert!(
            marker.exists(),
            "o jogo deveria ter rodado normalmente mesmo sem compatdata remoto prévio"
        );
    }

    #[test]
    fn upload_failure_after_game_run_skips_cleanup() {
        let work = test_dir("upload-fail");
        let local_lib = work.join("ssd");
        let compatdata = local_lib.join("steamapps").join("compatdata").join("730");
        fs::create_dir_all(&compatdata).unwrap();
        fs::write(compatdata.join("save.dat"), b"progresso").unwrap();

        let marker = work.join("game-ran.marker");
        let rsync = fails_only_on_upload_rsync(&work);
        let lock_path = work.join("session.lock");

        let cfg = base_config(&local_lib);
        let exec = vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            format!("touch {}", marker.display()),
        ];

        let result = run_session_with_options(
            &cfg,
            Launcher::Steam,
            "730",
            &exec,
            None,
            rsync.to_str().unwrap(),
            "ssh",
            &lock_path,
            None,
            |_, _| {},
        );

        assert!(matches!(result, Err(RunError::Upload(_))));
        assert!(marker.exists(), "o jogo deveria ter rodado antes do upload");
        assert!(
            compatdata.exists(),
            "a limpeza local não deve ocorrer quando o upload de volta falha"
        );
    }

    #[test]
    fn missing_local_compatdata_on_upload_after_game_crash_does_not_block_cleanup() {
        // Regressão do incidente real (2026-09-07, "Nobody Wants to Die",
        // segundo capítulo): o jogo abriu mas crashou antes de escrever
        // qualquer coisa no prefixo Wine local — sem download prévio
        // pendurado (já corrigido), mas agora o UPLOAD de volta que falhava
        // com o mesmo código 23, porque não havia nada local pra enviar.
        let work = test_dir("upload-missing-local-after-crash");
        let local_lib = work.join("ssd");
        fs::create_dir_all(&local_lib).unwrap();
        // Propositalmente SEM compatdata/730 local — é exatamente esse o
        // cenário: o "jogo" (comando de teste) roda e sai, sem nunca
        // escrever nada no prefixo.

        let marker = work.join("game-ran.marker");
        let rsync = upload_fails_with_missing_local_dir_rsync(&work);
        let lock_path = work.join("session.lock");
        let cfg = base_config(&local_lib);
        let exec = vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            format!("touch {}", marker.display()),
        ];

        let result = run_session_with_options(
            &cfg,
            Launcher::Steam,
            "730",
            &exec,
            None,
            rsync.to_str().unwrap(),
            "ssh",
            &lock_path,
            None,
            |_, _| {},
        );

        assert!(result.is_ok(), "esperava sucesso, obteve {result:?}");
        assert!(marker.exists(), "o jogo deveria ter rodado normalmente");
    }

    #[test]
    fn concurrent_session_is_rejected_by_the_lock() {
        let work = test_dir("lock-conflict");
        let local_lib = work.join("ssd");
        fs::create_dir_all(&local_lib).unwrap();
        let lock_path = work.join("session.lock");

        let _held = LockGuard::acquire(&lock_path).unwrap();

        let rsync = always_succeeds_rsync(&work);
        let cfg = base_config(&local_lib);
        let exec = vec!["/bin/true".to_string()];

        let result = run_session_with_options(
            &cfg,
            Launcher::Steam,
            "730",
            &exec,
            None,
            rsync.to_str().unwrap(),
            "ssh",
            &lock_path,
            None,
            |_, _| {},
        );

        assert!(matches!(result, Err(RunError::Lock(_))));
    }

    #[test]
    fn missing_local_library_is_rejected_before_any_io() {
        let cfg = Config::test_default();
        let result = run_session(
            &cfg,
            Launcher::Steam,
            "730",
            &["/bin/true".to_string()],
            None,
            |_, _| {},
        );
        assert!(matches!(result, Err(RunError::NoLocalLibrary)));
    }

    #[test]
    fn non_numeric_app_id_is_rejected() {
        let work = test_dir("bad-appid");
        let local_lib = work.join("ssd");
        fs::create_dir_all(&local_lib).unwrap();
        let cfg = base_config(&local_lib);

        let result = run_session(
            &cfg,
            Launcher::Steam,
            "../escape",
            &["/bin/true".to_string()],
            None,
            |_, _| {},
        );
        assert!(matches!(result, Err(RunError::InvalidAppId(_))));
    }

    #[test]
    fn game_process_inherits_parent_environment() {
        let work = test_dir("env-inherit");
        let local_lib = work.join("ssd");
        let compatdata = local_lib.join("steamapps").join("compatdata").join("730");
        fs::create_dir_all(&compatdata).unwrap();

        let env_marker = work.join("env.marker");
        let rsync = always_succeeds_rsync(&work);
        let lock_path = work.join("session.lock");
        let cfg = base_config(&local_lib);

        let expected_path = std::env::var("PATH").unwrap_or_default();
        let exec = vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            format!("printf '%s' \"$PATH\" > {}", env_marker.display()),
        ];

        let result = run_session_with_options(
            &cfg,
            Launcher::Steam,
            "730",
            &exec,
            None,
            rsync.to_str().unwrap(),
            "ssh",
            &lock_path,
            None,
            |_, _| {},
        );

        assert!(result.is_ok(), "esperava sucesso, obteve {result:?}");
        assert_eq!(fs::read_to_string(env_marker).unwrap(), expected_path);
    }

    #[test]
    fn game_process_receives_steam_compat_data_path_pointing_at_local_compatdata() {
        // Regressão do incidente real (2026-08-30): sem isso, `proton run
        // <exe>` (ou qualquer coisa que dependa de STEAM_COMPAT_DATA_PATH)
        // não tinha como saber que prefixo usar, arriscando um WINEPREFIX
        // nunca sincronizado com o FeuVault em vez do compatdata baixado.
        let work = test_dir("compat-data-path-env");
        let local_lib = work.join("ssd");
        let compatdata = local_lib.join("steamapps").join("compatdata").join("730");
        fs::create_dir_all(&compatdata).unwrap();

        let env_marker = work.join("compat-path.marker");
        let rsync = always_succeeds_rsync(&work);
        let lock_path = work.join("session.lock");
        let cfg = base_config(&local_lib);

        let exec = vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            format!(
                "printf '%s' \"$STEAM_COMPAT_DATA_PATH\" > {}",
                env_marker.display()
            ),
        ];

        let result = run_session_with_options(
            &cfg,
            Launcher::Steam,
            "730",
            &exec,
            None,
            rsync.to_str().unwrap(),
            "ssh",
            &lock_path,
            None,
            |_, _| {},
        );

        assert!(result.is_ok(), "esperava sucesso, obteve {result:?}");
        assert_eq!(
            fs::read_to_string(env_marker).unwrap(),
            compatdata.to_string_lossy()
        );
    }

    #[test]
    fn ensure_legendary_knows_about_install_patches_stale_install_path() {
        // Caso 1 do incidente real (2026-09-07, "Nobody Wants to Die"): já
        // existe uma entrada pra esse título, só apontando pro lugar
        // errado — deve corrigir só o campo, preservando o resto.
        let dir = test_dir("legendary-fix-stale-path");
        let installed_json = dir.join("installed.json");
        fs::write(
            &installed_json,
            r#"{"abc123":{"app_name":"abc123","title":"Meu Jogo","executable":"jogo.exe","install_path":"/caminho/velho"}}"#,
        )
        .unwrap();
        let cfg = base_config(&dir);
        let new_local_dir = dir.join("caminho-novo");

        ensure_legendary_knows_about_install_at(
            &cfg,
            "Meu Jogo",
            &new_local_dir,
            "ssh",
            &installed_json,
        );

        let content = fs::read_to_string(&installed_json).unwrap();
        let map: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(&content).unwrap();
        assert_eq!(
            map["abc123"]["install_path"].as_str().unwrap(),
            new_local_dir.to_string_lossy()
        );
        assert_eq!(
            map["abc123"]["executable"].as_str().unwrap(),
            "jogo.exe",
            "resto da entrada não deveria ter sido tocado"
        );
    }

    #[test]
    fn ensure_legendary_knows_about_install_is_a_no_op_when_path_already_correct() {
        let dir = test_dir("legendary-already-correct");
        let installed_json = dir.join("installed.json");
        let local_dir = dir.join("jogo-instalado");
        fs::write(
            &installed_json,
            format!(
                r#"{{"abc123":{{"app_name":"abc123","title":"Meu Jogo","install_path":"{}"}}}}"#,
                local_dir.to_string_lossy().replace('\\', "\\\\")
            ),
        )
        .unwrap();
        let cfg = base_config(&dir);
        let mtime_before = fs::metadata(&installed_json).unwrap().modified().unwrap();

        ensure_legendary_knows_about_install_at(
            &cfg,
            "Meu Jogo",
            &local_dir,
            "ssh",
            &installed_json,
        );

        let mtime_after = fs::metadata(&installed_json).unwrap().modified().unwrap();
        assert_eq!(
            mtime_before, mtime_after,
            "não deveria ter reescrito o arquivo quando o install_path já bate"
        );
    }

    #[test]
    fn clean_stale_legendary_entry_removes_the_matching_title() {
        // Regressão real (2026-09-07): `upload_and_cleanup` apagava a
        // instalação completa de um jogo Heroic ao fim de CADA sessão de
        // `run` sem nunca limpar a entrada correspondente no
        // `installed.json` — o catálogo (`SUPER+G`) continuava mostrando o
        // jogo como "Cliente"/"Cliente+Servidor" mesmo já 100% sincronizado
        // de volta, até o próximo `ingest --full` limpar por acaso.
        let dir = test_dir("clean-stale-legendary-entry");
        let installed_json = dir.join("installed.json");
        fs::write(
            &installed_json,
            r#"{
                "abc123": {"title": "Chivalry 2", "install_path": "/nao/existe/mais"},
                "def456": {"title": "Outro Jogo", "install_path": "/continua/existindo"}
            }"#,
        )
        .unwrap();

        clean_stale_legendary_entry_at("Chivalry 2", &installed_json);

        let content = fs::read_to_string(&installed_json).unwrap();
        let map: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(&content).unwrap();
        assert!(
            !map.contains_key("abc123"),
            "entrada de 'Chivalry 2' deveria ter sido removida"
        );
        assert_eq!(
            map["def456"]["title"].as_str().unwrap(),
            "Outro Jogo",
            "entrada de outro título não deveria ter sido tocada"
        );
    }

    #[test]
    fn clean_stale_legendary_entry_is_a_no_op_when_installed_json_is_missing() {
        let dir = test_dir("clean-stale-legendary-entry-missing-file");
        let installed_json = dir.join("installed.json");

        // Não deveria entrar em pânico nem criar o arquivo do nada.
        clean_stale_legendary_entry_at("Qualquer Jogo", &installed_json);

        assert!(!installed_json.exists());
    }

    #[test]
    fn ensure_legendary_knows_about_install_restores_missing_entry_from_feuvault_snapshot() {
        // Caso 2 do incidente real: nenhuma entrada local pra esse título
        // — restaura do retrato salvo no FeuVault (`ingest`), com
        // install_path já corrigido pro caminho local de verdade.
        let dir = test_dir("legendary-restore-from-snapshot");
        let installed_json = dir.join("installed.json");
        // Sem arquivo local nenhum ainda — simula Heroic nunca usado nesta
        // máquina, ou installed.json apagado.
        let remote_installs = dir.join("remote-installs");
        let snapshot_dir = remote_installs.join("epic-manifests");
        fs::create_dir_all(&snapshot_dir).unwrap();
        fs::write(
            snapshot_dir.join("installed.json"),
            r#"{"xyz789":{"app_name":"xyz789","title":"Outro Jogo","executable":"outro.exe","install_path":"/onde/foi/ingerido"}}"#,
        )
        .unwrap();
        let ssh = ssh_returns_file_content(&dir, &snapshot_dir.join("installed.json"));
        let cfg = Config {
            remote_installs_base_path: Some(remote_installs),
            ..base_config(&dir)
        };
        let new_local_dir = dir.join("jogo-restaurado");

        ensure_legendary_knows_about_install_at(
            &cfg,
            "Outro Jogo",
            &new_local_dir,
            ssh.to_str().unwrap(),
            &installed_json,
        );

        let content = fs::read_to_string(&installed_json).unwrap();
        let map: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(&content).unwrap();
        assert_eq!(
            map["xyz789"]["install_path"].as_str().unwrap(),
            new_local_dir.to_string_lossy()
        );
        assert_eq!(map["xyz789"]["executable"].as_str().unwrap(), "outro.exe");
    }

    #[test]
    fn ensure_legendary_knows_about_install_is_a_no_op_when_nothing_to_restore_from() {
        // Nem entrada local, nem retrato remoto, nem `remote_installs_base_path`
        // configurado — melhor esforço: não deve criar lixo nem quebrar.
        let dir = test_dir("legendary-nothing-to-restore");
        let installed_json = dir.join("installed.json");
        let cfg = base_config(&dir);

        ensure_legendary_knows_about_install_at(
            &cfg,
            "Jogo Inexistente",
            &dir.join("algum-caminho"),
            "ssh",
            &installed_json,
        );

        assert!(
            !installed_json.exists(),
            "não deveria ter criado o arquivo sem nada real pra restaurar"
        );
    }

    #[test]
    fn game_process_receives_steam_compat_client_install_path_when_steam_root_is_known() {
        let work = test_dir("compat-client-path-env");
        let local_lib = work.join("ssd");
        let compatdata = local_lib.join("steamapps").join("compatdata").join("730");
        fs::create_dir_all(&compatdata).unwrap();
        let steam_root = work.join("steam-root");
        fs::create_dir_all(&steam_root).unwrap();

        let env_marker = work.join("compat-client-path.marker");
        let rsync = always_succeeds_rsync(&work);
        let lock_path = work.join("session.lock");
        let cfg = base_config(&local_lib);

        let exec = vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            format!(
                "printf '%s' \"$STEAM_COMPAT_CLIENT_INSTALL_PATH\" > {}",
                env_marker.display()
            ),
        ];

        let result = run_session_with_options(
            &cfg,
            Launcher::Steam,
            "730",
            &exec,
            Some(&steam_root),
            rsync.to_str().unwrap(),
            "ssh",
            &lock_path,
            None,
            |_, _| {},
        );

        assert!(result.is_ok(), "esperava sucesso, obteve {result:?}");
        assert_eq!(
            fs::read_to_string(env_marker).unwrap(),
            steam_root.to_string_lossy()
        );
    }

    fn cfg_with_full_install_steam(
        local_lib: &Path,
        steam_root_for_full: &Path,
        remote_installs: &Path,
    ) -> Config {
        Config {
            local_game_root_steam: Some(steam_root_for_full.to_path_buf()),
            remote_installs_base_path: Some(remote_installs.to_path_buf()),
            ..base_config(local_lib)
        }
    }

    #[test]
    fn missing_manifest_with_full_install_tracked_triggers_steamcmd_validate() {
        let work = test_dir("missing-manifest-triggers-validate");
        let local_lib = work.join("ssd");
        let steam_full_root = work.join("steam-full");
        let remote_installs = work.join("remote-installs");
        fs::create_dir_all(steam_full_root.join("steamapps")).unwrap();
        let cfg = cfg_with_full_install_steam(&local_lib, &steam_full_root, &remote_installs);
        let full = Some(FullInstallTarget {
            local_dir: steam_full_root
                .join("steamapps")
                .join("common")
                .join("Cyberpunk 2077"),
            remote_dir: remote_installs.join("steam").join("Cyberpunk 2077"),
        });

        let result = full_install_missing_steam_manifest(&cfg, "1091500", &full);

        assert_eq!(result, full.as_ref());
    }

    #[test]
    fn existing_manifest_never_triggers_steamcmd_validate() {
        let work = test_dir("existing-manifest-skips-validate");
        let local_lib = work.join("ssd");
        let steam_full_root = work.join("steam-full");
        let remote_installs = work.join("remote-installs");
        fs::create_dir_all(steam_full_root.join("steamapps")).unwrap();
        fs::write(
            steam_full_root
                .join("steamapps")
                .join("appmanifest_1091500.acf"),
            "\"installdir\"\t\t\"Cyberpunk 2077\"\n",
        )
        .unwrap();
        let cfg = cfg_with_full_install_steam(&local_lib, &steam_full_root, &remote_installs);
        let full = Some(FullInstallTarget {
            local_dir: steam_full_root
                .join("steamapps")
                .join("common")
                .join("Cyberpunk 2077"),
            remote_dir: remote_installs.join("steam").join("Cyberpunk 2077"),
        });

        assert_eq!(
            full_install_missing_steam_manifest(&cfg, "1091500", &full),
            None
        );
    }

    #[test]
    fn no_full_install_tracked_never_triggers_steamcmd_validate() {
        let work = test_dir("no-full-install-skips-validate");
        let local_lib = work.join("ssd");
        let steam_full_root = work.join("steam-full");
        let remote_installs = work.join("remote-installs");
        fs::create_dir_all(steam_full_root.join("steamapps")).unwrap();
        let cfg = cfg_with_full_install_steam(&local_lib, &steam_full_root, &remote_installs);

        assert_eq!(
            full_install_missing_steam_manifest(&cfg, "1091500", &None),
            None
        );
    }

    #[test]
    fn without_local_game_root_steam_never_triggers_steamcmd_validate() {
        let work = test_dir("no-local-game-root-skips-validate");
        let local_lib = work.join("ssd");
        let remote_installs = work.join("remote-installs");
        let cfg = base_config(&local_lib);
        let full = Some(FullInstallTarget {
            local_dir: work.join("Cyberpunk 2077"),
            remote_dir: remote_installs.join("steam").join("Cyberpunk 2077"),
        });

        assert_eq!(
            full_install_missing_steam_manifest(&cfg, "1091500", &full),
            None
        );
    }

    #[test]
    fn without_full_install_config_manifest_is_deleted_like_before_fase_vii() {
        let work = test_dir("no-full-manifest-deleted");
        let local_lib = work.join("ssd");
        fs::create_dir_all(local_lib.join("steamapps").join("compatdata").join("730")).unwrap();
        let manifest = local_lib.join("steamapps").join("appmanifest_730.acf");
        fs::write(&manifest, "\"installdir\"\t\t\"Meu Jogo\"\n").unwrap();

        let rsync = always_succeeds_rsync(&work);
        let lock_path = work.join("session.lock");
        let cfg = base_config(&local_lib);

        let result = run_session_with_options(
            &cfg,
            Launcher::Steam,
            "730",
            &["/bin/true".to_string()],
            None,
            rsync.to_str().unwrap(),
            "ssh",
            &lock_path,
            None,
            |_, _| {},
        );

        assert!(result.is_ok(), "esperava sucesso, obteve {result:?}");
        assert!(
            !manifest.exists(),
            "sem rastreamento de instalação completa, o manifesto continua sendo apagado (comportamento pré-Fase VII, inalterado)"
        );
    }

    #[test]
    fn full_install_official_is_downloaded_when_missing_locally() {
        let work = test_dir("full-official-download");
        let steam_full_root = work.join("steam-full");
        fs::create_dir_all(
            steam_full_root
                .join("steamapps")
                .join("compatdata")
                .join("730"),
        )
        .unwrap();
        fs::write(
            steam_full_root
                .join("steamapps")
                .join("appmanifest_730.acf"),
            "\"installdir\"\t\t\"Meu Jogo\"\n",
        )
        .unwrap();
        // common/Meu Jogo NÃO existe local — simula "jogo hoje só no
        // FeuVault", exatamente o caso que a Fase VII precisa resolver.

        let rsync = always_succeeds_rsync(&work);
        let lock_path = work.join("session.lock");
        let remote_installs = work.join("remote-installs");
        let cfg = cfg_with_full_install_steam(&steam_full_root, &steam_full_root, &remote_installs);

        let result = run_session_with_options(
            &cfg,
            Launcher::Steam,
            "730",
            &["/bin/true".to_string()],
            None,
            rsync.to_str().unwrap(),
            "ssh",
            &lock_path,
            None,
            |_, _| {},
        );

        assert!(result.is_ok(), "esperava sucesso, obteve {result:?}");
    }

    /// Fake `ssh` que responde a `read_remote_file` (usado por
    /// `restore_official_manifest_snapshot`) com um conteúdo de manifesto
    /// fixo, independente do caminho pedido — suficiente pra testar a
    /// restauração sem simular o parsing do comando remoto inteiro (isso já
    /// tem cobertura própria em `remote.rs`).
    fn ssh_returns_manifest(dir: &Path, installdir: &str) -> PathBuf {
        write_script(
            dir,
            "fake-ssh-manifest.sh",
            &format!("printf '\"installdir\"\\t\\t\"{installdir}\"\\n'"),
        )
    }

    /// Fake `ssh` que responde a `read_remote_file` (usado por
    /// `ensure_legendary_knows_about_install_at`) despejando o conteúdo de
    /// um arquivo LOCAL já existente, independente do caminho remoto
    /// pedido — evita duplicar o JSON do retrato inline em cada teste.
    fn ssh_returns_file_content(dir: &Path, local_file: &Path) -> PathBuf {
        write_script(
            dir,
            "fake-ssh-cat-file.sh",
            &format!("cat '{}'", local_file.display()),
        )
    }

    #[test]
    fn full_install_official_restores_manifest_from_feuvault_when_missing_locally() {
        // Regressão do incidente real (2026-08-30/31): a própria Steam
        // "esqueceu" o manifesto do Kenshi (limpou sozinha um manifesto
        // fantasma cujo `installdir` não existia mais), fazendo o vault-sync
        // achar que o jogo nunca teve instalação completa rastreada.
        // Universal — não é específico do Kenshi: qualquer app_id sem
        // manifesto local, mas com um retrato salvo no FeuVault
        // (`steam-manifests/<app_id>.acf`, ver `ingest.rs`), deve se
        // recuperar sozinho.
        let work = test_dir("full-official-restore-manifest");
        let steam_full_root = work.join("steam-full");
        fs::create_dir_all(
            steam_full_root
                .join("steamapps")
                .join("compatdata")
                .join("730"),
        )
        .unwrap();
        // Nenhum appmanifest_730.acf local — é exatamente esse o gap.

        let rsync_log = work.join("rsync-calls.log");
        let rsync = logging_rsync(&work, &rsync_log);
        let ssh = ssh_returns_manifest(&work, "Jogo Restaurado");
        let lock_path = work.join("session.lock");
        let remote_installs = work.join("remote-installs");
        let cfg = cfg_with_full_install_steam(&steam_full_root, &steam_full_root, &remote_installs);

        let result = run_session_with_options(
            &cfg,
            Launcher::Steam,
            "730",
            &["/bin/true".to_string()],
            None,
            rsync.to_str().unwrap(),
            ssh.to_str().unwrap(),
            &lock_path,
            None,
            |_, _| {},
        );

        assert!(result.is_ok(), "esperava sucesso, obteve {result:?}");
        // `detect_full_install` restaura o manifesto do retrato remoto no
        // meio do fluxo (é o que permite a sessão nem começar a resolver
        // `installdir`) — o log de chamadas do rsync mostrando o upload de
        // volta é a evidência indireta de que isso aconteceu: sem o
        // restore, não haveria manifesto local nenhum pra `upload_and_cleanup`
        // reenviar no fim.
        let log_contents = fs::read_to_string(&rsync_log).unwrap();
        assert!(
            log_contents.contains("appmanifest_730.acf") && log_contents.contains("730.acf"),
            "esperava upload do manifesto restaurado de volta ao FeuVault no log do rsync: {log_contents}"
        );
        // Fim do ciclo (Incidente 2026-09-20/21): com o retrato já
        // reenviado, o manifesto local não fica pra trás apontando pra uma
        // instalação que `run` acabou de apagar — é exatamente isso que
        // fazia a Steam tentar reinstalar pela CDN vendo um manifesto
        // órfão.
        let restored_manifest = steam_full_root
            .join("steamapps")
            .join("appmanifest_730.acf");
        assert!(
            !restored_manifest.exists(),
            "manifesto deveria ter sido reenviado ao FeuVault e apagado localmente, não deixado pra trás"
        );
    }

    #[test]
    fn detect_full_install_falls_back_to_steam_override_when_no_manifest_survives_anywhere() {
        // Regressão real (2026-09-09): Kenshi tinha `compatdata` só local
        // (nunca sincronizado ao FeuVault, então não há retrato remoto pra
        // restaurar) e nenhum `appmanifest_233860.acf` local — sem esse
        // fallback, `detect_full_install` devolvia `(_, None)` e o jogo
        // nunca sabia de onde baixar a instalação completa, mesmo com
        // `install_path_overrides.steam` configurado apontando pra ela.
        let work = test_dir("detect-full-install-steam-override-fallback");
        let steam_full_root = work.join("steam-full");
        fs::create_dir_all(
            steam_full_root
                .join("steamapps")
                .join("compatdata")
                .join("233860"),
        )
        .unwrap();
        // Nenhum appmanifest_233860.acf local — é exatamente esse o gap.

        let ssh_fails = write_script(&work, "fake-ssh-no-snapshot.sh", "exit 1");
        let remote_installs = work.join("remote-installs");
        let override_dir = steam_full_root
            .join("steamapps")
            .join("common")
            .join("Kenshi");
        let mut cfg =
            cfg_with_full_install_steam(&steam_full_root, &steam_full_root, &remote_installs);
        cfg.install_path_overrides
            .steam
            .insert("233860".to_string(), override_dir.clone());

        let (library_override, full_install) =
            detect_full_install(&cfg, "233860", None, ssh_fails.to_str().unwrap()).unwrap();

        assert_eq!(library_override, Some(steam_full_root.clone()));
        let target = full_install.expect("esperava um FullInstallTarget vindo do override");
        assert_eq!(target.local_dir, override_dir);
        assert_eq!(
            target.remote_dir,
            remote_installs.join("steam").join("Kenshi")
        );
    }

    #[test]
    fn full_install_official_skips_download_when_already_local_and_uploads_then_drops_manifest_after_purge()
     {
        let work = test_dir("full-official-local");
        let steam_full_root = work.join("steam-full");
        fs::create_dir_all(
            steam_full_root
                .join("steamapps")
                .join("compatdata")
                .join("730"),
        )
        .unwrap();
        let manifest = steam_full_root
            .join("steamapps")
            .join("appmanifest_730.acf");
        fs::write(&manifest, "\"installdir\"\t\t\"Meu Jogo\"\n").unwrap();
        let install_dir = steam_full_root
            .join("steamapps")
            .join("common")
            .join("Meu Jogo");
        fs::create_dir_all(&install_dir).unwrap();

        let rsync_log = work.join("rsync-calls.log");
        let rsync = logging_rsync(&work, &rsync_log);
        let lock_path = work.join("session.lock");
        let remote_installs = work.join("remote-installs");
        let cfg = cfg_with_full_install_steam(&steam_full_root, &steam_full_root, &remote_installs);

        let result = run_session_with_options(
            &cfg,
            Launcher::Steam,
            "730",
            &["/bin/true".to_string()],
            None,
            rsync.to_str().unwrap(),
            "ssh",
            &lock_path,
            None,
            |_, _| {},
        );

        assert!(result.is_ok(), "esperava sucesso, obteve {result:?}");
        assert!(
            !install_dir.exists(),
            "instalação completa deve ser apagada após o upload confirmado"
        );
        // Incidente real (2026-09-20/21, Cyberpunk 2077): manter o
        // manifesto local sem os arquivos do jogo fazia a Steam achar que a
        // instalação estava corrompida e tentar reinstalar pela CDN da
        // Valve. Agora o retrato é reenviado ao FeuVault (evidência: a
        // chamada de rsync logada abaixo) e só então apagado localmente —
        // `detect_full_install`/`restore_official_manifest_snapshot` já
        // sabem trazê-lo de volta na próxima sessão.
        let log_contents = fs::read_to_string(&rsync_log).unwrap();
        assert!(
            log_contents.contains("appmanifest_730.acf") && log_contents.contains("730.acf"),
            "esperava upload do manifesto pro FeuVault antes de apagar: {log_contents}"
        );
        assert!(
            !manifest.exists(),
            "com o retrato já reenviado ao FeuVault, o manifesto local não deve mais ficar \
             pra trás apontando pra uma instalação que acabou de ser apagada"
        );
    }

    #[test]
    fn full_install_official_without_remote_installs_base_path_is_a_hard_error() {
        let work = test_dir("full-official-not-configured");
        let steam_full_root = work.join("steam-full");
        fs::create_dir_all(
            steam_full_root
                .join("steamapps")
                .join("compatdata")
                .join("730"),
        )
        .unwrap();
        fs::write(
            steam_full_root
                .join("steamapps")
                .join("appmanifest_730.acf"),
            "\"installdir\"\t\t\"Meu Jogo\"\n",
        )
        .unwrap();

        let rsync = always_succeeds_rsync(&work);
        let lock_path = work.join("session.lock");
        let cfg = Config {
            local_game_root_steam: Some(steam_full_root.clone()),
            // remote_installs_base_path deliberadamente ausente.
            ..base_config(&steam_full_root)
        };

        let result = run_session_with_options(
            &cfg,
            Launcher::Steam,
            "730",
            &["/bin/true".to_string()],
            None,
            rsync.to_str().unwrap(),
            "ssh",
            &lock_path,
            None,
            |_, _| {},
        );

        assert!(matches!(
            result,
            Err(RunError::FullInstallNotConfigured(id)) if id == "730"
        ));
    }

    #[test]
    fn full_install_pirated_resolves_via_shortcut_start_dir_without_manifest() {
        let work = test_dir("full-piratas");
        let local_lib = work.join("ssd");
        // Non-Steam shortcut: só compatdata, nunca appmanifest.
        fs::create_dir_all(
            local_lib
                .join("steamapps")
                .join("compatdata")
                .join("1234567890"),
        )
        .unwrap();

        let steam_root = work.join("steam");
        let userdata = steam_root.join("userdata").join("1").join("config");
        fs::create_dir_all(&userdata).unwrap();
        let install_dir = work.join("Piratas").join("Jogo Pirata");
        fs::create_dir_all(&install_dir).unwrap();
        let shortcuts_tree = vec![(
            "shortcuts".to_string(),
            crate::vdf::VdfValue::Object(vec![(
                "0".to_string(),
                crate::vdf::VdfValue::Object(vec![
                    (
                        "appid".to_string(),
                        crate::vdf::VdfValue::Int(1234567890u32 as i32),
                    ),
                    (
                        "appname".to_string(),
                        crate::vdf::VdfValue::Str("Jogo Pirata".to_string()),
                    ),
                    (
                        "StartDir".to_string(),
                        crate::vdf::VdfValue::Str(format!("\"{}\"", install_dir.display())),
                    ),
                ]),
            )]),
        )];
        fs::write(
            userdata.join("shortcuts.vdf"),
            crate::vdf::write_binary(&shortcuts_tree),
        )
        .unwrap();

        let rsync = always_succeeds_rsync(&work);
        let lock_path = work.join("session.lock");
        let remote_installs = work.join("remote-installs");
        let cfg = Config {
            remote_installs_base_path: Some(remote_installs),
            ..base_config(&local_lib)
        };

        let result = run_session_with_options(
            &cfg,
            Launcher::Steam,
            "1234567890",
            &["/bin/true".to_string()],
            Some(&steam_root),
            rsync.to_str().unwrap(),
            "ssh",
            &lock_path,
            None,
            |_, _| {},
        );

        assert!(result.is_ok(), "esperava sucesso, obteve {result:?}");
        assert!(
            !install_dir.exists(),
            "instalação pirata também deve ser apagada localmente após o upload confirmado"
        );
    }

    #[test]
    fn full_install_pirated_config_override_wins_over_stale_shortcut() {
        let work = test_dir("full-piratas-override");
        let local_lib = work.join("ssd");
        fs::create_dir_all(
            local_lib
                .join("steamapps")
                .join("compatdata")
                .join("1234567890"),
        )
        .unwrap();

        // shortcuts.vdf aponta pra um caminho morto (HD externo desmontado)
        // — o override no config.json deve ganhar dessa fonte, não dela.
        let steam_root = work.join("steam");
        let userdata = steam_root.join("userdata").join("1").join("config");
        fs::create_dir_all(&userdata).unwrap();
        let shortcuts_tree = vec![(
            "shortcuts".to_string(),
            crate::vdf::VdfValue::Object(vec![(
                "0".to_string(),
                crate::vdf::VdfValue::Object(vec![
                    (
                        "appid".to_string(),
                        crate::vdf::VdfValue::Int(1234567890u32 as i32),
                    ),
                    (
                        "appname".to_string(),
                        crate::vdf::VdfValue::Str("Jogo Pirata".to_string()),
                    ),
                    (
                        "StartDir".to_string(),
                        crate::vdf::VdfValue::Str(
                            "\"/run/media/rafeu/SSD/Piratas/Jogo\"".to_string(),
                        ),
                    ),
                ]),
            )]),
        )];
        fs::write(
            userdata.join("shortcuts.vdf"),
            crate::vdf::write_binary(&shortcuts_tree),
        )
        .unwrap();

        let real_install_dir = work.join("Piratas").join("Jogo Real");
        fs::create_dir_all(&real_install_dir).unwrap();

        let rsync = always_succeeds_rsync(&work);
        let lock_path = work.join("session.lock");
        let remote_installs = work.join("remote-installs");
        let mut cfg = Config {
            remote_installs_base_path: Some(remote_installs),
            ..base_config(&local_lib)
        };
        cfg.install_path_overrides
            .piratas
            .insert("1234567890".to_string(), real_install_dir.clone());

        let result = run_session_with_options(
            &cfg,
            Launcher::Steam,
            "1234567890",
            &["/bin/true".to_string()],
            Some(&steam_root),
            rsync.to_str().unwrap(),
            "ssh",
            &lock_path,
            None,
            |_, _| {},
        );

        assert!(result.is_ok(), "esperava sucesso, obteve {result:?}");
        assert!(
            !real_install_dir.exists(),
            "deveria ter usado o override (caminho real) e apagado ele após o upload, não \
             tentado usar o StartDir morto do atalho"
        );
    }

    #[test]
    fn full_install_heroic_uses_epic_override_basename_for_remote_dir_name() {
        // Regressão do incidente real (2026-09-07, "Nobody Wants to Die"):
        // sem `install_path_overrides.epic`, `local_dir` era sempre
        // `local_game_root_epic.join(<título>)` — o título do Heroic ao pé
        // da letra, com espaços — e o `remote_dir` usava esse mesmo título
        // cru. Mas `ingest`/`sync_full_install` nomeia a pasta remota a
        // partir do BASENAME do `local_dir` já resolvido (que, com override
        // configurado, é o nome real da pasta local, sem espaço nenhum) —
        // então `run` tentava baixar de um caminho remoto que nunca existiu
        // (`.../epic/Nobody Wants to Die`) enquanto os dados de verdade
        // estavam em `.../epic/NobodyWantsToDie`. `run` e `ingest` agora
        // concordam no mesmo nome de pasta remota.
        let work = test_dir("full-heroic-epic-override");
        let heroic_root = work.join("heroic-prefixes");
        fs::create_dir_all(&heroic_root).unwrap();

        // Override aponta pra uma pasta sem espaço, diferente do título —
        // e deliberadamente NÃO existe ainda local, pra forçar o download
        // da instalação completa (senão `!local_dir.exists()` já pularia
        // a chamada de rsync que o teste quer inspecionar).
        let override_dir = work.join("epic-override").join("NobodyWantsToDie");

        let log_path = work.join("rsync-calls.log");
        let rsync = logging_rsync(&work, &log_path);
        let lock_path = work.join("session.lock");

        let mut cfg = Config {
            heroic_local_root: Some(heroic_root.clone()),
            heroic_remote_base_path: Some(work.join("remote-heroic-prefixes")),
            local_game_root_epic: Some(work.join("epic-default-root")),
            remote_installs_base_path: Some(work.join("remote-installs")),
            ..Config::test_default()
        };
        cfg.install_path_overrides
            .epic
            .insert("Nobody Wants to Die".to_string(), override_dir.clone());

        let result = run_session_with_options(
            &cfg,
            Launcher::Heroic,
            "Nobody Wants to Die",
            &["/bin/true".to_string()],
            None,
            rsync.to_str().unwrap(),
            "ssh",
            &lock_path,
            None,
            |_, _| {},
        );

        assert!(result.is_ok(), "esperava sucesso, obteve {result:?}");
        let calls = fs::read_to_string(&log_path).unwrap_or_default();
        assert!(
            calls.contains("remote-installs/epic/NobodyWantsToDie"),
            "esperava o rsync pedir o caminho remoto nomeado pelo basename do \
             override (sem espaço); chamadas registradas: {calls}"
        );
        assert!(
            !calls.contains("epic/Nobody Wants to Die"),
            "não deveria mais tentar o caminho remoto com o título cru (com \
             espaço) — esse caminho nunca existe no FeuVault; chamadas: {calls}"
        );
    }

    #[test]
    fn run_session_cleans_up_the_legendary_entry_after_purging_a_heroic_full_install() {
        // Regressão real (2026-09-07): `upload_and_cleanup` apagava a
        // instalação completa de um jogo Heroic ao fim de CADA sessão de
        // `run` sem nunca limpar a entrada correspondente no
        // `installed.json` — o catálogo (SUPER+G) continuava mostrando o
        // jogo como "Cliente"/"Cliente+Servidor" mesmo já 100% sincronizado
        // de volta. Ver `clean_stale_legendary_entry_at`.
        let work = test_dir("run-cleans-legendary-entry");
        let heroic_root = work.join("heroic-prefixes");
        fs::create_dir_all(&heroic_root).unwrap();
        let epic_root = work.join("epic-default-root");
        let local_dir = epic_root.join("Chivalry 2");
        fs::create_dir_all(&local_dir).unwrap();
        fs::write(local_dir.join("game.bin"), b"fake install").unwrap();

        let installed_json = work.join("installed.json");
        fs::write(
            &installed_json,
            r#"{"abc123": {"title": "Chivalry 2", "install_path": "/qualquer/coisa"}}"#,
        )
        .unwrap();

        let rsync = always_succeeds_rsync(&work);
        let lock_path = work.join("session.lock");
        let cfg = Config {
            heroic_local_root: Some(heroic_root),
            heroic_remote_base_path: Some(work.join("remote-heroic-prefixes")),
            local_game_root_epic: Some(epic_root),
            remote_installs_base_path: Some(work.join("remote-installs")),
            ..Config::test_default()
        };

        let result = run_session_with_options(
            &cfg,
            Launcher::Heroic,
            "Chivalry 2",
            &["/bin/true".to_string()],
            None,
            rsync.to_str().unwrap(),
            "ssh",
            &lock_path,
            Some(&installed_json),
            |_, _| {},
        );

        assert!(result.is_ok(), "esperava sucesso, obteve {result:?}");
        assert!(
            !local_dir.exists(),
            "instalação completa deveria ter sido apagada após o upload"
        );
        let content = fs::read_to_string(&installed_json).unwrap();
        let map: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(&content).unwrap();
        assert!(
            !map.contains_key("abc123"),
            "entrada obsoleta de 'Chivalry 2' deveria ter sido removida do installed.json"
        );
    }
}
