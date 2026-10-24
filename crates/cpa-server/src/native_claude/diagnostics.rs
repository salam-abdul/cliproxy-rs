use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc::{SyncSender, sync_channel},
    },
};

const RECENT: usize = 512;
const QUEUE: usize = 256;
const FILE_BYTES: u64 = 8 * 1024 * 1024;
const EVENT_BYTES: usize = 8192;

#[derive(Default)]
pub(crate) struct Diagnostics {
    recent: Mutex<VecDeque<Value>>,
    pub(super) lost: Arc<AtomicU64>,
}
impl Diagnostics {
    pub(super) fn record(&self, event: Value) {
        let mut recent = self.recent.lock().unwrap();
        if recent.len() == RECENT {
            recent.pop_front();
        }
        recent.push_back(event);
    }
    pub(crate) fn snapshot(&self) -> Value {
        json!({"events": self.recent.lock().unwrap().iter().rev().cloned().collect::<Vec<_>>(), "lostEvents": self.lost.load(Ordering::Relaxed), "capacity": RECENT})
    }
}
pub(super) struct Writer(SyncSender<Vec<u8>>);
impl Writer {
    pub(super) fn start(path: PathBuf, lost: Arc<AtomicU64>) -> std::io::Result<Self> {
        let (tx, rx) = sync_channel::<Vec<u8>>(QUEUE);
        std::thread::Builder::new()
            .name("native-events".into())
            .spawn(move || {
                while let Ok(bytes) = rx.recv() {
                    if append(&path, &bytes, FILE_BYTES).is_err() {
                        lost.fetch_add(1, Ordering::Relaxed);
                    }
                }
            })?;
        Ok(Self(tx))
    }
    pub(super) fn send(&self, event: &Value, lost: &AtomicU64) {
        let mut bytes = serde_json::to_vec(event).unwrap_or_default();
        bytes.push(b'\n');
        if bytes.len() > EVENT_BYTES || self.0.try_send(bytes).is_err() {
            lost.fetch_add(1, Ordering::Relaxed);
        }
    }
}
fn append(path: &Path, bytes: &[u8], limit: u64) -> std::io::Result<()> {
    if std::fs::metadata(path).is_ok_and(|m| m.len().saturating_add(bytes.len() as u64) > limit) {
        std::fs::rename(path, path.with_extension("jsonl.1"))?;
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.append(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    opts.open(path)?.write_all(bytes)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounded_snapshot_is_nondestructive() {
        let d = Diagnostics::default();
        for i in 0..600 {
            d.record(json!({"n":i}));
        }
        let a = d.snapshot();
        assert_eq!(a, d.snapshot());
        assert_eq!(a["events"].as_array().unwrap().len(), RECENT);
        assert_eq!(a["events"][0]["n"], 599);
    }
    #[test]
    fn rotation_preserves_only_one_previous_private_file() {
        let dir = std::env::temp_dir().join(format!("native-log-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("events.jsonl");
        for _ in 0..4 {
            append(&path, b"123456\n", 10).unwrap();
        }
        assert_eq!(std::fs::read(&path).unwrap(), b"123456\n");
        assert_eq!(std::fs::read(path.with_extension("jsonl.1")).unwrap(), b"123456\n");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 2);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn queue_pressure_never_blocks_inference() {
        let (tx, _rx) = sync_channel(1);
        let w = Writer(tx);
        let lost = AtomicU64::new(0);
        w.send(&json!({"type":"inference"}), &lost);
        w.send(&json!({"type":"inference"}), &lost);
        assert_eq!(lost.load(Ordering::Relaxed), 1);
    }
}
