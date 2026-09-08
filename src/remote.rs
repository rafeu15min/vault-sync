//! Primitivas SSH cruas usadas por `ingest.rs` (5.1), `purge.rs` (5.3) e
//! `doctor.rs` para consultar/remover caminhos no FeuVault fora do fluxo de
//! transferência de arquivos do `rsync.rs`. Deliberadamente mínimo: apenas
//! listagem, remoção e checagens de diagnóstico, sempre sobre caminhos já
//! validados por `paths.rs`.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum RemoteError {
    #[error("falha ao executar '{0}': {1}")]
    Spawn(String, #[source] std::io::Error),
    #[error("comando remoto via ssh encerrou com código {0}: {1}")]
    NonZeroExit(i32, String),
    #[error("comando remoto via ssh foi encerrado por sinal")]
    KilledBySignal,
    #[error("saída do comando remoto não é UTF-8 válida")]
    InvalidUtf8,
}

/// Estado de um caminho no FeuVault, usado pelo `doctor` para avisar sobre
/// a mesma armadilha do lado remoto que `paths::is_mount_point` cobre do
/// lado local: um ponto de montagem esperado (o disco de armazenamento do
/// FeuVault) que na verdade não está montado aparece como um diretório
/// comum vazio, e escritas ali acabam silenciosamente na partição raiz.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemotePathStatus {
    Missing,
    Directory,
    MountPoint,
}

/// Encapsula `s` em aspas simples para uso seguro como um único argumento
/// dentro do comando de shell remoto que o `ssh` monta a partir do argv
/// (o próprio protocolo SSH sempre repassa o comando a um shell remoto).
fn shell_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Traduz um código de saída não-zero (ou encerramento por sinal) em
/// `RemoteError` — `None` significa sucesso (código 0). Quem chama decide o
/// que fazer com o sucesso: nem sempre é só `Ok(())` (ver `read_remote_file`,
/// que ainda trata um código específico à parte como "arquivo ausente").
fn nonzero_exit_error(status: &std::process::ExitStatus, stderr: &[u8]) -> Option<RemoteError> {
    match status.code() {
        Some(0) => None,
        Some(code) => Some(RemoteError::NonZeroExit(
            code,
            String::from_utf8_lossy(stderr).into_owned(),
        )),
        None => Some(RemoteError::KilledBySignal),
    }
}

/// Monta o `Command` base compartilhado por todas as chamadas SSH deste
/// módulo. `ConnectTimeout` evita travar indefinidamente se o FeuVault
/// estiver desligado/inacessível; `BatchMode=yes` desliga qualquer prompt
/// interativo de senha — coerente com uma ferramenta não interativa que só
/// deve operar com autenticação por chave já configurada. `remote_cmd` é
/// envolvido em `sh -c '...'` porque o SSH sempre repassa o comando ao
/// shell de login do usuário remoto — no FeuVault isso é `fish`, que não
/// entende a sintaxe POSIX (`if`/`[ ]`) usada aqui, então forçar `sh`
/// evita depender de qual shell de login está configurado do outro lado.
fn ssh_command(ssh_bin: &str, ssh_target: &str, remote_cmd: &str) -> Command {
    let mut cmd = Command::new(ssh_bin);
    cmd.args(["-o", "ConnectTimeout=10", "-o", "BatchMode=yes"])
        .arg(ssh_target)
        .arg(format!("sh -c {}", shell_single_quote(remote_cmd)));
    cmd
}

/// Lista os nomes de entrada de um diretório remoto (`ls -1`). Um diretório
/// inexistente é tratado como lista vazia, não como erro — condição normal
/// na primeira execução do `ingest` antes de qualquer dado remoto existir.
pub fn list_remote_dir_entries(
    ssh_bin: &str,
    ssh_target: &str,
    remote_dir: &Path,
) -> Result<Vec<String>, RemoteError> {
    let remote_cmd = format!(
        "ls -1 -- {} 2>/dev/null || true",
        shell_single_quote(&remote_dir.display().to_string())
    );
    let output = ssh_command(ssh_bin, ssh_target, &remote_cmd)
        .output()
        .map_err(|e| RemoteError::Spawn(ssh_bin.to_string(), e))?;

    if !output.status.success() {
        return Err(RemoteError::NonZeroExit(
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ));
    }

    let stdout = String::from_utf8(output.stdout).map_err(|_| RemoteError::InvalidUtf8)?;
    Ok(stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect())
}

/// Remove recursivamente um caminho remoto (`rm -rf`). `-f` também cobre o
/// caso do caminho já não existir (ex.: manifesto ausente para jogos
/// non-Steam), então chamadores não precisam checar existência antes.
pub fn remove_remote_path(
    ssh_bin: &str,
    ssh_target: &str,
    remote_path: &Path,
) -> Result<(), RemoteError> {
    let remote_cmd = format!(
        "rm -rf -- {}",
        shell_single_quote(&remote_path.display().to_string())
    );
    let output = ssh_command(ssh_bin, ssh_target, &remote_cmd)
        .output()
        .map_err(|e| RemoteError::Spawn(ssh_bin.to_string(), e))?;

    match nonzero_exit_error(&output.status, &output.stderr) {
        None => Ok(()),
        Some(e) => Err(e),
    }
}

/// Testa se o alvo SSH responde dentro do `ConnectTimeout` configurado —
/// usado pelo `doctor` como primeira checagem, antes de qualquer consulta
/// de caminho (que dependeria da mesma conectividade).
pub fn check_connectivity(ssh_bin: &str, ssh_target: &str) -> Result<(), RemoteError> {
    let output = ssh_command(ssh_bin, ssh_target, "true")
        .output()
        .map_err(|e| RemoteError::Spawn(ssh_bin.to_string(), e))?;

    match nonzero_exit_error(&output.status, &output.stderr) {
        None => Ok(()),
        Some(e) => Err(e),
    }
}

/// `turn-on FeuVault` é uma função fish (definida em `config.fish`, não um
/// binário no PATH) que publica um comando de wake via MQTT — fora do escopo
/// deste projeto, só é invocada. Como não é um executável, precisa passar
/// por `fish -c`, que carrega `config.fish` mesmo em modo não-interativo.
const WAKE_BIN: &str = "fish";
const WAKE_ARGS: [&str; 2] = ["-c", "turn-on FeuVault"];
const WAKE_RETRIES: u32 = 24;
const WAKE_RETRY_INTERVAL: Duration = Duration::from_secs(5);

/// Como `check_connectivity`, mas se o alvo estiver inacessível tenta
/// primeiro ligá-lo via `turn-on FeuVault` e aguarda (~2 minutos, tempo
/// típico de boot de um servidor doméstico) até a conectividade voltar,
/// antes de desistir. Usado pela CLI (`run`/`ingest`/`purge`) para que um
/// FeuVault desligado não seja simplesmente um erro — é ligado sob demanda.
pub fn ensure_reachable(ssh_bin: &str, ssh_target: &str) -> Result<(), RemoteError> {
    ensure_reachable_with_options(
        ssh_bin,
        ssh_target,
        WAKE_BIN,
        &WAKE_ARGS,
        WAKE_RETRIES,
        WAKE_RETRY_INTERVAL,
    )
}

fn ensure_reachable_with_options(
    ssh_bin: &str,
    ssh_target: &str,
    wake_bin: &str,
    wake_args: &[&str],
    retries: u32,
    retry_interval: Duration,
) -> Result<(), RemoteError> {
    let mut result = check_connectivity(ssh_bin, ssh_target);
    if result.is_ok() {
        return result;
    }

    tracing::info!(
        "servidor inacessível — ligando via '{wake_bin} {}'...",
        wake_args.join(" ")
    );
    let _ = Command::new(wake_bin).args(wake_args).status();

    for _ in 0..retries {
        std::thread::sleep(retry_interval);
        result = check_connectivity(ssh_bin, ssh_target);
        if result.is_ok() {
            return result;
        }
    }

    result
}

/// Código de saída interno (nunca gerado pelo `ssh` de verdade — ver
/// `check_connectivity`/outras chamadas, que só usam 0/255/sinal) usado só
/// dentro do `sh -c` remoto de `read_remote_file` pra distinguir "arquivo
/// ausente" (caso normal, nunca um erro) de qualquer outra falha real.
const REMOTE_FILE_MISSING_CODE: i32 = 44;

/// Lê o conteúdo de um único arquivo remoto — usado por
/// `session::restore_official_manifest_snapshot` pra buscar o
/// `appmanifest_<app_id>.acf` salvo em `steam-manifests/` no FeuVault (ver
/// `ingest.rs`) sem precisar de uma transferência `rsync` só pra um arquivo
/// pequeno. `Ok(None)` quando o arquivo simplesmente não existe — estado
/// normal pra qualquer jogo cuja instalação completa nunca foi rastreada,
/// nunca um erro.
pub fn read_remote_file(
    ssh_bin: &str,
    ssh_target: &str,
    remote_path: &Path,
) -> Result<Option<String>, RemoteError> {
    let quoted = shell_single_quote(&remote_path.display().to_string());
    let remote_cmd = format!(
        "if [ -f {quoted} ]; then cat -- {quoted}; else exit {REMOTE_FILE_MISSING_CODE}; fi"
    );
    let output = ssh_command(ssh_bin, ssh_target, &remote_cmd)
        .output()
        .map_err(|e| RemoteError::Spawn(ssh_bin.to_string(), e))?;

    if output.status.code() == Some(REMOTE_FILE_MISSING_CODE) {
        return Ok(None);
    }
    if let Some(e) = nonzero_exit_error(&output.status, &output.stderr) {
        return Err(e);
    }
    let stdout = String::from_utf8(output.stdout).map_err(|_| RemoteError::InvalidUtf8)?;
    Ok(Some(stdout))
}

/// Classifica um caminho remoto: ausente, diretório comum, ou ponto de
/// montagem (comparando o `st_dev` do caminho com o do seu pai via `stat`,
/// sem depender do utilitário externo `mountpoint`). Usado pelo `doctor`
/// para avisar quando `remote_base_path` existe apenas como um diretório
/// vazio deixado por um ponto de montagem que não está de fato montado no
/// FeuVault — a mesma armadilha que `paths::is_mount_point` cobre localmente.
pub fn check_remote_path(
    ssh_bin: &str,
    ssh_target: &str,
    remote_path: &Path,
) -> Result<RemotePathStatus, RemoteError> {
    let quoted = shell_single_quote(&remote_path.display().to_string());
    let remote_cmd = format!(
        "if [ ! -d {quoted} ]; then echo missing; \
         elif [ \"$(stat -c %d -- {quoted} 2>/dev/null)\" != \"$(stat -c %d -- {quoted}/.. 2>/dev/null)\" ]; then echo mount; \
         else echo dir; fi"
    );
    let output = ssh_command(ssh_bin, ssh_target, &remote_cmd)
        .output()
        .map_err(|e| RemoteError::Spawn(ssh_bin.to_string(), e))?;

    if !output.status.success() {
        return Err(RemoteError::NonZeroExit(
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ));
    }

    let stdout = String::from_utf8(output.stdout).map_err(|_| RemoteError::InvalidUtf8)?;
    Ok(match stdout.trim() {
        "missing" => RemotePathStatus::Missing,
        "mount" => RemotePathStatus::MountPoint,
        _ => RemotePathStatus::Directory,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, File};
    use std::io::Write;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    fn test_dir(label: &str) -> std::path::PathBuf {
        crate::testutil::test_dir("remote", label)
    }

    /// Simula o `ssh <target> <comando>` real: como o binário fake não é o
    /// ssh de verdade, ele recebe o alvo em `$1` e o comando remoto em `$2`
    /// — exatamente os dois argumentos que `Command::new(ssh_bin)` monta.
    fn fake_ssh(dir: &std::path::Path, body: &str) -> std::path::PathBuf {
        let path = dir.join("fake-ssh.sh");
        let mut f = File::create(&path).unwrap();
        writeln!(f, "#!/bin/sh\n{body}").unwrap();
        #[cfg(unix)]
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[test]
    fn list_remote_dir_entries_parses_newline_separated_output() {
        let dir = test_dir("list");
        let ssh = fake_ssh(&dir, "printf '730\\n1234567890\\n'");

        let entries =
            list_remote_dir_entries(ssh.to_str().unwrap(), "feuvault", Path::new("/vault/x"))
                .unwrap();

        assert_eq!(entries, vec!["730".to_string(), "1234567890".to_string()]);
    }

    #[test]
    fn list_remote_dir_entries_treats_missing_dir_as_empty() {
        let dir = test_dir("list-empty");
        // `ls` de um diretório inexistente sairia com stderr + código != 0,
        // mas o `|| true` embutido no comando remoto absorve isso.
        let ssh = fake_ssh(&dir, "printf ''");

        let entries =
            list_remote_dir_entries(ssh.to_str().unwrap(), "feuvault", Path::new("/vault/x"))
                .unwrap();

        assert!(entries.is_empty());
    }

    #[test]
    fn list_remote_dir_entries_reports_ssh_connection_failure() {
        let dir = test_dir("list-fail");
        let ssh = fake_ssh(&dir, "exit 255");

        let result =
            list_remote_dir_entries(ssh.to_str().unwrap(), "feuvault", Path::new("/vault/x"));

        assert!(matches!(result, Err(RemoteError::NonZeroExit(255, _))));
    }

    #[test]
    fn remove_remote_path_succeeds_on_exit_zero() {
        let dir = test_dir("remove-ok");
        let ssh = fake_ssh(&dir, "exit 0");

        let result =
            remove_remote_path(ssh.to_str().unwrap(), "feuvault", Path::new("/vault/x/730"));

        assert!(result.is_ok());
    }

    #[test]
    fn remove_remote_path_propagates_non_zero_exit() {
        let dir = test_dir("remove-fail");
        let ssh = fake_ssh(&dir, "exit 1");

        let result =
            remove_remote_path(ssh.to_str().unwrap(), "feuvault", Path::new("/vault/x/730"));

        assert!(matches!(result, Err(RemoteError::NonZeroExit(1, _))));
    }

    #[test]
    fn check_connectivity_succeeds_when_ssh_reaches_the_target() {
        let dir = test_dir("connectivity-ok");
        let ssh = fake_ssh(&dir, "exit 0");

        assert!(check_connectivity(ssh.to_str().unwrap(), "feuvault").is_ok());
    }

    #[test]
    fn check_connectivity_reports_unreachable_target() {
        let dir = test_dir("connectivity-fail");
        let ssh = fake_ssh(&dir, "exit 255");

        let result = check_connectivity(ssh.to_str().unwrap(), "feuvault");
        assert!(matches!(result, Err(RemoteError::NonZeroExit(255, _))));
    }

    #[test]
    fn ensure_reachable_skips_wake_when_already_reachable() {
        let dir = test_dir("ensure-reachable-already-up");
        let ssh = fake_ssh(&dir, "exit 0");
        // Binário de wake inexistente: se `ensure_reachable` tentasse chamá-lo
        // mesmo assim, o `Command::new` falharia silenciosamente (erro
        // ignorado), mas o teste comprova que nem chega a ser necessário.
        let wake_bin = dir.join("nao-existe").display().to_string();

        let result = ensure_reachable_with_options(
            ssh.to_str().unwrap(),
            "feuvault",
            &wake_bin,
            &[],
            0,
            Duration::from_millis(1),
        );

        assert!(result.is_ok());
    }

    #[test]
    fn ensure_reachable_wakes_and_retries_until_reachable() {
        let ssh_dir = test_dir("ensure-reachable-wakes-ssh");
        let wake_dir = test_dir("ensure-reachable-wakes-wake");
        let marker = wake_dir.join("awake");
        let ssh = fake_ssh(
            &ssh_dir,
            &format!("if [ -f {marker:?} ]; then exit 0; else exit 255; fi"),
        );
        let wake = fake_ssh(&wake_dir, &format!("touch {marker:?}"));

        let result = ensure_reachable_with_options(
            ssh.to_str().unwrap(),
            "feuvault",
            wake.to_str().unwrap(),
            &[],
            3,
            Duration::from_millis(1),
        );

        assert!(result.is_ok());
    }

    #[test]
    fn ensure_reachable_gives_up_after_exhausting_retries() {
        let ssh_dir = test_dir("ensure-reachable-gives-up-ssh");
        let wake_dir = test_dir("ensure-reachable-gives-up-wake");
        let ssh = fake_ssh(&ssh_dir, "exit 255");
        let wake = fake_ssh(&wake_dir, "exit 0");

        let result = ensure_reachable_with_options(
            ssh.to_str().unwrap(),
            "feuvault",
            wake.to_str().unwrap(),
            &[],
            2,
            Duration::from_millis(1),
        );

        assert!(matches!(result, Err(RemoteError::NonZeroExit(255, _))));
    }

    #[test]
    fn read_remote_file_returns_content_when_present() {
        let dir = test_dir("read-file-present");
        let ssh = fake_ssh(
            &dir,
            "case \"$*\" in *arquivo*) printf 'conteudo do manifesto';; *) exit 1;; esac",
        );

        let content = read_remote_file(
            ssh.to_str().unwrap(),
            "feuvault",
            Path::new("/vault/arquivo.acf"),
        )
        .unwrap();

        assert_eq!(content, Some("conteudo do manifesto".to_string()));
    }

    #[test]
    fn read_remote_file_returns_none_when_missing() {
        let dir = test_dir("read-file-missing");
        let ssh = fake_ssh(&dir, "exit 44");

        let content =
            read_remote_file(ssh.to_str().unwrap(), "feuvault", Path::new("/vault/x.acf")).unwrap();

        assert_eq!(content, None);
    }

    #[test]
    fn read_remote_file_propagates_a_real_ssh_failure() {
        let dir = test_dir("read-file-fail");
        let ssh = fake_ssh(&dir, "exit 255");

        let result = read_remote_file(ssh.to_str().unwrap(), "feuvault", Path::new("/vault/x.acf"));

        assert!(matches!(result, Err(RemoteError::NonZeroExit(255, _))));
    }

    #[test]
    fn check_remote_path_parses_missing_directory_and_mount_point_states() {
        let dir = test_dir("path-status");

        let missing = fake_ssh(&dir, "printf 'missing\\n'");
        assert_eq!(
            check_remote_path(missing.to_str().unwrap(), "feuvault", Path::new("/vault/x"))
                .unwrap(),
            RemotePathStatus::Missing
        );

        let plain_dir = fake_ssh(&dir, "printf 'dir\\n'");
        assert_eq!(
            check_remote_path(
                plain_dir.to_str().unwrap(),
                "feuvault",
                Path::new("/vault/x")
            )
            .unwrap(),
            RemotePathStatus::Directory
        );

        let mount = fake_ssh(&dir, "printf 'mount\\n'");
        assert_eq!(
            check_remote_path(mount.to_str().unwrap(), "feuvault", Path::new("/vault/x")).unwrap(),
            RemotePathStatus::MountPoint
        );
    }
}
