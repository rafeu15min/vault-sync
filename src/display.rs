use crate::progress::ProgressUpdate;
use std::io::Write;
use std::process::{Child, Command, Stdio};

/// Abstrai o backend de exibição gráfica para que o resto do código nunca
/// precise saber qual utilitário nativo (zenity, nenhum) está desenhando a
/// barra de progresso — apenas empurra `ProgressUpdate`s ou rótulos de
/// texto. `zenity` é GTK: já segue o tema (cores, modo claro/escuro) que
/// estiver configurado em `~/.config/gtk-3.0/settings.ini` sozinho, sem
/// nenhum código extra deste projeto.
pub trait ProgressSink {
    fn update(&mut self, update: &ProgressUpdate);
    /// Atualiza só o rótulo de texto, sem percentual — usado por comandos
    /// em lote (`ingest`) ou pulsantes (`purge`), onde não há uma única
    /// transferência contínua de 0 a 100 pra acompanhar.
    fn label(&mut self, text: &str);
    fn finish(&mut self);
}

/// Backend nulo: usado em sessões headless (sem zenity instalado, sem
/// sessão gráfica, ou `--background`) e em testes. A ausência de UI nunca
/// deve interromper a transferência.
pub struct NullSink;

impl ProgressSink for NullSink {
    fn update(&mut self, _update: &ProgressUpdate) {}
    fn label(&mut self, _text: &str) {}
    fn finish(&mut self) {}
}

/// Backend zenity: `zenity --progress` lê linha a linha do próprio stdin —
/// um número puro atualiza a barra, uma linha prefixada com `#` atualiza a
/// legenda. Deliberadamente SEM `--auto-close`: cada transferência
/// individual (compatdata, depois a instalação completa, depois o upload)
/// chega a 100% várias vezes ao longo de uma única sessão de `run` — com
/// `--auto-close`, a janela fechava sozinha na primeira vez que qualquer
/// uma delas terminasse, bem antes do jogo sequer abrir. Em vez disso, o
/// ciclo de vida da janela é todo controlado por quem chama: fica aberta
/// até `finish()` fechar o stdin do processo filho.
pub struct ZenitySink {
    child: Child,
}

impl ZenitySink {
    fn spawn_with_args(title: &str, extra_args: &[&str]) -> std::io::Result<Self> {
        let child = Command::new("zenity")
            .args([
                "--progress",
                "--title",
                title,
                "--percentage=0",
                "--no-cancel",
            ])
            .args(extra_args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        Ok(Self { child })
    }

    /// Barra determinada (0-100%) — pra transferências com percentual real
    /// conhecido (`run`).
    pub fn spawn(title: &str) -> std::io::Result<Self> {
        Self::spawn_with_args(title, &[])
    }

    /// Barra pulsante (sem percentual) — pra operações em lote onde só o
    /// rótulo de texto (qual item está sendo processado agora) importa,
    /// não uma fração conhecida do total (`ingest`, `purge`).
    pub fn spawn_pulsating(title: &str) -> std::io::Result<Self> {
        Self::spawn_with_args(title, &["--pulsate"])
    }

    pub fn is_available() -> bool {
        // Sem sessão gráfica (ex.: unit systemd de boot/poweroff, SSH sem
        // encaminhamento de display), tentar abrir o zenity travaria ou
        // falharia de forma menos previsível do que simplesmente checar
        // isso antes — mesmo raciocínio de `ensure_reachable` nunca supor
        // rede disponível sem checar.
        let has_display =
            std::env::var_os("WAYLAND_DISPLAY").is_some() || std::env::var_os("DISPLAY").is_some();
        has_display && which("zenity")
    }
}

impl ProgressSink for ZenitySink {
    fn update(&mut self, update: &ProgressUpdate) {
        if let Some(stdin) = self.child.stdin.as_mut() {
            let _ = writeln!(stdin, "{}", update.percent);
            let _ = writeln!(stdin, "# {}% — {}", update.percent, update.rate);
        }
    }

    fn label(&mut self, text: &str) {
        if let Some(stdin) = self.child.stdin.as_mut() {
            let _ = writeln!(stdin, "# {text}");
        }
    }

    fn finish(&mut self) {
        // Encerra o processo diretamente em vez de escrever "100" e
        // esperar o zenity sair sozinho: sem `--auto-close`, fechar o
        // stdin sem matar o processo deixa a janela parada numa tela de
        // "concluído" esperando o usuário clicar OK pra fechar — exatamente
        // o problema reportado (primeira tela pedindo confirmação, próxima
        // janela não abre até isso ser fechado). Matar o processo garante
        // que a janela some na hora, sem interação nenhuma, e a próxima
        // janela (ex.: o resumo em `notify_result`) já pode abrir em
        // seguida sem nada bloqueando.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn which_in(bin: &str, path_var: &str) -> bool {
    std::env::split_paths(path_var).any(|dir| dir.join(bin).is_file())
}

fn which(bin: &str) -> bool {
    std::env::var("PATH")
        .map(|path_var| which_in(bin, &path_var))
        .unwrap_or(false)
}

/// Escolhe o melhor backend de progresso determinado (0-100%) disponível —
/// usado só por `run`, a única transferência com fração real conhecida ao
/// longo de uma sessão inteira.
pub fn pick_sink(title: &str) -> Box<dyn ProgressSink> {
    if ZenitySink::is_available() {
        match ZenitySink::spawn(title) {
            Ok(sink) => return Box::new(sink),
            Err(err) => tracing::warn!("falha ao iniciar zenity, seguindo sem UI: {err}"),
        }
    }
    Box::new(NullSink)
}

/// Idem, mas pulsante — usado por comandos em lote (`ingest`, `purge`) que
/// não têm uma fração 0-100 única pra reportar, só uma sequência de itens.
pub fn pick_pulsating_sink(title: &str) -> Box<dyn ProgressSink> {
    if ZenitySink::is_available() {
        match ZenitySink::spawn_pulsating(title) {
            Ok(sink) => return Box::new(sink),
            Err(err) => tracing::warn!("falha ao iniciar zenity, seguindo sem UI: {err}"),
        }
    }
    Box::new(NullSink)
}

/// Notificação de resultado pós-comando (`doctor`, `scan`, resumo de
/// `ingest`/`purge`) — `zenity --info`/`--error`, melhor esforço: nunca
/// bloqueia nem falha o comando se não houver sessão gráfica ou o zenity
/// não estiver instalado, e roda em segundo plano sem travar o processo
/// principal (o usuário fecha quando quiser, o `vault-sync` já terminou).
pub fn notify_result(title: &str, text: &str, is_error: bool) {
    if !ZenitySink::is_available() {
        return;
    }
    // Sem `--no-wrap`: um resumo curto (`doctor`/`purge`/`update`) não muda
    // de aparência (zenity só quebra linha quando precisa), mas um relatório
    // longo (`ingest` com vários itens pulados) deixa de esticar a janela
    // pela tela inteira numa única linha horizontal gigante.
    let flag = if is_error { "--error" } else { "--info" };
    let _ = Command::new("zenity")
        .args([flag, "--title", title, "--text", text])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

/// Lista estruturada e rolável (`list`) via `zenity --list` — melhor
/// esforço, roda em segundo plano, nunca bloqueia nem falha o comando.
pub fn show_list(title: &str, columns: &[&str], rows: &[Vec<String>]) {
    if !ZenitySink::is_available() || rows.is_empty() {
        return;
    }
    let mut args: Vec<String> = vec![
        "--list".to_string(),
        "--title".to_string(),
        title.to_string(),
    ];
    for column in columns {
        args.push("--column".to_string());
        args.push((*column).to_string());
    }
    for row in rows {
        args.extend(row.iter().cloned());
    }
    let _ = Command::new("zenity")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

/// Confirmação de ação irrevogável (`purge`) via `zenity --question`.
/// `None` quando não há sessão gráfica disponível — quem chama deve cair
/// pro prompt de terminal nesse caso, nunca assumir uma resposta.
pub fn confirm(title: &str, question: &str) -> Option<bool> {
    if !ZenitySink::is_available() {
        return None;
    }
    let status = Command::new("zenity")
        .args([
            "--question",
            "--title",
            title,
            "--text",
            question,
            "--no-wrap",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()?;
    Some(status.success())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn which_in_finds_executable_in_given_path_list() {
        let dir =
            std::env::temp_dir().join(format!("vault-sync-which-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("fake-zenity");
        fs::write(&bin, "#!/bin/sh\n").unwrap();

        assert!(which_in("fake-zenity", dir.to_str().unwrap()));
        assert!(!which_in(
            "nao-existe-de-jeito-nenhum",
            dir.to_str().unwrap()
        ));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn null_sink_never_panics() {
        let mut sink = NullSink;
        sink.update(&ProgressUpdate {
            percent: 50,
            rate: "1MB/s".into(),
        });
        sink.label("qualquer coisa");
        sink.finish();
    }
}
