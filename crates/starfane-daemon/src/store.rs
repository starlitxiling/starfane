//! File-backed persistence: atomic JSON snapshot, JSONL event log, spool
//! draining and project-level artifact files.

use serde::Serialize;
use starfane_proto::{EmitRequest, NormalizedEvent};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone)]
pub struct Store {
    pub state_dir: PathBuf,
}

impl Store {
    pub fn new(state_dir: PathBuf) -> Self {
        Store { state_dir }
    }

    pub fn snapshot_path(&self) -> PathBuf {
        self.state_dir.join("state.json")
    }
    pub fn events_dir(&self) -> PathBuf {
        self.state_dir.join("events")
    }
    pub fn spool_dir(&self) -> PathBuf {
        self.state_dir.join("spool")
    }
    pub fn inputs_dir(&self) -> PathBuf {
        self.state_dir.join("inputs")
    }
    pub fn audit_path(&self) -> PathBuf {
        self.state_dir.join("audit.jsonl")
    }

    pub fn ensure_dirs(&self) -> io::Result<()> {
        fs::create_dir_all(&self.state_dir)?;
        fs::create_dir_all(self.events_dir())?;
        fs::create_dir_all(self.spool_dir())?;
        fs::create_dir_all(self.inputs_dir())?;
        Ok(())
    }

    /// Read the snapshot; `Ok(None)` when absent.
    pub fn load_snapshot<T: serde::de::DeserializeOwned>(&self) -> io::Result<Option<T>> {
        match fs::read_to_string(self.snapshot_path()) {
            Ok(text) => serde_json::from_str(&text)
                .map(Some)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub fn save_snapshot<T: Serialize>(&self, value: &T) -> io::Result<()> {
        let text = serde_json::to_string(value)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        atomic_write(&self.snapshot_path(), text.as_bytes())
    }

    /// Append to `events/YYYY-MM-DD.jsonl`.
    pub fn append_event(&self, ev: &NormalizedEvent) -> io::Result<()> {
        let path = self
            .events_dir()
            .join(format!("{}.jsonl", civil_date(ev.ts_ms)));
        append_json_line(&path, ev)
    }

    pub fn append_audit(&self, entry: &serde_json::Value) -> io::Result<()> {
        append_json_line(&self.audit_path(), entry)
    }

    /// Read and delete every spool file; malformed lines are reported in the
    /// returned error list (never silently dropped).
    pub fn drain_spool(&self) -> (Vec<EmitRequest>, Vec<String>) {
        let mut reqs = Vec::new();
        let mut errors = Vec::new();
        let entries = match fs::read_dir(self.spool_dir()) {
            Ok(e) => e,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return (reqs, errors),
            Err(e) => {
                errors.push(format!("read spool dir: {e}"));
                return (reqs, errors);
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                continue;
            }
            match File::open(&path) {
                Ok(f) => {
                    for (i, line) in BufReader::new(f).lines().enumerate() {
                        match line {
                            Ok(l) if l.trim().is_empty() => {}
                            Ok(l) => match serde_json::from_str::<EmitRequest>(&l) {
                                Ok(r) => reqs.push(r),
                                Err(e) => errors.push(format!("{}:{}: {e}", path.display(), i + 1)),
                            },
                            Err(e) => errors.push(format!("{}: {e}", path.display())),
                        }
                    }
                    if let Err(e) = fs::remove_file(&path) {
                        errors.push(format!("remove {}: {e}", path.display()));
                    }
                }
                Err(e) => errors.push(format!("open {}: {e}", path.display())),
            }
        }
        (reqs, errors)
    }
}

fn atomic_write_with_mode(path: &Path, bytes: &[u8], mode: Option<u32>) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!(
        "tmp-{}-{}",
        std::process::id(),
        TMP_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    {
        #[cfg(unix)]
        let mut f = if let Some(m) = mode {
            use std::os::unix::fs::OpenOptionsExt;
            OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(m)
                .open(&tmp)?
        } else {
            File::create(&tmp)?
        };
        #[cfg(not(unix))]
        let mut f = {
            let _ = mode;
            File::create(&tmp)?
        };
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)
}

/// tmp + rename in the same directory.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    atomic_write_with_mode(path, bytes, None)
}

pub fn append_json_line<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut line =
        serde_json::to_string(value).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    line.push('\n');
    let mut f = OpenOptions::new().create(true).append(true).open(path)?;
    f.write_all(line.as_bytes())
}

/// Write a file with mode 0600 from creation.
pub fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    atomic_write_with_mode(path, bytes, Some(0o600))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

/// `YYYY-MM-DD` (UTC) for a Unix-ms timestamp (Howard Hinnant's civil algorithm).
pub fn civil_date(ts_ms: u64) -> String {
    let days = (ts_ms / 86_400_000) as i64;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_dates() {
        assert_eq!(civil_date(0), "1970-01-01");
        assert_eq!(civil_date(951_782_400_000), "2000-02-29");
        assert_eq!(civil_date(1_763_596_800_000), "2025-11-20");
    }

    #[test]
    fn snapshot_and_spool_roundtrip() {
        let dir = std::env::temp_dir().join(format!(
            "starfane-store-{}",
            starfane_proto::id::new_id("T")
        ));
        let store = Store::new(dir.clone());
        store.ensure_dirs().unwrap();
        assert!(store
            .load_snapshot::<serde_json::Value>()
            .unwrap()
            .is_none());
        store.save_snapshot(&serde_json::json!({"a": 1})).unwrap();
        let v: serde_json::Value = store.load_snapshot().unwrap().unwrap();
        assert_eq!(v["a"], 1);
        let req = EmitRequest {
            v: 1,
            agent: starfane_proto::AgentKind::Codex,
            hook_event: Some("Stop".into()),
            payload: serde_json::json!({"session_id": "s"}),
            env: Default::default(),
            recv_ts_ms: 1,
        };
        let spool = store.spool_dir().join("1-1.jsonl");
        append_json_line(&spool, &req).unwrap();
        fs::write(store.spool_dir().join("2-2.jsonl"), "not json\n").unwrap();
        let (reqs, errs) = store.drain_spool();
        assert_eq!(reqs.len(), 1);
        assert_eq!(errs.len(), 1);
        assert!(fs::read_dir(store.spool_dir()).unwrap().next().is_none());
        fs::remove_dir_all(dir).unwrap();
    }
}
