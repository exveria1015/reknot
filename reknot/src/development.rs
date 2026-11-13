//! Opt-in source watching and Cargo build helpers for development hosts.
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Low-level watch for one crate's `src/`, `Cargo.toml` and `build.rs`.
/// Call [`source_changed`] regularly to detect edits and refresh registrations.
pub struct SourceWatcher {
    _watcher: RecommendedWatcher,
    root: PathBuf,
    changed: Arc<AtomicBool>,
    refresh: Arc<AtomicBool>,
}

pub fn library_file_name(package_name: &str) -> String {
    let base_name = package_name.replace('-', "_");
    if cfg!(target_os = "windows") {
        format!("{base_name}.dll")
    } else if cfg!(target_os = "macos") {
        format!("lib{base_name}.dylib")
    } else {
        format!("lib{base_name}.so")
    }
}

pub fn library_path_in(target_dir: &Path, package_name: &str) -> PathBuf {
    target_dir
        .join("debug")
        .join(library_file_name(package_name))
}

pub fn default_target_dir(manifest_dir: &Path) -> PathBuf {
    manifest_dir.join("target").join("hot-reload-build")
}

pub fn build_logic(
    package_name: &str,
    manifest_dir: &Path,
    target_dir: &Path,
) -> io::Result<std::process::ExitStatus> {
    Command::new("cargo")
        .arg("build")
        .arg("--lib")
        .arg("--package")
        .arg(package_name)
        .arg("--target-dir")
        .arg(target_dir)
        .arg("--jobs")
        .arg("1")
        .env("CARGO_INCREMENTAL", "0")
        .env("CARGO_PROFILE_DEV_INCREMENTAL", "false")
        .env("CARGO_BUILD_JOBS", "1")
        .current_dir(manifest_dir)
        .status()
}

pub fn start_source_watcher(manifest_dir: &Path) -> Option<SourceWatcher> {
    let root = match manifest_dir.canonicalize() {
        Ok(root) if root.is_dir() => watch_path(&root),
        result => {
            eprintln!(
                "Cannot watch source directory {}: {result:?}",
                manifest_dir.display()
            );
            return None;
        }
    };
    let changed = Arc::new(AtomicBool::new(false));
    let refresh = Arc::new(AtomicBool::new(false));
    match watch_source(&root, changed.clone(), refresh.clone()) {
        Ok(watcher) => Some(SourceWatcher {
            _watcher: watcher,
            root,
            changed,
            refresh,
        }),
        Err(error) => {
            eprintln!("Failed to start source watcher: {error}");
            None
        }
    }
}

fn watch_path(path: &Path) -> PathBuf {
    #[cfg(windows)]
    if let Some(path) = path.to_str() {
        if let Some(rest) = path.strip_prefix("\\\\?\\UNC\\") {
            return PathBuf::from(format!("\\\\{rest}"));
        }
        if let Some(rest) = path.strip_prefix("\\\\?\\") {
            return PathBuf::from(rest);
        }
    }
    path.to_path_buf()
}

fn watch_source(
    root: &Path,
    changed: Arc<AtomicBool>,
    refresh: Arc<AtomicBool>,
) -> notify::Result<RecommendedWatcher> {
    let source = root.join("src");
    let inputs = [root.join("Cargo.toml"), root.join("build.rs")];
    let event_source = source.clone();
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        let event = match event {
            Ok(event) => event,
            Err(error) => {
                eprintln!("Source watch failed: {error}; retrying registration on the next check");
                refresh.store(true, Ordering::Release);
                return;
            }
        };
        if !matches!(
            event.kind,
            EventKind::Create(_) | EventKind::Remove(_) | EventKind::Modify(_)
        ) {
            return;
        }
        for path in &event.paths {
            let path = watch_path(path);
            if path.components().any(|part| part.as_os_str() == ".git") {
                continue;
            }
            // A directory watch follows that instance. Its nonrecursive parent
            // watches survive removal and observe creation at the same path.
            let topology = matches!(
                event.kind,
                EventKind::Create(_)
                    | EventKind::Remove(_)
                    | EventKind::Modify(notify::event::ModifyKind::Name(_))
            ) && event_source.starts_with(&path);
            if topology {
                refresh.store(true, Ordering::Release);
            }
            let input = (inputs.contains(&path) || path.starts_with(&event_source))
                && !(matches!(event.kind, EventKind::Modify(kind)
                    if !matches!(kind, notify::event::ModifyKind::Name(_)))
                    && path.is_dir());
            if topology || input {
                changed.store(true, Ordering::Release);
            }
        }
    })?;
    // Directory watches also survive atomic replacement of Cargo.toml/build.rs.
    // Ancestors cover temporarily absent project directories. Unrelated sibling
    // changes and target/log writes are filtered before signalling a build.
    for directory in root.ancestors() {
        if directory.is_dir() {
            watcher.watch(directory, RecursiveMode::NonRecursive)?;
        }
    }
    if source.is_dir() {
        watcher.watch(&source, RecursiveMode::Recursive)?;
    }
    Ok(watcher)
}

/// Coalesce source events and rearm watches after directory replacement.
/// A successful refresh also reports a change to cover edits during registration.
pub fn source_changed(watcher: &mut Option<SourceWatcher>) -> bool {
    let Some(watcher) = watcher.as_mut() else {
        return false;
    };
    let mut refreshed = false;
    if watcher.refresh.swap(false, Ordering::AcqRel) {
        match watch_source(
            &watcher.root,
            watcher.changed.clone(),
            watcher.refresh.clone(),
        ) {
            Ok(new) => {
                watcher._watcher = new;
                // Force one build under the new registration, covering edits
                // made between directory creation and installing its watch.
                refreshed = true;
            }
            Err(error) => {
                watcher.refresh.store(true, Ordering::Release);
                eprintln!("Failed to refresh source watcher: {error}");
            }
        }
    }
    watcher.changed.swap(false, Ordering::AcqRel) || refreshed
}
