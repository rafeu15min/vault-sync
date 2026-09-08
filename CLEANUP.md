# vault-sync — Plano de polimento da V1

> Auditoria feita em 2026-09-07/08, depois que a ferramenta já estava
> funcionalmente completa e estável (Fases I-VII + universalização de
> nomes). Objetivo: "menos é mais" — identificar código morto, duplicação
> mecânica e oportunidades de simplificação, sem tocar em nenhuma proteção
> ligada a incidente real documentado. Nada deste arquivo foi executado
> ainda — é só o plano, pra execução futura.
>
> **Antes de executar qualquer camada abaixo**: este projeto não tem
> controle de versão (`git init` nunca foi feito). Inicialize um repositório
> git com um commit do estado atual antes de tocar em qualquer coisa aqui —
> mesmo a Camada 1, que é mecânica, merece um jeito seguro de reverter.

## Como isso foi levantado

Quatro auditorias em paralelo (uma por área do código: `ingest.rs`+
`session.rs`; `scan.rs`+`install_paths.rs`+`inject.rs`+`purge.rs`;
`main.rs`+`cli.rs`+`catalog.rs`+`list.rs`+`remote.rs`+`doctor.rs`; e os
módulos utilitários menores), mais uma mineração da memória do claude-mem
pra listar todo incidente real que já justificou código "especial-caso" no
projeto (StartDir=$HOME, manifesto-fantasma do Kenshi, `installed.json` do
Legendary ficando obsoleto sozinho, ambiguidade do exit-23 do rsync, drift
de AppID do Haste, etc.) — nada disso é candidato a remoção, é a rede de
segurança que o README já documenta. Também rodei `cargo build --release`
com `#![allow(dead_code)]` temporariamente removido pra expor código morto
de verdade (confirmado pelo compilador, não só leitura manual), e verifiquei
`Cargo.toml` (nenhuma dependência sem uso) e o projeto inteiro por
`TODO`/`FIXME`/`HACK` (nenhum encontrado).

## Camada 1 — código morto, risco zero, mecânico (~150-250 linhas)

Confirmado pelo compilador (com `#![allow(dead_code)]` removido) ou por
rastreamento manual de chamadores:

- `main.rs:10` — `#![allow(dead_code)]` **global**. Estava mascarando os
  itens abaixo o projeto inteiro. Remover por último, depois de já ter
  apagado os itens mortos — senão a build quebra com os warnings virando
  erros (dependendo de `#[deny]` em CI, ou só pra deixar limpo).
- `session.rs:258` — `ensure_legendary_knows_about_install` (o wrapper não-
  `_at`). Resíduo direto do conserto de teste-isolado feito em 2026-09-07:
  a chamada real foi trocada pela variante `_at` injetável, e o wrapper
  ficou sem chamador. Zero chamadores confirmados via grep.
- `session.rs:555` — `ensure_local_ready`. Zero chamadores em todo o
  projeto. O comentário dela (linha ~361) cita
  `ensure_local_ready_with_options`, uma função que **não existe** —
  comentário órfão também precisa ir junto.
- `session.rs:846` — `pid_is_alive` (cópia local). Só é usada pelo próprio
  teste (`pid_is_alive_reflects_real_proc_state`, linha ~1197). A versão
  realmente usada em produção é `lock.rs::pid_is_alive` (duplicada de
  propósito, com comentário já justificando o porquê de não acoplar
  `lock.rs` a `session.rs` por uma função de uma linha — essa decisão já
  foi tomada, não precisa reabrir). Se apagar, apagar o teste junto.
- `lock.rs:95` — `LockGuard::path()`. Só usado pelo próprio teste. Decisão
  de gosto: pode ficar como API razoável de um tipo "guard", ou apagar
  junto com o teste que só existe pra exercitá-lo. Baixo valor dos dois
  lados.
- `install_paths.rs:241` — `resolve_shortcut_install_dir`. **Investigado a
  fundo por ser adjacente à proteção do incidente StartDir=$HOME** (a
  memória do claude-mem apontou esse incidente como historicamente "nunca
  fechado, adiado pro refactor de universalização" — por isso a cautela
  extra aqui). Confirmado: `session.rs` e `purge.rs` não chamam mais essa
  função — eles chamam `scan::resolve_piratas_install_dir` direto e
  aplicam `install_paths::is_safe_install_dir` no próprio local de uso
  (session.rs:169, purge.rs — ambos citando o incidente explicitamente no
  comentário). A proteção real está lá, testada
  (`is_safe_install_dir_refuses_home_dir_itself` e mais 4 testes
  dedicados) e intacta — `resolve_shortcut_install_dir` é órfã de um
  design anterior, não é a proteção ativa. Seguro apagar, junto com seus 3
  usos (todos em teste): `resolve_shortcut_install_dir_extracts_and_
  unquotes_the_matching_entry`, `resolve_shortcut_install_dir_is_none_for_
  unknown_app_id`, e `shortcut_install_dir_refuses_start_dir_equal_to_
  real_home_dir` — este último vira redundante com os testes já dedicados
  de `is_safe_install_dir`.
- `domain.rs:125` — campo `GameSession.library`. Gravado em
  `GameSession::new`, nunca lido em lugar nenhum.
- `domain.rs:126` — campo `GameSession.started_at`. Também só-escrita
  (`session.rs:610,988` gravam, ninguém lê). Não pego pelo warning do
  compilador (possível limitação do lint pra esse campo específico), mas
  confirmado por grep manual — nenhuma leitura em todo `src/`.

Também candidato a apagar (não é "morto" tecnicamente, mas não pertence à
suíte de regressão):

- `install_paths.rs:764-795` — teste `real_machine_legendary_install_path_
  matches_known_state`, marcado `#[ignore]`, crava caminhos reais deste
  computador (`/mnt/SSD2`, `/mnt/HDD1`) e nomes de jogos específicos como
  estavam em 2026-08-14, com comentário admitindo que vai ficar obsoleto
  ("se este teste falhar, é sinal de que algum jogo mudou de disco"). É
  uma ferramenta de depuração pessoal, não um teste de regressão portável
  — não combina com o objetivo de universalidade do projeto. Apagar, ou
  mover pra um script local fora do repositório (git-ignorado).

## Camada 2 — duplicação mecânica, risco baixo (~250-300 linhas)

- **`scan.rs`** — 4 "andarilhos" de árvore VDF quase idênticos
  (`shortcuts_from_tree` ~120-151, `shortcut_field_from_tree` ~200-231,
  `exe_for_appname_from_tree` ~260-287, `shortcuts_with_exe_from_tree`
  ~354-392): todos fazem "achar objeto 'shortcuts' → iterar entradas →
  extrair campo(s) case-insensitive", só o campo extraído muda. ~170
  linhas colapsariam pra ~50 com um iterador genérico tipo
  `for_each_shortcut_entry(root, |fields| ...)`.
- **`scan.rs`+`inject.rs`+`purge.rs`** — 5 funções em 3 arquivos
  reimplementando o mesmo driver "ler `userdata/*/config/<arquivo>.vdf`,
  aplicar uma operação, coletar `(PathBuf, Result<T,E>)`":
  `find_shortcut_field` (scan.rs ~179-195), `find_shortcut_exe_by_name`
  (scan.rs ~242-258), `inject_all_shortcuts`/`inject_all_official_launch_
  options` (inject.rs ~114-130, ~215-232), `remove_shortcut_entries_
  everywhere` (purge.rs ~243-260). Um helper único `for_each_userdata_
  file(steam_root, filename, |path| -> R) -> Vec<(PathBuf, R)>` (viveria
  bem em `scan.rs`, que já é dono de `find_steam_root`) remove ~60 linhas
  duplicadas.
- **Helpers de teste repetidos** — `test_dir(label)` (construção de
  diretório temporário com PID+nanossegundos) está reimplementado quase
  palavra-por-palavra em `config.rs`, `backup.rs`, `proton.rs`,
  `update.rs`, `rsync.rs`, `session.rs`, `list.rs` e mais. Mesma história
  pra `fake_bin`/`fake_script`/`write_script` (escrever um script
  executável num caminho temporário). Um módulo `testutil` (ou macro)
  compartilhado, com ~6-8 pontos de troca, é a limpeza mais mecânica e de
  menor risco do projeto inteiro — zero nuance comportamental entre as
  cópias.
- **`main.rs`** — o padrão `cfg.ok_or_else(|| anyhow::anyhow!("'X'
  precisa de configuração válida..."))?` se repete em 13 dos ~20
  handlers de comando (Run, Catalog, MenuLaunch, Purge, Update, Ingest,
  Doctor, List, Stage, GetProton, SetProton, SetServerName,
  SetClientName), só o nome do comando muda. Uma função `require_config
  (cfg: Option<Config>, command: &str) -> anyhow::Result<Config>` colapsa
  ~50 linhas em 13 chamadas de uma linha.
- **`main.rs:499-522` vs `1224-1250`** — `Purge` e `PruneShortcuts` têm o
  mesmo par "confirma via zenity, senão `eprint!`+stdin no terminal"
  (~24 linhas cada), só o texto da pergunta muda. Uma função
  `confirm_irrevocable(gui_title, question, terminal_prompt) ->
  anyhow::Result<bool>` resolve.
- **`main.rs:1148-1157` vs `1172-1182`** — `GetProton`/`SetProton`
  compartilham o mesmo bloco "resolve entrada do catálogo, casa contra
  `NonSteamShortcut` ou aborta" (~10 linhas). Prioridade baixa, ganho
  pequeno.
- **`ingest.rs:359-538`** — 6 funções `count_matching_*` que só re-rodam a
  função de descoberta correspondente e tiram `.len()`. Duplicação trivial
  de wrappers finos — consolidar num único helper parametrizado, ou só
  inline `.len()` nos 3-4 pontos de chamada.
- **`remote.rs`** — o trio `match output.status.code() { Some(0) =>
  Ok(..), Some(code) => Err(NonZeroExit(..)), None => Err(KilledBySignal)
  }` aparece 3x, com uma variante ligeiramente diferente 2x mais. Ganho
  pequeno (~4 linhas por ponto), cosmético, prioridade baixa.
- **`doctor.rs:107-111` vs `127-131`** — construção de `LibraryCheck`
  duplicada entre `local_libraries` e `heroic_local_root`. Extração
  trivial, ganho trivial.

## Camada 3 — extração de lógica real, risco médio (~100 linhas + cobertura nova)

- **`main.rs:67-108` vs `323-360`** — o closure de progresso (rótulos
  Download/Running/Upload, a fórmula de escala 0-100% do zenity, o toggle
  de janela escondida pra não flutuar sobre um jogo em tela cheia) está
  duplicado palavra-por-palavra entre `dispatch_run` e o ramo
  `GameSource::Official` de `MenuLaunch`, só trocando `session::
  run_session` por `session::run_via_steam_applaunch`. **Esse closure não
  tem nenhum teste hoje**, apesar de ter lógica real (uma fórmula de
  escala errada erraria silenciosamente o progresso mostrado). Extrair pra
  uma função/módulo testável mata a duplicação e o buraco de cobertura ao
  mesmo tempo — maior valor combinado do plano inteiro, mas mecânica de
  borrow-checker em torno de `sink`/`current_stage`/`hidden` capturados
  por referência mutável deixa a extração menos trivial que copiar-colar.

## Camada 4 — redesenho estrutural, opcional, mais arriscado

- **`ingest.rs:918-1145` (+ passada Epic mais adiante no arquivo)** — três
  passadas quase idênticas (Steam oficial, piratas, Epic) repetindo
  "resolver instalação → pular se for ferramenta de compatibilidade →
  checar `already_remote` → `sync_full_install` → apagar local → checar
  metadado obsoleto no branch `None`" — ~70% de sobreposição estrutural.
  As diferenças reais são a fonte de descoberta (varredura de manifesto vs.
  chaves de override vs. títulos do `installed.json`) e efeitos colaterais
  específicos da categoria (adoção/purga de manifesto Steam). Dá pra
  unificar numa passada genérica parametrizada por (função de descoberta,
  nome da categoria, função de resolução, hook pós-purga), mas isso é
  trabalho de design de verdade, não mecânico — os testes pareados de cada
  categoria (`full_manifest_only_entry_already_on_remote_is_not_reported_
  as_skipped` / `full_pirata_manifest_only_entry_...`) colapsariam juntos
  na fusão. Recomendo tratar como fase separada, só depois de ver as
  Camadas 1-3 resolvidas e se ainda parecer valer a pena.
- Dentro da mesma função gigante (`run_ingest_with_options`, ~588-1450+):
  mesmo sem fundir a lógica duplicada acima, quebrar as 4 passadas
  sequenciais em funções privadas próprias (`run_steam_compatdata_pass`,
  `run_steam_manifest_only_pass`, `run_pirata_manifest_only_pass`,
  `run_epic_pass`) deixaria a função navegável sem mudar comportamento —
  extração pura, mais segura que a fusão acima, pode ser feita
  independentemente dela.

## O que foi investigado e decidido **não** tocar

- Todo código defensivo com cara de "especial-caso demais" encontrado
  durante a auditoria é proteção real contra incidente documentado — ver
  lista no início deste arquivo. Nenhum é lixo.
- `vdf.rs` vs `text_vdf.rs` — não são redundantes. Formatos Valve
  genuinamente diferentes (binário `shortcuts.vdf` vs. texto KeyValues
  `config.vdf`/`localconfig.vdf`), e `text_vdf.rs` já reaproveita os tipos
  de árvore (`VdfObject`/`VdfValue`) de `vdf.rs` em vez de duplicar o
  modelo. Já está otimamente fatorado.
- `display.rs::ProgressSink` — trait com duas implementações reais
  (headless `NullSink`, GUI `ZenitySink`), as duas exercitadas. Não é
  indireção sem propósito.
- `ingest.rs::sync_full_install`'s parâmetro `_remote_known_names` — não
  influencia mais comportamento, mas o próprio comentário já documenta o
  porquê de ter ficado (os 4 chamadores já tinham o `HashSet` computado
  pra seu próprio reporte de `already_remote`). Decisão já tomada e
  justificada, prioridade baixa de mexer.
- `install_paths.rs::resolve_official_install_dir` /
  `resolve_shortcut_install_dir` (o vivo em `session.rs`/`purge.rs` via
  chamada direta) / `resolve_legendary_install_path` — três resolvers com
  o mesmo formato "override primeiro, senão lê do formato-fonte,
  verifica", mas cada um lê um formato genuinamente diferente (regex ACF /
  VDF binário / JSON). Fundir trocaria clareza por um ganho marginal de
  linhas — não vale.
- `purge.rs`/`session.rs` chamando primitivas não-validadas
  (`manifest_installdir`, `scan::find_shortcut_start_dir`) direto em vez
  das versões validadas de alto nível que `ingest.rs` usa — **isso é
  intencional, não inconsistência**: purge/session precisam do metadado
  mesmo quando o diretório não existe (jogo só no servidor no momento);
  só `ingest.rs` precisa da versão verificada. Não consolidar esses pontos
  de chamada.
- `steam_appid.rs` (45 linhas, uma função) — pequeno mas autocontido, com
  seus próprios testes de regressão com dados reais. Dobrar dentro de
  outro arquivo economizaria um arquivo, nada mais — não vale.
- `BACKLOG.md` — já é um log histórico autoconsciente ("não mantido como
  fonte de verdade, o código é a autoridade"), fora do escopo desta
  limpeza de código.
- Nenhuma dependência do `Cargo.toml` está sem uso (11 dependências,
  todas referenciadas). Nenhum `TODO`/`FIXME`/`HACK` esquecido no projeto.

## Estimativa de redução total

Camadas 1-3 (excluindo o redesenho opcional da Camada 4): ~500-700 linhas
de ~16.120 linhas totais (~3-4%) — modesto mas real, e consistente com o
que a auditoria encontrou: a cultura de "um teste de regressão por
incidente real" deste projeto foi disciplinada desde o início, então a
maior parte do que parecia "gordura" à primeira vista era na verdade
proteção justificada.
