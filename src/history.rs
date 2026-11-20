use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::message;
use crate::receiver::Ack;

const SENT: &str = "sent.log";
const RECEIVED: &str = "received.log";

/// Where netcast keeps its logs and key unless told otherwise: next to the exe
/// when a file named `portable` is there, otherwise the user's own folder.
pub fn default_dir() -> PathBuf {
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
        && dir.join("portable").is_file()
    {
        return dir.to_path_buf();
    }
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        return PathBuf::from(local).join("netcast");
    }
    if let Some(state) = std::env::var_os("XDG_STATE_HOME") {
        return PathBuf::from(state).join("netcast");
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    home.join(".local").join("state").join("netcast")
}

fn append(dir: &Path, file: &str, line: &str) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(file))?;
    writeln!(f, "{line}")
}

pub fn record_sent(
    dir: &Path,
    unix: u64,
    id: u32,
    mid: &str,
    mode: &str,
    outcome: &[(String, String)],
) -> io::Result<()> {
    let states: Vec<String> = outcome.iter().map(|(c, s)| format!("{c}={s}")).collect();
    append(
        dir,
        SENT,
        &format!("{unix}\t{id:08x}\t{mid}\t{mode}\t{}", states.join(",")),
    )
}

pub struct Received {
    pub id: u32,
    pub expires_unix: u64,
    pub acks: Vec<Ack>,
}

pub fn record_received(
    dir: &Path,
    unix: u64,
    mid: &str,
    mode: &str,
    calls: &[String],
    r: &Received,
) -> io::Result<()> {
    let acks: Vec<String> = r.acks.iter().map(|(i, c)| format!("{i}:{c}")).collect();
    let line = format!(
        "{unix}\t{:08x}\t{mid}\t{mode}\t{}\t{}\t{}",
        r.id,
        r.expires_unix,
        acks.join(","),
        calls.join(",")
    );
    append(dir, RECEIVED, &line)
}

fn lines(dir: &Path, file: &str) -> io::Result<Vec<String>> {
    match fs::read_to_string(dir.join(file)) {
        Ok(s) => Ok(s.lines().map(str::to_string).collect()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(vec![]),
        Err(e) => Err(e),
    }
}

/// Decoded deliveries that haven't expired, so a restarted receiver still answers for them.
pub fn unexpired(dir: &Path, now_unix: u64) -> io::Result<Vec<Received>> {
    Ok(lines(dir, RECEIVED)?
        .iter()
        .filter_map(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            let id = u32::from_str_radix(f.get(1)?, 16).ok()?;
            let expires_unix = f.get(4)?.parse().ok()?;
            let acks = f
                .get(5)?
                .split(',')
                .filter(|a| !a.is_empty())
                .map(|a| {
                    let (i, c) = a.split_once(':')?;
                    Some((i.parse().ok()?, c.to_string()))
                })
                .collect::<Option<Vec<_>>>()?;
            Some(Received {
                id,
                expires_unix,
                acks,
            })
        })
        .filter(|r| r.expires_unix > now_unix)
        .collect())
}

/// Deliveries decoded since `since_unix`, whatever their TTL.
pub fn decoded_since(dir: &Path, since_unix: u64) -> io::Result<Vec<u32>> {
    Ok(lines(dir, RECEIVED)?
        .iter()
        .filter_map(|l| {
            let mut f = l.split('\t');
            let unix: u64 = f.next()?.parse().ok()?;
            let id = u32::from_str_radix(f.next()?, 16).ok()?;
            (unix >= since_unix).then_some(id)
        })
        .collect())
}

/// One line of history: a delivery this station sent or received.
#[derive(Debug, PartialEq, Serialize)]
pub struct Entry {
    pub unix: u64,
    pub id: String,
    pub mid: String,
    pub mode: String,
    /// For a sent delivery, each recipient and its state; for a received one,
    /// the local callsigns it went to.
    pub who: String,
}

/// Sent and received deliveries, each list oldest first.
pub fn entries(dir: &Path) -> io::Result<(Vec<Entry>, Vec<Entry>)> {
    let read = |file: &str, who_field: usize| -> io::Result<Vec<Entry>> {
        Ok(lines(dir, file)?
            .iter()
            .filter_map(|l| {
                let f: Vec<&str> = l.split('\t').collect();
                Some(Entry {
                    unix: f.first()?.parse().ok()?,
                    id: f.get(1)?.to_string(),
                    mid: f.get(2)?.to_string(),
                    mode: f.get(3)?.to_string(),
                    who: f.get(who_field)?.replace('=', " ").replace(',', ", "),
                })
            })
            .collect())
    };
    Ok((read(SENT, 4)?, read(RECEIVED, 6)?))
}

/// Both logs as readable text, oldest first.
pub fn report(dir: &Path) -> io::Result<String> {
    let (sent, received) = entries(dir)?;
    let mut out = String::new();
    for (title, rows) in [("sent", sent), ("received", received)] {
        out += &format!("{title}\n");
        if rows.is_empty() {
            out += "  nothing yet\n";
        }
        for e in rows {
            out += &format!(
                "  {}  {}  {:<12}  {:<9}  {}\n",
                message::date(e.unix),
                e.id,
                e.mid,
                e.mode,
                e.who
            );
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remembers_unexpired_decodes() {
        let tmp = tempfile::tempdir().unwrap();
        let calls = vec!["K1ABC".to_string()];
        let keep = Received {
            id: 0xAB,
            expires_unix: 2000,
            acks: vec![(3, "K1ABC".into())],
        };
        let old = Received {
            id: 0xCD,
            expires_unix: 500,
            acks: vec![],
        };
        record_received(tmp.path(), 100, "MID1", "addressed", &calls, &keep).unwrap();
        record_received(tmp.path(), 100, "MID2", "bulletin", &calls, &old).unwrap();
        assert_eq!(decoded_since(tmp.path(), 100).unwrap(), vec![0xAB, 0xCD]);
        assert!(decoded_since(tmp.path(), 101).unwrap().is_empty());
        let back = unexpired(tmp.path(), 1000).unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!((back[0].id, back[0].expires_unix), (0xAB, 2000));
        assert_eq!(back[0].acks, vec![(3, "K1ABC".to_string())]);
    }

    #[test]
    fn report_lists_both_sides() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(
            report(tmp.path()).unwrap(),
            "sent\n  nothing yet\nreceived\n  nothing yet\n"
        );
        let outcome = vec![
            ("K1ABC".into(), "decoded".into()),
            ("W2XYZ".into(), "need 3".into()),
        ];
        record_sent(tmp.path(), 0, 0x1234, "MID1", "addressed", &outcome).unwrap();
        let text = report(tmp.path()).unwrap();
        assert!(
            text.contains(
                "1970/01/01 00:00  00001234  MID1          addressed  K1ABC decoded, W2XYZ need 3"
            ),
            "{text}"
        );
    }
}
