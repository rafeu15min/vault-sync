//! Motor de injeção autônoma de matrizes de execução (4.3). Varre as
//! entradas de `shortcuts.vdf` (jogos non-Steam) em busca do marcador do
//! orquestrador em `LaunchOptions` e, quando ausente, o injeta de forma
//! não-destrutiva — a árvore inteira é preservada, só o campo necessário é
//! ajustado, e um backup atômico (`backup.rs`) precede qualquer escrita.
//!
//! Pensado para rodar a cada `scan` (e, futuramente, sob um timer systemd na
//! Fase V) — "execução periódica programada" da spec.

use crate::backup::atomic_backup;
use crate::scan;
use crate::steam_appid;
use crate::text_vdf::{self, TextVdfError};
use crate::vdf::{self, VdfError, VdfObject, VdfValue};
use std::path::{Path, PathBuf};

/// Prefixo que identifica uma `LaunchOptions` já orquestrada pelo
/// vault-sync — usado tanto para detectar quanto para montar o wrapper.
pub const MARKER: &str = "vault-sync run";

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct InjectionSummary {
    pub inspected: usize,
    pub injected: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum InjectError {
    #[error("falha de E/S ao processar '{0}': {1}")]
    Io(PathBuf, #[source] std::io::Error),
    #[error("falha ao decodificar VDF binário '{0}': {1}")]
    Vdf(PathBuf, #[source] VdfError),
    #[error("falha ao decodificar VDF em texto '{0}': {1}")]
    TextVdf(PathBuf, #[source] TextVdfError),
}

/// Varre um `shortcuts.vdf` específico e injeta `vault-sync run <appid> --
/// ` em qualquer `LaunchOptions` que ainda não contenha o marcador. Só
/// escreve o arquivo (com backup atômico prévio) se algo de fato mudou.
pub fn inject_shortcuts_file(path: &Path) -> Result<InjectionSummary, InjectError> {
    let raw = std::fs::read(path).map_err(|e| InjectError::Io(path.to_path_buf(), e))?;
    let mut root = vdf::parse_binary(&raw).map_err(|e| InjectError::Vdf(path.to_path_buf(), e))?;

    let mut summary = InjectionSummary::default();
    let mut changed = false;

    if let Some((_, VdfValue::Object(shortcuts))) = root
        .iter_mut()
        .find(|(k, _)| k.eq_ignore_ascii_case("shortcuts"))
    {
        for (_, entry) in shortcuts.iter_mut() {
            let VdfValue::Object(fields) = entry else {
                continue;
            };
            summary.inspected += 1;

            let app_id = fields
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("appid"))
                .and_then(|(_, v)| match v {
                    VdfValue::Int(i) => Some((*i as u32).to_string()),
                    _ => None,
                });
            let Some(app_id) = app_id else {
                continue;
            };

            if inject_launch_options(fields, &app_id) {
                changed = true;
                summary.injected += 1;
            }
        }
    }

    if changed {
        atomic_backup(path).map_err(|e| InjectError::Io(path.to_path_buf(), e))?;
        std::fs::write(path, vdf::write_binary(&root))
            .map_err(|e| InjectError::Io(path.to_path_buf(), e))?;
    }

    Ok(summary)
}

fn inject_launch_options(fields: &mut VdfObject, app_id: &str) -> bool {
    let wrapper_prefix = format!("{MARKER} {app_id} --steam -- ");
    let existing = fields
        .iter_mut()
        .find(|(k, _)| k.eq_ignore_ascii_case("launchoptions"));

    match existing {
        Some((_, VdfValue::Str(value))) => {
            if value.contains(MARKER) {
                false
            } else {
                *value = format!("{wrapper_prefix}{value}");
                true
            }
        }
        // Campo com tipo inesperado (nunca deveria ser Int/Object) — não
        // mexe, para nunca sobrescrever algo que não entendemos.
        Some(_) => false,
        None => {
            fields.push((
                "LaunchOptions".to_string(),
                VdfValue::Str(wrapper_prefix.trim_end().to_string()),
            ));
            true
        }
    }
}

/// Aplica [`inject_shortcuts_file`] a todos os `shortcuts.vdf` encontrados
/// sob `<steam_root>/userdata/*/config/` — um por usuário local da Steam.
pub fn inject_all_shortcuts(
    steam_root: &Path,
) -> Vec<(PathBuf, Result<InjectionSummary, InjectError>)> {
    scan::for_each_userdata_file(steam_root, "shortcuts.vdf", inject_shortcuts_file)
}

/// Varre um `localconfig.vdf` específico e injeta `vault-sync run <appid>
/// --steam -- ` no `LaunchOptions` de cada jogo Steam OFICIAL rastreado
/// (`tracked_app_ids`) que ainda não o tenha — o mesmo tratamento que
/// [`inject_shortcuts_file`] já dá pra atalhos non-Steam, só que num
/// arquivo e caminho completamente diferentes
/// (`Software\Valve\Steam\apps\<id>\LaunchOptions`, formato TEXTO — ver
/// `text_vdf.rs`).
///
/// Lacuna estrutural que motivou isso (2026-08-31): o motor de injeção
/// original só cobria `shortcuts.vdf`, então clicar "Play" direto na Steam
/// pra QUALQUER jogo oficial sempre pulou o vault-sync inteiro — nunca foi
/// um problema de um jogo específico, é assim desde o início do projeto.
/// Sem passar pelo `vault-sync run`, nada garante o download do compatdata
/// certo antes de jogar nem a sincronização/limpeza depois — o jogo roda
/// direto sobre o que estiver localmente, seja o que for.
///
/// Só injeta em blocos de app que já existem em `apps{}` — nunca cria uma
/// entrada do zero pra um jogo que a Steam ainda não tem registro nenhum
/// (mesmo princípio conservador de `inject_shortcuts_file`, que também
/// nunca inventa um atalho).
pub fn inject_official_launch_options(
    path: &Path,
    tracked_app_ids: &[u32],
) -> Result<InjectionSummary, InjectError> {
    let raw = std::fs::read_to_string(path).map_err(|e| InjectError::Io(path.to_path_buf(), e))?;
    let mut root =
        text_vdf::parse_text(&raw).map_err(|e| InjectError::TextVdf(path.to_path_buf(), e))?;

    let mut summary = InjectionSummary::default();
    let mut changed = false;

    if let Some(apps) = navigate_to_steam_apps_mut(&mut root) {
        for app_id in tracked_app_ids {
            let app_id_str = app_id.to_string();
            let Some((_, VdfValue::Object(fields))) =
                apps.iter_mut().find(|(k, _)| *k == app_id_str)
            else {
                continue;
            };
            summary.inspected += 1;
            if inject_launch_options(fields, &app_id_str) {
                changed = true;
                summary.injected += 1;
            }
        }
    }

    if changed {
        atomic_backup(path).map_err(|e| InjectError::Io(path.to_path_buf(), e))?;
        std::fs::write(path, text_vdf::write_text(&root))
            .map_err(|e| InjectError::Io(path.to_path_buf(), e))?;
    }

    Ok(summary)
}

/// Navega `UserLocalConfigStore -> Software -> Valve -> Steam -> apps`
/// (comparação sem diferenciar maiúsculas, igual ao resto do projeto) —
/// caminho verificado contra um `localconfig.vdf` real desta máquina. Há
/// mais de um bloco `"apps"` no arquivo (outros ramos não relacionados
/// reaproveitam o nome) — só este caminho exato é o namespace de
/// configuração por jogo da própria Steam.
fn navigate_to_steam_apps_mut(root: &mut VdfObject) -> Option<&mut VdfObject> {
    let path = ["UserLocalConfigStore", "Software", "Valve", "Steam", "apps"];
    let mut current = root;
    for (i, key) in path.iter().enumerate() {
        let (_, value) = current
            .iter_mut()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))?;
        let VdfValue::Object(inner) = value else {
            return None;
        };
        if i == path.len() - 1 {
            return Some(inner);
        }
        current = inner;
    }
    unreachable!("path nunca é vazio")
}

/// Aplica [`inject_official_launch_options`] a todos os `localconfig.vdf`
/// encontrados sob `<steam_root>/userdata/*/config/` — um por usuário
/// local da Steam, mesmo padrão de [`inject_all_shortcuts`].
pub fn inject_all_official_launch_options(
    steam_root: &Path,
    tracked_app_ids: &[u32],
) -> Vec<(PathBuf, Result<InjectionSummary, InjectError>)> {
    scan::for_each_userdata_file(steam_root, "localconfig.vdf", |p| {
        inject_official_launch_options(p, tracked_app_ids)
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StageShortcutOutcome {
    /// Atalho novo criado com sucesso, apontando pro caminho fixo
    /// (`local_game_root_piratas/<name>`).
    Created { app_id: u32, exe: String },
    /// Já existia um atalho com o AppID que seria criado agora — idempotente
    /// (`stage` rodado de novo pro mesmo jogo já consolidado), nada foi
    /// escrito.
    AlreadyUpToDate { app_id: u32 },
    /// Nenhum atalho existente com esse `appname` foi encontrado em nenhum
    /// `shortcuts.vdf` local — não há de onde copiar o nome do executável
    /// (primeiro cadastro de sempre pra este jogo), então nada é criado. O
    /// fluxo manual de sempre ("Add a Non-Steam Game" apontando pro `.exe`
    /// dentro da pasta que `stage` acabou de baixar) continua válido.
    NoPriorShortcutFound,
}

#[derive(Debug, thiserror::Error)]
pub enum StageShortcutError {
    #[error(
        "'{0}' não existe em {1} — o download do 'stage' terminou? (nome de arquivo reaproveitado \
         do atalho anterior)"
    )]
    ExeNotFoundAtNewPath(String, PathBuf),
    #[error("falha de E/S ao processar '{0}': {1}")]
    Io(PathBuf, #[source] std::io::Error),
    #[error("falha ao decodificar VDF binário '{0}': {1}")]
    Vdf(PathBuf, #[source] VdfError),
}

/// Cria (ou reconhece como já criado) o atalho Steam non-Steam de um jogo
/// pirata recém-consolidado em `local_dir` (`local_game_root_piratas/<name>`,
/// ver `Command::Stage` em main.rs) — fecha de vez o loop "ovo-e-galinha"
/// que `stage` só resolvia parcialmente até aqui (baixar os arquivos, mas
/// ainda exigir que o usuário apontasse manualmente "Add a Non-Steam Game").
///
/// Nunca inventa qual `.exe` usar: só age quando já existe um atalho
/// qualquer (em qualquer `userdata/`, mesmo com `Exe` obsoleto por remanejo
/// de disco — ver `scan::find_shortcut_exe_by_name`) com o mesmo `name`,
/// reaproveitando o NOME DO ARQUIVO daquele atalho contra a pasta nova.
/// Calcula o AppID resultante com o mesmo algoritmo que a própria Steam usa
/// (`steam_appid::compute_app_id`, validado contra dois AppIDs reais desta
/// máquina) e injeta uma entrada nova — nunca reescreve/apaga a antiga, que
/// fica só como resíduo (ver `scan::find_stale_shortcuts`, que existe
/// justamente pra sinalizar esses resíduos sem removê-los sozinho).
pub fn stage_pirata_shortcut(
    steam_root: &Path,
    name: &str,
    local_dir: &Path,
) -> Result<StageShortcutOutcome, StageShortcutError> {
    let Some((vdf_path, old_exe)) = scan::find_shortcut_exe_by_name(steam_root, name) else {
        return Ok(StageShortcutOutcome::NoPriorShortcutFound);
    };

    let file_name = Path::new(&old_exe).file_name().unwrap_or_default();
    let new_exe_path = local_dir.join(file_name);
    if !new_exe_path.is_file() {
        return Err(StageShortcutError::ExeNotFoundAtNewPath(
            old_exe,
            new_exe_path,
        ));
    }
    let new_exe = new_exe_path.to_string_lossy().into_owned();
    let app_id = steam_appid::compute_app_id(&new_exe, name);

    let raw = std::fs::read(&vdf_path).map_err(|e| StageShortcutError::Io(vdf_path.clone(), e))?;
    let mut root =
        vdf::parse_binary(&raw).map_err(|e| StageShortcutError::Vdf(vdf_path.clone(), e))?;

    let Some((_, VdfValue::Object(shortcuts))) = root
        .iter_mut()
        .find(|(k, _)| k.eq_ignore_ascii_case("shortcuts"))
    else {
        // `find_shortcut_exe_by_name` já achou uma entrada aqui — chegar
        // sem uma seção "shortcuts" válida não deveria acontecer. Nunca
        // inventa uma árvore do zero num arquivo que a própria Steam gravou.
        return Ok(StageShortcutOutcome::NoPriorShortcutFound);
    };

    let already_present = shortcuts.iter().any(|(_, entry)| {
        let VdfValue::Object(fields) = entry else {
            return false;
        };
        fields.iter().any(|(k, v)| {
            k.eq_ignore_ascii_case("appid") && matches!(v, VdfValue::Int(i) if *i as u32 == app_id)
        })
    });
    if already_present {
        return Ok(StageShortcutOutcome::AlreadyUpToDate { app_id });
    }

    let next_key = shortcuts
        .iter()
        .filter_map(|(k, _)| k.parse::<u32>().ok())
        .max()
        .map_or(0, |m| m + 1)
        .to_string();

    // Mesmo conjunto de campos que a própria Steam grava num atalho novo
    // (ver dump real capturado durante o incidente do Haste) — omitir um
    // campo que a Steam sempre escreve é arriscar um estado que ela nunca
    // gera sozinha; os valores default (0/vazio) são os mesmos que ela usa
    // pra um atalho recém-criado sem nenhuma opção customizada.
    let new_entry = VdfValue::Object(vec![
        ("appid".to_string(), VdfValue::Int(app_id as i32)),
        ("appname".to_string(), VdfValue::Str(name.to_string())),
        ("Exe".to_string(), VdfValue::Str(format!("\"{new_exe}\""))),
        (
            "StartDir".to_string(),
            VdfValue::Str(format!("\"{}\"", local_dir.display())),
        ),
        ("icon".to_string(), VdfValue::Str(String::new())),
        ("ShortcutPath".to_string(), VdfValue::Str(String::new())),
        (
            "LaunchOptions".to_string(),
            VdfValue::Str(format!("{MARKER} {app_id} --steam --")),
        ),
        ("IsHidden".to_string(), VdfValue::Int(0)),
        ("AllowDesktopConfig".to_string(), VdfValue::Int(1)),
        ("AllowOverlay".to_string(), VdfValue::Int(1)),
        ("OpenVR".to_string(), VdfValue::Int(0)),
        ("Devkit".to_string(), VdfValue::Int(0)),
        ("DevkitGameID".to_string(), VdfValue::Str(String::new())),
        ("DevkitOverrideAppID".to_string(), VdfValue::Int(0)),
        ("LastPlayTime".to_string(), VdfValue::Int(0)),
        ("FlatpakAppID".to_string(), VdfValue::Str(String::new())),
    ]);
    shortcuts.push((next_key, new_entry));

    atomic_backup(&vdf_path).map_err(|e| StageShortcutError::Io(vdf_path.clone(), e))?;
    std::fs::write(&vdf_path, vdf::write_binary(&root))
        .map_err(|e| StageShortcutError::Io(vdf_path.clone(), e))?;

    Ok(StageShortcutOutcome::Created {
        app_id,
        exe: new_exe,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn test_dir(label: &str) -> PathBuf {
        crate::testutil::test_dir("inject", label)
    }

    fn shortcut_tree(launch_options: Option<&str>) -> VdfObject {
        let mut fields = vec![
            ("appid".to_string(), VdfValue::Int(1234567890u32 as i32)),
            (
                "appname".to_string(),
                VdfValue::Str("Jogo Pirata".to_string()),
            ),
            ("Exe".to_string(), VdfValue::Str("/bin/jogo".to_string())),
        ];
        if let Some(opts) = launch_options {
            fields.push(("LaunchOptions".to_string(), VdfValue::Str(opts.to_string())));
        }
        vec![(
            "shortcuts".to_string(),
            VdfValue::Object(vec![("0".to_string(), VdfValue::Object(fields))]),
        )]
    }

    #[test]
    fn injects_marker_when_launch_options_is_missing() {
        let dir = test_dir("missing");
        let path = dir.join("shortcuts.vdf");
        fs::write(&path, vdf::write_binary(&shortcut_tree(None))).unwrap();

        let summary = inject_shortcuts_file(&path).unwrap();
        assert_eq!(summary.injected, 1);

        let rewritten = vdf::parse_binary(&fs::read(&path).unwrap()).unwrap();
        let opts = launch_options_of_first_shortcut(&rewritten);
        assert_eq!(opts, "vault-sync run 1234567890 --steam --");
    }

    #[test]
    fn wraps_existing_launch_options_preserving_original_value() {
        let dir = test_dir("wrap");
        let path = dir.join("shortcuts.vdf");
        fs::write(
            &path,
            vdf::write_binary(&shortcut_tree(Some("-fullscreen -w 1920"))),
        )
        .unwrap();

        inject_shortcuts_file(&path).unwrap();

        let rewritten = vdf::parse_binary(&fs::read(&path).unwrap()).unwrap();
        let opts = launch_options_of_first_shortcut(&rewritten);
        assert_eq!(
            opts,
            "vault-sync run 1234567890 --steam -- -fullscreen -w 1920"
        );
    }

    #[test]
    fn already_marked_entries_are_left_untouched_and_no_backup_is_created() {
        let dir = test_dir("already-marked");
        let path = dir.join("shortcuts.vdf");
        fs::write(
            &path,
            vdf::write_binary(&shortcut_tree(Some(
                "vault-sync run 1234567890 -- -fullscreen",
            ))),
        )
        .unwrap();

        let summary = inject_shortcuts_file(&path).unwrap();
        assert_eq!(summary.injected, 0);
        assert_eq!(summary.inspected, 1);

        let has_backup = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .any(|e| e.file_name().to_string_lossy().contains("vault-sync-bak"));
        assert!(!has_backup, "não deveria gerar backup sem nenhuma mudança");
    }

    #[test]
    fn injection_creates_atomic_backup_before_rewriting() {
        let dir = test_dir("backup");
        let path = dir.join("shortcuts.vdf");
        let original_bytes = vdf::write_binary(&shortcut_tree(None));
        fs::write(&path, &original_bytes).unwrap();

        inject_shortcuts_file(&path).unwrap();

        let backup = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .find(|e| e.file_name().to_string_lossy().contains("vault-sync-bak"))
            .expect("esperava um arquivo de backup");
        assert_eq!(fs::read(backup.path()).unwrap(), original_bytes);
    }

    #[test]
    fn preserves_unrelated_fields_untouched() {
        let dir = test_dir("preserve");
        let path = dir.join("shortcuts.vdf");
        fs::write(&path, vdf::write_binary(&shortcut_tree(None))).unwrap();

        inject_shortcuts_file(&path).unwrap();

        let rewritten = vdf::parse_binary(&fs::read(&path).unwrap()).unwrap();
        let VdfValue::Object(shortcuts) = &rewritten[0].1 else {
            panic!()
        };
        let VdfValue::Object(fields) = &shortcuts[0].1 else {
            panic!()
        };
        let exe = fields
            .iter()
            .find(|(k, _)| k == "Exe")
            .map(|(_, v)| v.clone());
        assert_eq!(exe, Some(VdfValue::Str("/bin/jogo".to_string())));
    }

    fn localconfig_tree(app_ids: &[(&str, Option<&str>)]) -> VdfObject {
        let app_entries: VdfObject = app_ids
            .iter()
            .map(|(id, launch_options)| {
                let mut fields = vec![(
                    "LastPlayed".to_string(),
                    VdfValue::Str("1788150145".to_string()),
                )];
                if let Some(opts) = launch_options {
                    fields.push(("LaunchOptions".to_string(), VdfValue::Str(opts.to_string())));
                }
                (id.to_string(), VdfValue::Object(fields))
            })
            .collect();
        vec![(
            "UserLocalConfigStore".to_string(),
            VdfValue::Object(vec![(
                "Software".to_string(),
                VdfValue::Object(vec![(
                    "Valve".to_string(),
                    VdfValue::Object(vec![(
                        "Steam".to_string(),
                        VdfValue::Object(vec![("apps".to_string(), VdfValue::Object(app_entries))]),
                    )]),
                )]),
            )]),
        )]
    }

    fn launch_options_for_app<'a>(root: &'a VdfObject, app_id: &str) -> Option<&'a str> {
        let apps = navigate_to_steam_apps(root)?;
        let (_, VdfValue::Object(fields)) = apps.iter().find(|(k, _)| k == app_id)? else {
            return None;
        };
        fields
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("launchoptions"))
            .and_then(|(_, v)| match v {
                VdfValue::Str(s) => Some(s.as_str()),
                _ => None,
            })
    }

    fn navigate_to_steam_apps(root: &VdfObject) -> Option<&VdfObject> {
        let path = ["UserLocalConfigStore", "Software", "Valve", "Steam", "apps"];
        let mut current = root;
        for (i, key) in path.iter().enumerate() {
            let (_, value) = current.iter().find(|(k, _)| k.eq_ignore_ascii_case(key))?;
            let VdfValue::Object(inner) = value else {
                return None;
            };
            if i == path.len() - 1 {
                return Some(inner);
            }
            current = inner;
        }
        unreachable!()
    }

    #[test]
    fn inject_official_launch_options_wraps_a_tracked_app_missing_the_marker() {
        let dir = test_dir("official-inject-missing");
        let path = dir.join("localconfig.vdf");
        fs::write(
            &path,
            text_vdf::write_text(&localconfig_tree(&[("233860", None)])),
        )
        .unwrap();

        let summary = inject_official_launch_options(&path, &[233860]).unwrap();
        assert_eq!(summary.inspected, 1);
        assert_eq!(summary.injected, 1);

        let rewritten = text_vdf::parse_text(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            launch_options_for_app(&rewritten, "233860"),
            Some("vault-sync run 233860 --steam --")
        );
    }

    #[test]
    fn inject_official_launch_options_preserves_existing_custom_flags() {
        let dir = test_dir("official-inject-wraps-existing");
        let path = dir.join("localconfig.vdf");
        fs::write(
            &path,
            text_vdf::write_text(&localconfig_tree(&[("233860", Some("-fullscreen"))])),
        )
        .unwrap();

        inject_official_launch_options(&path, &[233860]).unwrap();

        let rewritten = text_vdf::parse_text(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            launch_options_for_app(&rewritten, "233860"),
            Some("vault-sync run 233860 --steam -- -fullscreen")
        );
    }

    #[test]
    fn inject_official_launch_options_never_creates_an_untracked_app_block() {
        // Só injeta em blocos que já existem — nunca inventa um registro
        // pra um jogo que a Steam ainda não conhece.
        let dir = test_dir("official-inject-untracked");
        let path = dir.join("localconfig.vdf");
        let original = text_vdf::write_text(&localconfig_tree(&[("233860", None)]));
        fs::write(&path, &original).unwrap();

        // "730" não está no arquivo — pedir pra rastrear não deveria criar nada.
        let summary = inject_official_launch_options(&path, &[233860, 730]).unwrap();
        assert_eq!(
            summary.inspected, 1,
            "só 233860 existe de verdade no arquivo"
        );
        assert_eq!(summary.injected, 1);

        let rewritten = text_vdf::parse_text(&fs::read_to_string(&path).unwrap()).unwrap();
        assert!(
            navigate_to_steam_apps(&rewritten)
                .unwrap()
                .iter()
                .all(|(k, _)| k != "730"),
            "não deveria ter criado um bloco novo pro app_id 730"
        );
    }

    #[test]
    fn inject_official_launch_options_leaves_already_marked_entries_untouched_and_creates_no_backup(
    ) {
        let dir = test_dir("official-inject-already-marked");
        let path = dir.join("localconfig.vdf");
        fs::write(
            &path,
            text_vdf::write_text(&localconfig_tree(&[(
                "233860",
                Some("vault-sync run 233860 --steam --"),
            )])),
        )
        .unwrap();

        let summary = inject_official_launch_options(&path, &[233860]).unwrap();
        assert_eq!(summary.injected, 0);

        let has_backup = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .any(|e| e.file_name().to_string_lossy().contains("vault-sync-bak"));
        assert!(!has_backup, "não deveria gerar backup sem nenhuma mudança");
    }

    #[test]
    fn inject_all_official_launch_options_covers_every_userdata_profile() {
        let dir = test_dir("official-inject-all-profiles");
        let steam_root = dir.join("steam");
        for user in ["1", "2"] {
            let config_dir = steam_root.join("userdata").join(user).join("config");
            fs::create_dir_all(&config_dir).unwrap();
            fs::write(
                config_dir.join("localconfig.vdf"),
                text_vdf::write_text(&localconfig_tree(&[("233860", None)])),
            )
            .unwrap();
        }

        let results = inject_all_official_launch_options(&steam_root, &[233860]);
        assert_eq!(results.len(), 2);
        for (_, result) in results {
            assert_eq!(result.unwrap().injected, 1);
        }
    }

    fn launch_options_of_first_shortcut(root: &VdfObject) -> String {
        let VdfValue::Object(shortcuts) = &root[0].1 else {
            panic!("esperava objeto shortcuts")
        };
        let VdfValue::Object(fields) = &shortcuts[0].1 else {
            panic!("esperava objeto do atalho 0")
        };
        fields
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("launchoptions"))
            .and_then(|(_, v)| match v {
                VdfValue::Str(s) => Some(s.clone()),
                _ => None,
            })
            .expect("esperava LaunchOptions presente")
    }

    fn old_shortcut_tree(app_id: u32, name: &str, exe: &str) -> VdfObject {
        vec![(
            "shortcuts".to_string(),
            VdfValue::Object(vec![(
                "0".to_string(),
                VdfValue::Object(vec![
                    ("appid".to_string(), VdfValue::Int(app_id as i32)),
                    ("appname".to_string(), VdfValue::Str(name.to_string())),
                    ("Exe".to_string(), VdfValue::Str(format!("\"{exe}\""))),
                ]),
            )]),
        )]
    }

    fn app_id_of_entry(fields: &VdfObject) -> u32 {
        fields
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("appid"))
            .and_then(|(_, v)| match v {
                VdfValue::Int(i) => Some(*i as u32),
                _ => None,
            })
            .unwrap()
    }

    fn field_str(fields: &VdfObject, key: &str) -> String {
        fields
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .and_then(|(_, v)| match v {
                VdfValue::Str(s) => Some(s.clone()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("campo '{key}' ausente ou não é string"))
    }

    #[test]
    fn stage_pirata_shortcut_creates_entry_reusing_exe_filename_from_prior_shortcut() {
        // Regressão do caso real do Haste: atalho antigo aponta pro disco
        // que foi remanejado, mas o nome do arquivo (`Haste.exe`) continua
        // valendo contra a pasta nova consolidada.
        let dir = test_dir("stage-create");
        let steam_root = dir.join("steam");
        let userdata = steam_root.join("userdata").join("1").join("config");
        fs::create_dir_all(&userdata).unwrap();
        fs::write(
            userdata.join("shortcuts.vdf"),
            vdf::write_binary(&old_shortcut_tree(
                2385817016,
                "Haste",
                "/mnt/SSD2/Piratas/HASTE.v1.5.c/HASTE.v1.5.c/Haste.exe",
            )),
        )
        .unwrap();

        let local_dir = dir.join("Games").join("piratas").join("Haste");
        fs::create_dir_all(&local_dir).unwrap();
        fs::write(local_dir.join("Haste.exe"), b"fake exe").unwrap();

        let outcome = stage_pirata_shortcut(&steam_root, "Haste", &local_dir).unwrap();
        let expected_exe = local_dir.join("Haste.exe").to_string_lossy().into_owned();
        // AppID esperado calculado do mesmo jeito que a própria função
        // calcula (algoritmo já validado contra os dois valores reais em
        // `steam_appid.rs`) — o valor absoluto aqui depende do diretório
        // temporário do teste, não do caminho real de produção.
        let expected_app_id = crate::steam_appid::compute_app_id(&expected_exe, "Haste");
        match &outcome {
            StageShortcutOutcome::Created { app_id, exe } => {
                assert_eq!(*app_id, expected_app_id);
                assert_eq!(exe, &expected_exe);
            }
            other => panic!("esperava Created, obteve {other:?}"),
        }

        let rewritten =
            vdf::parse_binary(&fs::read(userdata.join("shortcuts.vdf")).unwrap()).unwrap();
        let VdfValue::Object(shortcuts) = &rewritten[0].1 else {
            panic!()
        };
        assert_eq!(shortcuts.len(), 2, "atalho antigo deve continuar presente");
        let VdfValue::Object(new_fields) = &shortcuts[1].1 else {
            panic!()
        };
        assert_eq!(app_id_of_entry(new_fields), expected_app_id);
        assert_eq!(field_str(new_fields, "Exe"), format!("\"{expected_exe}\""));
        assert_eq!(
            field_str(new_fields, "LaunchOptions"),
            format!("vault-sync run {expected_app_id} --steam --")
        );
    }

    #[test]
    fn stage_pirata_shortcut_is_no_prior_shortcut_found_when_name_is_unknown() {
        let dir = test_dir("stage-no-prior");
        let steam_root = dir.join("steam");
        fs::create_dir_all(steam_root.join("userdata").join("1").join("config")).unwrap();
        let local_dir = dir.join("Games").join("piratas").join("Jogo Novo");
        fs::create_dir_all(&local_dir).unwrap();

        let outcome = stage_pirata_shortcut(&steam_root, "Jogo Novo", &local_dir).unwrap();
        assert_eq!(outcome, StageShortcutOutcome::NoPriorShortcutFound);
    }

    #[test]
    fn stage_pirata_shortcut_errors_when_exe_file_missing_at_new_path() {
        let dir = test_dir("stage-missing-exe");
        let steam_root = dir.join("steam");
        let userdata = steam_root.join("userdata").join("1").join("config");
        fs::create_dir_all(&userdata).unwrap();
        fs::write(
            userdata.join("shortcuts.vdf"),
            vdf::write_binary(&old_shortcut_tree(1, "Jogo", "/velho/Jogo.exe")),
        )
        .unwrap();

        let local_dir = dir.join("Games").join("piratas").join("Jogo");
        fs::create_dir_all(&local_dir).unwrap();
        // Download nunca terminou / nome de arquivo diferente do esperado
        // — `Jogo.exe` não existe dentro de `local_dir`.

        let result = stage_pirata_shortcut(&steam_root, "Jogo", &local_dir);
        assert!(matches!(
            result,
            Err(StageShortcutError::ExeNotFoundAtNewPath(_, _))
        ));
    }

    #[test]
    fn stage_pirata_shortcut_is_idempotent_when_already_up_to_date() {
        let dir = test_dir("stage-idempotent");
        let steam_root = dir.join("steam");
        let userdata = steam_root.join("userdata").join("1").join("config");
        fs::create_dir_all(&userdata).unwrap();
        fs::write(
            userdata.join("shortcuts.vdf"),
            vdf::write_binary(&old_shortcut_tree(1, "Jogo", "/velho/Jogo.exe")),
        )
        .unwrap();

        let local_dir = dir.join("Games").join("piratas").join("Jogo");
        fs::create_dir_all(&local_dir).unwrap();
        fs::write(local_dir.join("Jogo.exe"), b"fake exe").unwrap();

        let first = stage_pirata_shortcut(&steam_root, "Jogo", &local_dir).unwrap();
        let app_id = match first {
            StageShortcutOutcome::Created { app_id, .. } => app_id,
            other => panic!("esperava Created na primeira chamada, obteve {other:?}"),
        };

        let second = stage_pirata_shortcut(&steam_root, "Jogo", &local_dir).unwrap();
        assert_eq!(second, StageShortcutOutcome::AlreadyUpToDate { app_id });

        let rewritten =
            vdf::parse_binary(&fs::read(userdata.join("shortcuts.vdf")).unwrap()).unwrap();
        let VdfValue::Object(shortcuts) = &rewritten[0].1 else {
            panic!()
        };
        assert_eq!(
            shortcuts.len(),
            2,
            "a segunda chamada não deveria adicionar outra entrada"
        );
    }

    #[test]
    fn stage_pirata_shortcut_creates_an_atomic_backup_before_writing() {
        let dir = test_dir("stage-backup");
        let steam_root = dir.join("steam");
        let userdata = steam_root.join("userdata").join("1").join("config");
        fs::create_dir_all(&userdata).unwrap();
        let original_bytes = vdf::write_binary(&old_shortcut_tree(1, "Jogo", "/velho/Jogo.exe"));
        fs::write(userdata.join("shortcuts.vdf"), &original_bytes).unwrap();

        let local_dir = dir.join("Games").join("piratas").join("Jogo");
        fs::create_dir_all(&local_dir).unwrap();
        fs::write(local_dir.join("Jogo.exe"), b"fake exe").unwrap();

        stage_pirata_shortcut(&steam_root, "Jogo", &local_dir).unwrap();

        let backup = fs::read_dir(&userdata)
            .unwrap()
            .flatten()
            .find(|e| e.file_name().to_string_lossy().contains("vault-sync-bak"))
            .expect("esperava um arquivo de backup");
        assert_eq!(fs::read(backup.path()).unwrap(), original_bytes);
    }
}
