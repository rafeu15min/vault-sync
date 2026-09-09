//! Progresso de uma sessão de jogo (`run`/`menu-launch`) exibido via
//! `display::ProgressSink` — rótulos por estágio (Download/Running/
//! Upload), uma única barra 0-100% pra sessão inteira (não por
//! transferência), e o toggle de janela escondida durante `Running` pra
//! não flutuar sobre o jogo. Extraído de `dispatch_run`/`MenuLaunch`
//! (CLEANUP.md, Camada 3), que reimplementavam o mesmo closure
//! palavra-por-palavra, sem nenhum teste sobre a fórmula de escala —
//! apesar de ter lógica real (uma fórmula errada erraria silenciosamente
//! o progresso mostrado).

use crate::display::{self, ProgressSink};
use crate::progress::ProgressUpdate;
use crate::session::Stage;

/// Mapeia o percentual bruto de UMA transferência (0-100, sempre relativo
/// só a Download ou só a Upload) pro percentual da barra da sessão
/// inteira: primeira metade é Download, segunda metade é Upload, Running
/// fica travado no meio (sem percentual real nessa fase). Sem isto, uma
/// única barra "reiniciaria" do zero a cada estágio — pareceria um erro
/// visual em vez do progresso real da sessão.
fn scaled_percent(stage: Stage, raw_percent: u8) -> u8 {
    match stage {
        Stage::Download => raw_percent / 2,
        Stage::Running => 50,
        Stage::Upload => 50 + raw_percent / 2,
    }
}

/// Estado necessário pra reportar o progresso de uma sessão via zenity.
/// `title` é reaproveitado quando a janela precisa nascer de novo — depois
/// de escondida durante `Running` (`finish()` mata o processo, então
/// "mostrar de novo" é sempre uma janela nova, nunca a mesma
/// reaproveitada). `make_sink` injetado (em vez de sempre `display::
/// pick_sink`) pra permitir testar as transições de estado sem depender
/// de detecção de zenity de verdade — mesmo princípio de `rsync_bin`/
/// `ssh_bin` injetáveis em `session.rs`.
pub struct SessionProgress<'a> {
    sink: Box<dyn ProgressSink>,
    make_sink: fn(&str) -> Box<dyn ProgressSink>,
    title: &'a str,
    server_name: &'a str,
    current_stage: Option<Stage>,
    hidden: bool,
}

impl<'a> SessionProgress<'a> {
    pub fn new(title: &'a str, server_name: &'a str) -> Self {
        Self::with_sink_factory(title, server_name, display::pick_sink)
    }

    fn with_sink_factory(
        title: &'a str,
        server_name: &'a str,
        make_sink: fn(&str) -> Box<dyn ProgressSink>,
    ) -> Self {
        Self {
            sink: make_sink(title),
            make_sink,
            title,
            server_name,
            current_stage: None,
            hidden: false,
        }
    }

    /// Callback pra passar direto como `on_progress` de
    /// `session::run_session`/`run_via_steam_applaunch`.
    pub fn on_progress(&mut self, stage: Stage, update: ProgressUpdate) {
        if self.current_stage != Some(stage) {
            self.current_stage = Some(stage);
            let label = match stage {
                Stage::Download => format!("baixando do {}", self.server_name),
                Stage::Running => "jogando".to_string(),
                Stage::Upload => format!("enviando para o {}", self.server_name),
            };
            tracing::info!("{label}...");
            if stage == Stage::Running {
                // Incidente real (2026-09-06): sem isto, a janela zenity de
                // "Download" continuava flutuando por cima do jogo — que
                // pode não ser fullscreen exclusivo aos olhos do
                // compositor — durante a sessão inteira, sobrepondo a UI
                // dele.
                self.sink.finish();
                self.hidden = true;
            } else if self.hidden {
                self.sink = (self.make_sink)(self.title);
                self.hidden = false;
            }
        }
        if stage == Stage::Running {
            // Janela já escondida acima — nada pra atualizar durante o
            // jogo (sem percentual real nessa fase de qualquer forma).
            return;
        }
        self.sink.update(&ProgressUpdate {
            percent: scaled_percent(stage, update.percent),
            rate: update.rate,
        });
    }

    /// Encerra o diálogo gráfico independentemente de sucesso ou falha —
    /// nunca deixa uma janela zenity órfã pendurada.
    pub fn finish(&mut self) {
        self.sink.finish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn download_scales_into_the_first_half() {
        assert_eq!(scaled_percent(Stage::Download, 0), 0);
        assert_eq!(scaled_percent(Stage::Download, 43), 21);
        assert_eq!(scaled_percent(Stage::Download, 100), 50);
    }

    #[test]
    fn upload_scales_into_the_second_half() {
        assert_eq!(scaled_percent(Stage::Upload, 0), 50);
        assert_eq!(scaled_percent(Stage::Upload, 43), 71);
        assert_eq!(scaled_percent(Stage::Upload, 100), 100);
    }

    #[test]
    fn running_is_pinned_at_the_midpoint_regardless_of_raw_percent() {
        assert_eq!(scaled_percent(Stage::Running, 0), 50);
        assert_eq!(scaled_percent(Stage::Running, 99), 50);
    }

    fn null_sink(_title: &str) -> Box<dyn ProgressSink> {
        Box::new(display::NullSink)
    }

    #[test]
    fn stays_hidden_through_repeated_running_updates_then_reappears_on_upload() {
        let mut progress = SessionProgress::with_sink_factory("titulo", "FeuVault", null_sink);

        progress.on_progress(
            Stage::Download,
            ProgressUpdate {
                percent: 10,
                rate: String::new(),
            },
        );
        assert!(!progress.hidden);

        // `Running` chega várias vezes seguidas na prática (uma checagem
        // periódica, não um evento único) — a janela só deveria esconder
        // na primeira, não alternar a cada chamada.
        for _ in 0..3 {
            progress.on_progress(
                Stage::Running,
                ProgressUpdate {
                    percent: 0,
                    rate: String::new(),
                },
            );
        }
        assert!(progress.hidden);

        progress.on_progress(
            Stage::Upload,
            ProgressUpdate {
                percent: 0,
                rate: String::new(),
            },
        );
        assert!(!progress.hidden);
    }
}
