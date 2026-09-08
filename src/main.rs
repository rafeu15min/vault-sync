// Fase I entrega a fundação (domínio, construtores de caminho, CLI), a
// Fase II entrega o motor de transferência (rsync/SSH, progresso, lock), a
// Fase III conecta os dois num ciclo de vida completo em `run` (ver
// session.rs), a Fase IV entrega a varredura forense + motor de injeção
// (scan.rs/vdf.rs/inject.rs/backup.rs) wireados ao comando `scan`, e a
// Fase V entrega ingestão em lote e remoção remota irrevogável
// (ingest.rs/purge.rs/remote.rs), wireados a `ingest`/`purge`. A unidade
// systemd de suspensão de encerramento (5.2) é um artefato à parte —
// `systemd/vault-sync-flush.service` — instalada manualmente pelo usuário,
// nunca por este binário.

mod backup;
mod catalog;
mod cli;
mod config;
mod display;
mod doctor;
mod domain;
mod ingest;
mod inject;
mod install_paths;
mod list;
mod lock;
mod paths;
mod progress;
mod proton;
mod purge;
mod remote;
mod rsync;
mod scan;
mod session;
mod steam_appid;
mod steam_catalog;
#[cfg(test)]
mod testutil;
mod text_vdf;
mod update;
mod vdf;

use clap::Parser;
use cli::{Cli, Command};
use domain::Launcher;

/// Corpo de `run`: baixa (se preciso), executa `exec`, sincroniza de volta e
/// limpa localmente. Extraído de `Command::Run` pra ser reaproveitado por
/// `Command::MenuLaunch` (o menu rofi despacha pra cá depois de resolver a
/// seleção contra o catálogo) — mesmo motor, dois pontos de entrada.
fn dispatch_run(
    cfg: &config::Config,
    launcher: Launcher,
    app_id: &str,
    exec: &[String],
    sink_title: &str,
) -> anyhow::Result<()> {
    remote::ensure_reachable("ssh", &cfg.remote_ssh_target)?;

    let steam_root = scan::find_steam_root();
    let mut sink = display::pick_sink(sink_title);
    let mut current_stage = None;
    // Incidente real (2026-09-06): sem isto, a janela zenity de "Download"
    // continuava flutuando por cima do jogo — que pode não ser fullscreen
    // exclusivo aos olhos do compositor — durante a sessão inteira,
    // sobrepondo a UI dele. `hidden` rastreia quando ela foi escondida de
    // propósito ao entrar em `Running`, pra saber quando precisa nascer de
    // novo (uma janela nova, não reaproveitada — `finish()` já matou o
    // processo) ao entrar em `Upload`.
    let mut hidden = false;
    let result = session::run_session(
        cfg,
        launcher,
        app_id,
        exec,
        steam_root.as_deref(),
        |stage, update| {
            if current_stage != Some(stage) {
                current_stage = Some(stage);
                let label = match stage {
                    session::Stage::Download => format!("baixando do {}", cfg.server_name()),
                    session::Stage::Running => "jogando".to_string(),
                    session::Stage::Upload => format!("enviando para o {}", cfg.server_name()),
                };
                tracing::info!("{label}...");
                if stage == session::Stage::Running {
                    sink.finish();
                    hidden = true;
                } else if hidden {
                    sink = display::pick_sink(sink_title);
                    hidden = false;
                }
            }
            if stage == session::Stage::Running {
                // Janela já escondida acima — nada pra atualizar durante o
                // jogo (sem percentual real nessa fase de qualquer forma).
                return;
            }
            // Uma única barra 0-100% pra sessão inteira, não uma
            // por transferência — sem isso, a barra "reinicia" do
            // zero a cada estágio, o que parece um erro visual em
            // vez do progresso real da sessão.
            let scaled_percent = match stage {
                session::Stage::Download => update.percent / 2,
                session::Stage::Running => 50,
                session::Stage::Upload => 50 + update.percent / 2,
            };
            sink.update(&crate::progress::ProgressUpdate {
                percent: scaled_percent,
                rate: update.rate,
            });
        },
    );
    // Encerra o diálogo gráfico independentemente de sucesso ou
    // falha — nunca deixa uma janela zenity órfã pendurada.
    sink.finish();
    let session = match result {
        Ok(session) => session,
        Err(err) => {
            // Incidente real (2026-09-06): a barra de progresso soma até um
            // ponto plausível, a janela some (`sink.finish()` acima), e sem
            // isto aqui o erro só existia em `tracing::error!` — invisível
            // tanto lançado via keybind do Hyprland quanto via LaunchOptions
            // da Steam, nenhum dos dois com terminal visível. Resultado:
            // falha de download/upload parecia sucesso silencioso, sem
            // nenhum sinal de que o jogo nunca chegou a baixar de verdade.
            tracing::error!("'run {app_id}' falhou: {err}");
            display::notify_result(
                sink_title,
                &format!("Falha ao rodar '{app_id}':\n{err}"),
                true,
            );
            return Err(err.into());
        }
    };

    tracing::info!(
        pid = ?session.child_pid,
        "'run {app_id}' concluído — biblioteca local limpa, dados sincronizados no {}.",
        cfg.server_name()
    );
    Ok(())
}

/// Versão só-nome de uma label de item ("`app_id` — Nome" ou, pra títulos
/// Heroic, o próprio título) pro popup zenity do `ingest` — o app_id
/// numérico é ruído visual ali (ninguém decora AppID de cabeça), diferente
/// dos logs (`tracing`/`println`), que continuam recebendo a label
/// completa de propósito, pra ficar grepável por id. Sem nome resolvido
/// (atalho non-Steam morto, ex.: um `StartDir` apontando pra um disco
/// desmontado), não sobra nada melhor pra mostrar além do próprio id —
/// devolve a label original nesse caso.
fn popup_friendly(label: &str) -> &str {
    label.split_once(" — ").map_or(label, |(_, name)| name)
}

/// Escreve cada linha de log tanto em stderr (comportamento original,
/// visível pra quem roda no terminal) quanto num arquivo persistente —
/// incidente real (2026-09-06, Phonopolis): rodado via keybind do Hyprland
/// ou LaunchOptions da Steam, stderr não vai a lugar nenhum que alguém veja,
/// então uma falha de `tracing::error!` desaparecia por completo, sem
/// nenhum rastro pra diagnosticar depois. `Arc<Mutex<File>>` porque
/// `tracing-subscriber` pode chamar o `MakeWriter` de threads diferentes.
struct TeeWriter(std::sync::Arc<std::sync::Mutex<std::fs::File>>);

impl std::io::Write for TeeWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let _ = std::io::Write::write_all(&mut std::io::stderr(), buf);
        self.0.lock().unwrap().write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let _ = std::io::Write::flush(&mut std::io::stderr());
        self.0.lock().unwrap().flush()
    }
}

/// Melhor esforço: sem `$XDG_STATE_HOME`/`$HOME` resolvível, ou sem
/// permissão de escrita ali, cai de volta pro `fmt::init()` original
/// (só stderr) — a ausência de log em arquivo nunca deve impedir o
/// `vault-sync` de rodar.
fn init_logging() {
    let log_file = dirs::state_dir()
        .or_else(dirs::home_dir)
        .map(|dir| dir.join("vault-sync"))
        .and_then(|dir| {
            std::fs::create_dir_all(&dir).ok()?;
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(dir.join("vault-sync.log"))
                .ok()
        });

    match log_file {
        Some(file) => {
            let file = std::sync::Arc::new(std::sync::Mutex::new(file));
            tracing_subscriber::fmt()
                .with_ansi(false)
                .with_writer(move || TeeWriter(std::sync::Arc::clone(&file)))
                .init();
        }
        None => tracing_subscriber::fmt::init(),
    }
}

fn main() -> anyhow::Result<()> {
    init_logging();
    let cli = Cli::parse();

    let cfg = config::Config::load(cli.config.as_deref())?;

    match cli.command {
        Command::Run {
            app_id,
            exec,
            steam,
            heroic,
        } => {
            let cfg = cfg.ok_or_else(|| {
                anyhow::anyhow!(
                    "'run' precisa de configuração válida (~/.config/vault-sync/config.json ou --config)"
                )
            })?;
            let launcher = if heroic {
                Launcher::Heroic
            } else {
                debug_assert!(steam);
                Launcher::Steam
            };

            dispatch_run(
                &cfg,
                launcher,
                &app_id,
                &exec,
                &format!("vault-sync — AppID {app_id}"),
            )?;
        }
        Command::Catalog { format } => {
            let cfg = cfg.ok_or_else(|| {
                anyhow::anyhow!(
                    "'catalog' precisa de configuração válida (~/.config/vault-sync/config.json ou --config)"
                )
            })?;
            // Deliberadamente sem `ensure_reachable` aqui — este comando
            // alimenta o menu rofi (`vault-sync-menu.sh`) e precisa aparecer
            // na hora; esperar o FeuVault ligar (~2min) faria o SUPER+G
            // parecer travado. `MenuLaunch`, disparado só depois que o
            // usuário já escolheu um jogo, é quem liga e espera o servidor.
            let catalog = catalog::build_catalog_best_effort(&cfg)?;
            match format {
                cli::CatalogFormat::Rofi => {
                    // Só o que o MenuLaunch de fato consegue lançar — órfão
                    // e non-Steam shortcut sempre terminam em erro se
                    // selecionados (ver CatalogEntry::is_launchable).
                    for entry in catalog.entries.iter().filter(|e| e.is_launchable()) {
                        println!("{}", entry.rofi_line(cfg.server_name(), cfg.client_name()));
                    }
                }
                cli::CatalogFormat::Human => {
                    let columns = ["Plataforma/Local", "Jogo"];
                    let rows: Vec<Vec<String>> = catalog
                        .entries
                        .iter()
                        .map(|entry| {
                            let line = entry.rofi_line(cfg.server_name(), cfg.client_name());
                            // "[Plataforma] Nome (onde)" -> separa em duas
                            // colunas pra tabela; reaproveita o mesmo texto
                            // do modo rofi em vez de duplicar formatação.
                            match line.split_once("] ") {
                                Some((platform, rest)) => {
                                    vec![format!("{platform}]"), rest.to_string()]
                                }
                                None => vec![String::new(), line],
                            }
                        })
                        .collect();
                    display::show_list("vault-sync — catálogo", &columns, &rows);
                }
            }
        }
        Command::MenuLaunch { selection } => {
            let cfg = cfg.ok_or_else(|| {
                anyhow::anyhow!(
                    "'menu-launch' precisa de configuração válida (~/.config/vault-sync/config.json ou --config)"
                )
            })?;
            remote::ensure_reachable("ssh", &cfg.remote_ssh_target)?;
            let catalog = catalog::build_catalog(&cfg)?;
            let entry = catalog
                .find_by_rofi_line(&selection, cfg.server_name(), cfg.client_name())
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "'{selection}' não está mais no catálogo — rode 'vault-sync catalog' de novo (a lista pode ter mudado)"
                    )
                })?;

            match &entry.source {
                domain::GameSource::Official { app_id } => {
                    // `steam -applaunch` só manda um sinal pro cliente Steam
                    // já aberto e o processo retorna quase na hora — o jogo
                    // de verdade sobe como filho do cliente, fora de
                    // qualquer processo que a gente controle diretamente.
                    // `run_via_steam_applaunch` (session.rs) resolve isso
                    // sem depender do processo que disparamos: baixa se
                    // faltar (inclusive "traz de volta" uma instalação
                    // fantasma — manifesto da Steam presente, pasta ausente;
                    // incidente real: Cyberpunk 2077 apagado por um `ingest
                    // --full --and-purge-local` anterior), dispara o
                    // `-applaunch`, e espera o `reaper` da Steam Linux
                    // Runtime (supervisor real da sessão, vive exatamente
                    // enquanto o jogo roda) desaparecer antes de sincronizar
                    // de volta e limpar — só então, nunca antes do jogo
                    // fechar de verdade. Só pra AppID oficial — atalho
                    // non-Steam usa outro caminho, ver o branch
                    // `NonSteamShortcut` abaixo.
                    let app_id = app_id.to_string();
                    let steam_root = scan::find_steam_root();
                    let sink_title = format!("vault-sync — {}", entry.display_name);
                    let mut sink = display::pick_sink(&sink_title);
                    let mut current_stage = None;
                    // Ver comentário equivalente em `dispatch_run`: sem
                    // isto, a janela fica flutuando por cima do jogo a
                    // sessão inteira.
                    let mut hidden = false;
                    let result = session::run_via_steam_applaunch(
                        &cfg,
                        &app_id,
                        steam_root.as_deref(),
                        |stage, update| {
                            if current_stage != Some(stage) {
                                current_stage = Some(stage);
                                let label = match stage {
                                    session::Stage::Download => {
                                        format!("baixando do {}", cfg.server_name())
                                    }
                                    session::Stage::Running => "jogando".to_string(),
                                    session::Stage::Upload => {
                                        format!("enviando para o {}", cfg.server_name())
                                    }
                                };
                                tracing::info!("{label}...");
                                if stage == session::Stage::Running {
                                    sink.finish();
                                    hidden = true;
                                } else if hidden {
                                    sink = display::pick_sink(&sink_title);
                                    hidden = false;
                                }
                            }
                            if stage == session::Stage::Running {
                                return;
                            }
                            let scaled_percent = match stage {
                                session::Stage::Download => update.percent / 2,
                                session::Stage::Running => 50,
                                session::Stage::Upload => 50 + update.percent / 2,
                            };
                            sink.update(&crate::progress::ProgressUpdate {
                                percent: scaled_percent,
                                rate: update.rate,
                            });
                        },
                    );
                    sink.finish();
                    let session = match result {
                        Ok(session) => session,
                        Err(err) => {
                            // Mesmo raciocínio de `dispatch_run`: sem isto, um
                            // `-applaunch` que nunca chega a abrir (Steam com
                            // diálogo de erro, download interrompido) fecha a
                            // barra de progresso e não deixa nenhum sinal
                            // visível de que nada foi sincronizado.
                            tracing::error!("'{}' falhou: {err}", entry.display_name);
                            display::notify_result(
                                &format!("vault-sync — {}", entry.display_name),
                                &format!("Falha ao rodar '{}':\n{err}", entry.display_name),
                                true,
                            );
                            return Err(err.into());
                        }
                    };
                    tracing::info!(
                        pid = ?session.child_pid,
                        "'{}' concluído — sincronizado de volta e limpo localmente.",
                        entry.display_name
                    );
                }
                domain::GameSource::NonSteamShortcut { app_id, .. } => {
                    // Deliberadamente NÃO usa `-applaunch`/`steam://rungameid`
                    // aqui — incidente real (2026-08-29/30): mesmo com
                    // `shortcuts.vdf` byte-a-byte idêntico ao que a própria
                    // Steam grava, AppID recalculado certinho (CRC do
                    // `Exe`+nome) e `CompatToolMapping` configurado, a Steam
                    // devolvia "Game configuration unavailable" pra atalho
                    // non-Steam, sempre — testado com ela fria e já aberta,
                    // sem diferença. `vault-sync run <id> --steam -- <exe>`
                    // chamado direto (o mesmo motor que `Command::Run` já
                    // usa) funcionou de primeira. Então `MenuLaunch`, pra
                    // non-Steam, ignora a resolução de atalho da Steam de
                    // propósito: lê `Exe` do `shortcuts.vdf` ao vivo e roda
                    // via `dispatch_run` — igual ao Heroic, só troca o
                    // launcher.
                    let app_id_str = app_id.to_string();
                    let steam_root = scan::find_steam_root().ok_or_else(|| {
                        anyhow::anyhow!("raiz da instalação da Steam não encontrada")
                    })?;
                    let exe = scan::find_shortcut_exe(&steam_root, *app_id).ok_or_else(|| {
                        anyhow::anyhow!(
                            "'{}' (AppID {app_id}) não tem campo Exe em nenhum shortcuts.vdf local",
                            entry.display_name
                        )
                    })?;

                    // Com a Steam fora da equação (acima), nada mais decidia
                    // qual Wine/prefixo rodava esse `.exe` — sobrava pro
                    // `binfmt_misc` do sistema, que não tinha por que saber
                    // do `compatdata` sincronizado com o FeuVault (ver
                    // `session::run_session_with_options`, que agora seta
                    // `STEAM_COMPAT_DATA_PATH` a partir do mesmo diretório).
                    // Por isso exige explicitamente um Proton já escolhido
                    // (`proton_overrides`, ver config.rs) e roda o script
                    // `proton run <exe>` dele em vez do `.exe` cru — troca
                    // de versão nunca arrisca o save porque o
                    // `STEAM_COMPAT_DATA_PATH`/prefixo-alvo nunca muda, só o
                    // binário que o interpreta. `tools/vault-sync-menu.sh`
                    // sempre chama `set-proton` no primeiro lançamento de um
                    // atalho non-Steam, então chegar aqui sem override só
                    // acontece rodando `menu-launch` direto (fora do
                    // wrapper) — erro acionável, nunca um fallback
                    // silencioso pro binfmt_misc de antes.
                    let tool_name = cfg.proton_overrides.get(&app_id_str).ok_or_else(|| {
                        anyhow::anyhow!(
                            "'{}' (AppID {app_id}) ainda não tem uma versão de Proton escolhida \
                             — rode pelo menu (SUPER+G): a primeira escolha vira o padrão. \
                             Versões instaladas: 'vault-sync list-proton'.",
                            entry.display_name
                        )
                    })?;
                    let tool = proton::resolve(Some(&steam_root), tool_name).ok_or_else(|| {
                        anyhow::anyhow!(
                            "Proton '{tool_name}' (configurado pra '{}') não está mais \
                             instalado — escolha outro (Alt+P no menu, ou 'vault-sync \
                             set-proton').",
                            entry.display_name
                        )
                    })?;
                    let exec = vec![
                        tool.proton_script().to_string_lossy().into_owned(),
                        "run".to_string(),
                        exe,
                    ];
                    dispatch_run(
                        &cfg,
                        Launcher::Steam,
                        &app_id_str,
                        &exec,
                        &format!("vault-sync — {}", entry.display_name),
                    )?;
                }
                domain::GameSource::HeroicGame { title } => {
                    dispatch_run(
                        &cfg,
                        Launcher::Heroic,
                        title,
                        &[
                            "legendary".to_string(),
                            "launch".to_string(),
                            title.clone(),
                            "--offline".to_string(),
                        ],
                        &format!("vault-sync — {}", entry.display_name),
                    )?;
                }
                domain::GameSource::Orphan { .. } => {
                    anyhow::bail!(
                        "'{}' é órfão (sem manifesto/atalho conhecido) — rode 'vault-sync scan' \
                         e investigue antes de tentar lançar pelo menu",
                        entry.display_name
                    );
                }
            }
        }
        Command::Purge {
            app_id,
            yes,
            steam,
            heroic,
        } => {
            let cfg = cfg.ok_or_else(|| {
                anyhow::anyhow!(
                    "'purge' precisa de configuração válida (~/.config/vault-sync/config.json ou --config)"
                )
            })?;
            let launcher = if heroic {
                Launcher::Heroic
            } else {
                debug_assert!(steam);
                Launcher::Steam
            };

            if !yes {
                let server_name = cfg.server_name();
                let question = format!(
                    "Isto vai apagar IRREVOGAVELMENTE os dados de '{app_id}' no {server_name} e os registros locais correspondentes.\n\nContinuar?"
                );
                let confirmed = match display::confirm("vault-sync — purge", &question) {
                    Some(answer) => answer,
                    None => {
                        // Sem sessão gráfica — cai pro prompt de terminal
                        // de sempre, nunca assume uma resposta.
                        eprint!(
                            "Isto vai apagar IRREVOGAVELMENTE os dados de '{app_id}' no {server_name} e os registros locais correspondentes. Digite 'sim' para confirmar: "
                        );
                        use std::io::Write as _;
                        std::io::stderr().flush().ok();
                        let mut answer = String::new();
                        std::io::stdin().read_line(&mut answer)?;
                        answer.trim().eq_ignore_ascii_case("sim")
                    }
                };
                if !confirmed {
                    anyhow::bail!("purge cancelado — confirmação não recebida");
                }
            }

            remote::ensure_reachable("ssh", &cfg.remote_ssh_target)?;

            let mut sink = display::pick_pulsating_sink(&format!("vault-sync — purge {app_id}"));
            sink.label(&format!(
                "removendo do {} e do disco local...",
                cfg.server_name()
            ));
            let steam_root = scan::find_steam_root();
            let result = purge::run_purge(&cfg, launcher, &app_id, steam_root.as_deref());
            sink.finish();
            let summary = result?;

            let report = format!(
                "remoto removido={}\ncompatdata local removido={}\nmanifesto local removido={}\nentradas de shortcuts.vdf removidas={}\ninstalação completa removida (local={}, remoto={})",
                summary.remote_removed,
                summary.local_compatdata_removed,
                summary.local_manifest_removed,
                summary.shortcut_entries_removed,
                summary.full_install_removed_local,
                summary.full_install_removed_remote,
            );
            println!("[vault-sync] 'purge {app_id}' concluído: {report}");
            display::notify_result(
                &format!("vault-sync — purge {app_id} concluído"),
                &report,
                false,
            );
        }
        Command::Update { app_id } => {
            let cfg = cfg.ok_or_else(|| {
                anyhow::anyhow!(
                    "'update' precisa de configuração válida (~/.config/vault-sync/config.json ou --config)"
                )
            })?;

            // Propositalmente sem `remote::ensure_reachable` -- update.rs
            // nunca toca o FeuVault, então uma falha de SSH aqui não deveria
            // impedir uma atualização puramente local via steamcmd.
            let mut sink = display::pick_pulsating_sink(&format!("vault-sync — update {app_id}"));
            sink.label("atualizando via steamcmd...");
            let result = update::run_update(&cfg, &app_id);
            sink.finish();
            let report = result?;

            let summary = format!(
                "AppID {} atualizado em {}",
                report.app_id,
                report.install_dir.display()
            );
            println!("[vault-sync] 'update {app_id}' concluído: {summary}");
            display::notify_result(
                &format!("vault-sync — update {app_id} concluído"),
                &summary,
                false,
            );
        }
        Command::Ingest {
            background,
            full,
            and_purge_local,
            only,
        } => {
            let cfg = cfg.ok_or_else(|| {
                anyhow::anyhow!(
                    "'ingest' precisa de configuração válida (~/.config/vault-sync/config.json ou --config)"
                )
            })?;

            remote::ensure_reachable("ssh", &cfg.remote_ssh_target)?;

            // `--background` já significa "sem interação/UI" (é o modo
            // usado pela unit systemd de poweroff, sem sessão gráfica
            // nenhuma) — respeita a mesma semântica aqui, nunca tenta
            // abrir zenity nesse caso. Barra DETERMINADA (não pulsante):
            // `on_item` abaixo já sabe a posição/total reais, então dá pra
            // mostrar porcentagem de verdade em vez de só "processando
            // algo". `zenity` não tem um widget nativo de "seta pra
            // expandir e mostrar histórico" — a aproximação possível é
            // manter as 3 operações mais recentes sempre visíveis na
            // própria legenda, sem precisar clicar em nada.
            let sink = if background {
                Box::new(display::NullSink) as Box<dyn display::ProgressSink>
            } else {
                display::pick_sink("vault-sync — ingest")
            };
            let sink_cell = std::cell::RefCell::new(sink);
            let recent = std::cell::RefCell::new(std::collections::VecDeque::<String>::new());
            let push_recent = |text: String| {
                let mut recent = recent.borrow_mut();
                if recent.len() >= 3 {
                    recent.pop_back();
                }
                recent.push_front(text);
                recent.iter().cloned().collect::<Vec<_>>().join("\n")
            };
            // `sink.update()` já escreve seu próprio texto de legenda
            // internamente, mas ele é sempre seguido por `sink.label()`
            // logo abaixo — que sobrescreve a legenda antes de qualquer
            // frame renderizar. Por isso a contagem "X/Y — NN%" é montada
            // aqui e prefixada manualmente à legenda dos recentes, em vez
            // de depender do texto interno do `update()`.
            let progress_counter = std::cell::Cell::new((0usize, 0usize));
            let build_label = |recent_text: String| {
                let (index, total) = progress_counter.get();
                if total > 0 {
                    format!("{index}/{total} — {}%\n{recent_text}", index * 100 / total)
                } else {
                    recent_text
                }
            };

            let steam_root = scan::find_steam_root();
            let summary = ingest::run_ingest(
                &cfg,
                full,
                and_purge_local,
                steam_root.as_deref(),
                only.as_deref(),
                |app_id| {
                    if background {
                        tracing::info!(app_id, server = cfg.server_name(), "enviando");
                    } else {
                        println!("[vault-sync] enviando {app_id} ao {}...", cfg.server_name());
                    }
                    let recent_text = push_recent(format!("enviando: {}", popup_friendly(app_id)));
                    sink_cell.borrow_mut().label(&build_label(recent_text));
                },
                |index, total, name| {
                    progress_counter.set((index, total));
                    let recent_text = push_recent(format!("verificando: {}", popup_friendly(name)));
                    let percent = if total > 0 {
                        (index * 100 / total) as u8
                    } else {
                        100
                    };
                    let mut sink = sink_cell.borrow_mut();
                    sink.update(&crate::progress::ProgressUpdate {
                        percent,
                        rate: format!("{index}/{total}"),
                    });
                    sink.label(&build_label(recent_text));
                },
                // Progresso de verdade (bytes transferidos pelo rsync) de
                // cada transferência individual — sem isso, um item sozinho
                // grande (`--only`, ou o último de uma lista) mostrava
                // "100%" parado até o rsync inteiro terminar, já que
                // `on_item` acima só sabe a posição no lote, não o quanto já
                // foi de fato transferido. Sobrescreve o percentual entre um
                // `on_item` e o próximo, sempre com o dado mais recente.
                |update| {
                    sink_cell.borrow_mut().update(&update);
                },
            )?;
            sink_cell.into_inner().finish();

            let mut report = format!(
                "{} inspecionados, {} enviados, {} já presentes no {}, {} apagados localmente",
                summary.inspected,
                summary.uploaded,
                summary.skipped_existing,
                cfg.server_name(),
                summary.purged_local,
            );
            println!("[vault-sync] 'ingest' concluído: {report}");
            if full {
                let full_line = format!(
                    "{} instalações completas enviadas, {} manifestos oficiais migrados pra local_game_root_steam",
                    summary.full_uploaded, summary.official_manifests_migrated,
                );
                println!("[vault-sync] 'ingest --full': {full_line}");
                report.push('\n');
                report.push_str(&full_line);
                if !summary.stale_metadata_cleaned.is_empty() {
                    println!(
                        "[vault-sync] 'ingest --full': {} registros obsoletos limpos (installed.json do Legendary / appmanifest da Steam — jogo já confirmado no {}, sem vestígio local):",
                        summary.stale_metadata_cleaned.len(),
                        cfg.server_name(),
                    );
                    for name in &summary.stale_metadata_cleaned {
                        println!("  {name}");
                    }
                    let cleaned_list = summary
                        .stale_metadata_cleaned
                        .iter()
                        .map(|s| format!("  {s}"))
                        .collect::<Vec<_>>()
                        .join("\n");
                    report.push_str(&format!(
                        "\n{} registros obsoletos limpos:\n{cleaned_list}",
                        summary.stale_metadata_cleaned.len(),
                    ));
                }
                if !summary.full_skipped.is_empty() {
                    println!(
                        "[vault-sync] 'ingest --full': {} jogos pulados (caminho de instalação não verificável — corrija o atalho/manifesto e rode de novo):",
                        summary.full_skipped.len(),
                    );
                    for name in &summary.full_skipped {
                        println!("  {name}");
                    }
                    // Um item por linha (não separados por vírgula numa
                    // única linha gigante) — com `--no-wrap` do zenity
                    // removido, essa lista ficava esticando a janela pela
                    // tela inteira. Capada em 15 no popup (o terminal já
                    // recebeu a lista completa acima, via println) pra uma
                    // migração grande não virar uma janela quilométrica.
                    const POPUP_SKIPPED_LIMIT: usize = 15;
                    let popup_list = summary
                        .full_skipped
                        .iter()
                        .take(POPUP_SKIPPED_LIMIT)
                        .map(|s| format!("  {s}"))
                        .collect::<Vec<_>>()
                        .join("\n");
                    let popup_suffix = if summary.full_skipped.len() > POPUP_SKIPPED_LIMIT {
                        format!(
                            "\n  … e mais {} (lista completa no terminal)",
                            summary.full_skipped.len() - POPUP_SKIPPED_LIMIT
                        )
                    } else {
                        String::new()
                    };
                    report.push_str(&format!(
                        "\n{} pulados:\n{popup_list}{popup_suffix}",
                        summary.full_skipped.len(),
                    ));
                }
            }
            if !background {
                display::notify_result("vault-sync — ingest concluído", &report, false);
            }
        }
        Command::Scan { output } => {
            let extra_libraries = cfg
                .as_ref()
                .map(|c| c.local_libraries.clone())
                .unwrap_or_default();

            let report = scan::run_scan(&extra_libraries)?;

            // 4.3: a cada varredura, também injeta o marcador do
            // orquestrador em qualquer LaunchOptions de shortcut non-Steam
            // que ainda não o tenha — "execução periódica programada" da
            // spec, disparada aqui manualmente (um timer systemd fica para
            // a Fase V).
            let mut injected_total = 0usize;
            if let Some(steam_root) = scan::find_steam_root() {
                for (path, result) in inject::inject_all_shortcuts(&steam_root) {
                    match result {
                        Ok(summary) if summary.injected > 0 => {
                            tracing::info!(
                                ?path,
                                injected = summary.injected,
                                "marcador vault-sync injetado em shortcuts.vdf"
                            );
                            injected_total += summary.injected;
                        }
                        Ok(_) => {}
                        Err(e) => {
                            tracing::warn!(?path, "falha ao processar shortcuts.vdf: {e}");
                        }
                    }
                }

                // Mesmo tratamento, agora pra jogos Steam OFICIAIS via
                // `localconfig.vdf` — lacuna estrutural corrigida em
                // 2026-08-31 (ver `inject::inject_official_launch_options`):
                // sem isso, "Play" direto na Steam pra qualquer jogo oficial
                // sempre pulava o vault-sync inteiro.
                let official_app_ids: Vec<u32> = report
                    .entries
                    .iter()
                    .filter_map(|e| match &e.source {
                        domain::GameSource::Official { app_id } => Some(*app_id),
                        _ => None,
                    })
                    .collect();
                for (path, result) in
                    inject::inject_all_official_launch_options(&steam_root, &official_app_ids)
                {
                    match result {
                        Ok(summary) if summary.injected > 0 => {
                            tracing::info!(
                                ?path,
                                injected = summary.injected,
                                "marcador vault-sync injetado em localconfig.vdf (jogos oficiais)"
                            );
                            injected_total += summary.injected;
                        }
                        Ok(_) => {}
                        Err(e) => {
                            tracing::warn!(?path, "falha ao processar localconfig.vdf: {e}");
                        }
                    }
                }
            }

            let json = serde_json::to_string_pretty(&report)?;
            std::fs::write(&output, json)?;

            let summary_text = format!(
                "{} oficiais, {} non-Steam, {} órfãos, {} LaunchOptions injetadas\n\nCatálogo salvo em {}",
                report.official_count,
                report.non_steam_count,
                report.orphan_count,
                injected_total,
                output.display(),
            );
            println!(
                "[vault-sync] varredura concluída -> {} ({} oficiais, {} non-Steam, {} órfãos, {} LaunchOptions injetadas)",
                output.display(),
                report.official_count,
                report.non_steam_count,
                report.orphan_count,
                injected_total,
            );
            display::notify_result("vault-sync — scan concluído", &summary_text, false);
        }
        Command::Doctor => {
            let cfg = cfg.ok_or_else(|| {
                anyhow::anyhow!(
                    "'doctor' precisa de configuração válida (~/.config/vault-sync/config.json ou --config)"
                )
            })?;

            let report = doctor::run_doctor(&cfg);

            println!("[vault-sync] doctor — bibliotecas locais:");
            for lib in &report.libraries {
                let status = if lib.exists { "OK" } else { "AUSENTE" };
                let mount_note = if lib.exists && !lib.is_mount_point {
                    " (aviso: não é um ponto de montagem separado)"
                } else {
                    ""
                };
                println!("  [{status}] {}{mount_note}", lib.path.display());
            }

            println!(
                "[vault-sync] doctor — {} ({}):",
                cfg.server_name(),
                report.remote.ssh_target
            );
            if !report.remote.reachable {
                println!(
                    "  [AUSENTE] SSH inalcançável (servidor desligado? ligue-o manualmente antes de rodar run/ingest/purge)"
                );
            } else {
                use remote::RemotePathStatus;
                let path_status = match report.remote.base_path_status {
                    Some(RemotePathStatus::Directory) => "OK",
                    Some(RemotePathStatus::MountPoint) => "OK (ponto de montagem)",
                    Some(RemotePathStatus::Missing) | None => "AUSENTE",
                };
                println!("  [OK] SSH alcançável");
                println!(
                    "  [{path_status}] remote_base_path: {}",
                    report.remote.base_path.display()
                );
            }

            if let Some(heroic) = &report.heroic {
                println!("[vault-sync] doctor — Heroic (Epic):");
                let local_status = if heroic.local_root.exists {
                    "OK"
                } else {
                    "AUSENTE"
                };
                println!(
                    "  [{local_status}] heroic_local_root: {}",
                    heroic.local_root.path.display()
                );
                use remote::RemotePathStatus;
                let remote_status = match heroic.remote_base_path_status {
                    Some(RemotePathStatus::Directory) => "OK",
                    Some(RemotePathStatus::MountPoint) => "OK (ponto de montagem)",
                    Some(RemotePathStatus::Missing) | None => "AUSENTE",
                };
                println!(
                    "  [{remote_status}] heroic_remote_base_path: {}",
                    heroic.remote_base_path.display()
                );
            }

            // Informativo, nunca uma falha dura (`has_hard_failures` não
            // olha pra isso) — resíduo de remanejo de disco (ver
            // `scan::find_stale_shortcuts`). Remoção continua manual/fora
            // deste comando de propósito: `doctor` nunca muta nada.
            if !report.stale_shortcuts.is_empty() {
                println!(
                    "[vault-sync] doctor — atalhos Steam obsoletos ({} — resíduo de remanejo de disco, veja 'vault-sync stage' pra recriar o atalho correto):",
                    report.stale_shortcuts.len()
                );
                for stale in &report.stale_shortcuts {
                    println!(
                        "  {} — AppID {} morto ({}), AppID {} é o vivo",
                        stale.name, stale.dead_app_id, stale.dead_exe, stale.live_app_id
                    );
                }
            }

            let libraries_ok = report.libraries.iter().filter(|l| l.exists).count();
            let mut summary_text = format!(
                "Bibliotecas locais: {}/{} OK\n{}: {}\n{}",
                libraries_ok,
                report.libraries.len(),
                cfg.server_name(),
                if report.remote.reachable {
                    "alcançável"
                } else {
                    "inalcançável"
                },
                if report.has_hard_failures() {
                    "Encontrou falhas — corrija antes de rodar run/ingest/purge."
                } else {
                    "Tudo pronto."
                },
            );
            if !report.stale_shortcuts.is_empty() {
                summary_text.push_str(&format!(
                    "\n{} atalho(s) Steam obsoleto(s) — resíduo de remanejo de disco",
                    report.stale_shortcuts.len()
                ));
            }

            if report.has_hard_failures() {
                display::notify_result("vault-sync — doctor", &summary_text, true);
                anyhow::bail!(
                    "'doctor' encontrou falhas — corrija antes de rodar run/ingest/purge"
                );
            }

            println!("[vault-sync] doctor: tudo pronto.");
            display::notify_result("vault-sync — doctor", &summary_text, false);
        }
        Command::List => {
            let cfg = cfg.ok_or_else(|| {
                anyhow::anyhow!(
                    "'list' precisa de configuração válida (~/.config/vault-sync/config.json ou --config)"
                )
            })?;

            remote::ensure_reachable("ssh", &cfg.remote_ssh_target)?;

            let listing = list::run_list(&cfg)?;

            // Best-effort, resolvido uma única vez por ID e reaproveitado
            // tanto no print quanto nas rows do zenity abaixo (não há mais
            // catálogo em lote — GetAppList foi descontinuado pela Valve —
            // então cada ID oficial é um GET individual; ver steam_catalog.rs).
            // Se a rede estiver indisponível, `resolve_name` simplesmente
            // não acha nada e o `list` segue mostrando só os IDs, como antes
            // — nunca falha por causa disso.
            let names: std::collections::HashMap<String, Option<String>> = listing
                .steam_compatdata_ids
                .iter()
                .map(|id| {
                    let name = steam_catalog::resolve_name(
                        id,
                        &cfg.install_path_overrides.piratas,
                        steam_catalog::fetch_official_name,
                    );
                    (id.clone(), name)
                })
                .collect();

            println!(
                "[vault-sync] list — compatdata Steam no {}:",
                cfg.server_name()
            );
            if listing.steam_compatdata_ids.is_empty() {
                println!("  (nenhum)");
            } else {
                for id in &listing.steam_compatdata_ids {
                    match names.get(id).and_then(Option::clone) {
                        Some(name) => println!("  {id}  —  {name}"),
                        None => println!("  {id}"),
                    }
                }
            }

            if let Some(titles) = &listing.heroic_titles {
                println!(
                    "[vault-sync] list — prefixos Heroic no {}:",
                    cfg.server_name()
                );
                if titles.is_empty() {
                    println!("  (nenhum)");
                } else {
                    for title in titles {
                        println!("  {title}");
                    }
                }
            }

            if let Some(names) = &listing.piratas_installs {
                println!(
                    "[vault-sync] list — instalações completas de pirata no {} (use 'vault-sync stage <nome>'):",
                    cfg.server_name()
                );
                if names.is_empty() {
                    println!("  (nenhuma)");
                } else {
                    for name in names {
                        println!("  {name}");
                    }
                }
            }

            let mut rows: Vec<Vec<String>> = listing
                .steam_compatdata_ids
                .iter()
                .map(|id| {
                    let name = names.get(id).and_then(Option::clone).unwrap_or_default();
                    vec!["Steam".to_string(), id.clone(), name]
                })
                .collect();
            if let Some(titles) = &listing.heroic_titles {
                rows.extend(
                    titles
                        .iter()
                        .map(|title| vec!["Heroic".to_string(), title.clone(), String::new()]),
                );
            }
            if let Some(names) = &listing.piratas_installs {
                rows.extend(
                    names
                        .iter()
                        .map(|name| vec!["Pirata".to_string(), name.clone(), String::new()]),
                );
            }
            display::show_list(
                &format!("vault-sync — o que já está no {}", cfg.server_name()),
                &["Tipo", "ID/Título", "Nome"],
                &rows,
            );
        }
        Command::Stage { name } => {
            let cfg = cfg.ok_or_else(|| {
                anyhow::anyhow!(
                    "'stage' precisa de configuração válida (~/.config/vault-sync/config.json ou --config)"
                )
            })?;
            let root = cfg.local_game_root_piratas.as_ref().ok_or_else(|| {
                anyhow::anyhow!("'local_game_root_piratas' não configurado — nada pra adotar")
            })?;
            let remote_base = cfg.remote_installs_base_path.as_ref().ok_or_else(|| {
                anyhow::anyhow!("'remote_installs_base_path' não configurado — nada pra trazer")
            })?;

            remote::ensure_reachable("ssh", &cfg.remote_ssh_target)?;

            let remote_dir = remote_base.join("piratas").join(&name);
            let local_dir = root.join(&name);
            let mut sink = display::pick_sink(&format!("vault-sync — preparando {name}"));
            let result = rsync::RsyncTransfer::full_download(
                &cfg.remote_ssh_target,
                &remote_dir,
                &local_dir,
            )
            .execute_with_progress(|u| sink.update(&u));
            sink.finish();
            result?;

            // Fecha o loop "ovo-e-galinha" quando dá: se já existe um
            // atalho non-Steam com esse nome em algum `shortcuts.vdf` local
            // (mesmo com `Exe` obsoleto por remanejo de disco — o caso
            // recorrente que motivou isso), reaproveita o nome do arquivo e
            // cria um atalho novo apontando pra `local_dir`, calculando o
            // AppID do mesmo jeito que a própria Steam calcularia (ver
            // `inject::stage_pirata_shortcut`/`steam_appid.rs`). Sem atalho
            // anterior pra copiar (primeiro cadastro de sempre), cai pro
            // fluxo manual de sempre.
            let manual_fallback = || {
                tracing::info!(
                    "'{name}' pronto em {} — aponte 'Add a Non-Steam Game' da Steam pro \
                     executável dentro dessa pasta. Depois, rode 'vault-sync scan' pra injetar \
                     a sincronização.",
                    local_dir.display()
                );
            };
            match scan::find_steam_root() {
                Some(steam_root) => {
                    match inject::stage_pirata_shortcut(&steam_root, &name, &local_dir) {
                        Ok(inject::StageShortcutOutcome::Created { app_id, exe }) => {
                            tracing::info!(
                                "'{name}' pronto em {} — atalho Steam criado automaticamente \
                                 (AppID {app_id}, {exe}). Reinicie a Steam pra ele aparecer na \
                                 biblioteca.",
                                local_dir.display()
                            );
                        }
                        Ok(inject::StageShortcutOutcome::AlreadyUpToDate { app_id }) => {
                            tracing::info!(
                                "'{name}' pronto em {} — atalho Steam (AppID {app_id}) já \
                                 apontava pra esse caminho, nada mudou.",
                                local_dir.display()
                            );
                        }
                        Ok(inject::StageShortcutOutcome::NoPriorShortcutFound) => manual_fallback(),
                        Err(e) => {
                            tracing::warn!(
                                "não consegui criar o atalho Steam automaticamente ({e})"
                            );
                            manual_fallback();
                        }
                    }
                }
                None => manual_fallback(),
            }
        }
        Command::ListProton => {
            let steam_root = scan::find_steam_root();
            for tool in proton::list_installed(steam_root.as_deref()) {
                println!("{}", tool.name);
            }
        }
        Command::GetProton { selection } => {
            let cfg = cfg.ok_or_else(|| {
                anyhow::anyhow!(
                    "'get-proton' precisa de configuração válida (~/.config/vault-sync/config.json ou --config)"
                )
            })?;
            let catalog = catalog::build_catalog_best_effort(&cfg)?;
            let entry = catalog
                .find_by_rofi_line(&selection, cfg.server_name(), cfg.client_name())
                .ok_or_else(|| anyhow::anyhow!("'{selection}' não está mais no catálogo"))?;
            let app_id = match &entry.source {
                domain::GameSource::NonSteamShortcut { app_id, .. } => app_id.to_string(),
                _ => anyhow::bail!(
                    "seleção de Proton só se aplica a atalhos non-Steam (esta é {:?})",
                    entry.source
                ),
            };
            let tool_name = cfg.proton_overrides.get(&app_id).ok_or_else(|| {
                anyhow::anyhow!(
                    "'{}' ainda não tem Proton padrão definido",
                    entry.display_name
                )
            })?;
            println!("{tool_name}");
        }
        Command::SetProton { selection, name } => {
            let mut cfg = cfg.ok_or_else(|| {
                anyhow::anyhow!(
                    "'set-proton' precisa de configuração válida (~/.config/vault-sync/config.json ou --config)"
                )
            })?;
            let catalog = catalog::build_catalog_best_effort(&cfg)?;
            let entry = catalog
                .find_by_rofi_line(&selection, cfg.server_name(), cfg.client_name())
                .ok_or_else(|| anyhow::anyhow!("'{selection}' não está mais no catálogo"))?;
            let app_id = match &entry.source {
                domain::GameSource::NonSteamShortcut { app_id, .. } => app_id.to_string(),
                _ => anyhow::bail!(
                    "seleção de Proton só se aplica a atalhos non-Steam (esta é {:?})",
                    entry.source
                ),
            };

            let steam_root = scan::find_steam_root();
            // Nunca grava uma versão que não existe de verdade — sem essa
            // checagem, um `set-proton` com nome digitado errado (ou uma
            // versão desinstalada depois) só falharia silenciosamente na
            // hora de jogar, bem mais tarde e mais confuso.
            proton::resolve(steam_root.as_deref(), &name).ok_or_else(|| {
                anyhow::anyhow!(
                    "Proton '{name}' não está instalado — veja as opções com 'vault-sync list-proton'"
                )
            })?;

            cfg.proton_overrides.insert(app_id, name.clone());
            let config_path =
                config::Config::resolve_path(cli.config.as_deref()).ok_or_else(|| {
                    anyhow::anyhow!("não foi possível resolver o caminho de config.json")
                })?;
            cfg.save(&config_path)?;

            println!(
                "[vault-sync] Proton '{name}' definido como padrão para '{}'",
                entry.display_name
            );
        }
        Command::PruneShortcuts { yes } => {
            let steam_root = scan::find_steam_root()
                .ok_or_else(|| anyhow::anyhow!("raiz da instalação da Steam não encontrada"))?;
            let stale = scan::find_stale_shortcuts(&steam_root);
            if stale.is_empty() {
                println!("[vault-sync] prune-shortcuts: nenhum atalho obsoleto encontrado.");
                return Ok(());
            }

            println!("[vault-sync] atalhos obsoletos a remover:");
            for s in &stale {
                println!(
                    "  {} — AppID {} morto ({}); AppID {} continua",
                    s.name, s.dead_app_id, s.dead_exe, s.live_app_id
                );
            }

            if !yes {
                let question = format!(
                    "Isto vai remover IRREVOGAVELMENTE {} entrada(s) obsoleta(s) de \
                     shortcuts.vdf (backup atômico automático antes). Nenhum dado remoto é \
                     afetado.\n\nContinuar?",
                    stale.len()
                );
                let confirmed = match display::confirm("vault-sync — prune-shortcuts", &question)
                {
                    Some(answer) => answer,
                    None => {
                        eprint!(
                            "Isto vai remover IRREVOGAVELMENTE {} entrada(s) obsoleta(s) de \
                             shortcuts.vdf. Digite 'sim' para confirmar: ",
                            stale.len()
                        );
                        use std::io::Write as _;
                        std::io::stderr().flush().ok();
                        let mut answer = String::new();
                        std::io::stdin().read_line(&mut answer)?;
                        answer.trim().eq_ignore_ascii_case("sim")
                    }
                };
                if !confirmed {
                    anyhow::bail!("prune-shortcuts cancelado — confirmação não recebida");
                }
            }

            let mut removed_total = 0usize;
            for s in &stale {
                for (path, result) in
                    purge::remove_shortcut_entries_everywhere(&steam_root, s.dead_app_id)
                {
                    match result {
                        Ok(removed) => removed_total += removed,
                        Err(e) => {
                            tracing::warn!(?path, "falha ao remover entrada obsoleta: {e}");
                        }
                    }
                }
            }

            let report = format!("{removed_total} entrada(s) obsoleta(s) removida(s)");
            println!("[vault-sync] prune-shortcuts concluído: {report}");
            display::notify_result("vault-sync — prune-shortcuts concluído", &report, false);
        }
        Command::SetServerName { name } => {
            let mut cfg = cfg.ok_or_else(|| {
                anyhow::anyhow!(
                    "'set-server-name' precisa de configuração válida (~/.config/vault-sync/config.json ou --config)"
                )
            })?;
            cfg.server_name = Some(name.clone());
            let config_path =
                config::Config::resolve_path(cli.config.as_deref()).ok_or_else(|| {
                    anyhow::anyhow!("não foi possível resolver o caminho de config.json")
                })?;
            cfg.save(&config_path)?;
            println!("[vault-sync] nome do servidor definido como '{name}'");
        }
        Command::SetClientName { name } => {
            let mut cfg = cfg.ok_or_else(|| {
                anyhow::anyhow!(
                    "'set-client-name' precisa de configuração válida (~/.config/vault-sync/config.json ou --config)"
                )
            })?;
            cfg.client_name = Some(name.clone());
            let config_path =
                config::Config::resolve_path(cli.config.as_deref()).ok_or_else(|| {
                    anyhow::anyhow!("não foi possível resolver o caminho de config.json")
                })?;
            cfg.save(&config_path)?;
            println!("[vault-sync] nome desta máquina definido como '{name}'");
        }
    }

    Ok(())
}
