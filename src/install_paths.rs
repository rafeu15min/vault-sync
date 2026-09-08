//! Fase VII: descoberta de onde a *instalação completa* de um jogo mora de
//! verdade — o pedaço grande do disco (`steamapps/common/<installdir>`,
//! `StartDir` de atalho non-Steam, `install_path` do Legendary), nunca o
//! `compatdata`/prefixo (isso continua em `paths.rs`, inalterado).
//!
//! Princípio único deste módulo, motivado por uma investigação real: o
//! `libraryfolders.vdf` da Steam, os `shortcuts.vdf` non-Steam e o
//! `installed.json` do Legendary acumulam caminhos obsoletos ao longo dos
//! anos (discos renomeados, HDs externos remontados) e continuam
//! reportando esses caminhos como válidos mesmo quando o diretório não
//! existe mais. Por isso, nenhuma função aqui devolve um caminho sem
//! confirmar `Path::is_dir()` primeiro — o valor gravado no metadado é só
//! um candidato, nunca a fonte de verdade final.

use crate::scan;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

fn installdir_regex() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r#""installdir"\s+"([^"]+)""#).unwrap())
}

/// Recusa candidatos perigosos antes mesmo de valer a pena checar `is_dir()`:
/// o próprio `$HOME`, um ancestral dele (ex.: "/", "/home", "/mnt") ou um
/// caminho raso demais pra ser uma pasta de jogo de verdade. Guarda central
/// pra todo resolver deste módulo — `StartDir`/`install_path`/override
/// gravados por launchers externos (Lutris, Legendary) não são confiáveis o
/// bastante pra alimentar um `remove_dir_all` sem essa checagem.
///
/// Incidente real que motivou isso: um atalho Lutris tinha
/// `StartDir="/home/rafeu"`, `ingest --full --and-purge-local` levou
/// ~106GB do home pro FeuVault e só não apagou `/home/rafeu` inteiro porque
/// um erro de permissão não relacionado interrompeu o `remove_dir_all` no
/// meio do caminho (ver memória do projeto — near-miss de 2026-08-14).
pub fn is_safe_install_dir(candidate: &Path, home_dir: Option<&Path>) -> bool {
    let resolved = candidate
        .canonicalize()
        .unwrap_or_else(|_| candidate.to_path_buf());
    if resolved == Path::new("/") {
        return false;
    }
    if let Some(home) = home_dir {
        let home = home.canonicalize().unwrap_or_else(|_| home.to_path_buf());
        if resolved == home || home.starts_with(&resolved) {
            return false;
        }
    }
    // "/" + pelo menos 2 níveis reais (ex.: /mnt/SSD2, /tmp/algo) — rejeita
    // só as raízes de topo inteiras (/, /mnt, /home, /etc...). A checagem
    // de `$HOME`/ancestral acima é a defesa primária contra o incidente
    // real; esta é só uma rede de segurança adicional pra outras raízes.
    resolved.components().count() >= 3
}

/// Confirma um candidato final: existe no disco E passou pela guarda de
/// segurança acima. Usado por todo resolver deste módulo no lugar de um
/// `candidate.is_dir().then_some(candidate)` cru.
fn verified_candidate(candidate: PathBuf) -> Option<PathBuf> {
    (candidate.is_dir() && is_safe_install_dir(&candidate, dirs::home_dir().as_deref()))
        .then_some(candidate)
}

/// Verifica um candidato vindo de `Config::install_path_overrides` — mesma
/// regra de "nunca confiar sem checar" das outras fontes: um override
/// também pode ficar desatualizado se o usuário mover o jogo de novo sem
/// atualizar o `config.json`. Pública porque `ingest.rs` usa isto sozinho
/// (sem cair pro `shortcuts.vdf`) pra rastrear a instalação completa de
/// jogos piratas — o `StartDir` de um atalho non-Steam é só o que o
/// usuário digitou uma vez ao criar o atalho, nunca atualizado depois, e
/// não é uma fonte confiável de "onde o jogo está" (diferente do manifesto
/// da Steam ou do `installed.json` do Legendary, mantidos pelas próprias
/// plataformas) — só o override explícito do usuário é.
pub fn verified_override(override_path: Option<&Path>) -> Option<PathBuf> {
    override_path.and_then(|p| verified_candidate(p.to_path_buf()))
}

/// Só a leitura do `installdir` gravado no `appmanifest_<app_id>.acf` de
/// `library`, **sem** checar se `steamapps/common/<installdir>` existe de
/// verdade. Existe separado de `resolve_official_install_dir` porque tem
/// um uso diferente: uma vez que um jogo está sob rastreamento de
/// instalação completa (Fase VII), o `run` nunca apaga esse manifesto (é a
/// única memória persistente do mapeamento app_id→pasta — ver
/// `session.rs`), então precisa conseguir ler o nome mesmo quando
/// `common/<installdir>` está vazio no ciclo atual (jogo "fora", só no
/// FeuVault). Quem precisa confirmar existência real usa
/// `resolve_official_install_dir`.
pub fn manifest_installdir(library: &Path, app_id: &str) -> Option<String> {
    let manifest = library
        .join("steamapps")
        .join(format!("appmanifest_{app_id}.acf"));
    let content = std::fs::read_to_string(manifest).ok()?;
    parse_installdir_from_acf(&content)
}

/// Núcleo de `manifest_installdir`, operando sobre o conteúdo já lido em
/// vez do caminho do arquivo — reaproveitado por
/// `session::restore_official_manifest_snapshot` pra extrair o `installdir`
/// de um manifesto trazido do FeuVault (ver `steam-manifests/<app_id>.acf`
/// em `ingest.rs`) sem precisar escrevê-lo em disco primeiro só pra
/// reler.
pub fn parse_installdir_from_acf(content: &str) -> Option<String> {
    Some(installdir_regex().captures(content)?[1].to_string())
}

/// Entradas que o Wine/Proton cria sozinho em `AppData/Local` de todo
/// prefixo novo, antes de qualquer jogo rodar — usado por
/// `looks_like_empty_compat_tool_prefix` como o "esqueleto padrão" contra
/// o qual comparar.
const DEFAULT_APPDATA_LOCAL_ENTRIES: &[&str] = &["Microsoft", "Temp"];

/// Um `compatdata/<id>` cujo `AppData/Local` só tem o esqueleto padrão do
/// Wine (nada além de `Microsoft`/`Temp`) **e** cujo `AppData/LocalLow`
/// está vazio/ausente nunca teve um jogo de verdade rodando ali — um jogo
/// real sempre grava algo a mais assim que roda uma vez. Comparado
/// empiricamente contra jogos confirmados de verdade nesta máquina:
/// `Program Files` e a contagem de `tracked_files` são idênticos entre
/// bookkeeping de ferramenta de compatibilidade e jogo real (ambos só têm
/// o esqueleto do Wine/Proton ali) — `AppData/Local`+`LocalLow` juntos é o
/// sinal que diferencia os dois de forma confiável.
///
/// `LocalLow` foi adicionado depois de um falso positivo real (2026-08-29):
/// Cloudpunk (motor Unity) salva em `AppData/LocalLow/Ion Lands/Cloudpunk/
/// Slot0.sav`, nunca em `Local` — checar só `Local` classificava um
/// prefixo com save de verdade (e mais novo que a cópia no FeuVault) como
/// bookkeeping vazio, o que o deixava invisível pro `ingest --full`
/// (`continue` silencioso, nem reportado em `full_skipped`) e o save novo
/// nunca seria sincronizado de volta. Unity é só o caso confirmado; nada
/// nesta função depende do motor específico, `LocalLow` é sempre um sinal
/// forte de aplicação real independente de qual.
///
/// Usado pra reconhecer automaticamente, em qualquer máquina, tanto o
/// bookkeeping compartilhado da própria Steam (app_id `"0"`) quanto o de
/// ferramentas de compatibilidade locais (Proton, builds da comunidade
/// como CachyOS/UMU-Proton) — sem depender de lista de exclusão nem de
/// reconhecer nome de ferramenta nenhuma.
pub fn looks_like_empty_compat_tool_prefix(compatdata_dir: &Path) -> bool {
    let appdata = compatdata_dir
        .join("pfx")
        .join("drive_c")
        .join("users")
        .join("steamuser")
        .join("AppData");

    // BUG real (2026-08-29, achado pelo usuário): a versão anterior dava
    // `return true` aqui embaixo assim que `AppData/Local` não existisse,
    // sem nunca chegar a olhar `LocalLow` -- exatamente o caso do
    // Cloudpunk (Unity nunca cria `Local`, só `LocalLow`), então o fix da
    // LocalLow nunca era executado pra ele. Local ausente/vazio não é
    // decisivo sozinho: só decide "parece vazio" depois de checar as
    // DUAS pastas.
    let local_is_skeleton = match std::fs::read_dir(appdata.join("Local")) {
        Ok(entries) => entries.flatten().all(|entry| {
            DEFAULT_APPDATA_LOCAL_ENTRIES.contains(&entry.file_name().to_string_lossy().as_ref())
        }),
        Err(_) => true, // pasta nem existe: consistente com esqueleto (nada rodou via Local)
    };
    if !local_is_skeleton {
        return false;
    }

    // `AppData/LocalLow` nunca é criado pelo esqueleto padrão do
    // Wine/Proton — só existe quando algum jogo/app grava ali de
    // propósito, então qualquer conteúdo (mesmo um único arquivo) já é
    // sinal suficiente de que rodou algo de verdade.
    match std::fs::read_dir(appdata.join("LocalLow")) {
        Ok(mut entries) => entries.next().is_none(),
        Err(_) => true,
    }
}

/// Extrai um campo numérico do `appmanifest_<app_id>.acf` (ex.:
/// `BytesToDownload`, `BytesDownloaded`) — mesmo formato chave/valor entre
/// aspas do `installdir_regex`, só que com valor numérico.
fn manifest_numeric_field(content: &str, field: &str) -> Option<u64> {
    let pattern = format!(r#""{field}"\s+"(\d+)""#);
    let re = regex::Regex::new(&pattern).ok()?;
    re.captures(content)?[1].parse().ok()
}

/// Um jogo com download/atualização em andamento não está pronto pra
/// sincronizar — descoberto na prática: `ingest --full` pegou um jogo
/// grande ainda baixando (achou o manifesto + a pasta `common/` já
/// existindo parcialmente) e o `rsync` falhou no meio porque os arquivos
/// mudavam durante a cópia. `BytesToDownload`/`BytesDownloaded` do próprio
/// manifesto da Steam são a fonte mais direta pra detectar isso. Ausência
/// de qualquer um dos dois campos (manifesto antigo/incomum) não bloqueia
/// — só quando os dois estão presentes E divergem.
fn is_download_in_progress(library: &Path, app_id: &str) -> bool {
    let manifest = library
        .join("steamapps")
        .join(format!("appmanifest_{app_id}.acf"));
    let Ok(content) = std::fs::read_to_string(manifest) else {
        return false;
    };
    match (
        manifest_numeric_field(&content, "BytesToDownload"),
        manifest_numeric_field(&content, "BytesDownloaded"),
    ) {
        (Some(total), Some(done)) => done < total,
        _ => false,
    }
}

/// Steam oficial: primeiro checa `override_path` (vindo de
/// `Config::install_path_overrides.steam`, ver `config.rs`) — se
/// verificável, ganha sem nem olhar o manifesto. Senão, cai pro caminho
/// original: lê `installdir` do `appmanifest_<app_id>.acf` dentro de
/// `library` e verifica se `library/steamapps/common/<installdir>` existe
/// de verdade — e se o download já terminou (ver `is_download_in_progress`).
/// Nunca consulta o `libraryfolders.vdf` global da Steam — essa é
/// justamente a fonte cheia de entradas obsoletas (o mesmo disco físico
/// registrado sob vários caminhos diferentes ao longo do tempo). Quem
/// chama itera sobre as `local_libraries` configuradas até achar.
pub fn resolve_official_install_dir(
    library: &Path,
    app_id: &str,
    override_path: Option<&Path>,
) -> Option<PathBuf> {
    if let Some(dir) = verified_override(override_path) {
        return Some(dir);
    }
    if is_download_in_progress(library, app_id) {
        return None;
    }
    let installdir = manifest_installdir(library, app_id)?;
    let candidate = library.join("steamapps").join("common").join(installdir);
    verified_candidate(candidate)
}

/// Steam pirata (atalho non-Steam): idem, `override_path` primeiro (vindo
/// de `install_path_overrides.piratas`), senão lê `StartDir` do
/// `shortcuts.vdf` que contém `app_id` (via
/// `scan::find_shortcut_start_dir`) e verifica se esse caminho existe de
/// verdade. `None` tanto quando não há atalho correspondente quanto
/// quando o `StartDir` gravado aponta pra um caminho morto — os dois
/// casos são "não verificável", quem chama decide como reportar a
/// diferença (hoje não precisa: ambos significam "pule este jogo, não
/// invente um caminho").
pub fn resolve_shortcut_install_dir(
    steam_root: &Path,
    app_id: u32,
    override_path: Option<&Path>,
) -> Option<PathBuf> {
    if let Some(dir) = verified_override(override_path) {
        return Some(dir);
    }
    let start_dir = scan::find_shortcut_start_dir(steam_root, app_id)?;
    let candidate = PathBuf::from(start_dir);
    verified_candidate(candidate)
}

/// Caminho convencional do `installed.json` do Legendary (backend do
/// Heroic) — não configurável, mesmo precedente de `scan::find_steam_root`
/// pros caminhos convencionais da Steam.
pub fn default_legendary_installed_json_path() -> Option<PathBuf> {
    Some(
        dirs::config_dir()?
            .join("heroic")
            .join("legendaryConfig")
            .join("legendary")
            .join("installed.json"),
    )
}

#[derive(Debug, Deserialize)]
struct LegendaryEntry {
    title: String,
    install_path: PathBuf,
}

/// Todos os títulos presentes no `installed.json`, sem verificar se o
/// `install_path` gravado ainda existe — só a lista de candidatos. Quem
/// chama usa `resolve_legendary_install_path` por título pra confirmar de
/// verdade (mesmo princípio de "nunca confiar num caminho sem checar" do
/// resto do módulo). Existe pra descobrir títulos Epic/Heroic que nunca
/// passaram pelo Wine (jogos nativos Linux) — sem prefixo local, eles são
/// invisíveis a qualquer varredura baseada em `heroic_local_root`.
pub fn list_legendary_installed_titles(installed_json: &Path) -> Vec<String> {
    let Ok(content) = std::fs::read_to_string(installed_json) else {
        return Vec::new();
    };
    let Ok(entries) = serde_json::from_str::<HashMap<String, LegendaryEntry>>(&content) else {
        return Vec::new();
    };
    entries.into_values().map(|entry| entry.title).collect()
}

/// Heroic/Epic: idem, `override_path` primeiro (vindo de
/// `install_path_overrides.epic`) — a fonte mais recomendada pra Epic, já
/// que o `install_path` do Legendary provou ficar obsoleto sozinho durante
/// o próprio desenvolvimento desta função (ver README/BACKLOG). Sem
/// override, lê `install_path` da entrada do `installed.json` cujo
/// `title` bate com o título do prefixo (a mesma chave já usada por
/// `heroic_local_root/<título>`), e verifica se esse caminho existe de
/// verdade. A chave de nível superior do JSON é um hash interno do
/// Legendary, não o título — por isso a busca é por valor do campo
/// `title`, não por chave direta.
pub fn resolve_legendary_install_path(
    installed_json: &Path,
    title: &str,
    override_path: Option<&Path>,
) -> Option<PathBuf> {
    if let Some(dir) = verified_override(override_path) {
        return Some(dir);
    }
    let content = std::fs::read_to_string(installed_json).ok()?;
    let entries: HashMap<String, LegendaryEntry> = serde_json::from_str(&content).ok()?;
    let candidate = entries
        .into_values()
        .find(|entry| entry.title == title)?
        .install_path;
    verified_candidate(candidate)
}

/// Como `manifest_installdir` (Steam), mas pro `installed.json` do
/// Legendary: nome cru da pasta de instalação de `title`, sem checar se
/// ela existe de verdade. Usado só pra comparar contra uma listagem remota
/// já confirmada (ver `ingest.rs`) quando a resolução verificada
/// (`resolve_legendary_install_path`) falhou — o `install_path` do
/// Legendary provou ficar obsoleto sozinho nesta máquina mais de uma vez
/// (ver README/BACKLOG), então uma pasta ausente não significa
/// necessariamente "nunca instalado": pode só significar "já migrado e
/// apagado, o `installed.json` que não foi atualizado". Nunca usado pra
/// decidir se um caminho é seguro de ler/escrever, só pra exibição/
/// comparação de nome.
pub fn legendary_installdir_name(installed_json: &Path, title: &str) -> Option<String> {
    let content = std::fs::read_to_string(installed_json).ok()?;
    let entries: HashMap<String, LegendaryEntry> = serde_json::from_str(&content).ok()?;
    let install_path = entries
        .into_values()
        .find(|entry| entry.title == title)?
        .install_path;
    install_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
}

/// Remove a entrada de `title` do `installed.json` do Legendary,
/// preservando os demais campos e entradas intactos — round-trip via
/// `serde_json::Value` genérico, não via `LegendaryEntry` (esse struct só
/// conhece `title`/`install_path`; uma (de)serialização por ele perderia
/// todo o resto do schema do Legendary nas entradas que sobram). A chave de
/// nível superior é um hash interno do Legendary, então a busca é pelo
/// campo `title`, mesmo princípio de `resolve_legendary_install_path`.
///
/// Só chamado por `ingest --full --and-purge-local` (ver `ingest.rs`)
/// depois de confirmar, via listagem SSH do FeuVault, que a instalação
/// completa de `title` já está lá e não existe prefixo Wine local nenhum
/// pra ele — nesse ponto o registro aqui é só metadado obsoleto (mesmo
/// raciocínio documentado em `resolve_legendary_install_path`: o
/// `install_path` do Legendary provou ficar obsoleto sozinho nesta
/// máquina). Devolve `false` sem erro se `title` não for encontrado
/// (idempotente — seguro chamar de novo numa execução futura).
pub fn remove_legendary_installed_entry(
    installed_json: &Path,
    title: &str,
) -> std::io::Result<bool> {
    let content = std::fs::read_to_string(installed_json)?;
    let mut entries: serde_json::Map<String, serde_json::Value> = serde_json::from_str(&content)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let key = entries
        .iter()
        .find(|(_, v)| v.get("title").and_then(|t| t.as_str()) == Some(title))
        .map(|(k, _)| k.clone());
    let Some(key) = key else {
        return Ok(false);
    };
    entries.remove(&key);
    let updated = serde_json::to_string_pretty(&entries)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(installed_json, updated)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vdf::{self, VdfValue};
    use std::fs;

    fn test_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "vault-sync-install-paths-test-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn appdata_local_dir(compatdata_dir: &Path) -> PathBuf {
        compatdata_dir
            .join("pfx")
            .join("drive_c")
            .join("users")
            .join("steamuser")
            .join("AppData")
            .join("Local")
    }

    #[test]
    fn looks_like_empty_compat_tool_prefix_true_when_only_default_wine_folders() {
        let compatdata = test_dir("compat-tool-empty");
        let appdata = appdata_local_dir(&compatdata);
        fs::create_dir_all(appdata.join("Microsoft")).unwrap();
        fs::create_dir_all(appdata.join("Temp")).unwrap();

        assert!(looks_like_empty_compat_tool_prefix(&compatdata));
    }

    #[test]
    fn looks_like_empty_compat_tool_prefix_false_when_a_game_folder_exists() {
        let compatdata = test_dir("compat-tool-real-game");
        let appdata = appdata_local_dir(&compatdata);
        fs::create_dir_all(appdata.join("Microsoft")).unwrap();
        fs::create_dir_all(appdata.join("Temp")).unwrap();
        // Uma pasta a mais (nome de estúdio, cache do motor, save) além do
        // esqueleto padrão do Wine prova que um jogo de verdade rodou ali.
        fs::create_dir_all(appdata.join("EstudioDoJogo")).unwrap();

        assert!(!looks_like_empty_compat_tool_prefix(&compatdata));
    }

    #[test]
    fn looks_like_empty_compat_tool_prefix_true_when_appdata_local_is_missing() {
        // Prefixo nem chegou a inicializar `AppData/Local` — também não é
        // sinal de jogo real.
        let compatdata = test_dir("compat-tool-no-appdata");
        assert!(looks_like_empty_compat_tool_prefix(&compatdata));
    }

    #[test]
    fn looks_like_empty_compat_tool_prefix_false_when_only_locallow_has_save_data() {
        // Regressão: Cloudpunk (Unity) salva em `AppData/LocalLow`, nunca
        // em `Local` — um prefixo assim tem `Local` batendo com o
        // esqueleto padrão do Wine, mas não é bookkeeping vazio nenhum.
        let compatdata = test_dir("compat-tool-locallow-save");
        let appdata_local = appdata_local_dir(&compatdata);
        fs::create_dir_all(appdata_local.join("Microsoft")).unwrap();
        fs::create_dir_all(appdata_local.join("Temp")).unwrap();
        let locallow = compatdata
            .join("pfx")
            .join("drive_c")
            .join("users")
            .join("steamuser")
            .join("AppData")
            .join("LocalLow")
            .join("Ion Lands")
            .join("Cloudpunk");
        fs::create_dir_all(&locallow).unwrap();
        fs::write(locallow.join("Slot0.sav"), b"save").unwrap();

        assert!(!looks_like_empty_compat_tool_prefix(&compatdata));
    }

    #[test]
    fn looks_like_empty_compat_tool_prefix_true_when_locallow_is_empty_too() {
        let compatdata = test_dir("compat-tool-locallow-empty");
        let appdata_local = appdata_local_dir(&compatdata);
        fs::create_dir_all(appdata_local.join("Microsoft")).unwrap();
        fs::create_dir_all(appdata_local.join("Temp")).unwrap();
        let locallow = compatdata
            .join("pfx")
            .join("drive_c")
            .join("users")
            .join("steamuser")
            .join("AppData")
            .join("LocalLow");
        fs::create_dir_all(&locallow).unwrap();

        assert!(looks_like_empty_compat_tool_prefix(&compatdata));
    }

    #[test]
    fn looks_like_empty_compat_tool_prefix_false_when_local_is_absent_but_locallow_has_save() {
        // Regressão real (2026-08-29): a primeira versão do fix de LocalLow
        // dava `return true` assim que `AppData/Local` não existisse, sem
        // nunca chegar a olhar LocalLow -- o caso exato do Cloudpunk
        // (Unity nunca cria `Local`, só `LocalLow`). O teste
        // "só_locallow_tem_save" acima não pegava isso porque criava
        // `Local` com o esqueleto; aqui `Local` nem existe.
        let compatdata = test_dir("compat-tool-local-absent-locallow-save");
        let locallow = compatdata
            .join("pfx")
            .join("drive_c")
            .join("users")
            .join("steamuser")
            .join("AppData")
            .join("LocalLow")
            .join("Ion Lands")
            .join("Cloudpunk");
        fs::create_dir_all(&locallow).unwrap();
        fs::write(locallow.join("Slot0.sav"), b"save").unwrap();

        assert!(!looks_like_empty_compat_tool_prefix(&compatdata));
    }

    #[test]
    fn manifest_installdir_reads_name_even_when_common_folder_is_absent() {
        let library = test_dir("manifest-only");
        let steamapps = library.join("steamapps");
        fs::create_dir_all(&steamapps).unwrap();
        // Igual ao estado "jogo fora, só no FeuVault" do rastreamento de
        // instalação completa: manifesto presente, common/ vazio.
        fs::write(
            steamapps.join("appmanifest_100.acf"),
            "\"installdir\"\t\t\"Meu Jogo\"\n",
        )
        .unwrap();

        assert_eq!(
            manifest_installdir(&library, "100"),
            Some("Meu Jogo".to_string())
        );
    }

    #[test]
    fn official_install_dir_resolves_when_manifest_and_folder_both_exist() {
        let library = test_dir("official-ok");
        let steamapps = library.join("steamapps");
        fs::create_dir_all(steamapps.join("common").join("Cyberpunk 2077")).unwrap();
        fs::write(
            steamapps.join("appmanifest_1091500.acf"),
            "\"AppState\"\n{\n\t\"installdir\"\t\t\"Cyberpunk 2077\"\n}\n",
        )
        .unwrap();

        let resolved = resolve_official_install_dir(&library, "1091500", None).unwrap();
        assert_eq!(resolved, steamapps.join("common").join("Cyberpunk 2077"));
    }

    #[test]
    fn official_install_dir_is_none_when_installdir_folder_is_missing() {
        let library = test_dir("official-stale");
        let steamapps = library.join("steamapps");
        fs::create_dir_all(&steamapps).unwrap();
        // Manifesto existe e aponta pro installdir, mas a pasta em
        // steamapps/common/ nunca foi criada (ex.: outro disco já removido).
        fs::write(
            steamapps.join("appmanifest_9.acf"),
            "\"installdir\"\t\t\"Jogo Fantasma\"\n",
        )
        .unwrap();

        assert!(resolve_official_install_dir(&library, "9", None).is_none());
    }

    #[test]
    fn official_install_dir_is_none_while_download_is_still_in_progress() {
        let library = test_dir("official-downloading");
        let steamapps = library.join("steamapps");
        // Pasta parcialmente criada pelo downloader da Steam — existe, mas
        // o manifesto diz que ainda faltam bytes.
        fs::create_dir_all(steamapps.join("common").join("Jogo Grande")).unwrap();
        fs::write(
            steamapps.join("appmanifest_500.acf"),
            "\"installdir\"\t\t\"Jogo Grande\"\n\"BytesToDownload\"\t\t\"80000000000\"\n\"BytesDownloaded\"\t\t\"12345678\"\n",
        )
        .unwrap();

        assert!(
            resolve_official_install_dir(&library, "500", None).is_none(),
            "download em andamento não deveria ser tratado como instalação pronta"
        );
    }

    #[test]
    fn official_install_dir_resolves_when_download_is_fully_complete() {
        let library = test_dir("official-download-done");
        let steamapps = library.join("steamapps");
        fs::create_dir_all(steamapps.join("common").join("Jogo Pronto")).unwrap();
        fs::write(
            steamapps.join("appmanifest_501.acf"),
            "\"installdir\"\t\t\"Jogo Pronto\"\n\"BytesToDownload\"\t\t\"1000\"\n\"BytesDownloaded\"\t\t\"1000\"\n",
        )
        .unwrap();

        assert_eq!(
            resolve_official_install_dir(&library, "501", None),
            Some(steamapps.join("common").join("Jogo Pronto"))
        );
    }

    #[test]
    fn official_install_dir_is_none_without_manifest() {
        let library = test_dir("official-no-manifest");
        fs::create_dir_all(library.join("steamapps")).unwrap();

        assert!(resolve_official_install_dir(&library, "12345", None).is_none());
    }

    #[test]
    fn official_install_dir_override_wins_over_manifest_when_verified() {
        let library = test_dir("official-override");
        let steamapps = library.join("steamapps");
        fs::create_dir_all(steamapps.join("common").join("Nome Antigo")).unwrap();
        fs::write(
            steamapps.join("appmanifest_100.acf"),
            "\"installdir\"\t\t\"Nome Antigo\"\n",
        )
        .unwrap();
        let override_dir = test_dir("official-override-target");

        let resolved = resolve_official_install_dir(&library, "100", Some(&override_dir)).unwrap();
        assert_eq!(
            resolved, override_dir,
            "override configurado deve ganhar do manifesto, sem nem precisar lê-lo"
        );
    }

    #[test]
    fn official_install_dir_falls_back_to_manifest_when_override_is_stale() {
        let library = test_dir("official-override-stale");
        let steamapps = library.join("steamapps");
        fs::create_dir_all(steamapps.join("common").join("Jogo Real")).unwrap();
        fs::write(
            steamapps.join("appmanifest_100.acf"),
            "\"installdir\"\t\t\"Jogo Real\"\n",
        )
        .unwrap();
        let stale_override = library.join("caminho-que-nao-existe");

        let resolved = resolve_official_install_dir(&library, "100", Some(&stale_override));
        assert_eq!(
            resolved,
            Some(steamapps.join("common").join("Jogo Real")),
            "override que não existe mais no disco não é confiado — cai pro manifesto, \
             mesma regra de qualquer outra fonte"
        );
    }

    fn shortcuts_tree_with_start_dir(app_id: u32, start_dir: &str) -> vdf::VdfObject {
        vec![(
            "shortcuts".to_string(),
            VdfValue::Object(vec![(
                "0".to_string(),
                VdfValue::Object(vec![
                    ("appid".to_string(), VdfValue::Int(app_id as i32)),
                    ("appname".to_string(), VdfValue::Str("Jogo".to_string())),
                    (
                        "StartDir".to_string(),
                        VdfValue::Str(format!("\"{start_dir}\"")),
                    ),
                ]),
            )]),
        )]
    }

    #[test]
    fn shortcut_install_dir_resolves_when_start_dir_exists_on_disk() {
        let dir = test_dir("shortcut-ok");
        let steam_root = dir.join("steam");
        let userdata = steam_root.join("userdata").join("1").join("config");
        fs::create_dir_all(&userdata).unwrap();
        let install_dir = dir.join("Piratas").join("Jogo");
        fs::create_dir_all(&install_dir).unwrap();
        fs::write(
            userdata.join("shortcuts.vdf"),
            vdf::write_binary(&shortcuts_tree_with_start_dir(
                42,
                install_dir.to_str().unwrap(),
            )),
        )
        .unwrap();

        let resolved = resolve_shortcut_install_dir(&steam_root, 42, None).unwrap();
        assert_eq!(resolved, install_dir);
    }

    #[test]
    fn shortcut_install_dir_is_none_when_start_dir_no_longer_exists() {
        let dir = test_dir("shortcut-stale");
        let steam_root = dir.join("steam");
        let userdata = steam_root.join("userdata").join("1").join("config");
        fs::create_dir_all(&userdata).unwrap();
        // Reproduz o caso real: StartDir aponta pra um HD externo
        // desmontado (`/run/media/...`), nunca criado aqui de propósito.
        fs::write(
            userdata.join("shortcuts.vdf"),
            vdf::write_binary(&shortcuts_tree_with_start_dir(
                42,
                "/run/media/rafeu/SSD/Piratas/Jogo",
            )),
        )
        .unwrap();

        assert!(resolve_shortcut_install_dir(&steam_root, 42, None).is_none());
    }

    #[test]
    fn legendary_install_path_resolves_when_title_matches_and_folder_exists() {
        let dir = test_dir("legendary-ok");
        let install_dir = dir.join("Epic").join("DiscoElysium");
        fs::create_dir_all(&install_dir).unwrap();
        let installed_json = dir.join("installed.json");
        fs::write(
            &installed_json,
            format!(
                r#"{{"abc123": {{"title": "Disco Elysium - The Final Cut", "install_path": "{}"}}}}"#,
                install_dir.display()
            ),
        )
        .unwrap();

        let resolved =
            resolve_legendary_install_path(&installed_json, "Disco Elysium - The Final Cut", None)
                .unwrap();
        assert_eq!(resolved, install_dir);
    }

    #[test]
    fn legendary_installdir_name_reads_folder_name_even_when_path_no_longer_exists() {
        // Reproduz o caso real: `install_path` do Legendary ficou obsoleto
        // (jogo já migrado e apagado, ou disco reorganizado) — diferente de
        // `resolve_legendary_install_path`, que exige a pasta existir de
        // verdade, isto só lê o nome cru pra comparar contra uma listagem
        // remota já confirmada.
        let dir = test_dir("legendary-installdir-name");
        let installed_json = dir.join("installed.json");
        fs::write(
            &installed_json,
            r#"{"abc123": {"title": "Chivalry 2", "install_path": "/mnt/SSD2/Epic/Chivalry2"}}"#,
        )
        .unwrap();

        assert_eq!(
            legendary_installdir_name(&installed_json, "Chivalry 2"),
            Some("Chivalry2".to_string())
        );
    }

    #[test]
    fn legendary_installdir_name_is_none_when_title_is_absent() {
        let dir = test_dir("legendary-installdir-name-absent");
        let installed_json = dir.join("installed.json");
        fs::write(&installed_json, r#"{}"#).unwrap();

        assert_eq!(
            legendary_installdir_name(&installed_json, "Qualquer Coisa"),
            None
        );
    }

    #[test]
    fn legendary_install_path_is_none_when_recorded_path_no_longer_exists() {
        let dir = test_dir("legendary-stale");
        let installed_json = dir.join("installed.json");
        // O caminho gravado (disco antigo "SSD1") nunca é criado aqui —
        // reproduz exatamente o caso real encontrado na investigação.
        fs::write(
            &installed_json,
            r#"{"xyz": {"title": "Hogwarts Legacy", "install_path": "/mnt/SSD1/EPIC/HogwartsLegacy"}}"#,
        )
        .unwrap();

        assert!(resolve_legendary_install_path(&installed_json, "Hogwarts Legacy", None).is_none());
    }

    #[test]
    #[ignore = "validação manual contra dados reais desta máquina, não roda em CI"]
    fn real_machine_legendary_install_path_matches_known_state() {
        let installed_json =
            Path::new("/home/rafeu/.config/heroic/legendaryConfig/legendary/installed.json");
        // Durante a Fase VII, a instalação do Disco Elysium se moveu de
        // /mnt/SSD2/Epic/ pra /mnt/HDD1/EPIC/ NO MEIO da sessão de
        // trabalho, sem o installed.json ser atualizado — prova real (não
        // hipotética) de por que este módulo nunca confia num caminho
        // gravado sem checar o disco primeiro. Os 4 `install_path` foram
        // corrigidos manualmente em 2026-08-14 (backup em
        // installed.json.vault-sync-bak) pra refletir a localização real
        // atual — se este teste falhar, é sinal de que algum jogo mudou de
        // disco de novo e o `install_path` precisa ser corrigido outra vez.
        for (title, expected) in [
            (
                "Disco Elysium - The Final Cut",
                "/mnt/HDD1/EPIC/DiscoElysium",
            ),
            ("Nobody Wants to Die", "/mnt/SSD2/Epic/NobodyWantsToDie"),
            ("Chivalry 2", "/mnt/SSD2/Epic/Chivalry2"),
            ("Hogwarts Legacy", "/mnt/SSD2/Epic/HogwartsLegacy"),
        ] {
            assert_eq!(
                resolve_legendary_install_path(installed_json, title, None),
                Some(PathBuf::from(expected)),
                "esperava '{title}' resolver pra '{expected}' — se isso falhar, o jogo \
                 provavelmente mudou de disco de novo (já aconteceu antes nesta mesma \
                 máquina) e o install_path precisa ser corrigido"
            );
        }
    }

    #[test]
    fn list_legendary_installed_titles_returns_every_title_regardless_of_disk_state() {
        let dir = test_dir("legendary-list");
        let installed_json = dir.join("installed.json");
        fs::write(
            &installed_json,
            r#"{
                "abc": {"title": "Jogo A", "install_path": "/nao/existe/a"},
                "def": {"title": "Jogo B", "install_path": "/nao/existe/b"}
            }"#,
        )
        .unwrap();

        let mut titles = list_legendary_installed_titles(&installed_json);
        titles.sort();
        assert_eq!(titles, vec!["Jogo A".to_string(), "Jogo B".to_string()]);
    }

    #[test]
    fn list_legendary_installed_titles_is_empty_when_file_is_missing() {
        assert!(
            list_legendary_installed_titles(Path::new("/nao/existe/installed.json")).is_empty()
        );
    }

    #[test]
    fn remove_legendary_installed_entry_removes_only_the_matching_title() {
        let dir = test_dir("legendary-remove-entry");
        let installed_json = dir.join("installed.json");
        fs::write(
            &installed_json,
            r#"{
                "aaa111": {"title": "Jogo A", "install_path": "/nao/existe/a", "install_size": 123, "executable": "a.exe"},
                "bbb222": {"title": "Jogo B", "install_path": "/nao/existe/b", "install_size": 456, "executable": "b.exe"}
            }"#,
        )
        .unwrap();

        let removed = remove_legendary_installed_entry(&installed_json, "Jogo A").unwrap();
        assert!(removed);

        let mut titles = list_legendary_installed_titles(&installed_json);
        titles.sort();
        assert_eq!(titles, vec!["Jogo B".to_string()]);

        // Campos que `LegendaryEntry` nem conhece (`install_size`,
        // `executable`) precisam sobreviver ao round-trip da entrada que
        // ficou — a função usa `serde_json::Value` genérico bem por isso.
        let remaining = fs::read_to_string(&installed_json).unwrap();
        assert!(remaining.contains("install_size"));
        assert!(remaining.contains("executable"));
        assert!(remaining.contains("456"));
    }

    #[test]
    fn remove_legendary_installed_entry_is_idempotent_when_title_absent() {
        let dir = test_dir("legendary-remove-entry-absent");
        let installed_json = dir.join("installed.json");
        fs::write(
            &installed_json,
            r#"{"aaa111": {"title": "Jogo A", "install_path": "/nao/existe/a"}}"#,
        )
        .unwrap();

        let removed =
            remove_legendary_installed_entry(&installed_json, "Jogo Inexistente").unwrap();
        assert!(!removed);
        assert_eq!(
            list_legendary_installed_titles(&installed_json),
            vec!["Jogo A".to_string()]
        );
    }

    #[test]
    fn legendary_install_path_is_none_when_title_is_absent() {
        let dir = test_dir("legendary-no-title");
        let installed_json = dir.join("installed.json");
        fs::write(&installed_json, r#"{}"#).unwrap();

        assert!(resolve_legendary_install_path(&installed_json, "Qualquer Coisa", None).is_none());
    }

    // Regressão do near-miss de 2026-08-14: um atalho Lutris com
    // StartDir=$HOME quase apagou a home inteira via `ingest --full
    // --and-purge-local` (só um erro de permissão não relacionado parou o
    // `remove_dir_all` no meio do caminho). Ver `is_safe_install_dir`.
    #[test]
    fn is_safe_install_dir_refuses_home_dir_itself() {
        let home = test_dir("fake-home");
        assert!(!is_safe_install_dir(&home, Some(&home)));
    }

    #[test]
    fn is_safe_install_dir_refuses_ancestor_of_home() {
        let home = test_dir("fake-home-nested").join("rafeu");
        fs::create_dir_all(&home).unwrap();
        let ancestor = home.parent().unwrap();
        assert!(!is_safe_install_dir(ancestor, Some(&home)));
    }

    #[test]
    fn is_safe_install_dir_refuses_filesystem_root() {
        assert!(!is_safe_install_dir(Path::new("/"), None));
    }

    #[test]
    fn is_safe_install_dir_refuses_shallow_top_level_path() {
        // Ex.: "/mnt" ou "/mnt/SSD2" sozinho — raso demais pra ser a pasta
        // de um jogo específico, mesmo não sendo $HOME nem ancestral dele.
        let shallow = test_dir("shallow");
        let top_level = shallow.parent().unwrap(); // .../vault-sync-install-paths-test-shallow-*  -> temp_dir()
        assert!(!is_safe_install_dir(top_level, None));
    }

    #[test]
    fn is_safe_install_dir_accepts_deep_real_game_folder() {
        let deep = test_dir("deep-ok").join("Piratas").join("Jogo");
        fs::create_dir_all(&deep).unwrap();
        assert!(is_safe_install_dir(&deep, None));
    }

    #[test]
    fn shortcut_install_dir_refuses_start_dir_equal_to_real_home_dir() {
        // Reproduz o incidente de ponta a ponta: StartDir gravado no
        // shortcuts.vdf aponta pro $HOME de verdade de quem roda o teste.
        let home = dirs::home_dir().expect("teste exige $HOME resolvível");
        let dir = test_dir("shortcut-home-incident");
        let steam_root = dir.join("steam");
        let userdata = steam_root.join("userdata").join("1").join("config");
        fs::create_dir_all(&userdata).unwrap();
        fs::write(
            userdata.join("shortcuts.vdf"),
            vdf::write_binary(&shortcuts_tree_with_start_dir(42, home.to_str().unwrap())),
        )
        .unwrap();

        assert!(
            resolve_shortcut_install_dir(&steam_root, 42, None).is_none(),
            "StartDir apontando pro $HOME nunca deveria resolver pra um install_dir"
        );
    }

    #[test]
    fn official_install_dir_refuses_override_pointing_at_home_dir() {
        let home = dirs::home_dir().expect("teste exige $HOME resolvível");
        let library = test_dir("official-override-home");
        assert!(resolve_official_install_dir(&library, "100", Some(&home)).is_none());
    }
}
