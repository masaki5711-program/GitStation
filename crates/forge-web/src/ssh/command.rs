//! The exec command an SSH client sends (M4 C13, ADR-0032 §3): only
//! `git-upload-pack '<path>'` and `git-receive-pack '<path>'` (also the
//! `git upload-pack` spelling), the path in git's single quoting. Nothing
//! here runs anything: the command is mapped onto the Smart HTTP routes
//! (`super::bridge`), whose dispatcher resolves and authorizes the path.
//! Pure; the refusal is `FG-GIT-0033` with `reason`.

use forge_errors::{ErrorCode, ForgeError};

/// The longest command line accepted.
pub const MAX_COMMAND_BYTES: usize = 1024;
/// The longest repository path accepted.
pub const MAX_PATH_BYTES: usize = 512;

/// The two git services served over SSH.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitService {
    UploadPack,
    ReceivePack,
}

impl GitService {
    /// The Smart HTTP service name (`?service=` and the POST path's last
    /// segment).
    pub fn name(self) -> &'static str {
        match self {
            Self::UploadPack => "git-upload-pack",
            Self::ReceivePack => "git-receive-pack",
        }
    }
}

/// A command the transport serves: the service and the repository path
/// as the dispatcher will see it (`<segments>.git`, no leading slash).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshCommand {
    pub service: GitService,
    pub repo_path: String,
}

fn refused(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::SshCommandRefused).with_context("reason", reason)
}

/// Parse the exec request's command line.
pub fn parse_command(raw: &[u8]) -> Result<SshCommand, ForgeError> {
    if raw.len() > MAX_COMMAND_BYTES {
        return Err(refused("malformed").with_context("detail", "too_long"));
    }
    let text = std::str::from_utf8(raw)
        .map_err(|_| refused("malformed").with_context("detail", "not_utf8"))?;
    if text.chars().any(char::is_control) {
        return Err(refused("malformed").with_context("detail", "control_character"));
    }
    let (service, argument) = split_service(text)?;
    let path = unquote(argument)?;
    Ok(SshCommand {
        service,
        repo_path: repo_path(&path)?,
    })
}

/// `git-upload-pack <arg>` / `git upload-pack <arg>`.
fn split_service(text: &str) -> Result<(GitService, &str), ForgeError> {
    let (first, rest) = text.split_once(' ').unwrap_or((text, ""));
    let (name, rest) = if first == "git" {
        let (second, rest) = rest.split_once(' ').unwrap_or((rest, ""));
        (format!("git-{second}"), rest)
    } else {
        (first.to_owned(), rest)
    };
    let service = match name.as_str() {
        "git-upload-pack" => GitService::UploadPack,
        "git-receive-pack" => GitService::ReceivePack,
        _ => {
            return Err(refused("not_git")
                .with_context("command", first.chars().take(64).collect::<String>()))
        }
    };
    if rest.is_empty() {
        return Err(refused("malformed").with_context("detail", "no_path"));
    }
    Ok((service, rest))
}

/// git's single quoting (`'a'\''b'` = `a'b`; `\!` too), or one bare word.
fn unquote(arg: &str) -> Result<String, ForgeError> {
    let malformed = |detail: &'static str| refused("malformed").with_context("detail", detail);
    if !arg.starts_with('\'') {
        if arg.contains([' ', '\'', '"', '\\']) {
            return Err(malformed("unquoted_argument"));
        }
        return Ok(arg.to_owned());
    }
    let mut out = String::new();
    let mut chars = arg.chars();
    loop {
        match chars.next() {
            None => return Ok(out),
            Some('\'') => loop {
                match chars.next() {
                    None => return Err(malformed("unterminated_quote")),
                    Some('\'') => break,
                    Some(c) => out.push(c),
                }
            },
            Some('\\') => match chars.next() {
                Some(c @ ('\'' | '!')) => out.push(c),
                _ => return Err(malformed("bad_escape")),
            },
            // A space here is a second argument.
            Some(_) => return Err(malformed("more_than_one_argument")),
        }
    }
}

/// The repository path: a leading `/` (an `ssh://` URL) and a trailing `/`
/// dropped, `.git` added when missing; segments of `A-Z a-z 0-9 . _ -`,
/// none empty or starting with a dot (no `..`).
fn repo_path(raw: &str) -> Result<String, ForgeError> {
    let invalid = |detail: &'static str| refused("path_invalid").with_context("detail", detail);
    let trimmed = raw.strip_prefix('/').unwrap_or(raw);
    let trimmed = trimmed.strip_suffix('/').unwrap_or(trimmed);
    if trimmed.is_empty() {
        return Err(invalid("empty"));
    }
    if trimmed.len() > MAX_PATH_BYTES {
        return Err(invalid("too_long"));
    }
    for segment in trimmed.split('/') {
        if segment.is_empty() {
            return Err(invalid("empty_segment"));
        }
        if segment.starts_with('.') {
            return Err(invalid("dot_segment"));
        }
        if !segment
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        {
            return Err(invalid("character"));
        }
    }
    let with_suffix = if trimmed.ends_with(".git") {
        trimmed.to_owned()
    } else {
        format!("{trimmed}.git")
    };
    if with_suffix == ".git" || with_suffix.ends_with("/.git") {
        return Err(invalid("empty_segment"));
    }
    Ok(with_suffix)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn reason(err: &ForgeError) -> String {
        err.context()
            .iter()
            .find(|(k, _)| *k == "reason")
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    }

    #[test]
    fn git_commands_parse_in_every_spelling_git_sends() {
        for (raw, service, path) in [
            (
                "git-upload-pack 'alice/web.git'",
                GitService::UploadPack,
                "alice/web.git",
            ),
            (
                "git-upload-pack '/alice/web.git'",
                GitService::UploadPack,
                "alice/web.git",
            ),
            (
                "git-receive-pack 'alice/web.git'",
                GitService::ReceivePack,
                "alice/web.git",
            ),
            (
                "git receive-pack 'group/sub/app'",
                GitService::ReceivePack,
                "group/sub/app.git",
            ),
            (
                "git-upload-pack alice/web.git",
                GitService::UploadPack,
                "alice/web.git",
            ),
            (
                "git-upload-pack 'alice/web.git/'",
                GitService::UploadPack,
                "alice/web.git",
            ),
            (
                "git-upload-pack 'a_b/c-d.e.git'",
                GitService::UploadPack,
                "a_b/c-d.e.git",
            ),
        ] {
            let cmd = parse_command(raw.as_bytes()).expect(raw);
            assert_eq!(cmd.service, service, "{raw}");
            assert_eq!(cmd.repo_path, path, "{raw}");
        }
    }

    /// The errors.toml test of FG-GIT-0033.
    #[test]
    fn an_ssh_command_other_than_git_is_refused() {
        for (raw, why) in [
            ("git-upload-archive 'alice/web.git'", "not_git"),
            ("sh -c 'id'", "not_git"),
            ("ls", "not_git"),
            ("git-upload-pack", "malformed"),
            ("git-upload-pack 'alice/web.git", "malformed"),
            ("git-upload-pack 'a' 'b'", "malformed"),
            ("git-upload-pack 'a'x", "malformed"),
            ("git-upload-pack a b", "malformed"),
            ("git-upload-pack 'a'\\x'b'", "malformed"),
            ("git-upload-pack 'alice/web.git'\n", "malformed"),
            ("git-upload-pack ''", "path_invalid"),
            ("git-upload-pack '/'", "path_invalid"),
            ("git-upload-pack '../etc/passwd'", "path_invalid"),
            ("git-upload-pack 'alice/../bob/web.git'", "path_invalid"),
            ("git-upload-pack 'alice//web.git'", "path_invalid"),
            ("git-upload-pack 'alice/.hidden'", "path_invalid"),
            ("git-upload-pack 'alice/web;rm -rf'", "path_invalid"),
            ("git-upload-pack 'alice/w\u{e9}b'", "path_invalid"),
            ("git-upload-pack 'alice/.git'", "path_invalid"),
            ("git-upload-pack '--upload-pack=x'", "path_invalid"),
            ("git-upload-pack 'C:\\\\x'", "path_invalid"),
        ] {
            let err = parse_command(raw.as_bytes()).expect_err(raw);
            assert_eq!(err.code(), ErrorCode::SshCommandRefused, "{raw}");
            assert_eq!(reason(&err), why, "{raw}");
        }
        let long = format!("git-upload-pack '{}'", "a".repeat(MAX_COMMAND_BYTES));
        assert_eq!(
            reason(&parse_command(long.as_bytes()).unwrap_err()),
            "malformed"
        );
        let deep = format!("git-upload-pack '{}'", "ab/".repeat(200));
        assert_eq!(
            reason(&parse_command(deep.as_bytes()).unwrap_err()),
            "path_invalid"
        );
        assert_eq!(
            reason(&parse_command(&[0xff, 0xfe]).unwrap_err()),
            "malformed"
        );
    }

    #[test]
    fn git_single_quote_escapes_are_undone_and_then_refused_as_a_path() {
        // `'it'\''s'` is `it's`: a quote is no path character.
        let err = parse_command(b"git-upload-pack 'it'\\''s'").unwrap_err();
        assert_eq!(reason(&err), "path_invalid");
        assert_eq!(unquote("'a'\\!'b'").unwrap(), "a!b");
        assert_eq!(unquote("'a'\\''b'").unwrap(), "a'b");
    }

    #[test]
    fn the_command_word_is_cut_in_the_log_context() {
        let raw = format!("{} x", "y".repeat(300));
        let err = parse_command(raw.as_bytes()).unwrap_err();
        let command = err
            .context()
            .iter()
            .find(|(k, _)| *k == "command")
            .map(|(_, v)| v.len());
        assert_eq!(command, Some(64));
    }
}
