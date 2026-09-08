use regex::Regex;
use std::io::Read;
use std::sync::mpsc::{self, Receiver};
use std::sync::OnceLock;
use std::thread;

/// Um ponto de progresso extraído de uma linha `--info=progress2` do rsync,
/// ex.: `"     1,234,567  43%   12.34MB/s    0:00:05 (xfr#1, to-check=10/20)"`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgressUpdate {
    pub percent: u8,
    pub rate: String,
}

fn progress_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // `[0-9.,]` (não só `.`) porque o rsync formata a taxa de acordo
        // com `LC_NUMERIC` do processo, não em formato fixo — nesta máquina
        // (pt_BR.UTF-8) sai "1,62GB/s", vírgula decimal. Sem isso, o regex
        // nunca batia com NENHUMA linha de progresso real neste locale (só
        // nos testes, que usam ponto) — barra sempre ficava sem percentual
        // (achado real: transferência de 71GB do Cyberpunk 2077 mostrando
        // só "Executando..." o tempo todo).
        Regex::new(r"(?P<percent>\d{1,3})%\s+(?P<rate>[0-9.,]+[KMGkmg]?B/s)")
            .expect("padrão de progresso do rsync é um regex estático válido")
    })
}

/// Extrai percentual e taxa de transferência de uma linha de saída do
/// rsync. Retorna `None` para qualquer linha que não seja uma atualização
/// de progresso (ex.: "receiving file list...", avisos, linhas em branco).
pub fn parse_progress_line(line: &str) -> Option<ProgressUpdate> {
    let caps = progress_regex().captures(line)?;
    let percent: u8 = caps["percent"].parse().ok()?;
    Some(ProgressUpdate {
        percent,
        rate: caps["rate"].to_string(),
    })
}

/// Lê a saída do rsync numa thread dedicada, para que a captura de
/// progresso nunca bloqueie a thread que orquestra o processo principal
/// (aquisição do próximo `child.wait()` ou o encaminhamento ao utilitário de
/// exibição). rsync reescreve a linha de progresso via `\r` quando o stdout
/// não é um TTY, então a leitura é feita byte a byte, quebrando tanto em
/// `\r` quanto em `\n`.
pub fn spawn_progress_reader<R: Read + Send + 'static>(reader: R) -> Receiver<ProgressUpdate> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut reader = reader;
        let mut line = String::new();
        let mut byte = [0u8; 1];
        loop {
            match reader.read(&mut byte) {
                Ok(0) => break,
                Ok(_) => {
                    let c = byte[0] as char;
                    if c == '\n' || c == '\r' {
                        if !line.is_empty() {
                            if let Some(update) = parse_progress_line(&line) {
                                if tx.send(update).is_err() {
                                    return;
                                }
                            }
                            line.clear();
                        }
                    } else {
                        line.push(c);
                    }
                }
                Err(_) => break,
            }
        }
    });
    rx
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn parses_typical_progress2_line() {
        let line = "     1,234,567  43%   12.34MB/s    0:00:05 (xfr#1, to-check=10/20)";
        assert_eq!(
            parse_progress_line(line),
            Some(ProgressUpdate {
                percent: 43,
                rate: "12.34MB/s".to_string()
            })
        );
    }

    #[test]
    fn parses_line_with_comma_decimal_rate_from_pt_br_locale() {
        // Formato real observado nesta máquina (LC_NUMERIC=pt_BR.UTF-8):
        // rsync usa vírgula decimal na taxa, não ponto.
        let line = "     5.242.880   2%    1,62GB/s    0:00:00 (xfr#1, to-chk=49/51)";
        assert_eq!(
            parse_progress_line(line),
            Some(ProgressUpdate {
                percent: 2,
                rate: "1,62GB/s".to_string()
            })
        );
    }

    #[test]
    fn parses_line_with_kb_rate() {
        let line = "        512  7%  345.20kB/s    0:00:01";
        assert_eq!(
            parse_progress_line(line),
            Some(ProgressUpdate {
                percent: 7,
                rate: "345.20kB/s".to_string()
            })
        );
    }

    #[test]
    fn non_progress_lines_return_none() {
        for line in [
            "receiving file list ... done",
            "sending incremental file list",
            "",
            "total size is 1,234,567  speedup is 2.00",
        ] {
            assert_eq!(parse_progress_line(line), None, "linha: {line:?}");
        }
    }

    #[test]
    fn spawn_progress_reader_parses_carriage_return_delimited_updates() {
        let data = b"     100  10%   1.00MB/s    0:00:09\r     500  50%   2.50MB/s    0:00:05\r     999  99%   3.00MB/s    0:00:01\n";
        let rx = spawn_progress_reader(Cursor::new(data.to_vec()));

        let updates: Vec<ProgressUpdate> = rx.iter().collect();

        assert_eq!(
            updates,
            vec![
                ProgressUpdate {
                    percent: 10,
                    rate: "1.00MB/s".to_string()
                },
                ProgressUpdate {
                    percent: 50,
                    rate: "2.50MB/s".to_string()
                },
                ProgressUpdate {
                    percent: 99,
                    rate: "3.00MB/s".to_string()
                },
            ]
        );
    }

    #[test]
    fn spawn_progress_reader_ignores_non_progress_noise() {
        let data = b"receiving file list ... done\n     100  20%   1.00MB/s    0:00:09\n";
        let rx = spawn_progress_reader(Cursor::new(data.to_vec()));

        let updates: Vec<ProgressUpdate> = rx.iter().collect();

        assert_eq!(
            updates,
            vec![ProgressUpdate {
                percent: 20,
                rate: "1.00MB/s".to_string()
            }]
        );
    }
}
