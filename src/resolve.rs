//! Resolve o "jogo" que o usuário digitou num comando da CLI: AppID Steam
//! (como sempre), título Heroic com `--heroic`, ou parte do nome (busca
//! fuzzy no catálogo unificado) — com desambiguação/confirmação interativa.

use crate::catalog::{Catalog, CatalogEntry};
use crate::domain::Launcher;
use std::io::{BufRead, Write};

/// Jogo já decidido: o par que os motores (`run`/`pull`/`purge`...) recebem
/// e um rótulo legível pra janelas/notificações.
#[derive(Debug, PartialEq, Eq)]
pub struct Resolved {
    pub launcher: Launcher,
    pub id: String,
    pub label: String,
    /// Como o alvo aparece em perguntas: `'texto'` quando exato, nome + tipo
    /// + identificador real quando veio de busca por nome.
    pub description: String,
}

/// Como o comando quer tratar o catálogo e a confirmação.
pub struct Query<'a> {
    pub text: &'a str,
    /// `--steam`/`--heroic` explícito: o texto é o identificador exato.
    pub forced: Option<Launcher>,
    /// Quais entradas do catálogo servem a este comando.
    pub accepts: fn(&CatalogEntry) -> bool,
    /// Pergunta "É este?" quando só uma entrada casa (desligado quando o
    /// comando já vai fazer a própria confirmação do jogo, ex.: `purge`).
    pub confirm_single: bool,
}

fn is_steam_app_id(text: &str) -> bool {
    !text.is_empty() && text.chars().all(|c| c.is_ascii_digit())
}

/// Ponto de entrada: flag explícita ou AppID numérico passam direto (sem
/// tocar no catálogo, que só é carregado por `load_catalog` quando há busca
/// por nome a fazer); o resto vira busca fuzzy interativa no terminal.
pub fn resolve_game(
    query: &Query,
    load_catalog: impl FnOnce() -> anyhow::Result<Catalog>,
) -> anyhow::Result<Resolved> {
    let exact = |launcher| Resolved {
        launcher,
        id: query.text.to_string(),
        label: query.text.to_string(),
        description: format!("'{}'", query.text),
    };
    if let Some(launcher) = query.forced {
        return Ok(exact(launcher));
    }
    if is_steam_app_id(query.text) {
        return Ok(exact(Launcher::Steam));
    }

    let catalog = load_catalog()?;
    let candidates: Vec<&CatalogEntry> = catalog
        .search(query.text)
        .into_iter()
        .filter(|e| (query.accepts)(e))
        .collect();
    let entry = choose(
        &candidates,
        query,
        &mut std::io::stdin().lock(),
        &mut std::io::stderr(),
    )?;
    let (launcher, id) = entry.purge_target();
    Ok(Resolved {
        launcher,
        id,
        label: entry.display_name.clone(),
        description: entry.description(),
    })
}

/// Zero candidatos: erro. Um: confirmação (se pedida). Vários: lista
/// numerada e pergunta qual. Entrada vazia/EOF/inválida cancela — nunca
/// assume uma resposta quando não há quem responda (ex.: sem terminal).
fn choose<'a>(
    candidates: &[&'a CatalogEntry],
    query: &Query,
    input: &mut impl BufRead,
    out: &mut impl Write,
) -> anyhow::Result<&'a CatalogEntry> {
    let text = query.text;
    match candidates {
        [] => anyhow::bail!(
            "nenhum jogo do catálogo corresponde a '{text}' (veja os nomes em 'vault-sync catalog')"
        ),
        [only] if !query.confirm_single => Ok(*only),
        [only] => {
            write!(
                out,
                "'{text}' corresponde a {}. É este? [s/N]: ",
                only.description()
            )?;
            if !is_yes(&read_answer(input, out)?) {
                anyhow::bail!("cancelado — jogo não confirmado");
            }
            Ok(*only)
        }
        many => {
            writeln!(out, "'{text}' corresponde a {} jogos:", many.len())?;
            for (i, entry) in many.iter().enumerate() {
                writeln!(out, "  {}) {}", i + 1, entry.description())?;
            }
            write!(out, "Qual deles? [1-{}, Enter cancela]: ", many.len())?;
            let answer = read_answer(input, out)?;
            let picked = answer.parse::<usize>().ok().and_then(|n| n.checked_sub(1));
            let Some(entry) = picked.and_then(|i| many.get(i)) else {
                anyhow::bail!("cancelado — nenhum jogo escolhido");
            };
            Ok(*entry)
        }
    }
}

fn read_answer(input: &mut impl BufRead, out: &mut impl Write) -> anyhow::Result<String> {
    out.flush()?;
    let mut line = String::new();
    input.read_line(&mut line)?;
    Ok(line.trim().to_lowercase())
}

fn is_yes(answer: &str) -> bool {
    matches!(answer, "s" | "sim" | "y" | "yes")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::GameLocation;
    use crate::domain::GameSource;

    fn entry(app_id: u32, name: &str) -> CatalogEntry {
        CatalogEntry {
            source: GameSource::Official { app_id },
            display_name: name.to_string(),
            identity_known: true,
            location: GameLocation::Both,
        }
    }

    fn query(text: &str, confirm_single: bool) -> Query<'_> {
        Query {
            text,
            forced: None,
            accepts: |_| true,
            confirm_single,
        }
    }

    fn run_choose<'a>(
        candidates: &[&'a CatalogEntry],
        q: &Query,
        typed: &str,
    ) -> (anyhow::Result<&'a CatalogEntry>, String) {
        let mut out = Vec::new();
        let result = choose(candidates, q, &mut typed.as_bytes(), &mut out);
        (result, String::from_utf8(out).unwrap())
    }

    #[test]
    fn no_candidates_is_an_error() {
        let (result, _) = run_choose(&[], &query("x", true), "");
        assert!(result.unwrap_err().to_string().contains("nenhum jogo"));
    }

    #[test]
    fn single_candidate_asks_confirmation_and_obeys_the_answer() {
        let game = entry(620, "Portal 2");
        let q = query("portal", true);
        let (yes, prompt) = run_choose(&[&game], &q, "s\n");
        assert_eq!(yes.unwrap().display_name, "Portal 2");
        assert!(prompt.contains("É este?") && prompt.contains("AppID 620"));
        for no in ["\n", "n\n", ""] {
            assert!(run_choose(&[&game], &q, no).0.is_err());
        }
    }

    #[test]
    fn single_candidate_skips_the_question_when_the_command_confirms_itself() {
        let game = entry(620, "Portal 2");
        let (result, prompt) = run_choose(&[&game], &query("portal", false), "");
        assert!(result.is_ok());
        assert!(prompt.is_empty());
    }

    #[test]
    fn many_candidates_are_listed_and_the_chosen_number_wins() {
        let (a, b) = (entry(620, "Portal 2"), entry(400, "Portal"));
        let q = query("portal", true);
        let (result, prompt) = run_choose(&[&a, &b], &q, "2\n");
        assert_eq!(result.unwrap().display_name, "Portal");
        assert!(prompt.contains("1) 'Portal 2'") && prompt.contains("2) 'Portal'"));
        for bad in ["\n", "0\n", "3\n", "abc\n", ""] {
            assert!(run_choose(&[&a, &b], &q, bad).0.is_err());
        }
    }

    #[test]
    fn explicit_launcher_or_numeric_id_never_loads_the_catalog() {
        let never = || -> anyhow::Result<Catalog> { panic!("catálogo não deveria carregar") };
        let forced = Query {
            forced: Some(Launcher::Heroic),
            ..query("Hogwarts Legacy", true)
        };
        let resolved = resolve_game(&forced, never).unwrap();
        assert_eq!(resolved.launcher, Launcher::Heroic);
        assert_eq!(resolved.id, "Hogwarts Legacy");

        let resolved = resolve_game(&query("730", true), never).unwrap();
        assert_eq!(
            (resolved.launcher, resolved.id.as_str()),
            (Launcher::Steam, "730")
        );
    }
}
