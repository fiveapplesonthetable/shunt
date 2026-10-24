//! Minimal workflow YAML reader.

use super::*;

pub(crate) fn expand_workflow_expr(
    raw: &str,
    expressions: &BTreeMap<String, String>,
    context: &str,
) -> Result<String, String> {
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(open_idx) = rest.find("${{") {
        out.push_str(&rest[..open_idx]);
        let after_open = &rest[open_idx + 3..];
        let Some(close_idx) = after_open.find("}}") else {
            return Err(format!(
                "{context}: unclosed workflow expression in '{raw}'"
            ));
        };
        let key = after_open[..close_idx].trim();
        let Some(val) = expressions.get(key) else {
            return Err(format!(
                "{context}: unsupported workflow expression '${{{{ {key} }}}}'"
            ));
        };
        out.push_str(val);
        rest = &after_open[close_idx + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

pub(crate) fn strip_yaml_comment(line: &str) -> Result<&str, String> {
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;
    for (idx, ch) in line.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' && in_double {
            escaped = true;
            continue;
        }
        if ch == '\'' && !in_double {
            in_single = !in_single;
            continue;
        }
        if ch == '"' && !in_single {
            in_double = !in_double;
            continue;
        }
        if ch == '#' && !in_single && !in_double && (idx == 0 || line[..idx].ends_with([' ', '\t']))
        {
            return Ok(line[..idx].trim_end());
        }
    }
    if in_single || in_double {
        return Err(format!("unclosed quote in YAML line: {line}"));
    }
    Ok(line.trim_end())
}

pub(crate) fn split_yaml_key_val(text: &str) -> Option<(&str, &str)> {
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;
    let mut depth_brace = 0usize;
    let mut depth_bracket = 0usize;
    let bytes = text.as_bytes();
    for (idx, ch) in text.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' && in_double {
            escaped = true;
            continue;
        }
        if ch == '\'' && !in_double {
            in_single = !in_single;
            continue;
        }
        if ch == '"' && !in_single {
            in_double = !in_double;
            continue;
        }
        if !in_single && !in_double {
            match ch {
                '{' => depth_brace += 1,
                '}' => depth_brace = depth_brace.saturating_sub(1),
                '[' => depth_bracket += 1,
                ']' => depth_bracket = depth_bracket.saturating_sub(1),
                ':' if depth_brace == 0 && depth_bracket == 0 => {
                    let next_byte = bytes.get(idx + 1).copied();
                    if next_byte.is_none() || next_byte == Some(b' ') || next_byte == Some(b'\t') {
                        let key = text[..idx].trim();
                        if !key.is_empty() {
                            let val = text[idx + 1..].trim();
                            return Some((key, val));
                        }
                    }
                }
                _ => {}
            }
        }
    }
    None
}

pub(crate) fn split_flow_items(inner: &str) -> Vec<&str> {
    let mut items = Vec::new();
    let mut start = 0;
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;
    let mut depth_brace = 0usize;
    let mut depth_bracket = 0usize;
    for (idx, ch) in inner.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' && in_double {
            escaped = true;
            continue;
        }
        if ch == '\'' && !in_double {
            in_single = !in_single;
            continue;
        }
        if ch == '"' && !in_single {
            in_double = !in_double;
            continue;
        }
        if !in_single && !in_double {
            match ch {
                '{' => depth_brace += 1,
                '}' => depth_brace = depth_brace.saturating_sub(1),
                '[' => depth_bracket += 1,
                ']' => depth_bracket = depth_bracket.saturating_sub(1),
                ',' if depth_brace == 0 && depth_bracket == 0 => {
                    let seg = inner[start..idx].trim();
                    if !seg.is_empty() {
                        items.push(seg);
                    }
                    start = idx + 1;
                }
                _ => {}
            }
        }
    }
    let tail = inner[start..].trim();
    if !tail.is_empty() {
        items.push(tail);
    }
    items
}

pub(crate) fn unquote_yaml_str(raw: &str) -> Result<String, String> {
    let s = raw.trim();
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        let inner = &s[1..s.len() - 1];
        let mut out = String::with_capacity(inner.len());
        let mut chars = inner.chars();
        while let Some(c) = chars.next() {
            if c == '\\' {
                match chars.next() {
                    Some('n') => out.push('\n'),
                    Some('r') => out.push('\r'),
                    Some('t') => out.push('\t'),
                    Some('"') => out.push('"'),
                    Some('\\') => out.push('\\'),
                    Some(other) => {
                        out.push('\\');
                        out.push(other);
                    }
                    None => return Err(format!("trailing backslash in string {raw}")),
                }
            } else {
                out.push(c);
            }
        }
        Ok(out)
    } else if s.len() >= 2 && s.starts_with('\'') && s.ends_with('\'') {
        Ok(s[1..s.len() - 1].replace("''", "'"))
    } else {
        Ok(s.to_string())
    }
}

pub(crate) fn parse_yaml_scalar_or_flow(
    raw: &str,
    line_no: usize,
) -> Result<serde_json::Value, String> {
    let s = raw.trim();
    if s.is_empty() || s == "null" || s == "~" {
        return Ok(serde_json::Value::Null);
    }
    if s == "true" {
        return Ok(serde_json::Value::Bool(true));
    }
    if s == "false" {
        return Ok(serde_json::Value::Bool(false));
    }
    if (s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\'')) {
        return Ok(serde_json::Value::String(unquote_yaml_str(s)?));
    }
    if s.starts_with('[') {
        if !s.ends_with(']') {
            return Err(format!("line {line_no}: unclosed flow sequence '{s}'"));
        }
        let inner = &s[1..s.len() - 1];
        let mut arr = Vec::new();
        for item in split_flow_items(inner) {
            arr.push(parse_yaml_scalar_or_flow(item, line_no)?);
        }
        return Ok(serde_json::Value::Array(arr));
    }
    if s.starts_with('{') {
        if !s.ends_with('}') {
            return Err(format!("line {line_no}: unclosed flow mapping '{s}'"));
        }
        let inner = &s[1..s.len() - 1];
        let mut map = serde_json::Map::new();
        for pair in split_flow_items(inner) {
            let Some((k, v)) = split_yaml_key_val(pair) else {
                return Err(format!(
                    "line {line_no}: invalid flow mapping entry '{pair}'"
                ));
            };
            let key = unquote_yaml_str(k)?;
            map.insert(key, parse_yaml_scalar_or_flow(v, line_no)?);
        }
        return Ok(serde_json::Value::Object(map));
    }
    if let Ok(i) = s.parse::<i64>() {
        return Ok(serde_json::Value::Number(i.into()));
    }
    Ok(serde_json::Value::String(s.to_string()))
}

#[derive(Debug, Clone)]
pub(crate) struct YamlLine {
    pub(crate) line_no: usize,
    pub(crate) indent: usize,
    pub(crate) raw_content: String,
    pub(crate) stripped: String,
}

pub(crate) fn collect_block_scalar(
    lines: &[YamlLine],
    idx: &mut usize,
    parent_indent: usize,
    folded: bool,
) -> String {
    let mut block_lines: Vec<(usize, &str)> = Vec::new();
    let mut min_indent: Option<usize> = None;
    while *idx < lines.len() {
        let l = &lines[*idx];
        if l.raw_content.trim().is_empty() {
            block_lines.push((0, ""));
            *idx += 1;
            continue;
        }
        if l.indent <= parent_indent {
            break;
        }
        if min_indent.is_none() {
            min_indent = Some(l.indent);
        }
        block_lines.push((l.indent, l.raw_content.as_str()));
        *idx += 1;
    }
    while block_lines.last().is_some_and(|(_, s)| s.is_empty()) {
        block_lines.pop();
    }
    let base = min_indent.unwrap_or(parent_indent + 2);
    let mut rendered = Vec::with_capacity(block_lines.len());
    for (ind, text) in block_lines {
        if text.is_empty() {
            rendered.push(String::new());
        } else {
            let extra = ind.saturating_sub(base);
            rendered.push(format!("{}{}", " ".repeat(extra), text));
        }
    }
    if folded {
        rendered.join(" ")
    } else {
        rendered.join("\n")
    }
}

pub(crate) fn next_non_empty(lines: &[YamlLine], mut idx: usize) -> Option<&YamlLine> {
    while idx < lines.len() {
        if !lines[idx].stripped.is_empty() {
            return Some(&lines[idx]);
        }
        idx += 1;
    }
    None
}

pub(crate) fn parse_yaml_block(
    lines: &[YamlLine],
    idx: &mut usize,
    min_indent: usize,
) -> Result<serde_json::Value, String> {
    while *idx < lines.len() && lines[*idx].stripped.is_empty() {
        *idx += 1;
    }
    let Some(first) = lines.get(*idx) else {
        return Ok(serde_json::Value::Object(serde_json::Map::new()));
    };
    if first.indent < min_indent {
        return Ok(serde_json::Value::Null);
    }
    let block_indent = first.indent;
    if first.stripped == "-" || first.stripped.starts_with("- ") {
        let mut arr = Vec::new();
        while *idx < lines.len() {
            if lines[*idx].stripped.is_empty() {
                *idx += 1;
                continue;
            }
            if lines[*idx].indent < block_indent {
                break;
            }
            if lines[*idx].indent > block_indent {
                return Err(format!(
                    "line {}: unexpected indentation in sequence",
                    lines[*idx].line_no
                ));
            }
            let cur = &lines[*idx];
            if cur.stripped != "-" && !cur.stripped.starts_with("- ") {
                break;
            }
            let rest = cur
                .stripped
                .strip_prefix("- ")
                .unwrap_or("")
                .trim()
                .to_string();
            let line_no = cur.line_no;
            let item_indent = cur.indent;
            *idx += 1;
            if rest.is_empty() {
                let val = parse_yaml_block(lines, idx, item_indent + 1)?;
                arr.push(val);
            } else if let Some((k, v)) = split_yaml_key_val(&rest) {
                let mut map = serde_json::Map::new();
                let key = unquote_yaml_str(k)?;
                let val = parse_yaml_rhs(v, line_no, lines, idx, item_indent + 2)?;
                map.insert(key, val);
                parse_yaml_map_entries(&mut map, lines, idx, item_indent + 2)?;
                arr.push(serde_json::Value::Object(map));
            } else {
                arr.push(parse_yaml_scalar_or_flow(&rest, line_no)?);
            }
        }
        Ok(serde_json::Value::Array(arr))
    } else {
        let mut map = serde_json::Map::new();
        parse_yaml_map_entries(&mut map, lines, idx, block_indent)?;
        Ok(serde_json::Value::Object(map))
    }
}

pub(crate) fn parse_yaml_rhs(
    v: &str,
    line_no: usize,
    lines: &[YamlLine],
    idx: &mut usize,
    key_indent: usize,
) -> Result<serde_json::Value, String> {
    if v == "|" || v == "|-" || v == "|+" {
        Ok(serde_json::Value::String(collect_block_scalar(
            lines, idx, key_indent, false,
        )))
    } else if v == ">" || v == ">-" || v == ">+" {
        Ok(serde_json::Value::String(collect_block_scalar(
            lines, idx, key_indent, true,
        )))
    } else if v.is_empty() {
        if let Some(next_line) = next_non_empty(lines, *idx) {
            if next_line.indent > key_indent
                || (next_line.indent == key_indent
                    && (next_line.stripped == "-" || next_line.stripped.starts_with("- ")))
            {
                let child_min = next_line.indent;
                return parse_yaml_block(lines, idx, child_min);
            }
        }
        Ok(serde_json::Value::Null)
    } else {
        parse_yaml_scalar_or_flow(v, line_no)
    }
}

pub(crate) fn parse_yaml_map_entries(
    map: &mut serde_json::Map<String, serde_json::Value>,
    lines: &[YamlLine],
    idx: &mut usize,
    map_indent: usize,
) -> Result<(), String> {
    while *idx < lines.len() {
        if lines[*idx].stripped.is_empty() {
            *idx += 1;
            continue;
        }
        let cur_indent = lines[*idx].indent;
        if cur_indent < map_indent {
            break;
        }
        if cur_indent > map_indent {
            return Err(format!(
                "line {}: unexpected indentation (expected {}, got {})",
                lines[*idx].line_no, map_indent, cur_indent
            ));
        }
        let line_no = lines[*idx].line_no;
        let stripped = lines[*idx].stripped.clone();
        if stripped == "-" || stripped.starts_with("- ") {
            break;
        }
        let Some((k, v)) = split_yaml_key_val(&stripped) else {
            return Err(format!(
                "line {line_no}: expected 'key: value' mapping entry, found '{stripped}'"
            ));
        };
        let key = unquote_yaml_str(k)?;
        *idx += 1;
        let val = parse_yaml_rhs(v, line_no, lines, idx, cur_indent)?;
        map.insert(key, val);
    }
    Ok(())
}
