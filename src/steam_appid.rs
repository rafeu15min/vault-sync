//! Cálculo do AppID que a Steam atribui a atalhos non-Steam — CRC32 do
//! caminho do executável (sem aspas) concatenado com o nome do jogo,
//! mascarado com `0xFFFFFFFF` e OR'd com `0x80000000` (o bit alto ligado é
//! o que distingue AppIDs "sintéticos" — non-Steam — dos oficiais).
//! Reverse-engenheirado e validado byte a byte contra dois AppIDs reais
//! desta máquina (Haste, antes e depois de um remanejo de disco — ver
//! `inject::stage_pirata_shortcut`) antes de qualquer uso em escrita. Sem
//! esse cálculo estar certo, criar um atalho programaticamente geraria um
//! AppID que a própria Steam nunca reconheceria como o mesmo jogo.

pub fn compute_app_id(exe: &str, name: &str) -> u32 {
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(exe.as_bytes());
    hasher.update(name.as_bytes());
    hasher.finalize() | 0x8000_0000
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regressão de dados reais: AppID observado nesta máquina antes de um
    /// remanejo de disco (jogo ainda em `/mnt/SSD2/...`).
    #[test]
    fn matches_haste_appid_before_the_disk_reshuffle() {
        assert_eq!(
            compute_app_id(
                "/mnt/SSD2/Piratas/HASTE.v1.5.c/HASTE.v1.5.c/Haste.exe",
                "Haste"
            ),
            2385817016
        );
    }

    /// Mesma regressão, depois do jogo consolidado em
    /// `local_game_root_piratas` — os dois valores reais confirmam o
    /// algoritmo (CRC32 de Exe+nome) independente do caminho em si.
    #[test]
    fn matches_haste_appid_after_consolidating_into_local_game_root_piratas() {
        assert_eq!(
            compute_app_id("/home/rafeu/Games/piratas/Haste/Haste.exe", "Haste"),
            2892464705
        );
    }
}
