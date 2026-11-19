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

#[derive(Debug, PartialEq)]
pub enum Stored {
    New(PathBuf),
    /// Pat already has this message.
    Already,
    /// A different message with the same MID is there, and was left alone.
    Clash,
}

/// Writes `<MID>.b2f` into `call`'s inbox, marked unread the way Pat marks mail it
/// receives. A message already there with the same MID is a duplicate if its body
/// matches (Pat rewrites the headers when mail is read), and is only replaced when
/// `replace` is set. `b2f` must have passed `message::check`.
pub fn deliver(
    mailbox: &Path,
    call: &str,
    mid: &str,
    b2f: &[u8],
    replace: bool,
) -> io::Result<Stored> {
    let dir = mailbox.join(call).join("in");
    fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{mid}.b2f"));
    if path.exists() {
        if body(&fs::read(&path)?) == body(b2f) {
            return Ok(Stored::Already);
        }
        if !replace {
            return Ok(Stored::Clash);
        }
    }
    // write beside it first so Pat never reads half a message
    let part = dir.join(format!("{mid}.b2f.part"));
    fs::write(&part, unread(b2f))?;
    fs::rename(&part, &path)?;
    Ok(Stored::New(path))
}

fn body(b2f: &[u8]) -> &[u8] {
    match b2f.windows(4).position(|w| w == b"\r\n\r\n") {
        Some(end) => &b2f[end + 4..],
        None => b2f,
    }
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
        let first = deliver(tmp.path(), "K1ABC", "ABC123", msg, false).unwrap();
        let expected = tmp.path().join("K1ABC").join("in").join("ABC123.b2f");
        assert_eq!(first, Stored::New(expected.clone()));
        let stored = b"Mid: ABC123\r\nBody: 2\r\nX-Unread: true\r\n\r\nhi";
        assert_eq!(fs::read(&expected).unwrap(), stored);
        assert_eq!(crate::message::check(stored), Ok("ABC123".into()));
        assert_eq!(fs::read_dir(expected.parent().unwrap()).unwrap().count(), 1);

        // read in Pat since: headers rewritten, same body
        fs::write(&expected, b"Body: 2\r\nMid: ABC123\r\n\r\nhi").unwrap();
        let again = deliver(tmp.path(), "K1ABC", "ABC123", msg, false).unwrap();
        assert_eq!(again, Stored::Already);

        let squatter = b"Mid: ABC123\r\nBody: 3\r\n\r\nbad";
        fs::write(&expected, squatter).unwrap();
        let clash = deliver(tmp.path(), "K1ABC", "ABC123", msg, false).unwrap();
        assert_eq!(clash, Stored::Clash);
        assert_eq!(fs::read(&expected).unwrap(), squatter);
        let replaced = deliver(tmp.path(), "K1ABC", "ABC123", msg, true).unwrap();
        assert_eq!(replaced, Stored::New(expected.clone()));
        assert_eq!(fs::read(&expected).unwrap(), stored);
    }
}
