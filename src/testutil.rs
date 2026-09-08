//! Fixtures compartilhadas pelos módulos de teste — extraído da duplicação
//! mecânica de `test_dir`/`fake_bin`/`write_script` reimplementada quase
//! palavra-por-palavra em ~12 arquivos (ver CLEANUP.md, Camada 2).

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Diretório temporário isolado por chamada: `module` identifica quem
/// pediu (aparece no nome, útil pra depurar um teste que não limpou depois
/// de si), `label` identifica o cenário dentro do módulo. PID + thread ID +
/// tempo evitam colisão de *caminho* entre chamadas concorrentes — a
/// maioria das cópias originais só tinha PID + nanossegundos (repetível em
/// tese, se dois threads caíssem no mesmo nanossegundo), a versão de
/// `session.rs` já incluía o thread ID, adotada aqui como a mais robusta.
/// Não resolve a flakiness intermitente de "Text file busy" observada em
/// `cargo test` sem `--test-threads=1` (confirmada preexistente, não
/// causada nem corrigida por esta consolidação — ver CLEANUP.md): essa
/// falha é por execução de um script recém-escrito ainda "ocupado", não
/// por colisão de caminho, e continua acontecendo mesmo com caminhos 100%
/// únicos. Investigar como item separado; `--test-threads=1` continua
/// necessário pra uma execução local confiável.
pub(crate) fn test_dir(module: &str, label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "vault-sync-{module}-test-{label}-{}-{}",
        std::process::id(),
        unique_suffix()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn unique_suffix() -> String {
    format!(
        "{:?}-{}",
        std::thread::current().id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )
    .replace(['(', ')', ' '], "")
}

/// Escreve um script shell executável em `dir/name` — usado pra simular
/// binários externos (rsync/ssh/steamcmd/legendary) nos testes de
/// orquestração, sem depender do binário real instalado no sistema.
pub(crate) fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    let mut f = File::create(&path).unwrap();
    writeln!(f, "#!/bin/sh\n{body}").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    path
}
