//! The install baseline on disk. Running the install code of the dependency apps and `Company-Initialize` takes
//! longer than reading what they leave: the data of a baseline is a few hundred rows, and it is the same for the same
//! program. A run with the compile cache (ADR-022) keeps the baseline next to it, in `<cache>/baseline/`, under a key
//! of everything the install code can read, and reads it back instead of running the install code again.
//!
//! The file is the [`BaselineRun`] without the part only a run with test data has (`restored`), so a run
//! with test data never stores or reads one. The file is written to a name of its own and renamed, so a reader never
//! sees half a file; a file that cannot be read is removed and the install code runs.

use crate::baseline::BaselineRun;
use al_builtins::value_codec::{Reader, put_str, put_u8, put_u32};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// The first bytes of a file: what it is and which layout it has.
const MAGIC: &[u8; 8] = b"ALBRUN\0\x01";

/// How many files the folder keeps: the newest ones.
const KEPT: usize = 8;

/// The folder of the files, under the compile cache's.
pub fn folder(compile_cache_dir: &Path) -> PathBuf {
    compile_cache_dir.join("baseline")
}

/// What the key is made of, one part after the other; a part that is not given changes the key as well as one that
/// differs.
pub struct KeyBuilder(Sha256);

impl KeyBuilder {
    pub fn new() -> KeyBuilder {
        let mut h = Sha256::new();
        h.update(MAGIC);
        KeyBuilder(h)
    }
    pub fn text(&mut self, name: &str, value: &str) -> &mut Self {
        self.bytes(name, value.as_bytes())
    }
    pub fn bytes(&mut self, name: &str, value: &[u8]) -> &mut Self {
        self.0.update((name.len() as u64).to_le_bytes());
        self.0.update(name.as_bytes());
        self.0.update((value.len() as u64).to_le_bytes());
        self.0.update(value);
        self
    }
    pub fn finish(&self) -> String {
        self.0.clone().finalize().iter().map(|b| format!("{b:02x}")).collect()
    }
}

impl Default for KeyBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// The bytes of a run, or `None` when it cannot be kept: it recorded changes to a restored company (test data), or
/// its baseline has no bytes.
pub fn encode(run: &BaselineRun) -> Option<Vec<u8>> {
    if !run.restored.is_empty() {
        return None;
    }
    let mut out = MAGIC.to_vec();
    put_u32(&mut out, run.unseeded.len() as u32);
    for t in &run.unseeded {
        put_u32(&mut out, *t);
    }
    put_u32(&mut out, run.filled as u32);
    put_u32(&mut out, run.ran as u32);
    put_u32(&mut out, run.total as u32);
    put_u32(&mut out, run.notes.len() as u32);
    for n in &run.notes {
        put_str(&mut out, n);
    }
    put_u8(&mut out, 1);
    let baseline = run.baseline.to_bytes()?;
    out.extend_from_slice(&baseline);
    Some(out)
}

/// The run [`encode`] wrote, or `None` for anything else.
pub fn decode(bytes: &[u8]) -> Option<BaselineRun> {
    let mut r = Reader::new(bytes.strip_prefix(&MAGIC[..])?);
    let mut unseeded = std::collections::BTreeSet::new();
    for _ in 0..r.u32()? {
        unseeded.insert(r.u32()?);
    }
    let filled = r.u32()? as usize;
    let ran = r.u32()? as usize;
    let total = r.u32()? as usize;
    let mut notes = Vec::new();
    for _ in 0..r.u32()? {
        notes.push(r.str()?);
    }
    if r.u8()? != 1 {
        return None;
    }
    let baseline = al_vm::Baseline::from_bytes(r.rest())?;
    Some(BaselineRun {
        baseline,
        unseeded,
        filled,
        notes,
        ran,
        total,
        restored: Vec::new(),
    })
}

fn file(dir: &Path, key: &str) -> PathBuf {
    folder(dir).join(format!("{key}.albrun"))
}

/// The run kept under `key`. A file that cannot be read is removed.
pub fn read(dir: &Path, key: &str) -> Option<BaselineRun> {
    let path = file(dir, key);
    let bytes = std::fs::read(&path).ok()?;
    match decode(&bytes) {
        Some(run) => Some(run),
        None => {
            let _ = std::fs::remove_file(&path);
            None
        }
    }
}

/// Keeps `run` under `key` and drops all but the newest files. Best effort: a folder that cannot be written costs
/// the next run its install code, nothing else.
pub fn write(dir: &Path, key: &str, run: &BaselineRun) {
    let Some(bytes) = encode(run) else {
        return;
    };
    let folder = folder(dir);
    if std::fs::create_dir_all(&folder).is_err() {
        return;
    }
    let target = file(dir, key);
    // A name of its own per writer: two runs that write at once do not mix their bytes.
    let temp = folder.join(format!("{key}.{}.tmp", std::process::id()));
    if std::fs::write(&temp, &bytes).is_err() || std::fs::rename(&temp, &target).is_err() {
        let _ = std::fs::remove_file(&temp);
        return;
    }
    prune(&folder);
}

fn prune(folder: &Path) {
    let Ok(entries) = std::fs::read_dir(folder) else {
        return;
    };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == "albrun"))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    files.sort();
    let drop = files.len().saturating_sub(KEPT);
    for (_, path) in files.into_iter().take(drop) {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run() -> BaselineRun {
        let engine = al_vm::Engine::new(al_ir::Program::default(), Vec::new());
        let mut session = al_vm::Session::new(engine);
        BaselineRun {
            baseline: session.capture_baseline(),
            unseeded: [7u32, 9].into_iter().collect(),
            filled: 3,
            notes: vec!["Company-Initialize.OnRun did not run to its end: why".into()],
            ran: 17,
            total: 18,
            restored: Vec::new(),
        }
    }

    fn same(a: &BaselineRun, b: &BaselineRun) -> bool {
        (&a.unseeded, a.filled, &a.notes, a.ran, a.total, a.baseline.to_bytes())
            == (&b.unseeded, b.filled, &b.notes, b.ran, b.total, b.baseline.to_bytes())
    }

    #[test]
    fn a_run_survives_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let r = run();
        write(dir.path(), "k1", &r);
        let back = read(dir.path(), "k1").expect("written and read");
        assert!(same(&r, &back));
        assert!(back.restored.is_empty());
    }

    #[test]
    fn a_key_with_no_file_reads_nothing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read(dir.path(), "none").is_none());
        write(dir.path(), "k1", &run());
        assert!(read(dir.path(), "other").is_none());
    }

    #[test]
    fn a_file_that_cannot_be_read_is_removed() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "k1", &run());
        let path = file(dir.path(), "k1");
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.truncate(bytes.len() - 3);
        std::fs::write(&path, bytes).unwrap();
        assert!(read(dir.path(), "k1").is_none());
        assert!(!path.exists(), "the damaged file is gone");
        std::fs::write(&path, b"nonsense").unwrap();
        assert!(read(dir.path(), "k1").is_none());
        assert!(!path.exists());
    }

    #[test]
    fn a_run_with_changes_to_restored_rows_is_not_kept() {
        let dir = tempfile::tempdir().unwrap();
        let mut r = run();
        r.restored.push(al_data::memory::RestoredChange {
            table: "T".into(),
            op: "Modify",
            same: false,
            fields: Vec::new(),
        });
        write(dir.path(), "k1", &r);
        assert!(read(dir.path(), "k1").is_none());
        assert!(!folder(dir.path()).join("k1.albrun").exists());
    }

    #[test]
    fn only_the_newest_files_are_kept() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..KEPT + 3 {
            write(dir.path(), &format!("k{i:02}"), &run());
            // File times have a coarse grain on some systems.
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let left: Vec<String> = std::fs::read_dir(folder(dir.path()))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(left.len(), KEPT, "{left:?}");
        assert!(
            read(dir.path(), &format!("k{:02}", KEPT + 2)).is_some(),
            "the newest stays"
        );
        assert!(read(dir.path(), "k00").is_none(), "the oldest goes");
        assert!(left.iter().all(|f| !f.ends_with(".tmp")), "no temporary file is left");
    }

    #[test]
    fn the_key_depends_on_every_part_and_on_its_name() {
        let k = |f: &dyn Fn(&mut KeyBuilder)| {
            let mut b = KeyBuilder::new();
            f(&mut b);
            b.finish()
        };
        let a = k(&|b| {
            b.text("build", "x").text("day", "1");
        });
        assert_eq!(
            a,
            k(&|b| {
                b.text("build", "x").text("day", "1");
            })
        );
        assert_ne!(
            a,
            k(&|b| {
                b.text("build", "y").text("day", "1");
            })
        );
        assert_ne!(
            a,
            k(&|b| {
                b.text("build", "x").text("day", "2");
            })
        );
        // The same bytes under another name, and the same text cut elsewhere, are other keys.
        assert_ne!(
            a,
            k(&|b| {
                b.text("build", "x").text("night", "1");
            })
        );
        assert_ne!(
            k(&|b| {
                b.text("a", "bc").text("d", "e");
            }),
            k(&|b| {
                b.text("a", "b").text("cd", "e");
            })
        );
        assert_eq!(a.len(), 64);
    }
}
