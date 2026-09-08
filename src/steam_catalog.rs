//! Resolve nomes de AppIDs Steam pra exibição no `list` — best-effort, nunca
//! bloqueia nem falha o comando se a rede estiver indisponível (mesmo
//! espírito read-only/resiliente de `doctor.rs`/`list.rs`).
//!
//! IDs oficiais (< 2^31) são resolvidos via `store.steampowered.com/api/
//! appdetails`, um GET por jogo, sem chave de API — a rota antiga de
//! catálogo em lote (`ISteamApps/GetAppList`) foi descontinuada pela Valve
//! (confirmado contra `ISteamWebAPIUtil/GetSupportedAPIList`: o método não
//! existe mais na interface), então não há mais como buscar tudo de uma vez.
//!
//! IDs de atalho non-Steam ("piratas") não existem em nenhuma API da Steam —
//! o algoritmo que o cliente usa pra gerá-los (CRC32 do caminho+nome do
//! atalho, OR'd com 0x80000000) garante que caem sempre em [2^31, 2^32),
//! acima de qualquer AppID oficial real — é essa faixa que usamos pra
//! distinguir os dois casos sem precisar de nenhum metadado extra. Pra
//! esses, o nome vem do diretório final em
//! `install_path_overrides.piratas[app_id]` quando configurado, senão fica
//! desconhecido.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

const NON_STEAM_SHORTCUT_THRESHOLD: u64 = 0x8000_0000; // 2^31

/// `true` quando o AppID está na faixa de IDs oficiais do catálogo Steam.
pub fn is_official_app_id(app_id: &str) -> bool {
    app_id
        .parse::<u64>()
        .map(|id| id < NON_STEAM_SHORTCUT_THRESHOLD)
        .unwrap_or(false)
}

/// Busca o nome de um único AppID oficial via `appdetails`. `None` em
/// qualquer falha de rede/parse ou quando a Steam não conhece o ID (jogo
/// removido do catálogo, região bloqueada etc.) — resolução de nome é
/// best-effort, nunca deve derrubar o `list`.
pub fn fetch_official_name(app_id: u64) -> Option<String> {
    let url =
        format!("https://store.steampowered.com/api/appdetails?appids={app_id}&filters=basic");
    let body = ureq::get(&url)
        .timeout(Duration::from_secs(5))
        .call()
        .ok()?
        .into_string()
        .ok()?;
    parse_appdetails(&body, app_id)
}

fn parse_appdetails(body: &str, app_id: u64) -> Option<String> {
    let parsed: serde_json::Value = serde_json::from_str(body).ok()?;
    let entry = parsed.get(app_id.to_string())?;
    if !entry.get("success")?.as_bool()? {
        return None;
    }
    let name = entry.get("data")?.get("name")?.as_str()?;
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

/// Resolve o nome de exibição de um AppID: `fetch_official` (injetável pra
/// teste — em produção sempre `fetch_official_name`) se for um ID oficial;
/// senão, pro caso "pirata", o nome do diretório final em
/// `install_path_overrides.piratas[app_id]`, se configurado. `None` quando
/// nenhuma fonte souber o nome.
pub fn resolve_name(
    app_id: &str,
    pirata_overrides: &HashMap<String, PathBuf>,
    fetch_official: impl FnOnce(u64) -> Option<String>,
) -> Option<String> {
    if is_official_app_id(app_id) {
        let id: u64 = app_id.parse().ok()?;
        fetch_official(id)
    } else {
        pirata_overrides
            .get(app_id)
            .and_then(|path| path.file_name())
            .map(|s| s.to_string_lossy().into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn official_ids_are_below_shortcut_threshold() {
        assert!(is_official_app_id("730"));
        assert!(is_official_app_id("4148250"));
        assert!(!is_official_app_id("2385817016"));
        assert!(!is_official_app_id("4192946371"));
    }

    #[test]
    fn non_numeric_id_is_not_official() {
        assert!(!is_official_app_id("not-a-number"));
    }

    #[test]
    fn parses_appdetails_success_response() {
        let body = r#"{"730":{"success":true,"data":{"name":"Counter-Strike 2"}}}"#;
        assert_eq!(
            parse_appdetails(body, 730),
            Some("Counter-Strike 2".to_string())
        );
    }

    #[test]
    fn appdetails_success_false_resolves_to_none() {
        let body = r#"{"123":{"success":false}}"#;
        assert_eq!(parse_appdetails(body, 123), None);
    }

    #[test]
    fn malformed_appdetails_body_resolves_to_none() {
        assert_eq!(parse_appdetails("not json", 730), None);
        assert_eq!(parse_appdetails("{}", 730), None);
    }

    #[test]
    fn resolves_official_name_via_injected_fetcher() {
        let overrides = HashMap::new();
        let name = resolve_name("730", &overrides, |id| {
            assert_eq!(id, 730);
            Some("Counter-Strike 2".to_string())
        });
        assert_eq!(name, Some("Counter-Strike 2".to_string()));
    }

    #[test]
    fn official_lookup_failure_resolves_to_none() {
        let overrides = HashMap::new();
        assert_eq!(resolve_name("730", &overrides, |_| None), None);
    }

    #[test]
    fn resolves_pirata_name_from_override_path_without_touching_network() {
        let mut overrides = HashMap::new();
        overrides.insert(
            "2385817016".to_string(),
            PathBuf::from("/mnt/SSD2/Piratas/HASTE.v1.5.c/HASTE.v1.5.c"),
        );
        let name = resolve_name("2385817016", &overrides, |_| {
            panic!("pirata IDs não devem chamar o fetcher oficial")
        });
        assert_eq!(name, Some("HASTE.v1.5.c".to_string()));
    }

    #[test]
    fn pirata_id_without_override_resolves_to_none() {
        let overrides = HashMap::new();
        let name = resolve_name("4192946371", &overrides, |_| {
            panic!("pirata IDs não devem chamar o fetcher oficial")
        });
        assert_eq!(name, None);
    }
}
