//! Native images stay loaded unless explicitly reclaimed on their owner thread.
use crate::generation::{CodeLease, GenerationOwner};
use crate::{ArtifactId, CopyRemovalError, ImageOutcome, ImageRecord, ImageState, RetentionReason};
use crate::{LoadedHandler, RouteError};
use libloading::Library;
use reknot_abi::services::HandlerApiFn;
use std::fmt;
#[cfg(all(feature = "dev-tools", feature = "typed"))]
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, ThreadId};

#[derive(Debug)]
pub enum LoadError {
    Io(std::io::Error),
    LibLoading(libloading::Error),
}
impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "io error: {error}"),
            Self::LibLoading(error) => write!(f, "library load error: {error}"),
        }
    }
}
impl std::error::Error for LoadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::LibLoading(error) => Some(error),
        }
    }
}
impl From<std::io::Error> for LoadError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}
impl From<libloading::Error> for LoadError {
    fn from(error: libloading::Error) -> Self {
        Self::LibLoading(error)
    }
}

// Keep an owning reference from the moment Library::new succeeds. A panic or
// failed validation must never unload callbacks that may have escaped loading.
// Neither a lease's Drop nor a notification can close a library. Static storage
// deliberately has no destructor at process shutdown.
static IMAGES: Mutex<Vec<Arc<LibraryImage>>> = Mutex::new(Vec::new());
struct LibraryImage {
    library: Library,
    copy: LibraryCopy,
    executions: Arc<AtomicUsize>,
    owner: ThreadId,
}

struct LibraryCopy {
    path: PathBuf,
    id: crate::ArtifactId,
    remove_on_drop: bool,
}

/// An unloaded, private copy. Sending it transfers only filesystem ownership;
/// no DLL entry point, symbol lookup or validation callback has run yet.
#[cfg(all(feature = "dev-tools", feature = "typed"))]
pub(crate) struct PreparedLibrary(LibraryCopy);

#[cfg(all(feature = "dev-tools", feature = "typed"))]
pub(crate) fn prepare_library(path: &Path) -> Result<PreparedLibrary, LoadError> {
    let copy = copy_library(path)?;
    // Closing the writer and reopening the copy may wait for filesystem work.
    // Do this on the build worker, and verify content before announcing readiness.
    if let Err(error) = verify_copy(path, &copy.path) {
        crate::retention::copy_failed();
        return Err(error.into()); // Drop removes this never-loaded copy.
    }
    Ok(PreparedLibrary(copy))
}

#[cfg(all(feature = "dev-tools", feature = "typed"))]
fn verify_copy(source: &Path, destination: &Path) -> std::io::Result<()> {
    let mut source = std::fs::File::open(source)?;
    let mut destination = std::fs::File::open(destination)?;
    let mut remaining = source.metadata()?.len();
    let mismatch = || {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "native library copy differs from the build artifact",
        )
    };
    if remaining != destination.metadata()?.len() {
        return Err(mismatch());
    }
    let mut left = [0u8; 64 * 1024];
    let mut right = [0u8; 64 * 1024];
    while remaining != 0 {
        let count = remaining.min(left.len() as u64) as usize;
        source.read_exact(&mut left[..count])?;
        destination.read_exact(&mut right[..count])?;
        if left[..count] != right[..count] {
            return Err(mismatch());
        }
        remaining -= count as u64;
    }
    if source.read(&mut left[..1])? != 0 || destination.read(&mut right[..1])? != 0 {
        return Err(mismatch());
    }
    Ok(())
}

impl LibraryCopy {
    fn remove(&mut self) -> Option<CopyRemovalError> {
        self.remove_on_drop = false;
        let error = std::fs::remove_file(&self.path)
            .err()
            .filter(|e| e.kind() != std::io::ErrorKind::NotFound)
            .map(|e| crate::CopyRemovalError {
                id: self.id,
                path: self.path.clone(),
                message: e.to_string(),
            });
        crate::retention::removed(self.id, error.clone());
        error
    }
}
impl Drop for LibraryCopy {
    fn drop(&mut self) {
        if self.remove_on_drop {
            self.remove();
        }
    }
}

fn copy_library(source_path: &Path) -> Result<LibraryCopy, LoadError> {
    let result = (|| {
        let directory = crate::cache::session_directory()?.join("copies");
        let id = crate::retention::next_id()?;
        let ext = source_path
            .extension()
            .and_then(|s| s.to_str())
            .filter(|s| matches!(*s, "dll" | "so" | "dylib"))
            .unwrap_or("bin");
        let path = directory.join(format!("{:016x}.{ext}", id.0));
        let mut source = std::fs::File::open(source_path)?;
        let mut dest = crate::cache::new_file(&path)?;
        crate::retention::copied(id, source_path.to_path_buf(), path.clone(), 0);
        let copy = LibraryCopy {
            path,
            id,
            remove_on_drop: true,
        };
        let bytes = std::io::copy(&mut source, &mut dest)?;
        crate::retention::update(id, |v| v.copy_bytes = bytes);
        drop(dest);
        Ok(copy)
    })();
    if result.is_err() {
        crate::retention::copy_failed();
    }
    result
}
pub(crate) unsafe fn load(path: &Path) -> Result<LoadedHandler, RouteError> {
    let copy = copy_library(path).map_err(RouteError::Load)?;
    load_copy(copy)
}

#[cfg(all(feature = "dev-tools", feature = "typed"))]
pub(crate) unsafe fn load_prepared(prepared: PreparedLibrary) -> Result<LoadedHandler, RouteError> {
    load_copy(prepared.0)
}

unsafe fn load_copy(copy: LibraryCopy) -> Result<LoadedHandler, RouteError> {
    let library = open_library(&copy.path)
        .inspect_err(|_| {
            crate::retention::open_failed();
        })
        .map_err(RouteError::Load)?;
    crate::retention::update(copy.id, |v| v.loaded = true);
    let id = copy.id;
    let library = Arc::new(LibraryImage {
        library,
        copy,
        executions: Arc::new(AtomicUsize::new(0)),
        owner: thread::current().id(),
    });
    IMAGES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(library.clone());
    let image = GenerationOwner {
        code: CodeLease::new(library.clone(), library.executions.clone()).with_native_id(id),
        id: Some(id),
    };
    let entry = library
        .library
        .get::<HandlerApiFn>(b"hot_handler_api")
        .map_err(|error| RouteError::Load(LoadError::LibLoading(error)))?;
    LoadedHandler::from_entry(*entry, image)
}

unsafe fn open_library(path: &Path) -> Result<Library, LoadError> {
    #[cfg(windows)]
    {
        use libloading::os::windows::{
            Library as WindowsLibrary, LOAD_LIBRARY_SEARCH_DEFAULT_DIRS,
            LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR,
        };
        // The copied image, executable directory, System32 and explicitly
        // registered loader directories are available for its dependencies.
        let path = path.canonicalize()?;
        let library = WindowsLibrary::load_with_flags(
            path,
            LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_DEFAULT_DIRS,
        )?;
        Ok(library.into())
    }
    #[cfg(not(windows))]
    Ok(Library::new(path)?)
}

/// Outcome of one explicit reclamation attempt. Copy deletion and native close
/// are separate operations; a deletion failure never changes a successful close.
#[derive(Clone, Debug)]
pub enum ReclaimResult {
    Released {
        copy_removal_error: Option<CopyRemovalError>,
    },
    Retained {
        reasons: Vec<RetentionReason>,
    },
    /// The consumed loader handle is never reused or closed a second time.
    CloseFailed {
        message: String,
    },
    NotFound,
}

fn reasons(record: &ImageRecord, image: Option<&Arc<LibraryImage>>) -> Vec<RetentionReason> {
    let mut reasons = Vec::new();
    if record.registered_locals != 0 {
        reasons.push(RetentionReason::RegisteredLocals {
            count: record.registered_locals,
        });
    }
    if record.local_cleanup_failures != 0 {
        reasons.push(RetentionReason::LocalCleanupFailed {
            count: record.local_cleanup_failures,
        });
    }
    if let Some(error) = record.task_cleanup_failure {
        reasons.push(RetentionReason::TaskCleanupFailed(error));
    }
    if record.reclaiming {
        reasons.push(RetentionReason::Reclaiming);
    }
    if record.close_error.is_some() {
        reasons.push(RetentionReason::CloseUncertain);
    }
    if record.state != ImageState::Retired {
        reasons.push(RetentionReason::NotRetired);
    }
    if matches!(
        record.outcome,
        ImageOutcome::Pending | ImageOutcome::Rejected
    ) {
        reasons.push(RetentionReason::UnvalidatedCandidate);
    }
    if let Some(image) = image {
        let count = Arc::strong_count(image).saturating_sub(1);
        if count != 0 {
            reasons.push(RetentionReason::CodeLeases { count });
        }
        let count = image.executions.load(Ordering::Acquire);
        if count != 0 {
            reasons.push(RetentionReason::HostCalls { count });
        }
        if image.owner != thread::current().id() {
            reasons.push(RetentionReason::OwnerThreadRequired);
        }
    }
    reasons
}

pub(crate) fn annotate(records: &mut [ImageRecord]) {
    let images = IMAGES.lock().unwrap_or_else(|e| e.into_inner());
    for record in records {
        let image = images.iter().find(|image| image.copy.id == record.id);
        if let Some(image) = image {
            record.code_leases = Arc::strong_count(image).saturating_sub(1);
            record.active_calls = image.executions.load(Ordering::Acquire);
        }
        record.retention_reasons = reasons(record, image);
        record
            .retention_reasons
            .push(RetentionReason::ExternalQuiescenceUnconfirmed);
    }
}

/// Releases one retired native image's loader reference, then removes its copy.
/// The default is to keep every image loaded; no Drop or polling path calls this.
/// Call on the loading thread after releasing completed retired generations and
/// dropping their tasks/candidates. Rejected candidates remain retained.
///
/// # Safety
/// The caller must establish, for this specific image and all its dependencies,
/// that external execution has returned and cannot resume. This includes the
/// *entire* DLL-side wake/drop/clone tail of every escaped Waker: the last SDK
/// lease disappearing is not proof. Join external threads or use an equivalent
/// return/unregistration barrier before calling. All foreign pointers, callbacks,
/// TLS destructors, OS registrations and dependency finalizers must be finished
/// or otherwise safe to unload on this thread; finalizers must not start new work.
/// No SDK counter, timeout or subsequent poll establishes these conditions.
/// Unsupported/unknown TLS or external SDK behavior must keep the image loaded.
pub unsafe fn reclaim_native_image(id: ArtifactId) -> ReclaimResult {
    let image = {
        let mut images = IMAGES.lock().unwrap_or_else(|e| e.into_inner());
        let Some(record) = crate::retention::record(id) else {
            return ReclaimResult::NotFound;
        };
        let index = images.iter().position(|image| image.copy.id == id);
        let blockers = reasons(&record, index.map(|index| &images[index]));
        if !blockers.is_empty() {
            return ReclaimResult::Retained { reasons: blockers };
        }
        let Some(index) = index else {
            return ReclaimResult::NotFound;
        };
        // With only the retention owner left, no safe caller can create a new
        // lease. Recheck/extract under the same lock; never close under that lock.
        crate::retention::update(id, |record| record.reclaiming = true);
        images.swap_remove(index)
    };
    let image = match Arc::try_unwrap(image) {
        Ok(image) => image,
        Err(image) => {
            let count = Arc::strong_count(&image).saturating_sub(1);
            let mut images = IMAGES.lock().unwrap_or_else(|e| e.into_inner());
            images.push(image);
            crate::retention::update(id, |record| record.reclaiming = false);
            return ReclaimResult::Retained {
                reasons: vec![RetentionReason::CodeLeases { count }],
            };
        }
    };
    let LibraryImage { library, copy, .. } = image;
    finish_reclamation(copy, library.close().map_err(|error| error.to_string()))
}

fn finish_reclamation(mut copy: LibraryCopy, result: Result<(), String>) -> ReclaimResult {
    match result {
        Ok(()) => {
            crate::retention::released();
            ReclaimResult::Released {
                copy_removal_error: copy.remove(),
            }
        }
        Err(message) => {
            // close consumes its handle even on failure. Preserve the copy and
            // diagnostic record; claiming a free slot or retrying is unsound.
            copy.remove_on_drop = false;
            crate::retention::close_failed(copy.id, message.clone());
            ReclaimResult::CloseFailed { message }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(all(feature = "dev-tools", feature = "typed", not(miri)))]
    fn prepared_copy_crosses_threads_without_loading_and_abandonment_removes_it() {
        let id = crate::retention::next_id().unwrap();
        let source = std::env::temp_dir().join(format!(
            "reknot-copy-source-{}-{}.bin",
            std::process::id(),
            id.0
        ));
        let bytes: Vec<u8> = (0..150_001).map(|i| i as u8).collect();
        std::fs::write(&source, &bytes).unwrap();
        let worker_source = source.clone();
        // Deliberately not a library: preparation must never try to load it.
        let prepared = std::thread::spawn(move || prepare_library(&worker_source).unwrap())
            .join()
            .unwrap();
        let path = prepared.0.path.clone();
        let copy_id = prepared.0.id;
        assert!(!crate::retention::record(copy_id).unwrap().loaded);
        std::fs::write(&source, b"newer build").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        drop(prepared); // Same cleanup path as a superseded/undeliverable output.
        assert!(!path.exists());
        assert!(crate::retention::record(copy_id).is_none());
        std::fs::remove_file(source).unwrap();
    }

    #[test]
    #[cfg(all(feature = "dev-tools", feature = "typed", not(miri)))]
    fn copy_verification_rejects_content_and_length_mismatches() {
        let id = crate::retention::next_id().unwrap();
        let source = std::env::temp_dir().join(format!(
            "reknot-copy-check-{}-{}.bin",
            std::process::id(),
            id.0
        ));
        let bytes = vec![23u8; 70_003];
        std::fs::write(&source, &bytes).unwrap();
        let copy = copy_library(&source).unwrap();
        verify_copy(&source, &copy.path).unwrap();
        let mut changed = bytes.clone();
        changed[69_999] ^= 1;
        std::fs::write(&copy.path, &changed).unwrap();
        assert_eq!(
            verify_copy(&source, &copy.path).unwrap_err().kind(),
            std::io::ErrorKind::InvalidData
        );
        std::fs::write(&copy.path, &bytes[..100]).unwrap();
        assert_eq!(
            verify_copy(&source, &copy.path).unwrap_err().kind(),
            std::io::ErrorKind::InvalidData
        );
        std::fs::remove_file(&source).unwrap();
        assert!(verify_copy(&source, &copy.path).is_err());
        let path = copy.path.clone();
        drop(copy);
        assert!(!path.exists());
    }

    #[test]
    fn failed_close_preserves_the_copy_and_uncertain_record_without_retrying_handle() {
        let id = crate::retention::next_id().unwrap();
        let path = std::env::temp_dir().join(format!(
            "reknot-close-failed-{}-{}.bin",
            std::process::id(),
            id.0
        ));
        std::fs::write(&path, b"close failure injection").unwrap();
        crate::retention::copied(id, path.clone(), path.clone(), 0);
        crate::retention::update(id, |record| {
            record.loaded = true;
            record.state = ImageState::Retired;
            record.outcome = ImageOutcome::Adopted;
            record.reclaiming = true;
        });
        // Inject only the close result; no invalid OS handle is created/closed.
        let result = finish_reclamation(
            LibraryCopy {
                path: path.clone(),
                id,
                remove_on_drop: true,
            },
            Err("injected close failure".into()),
        );
        assert!(matches!(result, ReclaimResult::CloseFailed { .. }));
        assert!(path.exists());
        let record = crate::retention::record(id).unwrap();
        assert!(record.loaded);
        assert_eq!(
            record.close_error.as_deref(),
            Some("injected close failure")
        );
        assert_eq!(reasons(&record, None), [RetentionReason::CloseUncertain]);
        assert!(
            matches!(unsafe { reclaim_native_image(id) }, ReclaimResult::Retained { reasons } if reasons == [RetentionReason::CloseUncertain])
        );
        std::fs::remove_file(path).unwrap();
        crate::retention::removed(id, None);
    }
}
