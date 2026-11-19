use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result, bail};
use ed25519_dalek::VerifyingKey;
use serde::{Deserialize, Serialize};

use crate::ax25::Addr;
use crate::{keys, londopy, message};

const KIND: &str = "roster";
const ENTRY: &str = "roster.toml";
const SCHEMA: u32 = 1;
const MAX_BYTES: u64 = 1 << 20;
const MAX_MEMBERS: usize = 255;
const MAX_GROUPS: usize = 64;

/// Who is on a net: member callsigns with their public keys, and the group
/// names the net uses. Net control shares it ahead of time as a .londopy file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Roster {
    pub schema: u32,
    pub net: String,
    #[serde(default)]
    pub groups: Vec<String>,
    #[serde(default, rename = "member")]
    pub members: Vec<Member>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    pub call: String,
    /// Ed25519 public key, 64 hex digits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

impl Roster {
    pub fn new(net: &Addr) -> Roster {
        Roster {
            schema: SCHEMA,
            net: net.to_string(),
            groups: vec![],
            members: vec![],
        }
    }

    pub fn load(path: &Path) -> Result<Roster> {
        let mut f = londopy::LondopyFile::open(path)?;
        let m = &f.manifest;
        if m.app != londopy::APP_ID || m.kind != KIND {
            bail!(
                "{} is a {} {} file, not a netcast roster",
                path.display(),
                m.app,
                m.kind
            );
        }
        let text = String::from_utf8(f.read_entry(ENTRY, MAX_BYTES)?)
            .context("roster.toml is not UTF-8")?;
        let r: Roster = toml::from_str(&text).context("reading roster.toml")?;
        if r.schema > SCHEMA {
            bail!(
                "{} is a newer roster (schema {}); update netcast",
                path.display(),
                r.schema
            );
        }
        r.check()
            .with_context(|| format!("{} has a bad entry", path.display()))?;
        Ok(r)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        self.check()?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let manifest = londopy::Manifest::new(KIND, &self.net, &message::rfc3339(now));
        let text = toml::to_string(self)?;
        londopy::write(path, &manifest, &[(ENTRY, text.as_bytes())])
    }

    fn check(&self) -> Result<()> {
        // a delivery names at most 255 stations, so a bigger roster can't be a real net
        if self.members.len() > MAX_MEMBERS || self.groups.len() > MAX_GROUPS {
            bail!(
                "a roster holds up to {MAX_MEMBERS} members and {MAX_GROUPS} groups; this one has {} and {}",
                self.members.len(),
                self.groups.len()
            );
        }
        let mut seen = HashSet::new();
        for name in std::iter::once(&self.net)
            .chain(&self.groups)
            .chain(self.members.iter().map(|m| &m.call))
        {
            let a: Addr = name.parse().map_err(anyhow::Error::msg)?;
            if a.to_string() != *name {
                bail!("{name:?} should be written {a}");
            }
            if !seen.insert(name) {
                bail!("{name} is listed twice");
            }
        }
        for m in &self.members {
            if let Some(k) = &m.key {
                keys::parse_public(k).with_context(|| format!("key for {}", m.call))?;
            }
        }
        Ok(())
    }

    /// Adds a member, or replaces its key if it's already there.
    pub fn add_member(&mut self, call: &Addr, key: Option<&VerifyingKey>) -> Result<()> {
        let call = call.to_string();
        if call == self.net || self.groups.contains(&call) {
            bail!("{call} is already a group name in this roster");
        }
        let key = key.map(|k| keys::hex(k.as_bytes()));
        match self.members.iter_mut().find(|m| m.call == call) {
            Some(m) => m.key = key,
            None => self.members.push(Member { call, key }),
        }
        Ok(())
    }

    pub fn add_group(&mut self, name: &Addr) -> Result<()> {
        let name = name.to_string();
        if self.knows(&name) {
            bail!("{name} is already in this roster");
        }
        self.groups.push(name);
        Ok(())
    }

    /// Removes a member or group. False if it wasn't there.
    pub fn remove(&mut self, name: &Addr) -> bool {
        let name = name.to_string();
        let before = self.members.len() + self.groups.len();
        self.members.retain(|m| m.call != name);
        self.groups.retain(|g| *g != name);
        before != self.members.len() + self.groups.len()
    }

    /// True for a member, a group, or the net itself.
    pub fn knows(&self, name: &str) -> bool {
        name == self.net
            || self.groups.iter().any(|g| g == name)
            || self.members.iter().any(|m| m.call == name)
    }

    pub fn calls(&self) -> Vec<String> {
        self.members.iter().map(|m| m.call.clone()).collect()
    }

    pub fn keys(&self) -> HashMap<String, VerifyingKey> {
        self.members
            .iter()
            .filter_map(|m| {
                let k = keys::parse_public(m.key.as_deref()?).expect("checked on load");
                Some((m.call.clone(), k))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;

    fn addr(s: &str) -> Addr {
        s.parse().unwrap()
    }

    #[test]
    fn round_trips_through_a_londopy_file() {
        let key = SigningKey::from_bytes(&[4; 32]).verifying_key();
        let mut r = Roster::new(&addr("NET1"));
        r.add_group(&addr("ARES")).unwrap();
        r.add_member(&addr("k1abc"), Some(&key)).unwrap();
        r.add_member(&addr("W2XYZ-7"), None).unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("net1.londopy");
        r.save(&path).unwrap();

        let back = Roster::load(&path).unwrap();
        assert_eq!(back, r);
        assert_eq!(back.keys().get("K1ABC"), Some(&key));
        assert!(back.knows("ARES") && back.knows("NET1") && back.knows("W2XYZ-7"));
        assert!(!back.knows("N3OOO"));
        assert_eq!(back.calls(), vec!["K1ABC", "W2XYZ-7"]);
    }

    #[test]
    fn edits_replace_and_remove() {
        let mut r = Roster::new(&addr("NET1"));
        r.add_member(&addr("K1ABC"), None).unwrap();
        let key = SigningKey::from_bytes(&[5; 32]).verifying_key();
        r.add_member(&addr("K1ABC"), Some(&key)).unwrap();
        assert_eq!(r.members.len(), 1);
        assert!(r.members[0].key.is_some());
        assert!(r.add_group(&addr("K1ABC")).is_err());
        assert!(r.add_member(&addr("NET1"), None).is_err());
        assert!(r.remove(&addr("K1ABC")));
        assert!(!r.remove(&addr("K1ABC")));
    }

    #[test]
    fn refuses_bad_or_foreign_files() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("x.londopy");
        let other = londopy::Manifest {
            app: "clipvault".into(),
            ..londopy::Manifest::new("profile", "x", "2026-11-19T00:00:00Z")
        };
        londopy::write(&path, &other, &[(ENTRY, b"schema = 1\nnet = \"NET1\"\n")]).unwrap();
        assert!(
            Roster::load(&path)
                .unwrap_err()
                .to_string()
                .contains("not a netcast roster")
        );

        let mine = londopy::Manifest::new(KIND, "NET1", "2026-11-19T00:00:00Z");
        londopy::write(&path, &mine, &[(ENTRY, b"schema = 2\nnet = \"NET1\"\n")]).unwrap();
        assert!(
            Roster::load(&path)
                .unwrap_err()
                .to_string()
                .contains("update netcast")
        );

        let bad_key = b"schema = 1\nnet = \"NET1\"\n[[member]]\ncall = \"K1ABC\"\nkey = \"zz\"\n";
        londopy::write(&path, &mine, &[(ENTRY, bad_key)]).unwrap();
        assert!(Roster::load(&path).is_err());

        let twice = b"schema = 1\nnet = \"NET1\"\n[[member]]\ncall = \"K1ABC\"\n[[member]]\ncall = \"K1ABC\"\n";
        londopy::write(&path, &mine, &[(ENTRY, twice)]).unwrap();
        assert!(format!("{:#}", Roster::load(&path).unwrap_err()).contains("listed twice"));

        let mut big = Roster::new(&addr("NET1"));
        for i in 0..=MAX_MEMBERS {
            big.members.push(Member {
                call: format!("K{i}AA"),
                key: None,
            });
        }
        assert!(big.save(&path).is_err());

        let lower = b"schema = 1\nnet = \"net1\"\n";
        londopy::write(&path, &mine, &[(ENTRY, lower)]).unwrap();
        assert!(Roster::load(&path).is_err());
    }
}
