use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Pat's mailbox root, as `pat env` reports it.
pub fn mailbox() -> io::Result<PathBuf> {
    let out = Command::new("pat").arg("env").output().map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("can't run `pat env` ({e}); pass --mailbox instead"),
        )
    })?;
    let text = String::from_utf8_lossy(&out.stdout);
    parse_env(&text).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "`pat env` printed no PAT_MAILBOX_PATH; pass --mailbox instead",
        )
    })
}

fn parse_env(text: &str) -> Option<PathBuf> {
    let line = text
        .lines()
        .find_map(|l| l.trim().strip_prefix("PAT_MAILBOX_PATH="))?;
    // pat quotes the value but doesn't escape it
    let v = line.trim();
    let v = v
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .unwrap_or(v);
    (!v.is_empty()).then(|| PathBuf::from(v))
}

/// Writes `<MID>.b2f` into `call`'s inbox, marked unread the way Pat marks mail it
/// receives. Returns `None` if Pat already has that message, which is how duplicates
/// are dropped. `b2f` must have passed `message::check`.
pub fn deliver(mailbox: &Path, call: &str, mid: &str, b2f: &[u8]) -> io::Result<Option<PathBuf>> {
    let dir = mailbox.join(call).join("in");
    fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{mid}.b2f"));
    if path.exists() {
        return Ok(None);
    }
    // write beside it first so Pat never reads half a message
    let part = dir.join(format!("{mid}.b2f.part"));
    fs::write(&part, unread(b2f))?;
    fs::rename(&part, &path)?;
    Ok(Some(path))
}

fn unread(b2f: &[u8]) -> Vec<u8> {
    let end = b2f
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("checked B2F has a blank line after its headers");
    [&b2f[..end], &b"\r\nX-Unread: true"[..], &b2f[end..]].concat()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_mailbox_path_from_pat_env() {
        let out = "PAT_MYCALL=\"K1ABC\"\nPAT_MAILBOX_PATH=\"C:\\Users\\op\\AppData\\Local\\pat\\mailbox\"\n";
        assert_eq!(
            parse_env(out),
            Some(PathBuf::from(r"C:\Users\op\AppData\Local\pat\mailbox"))
        );
        assert_eq!(
            parse_env("PAT_MAILBOX_PATH=\"\\\\nas\\pat\\mailbox\""),
            Some(PathBuf::from(r"\\nas\pat\mailbox"))
        );
        assert_eq!(
            parse_env("PAT_MAILBOX_PATH=/home/op/.local/share/pat/mailbox"),
            Some(PathBuf::from("/home/op/.local/share/pat/mailbox"))
        );
        assert_eq!(parse_env("PAT_MYCALL=K1ABC"), None);
    }

    #[test]
    fn writes_into_inbox_once() {
        let tmp = tempfile::tempdir().unwrap();
        let msg = b"Mid: ABC123\r\nBody: 2\r\n\r\nhi";
        let first = deliver(tmp.path(), "K1ABC", "ABC123", msg).unwrap();
        let expected = tmp.path().join("K1ABC").join("in").join("ABC123.b2f");
        assert_eq!(first, Some(expected.clone()));
        let stored = b"Mid: ABC123\r\nBody: 2\r\nX-Unread: true\r\n\r\nhi";
        assert_eq!(fs::read(&expected).unwrap(), stored);
        assert_eq!(crate::message::check(stored), Ok("ABC123".into()));
        assert_eq!(
            deliver(
                tmp.path(),
                "K1ABC",
                "ABC123",
                b"Mid: ABC123\r\nBody: 0\r\n\r\n"
            )
            .unwrap(),
            None
        );
        assert_eq!(fs::read(&expected).unwrap(), stored);
        assert_eq!(fs::read_dir(expected.parent().unwrap()).unwrap().count(), 1);
    }
}
