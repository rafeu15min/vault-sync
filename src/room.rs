//! Pedido de espaço ao Anubis antes de enviar um jogo novo ao servidor: o
//! Anubis apaga mídia (já vista primeiro, depois a mais antiga) até o disco
//! ficar com no máximo 85 % usado depois do jogo. Jogo nunca é apagado.

use crate::config::Config;
use serde::Deserialize;
use std::path::Path;
use std::time::Duration;

/// A poda apaga arquivos e avisa Jellyfin/Sonarr/Radarr/qBittorrent: demora.
const PRUNE_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Debug, thiserror::Error)]
pub enum RoomError {
    #[error("o Anubis recusou o pedido (HTTP {status}): {message}")]
    Refused { status: u16, message: String },
    #[error("falha ao falar com o Anubis em {url}: {reason}")]
    Unreachable { url: String, reason: String },
    #[error("resposta do Anubis ilegível: {0}")]
    BadResponse(String),
    #[error(
        "mesmo apagando tudo o que o Anubis pode apagar ({freed_mb} MB liberados), o jogo não cabe no servidor"
    )]
    NotEnough { freed_mb: u64 },
}

/// Só o que interessa da resposta de `POST /storage/prune`.
#[derive(Debug, Deserialize)]
struct PruneReport {
    plan: PrunePlan,
    #[serde(default)]
    done: Vec<String>,
    #[serde(default)]
    errors: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct PrunePlan {
    short: bool,
    freed: u64,
}

/// Soma dos tamanhos dos arquivos sob `root` (links simbólicos não são
/// seguidos). O que não puder ser lido conta zero.
pub fn dir_size(root: &Path) -> u64 {
    let mut total = 0;
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.is_file() {
            total += meta.len();
            continue;
        }
        if meta.is_dir() {
            pending.extend(
                std::fs::read_dir(&path)
                    .into_iter()
                    .flatten()
                    .flatten()
                    .map(|e| e.path()),
            );
        }
    }
    total
}

/// Pede ao Anubis espaço pra `bytes` novos. No-op sem `anubis_url`. Erro
/// quando o Anubis não responde, recusa (inclusive `dry_run` ligado) ou não
/// consegue liberar o bastante — quem chama não deve enviar o jogo nesse caso.
pub fn ensure_room(cfg: &Config, bytes: u64) -> Result<(), RoomError> {
    let Some(base) = cfg.anubis_url.as_deref() else {
        return Ok(());
    };
    if bytes == 0 {
        return Ok(());
    }
    let report = request_prune(base, cfg.anubis_token.as_deref(), bytes)?;
    for error in &report.errors {
        tracing::warn!("Anubis ao abrir espaço: {error}");
    }
    tracing::info!(
        bytes,
        apagados = report.done.len(),
        "Anubis liberou espaço no servidor"
    );
    if report.plan.short {
        return Err(RoomError::NotEnough {
            freed_mb: report.plan.freed >> 20,
        });
    }
    Ok(())
}

fn request_prune(base: &str, token: Option<&str>, bytes: u64) -> Result<PruneReport, RoomError> {
    let url = format!("{}/storage/prune", base.trim_end_matches('/'));
    let mut request = ureq::post(&url)
        .timeout(PRUNE_TIMEOUT)
        .set("Content-Type", "application/json");
    if let Some(token) = token {
        request = request.set("Authorization", &format!("Bearer {token}"));
    }
    let response =
        match request.send_string(&serde_json::json!({ "bytes_needed": bytes }).to_string()) {
            Ok(response) => response,
            Err(ureq::Error::Status(status, response)) => {
                let message = response.into_string().unwrap_or_default();
                return Err(RoomError::Refused { status, message });
            }
            Err(other) => {
                return Err(RoomError::Unreachable {
                    url,
                    reason: other.to_string(),
                })
            }
        };
    let body = response
        .into_string()
        .map_err(|e| RoomError::BadResponse(e.to_string()))?;
    serde_json::from_str(&body).map_err(|e| RoomError::BadResponse(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// Servidor HTTP de uma resposta só; devolve a URL base e o que recebeu.
    fn serve_once(status: &str, body: &str) -> (String, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let reply = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            // cabeçalho e corpo podem chegar em pacotes separados: lê até o fecha-chaves do JSON
            let mut received = Vec::new();
            let mut buf = [0u8; 4096];
            while !received.ends_with(b"}") {
                let n = stream.read(&mut buf).unwrap();
                received.extend_from_slice(&buf[..n]);
            }
            stream.write_all(reply.as_bytes()).unwrap();
            String::from_utf8_lossy(&received).into_owned()
        });
        (base, handle)
    }

    fn cfg_with(url: Option<String>) -> Config {
        Config {
            anubis_url: url,
            anubis_token: Some("segredo".to_string()),
            ..Config::test_default()
        }
    }

    #[test]
    fn without_anubis_url_it_does_nothing() {
        assert!(ensure_room(&cfg_with(None), 1 << 30).is_ok());
    }

    #[test]
    fn sends_bytes_and_bearer_token_and_accepts_a_sufficient_plan() {
        let (base, seen) = serve_once(
            "200 OK",
            r#"{"plan":{"short":false,"freed":3221225472},"done":["filme"],"errors":[]}"#,
        );
        ensure_room(&cfg_with(Some(base)), 5_000).unwrap();
        let request = seen.join().unwrap();
        assert!(request.starts_with("POST /storage/prune"));
        assert!(request
            .to_lowercase()
            .contains("authorization: bearer segredo"));
        assert!(request.contains(r#""bytes_needed":5000"#));
    }

    #[test]
    fn a_short_plan_means_the_game_does_not_fit() {
        let (base, _) = serve_once(
            "200 OK",
            r#"{"plan":{"short":true,"freed":1048576},"done":[],"errors":[]}"#,
        );
        let err = ensure_room(&cfg_with(Some(base)), 5_000).unwrap_err();
        assert!(matches!(err, RoomError::NotEnough { freed_mb: 1 }));
    }

    #[test]
    fn dry_run_refusal_is_reported_with_its_message() {
        let (base, _) = serve_once("409 Conflict", r#"{"error":"storage.dry_run = true"}"#);
        let err = ensure_room(&cfg_with(Some(base)), 5_000).unwrap_err();
        assert!(
            matches!(err, RoomError::Refused { status: 409, ref message } if message.contains("dry_run"))
        );
    }

    #[test]
    fn unreachable_anubis_is_an_error_not_a_silent_skip() {
        let err =
            ensure_room(&cfg_with(Some("http://127.0.0.1:1".to_string())), 5_000).unwrap_err();
        assert!(matches!(err, RoomError::Unreachable { .. }));
    }

    #[test]
    fn dir_size_sums_nested_files_and_ignores_symlinks() {
        let dir = crate::testutil::test_dir("room", "dir-size");
        std::fs::create_dir_all(dir.join("a/b")).unwrap();
        std::fs::write(dir.join("x.bin"), [0u8; 100]).unwrap();
        std::fs::write(dir.join("a/b/y.bin"), [0u8; 50]).unwrap();
        std::os::unix::fs::symlink(dir.join("x.bin"), dir.join("a/link")).unwrap();
        assert_eq!(dir_size(&dir), 150);
    }
}
