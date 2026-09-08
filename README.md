# vault-sync

Mantém o SSD/NVMe local como zona de alocação efêmera para jogos,
sincronizando com um servidor remoto via SSH/rsync — Steam (oficial e
non-Steam/pirata) e Epic (via Heroic Games Launcher). Baixa um jogo pra
jogar, sobe de volta ao fechar, e libera o espaço local.

Ferramenta universal: nenhum nome de servidor/máquina fica cravado no
código. Nas mensagens, "Servidor" e "Cliente" são os nomes padrão até você
configurar os seus — ver [Nome de exibição](#nome-de-exibição-do-servidor-e-da-máquina-local).

> Histórico de design, decisões de arquitetura e o raciocínio por trás de
> cada fase estão em [BACKLOG.md](BACKLOG.md) — este README documenta só
> instalação e uso.

## Requisitos

- Rust (`cargo build --release`)
- `ssh`/`rsync` no PATH, com autenticação por chave já configurada pro
  servidor remoto (`~/.ssh/config`)
- Fish shell (opcional, só pro wrapper de conveniência) e `keychain`
  (opcional, só se o terminal interativo usar chave com passphrase)
- `systemd` (opcional, só pra automação de boot/poweroff)

## Instalação

```bash
cargo build --release
sudo install -m755 target/release/vault-sync /usr/local/bin/vault-sync
```

### Wrapper fish (opcional, recomendado se você usa fish)

Se a chave SSH pro servidor remoto tem passphrase, crie
`~/.config/fish/functions/vault-sync.fish`:

```fish
function vault-sync --description 'Wrapper: garante a chave SSH interativa carregada via keychain antes de chamar o binário real'
    keychain --quiet id_ed25519
    keychain env --shell fish --quiet | source
    command vault-sync $argv
end
```

(troque `id_ed25519` pelo nome real da sua chave). Sem isso, cada sessão de
terminal nova precisa carregar a chave manualmente antes de rodar
`vault-sync` contra o servidor.

### Automação de boot/poweroff (opcional)

`systemd/vault-sync-flush.service` (sobe pendências ao desligar) e
`systemd/vault-sync-doctor-boot.service` (diagnóstico no boot) são
artefatos manuais — cada `.service` tem instruções de instalação no
próprio cabeçalho do arquivo. Requerem uma chave SSH **sem passphrase**
dedicada (units do systemd não têm terminal/agente de sessão), registrada
como `IdentityFile` extra no `Host` correspondente em `~/.ssh/config`.

## Arquivos de configuração

O `vault-sync` lê/grava três arquivos, todos sob
`~/.config/vault-sync/` (ou `$XDG_CONFIG_HOME/vault-sync/`):

| Arquivo | Editado por | Propósito |
|---|---|---|
| `config.json` | você (ou `set-proton`/`set-server-name`/`set-client-name`) | Configuração principal — caminhos locais/remotos e preferências. Ver campos abaixo. |
| `remote_catalog_cache.json` | só o `vault-sync` (automático) | Última listagem remota bem-sucedida, gravada por `run_list`/`ingest`/`catalog` — permite o catálogo (menu SUPER+G) continuar mostrando jogos remotos com o servidor desligado. Nunca edite na mão; apagar é seguro (só perde o cache, refaz sozinho na próxima listagem). |
| `~/.local/state/vault-sync/vault-sync.log` (`$XDG_STATE_HOME`, fora de `~/.config`) | só o `vault-sync` (automático) | Log persistente (append), espelha o que sai no terminal — útil pra investigar uma sessão depois de fechada. |

### `config.json`

`run`, `ingest`, `purge`, `doctor` e `list` exigem um `config.json` válido
em `~/.config/vault-sync/config.json` (ou `--config <caminho>`):

```bash
mkdir -p ~/.config/vault-sync
cp config.example.json ~/.config/vault-sync/config.json
```

| Campo | Obrigatório | Descrição |
|---|---|---|
| `remote_ssh_target` | sim | Alias do `~/.ssh/config` (nunca IP/hostname direto) |
| `remote_base_path` | sim | Raiz da biblioteca Steam espelhada no servidor |
| `local_libraries` | sim (≥1) | Bibliotecas Steam locais, em ordem de preferência |
| `heroic_local_root` | não | Raiz dos prefixos Heroic (`Prefixes/default`) |
| `heroic_remote_base_path` | não | Espelho remoto dos prefixos Heroic |
| `local_game_root_steam` | não | Raiz local unificada p/ instalação completa de jogos Steam oficiais (precisa estar registrada como Library Folder na Steam) |
| `local_game_root_piratas` | não | Idem, non-Steam/pirata (informativo — `run`/`ingest` usam o `StartDir` já gravado no atalho, não este campo) |
| `local_game_root_epic` | não | Raiz local unificada p/ instalação completa Epic/Heroic |
| `remote_installs_base_path` | não | Raiz remota única p/ instalações completas dos 3 tipos |
| `install_path_overrides` | não | Correção manual de onde um jogo específico está instalado — ver abaixo |
| `server_name` | não | Nome de exibição do servidor remoto nas mensagens/catálogo — padrão `"Servidor"` se omitido. Ver abaixo. |
| `client_name` | não | Nome de exibição desta máquina local nas mensagens/catálogo — padrão `"Cliente"` se omitido. Ver abaixo. |

### Nome de exibição do servidor e da máquina local

Nenhum nome de instalação específica fica cravado no código — o
`vault-sync` é a mesma ferramenta pra qualquer servidor/máquina. Sem
configurar nada, o catálogo, o progresso de sincronização e o `doctor`
mostram os nomes genéricos `"Servidor"` e `"Cliente"`. Pra dar um nome
próprio (ex.: o hostname real da sua máquina servidor), use:

```bash
vault-sync set-server-name "FeuVault"
vault-sync set-client-name "ThinClient"
```

Isso grava `server_name`/`client_name` em `config.json` (mesmo mecanismo
de `set-proton` — escrita atômica com backup automático antes). Também dá
pra editar `config.json` na mão. Um nome em branco (string vazia, só
espaço) é tratado como se o campo não existisse — cai de volta pro
padrão.

### `install_path_overrides`: corrigindo um caminho sem mexer em código

`ACF`/`shortcuts.vdf`/`installed.json` (os arquivos que a Steam/Heroic
mantêm sozinhos) ficam desatualizados sozinhos com o tempo — discos
renomeados, jogos movidos manualmente. Em vez de precisar editar esses
arquivos externos (ou o código do `vault-sync`) toda vez que isso
acontece, corrija direto no `config.json`:

```json
{
  "install_path_overrides": {
    "steam": { "1449850": "/home/rafeu/Games/steam/YuGiOhMasterDuel" },
    "piratas": { "2385817016": "/mnt/SSD2/Piratas/HASTE.v1.5.c/HASTE.v1.5.c" },
    "epic": { "Disco Elysium - The Final Cut": "/mnt/HDD1/EPIC/DiscoElysium" }
  }
}
```

- Chave em `steam`/`piratas`: o `app_id` numérico (mesmo valor usado em
  `run <app_id> --steam`). Chave em `epic`: o título exato (mesmo valor
  usado em `run <título> --heroic`).
- Sempre tem prioridade sobre o metadado do launcher — se o override
  resolver, nem o `appmanifest`/`shortcuts.vdf`/`installed.json` chegam a
  ser consultados.
- Passa pela mesma verificação de qualquer outra fonte (`Path::is_dir()`)
  — um override desatualizado não é confiado cegamente, só ignorado
  (cai de volta pro metadado do launcher).
- Nada aqui é obrigatório: sem overrides, tudo funciona como antes,
  lendo o metadado do launcher normalmente.

Campos não marcados como obrigatórios podem ser omitidos por completo —
os comandos que dependem deles simplesmente ficam indisponíveis (com erro
claro) ou pulam a parte correspondente. `scan` não exige config nenhuma.

Depois de configurar, rode `vault-sync doctor` antes de qualquer outro
comando — confere bibliotecas locais, conectividade SSH e caminhos
remotos sem mutar nada.

## Comandos

```bash
vault-sync doctor                                                   # diagnóstico pré-voo, só leitura
vault-sync list                                                     # o que já existe no servidor
vault-sync run <app_id> --steam -- <comando>                        # ciclo completo: baixa, roda, sobe, limpa
vault-sync run <título> --heroic -- <comando>                       # idem, jogo Epic via Heroic
vault-sync purge <app_id> --steam [--yes]                           # remoção remota irrevogável
vault-sync purge <título> --heroic [--yes]
vault-sync ingest [--full] [--and-purge-local] [--only <id>] [--background]  # migra jogos local-only pro servidor
vault-sync scan --output catalogo.json                              # varredura forense da biblioteca Steam
vault-sync set-server-name <nome>                                   # nome de exibição do servidor (padrão "Servidor")
vault-sync set-client-name <nome>                                   # nome de exibição desta máquina (padrão "Cliente")
```

- `--steam`/`--heroic` decidem tanto o formato do identificador (AppID
  numérico vs. título de pasta) quanto qual par de campos de config é
  usado. Um dos dois é obrigatório em `run`/`purge`.
- `run` é normalmente invocado pela própria Steam/Heroic, não digitado à
  mão — ver "Ligando ao launcher" abaixo.
- `ingest` sem `--full` só sincroniza `compatdata` (saves/prefixo,
  pequeno). Com `--full`, também resolve e envia a instalação completa
  (o grosso do espaço em disco) de qualquer jogo local-only — útil pra
  adotar um jogo já instalado sem precisar abrir ele. `--and-purge-local`
  apaga o local depois do upload confirmado (funciona com ou sem
  `--full`).
- `purge` é **irrevogável** — remove dos dois lados (remoto e local),
  incluindo a instalação completa quando resolvível.
- `--only <app_id ou título>` restringe o `ingest` a um único jogo,
  ignorando o resto da varredura — essencial antes de rodar `--full` sem
  filtro nenhum pela primeira vez (senão a varredura pega tudo que é
  local-only de uma vez, incluindo jogos gigantes).

## Ligando ao launcher

**Steam (jogos non-Steam/atalhos/piratas)**: `scan` injeta automaticamente
o marcador `vault-sync run <app_id> --steam --` no `LaunchOptions` de
qualquer atalho em `shortcuts.vdf` que ainda não o tenha.

**Steam (jogos oficiais)**: não é automatizável com segurança pela mesma
via (`LaunchOptions` de jogo oficial vive em `localconfig.vdf`, fora do
escopo de edição deste projeto) — configure manualmente uma vez, em
Propriedades → Geral → Opções de Inicialização:

```
vault-sync run <app_id> --steam -- %command%
```

**Heroic (Epic)**: idem, nas configurações de jogo do Heroic, no campo de
argumentos de execução/wrapper.

## Adotando um jogo Steam oficial já instalado (instalação completa)

Fluxo pra trazer um jogo Steam oficial que já existe (HDD/SSD atual) pra
dentro do ciclo de instalação completa — **totalmente automático**, um
comando, sem mover manualmente cada jogo pela Steam:

1. **Uma vez, por máquina**: configure `local_game_root_steam` (ex.:
   `~/Games/steam`) no `config.json`. Não precisa registrar como Library
   Folder na Steam nem mover nada por lá — só o pequeno
   `appmanifest_<id>.acf` de cada jogo (poucos KB) é copiado pra lá;
   `vault-sync` nunca copia a instalação em si pra esse caminho, só
   registra o mapeamento. O local físico onde a Steam acha que o jogo
   está instalado (`libraryfolders.vdf`) fica desatualizado depois da
   adoção — **isso é esperado e inofensivo**: o `run` intercepta o "Jogar"
   antes do jogo abrir de verdade, então não depende da Steam achar que os
   arquivos estão íntegros.
2. Rode `vault-sync ingest --full --and-purge-local` — pra cada jogo
   Steam oficial local-only: envia a instalação completa e o compatdata
   direto de onde já estão pro servidor, apaga os dois do disco local
   (biblioteca antiga incluída), e migra só o manifesto pra
   `local_game_root_steam`. Não precisa abrir nenhum jogo.
3. Configure as Opções de Inicialização de cada jogo adotado (Steam →
   Propriedades → Geral) uma vez: `vault-sync run <app_id> --steam --
   %command%` — necessário pra próxima vez que clicar "Jogar" acionar o
   ciclo baixa→roda→sobe→apaga.
4. Da próxima vez que jogar, `run` baixa automaticamente de volta (pra
   `local_game_root_steam`, não pro caminho antigo), executa, e repete o
   ciclo sobe+apaga ao fechar.

Como Steam Cloud/Epic Cloud já cobrem a maioria dos saves desses dois
launchers, o risco real de perda de dado nesse fluxo é baixo — a
instalação em si sempre pode ser rebaixada do servidor (ou reinstalada
pela própria loja, no limite). **Jogos piratas são a exceção**: não têm
nenhum sync de nuvem, então o `StartDir` do atalho (não um `mv` manual)
continua sendo a única fonte de verdade de onde os arquivos estão — o
`vault-sync` nunca tenta relocar isso, só sobe/baixa/apaga no lugar já
registrado no atalho.

## Epic/Heroic

Sem passo de registro (ao contrário da Steam) — `local_game_root_epic`,
uma vez configurado, já é o alvo fixo que `run`/`purge` usam pra restaurar
a instalação entre sessões, independente de onde o Heroic/Legendary ache
que o jogo está instalado hoje (esse metadado já provou ficar obsoleto
sozinho — ver BACKLOG.md). Pra adotar um jogo Epic já instalado noutro
lugar, mova pela própria interface do Heroic (se ela tiver essa opção) ou
reinstale direto no caminho configurado; depois, `ingest --full` ou um
`run` normal cuidam do resto.

## Verificação

```bash
cargo test -- --test-threads=1
cargo clippy --all-targets
cargo fmt --check
```
