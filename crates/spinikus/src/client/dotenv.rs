//! `.env` files for `env import` and `env export`: `NAME=value` lines, `#` comments, an
//! optional `export ` prefix, single quotes taken literally, double quotes with `\n`, `\"`
//! and `\\` escapes (and newlines inside them). Nothing is expanded, so references such as
//! `${{ db.DATABASE_URL }}` pass through as they are.

use anyhow::bail;

/// The variables in `text`, in order.
pub fn parse(text: &str) -> anyhow::Result<Vec<(String, String)>> {
    let mut variables = Vec::new();
    let mut rest = text;
    let mut line_number = 1;
    while !rest.is_empty() {
        let (line, after) = rest.split_once('\n').unwrap_or((rest, ""));
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            rest = after;
            line_number += 1;
            continue;
        }
        let assignment = trimmed.strip_prefix("export ").map(str::trim_start).unwrap_or(trimmed);
        let Some((name, _)) = assignment.split_once('=') else {
            bail!("line {line_number}: expected NAME=value");
        };
        let name = name.trim();
        if name.is_empty() || name.contains(char::is_whitespace) {
            bail!("line {line_number}: `{name}` is not a variable name");
        }
        // The value starts right after `=`, and may run over several lines when quoted.
        let start = line.find('=').map_or(0, |at| at + 1);
        let value_text = &rest[start..];
        let value_text = value_text.trim_start_matches([' ', '\t']);
        let (value, consumed) = match value_text.chars().next() {
            Some(quote @ ('\'' | '"')) => quoted(&value_text[1..], quote)
                .map(|(value, used)| (value, used + 1))
                .ok_or_else(|| anyhow::anyhow!("line {line_number}: the {quote} quote is never closed"))?,
            _ => {
                let end = value_text.find('\n').unwrap_or(value_text.len());
                (unquoted(&value_text[..end]), end)
            }
        };
        variables.push((name.to_string(), value));
        let remaining = &value_text[consumed..];
        line_number += value_text[..consumed].matches('\n').count();
        // Whatever follows a closing quote on its line is ignored (such as a comment).
        rest = remaining.split_once('\n').map_or("", |(_, after)| after);
        line_number += 1;
    }
    Ok(variables)
}

/// An unquoted value: trimmed, and cut at ` #`.
fn unquoted(text: &str) -> String {
    let text = text.trim_end_matches('\r');
    let text = match text.find(" #").or_else(|| text.find("\t#")) {
        Some(at) => &text[..at],
        None => text,
    };
    text.trim().to_string()
}

/// The value inside quotes, and the bytes used including the closing quote.
fn quoted(text: &str, quote: char) -> Option<(String, usize)> {
    let mut value = String::new();
    let mut chars = text.char_indices();
    while let Some((at, char)) = chars.next() {
        match char {
            '\\' if quote == '"' => match chars.next()?.1 {
                'n' => value.push('\n'),
                'r' => value.push('\r'),
                't' => value.push('\t'),
                other @ ('"' | '\\') => value.push(other),
                other => {
                    value.push('\\');
                    value.push(other);
                }
            },
            char if char == quote => return Some((value, at + 1)),
            char => value.push(char),
        }
    }
    None
}

/// `NAME=value`, quoted so [`parse`] reads back exactly `value`.
pub fn line(name: &str, value: &str) -> String {
    let plain = !value.is_empty() && value.chars().all(|char| char.is_ascii_alphanumeric() || "_-./:@,+%".contains(char));
    if plain {
        format!("{name}={value}")
    } else if !value.contains(['\'', '\n', '\r']) {
        format!("{name}='{value}'")
    } else {
        let escaped =
            value.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n").replace('\r', "\\r").replace('\t', "\\t");
        format!("{name}=\"{escaped}\"")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs(text: &str) -> Vec<(String, String)> {
        parse(text).unwrap()
    }

    #[test]
    fn reads_the_usual_forms() {
        let text = "# settings\nexport PORT=3000\nNAME = shop # the slug\nURL='postgres://a:b@db/x#y'\n\
                    GREETING=\"hello\\n\\\"world\\\"\"\nMULTI=\"one\ntwo\"\nREF=${{ db.DATABASE_URL }}\nEMPTY=\n";
        assert_eq!(
            pairs(text),
            [
                ("PORT".into(), "3000".into()),
                ("NAME".into(), "shop".into()),
                ("URL".into(), "postgres://a:b@db/x#y".into()),
                ("GREETING".into(), "hello\n\"world\"".into()),
                ("MULTI".into(), "one\ntwo".into()),
                ("REF".into(), "${{ db.DATABASE_URL }}".into()),
                ("EMPTY".into(), String::new()),
            ]
        );
        assert_eq!(pairs("A=1\r\nB='2' # two\r\n"), [("A".into(), "1".into()), ("B".into(), "2".into())]);
    }

    #[test]
    fn says_which_line_is_wrong() {
        assert_eq!(parse("A=1\nnot an assignment\n").unwrap_err().to_string(), "line 2: expected NAME=value");
        assert_eq!(parse("A=1\nB=\"open\n\nC=3").unwrap_err().to_string(), "line 2: the \" quote is never closed");
        assert_eq!(parse("A=\"x\ny\"\nbad\n").unwrap_err().to_string(), "line 3: expected NAME=value");
    }

    #[test]
    fn written_lines_read_back_the_same() {
        for value in ["3000", "", "two words", "it's", "a\nb", "say \"hi\"\t\\o/", "${{ db.URL }}", "x #y"] {
            assert_eq!(pairs(&line("V", value)), [("V".to_string(), value.to_string())], "{value:?}");
        }
        assert_eq!(line("PORT", "3000"), "PORT=3000");
        assert_eq!(line("NAME", "two words"), "NAME='two words'");
    }
}
