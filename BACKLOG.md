# vault-sync — Backlog e histórico de design (Fases I a VII)

> Este arquivo é o registro histórico de desenvolvimento — decisões de
> arquitetura, o que foi tentado, por que, e o estado de cada fase no
> momento em que foi escrita. Para instalar/usar a ferramenta, veja o
> [README.md](README.md). Este documento não é mantido como fonte de
> verdade do comportamento atual — pode conter contagens de teste ou
> detalhes desatualizados; o código é sempre a autoridade final.

Implementação completa das cinco fases descritas em *Especificação
Arquitetural e Cronograma de Desenvolvimento: vault-sync*. A Fase I, por
definição da própria especificação, **não realiza nenhuma operação de
escrita em armazenamento físico** — apenas modela o domínio, constrói
caminhos e valida a CLI. A Fase II entregou o motor de transferência
(rsync/SSH, progresso, lockfile) como módulos autônomos. A Fase III conecta
tudo isso num ciclo de vida completo: o comando `run` baixa, executa o
jogo, sincroniza de volta e limpa o SSD local de fato. A Fase IV entrega a
varredura forense e a manipulação segura de `shortcuts.vdf` (comando
`scan`). A Fase V fecha o ciclo com ingestão em lote (`ingest`), remoção
remota irrevogável (`purge`) e uma unidade `systemd` para suspender o
desligamento até que as sincronizações pendentes terminem.

## O que foi entregue — Fase I

| Item da spec | Onde está |
|---|---|
| 1.1 Inicialização do repositório + dependências | `Cargo.toml` (clap, serde/serde_json, thiserror/anyhow, tracing, dirs, regex) |
| 1.2 Modelagem de domínio (`GameSource`, `GameSession`) | `src/domain.rs` |
| 1.3 Construtores de rotas absolutas (remoto e local) | `src/paths.rs` |
| 1.4 Estruturação analítica de argumentos | `src/cli.rs` (`run`, `purge`, `ingest`, `scan`) |

`src/config.rs` foi adicionado como pré-requisito prático para 1.3: sem ele
não há de onde tirar o `RemoteVault`/`LibraryLocation` reais — mas ele só
**lê** JSON, nunca escreve.

## O que foi entregue — Fase II

| Item da spec | Onde está |
|---|---|
| 2.1 Rotinas assimétricas + matriz de comando rsync | `src/rsync.rs` (`RsyncTransfer::full_download` / `::incremental_upload`, `build_args`) |
| 2.2 Captura não-bloqueante + regex de progresso | `src/progress.rs` (`spawn_progress_reader`, `parse_progress_line`) |
| 2.2 Direcionamento gráfico | `src/display.rs` (`ProgressSink`, `ZenitySink`, `NullSink` de fallback) |
| 2.3 Validação de código de saída | `src/rsync.rs` (`RsyncTransfer::execute_with_progress` → `TransferError`) |
| 2.3 Lockfile anti-concorrência | `src/lock.rs` (`LockGuard`, RAII — remove o lock mesmo em `panic`/erro) |

Decisões notáveis:

- **Cifra fixa `aes128-gcm@openssh.com`, sem `-z`**: hardcoded em
  `src/rsync.rs` conforme a spec (aceleração AES-NI, sem penalizar os
  280 MB/s do enlace DAC com compressão).
- **Direção via barra final**: origem sempre termina em `/` (copia
  conteúdo), destino nunca — evita aninhar `compatdata/<id>/<id>/`.
- **Progresso via `--info=progress2` + regex**, lido byte a byte numa
  thread separada (rsync usa `\r` para reescrever a linha quando o stdout
  não é um TTY) — a leitura nunca bloqueia o processo que a invoca.
- **zenity como backend gráfico primário**: protocolo simples via stdin
  (`N\n` atualiza a barra, `# texto\n` atualiza a legenda). Se `zenity` não
  estiver no `PATH`, `pick_sink` cai para `NullSink` — a sincronização
  nunca falha por falta de UI. kdialog não foi implementado nesta fase (a
  spec permite "zenity **ou** kdialog"); o trait `ProgressSink` já suporta
  adicionar esse backend depois sem redesenho.

## O que foi entregue — Fase III

| Item da spec | Onde está |
|---|---|
| 3.1 Interceptação da matriz de comando + propagação de ambiente | `src/cli.rs` (`Run { exec: Vec<String> }` via `trailing_var_arg`) + `src/session.rs` |
| 3.2 Retenção sistêmica de execução (Zero-Overhead) | `src/session.rs` (`child.wait()`) |
| 3.3 Retomada + sincronização diferencial | `src/session.rs` (upload incremental restrito ao `compatdata_dir`) |
| 3.4 Exclusão categórica pós-sync | `src/session.rs` (`cleanup_local`, só roda se o upload teve exit 0) |

Decisões notáveis:

- **`run <app_id> -- <comando do jogo...>`**: o `--` captura a matriz de
  comando exatamente como a Steam a injetaria via `%command%` nas opções de
  lançamento. Tudo após `--` (inclusive flags com `-`) vai literalmente
  para o processo filho.
- **Ambiente herdado por padrão**: `std::process::Command` já herda todo o
  ambiente do processo pai (DISPLAY/WAYLAND_DISPLAY/XAUTHORITY etc.) e
  stdin/stdout/stderr sem nenhuma configuração extra — testado em
  `session.rs` (`game_process_inherits_parent_environment`).
- **`.wait()` sem busy-loop**: bloqueia a thread via `waitpid`, custo de
  CPU zero até o kernel notificar o término do processo filho — é
  literalmente a chamada usada, sem polling.
- **Ordem estrita e cancelável em qualquer etapa**: download falha →
  jogo nunca inicia; jogo termina com erro/sinal → sincroniza mesmo assim
  (dados de progresso importam mais que o código de saída do jogo); upload
  falha → limpeza local **não** ocorre (evita perda de dados ainda não
  confirmados no FeuVault). As quatro combinações têm teste dedicado em
  `session.rs`.
- **Detecção de `GameSource` ainda não existe**: todo `app_id` numérico
  passado a `run` é tratado como `Official` e a primeira biblioteca de
  `local_libraries` é usada como destino. A classificação real
  (oficial/non-Steam/órfão via varredura do sistema de arquivos) é a
  própria Fase IV — não faz sentido antecipá-la aqui.
- **Testes de orquestração sem SSH/GUI reais**: `session.rs` injeta um
  script de shell falso no lugar do binário `rsync` (mesmo mecanismo de
  `RsyncTransfer::with_binary` da Fase II) e usa `/bin/sh` como "jogo" —
  cobre lock concorrente, falha de download, falha de upload e o caminho
  feliz completo, sem depender de rede ou de uma sessão gráfica.

`cargo test` → **41/41 passando**. `cargo clippy --all-targets` → limpo.

## O que foi entregue — Fase IV

| Item da spec | Onde está |
|---|---|
| 4.1 Varredura forense do sistema de arquivos (port de `steam_scan.py`) | `src/scan.rs` (`run_scan`, `investigate_orphan`) |
| 4.2 Preservação pregressa antes de mutações + codec binário | `src/backup.rs` (`atomic_backup`), `src/vdf.rs` (`parse_binary`/`write_binary`) |
| 4.3 Motor de injeção autônoma do marcador do orquestrador | `src/inject.rs` (`inject_shortcuts_file`, `inject_all_shortcuts`) |

Decisões notáveis:

- **VDF binário decodificado numa árvore genérica**, não via regex sobre os
  bytes como o script Python original — pré-requisito para o motor de
  injeção conseguir reescrever o arquivo inteiro alterando só o campo
  `LaunchOptions`, sem nunca perder um campo desconhecido do resto da
  árvore. Ordem dos campos preservada com `Vec<(String, VdfValue)>`, não
  `HashMap`, porque a Steam é sensível à ordem ao reler o arquivo.
- **Backup atômico obrigatório antes de qualquer escrita** em
  `shortcuts.vdf`: grava num `.tmp` no mesmo diretório e só promove via
  `rename` — mitigação direta do risco "degradação catastrófica de
  arquivos restritos binários" da spec.
- **Consulta online à Steam Store API deliberadamente omitida** de
  `investigate_orphan` (presente no `steam_scan.py` original): introduziria
  I/O de rede não-determinístico numa varredura que deve ser rápida e
  reproduzível. As pistas offline (registro do Wine, atalhos, pastas de
  save) continuam intactas.
- **`run_scan` usa `cfg.local_libraries`** em vez dos caminhos pessoais
  hardcoded do script original.
- **Incidente de segurança durante o desenvolvimento**: um smoke test
  manual do comando `scan` rodou, por descuido, contra a instalação Steam
  real em vez de um diretório de teste isolado, injetando o marcador em 20
  shortcuts reais. Os backups atômicos criados pelo próprio código
  permitiram reverter a mutação sem perda de dados. Lição aplicada a todo
  o resto do projeto: qualquer teste manual que toque `shortcuts.vdf` ou o
  FeuVault real precisa rodar isolado (diretório temporário e/ou binário
  `ssh`/`rsync` falso), nunca contra o ambiente de produção do usuário.

## O que foi entregue — Fase V

| Item da spec | Onde está |
|---|---|
| 5.1 Ingestão em lote (local → FeuVault) | `src/ingest.rs` (`run_ingest`) |
| 5.2 Suspensão operacional via unidade de encerramento | `systemd/vault-sync-flush.service` (template, instalação manual) |
| 5.3 Remoção remota irrevogável + limpeza de evidência local | `src/purge.rs` (`run_purge`) |
| Primitivas SSH cruas (listar/remover) compartilhadas por 5.1/5.3 | `src/remote.rs` |

Decisões notáveis:

- **`ingest` compara conjuntos, não faz diffs por arquivo**: uma única
  consulta SSH (`ls -1` no `compatdata/` remoto) traz os app_ids já
  presentes no FeuVault; qualquer `compatdata/<app_id>` local ausente
  dessa lista é enviado via `RsyncTransfer::incremental_upload`. Simples e
  suficiente para o requisito da spec ("divergências positivas
  localmente"), sem precisar comparar timestamps/hashes.
- **`purge` é estritamente aditivo em validação, nunca em confiança**: o
  `app_id` passa pela mesma barreira de `paths.rs` (`validated_app_id`) que
  `run`/`ingest` já usam, então nenhuma remoção — local ou remota — roda
  sobre um caminho não estritamente numérico. A ordem é remoto → local →
  `shortcuts.vdf`, e cada etapa só roda se a anterior teve sucesso.
- **Limpeza de `shortcuts.vdf` reaproveita o mesmo padrão não-destrutivo do
  motor de injeção (4.3)**: parseia a árvore inteira, remove só a entrada
  cujo `appid` bate, faz backup atômico antes de reescrever, e só toca no
  arquivo se algo de fato mudou — evita a "evidência remanescente" que a
  spec pede para eliminar (jogo removido do FeuVault que continuaria
  aparecendo na UI da Steam).
- **`GameSource::Orphan` como veículo de caminho**: nem `ingest` nem
  `purge` classificam a origem do jogo — só precisam dos mesmos
  `compatdata_dir`/`manifest_file` que `paths.rs` já sabe construir. Reusar
  a variante `Orphan` (que nunca inclui manifesto) evita duplicar essa
  lógica de junção de caminhos num terceiro lugar.
- **Confirmação interativa de `purge` fica em `main.rs`, não em
  `purge.rs`**: a função `run_purge` é pura E/S de arquivo/rede, sem
  `stdin`, o que a mantém testável sem simular um terminal. Sem `--yes`, a
  CLI pede a palavra "sim" digitada antes de prosseguir.
- **`ingest --background` não faz fork/daemonização real**: só troca
  `println!` por `tracing::info!` (sem diálogo interativo). Backgrounding
  de verdade é responsabilidade do shell (`&`) ou do systemd (unidade
  abaixo) — implementar duplo-fork em Rust seria engenharia desproporcional
  ao que a spec pede.
- **Unidade systemd é um template, não uma instalação**: `ExecStart=/bin/true`
  com `RemainAfterExit=yes` mantém a unit "ativa" sem custo; todo o
  trabalho roda em `ExecStop=vault-sync ingest --background`, disparado
  automaticamente quando o systemd começa a processar
  `shutdown.target`/`reboot.target`/`halt.target` (por causa do `Before=`).
  `TimeoutStopSec=120` é obrigatório e finito — mitigação direta do risco
  "obstrução perene dos procedimentos normativos de interrupção elétrica"
  da spec: uma falha de rede trava o desligamento por no máximo esse
  tempo, nunca para sempre. Este binário nunca instala/habilita a unidade
  sozinho; ficou documentado no arquivo como um passo manual do usuário.

`cargo test` → **73/73 passando** (`--test-threads=1`; em paralelo, a
criação/execução simultânea de muitos scripts de shell falsos nos testes
ocasionalmente produz um `ETXTBSY` transiente do kernel — não é um bug do
código, só uma corrida de I/O do próprio ambiente de teste).
`cargo clippy --all-targets` → limpo. `cargo fmt --check` → limpo.

## Robustez adicional (pós-Fase V): `doctor` e hardening de rede

Com as cinco fases implementadas, uma lacuna concreta permanecia: nenhum
comando validava, *antes* de agir, se os caminhos configurados para cada
máquina (bibliotecas locais no Rafeu-PC, `remote_base_path` no FeuVault) e a
conectividade SSH estavam realmente prontos. `run`/`ingest`/`purge` só
descobriam um problema no meio de uma transferência — ou, no caso de
`purge`, depois de já ter começado uma remoção irrevogável.

| Melhoria | Onde está |
|---|---|
| Diagnóstico pré-voo sem mutação | `src/doctor.rs` (`run_doctor`, `DoctorReport::has_hard_failures`) + comando `vault-sync doctor` |
| Timeout finito + auth não-interativa em toda chamada SSH | `src/remote.rs` (`ssh_command`), `src/rsync.rs` (`SSH_TRANSPORT_OPTIONS`) |
| Criação automática de diretórios remotos ausentes | `src/rsync.rs` (`--mkpath` em `RsyncTransfer::build_args`) |
| Detecção de ponto de montagem "fantasma" (local e remoto) | `src/paths.rs` (`is_mount_point`), `src/remote.rs` (`check_remote_path`) |
| Wake automático do FeuVault antes de `run`/`ingest`/`purge` | `src/remote.rs` (`ensure_reachable`), chamado em `src/main.rs` |

Decisões notáveis:

- **`vault-sync doctor` é só leitura**: confere se cada `local_libraries`
  existe, se o alvo SSH responde e se `remote_base_path` existe do outro
  lado — sem criar, apagar ou transferir nada. Falhas duras (biblioteca
  local ausente, SSH inalcançável, `remote_base_path` ausente) fazem o
  comando sair com código não-zero; um caminho que existe mas não é um
  ponto de montagem separado é reportado como aviso, não como falha — um
  SSD "efêmero" configurado como partição única continua sendo uma
  configuração válida.
- **Por que mount-point é só aviso, nunca um gate em `run`/`ingest`/`purge`**:
  o caso perigoso de verdade é o diretório existir (foi criado com `mkdir`
  como alvo do mount) mas o disco não estar montado ali — nesse estado ele
  aparece vazio e gravável normalmente, e dados acabam silenciosamente
  gravados na partição raiz em vez do SSD/HDD esperado. Mas exigir
  ponto-de-montagem à força quebraria qualquer configuração legítima de
  disco único, então a checagem existe só em `doctor`, nunca como barreira
  nos comandos que de fato escrevem.
- **`ConnectTimeout=10` + `BatchMode=yes` em toda invocação de `ssh`/rsync**:
  sem timeout finito, um FeuVault desligado/inacessível travaria
  `run`/`ingest` indefinidamente — inclusive o `ExecStop` do systemd da
  Fase V, que tem um `TimeoutStopSec` mas ainda dependeria do `ssh`
  desistir sozinho. `BatchMode=yes` desliga qualquer prompt interativo de
  senha, coerente com uma ferramenta que só deve operar com autenticação
  por chave já configurada.
- **`--mkpath` corrige um bug real de bootstrap**: sem ele, o primeiro
  `ingest`/`run` de sempre contra um FeuVault recém-provisionado (antes de
  `steamapps/compatdata/` existir remotamente) falhava com "No such file or
  directory" em vez de criar a árvore de diretórios automaticamente.
- **Wake automático via `turn-on FeuVault`**: `run`/`ingest`/`purge` chamam
  `remote::ensure_reachable` antes de qualquer operação real; se o SSH
  falhar, ela invoca o comando externo `turn-on FeuVault` (já existente no
  ecossistema do usuário, fora do escopo deste projeto) e faz polling da
  conectividade por até ~2 minutos antes de desistir — tempo típico de boot
  de um servidor doméstico. `doctor` continua deliberadamente só-leitura e
  não chama isso; ao encontrar o FeuVault inalcançável, apenas sugere o
  comando na mensagem.

`cargo test` → **89/89 passando** (`--test-threads=1`). `cargo clippy
--all-targets` → limpo. `cargo fmt --check` → limpo.

## Fase VI: suporte a Heroic (Epic)

Até aqui, `run`/`purge`/`ingest`/`doctor` só conheciam a Steam. A Fase VI
estende o mesmo ciclo de vida (aquisição → execução → sincronização
diferencial → limpeza/remoção) a jogos Epic rodados via Heroic Games
Launcher/Legendary sobre Wine, sem duplicar a lógica existente — só um novo
`GameSource` e um novo par de campos de configuração.

| Item | Onde está |
|---|---|
| `GameSource::HeroicGame { title }` | `src/domain.rs` |
| `Launcher` (`Steam`/`Heroic`) — seleção explícita via `--steam`/`--heroic` | `src/domain.rs`, `src/cli.rs` |
| Validação de título contra path traversal (`validated_heroic_title`) | `src/paths.rs` |
| Construção de caminhos local/remoto para Heroic (`RemoteVault::paths_for`, `LibraryLocation::deletion_targets`) | `src/paths.rs` |
| `heroic_local_root`/`heroic_remote_base_path` (opcionais) | `src/config.rs` |
| `run`/`purge` despacham por `Launcher` | `src/session.rs`, `src/purge.rs` |
| `ingest` varre e envia prefixos Heroic novos, além do `compatdata` Steam | `src/ingest.rs` |
| `doctor` confere `heroic_local_root`/`heroic_remote_base_path` quando configurados | `src/doctor.rs` |

Decisões notáveis:

- **Prefixo Wine inteiro como unidade de sincronização, não só as saves**:
  o `installed.json` do Legendary (backend do Heroic) tem `save_path: null`
  em vários títulos — não há um caminho de save confiável e universal para
  todo jogo Epic. A mesma estratégia já usada para `compatdata/<app_id>` da
  Steam (o prefixo inteiro, não um subdiretório) se aplica aqui, mapeada
  para `~/Games/Heroic/Prefixes/default/<título>`.
- **Título da pasta como identificador, não um app_id numérico**: ao
  contrário da Steam, o Heroic não expõe um ID estável e curto — o nome da
  pasta em `Prefixes/default/` (idêntico ao campo `title` do
  `installed.json`) é a única chave disponível sem depender do backend do
  Legendary em tempo de execução.
- **`--steam`/`--heroic` são duas flags booleanas distintas, não um único
  flag `--launcher steam|heroic`**: decisão deliberada para manter cada
  método de seleção explícito e symmetric na CLI. Um `ArgGroup` exige
  exatamente uma das duas em `run` e `purge`.
- **`heroic_local_root`/`heroic_remote_base_path` são campos separados de
  `local_libraries`/`remote_base_path`, nunca reaproveitados**: os dois
  namespaces (Steam e Heroic) não compartilham estrutura de diretório
  (`steamapps/compatdata/<id>` vs. `<título>` direto), então misturá-los
  arriscaria colisão de nomes ou uma junção de caminho incorreta. Ambos os
  campos são opcionais (`#[serde(default)]`) — configs Steam-only
  existentes continuam válidas sem alteração.
- **`Launcher` nunca é inferido do valor de `app_id`**: um título Heroic
  poderia coincidentemente ser só dígitos (ex.: um jogo chamado "7"), então
  a seleção do launcher é sempre explícita via `--steam`/`--heroic` — nunca
  adivinhada a partir do formato do identificador.
- **Heroic nunca é registrado em `shortcuts.vdf`**: a limpeza de "evidência
  remanescente" na Steam UI (`purge`, 5.3) roda exclusivamente quando
  `launcher == Launcher::Steam` — jogos Heroic não têm shortcut Steam
  correspondente para limpar.
- **`ingest` reaproveita a mesma comparação por conjunto** (5.1) para
  Heroic: uma consulta SSH lista os títulos já presentes sob
  `heroic_remote_base_path`, e qualquer subpasta de `heroic_local_root`
  ausente dessa lista é enviada — mesma lógica do `compatdata` Steam, só
  trocando app_id numérico por nome de pasta.
- **`doctor` só reporta Heroic quando ambos os campos estão configurados**:
  se `heroic_local_root`/`heroic_remote_base_path` forem `None` (config
  Steam-only), a seção "Heroic" é omitida do relatório inteiramente — não
  aparece como falha, só não é verificada.

`cargo test` → **103/103 passando** (`--test-threads=1`; ver nota sobre
`ETXTBSY` transiente na Fase V). `cargo clippy --all-targets` → limpo.

## Fase VII: instalação completa (Steam oficial, Steam pirata, Epic/Heroic)

Até aqui, tudo o que o `vault-sync` sincroniza é o `compatdata`/prefixo
(saves, registro do Windows, config de engine) — nunca a instalação em si
(o grosso do espaço em disco, às vezes 100+ GB por jogo). A Fase VII
estende o mesmo ciclo (baixar → jogar → subir → apagar) pra instalação
completa dos três tipos de jogo, permitindo de fato esvaziar os discos
locais entre sessões, não só os saves.

**Descoberta que moldou o design**: investigando como localizar "onde a
instalação de um jogo realmente está", ficou claro que nenhuma fonte de
metadado é confiável sem verificação — `libraryfolders.vdf` da Steam,
`shortcuts.vdf` de atalhos non-Steam e `installed.json` do Legendary
(Heroic) acumulam caminhos obsoletos com o tempo (discos renomeados, HDs
externos remontados). Um caso real apareceu *durante o próprio
desenvolvimento*: a instalação do Disco Elysium mudou de disco no meio da
sessão de trabalho sem o Legendary saber. Por isso, `src/install_paths.rs`
nunca inventa ou confia cegamente num caminho gravado — só usa como
candidato, sempre verificado com `Path::is_dir()` antes de qualquer ação.

| Item | Onde está |
|---|---|
| Resolução verificada de instalação (3 fontes distintas) | `src/install_paths.rs` |
| `local_game_root_steam`/`_piratas`/`_epic`, `remote_installs_base_path` (config) | `src/config.rs` |
| `ingest --full` (adoção em lote, sem precisar jogar) + `--and-purge-local` | `src/ingest.rs` |
| `list` (o que já existe no FeuVault, compatdata + prefixos) | `src/list.rs` |
| `run`/`purge` estendidos pra instalação completa | `src/session.rs`, `src/purge.rs` |

Decisões notáveis:

- **Três fontes de resolução, uma por tipo, todas com o mesmo princípio de
  segurança**: `resolve_official_install_dir` lê `installdir` do
  `appmanifest_<id>.acf` (nunca consulta o `libraryfolders.vdf` global —
  essa é justamente a fonte cheia de entradas obsoletas);
  `resolve_shortcut_install_dir` lê `StartDir` do `shortcuts.vdf`;
  `resolve_legendary_install_path` lê `install_path` do `installed.json`.
  Todas as três só devolvem `Some` quando o caminho existe de verdade no
  disco agora.
- **`ingest --full` é a via "eu instalei, quero migrar sem jogar"**:
  reaproveita a mesma comparação por conjunto do `ingest` normal, só que
  também resolvendo/enviando a instalação completa. Jogos cujo caminho não
  resolve são listados como pulados (nunca erro fatal do comando inteiro)
  — esperado principalmente pra piratas, onde `shortcuts.vdf` está
  frequentemente desatualizado por razões alheias ao `vault-sync`.
- **Migração é opt-in e gradual, por design do usuário**: em vez de mover
  tudo de uma vez (risco real com instalações de 100+ GB), o fluxo adotado
  é jogo a jogo — instalar fresco no local final (`~/Games/{steam,epic}`,
  requer registrar `~/Games/steam` como Library Folder na Steam pra jogos
  oficiais — passo manual, o `vault-sync` nunca edita `libraryfolders.vdf`)
  ou simplesmente continuar usando o `StartDir` já existente (piratas, sem
  nenhuma migração de fato necessária), e deixar `run`/`ingest --full`
  levarem o resto.
- **O manifesto Steam oficial nunca é apagado por `run` uma vez rastreado**:
  mudança deliberada em relação ao comportamento pré-Fase VII (que sempre
  apagava `appmanifest_<id>.acf` no fim da sessão). Sem isso, depois que a
  instalação completa também é apagada, ninguém mais saberia o `installdir`
  pra restaurar na próxima sessão — nem a Steam, nem o `vault-sync`. Efeito
  colateral aceito conscientemente: a Steam continua "achando" o jogo
  instalado entre sessões mesmo com `steamapps/common/` vazio — inofensivo
  porque `run` é quem intercepta o "Play" antes do jogo abrir de verdade,
  não depende da Steam achar que os arquivos estão íntegros. Sem
  rastreamento configurado, o comportamento é idêntico ao pré-Fase VII.
- **Epic/Heroic nunca restaura via `installed.json`**: dado que essa fonte
  já provou ficar obsoleta sozinha (o próprio caso do Disco Elysium), o
  caminho canônico de um jogo Epic rastreado é sempre
  `local_game_root_epic/<título>` — fixo, nunca dependente do que o
  Legendary acha que é verdade.
- **Piratas nunca precisa que o `vault-sync` reescreva `shortcuts.vdf`**:
  ao contrário de oficiais/Epic, o `StartDir` já gravado no atalho é
  suficiente como alvo estável de download/upload — `run`/`purge` sempre
  restauram exatamente onde o atalho já aponta, sem mover nada.
- **`purge` remove a instalação completa em melhor esforço**: só quando
  consegue resolver de onde ela vem (mesmas três fontes acima, ou o
  caminho fixo do Epic) — não resolver não é erro, só significa que não
  havia nada de instalação completa rastreada além do compatdata já
  removido.

**Não testado ao vivo ainda**: o efeito prático de manter o manifesto
Steam sempre presente (a Steam pode ou não oferecer "verificar
integridade" nesse estado — nunca confirmado contra o cliente real). A
migração real de qualquer jogo (mesmo pequeno) ainda não foi feita em
produção — só validado com fixtures/dados sintéticos e contra o estado real
do `installed.json`/`shortcuts.vdf` desta máquina (só leitura).

`cargo test` → **135/135 passando** (`--test-threads=1`). `cargo clippy
--all-targets` → limpo. `cargo fmt --check` → limpo.

## Proteções de fim de sessão e de download (The Sims 4, 2026-10-24)

Investigando o prefixo do The Sims 4 (AppID 1222670, EA app dentro do
Proton) que voltou quebrado no segundo download, apareceram dois riscos
pra saves, que não têm como ser recuperados:

| Risco | Correção | Onde está |
|---|---|---|
| Fim de sessão envia e apaga com o prefixo Wine ainda vivo (serviços do EA app, wineserver ainda sem gravar o registro) | Espera até nenhum processo ter `WINEPREFIX` apontando pro prefixo (180s, log a cada 30s), antes do envio e de novo antes da limpeza; estourou → `RunError::PrefixStillActive`, nada apagado | `src/session.rs` (`wait_for_prefix_quiet`, `prefix_users_in`, chamado em `upload_and_cleanup`) |
| `ingest` envia/apaga (`--and-purge-local`) um prefixo vivo | Item pulado com aviso, sem espera | `src/ingest.rs` (`prefix_in_use`) |
| Download (`rsync -a`) substitui arquivo local mais novo sem cópia | `--backup --backup-dir=~/.local/state/vault-sync/overwritten/<nome>-<unix>` em todo `full_download` (run, pull, stage); sem diretório seguro, o download é recusado | `src/rsync.rs` (`backup_dir_for`, `build_args`, `execute_with_progress`) |

Decisões notáveis:

- **Varredura de `/proc` por `WINEPREFIX`, não `wineserver -w`**: o
  `wineserver -w` espera o servidor certo, mas exige achar o binário do
  Proton que a Steam escolheu (o vault-sync não sabe qual é pra jogo
  oficial) e não tem timeout. Todo processo Wine — inclusive o wineserver,
  que só encerra depois de gravar o registro — tem `WINEPREFIX` no
  ambiente, então "nenhum processo com esse `WINEPREFIX`" equivale a
  "wineserver encerrado". A comparação é por (dispositivo, inode), o mesmo
  critério do wineserver, e resolve pelo `/proc/<pid>/root` do processo
  (contêiner da Steam Linux Runtime) antes do caminho do host.
- **Timeout não envia**: o envio de um prefixo vivo é justamente o retrato
  inconsistente que a espera evita. A mensagem manda fechar os processos e
  rodar `vault-sync ingest --only <id>`, que reenvia o compatdata local
  (o `ingest` nunca pula o envio de um item já remoto, ver incidente do
  Kenshi acima).
- **`ingest` não espera**: o `ingest` do desligamento
  (`vault-sync-flush.service`) tem só `TimeoutStopSec=120`.
- **`purge` ficou de fora**: é remoção irrevogável e confirmada dos dois
  lados; não existe retrato a proteger.

Pendências conhecidas:

- **Poda de `overwritten/`**: não há. Esses diretórios só aparecem depois
  de uma sessão anormal (envio que falhou, timeout acima), justamente
  quando a cópia guardada pode ser o save mais novo que existe — apagar
  sozinho os mais antigos contraria a prioridade de nunca perder save.
  Candidatos se crescer demais: poda por idade (ex.: 90 dias) ou um
  comando explícito que liste e apague.
- **Não existe "terminar a sessão pendente"**: depois de um
  `PrefixStillActive` (ou de qualquer falha de envio), se o usuário rodar o
  jogo de novo antes do `ingest --only`, o download traz a cópia do
  servidor por cima da local — agora com backup em `overwritten/`, mas o
  jogo abre com o estado antigo até alguém restaurar.
- **Envio sem `--delete` acumula lixo no servidor**: quando o EA app troca
  um diretório por symlink (ou o contrário) ao se atualizar, o rsync sem
  `--delete` não consegue propagar ("cannot delete non-empty directory",
  código 23) e arquivos renomeados localmente continuam com o nome antigo
  no servidor. Possível causa do prefixo quebrado; não tratado aqui
  (`--delete` apagaria dados do servidor e merece decisão própria).

## Sobre `GameSource`

Replica 1:1 a heurística já validada em `steam_scan.py`:

- `Official { app_id }` — tem `appmanifest_<id>.acf` numa biblioteca Steam.
- `NonSteamShortcut { app_id, shortcut_name }` — está em `shortcuts.vdf`
  (jogos non-Steam: GOG, homebrew, launchers de terceiros).
- `Orphan { app_id, clues }` — prefixo `compatdata` sem manifesto nem
  shortcut; origem só reconstruída via investigação forense.
- `HeroicGame { title }` — prefixo Wine do Heroic Games Launcher (jogos
  Epic via Legendary), identificado pelo nome da pasta em
  `Prefixes/default/<título>` (Fase VI).

## O ponto crítico: `paths.rs`

A spec exige "testes unitários exaustivos para evitar a deleção indevida de
partições sistêmicas essenciais". A abordagem adotada:

- Todo `app_id` é validado como **estritamente numérico** antes de qualquer
  `PathBuf` ser montado (mesma garantia que `steam_scan.py` já assume com
  `appid.isdigit()`). Isso elimina `..`, `/`, `.` etc. na raiz, sem precisar
  canonicalizar caminhos.
- `LibraryLocation::deletion_targets` nunca retorna `<root>` nem o diretório
  `compatdata` compartilhado — só `<root>/steamapps/compatdata/<id>`.
- 9 testes em `paths.rs` cobrem: app_id vazio, só espaços, tentativas de
  path traversal, target sempre aninhado sob a raiz da biblioteca, e target
  nunca igual ao `compatdata` compartilhado.

## Configuração inicial

`run`, `ingest`, `purge` e `doctor` exigem um `config.json` válido em
`~/.config/vault-sync/config.json` (ou `--config <caminho>`). Copie o
template e ajuste os três campos para a topologia real das suas máquinas:

```bash
mkdir -p ~/.config/vault-sync
cp config.example.json ~/.config/vault-sync/config.json
```

```json
{
  "remote_ssh_target": "feuvault",
  "remote_base_path": "/mnt/vault/steam-library",
  "local_libraries": [
    "/mnt/SSD2/SteamLibrary",
    "/mnt/HDD1/SteamLibrary"
  ],
  "heroic_local_root": "/home/rafeu/Games/Heroic/Prefixes/default",
  "heroic_remote_base_path": "/mnt/vault/heroic-prefixes",
  "local_game_root_steam": "/home/rafeu/Games/steam",
  "local_game_root_piratas": "/home/rafeu/Games/piratas",
  "local_game_root_epic": "/home/rafeu/Games/epic",
  "remote_installs_base_path": "/mnt/vault/game-installs"
}
```

- `remote_ssh_target` **não** é um IP nem um hostname direto — é um alias
  resolvido pelo próprio `~/.ssh/config` do usuário. `vault-sync` nunca
  hardcoda endereços internos; quem sabe o IP/porta/usuário/chave do
  FeuVault é o SSH, não este projeto. Exemplo de entrada correspondente em
  `~/.ssh/config`:

  ```
  Host feuvault
      HostName 192.168.1.80
      User rafeu
      IdentityFile ~/.ssh/id_ed25519
  ```

- `remote_base_path` é o diretório raiz da "biblioteca espelho" no
  FeuVault (equivalente remoto de `local_libraries`).
- `local_libraries` lista as bibliotecas Steam locais no Rafeu-PC, na ordem
  de preferência — a primeira é o destino de `run`.
- `heroic_local_root`/`heroic_remote_base_path` (opcionais, Fase VI):
  respectivamente a raiz local dos prefixos Heroic
  (`Prefixes/default`) e o diretório-espelho correspondente no FeuVault.
  Omita os dois campos se você não usa Heroic — `run --heroic`/`purge
  --heroic` falham cedo com uma mensagem explícita nesse caso, e `ingest`
  simplesmente pula a varredura Heroic.
- `local_game_root_steam`/`_piratas`/`_epic` e `remote_installs_base_path`
  (opcionais, Fase VII): raízes locais (uma por tipo) e a raiz remota única
  onde a *instalação completa* dos jogos (não só o `compatdata`) é
  rastreada. Omita todos se você só quer sincronizar saves/prefixos como
  nas Fases I-VI — nenhum comando exige esses campos, e `run`/`ingest
  --full`/`purge` simplesmente não rastreiam instalação completa sem eles.
  `local_game_root_steam` só funciona de fato depois de registrado como
  Library Folder no cliente Steam (passo manual, fora do escopo deste
  projeto).

Depois de configurar, rode `vault-sync doctor` **antes** de qualquer outro
comando: ele confere se cada biblioteca local existe, se o alvo SSH
responde e se `remote_base_path` existe do outro lado, sem mutar nada. Ver
seção "Robustez adicional" abaixo para detalhes.

## Rodando

```bash
cargo build
cargo test -- --test-threads=1
cargo clippy --all-targets
cargo run -- --help
cargo run -- doctor                                                    # diagnóstico pré-voo, só leitura
cargo run -- run 730 --steam -- /caminho/do/jogo --alguma-flag        # ciclo de vida completo (Fase III)
cargo run -- run "Hogwarts Legacy" --heroic -- /caminho/do/jogo       # idem, jogo Epic via Heroic (Fase VI)
cargo run -- scan --output steam_catalog.json                         # varredura + injeção (Fase IV)
cargo run -- ingest [--background]                                    # envia jogos novos ao FeuVault (Fase V + VI)
cargo run -- ingest --full [--and-purge-local]                        # idem, + instalação completa (Fase VII)
cargo run -- purge <app_id> --steam [--yes]                            # remoção remota irrevogável (Fase V)
cargo run -- purge <título> --heroic [--yes]                           # idem, jogo Epic via Heroic (Fase VI)
cargo run -- list                                                      # o que já existe no FeuVault (Fase VII)
```

`--steam`/`--heroic` são mutuamente exclusivos e um dos dois é obrigatório
em `run`/`purge` — decide tanto o formato esperado do identificador
(AppID numérico vs. título de pasta) quanto qual par de campos de
configuração (`local_libraries`/`remote_base_path` vs.
`heroic_local_root`/`heroic_remote_base_path`) é usado.

`run`, `ingest`, `purge` e `doctor` exigem um `config.json` válido
(`~/.config/vault-sync/config.json` ou `--config`) com `remote_ssh_target`,
`remote_base_path` e pelo menos um `local_libraries`. Sem isso, falham cedo
com uma mensagem explícita em vez de tentar adivinhar.

`scan` não exige config — funciona com as bibliotecas detectadas
automaticamente via `libraryfolders.vdf`, e usa `local_libraries` (se
houver config) só como bibliotecas extras.

## Estado dos itens da spec

Todas as cinco fases da especificação estão implementadas. O único item
deliberadamente fora de escopo é a consulta online à Steam Store API do
`steam_scan.py` original (ver seção da Fase IV) — decisão documentada, não
uma pendência.

`doctor`, o hardening de SSH/rsync e o `config.example.json` (seção
"Robustez adicional" acima) não fazem parte da spec original — são
correções de lacunas descobertas na revisão pós-Fase V: nenhum comando
validava pré-requisitos de rede/caminho antes de agir, e a topologia de
cada máquina não tinha um template documentado.

A Fase VI (suporte a Heroic/Epic) também é uma extensão pós-spec original,
que tratava exclusivamente da Steam — motivada pela necessidade real de
sincronizar jogos Epic rodados via Heroic Games Launcher com a mesma
garantia de ciclo de vida que a Steam já tinha.

A Fase VII (instalação completa) estende o mesmo ciclo de vida da
Fase VI/original — que sempre tratou só do `compatdata`/prefixo — pra
instalação inteira do jogo, motivada pela necessidade real de liberar
espaço de disco de fato, não só o pequeno espaço ocupado por saves.

## Implantação no Rafeu-PC: binário, fish e systemd (boot/poweroff)

Além de `cargo run --`, o `vault-sync` está instalado como comando real no
Rafeu-PC, com automação de boot/poweroff via `systemd`. Três peças:

| Peça | Onde está | Papel |
|---|---|---|
| Binário release | `/usr/local/bin/vault-sync` (`sudo install -m755 target/release/vault-sync /usr/local/bin/vault-sync`) | Usado tanto pelo terminal interativo quanto pelos units do systemd |
| Wrapper fish | `~/.config/fish/functions/vault-sync.fish` | Sombra o binário homônimo: garante a chave SSH interativa carregada via `keychain` antes de delegar pro binário real (`command vault-sync $argv`) |
| Chave de automação | `~/.ssh/vault_sync_ed25519` (sem passphrase, `IdentityFile` extra em `Host meuserver`) | Usada só pelos units do systemd (boot/poweroff), que rodam sem terminal/agente de sessão nenhum |
| `systemd/vault-sync-flush.service` | Ativa no poweroff/reboot | `ExecStop` roda `ingest --background` antes do desligamento completar (Fase V, `TimeoutStopSec=120`) |
| `systemd/vault-sync-doctor-boot.service` | Ativa no boot | Roda `doctor` uma vez, só-leitura, registra no journal — nunca bloqueia o boot |

Decisões notáveis:

- **Duas chaves SSH para dois contextos, nunca uma só**: o terminal
  interativo usa a chave pessoal (`id_ed25519`, com passphrase, carregada
  via `keychain`/agente); os units do systemd usam uma chave dedicada sem
  passphrase (`vault_sync_ed25519`) — não há como digitar senha num
  `ExecStop`/`ExecStart` de systemd, e depender do agente da sessão
  interativa seria frágil (ele pode já estar morto no boot, ou caindo junto
  com a sessão gráfica no shutdown). Ambas resolvem através do mesmo `Host
  meuserver` em `~/.ssh/config` (duas `IdentityFile`) — o `ssh`/`rsync` do
  `vault-sync` já roda com `BatchMode=yes`, então cada contexto
  naturalmente cai na chave que funciona sem prompt, sem nenhuma mudança de
  código.
- **O agente do keychain morre a cada boot/nova sessão**: `keychain
  add`/`--eval` não suporta `--shell fish` diretamente (só a ação `env`
  suporta) — por isso o wrapper faz duas chamadas: uma carrega a chave
  (`keychain --quiet id_ed25519`), outra emite o env já formatado
  (`keychain env --shell fish --quiet | source`). Ver memória
  `project_ssh_agent_keychain` para o histórico desse problema.
- **`doctor` no boot, nunca `ingest`/`run` no boot**: só-leitura por design
  (mesma garantia do comando em si) — evita qualquer mutação de dados
  logo cedo, antes até do usuário logar. O poweroff, ao contrário, *precisa*
  mutar (subir o que sobrou local pro FeuVault), por isso usa `ingest`.
- Nenhum dos dois units é instalado/habilitado automaticamente pelo
  `vault-sync` — são artefatos manuais (`sudo cp` + `daemon-reload` +
  `enable`), documentados no cabeçalho de cada `.service`.
