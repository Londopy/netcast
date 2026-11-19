use rand::RngExt;

/// Checks a B2F message the way Pat's parser reads it, and returns its MID.
/// One message Pat can't parse breaks its whole inbox listing, so anything doubtful is refused.
pub fn check(b2f: &[u8]) -> Result<String, String> {
    let end = b2f
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or("no blank line after the headers")?;
    let head = std::str::from_utf8(&b2f[..end]).map_err(|_| "headers are not UTF-8")?;
    let mut mid = None;
    let mut body = None;
    let mut files = vec![];
    for line in head.split("\r\n") {
        let (k, v) = line
            .split_once(':')
            .ok_or_else(|| format!("header line {line:?} has no colon"))?;
        if k.is_empty() || !k.bytes().all(is_token) {
            return Err(format!("bad header name {k:?}"));
        }
        let v = v.trim();
        match k.to_ascii_lowercase().as_str() {
            "mid" if mid.is_none() => mid = Some(v.to_string()),
            "body" if body.is_none() => body = Some(size(v)?),
            "file" => {
                let (n, _) = v
                    .split_once(' ')
                    .ok_or_else(|| format!("File: header {v:?} has no name"))?;
                files.push(size(n)?);
            }
            "date" if !v.is_empty() && !is_date(v) => {
                return Err(format!("Date: {v:?} is not YYYY/MM/DD HH:MM"));
            }
            _ => {}
        }
    }
    let mid = mid
        .filter(|m| valid_mid(m))
        .ok_or("no usable Mid: header (1 to 12 letters and digits)")?;
    let body = body.ok_or("no Body: header")?;
    // each section is followed by CRLF or the end of the message, and nothing comes after
    let mut rest = &b2f[end + 4..];
    for n in std::iter::once(body).chain(files) {
        if rest.len() < n {
            return Err("message is shorter than its Body: and File: sizes".into());
        }
        rest = &rest[n..];
        if !rest.is_empty() {
            rest = rest
                .strip_prefix(b"\r\n")
                .ok_or("a section doesn't end where its size says")?;
        }
    }
    if !rest.is_empty() {
        return Err("data after the last section".into());
    }
    Ok(mid)
}

fn size(v: &str) -> Result<usize, String> {
    if v.is_empty() || !v.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("size {v:?} is not a whole number"));
    }
    v.parse().map_err(|_| format!("size {v:?} is too big"))
}

fn is_token(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

fn is_date(v: &str) -> bool {
    let b = v.as_bytes();
    b.len() == 16
        && b.iter().enumerate().all(|(i, c)| match i {
            4 | 7 => *c == b'/',
            10 => *c == b' ',
            13 => *c == b':',
            _ => c.is_ascii_digit(),
        })
}

/// A MID ends up as a file name in Pat's mailbox, so only plain MIDs are allowed,
/// and none of the names Windows reserves for devices.
pub fn valid_mid(mid: &str) -> bool {
    let reserved = ["CON", "PRN", "AUX", "NUL"].contains(&mid.to_ascii_uppercase().as_str())
        || ["COM", "LPT"].iter().any(|p| {
            mid.len() == 4 && mid[..3].eq_ignore_ascii_case(p) && mid.as_bytes()[3].is_ascii_digit()
        });
    (1..=12).contains(&mid.len()) && mid.bytes().all(|b| b.is_ascii_alphanumeric()) && !reserved
}

/// Builds a plain-text B2F message the way Pat stores them.
pub fn compose(
    from: &str,
    to: &[String],
    subject: &str,
    body: &str,
    unix: u64,
) -> (String, Vec<u8>) {
    let mut rng = rand::rng();
    let mid: String = (0..12)
        .map(|_| {
            let c = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789"[rng.random_range(0..36)];
            c as char
        })
        .collect();
    let body = body.replace("\r\n", "\n").replace('\n', "\r\n");
    let subject: String = subject
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let mut head = format!("Mid: {mid}\r\nBody: {}\r\n", body.len());
    head += "Content-Transfer-Encoding: 8bit\r\nContent-Type: text/plain; charset=UTF-8\r\n";
    head += &format!(
        "Date: {}\r\nFrom: {from}\r\nMbo: {from}\r\nSubject: {subject}\r\n",
        date(unix)
    );
    for t in to {
        head += &format!("To: {t}\r\n");
    }
    head += "Type: Private\r\n\r\n";
    let mut msg = head.into_bytes();
    msg.extend_from_slice(body.as_bytes());
    msg.extend_from_slice(b"\r\n");
    (mid, msg)
}

/// UTC time as `YYYY/MM/DD HH:MM`, the form B2F uses.
pub fn date(unix: u64) -> String {
    let days = (unix / 86400) as i64;
    let secs = unix % 86400;
    // days-to-civil from Howard Hinnant's date algorithms
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}/{m:02}/{d:02} {:02}:{:02}",
        secs / 3600,
        secs % 3600 / 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn composed_message_parses_back() {
        let to = vec!["K1ABC".to_string(), "W2XYZ".to_string()];
        let (mid, msg) = compose("N0CALL", &to, "Net\r\nbulletin", "line one\nline two", 0);
        assert_eq!(check(&msg), Ok(mid));
        let text = String::from_utf8(msg).unwrap();
        assert!(text.contains("Subject: Net  bulletin\r\n"));
        assert!(text.contains("Body: 18\r\n"));
        assert!(text.contains("To: K1ABC\r\nTo: W2XYZ\r\n"));
        assert!(text.ends_with("\r\n\r\nline one\r\nline two\r\n"));
    }

    #[test]
    fn accepts_attachments_laid_out_like_pat() {
        let msg =
            b"Mid: ABC123\r\nBody: 2\r\nFile: 3 a.txt\r\nFile: 1 b.bin\r\n\r\nhi\r\nxyz\r\nq\r\n";
        assert_eq!(check(msg), Ok("ABC123".into()));
        // Pat leaves out the CRLF after a body with no attachments
        assert_eq!(
            check(b"Mid: ABC123\r\nBody: 2\r\n\r\nhi"),
            Ok("ABC123".into())
        );
    }

    #[test]
    fn refuses_what_would_break_pat() {
        let bad: [&[u8]; 9] = [
            b"Mid: ../../evil\r\nBody: 0\r\n\r\n",
            b"Mid: NUL\r\nBody: 0\r\n\r\n",
            b"Subject: hi\r\nBody: 0\r\n\r\n",
            b"Mid: ABC\r\nBody: 999999\r\n\r\nhi",
            b"Mid: ABC\r\nBody: -1\r\n\r\n",
            b"Mid: ABC\r\nBody: 2\r\n\r\nhi there",
            b"Mid: ABC\r\nBody: 2\r\nFile: 5 a.txt\r\n\r\nhi\r\nab\r\n",
            b"Mid: ABC\r\nBad Key: x\r\nBody: 0\r\n\r\n",
            b"Mid: ABC\r\nDate: yesterday\r\nBody: 0\r\n\r\n",
        ];
        for b in bad {
            assert!(check(b).is_err(), "{}", String::from_utf8_lossy(b));
        }
        assert!(check(b"just some text").is_err());
    }

    #[test]
    fn dates() {
        assert_eq!(date(0), "1970/01/01 00:00");
        assert_eq!(date(1_795_000_000), "2026/11/18 11:06");
        assert_eq!(date(951_782_400), "2000/02/29 00:00");
    }
}
