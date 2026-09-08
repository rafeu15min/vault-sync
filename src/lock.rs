use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum LockError {
    #[error("já existe uma instância do vault-sync em execução (lock em {0})")]
    AlreadyLocked(PathBuf),
    #[error("falha de I/O ao manipular lockfile {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Guarda RAII de um lockfile: a criação é atômica (`create_new`, que falha
/// se o arquivo já existir), impedindo duas instâncias concorrentes do
/// vault-sync. O arquivo é removido automaticamente quando o guard sai de
/// escopo — inclusive em caminhos de erro, via unwind normal do Rust.
#[derive(Debug)]
pub struct LockGuard {
    path: PathBuf,
}

impl LockGuard {
    /// Cria o lockfile; se já existe e o PID gravado nele não corresponde a
    /// nenhum processo vivo, assume um lock morto (o `Drop` só roda em
    /// unwind normal — `kill -9`, crash ou queda de energia nunca o
    /// executam) e o retoma sozinho, em vez de bloquear todo `run`/
    /// `ingest`/`purge` futuro permanentemente até alguém apagar o arquivo
    /// na mão. Incidente real: um `vault-sync run` de teste morto sem
    /// aguardar o próprio encerramento deixou o lock travado, e todo
    /// lançamento seguinte — pirata, oficial, Heroic, sem distinção —
    /// falhava com "instância concorrente detectada".
    pub fn acquire(path: impl Into<PathBuf>) -> Result<Self, LockError> {
        let path = path.into();
        match Self::create_new(&path) {
            Ok(()) => Ok(Self { path }),
            Err(LockError::AlreadyLocked(_)) if Self::holder_is_dead(&path) => {
                let _ = fs::remove_file(&path);
                Self::create_new(&path).map(|()| Self { path })
            }
            Err(e) => Err(e),
        }
    }

    fn create_new(path: &Path) -> Result<(), LockError> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|source| {
                if source.kind() == std::io::ErrorKind::AlreadyExists {
                    LockError::AlreadyLocked(path.to_path_buf())
                } else {
                    LockError::Io {
                        path: path.to_path_buf(),
                        source,
                    }
                }
            })?;

        // PID gravado tanto pra diagnóstico manual (`cat` no lockfile)
        // quanto pra detecção de lock morto acima (`holder_is_dead`).
        let _ = write!(file, "{}", std::process::id());

        Ok(())
    }

    /// `true` quando o lockfile existe mas o PID gravado nele não
    /// corresponde a nenhum processo vivo — inclusive quando o conteúdo
    /// está ausente/corrompido (lock de uma versão antiga, ou gravação
    /// interrompida no meio), já que nesses casos também não há como o
    /// processo original ainda estar de pé segurando esse mesmo PID.
    fn holder_is_dead(path: &Path) -> bool {
        let Ok(contents) = fs::read_to_string(path) else {
            return true;
        };
        match contents.trim().parse::<u32>() {
            Ok(pid) => !pid_is_alive(pid),
            Err(_) => true,
        }
    }

    /// Caminho padrão do lockfile: `$XDG_RUNTIME_DIR/vault-sync.lock`, com
    /// fallback para o diretório temporário do sistema quando
    /// `XDG_RUNTIME_DIR` não está definido.
    pub fn default_path() -> PathBuf {
        dirs::runtime_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join("vault-sync.lock")
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Mesma checagem de `session::pid_is_alive` — duplicada aqui de propósito
/// (uma linha, sem justificar acoplar `lock.rs` a `session.rs` por isso).
fn pid_is_alive(pid: u32) -> bool {
    Path::new("/proc").join(pid.to_string()).exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_lock_path(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "vault-sync-lock-test-{label}-{}.lock",
            std::process::id()
        ))
    }

    #[test]
    fn acquire_creates_file_and_drop_removes_it() {
        let path = temp_lock_path("basic");
        assert!(!path.exists());

        {
            let guard = LockGuard::acquire(&path).unwrap();
            assert!(path.exists());
            assert_eq!(guard.path(), path.as_path());
        }

        assert!(
            !path.exists(),
            "o lockfile deveria ter sido removido no Drop"
        );
    }

    #[test]
    fn second_acquire_fails_while_first_guard_is_alive() {
        let path = temp_lock_path("concurrent");
        let _first = LockGuard::acquire(&path).unwrap();

        let err = LockGuard::acquire(&path).unwrap_err();
        assert!(matches!(err, LockError::AlreadyLocked(p) if p == path));
    }

    #[test]
    fn acquire_succeeds_again_after_previous_guard_dropped() {
        let path = temp_lock_path("reacquire");
        {
            let _guard = LockGuard::acquire(&path).unwrap();
        }
        let guard = LockGuard::acquire(&path);
        assert!(guard.is_ok());
    }

    #[test]
    fn acquire_takes_over_a_lock_left_by_a_dead_pid() {
        let path = temp_lock_path("stale-dead-pid");
        // PID acima do limite prático do kernel — não deveria existir de
        // verdade (mesmo limite usado em session::pid_is_alive_reflects_real_proc_state).
        fs::write(&path, "4194304").unwrap();

        let guard = LockGuard::acquire(&path);

        assert!(
            guard.is_ok(),
            "lock morto deveria ser retomado, não bloquear"
        );
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            std::process::id().to_string()
        );
    }

    #[test]
    fn acquire_takes_over_a_lock_with_corrupted_content() {
        let path = temp_lock_path("stale-corrupt");
        fs::write(&path, "isto nao e um pid").unwrap();

        let guard = LockGuard::acquire(&path);

        assert!(
            guard.is_ok(),
            "lock corrompido deveria ser retomado, não bloquear"
        );
    }

    #[test]
    fn acquire_still_refuses_a_lock_held_by_a_live_pid() {
        let path = temp_lock_path("live-holder");
        // O próprio processo de teste está vivo por definição.
        fs::write(&path, std::process::id().to_string()).unwrap();

        let err = LockGuard::acquire(&path).unwrap_err();

        assert!(matches!(err, LockError::AlreadyLocked(p) if p == path));
        // Não deve ter mexido no lock alheio.
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            std::process::id().to_string()
        );
    }
}
