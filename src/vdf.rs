//! Codec do formato binário compilado usado por `shortcuts.vdf` (4.2). Ao
//! contrário do `steam_scan.py`, que só extraía pares `appid`/`appname` via
//! heurística de regex sobre os bytes, aqui a árvore inteira é decodificada
//! numa estrutura genérica e pode ser recodificada byte a byte — pré-requisito
//! para o motor de injeção (4.3) conseguir alterar um único campo sem perder
//! nenhum dado desconhecido do resto do arquivo.
//!
//! Formato (não documentado oficialmente, reverso a partir de arquivos reais):
//! cada entrada começa com um byte de tipo, seguido de uma chave terminada em
//! `\0`:
//! - `0x00` objeto aninhado — os filhos seguem até um `0x08` de fechamento.
//! - `0x01` string — valor também terminado em `\0`.
//! - `0x02` inteiro de 32 bits little-endian.
//!
//! O nível raiz não tem envelope próprio (sem byte de tipo/chave/fechamento):
//! é uma sequência de entradas que termina no fim do arquivo.

/// Nó de uma árvore VDF binária. Objetos preservam a ordem original das
/// chaves (`Vec`, não `HashMap`) porque o Steam é sensível à ordem dos
/// campos ao reler o arquivo.
#[derive(Debug, Clone, PartialEq)]
pub enum VdfValue {
    Str(String),
    Int(i32),
    Object(VdfObject),
}

pub type VdfObject = Vec<(String, VdfValue)>;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum VdfError {
    #[error("fim inesperado do arquivo binário VDF")]
    UnexpectedEof,
    #[error("byte de tipo desconhecido no VDF binário: 0x{0:02x}")]
    UnknownTypeByte(u8),
    #[error("chave ou valor de string VDF não é UTF-8 válido")]
    InvalidUtf8,
}

const TYPE_OBJECT: u8 = 0x00;
const TYPE_STRING: u8 = 0x01;
const TYPE_INT32: u8 = 0x02;
const TYPE_END: u8 = 0x08;

/// Decodifica um `shortcuts.vdf` (ou qualquer VDF binário do mesmo formato)
/// completo a partir dos bytes brutos do arquivo.
pub fn parse_binary(bytes: &[u8]) -> Result<VdfObject, VdfError> {
    let mut pos = 0;
    parse_object(bytes, &mut pos, false)
}

fn parse_object(bytes: &[u8], pos: &mut usize, nested: bool) -> Result<VdfObject, VdfError> {
    let mut entries = Vec::new();
    loop {
        if *pos >= bytes.len() {
            if nested {
                return Err(VdfError::UnexpectedEof);
            }
            return Ok(entries);
        }
        let type_byte = bytes[*pos];
        *pos += 1;
        if type_byte == TYPE_END {
            return Ok(entries);
        }

        let key = read_cstring(bytes, pos)?;
        let value = match type_byte {
            TYPE_OBJECT => VdfValue::Object(parse_object(bytes, pos, true)?),
            TYPE_STRING => VdfValue::Str(read_cstring(bytes, pos)?),
            TYPE_INT32 => VdfValue::Int(read_i32(bytes, pos)?),
            other => return Err(VdfError::UnknownTypeByte(other)),
        };
        entries.push((key, value));
    }
}

fn read_cstring(bytes: &[u8], pos: &mut usize) -> Result<String, VdfError> {
    let start = *pos;
    while *pos < bytes.len() && bytes[*pos] != 0 {
        *pos += 1;
    }
    if *pos >= bytes.len() {
        return Err(VdfError::UnexpectedEof);
    }
    let s = std::str::from_utf8(&bytes[start..*pos])
        .map_err(|_| VdfError::InvalidUtf8)?
        .to_string();
    *pos += 1; // pula o terminador nulo
    Ok(s)
}

fn read_i32(bytes: &[u8], pos: &mut usize) -> Result<i32, VdfError> {
    if *pos + 4 > bytes.len() {
        return Err(VdfError::UnexpectedEof);
    }
    let arr: [u8; 4] = bytes[*pos..*pos + 4].try_into().unwrap();
    *pos += 4;
    Ok(i32::from_le_bytes(arr))
}

/// Recodifica uma árvore para os bytes brutos do formato binário —
/// operação inversa de [`parse_binary`], usada pelo motor de injeção (4.3)
/// para regravar o arquivo após alterar um campo pontual.
pub fn write_binary(root: &VdfObject) -> Vec<u8> {
    let mut out = Vec::new();
    write_entries(root, &mut out);
    out
}

fn write_entries(entries: &VdfObject, out: &mut Vec<u8>) {
    for (key, value) in entries {
        match value {
            VdfValue::Object(child) => {
                out.push(TYPE_OBJECT);
                write_cstring(key, out);
                write_entries(child, out);
                out.push(TYPE_END);
            }
            VdfValue::Str(s) => {
                out.push(TYPE_STRING);
                write_cstring(key, out);
                write_cstring(s, out);
            }
            VdfValue::Int(i) => {
                out.push(TYPE_INT32);
                write_cstring(key, out);
                out.extend_from_slice(&i.to_le_bytes());
            }
        }
    }
}

fn write_cstring(s: &str, out: &mut Vec<u8>) {
    out.extend_from_slice(s.as_bytes());
    out.push(0);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Monta um `shortcuts.vdf` mínimo com um atalho, no mesmo layout de
    /// campos observado em arquivos reais (`appid`/`appname` em minúsculas,
    /// como já validado empiricamente pelo `steam_scan.py`).
    fn sample_tree() -> VdfObject {
        vec![(
            "shortcuts".to_string(),
            VdfValue::Object(vec![(
                "0".to_string(),
                VdfValue::Object(vec![
                    ("appid".to_string(), VdfValue::Int(-1234567890)),
                    ("appname".to_string(), VdfValue::Str("Exemplo".to_string())),
                    ("Exe".to_string(), VdfValue::Str("/bin/exemplo".to_string())),
                    ("tags".to_string(), VdfValue::Object(vec![])),
                ]),
            )]),
        )]
    }

    #[test]
    fn round_trip_preserves_structure() {
        let tree = sample_tree();
        let bytes = write_binary(&tree);
        let parsed = parse_binary(&bytes).unwrap();
        assert_eq!(parsed, tree);
    }

    #[test]
    fn nested_object_missing_end_byte_is_unexpected_eof() {
        // 0x00 "shortcuts\0" sem o 0x08 de fechamento correspondente.
        let mut bytes = vec![TYPE_OBJECT];
        bytes.extend_from_slice(b"shortcuts\0");
        let err = parse_binary(&bytes).unwrap_err();
        assert_eq!(err, VdfError::UnexpectedEof);
    }

    #[test]
    fn unknown_type_byte_is_rejected() {
        let bytes = vec![0xff, b'x', 0];
        let err = parse_binary(&bytes).unwrap_err();
        assert_eq!(err, VdfError::UnknownTypeByte(0xff));
    }

    #[test]
    fn root_level_reads_until_eof_without_wrapper() {
        let tree = sample_tree();
        let bytes = write_binary(&tree);
        // Sem fechamento extra no nível raiz: o arquivo termina logo após o
        // 0x08 que fecha o objeto "shortcuts".
        assert_eq!(*bytes.last().unwrap(), TYPE_END);
        assert!(parse_binary(&bytes).is_ok());
    }

    #[test]
    fn int32_round_trips_negative_values_as_unsigned_appid() {
        // appids grandes viram i32 negativo na representação binária; a
        // reconstrução do valor sem sinal é responsabilidade de quem
        // interpreta o campo (scan.rs), não do codec.
        let tree = vec![("appid".to_string(), VdfValue::Int(-1))];
        let bytes = write_binary(&tree);
        let parsed = parse_binary(&bytes).unwrap();
        assert_eq!(parsed, tree);
        if let VdfValue::Int(i) = parsed[0].1 {
            assert_eq!(i as u32, u32::MAX);
        } else {
            panic!("esperava VdfValue::Int");
        }
    }
}
