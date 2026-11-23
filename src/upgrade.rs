use std::fs;
use std::io::{self, BufRead, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use semver::Version;
use serde::Deserialize;

use crate::{codec, keys};

const LATEST: &str = "https://api.github.com/repos/Londopy/netcast/releases/latest";
const DOWNLOADS: &str = "https://github.com/Londopy/netcast/releases/download";
const UA: &str = concat!("netcast/", env!("CARGO_PKG_VERSION"));
// per step, not for the whole download: a station on a slow link still gets there
const TIMEOUT: Duration = Duration::from_secs(30);
const MAX_DOWNLOAD: u64 = 64 << 20;
const DAY_S: u64 = 86400;
const SMOKE_TIMEOUT: Duration = Duration::from_secs(15);
const CACHE: &str = "update-check";

#[derive(Deserialize)]
struct Release {
    tag_name: String,
}

fn current() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION")).expect("the crate version is semver")
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .https_only(true)
        .timeout_connect(Some(TIMEOUT))
        .timeout_recv_response(Some(TIMEOUT))
        .timeout_per_read(Some(TIMEOUT))
        .build()
        .into()
}

fn fetch(agent: &ureq::Agent, url: &str) -> Result<Vec<u8>> {
    let resp = agent
        .get(url)
        .header("User-Agent", UA)
        .call()
        .with_context(|| format!("downloading {url}"))?;
    let mut body = resp.into_body();
    body.with_config()
        .limit(MAX_DOWNLOAD)
        .read_to_vec()
        .with_context(|| format!("reading {url}"))
}

/// The newest published netcast.
fn latest(agent: &ureq::Agent) -> Result<Version> {
    let body = fetch(agent, LATEST).context("asking GitHub for the latest release")?;
    let r: Release = serde_json::from_slice(&body)?;
    Version::parse(r.tag_name.trim_start_matches('v'))
        .with_context(|| format!("the latest release tag {:?} isn't a version", r.tag_name))
}

/// The release file this build of netcast upgrades from: the standalone exe
/// on Windows, the bare binary on Linux.
fn asset_name(v: &Version, os: &str, arch: &str) -> Option<String> {
    let arch = match arch {
        "x86_64" => "x64",
        "x86" => "x86",
        "aarch64" => "arm64",
        _ => return None,
    };
    match os {
        "windows" => Some(format!("netcast-v{v}-{arch}-portable.exe")),
        "linux" if arch != "x86" => Some(format!("netcast-v{v}-linux-{arch}")),
        _ => None,
    }
}

/// Checks a download against its line in SHA256SUMS.txt. A missing line is as
/// bad as a wrong one: nothing unverified gets installed.
fn verify(data: &[u8], sums: &str, name: &str) -> Result<()> {
    let want = sums
        .lines()
        .filter_map(|l| l.split_once(char::is_whitespace))
        .find(|(_, file)| file.trim().trim_start_matches('*') == name)
        .map(|(hash, _)| hash.to_ascii_lowercase())
        .ok_or_else(|| {
            anyhow!("SHA256SUMS.txt has no entry for {name}, so it won't be installed")
        })?;
    let got = keys::hex(&codec::hash(data));
    if got != want {
        bail!(
            "{name} doesn't match SHA256SUMS.txt (got {got}, expected {want}), so it won't be installed"
        );
    }
    Ok(())
}

/// True for an answer that means go ahead. Anything else, an empty line
/// included, leaves the old version in place.
fn agreed(answer: &str) -> bool {
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// Asks before installing, unless `yes` already said so. With nobody at the
/// keyboard to ask, it doesn't install.
fn confirm(latest: &Version, yes: bool) -> Result<bool> {
    if yes {
        return Ok(true);
    }
    if !io::stdin().is_terminal() {
        bail!(
            "not installing without asking; run `netcast upgrade --yes` to install from a script"
        );
    }
    print!("Install netcast {latest} now? [y/N] ");
    io::stdout().flush()?;
    let mut answer = String::new();
    io::stdin().lock().read_line(&mut answer)?;
    Ok(agreed(&answer))
}

/// `netcast upgrade`: tells what's available, and unless `check_only`, installs
/// it once the user says yes (or `yes` says so for them).
pub fn run(check_only: bool, yes: bool) -> Result<()> {
    let agent = agent();
    let latest = latest(&agent)?;
    let cur = current();
    if latest <= cur {
        println!("netcast {cur} is up to date");
        return Ok(());
    }
    println!("netcast {latest} is available (you have {cur})");
    println!("what's new: https://github.com/Londopy/netcast/releases/tag/v{latest}");
    if check_only {
        return Ok(());
    }
    let exe = std::env::current_exe().context("finding the running netcast")?;
    // before asking, so a yes isn't followed by a download that can't be installed
    can_write_next_to(&exe)?;
    if !confirm(&latest, yes)? {
        println!("not installed; netcast {cur} is unchanged");
        return Ok(());
    }
    let name = asset_name(&latest, std::env::consts::OS, std::env::consts::ARCH).ok_or_else(|| {
        anyhow!("there's no netcast build for this platform; see https://github.com/Londopy/netcast/releases")
    })?;
    let base = format!("{DOWNLOADS}/v{latest}");
    let sums = fetch(&agent, &format!("{base}/SHA256SUMS.txt"))?;
    let data = fetch(&agent, &format!("{base}/{name}"))?;
    verify(&data, &String::from_utf8_lossy(&sums), &name)?;
    install(&exe, &data, &latest)?;
    println!("updated to netcast {latest}");
    Ok(())
}

fn can_write_next_to(exe: &Path) -> Result<()> {
    let dir = exe
        .parent()
        .ok_or_else(|| anyhow!("{} has no folder", exe.display()))?;
    let probe = dir.join(".netcast-upgrade-check");
    fs::write(&probe, b"").map_err(|e| not_writable(dir, e))?;
    let _ = fs::remove_file(&probe);
    Ok(())
}

fn not_writable(dir: &Path, e: std::io::Error) -> anyhow::Error {
    let how = if cfg!(windows) {
        "if netcast is installed for all users, run the new installer from https://github.com/Londopy/netcast/releases instead"
    } else {
        "run `sudo netcast upgrade`, or install netcast somewhere your user can write, like ~/.local/bin"
    };
    anyhow!("can't write to {} ({e}); {how}", dir.display())
}

/// Puts the new binary in place of `exe`. It's written next to `exe` (so the
/// final rename stays on one disk and the bytes that were checked are the
/// bytes that run) and must start before anything is replaced.
fn install(exe: &Path, data: &[u8], version: &Version) -> Result<()> {
    let dir = exe
        .parent()
        .ok_or_else(|| anyhow!("{} has no folder", exe.display()))?;
    let staged = dir.join(format!(
        ".netcast-{version}{}",
        std::env::consts::EXE_SUFFIX
    ));
    // a leftover from an earlier attempt that didn't finish
    let _ = fs::remove_file(&staged);
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&staged)
        .map_err(|e| not_writable(dir, e))?;
    let written = f.write_all(data).and_then(|_| f.sync_all());
    drop(f);
    // the same permissions as the binary it replaces, so a system-wide install
    // stays usable by everyone
    #[cfg(unix)]
    let written = written
        .and_then(|_| fs::metadata(exe))
        .and_then(|m| fs::set_permissions(&staged, m.permissions()));
    if let Err(e) = written
        .map_err(anyhow::Error::from)
        .and_then(|_| smoke_test(&staged, version))
    {
        let _ = fs::remove_file(&staged);
        return Err(e);
    }
    swap(exe, &staged).inspect_err(|_| {
        let _ = fs::remove_file(&staged);
    })
}

/// Runs the new binary once, so a build that can't start here (a missing
/// library, the wrong glibc) never replaces one that works.
fn smoke_test(staged: &Path, version: &Version) -> Result<()> {
    let mut child = Command::new(staged)
        .arg("--version")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("starting the downloaded netcast {version}"))?;
    let deadline = Instant::now() + SMOKE_TIMEOUT;
    let status = loop {
        if let Some(s) = child.try_wait()? {
            break s;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "the downloaded netcast {version} didn't answer `--version` within {} s, so nothing was changed",
                SMOKE_TIMEOUT.as_secs()
            );
        }
        thread::sleep(Duration::from_millis(100));
    };
    let mut said = String::new();
    if let Some(mut out) = child.stdout.take() {
        out.read_to_string(&mut said)?;
    }
    if !status.success() || !said.contains(&version.to_string()) {
        bail!(
            "the downloaded netcast {version} didn't run here (it said {:?}), so nothing was changed",
            said.trim()
        );
    }
    Ok(())
}

// a running exe can't be overwritten on Windows, but it can be renamed
#[cfg(windows)]
fn swap(exe: &Path, staged: &Path) -> Result<()> {
    let old = old_exe(exe);
    if fs::remove_file(&old).is_err() && old.exists() {
        bail!(
            "an earlier netcast is still running from {} (a listener or the web page?); stop it and try again",
            old.display()
        );
    }
    fs::rename(exe, &old).with_context(|| format!("moving {} aside", exe.display()))?;
    if let Err(e) = fs::rename(staged, exe) {
        let _ = fs::rename(&old, exe);
        return Err(e).with_context(|| format!("putting the new netcast at {}", exe.display()));
    }
    Ok(())
}

// on one filesystem a rename replaces the old file in a single step
#[cfg(not(windows))]
fn swap(exe: &Path, staged: &Path) -> Result<()> {
    fs::rename(staged, exe).with_context(|| format!("putting the new netcast at {}", exe.display()))
}

fn old_exe(exe: &Path) -> PathBuf {
    exe.with_extension("exe.old")
}

/// Removes the old exe a Windows upgrade left behind, which couldn't be deleted
/// while it was still running.
pub fn clean_up() {
    if cfg!(windows)
        && let Ok(exe) = std::env::current_exe()
    {
        let _ = fs::remove_file(old_exe(&exe));
    }
}

/// What the daily check remembers: when it last asked GitHub, what it heard,
/// and when it last told the user.
#[derive(Debug, PartialEq)]
struct Cache {
    checked: u64,
    latest: Option<Version>,
    told: u64,
}

impl Cache {
    fn parse(text: &str) -> Cache {
        let mut f = text.split_whitespace();
        let checked = f.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        let latest = f.next().and_then(|s| Version::parse(s).ok());
        let told = f.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        Cache {
            checked,
            latest,
            told,
        }
    }

    /// What's due now: a newer version to tell about (at most once a day), and
    /// whether to ask GitHub (also at most once a day). Stamps both, so an
    /// offline station doesn't ask on every command either.
    fn due(&mut self, now: u64, current: &Version) -> (Option<Version>, bool) {
        let tell = self
            .latest
            .clone()
            .filter(|v| v > current && a_day_since(self.told, now));
        if tell.is_some() {
            self.told = now;
        }
        let check = a_day_since(self.checked, now);
        if check {
            self.checked = now;
        }
        (tell, check)
    }

    fn line(&self) -> String {
        let v = self
            .latest
            .as_ref()
            .map_or("-".to_string(), Version::to_string);
        format!("{} {v} {}\n", self.checked, self.told)
    }
}

// a stamp from the future (a clock that was wrong) counts as long ago
fn a_day_since(then: u64, now: u64) -> bool {
    then > now || now - then >= DAY_S
}

/// Prints a one-line notice, at most once a day, when a newer netcast is out.
/// GitHub is asked at most once a day too, from a background thread, so a
/// command is never held up by it. `NETCAST_NO_UPDATE_CHECK=1` turns it off,
/// and it stays off under CI.
pub fn notice(dir: &Path, now: u64) {
    if std::env::var_os("NETCAST_NO_UPDATE_CHECK").is_some() || std::env::var_os("CI").is_some() {
        return;
    }
    let path = dir.join(CACHE);
    let mut cache = Cache::parse(&fs::read_to_string(&path).unwrap_or_default());
    let (tell, check) = cache.due(now, &current());
    if let Some(v) = tell {
        eprintln!(
            "netcast {v} is available (you have {}); run `netcast upgrade` to install it",
            current()
        );
    }
    if fs::create_dir_all(dir).is_err() || fs::write(&path, cache.line()).is_err() {
        return;
    }
    if check {
        thread::spawn(move || {
            // a check that fails just runs again tomorrow; it's only a courtesy
            if let Ok(v) = latest(&agent()) {
                cache.latest = Some(v);
                let _ = fs::write(&path, cache.line());
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_the_file_for_each_platform() {
        let v = Version::new(1, 2, 3);
        assert_eq!(
            asset_name(&v, "windows", "x86_64").as_deref(),
            Some("netcast-v1.2.3-x64-portable.exe")
        );
        assert_eq!(
            asset_name(&v, "windows", "aarch64").as_deref(),
            Some("netcast-v1.2.3-arm64-portable.exe")
        );
        assert_eq!(
            asset_name(&v, "linux", "aarch64").as_deref(),
            Some("netcast-v1.2.3-linux-arm64")
        );
        assert_eq!(asset_name(&v, "linux", "x86"), None);
        assert_eq!(asset_name(&v, "macos", "aarch64"), None);
    }

    #[test]
    fn installs_only_what_the_checksums_vouch_for() {
        let data = b"new netcast";
        let hash = keys::hex(&codec::hash(data));
        let sums = format!("{hash}  netcast-v1.2.3-x64-portable.exe\nabc  other.zip\n");
        assert!(verify(data, &sums, "netcast-v1.2.3-x64-portable.exe").is_ok());
        let star = format!("{hash} *netcast-v1.2.3-x64-portable.exe\n");
        assert!(verify(data, &star, "netcast-v1.2.3-x64-portable.exe").is_ok());
        assert!(verify(b"tampered", &sums, "netcast-v1.2.3-x64-portable.exe").is_err());
        let missing = verify(data, &sums, "netcast-v1.2.3-arm64-portable.exe").unwrap_err();
        assert!(missing.to_string().contains("no entry"));
    }

    #[test]
    fn a_binary_that_wont_run_never_replaces_the_one_that_does() {
        let tmp = tempfile::tempdir().unwrap();
        let exe = tmp
            .path()
            .join(format!("netcast{}", std::env::consts::EXE_SUFFIX));
        fs::write(&exe, b"the working netcast").unwrap();
        // the MZ header makes Windows refuse it quietly; without one it treats the
        // file as a 16-bit program and opens a dialog that stalls the test run
        let err = install(&exe, b"MZ not a program", &Version::new(9, 9, 9)).unwrap_err();
        assert!(format!("{err:#}").contains("9.9.9"), "{err:#}");
        assert_eq!(fs::read(&exe).unwrap(), b"the working netcast");
        // nothing staged is left behind
        assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 1);
    }

    #[test]
    fn the_swap_puts_the_new_binary_in_place() {
        let tmp = tempfile::tempdir().unwrap();
        let exe = tmp.path().join("netcast.exe");
        let staged = tmp.path().join(".netcast-9.9.9.exe");
        fs::write(&exe, b"old").unwrap();
        fs::write(&staged, b"new").unwrap();
        swap(&exe, &staged).unwrap();
        assert_eq!(fs::read(&exe).unwrap(), b"new");
        assert!(!staged.exists());
    }

    #[test]
    fn the_notice_shows_once_a_day_and_the_check_runs_once_a_day() {
        assert!(a_day_since(0, DAY_S));
        assert!(!a_day_since(DAY_S, DAY_S + 10));
        assert!(a_day_since(DAY_S * 9, DAY_S), "a stamp from the future");
        let c = Cache::parse("1700000000 0.2.0 1700000100\n");
        assert_eq!(
            c,
            Cache {
                checked: 1_700_000_000,
                latest: Some(Version::new(0, 2, 0)),
                told: 1_700_000_100
            }
        );
        assert_eq!(Cache::parse(&c.line()), c);
        let offline = Cache::parse("1700000000 - 0");
        assert_eq!(offline.latest, None);
        assert_eq!(Cache::parse("garbage").checked, 0);
    }

    #[test]
    fn tells_and_asks_at_most_once_a_day() {
        let mine = Version::new(0, 1, 0);
        let mut c = Cache::parse("");
        // first run: nothing known yet, so ask, and say nothing
        assert_eq!(c.due(DAY_S * 10, &mine), (None, true));
        assert_eq!(c.due(DAY_S * 10 + 60, &mine), (None, false));
        // the answer came back with a newer version: tell once, not again that day
        c.latest = Some(Version::new(0, 2, 0));
        assert_eq!(
            c.due(DAY_S * 10 + 120, &mine),
            (Some(Version::new(0, 2, 0)), false)
        );
        assert_eq!(c.due(DAY_S * 10 + 180, &mine), (None, false));
        // a day later both are due again
        assert_eq!(
            c.due(DAY_S * 11 + 200, &mine),
            (Some(Version::new(0, 2, 0)), true)
        );
        // nothing to tell once this version is installed
        let mut c = Cache::parse(&c.line());
        assert_eq!(c.due(DAY_S * 13, &Version::new(0, 2, 0)), (None, true));
    }

    #[test]
    fn a_failed_swap_keeps_the_old_binary() {
        let tmp = tempfile::tempdir().unwrap();
        let exe = tmp.path().join("netcast.exe");
        fs::write(&exe, b"old").unwrap();
        let missing = tmp.path().join(".netcast-9.9.9.exe");
        assert!(swap(&exe, &missing).is_err());
        assert_eq!(fs::read(&exe).unwrap(), b"old");
    }

    #[test]
    fn only_a_yes_installs() {
        for a in ["y\n", "Y", "yes\r\n", " YES "] {
            assert!(agreed(a), "{a:?}");
        }
        for a in ["", "\n", "n", "no", "yess", "sure"] {
            assert!(!agreed(a), "{a:?}");
        }
    }
}
