//! Parser/serializer do formato de TEXTO do VDF (KeyValues) — usado por
//! `localconfig.vdf`, `config.vdf` e outros arquivos de configuração da
//! Steam, diferente do formato BINÁRIO de `shortcuts.vdf` (ver `vdf.rs`).
//! Reaproveita a mesma árvore genérica (`VdfObject`/`VdfValue`) em vez de
//! duplicar a modelagem: no texto, todo valor é string — a Steam grava até
//! número como string literal (`"233860"`) — então `VdfValue::Int` nunca
//! aparece num resultado deste módulo, só `Str`/`Object`.
//!
//! Formato: pares `"chave" "valor"` ou `"chave" { ... }`, indentação por
//! tabs (não significativa pro parser, só estética), aspas escapadas como
//! `\"` e barra invertida como `\\` dentro de strings. Comentários `//` até
//! o fim da linha são tolerados (raros em arquivos gerados pela própria
//! Steam, mas o parser não falha se aparecerem). Ao contrário do binário
//! (onde a ordem/bytes exatos importam e a árvore é reescrita campo a
//! campo), este módulo reserializa a árvore inteira — a Steam relê KeyValues
//! em texto sem se importar com espaçamento exato, só com a estrutura.

use crate::vdf::{VdfObject, VdfValue};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TextVdfError {
    #[error("fim inesperado do arquivo de texto VDF")]
    UnexpectedEof,
    #[error("esperava uma chave (string entre aspas) na posição {0}")]
    ExpectedKey(usize),
    #[error("esperava um valor (string ou '{{') na posição {0}")]
    ExpectedValue(usize),
}

/// Decodifica um KeyValues em texto completo a partir do conteúdo já lido
/// como string. Nível raiz: sequência de pares chave/valor até o fim do
/// arquivo (igual ao nível raiz do formato binário em `vdf.rs`) — os
/// arquivos reais da Steam têm só uma chave no topo
/// (`"UserLocalConfigStore"`), mas o parser não assume isso.
pub fn parse_text(input: &str) -> Result<VdfObject, TextVdfError> {
    let chars: Vec<char> = input.chars().collect();
    let mut pos = 0usize;
    parse_object(&chars, &mut pos, false)
}

fn skip_ws_and_comments(chars: &[char], pos: &mut usize) {
    loop {
        while *pos < chars.len() && chars[*pos].is_whitespace() {
            *pos += 1;
        }
        if *pos + 1 < chars.len() && chars[*pos] == '/' && chars[*pos + 1] == '/' {
            while *pos < chars.len() && chars[*pos] != '\n' {
                *pos += 1;
            }
            continue;
        }
        break;
    }
}

fn read_quoted_string(chars: &[char], pos: &mut usize) -> Result<String, TextVdfError> {
    if chars.get(*pos) != Some(&'"') {
        return Err(TextVdfError::ExpectedKey(*pos));
    }
    *pos += 1;
    let mut out = String::new();
    loop {
        match chars.get(*pos) {
            None => return Err(TextVdfError::UnexpectedEof),
            Some('"') => {
                *pos += 1;
                return Ok(out);
            }
            Some('\\') => {
                *pos += 1;
                match chars.get(*pos) {
                    Some(c) => {
                        out.push(*c);
                        *pos += 1;
                    }
                    None => return Err(TextVdfError::UnexpectedEof),
                }
            }
            Some(c) => {
                out.push(*c);
                *pos += 1;
            }
        }
    }
}

fn parse_object(chars: &[char], pos: &mut usize, nested: bool) -> Result<VdfObject, TextVdfError> {
    let mut entries = Vec::new();
    loop {
        skip_ws_and_comments(chars, pos);
        match chars.get(*pos) {
            None => {
                if nested {
                    return Err(TextVdfError::UnexpectedEof);
                }
                return Ok(entries);
            }
            Some('}') => {
                if nested {
                    *pos += 1;
                    return Ok(entries);
                }
                return Err(TextVdfError::ExpectedKey(*pos));
            }
            Some('"') => {
                let key = read_quoted_string(chars, pos)?;
                skip_ws_and_comments(chars, pos);
                let value = match chars.get(*pos) {
                    Some('"') => VdfValue::Str(read_quoted_string(chars, pos)?),
                    Some('{') => {
                        *pos += 1;
                        VdfValue::Object(parse_object(chars, pos, true)?)
                    }
                    _ => return Err(TextVdfError::ExpectedValue(*pos)),
                };
                entries.push((key, value));
            }
            _ => return Err(TextVdfError::ExpectedKey(*pos)),
        }
    }
}

/// Reserializa a árvore inteira, com a mesma indentação por tabs que a
/// própria Steam usa (não é exigido, mas mantém o arquivo legível se
/// alguém for olhar). `VdfValue::Int` nunca deveria aparecer numa árvore
/// vinda deste módulo — se aparecer (ex.: árvore construída manualmente
/// por engano), é serializado como o próprio número decimal, mesmo
/// comportamento textual que a Steam usa pra tudo.
pub fn write_text(tree: &VdfObject) -> String {
    let mut out = String::new();
    write_object(tree, 0, &mut out);
    out
}

fn write_object(obj: &VdfObject, depth: usize, out: &mut String) {
    for (key, value) in obj {
        push_indent(out, depth);
        push_quoted(out, key);
        match value {
            VdfValue::Str(s) => {
                out.push('\t');
                push_quoted(out, s);
                out.push('\n');
            }
            VdfValue::Int(i) => {
                out.push('\t');
                out.push_str(&i.to_string());
                out.push('\n');
            }
            VdfValue::Object(inner) => {
                out.push('\n');
                push_indent(out, depth);
                out.push_str("{\n");
                write_object(inner, depth + 1, out);
                push_indent(out, depth);
                out.push_str("}\n");
            }
        }
    }
}

fn push_indent(out: &mut String, depth: usize) {
    for _ in 0..depth {
        out.push('\t');
    }
}

fn push_quoted(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_key_value_pairs() {
        let input = r#""a"		"1"
"b"		"2"
"#;
        let tree = parse_text(input).unwrap();
        assert_eq!(
            tree,
            vec![
                ("a".to_string(), VdfValue::Str("1".to_string())),
                ("b".to_string(), VdfValue::Str("2".to_string())),
            ]
        );
    }

    #[test]
    fn parses_nested_objects() {
        let input = r#""root"
{
	"child"
	{
		"leaf"		"valor"
	}
}
"#;
        let tree = parse_text(input).unwrap();
        let VdfValue::Object(root_fields) = &tree[0].1 else {
            panic!("esperava objeto")
        };
        let VdfValue::Object(child_fields) = &root_fields[0].1 else {
            panic!("esperava objeto aninhado")
        };
        assert_eq!(
            child_fields[0],
            ("leaf".to_string(), VdfValue::Str("valor".to_string()))
        );
    }

    #[test]
    fn handles_escaped_quotes_and_backslashes() {
        let input = r#""chave"		"valor com \"aspas\" e \\barra""#;
        let tree = parse_text(input).unwrap();
        assert_eq!(
            tree[0].1,
            VdfValue::Str("valor com \"aspas\" e \\barra".to_string())
        );
    }

    #[test]
    fn skips_line_comments() {
        let input = "// comentário solto\n\"a\"\t\"1\" // outro comentário\n";
        let tree = parse_text(input).unwrap();
        assert_eq!(
            tree,
            vec![("a".to_string(), VdfValue::Str("1".to_string()))]
        );
    }

    #[test]
    fn round_trips_through_write_and_parse() {
        let original = vec![(
            "root".to_string(),
            VdfValue::Object(vec![
                ("leaf".to_string(), VdfValue::Str("valor".to_string())),
                (
                    "nested".to_string(),
                    VdfValue::Object(vec![(
                        "deep".to_string(),
                        VdfValue::Str("com \"aspas\"".to_string()),
                    )]),
                ),
            ]),
        )];

        let text = write_text(&original);
        let reparsed = parse_text(&text).unwrap();
        assert_eq!(reparsed, original);
    }

    #[test]
    fn unexpected_eof_inside_nested_object_is_an_error() {
        let input = r#""root"
{
	"leaf"		"valor"
"#;
        assert_eq!(parse_text(input), Err(TextVdfError::UnexpectedEof));
    }
}
