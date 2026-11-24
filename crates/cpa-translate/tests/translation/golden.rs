//! Golden files: compared byte for byte, or rewritten when `UPDATE_GOLDEN` is set.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

pub fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/translation")
}

pub fn updating() -> bool {
    std::env::var_os("UPDATE_GOLDEN").is_some()
}

/// Compares `actual` with the golden at `rel` (under `golden/`), or writes it when
/// updating. Records the path as produced.
pub struct Goldens {
    pub produced: BTreeSet<PathBuf>,
    pub failures: Vec<String>,
}

impl Goldens {
    pub fn new() -> Self {
        Self {
            produced: BTreeSet::new(),
            failures: vec![],
        }
    }

    pub fn check(&mut self, rel: &str, actual: &[u8]) {
        let path = root().join("golden").join(rel);
        self.produced.insert(path.clone());
        let expected = std::fs::read(&path).ok();
        if expected.as_deref() == Some(actual) {
            return;
        }
        if updating() {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, actual).unwrap();
            return;
        }
        match expected {
            None => self
                .failures
                .push(format!("{rel}: no golden (run with UPDATE_GOLDEN=1)")),
            Some(expected) => self.failures.push(format!(
                "{rel}: differs from its golden\n--- golden\n{}\n--- actual\n{}",
                String::from_utf8_lossy(&expected),
                String::from_utf8_lossy(actual)
            )),
        }
    }

    /// Golden files no case produced: removed when updating, else failures.
    pub fn orphans(&mut self) {
        let mut stack = vec![root().join("golden")];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else { continue };
            for entry in entries {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    stack.push(path);
                } else if !self.produced.contains(&path) {
                    if updating() {
                        std::fs::remove_file(&path).unwrap();
                    } else {
                        self.failures
                            .push(format!("{}: no case produces this golden", path.display()));
                    }
                }
            }
            if updating() && std::fs::read_dir(&dir).is_ok_and(|mut d| d.next().is_none()) {
                let _ = std::fs::remove_dir(&dir);
            }
        }
    }
}
