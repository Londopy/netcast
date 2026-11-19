use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use rand::RngExt;

use crate::frame::Announce;

const KEY_FILE: &str = "station.key";
// keeps a signature from being reused for anything but a netcast announcement
const DOMAIN: &[u8] = b"netcast announce v1\0";

pub fn key_path(dir: &Path) -> PathBuf {
    dir.join(KEY_FILE)
}

/// This station's signing key, if `netcast key` has made one.
pub fn load(dir: &Path) -> Result<Option<SigningKey>> {
    let path = key_path(dir);
    let text = match fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let seed = unhex::<32>(text.trim())
        .with_context(|| format!("{} is not a netcast key file", path.display()))?;
    Ok(Some(SigningKey::from_bytes(&seed)))
}

/// Loads this station's key, making one first if there is none.
pub fn load_or_create(dir: &Path) -> Result<SigningKey> {
    if let Some(k) = load(dir)? {
        return Ok(k);
    }
    let seed: [u8; 32] = rand::rng().random();
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = key_path(dir);
    write_private(&path, &hex(&seed))?;
    Ok(SigningKey::from_bytes(&seed))
}

/// Writes beside the key file and renames, so a crash never leaves half a key.
/// On Windows the file takes its folder's permissions, and the default folder
/// under %LOCALAPPDATA% is private to the user.
fn write_private(path: &Path, text: &str) -> Result<()> {
    use std::io::Write;
    let tmp = path.with_extension("key.tmp");
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    let mut f = opts
        .open(&tmp)
        .with_context(|| format!("creating {}", tmp.display()))?;
    f.write_all(text.as_bytes())?;
    f.sync_all()?;
    drop(f);
    fs::rename(&tmp, path).with_context(|| format!("saving {}", path.display()))?;
    Ok(())
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex<const N: usize>(s: &str) -> Result<[u8; N]> {
    if s.len() != N * 2 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("expected {} hex digits", N * 2);
    }
    let mut out = [0u8; N];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16)?;
    }
    Ok(out)
}

pub fn parse_public(s: &str) -> Result<VerifyingKey> {
    let bytes = unhex::<32>(s.trim()).with_context(|| format!("{s:?} is not a public key"))?;
    let key = VerifyingKey::from_bytes(&bytes)
        .map_err(|_| anyhow::anyhow!("{s} is not a valid Ed25519 key"))?;
    // a weak key lets anyone make signatures that check out against it
    if key.is_weak() {
        bail!("{s} is a weak Ed25519 key and can't be trusted");
    }
    Ok(key)
}

fn message(a: &Announce, id: u32) -> Vec<u8> {
    [DOMAIN, &id.to_be_bytes()[..], &a.body()].concat()
}

pub fn sign(a: &mut Announce, id: u32, key: &SigningKey) {
    a.signature = Some(key.sign(&message(a, id)).to_bytes());
}

/// True when the announcement carries a valid signature by `key` for this delivery.
pub fn verify(a: &Announce, id: u32, key: &VerifyingKey) -> bool {
    a.signature.is_some_and(|s| {
        key.verify_strict(&message(a, id), &Signature::from_bytes(&s))
            .is_ok()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{CODEC_BROTLI_RAPTORQ, Mode};
    use raptorq::ObjectTransmissionInformation;

    fn announce() -> Announce {
        Announce {
            codec: CODEC_BROTLI_RAPTORQ,
            oti: ObjectTransmissionInformation::new(1000, 244, 1, 1, 4),
            hash: [1; 32],
            mode: Mode::Addressed,
            ttl_min: 60,
            from: "N0NCS".into(),
            mid: "ABC123".into(),
            recipients: vec!["K1ABC".into()],
            signature: None,
        }
    }

    #[test]
    fn signature_covers_the_announcement_and_delivery() {
        let key = SigningKey::from_bytes(&[9; 32]);
        let public = key.verifying_key();
        let mut a = announce();
        sign(&mut a, 77, &key);
        assert!(verify(&a, 77, &public));
        assert!(!verify(&a, 78, &public), "reused under another delivery id");

        let mut changed = a.clone();
        changed.hash[0] ^= 1;
        assert!(!verify(&changed, 77, &public), "message hash changed");
        let mut changed = a.clone();
        changed.recipients.push("W2XYZ".into());
        assert!(!verify(&changed, 77, &public), "recipient added");

        let other = SigningKey::from_bytes(&[8; 32]).verifying_key();
        assert!(!verify(&a, 77, &other));
        assert!(!verify(&announce(), 77, &public), "unsigned");
    }

    #[test]
    fn key_file_is_made_once_and_reloads() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(load(tmp.path()).unwrap().is_none());
        let made = load_or_create(tmp.path()).unwrap();
        let again = load_or_create(tmp.path()).unwrap();
        assert_eq!(made.to_bytes(), again.to_bytes());
        let public = hex(made.verifying_key().as_bytes());
        assert_eq!(parse_public(&public).unwrap(), made.verifying_key());
        assert!(parse_public("abcd").is_err());
        fs::write(key_path(tmp.path()), "not a key").unwrap();
        assert!(load(tmp.path()).is_err());
    }

    // the same values are published in docs/protocol.md, so other implementations
    // can check theirs
    #[test]
    fn known_answer() {
        let key = SigningKey::from_bytes(&[1; 32]);
        let mut a = announce();
        sign(&mut a, 0x0102_0304, &key);
        assert_eq!(
            hex(key.verifying_key().as_bytes()),
            "8a88e3dd7409f195fd52db2d3cba5d72ca6709bf1d94121bf3748801b40f6f5c"
        );
        assert_eq!(
            hex(&a.body()),
            "0100000003e80000f401000104010101010101010101010101010101010101010101010101010101010101010101003c054e304e43530641424331323301054b31414243"
        );
        assert_eq!(
            hex(&a.signature.unwrap()),
            "c031e185fa51601ced1672303a51c0652d57f4efc863a98d66cf0b83a2a3164bc612b7dd3208d83f2e8dfbedd0f92f0d2a34196b3b52afc892cadaef732d9804"
        );
        assert_eq!(&DOMAIN[..19], b"netcast announce v1");
        assert_eq!(DOMAIN.len(), 20);
    }

    #[test]
    fn weak_keys_are_refused() {
        let identity = format!("01{}", "00".repeat(31));
        assert!(
            parse_public(&identity)
                .unwrap_err()
                .to_string()
                .contains("weak")
        );
    }
}
