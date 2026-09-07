use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use crate::error::{BridgeError, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsoncPatch {
    pub text: String,
    pub previous: Option<String>,
}

#[derive(Debug)]
struct Member {
    key: String,
    property_start: usize,
    value_start: usize,
    value_end: usize,
    delimiter: usize,
    comma_after: bool,
}

#[derive(Debug)]
struct ObjectLayout {
    members: Vec<Member>,
    close: usize,
}

pub fn patch_jsonc_string(text: &str, key: &str, value: &str) -> Result<JsoncPatch> {
    let layout = parse_root_object(text)?;
    let matches = layout
        .members
        .iter()
        .filter(|member| member.key == key)
        .collect::<Vec<_>>();
    if matches.len() > 1 {
        return Err(BridgeError::InvalidConfig(format!(
            "duplicate top-level key {key:?}"
        )));
    }
    let encoded = serde_json::to_string(value).expect("strings always serialize");
    if let Some(member) = matches.first() {
        let (previous, string_end) = parse_string_value(text, member)?;
        let mut output = text.to_owned();
        output.replace_range(member.value_start..string_end, &encoded);
        return Ok(JsoncPatch {
            text: output,
            previous: Some(previous),
        });
    }

    let encoded_key = serde_json::to_string(key).expect("strings always serialize");
    let indentation = layout
        .members
        .first()
        .map(|member| line_indentation(text, member.property_start))
        .unwrap_or_else(|| "  ".to_string());
    let property = format!("{indentation}{encoded_key}: {encoded}");
    let (insert_at, insertion) = match layout.members.last() {
        Some(last) if last.comma_after => (last.delimiter + 1, format!("\n{property}")),
        Some(last) => (last.value_end, format!(",\n{property}")),
        None => (layout.close, format!("\n{property}\n")),
    };
    let mut output = text.to_owned();
    output.insert_str(insert_at, &insertion);
    Ok(JsoncPatch {
        text: output,
        previous: None,
    })
}

pub fn read_jsonc_string(text: &str, key: &str) -> Result<Option<String>> {
    let layout = parse_root_object(text)?;
    let matches = layout
        .members
        .iter()
        .filter(|member| member.key == key)
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [] => Ok(None),
        [member] => parse_string_value(text, member).map(|(value, _)| Some(value)),
        _ => Err(BridgeError::InvalidConfig(format!(
            "duplicate top-level key {key:?}"
        ))),
    }
}

pub fn restore_jsonc_string(
    text: &str,
    key: &str,
    installed: &str,
    previous: Option<&str>,
) -> Result<String> {
    let layout = parse_root_object(text)?;
    let matches = layout
        .members
        .iter()
        .enumerate()
        .filter(|(_, member)| member.key == key)
        .collect::<Vec<_>>();
    if matches.len() != 1 {
        return Err(BridgeError::Conflict(format!(
            "expected exactly one installed {key:?} key, found {}",
            matches.len()
        )));
    }
    let (index, member) = matches[0];
    let (current, _) = parse_string_value(text, member)?;
    if current != installed {
        return Err(BridgeError::Conflict(format!(
            "{key:?} was changed after installation"
        )));
    }
    if let Some(previous) = previous {
        return patch_jsonc_string(text, key, previous).map(|patch| patch.text);
    }

    let range = if member.comma_after {
        member.property_start..member.delimiter + 1
    } else if index > 0 && layout.members[index - 1].comma_after {
        layout.members[index - 1].delimiter..member.value_end
    } else {
        member.property_start..member.value_end
    };
    let mut output = text.to_owned();
    output.replace_range(range, "");
    Ok(output)
}

pub fn write_atomic(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    let parent = path.parent().ok_or_else(|| {
        BridgeError::InvalidConfig(format!("path has no parent: {}", path.display()))
    })?;
    std::fs::create_dir_all(parent).map_err(|source| BridgeError::Write {
        path: parent.to_path_buf(),
        source,
    })?;
    let mut temporary =
        tempfile::NamedTempFile::new_in(parent).map_err(|source| BridgeError::Write {
            path: parent.to_path_buf(),
            source,
        })?;
    temporary
        .as_file()
        .set_permissions(std::fs::Permissions::from_mode(mode))
        .map_err(|source| BridgeError::Write {
            path: temporary.path().to_path_buf(),
            source,
        })?;
    temporary
        .write_all(bytes)
        .and_then(|_| temporary.flush())
        .and_then(|_| temporary.as_file().sync_all())
        .map_err(|source| BridgeError::Write {
            path: temporary.path().to_path_buf(),
            source,
        })?;
    temporary
        .persist(path)
        .map_err(|error| BridgeError::Write {
            path: path.to_path_buf(),
            source: error.error,
        })?;
    std::fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| BridgeError::Write {
            path: parent.to_path_buf(),
            source,
        })?;
    Ok(())
}

fn parse_root_object(text: &str) -> Result<ObjectLayout> {
    let bytes = text.as_bytes();
    let mut cursor = skip_trivia(bytes, 0)?;
    if bytes.get(cursor) != Some(&b'{') {
        return Err(BridgeError::InvalidConfig(
            "root value must be an object".to_string(),
        ));
    }
    cursor += 1;
    let mut members = Vec::new();
    loop {
        cursor = skip_trivia(bytes, cursor)?;
        if bytes.get(cursor) == Some(&b'}') {
            let tail = skip_trivia(bytes, cursor + 1)?;
            if tail != bytes.len() {
                return Err(BridgeError::InvalidConfig(
                    "unexpected content after root object".to_string(),
                ));
            }
            return Ok(ObjectLayout {
                members,
                close: cursor,
            });
        }
        let property_start = cursor;
        let key_end = string_end(bytes, cursor)?;
        let key = serde_json::from_str::<String>(&text[cursor..key_end])
            .map_err(|error| BridgeError::InvalidConfig(format!("invalid object key: {error}")))?;
        cursor = skip_trivia(bytes, key_end)?;
        if bytes.get(cursor) != Some(&b':') {
            return Err(BridgeError::InvalidConfig(format!(
                "missing colon after key {key:?}"
            )));
        }
        cursor = skip_trivia(bytes, cursor + 1)?;
        let value_start = cursor;
        let value_end = jsonc_value_end(bytes, cursor)?;
        if value_end == value_start {
            return Err(BridgeError::InvalidConfig(format!(
                "missing value for key {key:?}"
            )));
        }
        let delimiter = skip_trivia(bytes, value_end)?;
        let comma_after = match bytes.get(delimiter) {
            Some(b',') => true,
            Some(b'}') => false,
            _ => {
                return Err(BridgeError::InvalidConfig(format!(
                    "expected a comma or closing brace after key {key:?}"
                )));
            }
        };
        members.push(Member {
            key,
            property_start,
            value_start,
            value_end,
            delimiter,
            comma_after,
        });
        if comma_after {
            cursor = delimiter + 1;
        } else {
            cursor = delimiter;
        }
    }
}

fn jsonc_value_end(bytes: &[u8], start: usize) -> Result<usize> {
    match bytes.get(start) {
        Some(b'"') => string_end(bytes, start),
        Some(b'{') | Some(b'[') => {
            let mut stack = vec![bytes[start]];
            let mut cursor = start + 1;
            while cursor < bytes.len() {
                match bytes[cursor] {
                    b'"' => cursor = string_end(bytes, cursor)?,
                    b'/' if bytes.get(cursor + 1) == Some(&b'/') => {
                        cursor = line_comment_end(bytes, cursor + 2)
                    }
                    b'/' if bytes.get(cursor + 1) == Some(&b'*') => {
                        cursor = block_comment_end(bytes, cursor + 2)?
                    }
                    b'{' | b'[' => {
                        stack.push(bytes[cursor]);
                        cursor += 1;
                    }
                    b'}' | b']' => {
                        let open = stack.pop().ok_or_else(|| {
                            BridgeError::InvalidConfig("unbalanced nested value".to_string())
                        })?;
                        let matches = (open == b'{' && bytes[cursor] == b'}')
                            || (open == b'[' && bytes[cursor] == b']');
                        if !matches {
                            return Err(BridgeError::InvalidConfig(
                                "mismatched nested value delimiters".to_string(),
                            ));
                        }
                        cursor += 1;
                        if stack.is_empty() {
                            return Ok(cursor);
                        }
                    }
                    _ => cursor += 1,
                }
            }
            Err(BridgeError::InvalidConfig(
                "unterminated nested value".to_string(),
            ))
        }
        Some(_) => {
            let mut cursor = start;
            while cursor < bytes.len()
                && !bytes[cursor].is_ascii_whitespace()
                && bytes[cursor] != b','
                && bytes[cursor] != b'}'
                && !(bytes[cursor] == b'/'
                    && matches!(bytes.get(cursor + 1), Some(b'/') | Some(b'*')))
            {
                cursor += 1;
            }
            Ok(cursor)
        }
        None => Err(BridgeError::InvalidConfig(
            "missing JSONC value".to_string(),
        )),
    }
}

fn parse_string_value(text: &str, member: &Member) -> Result<(String, usize)> {
    let end = string_end(text.as_bytes(), member.value_start).map_err(|_| {
        BridgeError::InvalidConfig(format!(
            "top-level key {:?} must contain a string",
            member.key
        ))
    })?;
    if skip_trivia(text.as_bytes(), end)? != member.delimiter {
        return Err(BridgeError::InvalidConfig(format!(
            "top-level key {:?} must contain a string",
            member.key
        )));
    }
    serde_json::from_str(&text[member.value_start..end])
        .map(|value| (value, end))
        .map_err(|error| {
            BridgeError::InvalidConfig(format!(
                "invalid string value for key {:?}: {error}",
                member.key
            ))
        })
}

fn skip_trivia(bytes: &[u8], mut cursor: usize) -> Result<usize> {
    loop {
        while bytes.get(cursor).is_some_and(u8::is_ascii_whitespace) {
            cursor += 1;
        }
        match (bytes.get(cursor), bytes.get(cursor + 1)) {
            (Some(b'/'), Some(b'/')) => cursor = line_comment_end(bytes, cursor + 2),
            (Some(b'/'), Some(b'*')) => cursor = block_comment_end(bytes, cursor + 2)?,
            _ => return Ok(cursor),
        }
    }
}

fn string_end(bytes: &[u8], start: usize) -> Result<usize> {
    if bytes.get(start) != Some(&b'"') {
        return Err(BridgeError::InvalidConfig(
            "expected a JSON string".to_string(),
        ));
    }
    let mut cursor = start + 1;
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'\\' => cursor += 2,
            b'"' => return Ok(cursor + 1),
            _ => cursor += 1,
        }
    }
    Err(BridgeError::InvalidConfig(
        "unterminated JSON string".to_string(),
    ))
}

fn line_comment_end(bytes: &[u8], mut cursor: usize) -> usize {
    while cursor < bytes.len() && bytes[cursor] != b'\n' {
        cursor += 1;
    }
    cursor
}

fn block_comment_end(bytes: &[u8], mut cursor: usize) -> Result<usize> {
    while cursor + 1 < bytes.len() {
        if bytes[cursor] == b'*' && bytes[cursor + 1] == b'/' {
            return Ok(cursor + 2);
        }
        cursor += 1;
    }
    Err(BridgeError::InvalidConfig(
        "unterminated block comment".to_string(),
    ))
}

fn line_indentation(text: &str, position: usize) -> String {
    let line_start = text[..position].rfind('\n').map_or(0, |index| index + 1);
    text[line_start..position]
        .chars()
        .take_while(|character| character.is_ascii_whitespace())
        .collect()
}
