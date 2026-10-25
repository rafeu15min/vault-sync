use crate::progress::{spawn_progress_reader, ProgressUpdate};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

/// Cifra fixa exigida pela especificação: acelerável via AES-NI, para
/// saturar o enlace Direct Attach de 2.5 Gbps sem gargalo criptográfico.
/// O parâmetro `-z` (compressão) é deliberadamente omitido em todos os
/// comandos montados abaixo — comprimir sobrecarregaria a CPU sem ganho
/// relevante num link já rápido e seria a própria penalização que a spec
/// pede para evitar.
const SSH_CIPHER: &str = "aes128-gcm@openssh.com";

/// `ConnectTimeout` evita que uma transferência fique pendurada
/// indefinidamente se o FeuVault estiver desligado/inacessível — sem isso,
/// `run`/`ingest` travariam o terminal (ou o `ExecStop` do systemd da Fase
/// V) esperando um TCP handshake que nunca chega. `BatchMode=yes` desliga
/// qualquer prompt interativo de senha, coerente com uma ferramenta não
/// interativa que só deve funcionar com autenticação por chave.
const SSH_TRANSPORT_OPTIONS: &str = "-o ConnectTimeout=10 -o BatchMode=yes";

/// As duas rotinas assimétricas da spec: obtenção da instância completa
/// (antes de rodar o jogo) e envio apenas do que mudou (depois de encerrar).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferKind {
    /// remoto -> local, instância inteira.
    FullDownload,
    /// local -> remoto, apenas alterações (comportamento incremental nativo do rsync).
    IncrementalUpload,
    /// local -> remoto, um único arquivo — sem a barra final que
    /// `IncrementalUpload` sempre adiciona (que só faz sentido pra
    /// diretórios; num arquivo, rsync recusa com "Not a directory"). Usado
    /// por `ingest::upload_manifest_snapshot` pro retrato de
    /// `appmanifest_<id>.acf` salvo junto com a instalação completa.
    UploadSingleFile,
}

#[derive(Debug, thiserror::Error)]
pub enum TransferError {
    #[error("falha ao iniciar processo rsync: {0}")]
    Spawn(#[source] std::io::Error),
    #[error(
        "rsync encerrou com código de saída {0} — enlace instável ou falha remota (stderr: {1})"
    )]
    NonZeroExit(i32, String),
    #[error("rsync foi encerrado por sinal — enlace provavelmente caiu")]
    KilledBySignal,
    #[error(
        "sem um diretório seguro (absoluto e fora de '{0}') pra guardar o que o download \
         sobrescreveria — download cancelado pra não substituir nada local sem cópia"
    )]
    NoBackupDir(String),
}

impl TransferError {
    /// `true` só quando a falha é especificamente "o diretório de ORIGEM
    /// desta transferência não existe" — o rsync real emite essa frase
    /// exata em stderr (`[sender] change_dir "..." failed: No such file or
    /// directory`) quando pedido pra sincronizar algo que nunca foi criado.
    /// A origem pode ser o lado remoto (`full_download`: nada no FeuVault
    /// ainda pra baixar) OU o lado local (`incremental_upload`: o jogo
    /// nunca chegou a escrever nada localmente, ex.: crashou antes de
    /// abrir de verdade) — o texto do rsync é o mesmo nos dois casos, só
    /// muda qual caminho aparece dentro das aspas.
    /// Incidente real (2026-09-07, "Nobody Wants to Die"): apareceu nas
    /// DUAS direções — primeiro no download (jogo baixado mas nunca aberto,
    /// nada no FeuVault ainda), depois, já corrigido esse lado, no upload
    /// (o jogo abriu mas crashou antes de escrever qualquer coisa no
    /// prefixo Wine local). Nos dois casos saía com o mesmo código 23 de
    /// uma falha de rede de verdade — sem distinguir, `run` bloqueava uma
    /// sessão que não tinha nada pra sincronizar, tratando "nada aqui
    /// ainda" como se fosse "a rede caiu". Casa só pelo texto porque o
    /// código de saída sozinho (23 = "partial transfer due to error") cobre
    /// dezenas de causas diferentes, várias delas erros de verdade que
    /// continuam bloqueando normalmente.
    pub fn is_missing_source_dir(&self) -> bool {
        match self {
            TransferError::NonZeroExit(_, stderr) => {
                stderr.contains("change_dir") && stderr.contains("No such file or directory")
            }
            _ => false,
        }
    }
}

/// Uma transferência rsync sobre SSH entre um caminho local e um caminho
/// remoto (`ssh_target:remote_path`). Monta a matriz de comando bruta e,
/// opcionalmente, executa o processo com captura de progresso em tempo real.
#[derive(Debug, Clone)]
pub struct RsyncTransfer {
    kind: TransferKind,
    ssh_target: String,
    local_path: PathBuf,
    remote_path: PathBuf,
    rsync_bin: String,
    /// Só `FullDownload`: onde o rsync guarda (`--backup --backup-dir`) cada
    /// arquivo local que o download substituir. `None` num download significa
    /// "não deu pra garantir um lugar seguro" e faz `execute_with_progress`
    /// recusar a transferência (ver `backup_dir_for`).
    backup_dir: Option<PathBuf>,
}

/// Onde o download guarda o que sobrescreveu, sob `root`
/// (`~/.local/state/vault-sync/overwritten` em produção): um diretório por
/// transferência, `<nome do destino>-<segundos unix>`. O rsync só o cria se
/// de fato tirar algo do caminho, então o normal (destino vazio, cópia local
/// apagada no fim da sessão anterior) não deixa rastro nenhum.
///
/// Risco achado investigando o prefixo quebrado do The Sims 4 (2026-10-23):
/// o `-a` sem `--backup` substituía qualquer arquivo local diferente do
/// servidor — inclusive um save ou `system.reg` local MAIS NOVO que sobrou de
/// uma sessão que não terminou de sincronizar — sem deixar cópia, e saves não
/// têm como ser recuperados depois. Precisa ser absoluto (relativo,
/// o rsync resolveria a partir do destino) e fora do destino (dentro, o
/// backup subiria pro servidor no próximo envio e seria apagado na limpeza
/// junto com o prefixo). `None` quando não dá pra garantir as duas coisas.
fn backup_dir_for(root: Option<PathBuf>, local_path: &Path, stamp: u64) -> Option<PathBuf> {
    let name = local_path.file_name()?.to_string_lossy();
    let dir = root?.join(format!("{name}-{stamp}"));
    (dir.is_absolute() && !dir.starts_with(local_path)).then_some(dir)
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl RsyncTransfer {
    pub fn full_download(
        ssh_target: impl Into<String>,
        remote_path: impl Into<PathBuf>,
        local_path: impl Into<PathBuf>,
    ) -> Self {
        let local_path = local_path.into();
        let backup_root = dirs::state_dir().map(|d| d.join("vault-sync").join("overwritten"));
        Self {
            kind: TransferKind::FullDownload,
            ssh_target: ssh_target.into(),
            remote_path: remote_path.into(),
            backup_dir: backup_dir_for(backup_root, &local_path, unix_now()),
            local_path,
            rsync_bin: "rsync".to_string(),
        }
    }

    pub fn incremental_upload(
        ssh_target: impl Into<String>,
        local_path: impl Into<PathBuf>,
        remote_path: impl Into<PathBuf>,
    ) -> Self {
        Self {
            kind: TransferKind::IncrementalUpload,
            ssh_target: ssh_target.into(),
            remote_path: remote_path.into(),
            local_path: local_path.into(),
            rsync_bin: "rsync".to_string(),
            backup_dir: None,
        }
    }

    pub fn upload_single_file(
        ssh_target: impl Into<String>,
        local_path: impl Into<PathBuf>,
        remote_path: impl Into<PathBuf>,
    ) -> Self {
        Self {
            kind: TransferKind::UploadSingleFile,
            ssh_target: ssh_target.into(),
            remote_path: remote_path.into(),
            local_path: local_path.into(),
            rsync_bin: "rsync".to_string(),
            backup_dir: None,
        }
    }

    /// Substitui o binário `rsync` invocado — usado por `session.rs` nos
    /// testes de orquestração (injeta um script falso) e, em produção, por
    /// quem precisar apontar para um rsync fora do PATH padrão.
    pub fn with_binary(mut self, bin: impl Into<String>) -> Self {
        self.rsync_bin = bin.into();
        self
    }

    fn remote_spec(&self) -> String {
        format!("{}:{}", self.ssh_target, self.remote_path.display())
    }

    /// Monta a matriz de argumentos bruta submetida ao rsync. Pública para
    /// permitir inspeção/testes sem precisar spawnar um processo.
    ///
    /// A barra final no lado de origem faz o rsync copiar o *conteúdo* do
    /// diretório para dentro do destino (que já é o diretório final
    /// correto, calculado por `paths.rs`) em vez de aninhar mais um nível.
    pub fn build_args(&self) -> Vec<String> {
        let (source, destination) = match self.kind {
            TransferKind::FullDownload => (
                format!("{}:{}/", self.ssh_target, self.remote_path.display()),
                self.local_path.display().to_string(),
            ),
            TransferKind::IncrementalUpload => (
                format!("{}/", self.local_path.display()),
                self.remote_spec(),
            ),
            TransferKind::UploadSingleFile => {
                (self.local_path.display().to_string(), self.remote_spec())
            }
        };

        let mut args = vec![
            "-a".to_string(),
            "--partial".to_string(),
            // Sem `-X` o rsync perde o `user.WINEREPARSE`, com que o Wine
            // guarda os links do Windows (ex.: `EA Desktop`): vira pasta
            // vazia e o EABackgroundService não sobe.
            "-X".to_string(),
            // Cria os diretórios intermediários do destino que ainda não
            // existem (ex.: primeiro `ingest` de sempre para um FeuVault
            // recém-provisionado, antes de qualquer `steamapps/compatdata`
            // existir lá) — sem isso, o rsync recusa gravar com "No such
            // file or directory" em vez de criar a árvore automaticamente.
            "--mkpath".to_string(),
            "--info=progress2".to_string(),
            "-e".to_string(),
            format!("ssh -c {SSH_CIPHER} {SSH_TRANSPORT_OPTIONS}"),
        ];
        // Mecanismo nativo do rsync, sem cópia nossa: cada arquivo local
        // que o download substituiria vai antes pra `backup_dir`, no mesmo
        // caminho relativo (ver `backup_dir_for`).
        if let Some(dir) = &self.backup_dir {
            args.push("--backup".to_string());
            args.push(format!("--backup-dir={}", dir.display()));
        }
        args.push(source);
        args.push(destination);
        args
    }

    /// O rsync só cria `backup_dir` quando de fato tirou algo do caminho —
    /// se ele existe, havia arquivo local diferente do servidor (talvez um
    /// save mais novo) e quem usa precisa saber onde ficou. Roda também
    /// quando o download falha no meio: o que já foi guardado continua lá.
    fn warn_if_anything_was_overwritten(&self) {
        let Some(dir) = self.backup_dir.as_ref().filter(|d| d.exists()) else {
            return;
        };
        tracing::warn!(
            "o download substituiu arquivos locais diferentes do servidor em '{}' — as versões \
             locais antigas foram guardadas em '{}' (confira antes de apagar: podem ser saves \
             mais novos que a cópia do servidor)",
            self.local_path.display(),
            dir.display()
        );
    }

    fn to_command(&self) -> Command {
        let mut cmd = Command::new(&self.rsync_bin);
        cmd.args(self.build_args());
        cmd
    }

    /// Executa a transferência, encaminhando cada atualização de progresso
    /// capturada em tempo real para `on_progress`, e valida o código de
    /// saída ao final — uma falha de rede (ou qualquer código não-zero)
    /// nunca é silenciada, propagando `TransferError` para que o chamador
    /// bloqueie a inicialização de qualquer etapa dependente.
    pub fn execute_with_progress(
        &self,
        mut on_progress: impl FnMut(ProgressUpdate),
    ) -> Result<(), TransferError> {
        if self.kind == TransferKind::FullDownload && self.backup_dir.is_none() {
            return Err(TransferError::NoBackupDir(
                self.local_path.display().to_string(),
            ));
        }
        let mut child = self
            .to_command()
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(TransferError::Spawn)?;

        let stdout = child
            .stdout
            .take()
            .expect("stdout foi configurado como piped acima");
        // stderr precisa ser drenado em paralelo com stdout, numa thread à
        // parte — se ninguém ler o pipe de stderr enquanto o loop abaixo
        // espera stdout fechar, o rsync trava assim que o buffer do pipe
        // enche (deadlock clássico de dois pipes só um sendo lido). Só o
        // texto final importa aqui (usado por `is_missing_source_dir`),
        // não streaming linha a linha como o progresso em stdout.
        let mut stderr = child
            .stderr
            .take()
            .expect("stderr foi configurado como piped acima");
        let stderr_handle = thread::spawn(move || {
            let mut buf = String::new();
            let _ = stderr.read_to_string(&mut buf);
            buf
        });

        let rx = spawn_progress_reader(stdout);
        for update in rx {
            on_progress(update);
        }

        let status = child.wait().map_err(TransferError::Spawn)?;
        let stderr_text = stderr_handle.join().unwrap_or_default();
        self.warn_if_anything_was_overwritten();
        match status.code() {
            Some(0) => Ok(()),
            Some(code) => Err(TransferError::NonZeroExit(code, stderr_text)),
            None => Err(TransferError::KilledBySignal),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn full_download_args_have_correct_direction_and_no_compression() {
        let t = RsyncTransfer::full_download(
            "feuvault",
            "/vault/steamapps/compatdata/730",
            "/ssd/steamapps/compatdata/730",
        );
        let args = t.build_args();

        assert!(
            !args.contains(&"-z".to_string()),
            "compressão deve ficar sempre desligada"
        );
        assert_eq!(args[0], "-a");
        assert!(args.contains(&"--partial".to_string()));
        assert!(args.contains(&"-X".to_string()));
        assert!(args.contains(&"--info=progress2".to_string()));
        assert_eq!(
            args[args.len() - 2],
            "feuvault:/vault/steamapps/compatdata/730/"
        );
        assert_eq!(args[args.len() - 1], "/ssd/steamapps/compatdata/730");
    }

    #[test]
    fn incremental_upload_args_have_correct_direction() {
        let t = RsyncTransfer::incremental_upload(
            "feuvault",
            "/ssd/steamapps/compatdata/730",
            "/vault/steamapps/compatdata/730",
        );
        let args = t.build_args();

        assert_eq!(args[args.len() - 2], "/ssd/steamapps/compatdata/730/");
        assert_eq!(
            args[args.len() - 1],
            "feuvault:/vault/steamapps/compatdata/730"
        );
    }

    #[test]
    fn upload_single_file_args_never_add_a_trailing_slash() {
        // Diferente de `incremental_upload`, a origem aqui é um arquivo —
        // uma barra final faria o rsync recusar com "Not a directory".
        let t = RsyncTransfer::upload_single_file(
            "feuvault",
            "/ssd/steamapps/appmanifest_730.acf",
            "/vault/game-installs/steam-manifests/730.acf",
        );
        let args = t.build_args();

        assert_eq!(args[args.len() - 2], "/ssd/steamapps/appmanifest_730.acf");
        assert_eq!(
            args[args.len() - 1],
            "feuvault:/vault/game-installs/steam-manifests/730.acf"
        );
    }

    #[test]
    fn ssh_option_uses_the_mandated_cipher_and_a_finite_connect_timeout() {
        let t = RsyncTransfer::full_download("feuvault", "/vault/x", "/ssd/x");
        let args = t.build_args();
        let e_index = args.iter().position(|a| a == "-e").unwrap();
        assert_eq!(
            args[e_index + 1],
            "ssh -c aes128-gcm@openssh.com -o ConnectTimeout=10 -o BatchMode=yes"
        );
    }

    #[test]
    fn always_creates_missing_intermediate_destination_directories() {
        let t = RsyncTransfer::full_download("feuvault", "/vault/x", "/ssd/x");
        assert!(t.build_args().contains(&"--mkpath".to_string()));
    }

    #[test]
    fn full_download_backs_up_what_it_overwrites_outside_the_destination() {
        let dest = "/ssd/steamapps/compatdata/1222670";
        let t =
            RsyncTransfer::full_download("feuvault", "/vault/steamapps/compatdata/1222670", dest);
        let args = t.build_args();

        assert!(args.contains(&"--backup".to_string()));
        let backup_dir = args
            .iter()
            .find_map(|a| a.strip_prefix("--backup-dir="))
            .map(Path::new)
            .expect("download sem --backup-dir");
        assert!(
            backup_dir.is_absolute(),
            "relativo cairia dentro do destino"
        );
        assert!(!backup_dir.starts_with(dest));
        assert!(backup_dir
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("1222670-"));
        // Opções sempre antes da origem/destino.
        assert_eq!(args[args.len() - 1], dest);
    }

    #[test]
    fn uploads_never_ask_rsync_for_backups() {
        let uploads = [
            RsyncTransfer::incremental_upload("feuvault", "/ssd/x", "/vault/x"),
            RsyncTransfer::upload_single_file("feuvault", "/ssd/x.acf", "/vault/x.acf"),
        ];
        for t in uploads {
            assert!(!t.build_args().iter().any(|a| a.starts_with("--backup")));
        }
    }

    #[test]
    fn backup_dir_for_only_accepts_an_absolute_dir_outside_the_destination() {
        let root = || Some(PathBuf::from("/home/u/.local/state/vault-sync/overwritten"));
        assert_eq!(
            backup_dir_for(root(), Path::new("/ssd/compatdata/730"), 42),
            Some(PathBuf::from(
                "/home/u/.local/state/vault-sync/overwritten/730-42"
            ))
        );
        // Destino que contém a raiz do backup (ex.: $HOME inteiro).
        assert_eq!(backup_dir_for(root(), Path::new("/home/u"), 42), None);
        assert_eq!(
            backup_dir_for(Some(PathBuf::from("relativo")), Path::new("/ssd/730"), 42),
            None
        );
        assert_eq!(backup_dir_for(None, Path::new("/ssd/730"), 42), None);
        assert_eq!(backup_dir_for(root(), Path::new("/"), 42), None);
    }

    #[test]
    fn download_without_a_safe_backup_dir_is_refused_before_rsync_runs() {
        let dir = crate::testutil::test_dir("rsync", "no-backup-dir");
        let marker = dir.join("rsync-ran");
        let script = crate::testutil::write_script(
            &dir,
            "fake-rsync.sh",
            &format!("touch '{}'", marker.display()),
        );
        let mut t = RsyncTransfer::full_download("feuvault", "/vault/x", "/ssd/x");
        t.rsync_bin = script.display().to_string();
        t.backup_dir = None;

        let result = t.execute_with_progress(|_| {});

        assert!(matches!(result, Err(TransferError::NoBackupDir(_))));
        assert!(!marker.exists(), "o rsync não deveria nem ter sido chamado");
    }

    fn fake_script(body: &str) -> PathBuf {
        let dir = crate::testutil::test_dir("rsync", "fake-script");
        crate::testutil::write_script(&dir, "fake-rsync.sh", body)
    }

    #[test]
    fn execute_with_progress_collects_updates_and_succeeds_on_exit_zero() {
        let script = fake_script(
            "printf '     100  10%%   1.00MB/s    0:00:09\\r     999  99%%   2.00MB/s    0:00:01\\n'\nexit 0",
        );
        let mut t = RsyncTransfer::full_download("feuvault", "/vault/x", "/ssd/x");
        t.rsync_bin = script.display().to_string();

        let mut seen = Vec::new();
        let result = t.execute_with_progress(|u| seen.push(u));

        assert!(result.is_ok());
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].percent, 10);
        assert_eq!(seen[1].percent, 99);

        fs::remove_file(&script).ok();
    }

    #[test]
    fn execute_with_progress_reports_non_zero_exit_as_error() {
        let script = fake_script("exit 23");
        let mut t = RsyncTransfer::full_download("feuvault", "/vault/x", "/ssd/x");
        t.rsync_bin = script.display().to_string();

        let result = t.execute_with_progress(|_| {});

        match result {
            Err(TransferError::NonZeroExit(23, _)) => {}
            other => panic!("esperava NonZeroExit(23, _), obteve {other:?}"),
        }

        fs::remove_file(&script).ok();
    }

    #[test]
    fn execute_with_progress_captures_stderr_text_on_failure() {
        // Sem isto, `stderr` era descartado (`Stdio::null()`) e
        // `is_missing_source_dir` nunca teria texto nenhum pra
        // reconhecer — precisa vir de verdade do processo real, não só do
        // código de saída.
        let script = fake_script("echo 'rsync: algo deu errado' >&2\nexit 23");
        let mut t = RsyncTransfer::full_download("feuvault", "/vault/x", "/ssd/x");
        t.rsync_bin = script.display().to_string();

        let result = t.execute_with_progress(|_| {});

        match result {
            Err(TransferError::NonZeroExit(23, stderr)) => {
                assert!(stderr.contains("algo deu errado"), "stderr: {stderr:?}");
            }
            other => panic!("esperava NonZeroExit(23, _) com stderr, obteve {other:?}"),
        }

        fs::remove_file(&script).ok();
    }

    #[test]
    fn is_missing_source_dir_recognizes_rsync_change_dir_failure() {
        // Regressão do incidente real (2026-09-07, "Nobody Wants to Die"):
        // este texto exato é o que o rsync real emite quando o diretório
        // de origem remoto não existe — o único caso que `run` (session.rs)
        // deve tratar como "nada sincronizado ainda", não como erro.
        let err = TransferError::NonZeroExit(
            23,
            "rsync: [sender] change_dir \"/vault/heroic-prefixes/Jogo\" failed: \
             No such file or directory (2)"
                .to_string(),
        );
        assert!(err.is_missing_source_dir());
    }

    #[test]
    fn is_missing_source_dir_is_false_for_unrelated_failures() {
        // Qualquer outra causa de código 23 (permissão negada, conexão
        // caindo no meio, etc.) precisa continuar bloqueando como erro de
        // verdade — só o "diretório não existe" é seguro pular.
        let err = TransferError::NonZeroExit(23, "rsync: connection unexpectedly closed".into());
        assert!(!err.is_missing_source_dir());

        let err_no_stderr = TransferError::NonZeroExit(23, String::new());
        assert!(!err_no_stderr.is_missing_source_dir());

        assert!(!TransferError::KilledBySignal.is_missing_source_dir());
    }
}
