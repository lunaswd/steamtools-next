//! A bounded parser for Valve's text KeyValues format. Directives are data, never followed.
use anyhow::{bail, Context, Result};
use std::{fs::File, io::Read, path::Path};

const MAX_BYTES: usize = 8 * 1024 * 1024;
const MAX_DEPTH: usize = 64;
const MAX_TOKENS: usize = 500_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    Text(String),
    Object(Vec<(String, Value)>),
}

impl Value {
    pub fn get(&self, name: &str) -> Option<&Value> {
        self.entries()?
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value)
    }

    pub fn text(&self) -> Option<&str> {
        match self {
            Self::Text(value) => Some(value),
            Self::Object(_) => None,
        }
    }

    pub fn entries(&self) -> Option<&[(String, Value)]> {
        match self {
            Self::Object(entries) => Some(entries),
            Self::Text(_) => None,
        }
    }

    pub fn string(&self, name: &str) -> Option<&str> {
        self.get(name)?.text()
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Token {
    Text(String),
    Open,
    Close,
}

pub fn read(path: &Path) -> Result<Value> {
    let mut bytes = Vec::new();
    File::open(path)
        .with_context(|| format!("Could not open {}", path.display()))?
        .take((MAX_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_BYTES {
        bail!("VDF exceeds the 8 MiB inspection limit");
    }
    let text = if bytes.starts_with(&[0xff, 0xfe]) || bytes.starts_with(&[0xfe, 0xff]) {
        if (bytes.len() - 2) % 2 != 0 {
            bail!("Truncated UTF-16 VDF");
        }
        let little = bytes[0] == 0xff;
        let words: Vec<u16> = bytes[2..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|p| {
                if little {
                    u16::from_le_bytes([p[0], p[1]])
                } else {
                    u16::from_be_bytes([p[0], p[1]])
                }
            })
            .collect();
        String::from_utf16(&words).context("VDF contains invalid UTF-16")?
    } else {
        String::from_utf8(bytes).context("VDF is not valid UTF-8")?
    };
    parse(&text)
}

pub fn parse(input: &str) -> Result<Value> {
    if input.len() > MAX_BYTES {
        bail!("VDF exceeds the 8 MiB inspection limit");
    }
    let tokens = tokenize(input.trim_start_matches('\u{feff}'))?;
    let mut index = 0;
    let root = object(&tokens, &mut index, 0, false)?;
    if index != tokens.len() {
        bail!("Unexpected trailing VDF token");
    }
    Ok(root)
}

fn tokenize(input: &str) -> Result<Vec<Token>> {
    let mut result = Vec::new();
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch.is_whitespace() {
            continue;
        }
        if ch == '/' && chars.peek() == Some(&'/') {
            for c in chars.by_ref() {
                if c == '\n' {
                    break;
                }
            }
            continue;
        }
        let token = match ch {
            '{' => Token::Open,
            '}' => Token::Close,
            '"' => {
                let mut value = String::new();
                let mut closed = false;
                while let Some(c) = chars.next() {
                    match c {
                        '"' => {
                            closed = true;
                            break;
                        }
                        '\\' => {
                            let escaped = chars.next().context("Unterminated VDF escape")?;
                            match escaped {
                                '"' | '\\' => value.push(escaped),
                                'n' => value.push('\n'),
                                'r' => value.push('\r'),
                                't' => value.push('\t'),
                                _ => {
                                    value.push('\\');
                                    value.push(escaped);
                                }
                            }
                        }
                        '\0' => bail!("NUL is not allowed in a VDF string"),
                        _ => value.push(c),
                    }
                }
                if !closed {
                    bail!("Unterminated VDF string");
                }
                Token::Text(value)
            }
            '\0' => bail!("NUL is not allowed in VDF"),
            _ => {
                let mut value = String::from(ch);
                while let Some(&c) = chars.peek() {
                    if c.is_whitespace() || c == '{' || c == '}' {
                        break;
                    }
                    if c == '"' || c == '\0' {
                        bail!("Invalid unquoted VDF token");
                    }
                    value.push(chars.next().unwrap());
                }
                Token::Text(value)
            }
        };
        result.push(token);
        if result.len() > MAX_TOKENS {
            bail!("VDF exceeds token limit");
        }
    }
    Ok(result)
}

fn object(tokens: &[Token], index: &mut usize, depth: usize, nested: bool) -> Result<Value> {
    if depth > MAX_DEPTH {
        bail!("VDF nesting exceeds 64 levels");
    }
    let mut entries = Vec::new();
    while *index < tokens.len() {
        let key = match &tokens[*index] {
            Token::Close if nested => {
                *index += 1;
                return Ok(Value::Object(entries));
            }
            Token::Text(key) => key.clone(),
            _ => bail!("Expected a VDF key"),
        };
        *index += 1;
        let value = match tokens.get(*index) {
            Some(Token::Text(text)) => {
                *index += 1;
                Value::Text(text.clone())
            }
            Some(Token::Open) => {
                *index += 1;
                object(tokens, index, depth + 1, true)?
            }
            _ => bail!("Expected a VDF value"),
        };
        entries.push((key, value));
    }
    if nested {
        bail!("Unclosed VDF object");
    }
    Ok(Value::Object(entries))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_modern_libraries_and_unicode() {
        let value = parse("\u{feff}// start\n\"libraryfolders\" { \"0\" { \"path\" \"C:\\\\游戏\\\\Steam\" \"apps\" {\"570\" \"123\"} } }").unwrap();
        assert_eq!(
            value
                .get("LibraryFolders")
                .unwrap()
                .get("0")
                .unwrap()
                .string("path"),
            Some("C:\\游戏\\Steam")
        );
    }

    #[test]
    fn handles_unquoted_comments_and_escapes() {
        let v = parse("root { empty \"\" quoted \"a\\\"b\" text \"a\\nb\" url \"https://example.invalid/a\" } //comment").unwrap();
        let root = v.get("root").unwrap();
        assert_eq!(root.string("quoted"), Some("a\"b"));
        assert_eq!(root.string("text"), Some("a\nb"));
        assert_eq!(root.string("empty"), Some(""));
    }

    #[test]
    fn rejects_truncation_and_bad_braces() {
        for bad in [
            "\"key\"",
            "root { key value",
            "}",
            "key }",
            "key \"oops",
            "{ key value }",
            "key a\0b",
        ] {
            assert!(parse(bad).is_err(), "accepted malformed structure");
        }
    }

    #[test]
    fn caps_recursive_depth() {
        let input = format!("{}{}", "x {".repeat(66), "}".repeat(66));
        assert!(parse(&input).is_err());
    }

    #[test]
    fn utf16_little_endian_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.vdf");
        let mut bytes = vec![0xff, 0xfe];
        for value in "root { name \"日本語\" }".encode_utf16() {
            bytes.extend(value.to_le_bytes());
        }
        std::fs::write(&path, bytes).unwrap();
        assert_eq!(
            read(&path).unwrap().get("root").unwrap().string("name"),
            Some("日本語")
        );
    }
}
