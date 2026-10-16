//! The host the command line gives a session (ADR-006): the real wall clock, and AL Runner's
//! identity. The core crates read both through `al-host`; only this crate touches the OS.

use al_host::{AlRunnerIdentity, Clock, FileSystem, Host};
use std::path::{Component, PathBuf};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

/// The system clock, read once: the instant the run first asked for the time. Every later answer
/// is that instant (the session adds the time `Sleep` has waited and keeps system stamps strictly
/// increasing, see `al-host`). A clock that ran with the wall would make an outcome depend on the
/// load of the machine: `Item.OnModify` sets "Last Time Modified" to `Time`, and
/// `Format(xRec) <> Format(Rec)` is then true only when a second passed between the insert and the
/// modify, which decides whether Base Application's workflow subscriber runs. The wall clock is
/// back with `ALRUNNER_REAL_CLOCK=1`.
pub struct SystemClock;

impl SystemClock {
    fn wall() -> i64 {
        match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(d) => d.as_millis() as i64,
            // A clock set before 1970: the same distance on the other side.
            Err(e) => -(e.duration().as_millis() as i64),
        }
    }
}

impl Clock for SystemClock {
    fn now_utc_millis(&self) -> i64 {
        static START: OnceLock<i64> = OnceLock::new();
        if std::env::var_os("ALRUNNER_REAL_CLOCK").is_some() {
            return SystemClock::wall();
        }
        *START.get_or_init(SystemClock::wall)
    }
}

/// A clock that stands still at one instant.
pub struct FixedClock(pub i64);

impl Clock for FixedClock {
    fn now_utc_millis(&self) -> i64 {
        self.0
    }
}

/// The directory `--files-dir` named, if any. Set once at start-up.
static FILES_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Makes the `File` data type use a real directory instead of memory (`--files-dir DIR`).
pub fn set_files_dir(dir: PathBuf) {
    let _ = FILES_DIR.set(dir);
}

/// The environment `--environment` named. Set once at start-up; the default is the probed SaaS-like sandbox.
static ENVIRONMENT: OnceLock<al_host::Environment> = OnceLock::new();

/// `--environment saas|onprem` (owner decision 2026-10-15, ADR-014 amendment).
pub fn set_environment(environment: al_host::Environment) {
    let _ = ENVIRONMENT.set(environment);
}

/// The environment of the system host, as text (a key of the install baseline on disk holds it).
pub fn environment_text() -> &'static str {
    match ENVIRONMENT.get().copied().unwrap_or_default() {
        al_host::Environment::Saas => "saas",
        al_host::Environment::OnPrem => "onprem",
    }
}

/// Whether `--files-dir` gave the `File` type a real directory: what a run reads from there is not part of any key.
pub fn uses_real_files() -> bool {
    FILES_DIR.get().is_some()
}

/// Whether the system host reads the wall clock as it goes (`ALRUNNER_REAL_CLOCK`) instead of standing at the instant
/// the run began.
pub fn real_clock() -> bool {
    std::env::var_os("ALRUNNER_REAL_CLOCK").is_some()
}

/// The real clock and AL Runner's identity, on the file system the command line chose: memory,
/// unless `--files-dir` named a directory, on the environment `--environment` named.
pub fn system_host() -> Host {
    host_at(None)
}

/// [`system_host`] whose clock stands still at `start` (Unix milliseconds), or reads the system clock when `None`.
pub fn host_at(start: Option<i64>) -> Host {
    let host = match start {
        Some(ms) => Host::new(FixedClock(ms), AlRunnerIdentity),
        None => Host::new(SystemClock, AlRunnerIdentity),
    };
    let host = host.in_environment(ENVIRONMENT.get().copied().unwrap_or_default());
    match FILES_DIR.get() {
        Some(dir) => host.with_files(DirFileSystem::new(dir.clone())),
        None => host,
    }
}

/// Files in one directory. AL paths are taken as relative to it, with `/` or `\` as separators;
/// a path that is absolute or climbs out with `..` names no file (the run cannot touch anything
/// outside the directory the user opted into).
pub struct DirFileSystem {
    root: PathBuf,
}

impl DirFileSystem {
    pub fn new(root: PathBuf) -> DirFileSystem {
        DirFileSystem { root }
    }

    fn resolve(&self, path: &str) -> Option<PathBuf> {
        let rel = PathBuf::from(path.replace('\\', "/"));
        let mut out = self.root.clone();
        for c in rel.components() {
            match c {
                Component::Normal(p) => out.push(p),
                Component::CurDir => {}
                _ => return None,
            }
        }
        (out != self.root).then_some(out)
    }
}

impl FileSystem for DirFileSystem {
    fn read(&self, path: &str) -> Option<Vec<u8>> {
        std::fs::read(self.resolve(path)?).ok()
    }
    fn write(&mut self, path: &str, bytes: &[u8]) -> Result<(), String> {
        let p = self
            .resolve(path)
            .ok_or_else(|| format!("`{path}` is outside the files directory"))?;
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        std::fs::write(p, bytes).map_err(|e| e.to_string())
    }
    fn exists(&self, path: &str) -> bool {
        self.resolve(path).is_some_and(|p| p.is_file())
    }
    fn remove(&mut self, path: &str) -> bool {
        self.resolve(path).is_some_and(|p| std::fs::remove_file(p).is_ok())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_directory_file_system_keeps_files_under_its_root_only() {
        let dir = tempfile::tempdir().unwrap();
        let mut fs = DirFileSystem::new(dir.path().to_path_buf());
        fs.write("a/b.txt", b"hi").unwrap();
        assert_eq!(std::fs::read(dir.path().join("a/b.txt")).unwrap(), b"hi");
        assert_eq!(fs.read("a\\b.txt").unwrap(), b"hi");
        assert!(fs.exists("a/b.txt") && !fs.exists("a"));
        assert!(fs.write("../escape.txt", b"x").is_err());
        assert!(fs.write("/abs.txt", b"x").is_err());
        assert!(!fs.exists("../escape.txt"));
        assert!(fs.remove("a/b.txt") && !fs.remove("a/b.txt"));
    }

    #[test]
    fn the_system_clock_is_after_2024_and_does_not_run_backwards() {
        let c = SystemClock;
        let (a, b) = (c.now_utc_millis(), c.now_utc_millis());
        assert!(a > 1_704_067_200_000, "{a}");
        assert!(b >= a);
    }

    /// Red before the fix: the clock followed the wall, so a run on a loaded machine saw other
    /// times than one on a quiet machine.
    #[test]
    fn the_clock_of_a_run_stands_still() {
        let c = SystemClock;
        let a = c.now_utc_millis();
        std::thread::sleep(std::time::Duration::from_millis(30));
        assert_eq!(c.now_utc_millis(), a);
    }
}
