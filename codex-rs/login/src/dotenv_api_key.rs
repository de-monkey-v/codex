use std::fs::OpenOptions;
use std::io;
use std::io::ErrorKind;
use std::path::Path;

use codex_core::auth::CODEX_API_KEY_ENV_VAR;

pub fn validate_dotenv_target(path: &Path) -> io::Result<()> {
    ensure_parent_dir(path)?;

    if path.exists() {
        OpenOptions::new().append(true).open(path)?;
        return Ok(());
    }

    OpenOptions::new().write(true).create_new(true).open(path)?;
    std::fs::remove_file(path)
}

pub fn upsert_dotenv_api_key(path: &Path, api_key: &str) -> io::Result<()> {
    if api_key.contains(['\n', '\r']) {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "CODEX_API_KEY must not contain newlines",
        ));
    }

    ensure_parent_dir(path)?;

    let existing = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == ErrorKind::NotFound => String::new(),
        Err(err) => return Err(err),
    };

    let mut next = String::new();
    let mut wrote_api_key = false;

    for segment in split_lines_preserving_terminators(&existing) {
        if is_active_assignment_for(segment, CODEX_API_KEY_ENV_VAR) {
            if !wrote_api_key {
                next.push_str(&format!("{CODEX_API_KEY_ENV_VAR}={api_key}\n"));
                wrote_api_key = true;
            }
            continue;
        }

        next.push_str(segment);
    }

    if !wrote_api_key {
        if !next.is_empty() && !next.ends_with('\n') {
            next.push('\n');
        }
        next.push_str(&format!("{CODEX_API_KEY_ENV_VAR}={api_key}\n"));
    }

    std::fs::write(path, next)
}

fn ensure_parent_dir(path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    Ok(())
}

fn split_lines_preserving_terminators(contents: &str) -> Vec<&str> {
    if contents.is_empty() {
        return Vec::new();
    }

    contents.split_inclusive('\n').collect()
}

fn is_active_assignment_for(line: &str, key: &str) -> bool {
    let mut rest = line.trim_start();
    if rest.starts_with('#') {
        return false;
    }

    if let Some(stripped) = rest.strip_prefix("export") {
        rest = stripped.trim_start();
    }

    let Some(rest) = rest.strip_prefix(key) else {
        return false;
    };

    rest.trim_start().starts_with('=')
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use tempfile::tempdir;

    #[test]
    fn upsert_creates_dotenv_file_when_missing() {
        let temp_dir = tempdir().expect("tempdir");
        let dotenv_path = temp_dir.path().join(".env");

        upsert_dotenv_api_key(&dotenv_path, "sk-test-key").expect("write dotenv");

        let written = std::fs::read_to_string(&dotenv_path).expect("read dotenv");
        assert_eq!(written, "CODEX_API_KEY=sk-test-key\n");
    }

    #[test]
    fn upsert_replaces_existing_api_key_and_collapses_duplicates() {
        let temp_dir = tempdir().expect("tempdir");
        let dotenv_path = temp_dir.path().join(".env");
        std::fs::write(
            &dotenv_path,
            "# comment\nCODEX_API_KEY=sk-old-1\nOTHER=value\nexport CODEX_API_KEY = sk-old-2\n",
        )
        .expect("seed dotenv");

        upsert_dotenv_api_key(&dotenv_path, "sk-new-key").expect("update dotenv");

        let written = std::fs::read_to_string(&dotenv_path).expect("read dotenv");
        assert_eq!(
            written,
            "# comment\nCODEX_API_KEY=sk-new-key\nOTHER=value\n"
        );
    }

    #[test]
    fn validate_dotenv_target_succeeds_for_missing_file() {
        let temp_dir = tempdir().expect("tempdir");
        let dotenv_path = temp_dir.path().join(".env");

        validate_dotenv_target(&dotenv_path).expect("validate dotenv");

        assert!(
            !dotenv_path.exists(),
            "validation should not leave behind a new file"
        );
    }
}
