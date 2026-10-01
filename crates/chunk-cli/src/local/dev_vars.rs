//! `.dev.vars`, the gitignored dotenv file of the secrets `chunk dev` grants actions.

use std::{fs, io, path::Path};

use chunk_backend::Secrets;

pub(super) const FILE: &str = ".dev.vars";

/// Reads `FILE` in `root`, if there is one. Errors name the line, never a value.
pub(super) fn read(root: &Path) -> io::Result<Secrets> {
    match fs::read_to_string(root.join(FILE)) {
        Ok(source) => parse(&source),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Secrets::default()),
        Err(error) => Err(io::Error::new(error.kind(), format!("{FILE}: {error}"))),
    }
}

/// Parses `NAME=value` lines, skipping blank lines and `#` comments. A value wrapped in single or double quotes loses
/// them; anything else is taken as written, without the surrounding whitespace. A later line wins over an earlier one.
fn parse(source: &str) -> io::Result<Secrets> {
    let mut secrets = Secrets::default();
    for (index, line) in source.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let invalid = |reason: &str| io::Error::other(format!("{FILE}:{}: {reason}", index + 1));
        let (name, value) = line.split_once('=').ok_or_else(|| invalid("expected NAME=value"))?;
        let name = name.trim();
        if !chunk_contract::valid_env_name(name) {
            return Err(invalid("names are 1-128 letters, digits or underscores, not starting with a digit"));
        }
        let value = value.trim();
        let unquoted = ['"', '\''].into_iter().find_map(|quote| value.strip_prefix(quote)?.strip_suffix(quote));
        let value = unquoted.unwrap_or(value);
        if !chunk_contract::valid_env_value(value) {
            return Err(invalid(&format!("{name} must be non-empty and at most 64 KiB")));
        }
        secrets.insert(name.to_owned(), value.to_owned());
    }
    Ok(secrets)
}

#[cfg(test)]
mod tests {
    use super::parse;

    #[test]
    fn dev_vars_parse_names_quotes_and_comments() {
        let source = "# local secrets\n\nAPI_KEY=abc=def\n  TOKEN = \"quoted value\" \nSINGLE='a # b'\nAPI_KEY=later\n";
        let secrets = parse(source).unwrap();
        assert_eq!(secrets.names().collect::<Vec<_>>(), ["API_KEY", "SINGLE", "TOKEN"]);
        assert_eq!(secrets.get("API_KEY"), Some("later"));
        assert_eq!(secrets.get("TOKEN"), Some("quoted value"));
        assert_eq!(secrets.get("SINGLE"), Some("a # b"));
        for (source, line) in [("OK=1\nmissing", ":2:"), ("1BAD=x", ":1:"), ("EMPTY=\"\"", ":1:")] {
            let error = parse(source).err().unwrap().to_string();
            assert!(error.contains(line), "{error}");
        }
    }
}
