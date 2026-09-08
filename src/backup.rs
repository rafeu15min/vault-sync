//! Salvaguarda atômica pregressa a qualquer mutação de arquivo de
//! configuração restrito (`shortcuts.vdf`, registro do Wine) — 4.2. Mitiga
//! o risco central da Fase IV apontado na spec: degradação catastrófica de
//! um arquivo binário proprietário por uma escrita mal-sucedida.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Copia `path` para um arquivo irmão com sufixo `.vault-sync-bak-<ts>`
/// *antes* de qualquer escrita em `path`. A cópia em si é atômica: o
/// conteúdo é gravado num arquivo temporário no mesmo diretório e só então
/// promovido via `rename` (atômico no mesmo sistema de arquivos), então uma
/// interrupção no meio da gravação nunca deixa um backup corrompido no lugar
/// de um conteúdo válido.
pub fn atomic_backup(path: &Path) -> std::io::Result<PathBuf> {
    let original = std::fs::read(path)?;

    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "backup".to_string());

    let backup_path = path.with_file_name(format!("{file_name}.vault-sync-bak-{ts}"));
    let tmp_path = path.with_file_name(format!("{file_name}.vault-sync-bak-{ts}.tmp"));

    std::fs::write(&tmp_path, &original)?;
    std::fs::rename(&tmp_path, &backup_path)?;
    Ok(backup_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn test_dir(label: &str) -> PathBuf {
        crate::testutil::test_dir("backup", label)
    }

    #[test]
    fn backup_preserves_original_content_byte_for_byte() {
        let dir = test_dir("content");
        let original = dir.join("shortcuts.vdf");
        fs::write(&original, b"\x00shortcuts\x00\x08").unwrap();

        let backup_path = atomic_backup(&original).unwrap();

        assert!(backup_path.exists());
        assert_eq!(
            fs::read(&backup_path).unwrap(),
            fs::read(&original).unwrap()
        );
        assert_ne!(backup_path, original);
    }

    #[test]
    fn backup_leaves_no_temp_file_behind() {
        let dir = test_dir("no-tmp");
        let original = dir.join("shortcuts.vdf");
        fs::write(&original, b"conteudo").unwrap();

        atomic_backup(&original).unwrap();

        let leftover_tmp = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .any(|e| e.file_name().to_string_lossy().ends_with(".tmp"));
        assert!(
            !leftover_tmp,
            "arquivo temporário de backup não foi promovido"
        );
    }
}
