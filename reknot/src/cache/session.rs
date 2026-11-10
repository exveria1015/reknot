use super::*;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

pub(super) struct Session {
    pub directory: PathBuf,
    _lease: File,
}
// Deliberately process-lifetime, including after all routers/runners disappear.
static SESSION: Mutex<Option<&'static Session>> = Mutex::new(None);
static NEXT_SESSION: AtomicU64 = AtomicU64::new(1);
pub(super) fn is_current(path: &Path) -> bool {
    SESSION
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_some_and(|s| s.directory == path)
}
pub(crate) fn directory() -> io::Result<PathBuf> {
    let mut slot = SESSION.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(s) = *slot {
        return Ok(s.directory.clone());
    }
    let root = CacheOptions::default().root;
    ensure_directory(&root)?;
    permissions::private(&root)?;
    let managed = root.join(MANAGED);
    ensure_directory(&managed)?;
    permissions::private(&managed)?;
    let namespace = open_control(&managed.join("namespace.lock"), true)?;
    lock(&namespace)?;
    for name in ["sessions", "trash"] {
        ensure_directory(&managed.join(name))?;
        permissions::private(&managed.join(name))?;
    }
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    for _ in 0..32 {
        let seq = NEXT_SESSION
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
            .map_err(|_| io::Error::other("session IDs exhausted"))?;
        let id = format!("s{:08x}-{timestamp:032x}-{seq:016x}", std::process::id());
        let directory = managed.join("sessions").join(&id);
        match new_directory(&directory) {
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
            Ok(()) => (),
        }
        let lease = open_control(&directory.join("lease.lock"), true)?;
        lock(&lease)?;
        new_directory(&directory.join("copies"))?;
        let mut meta = new_file(&directory.join("session.meta"))?;
        write!(
            meta,
            "{MAGIC}\nid={id}\npid={}\ncreated={timestamp}\n",
            std::process::id()
        )?;
        meta.sync_all()?;
        let session = Box::leak(Box::new(Session {
            directory,
            _lease: lease,
        }));
        let result = session.directory.clone();
        *slot = Some(session);
        return Ok(result);
    }
    Err(io::Error::other("session ID collisions"))
}
