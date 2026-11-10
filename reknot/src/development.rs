//! Opt-in source watching and Cargo build helpers for development hosts.
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{self, Receiver};

pub struct SourceWatcher {
    _watcher: RecommendedWatcher,
    rx: Receiver<()>,
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
    let src_dir = manifest_dir.join("src");
    let cargo_manifest = manifest_dir.join("Cargo.toml");
    let build_script = manifest_dir.join("build.rs");
    let (tx, rx) = mpsc::channel();
    let mut watcher =
        match notify::recommended_watcher(move |event: Result<notify::Event, notify::Error>| {
            let Ok(event) = event else {
                return;
            };
            if matches!(
                event.kind,
                EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_)
            ) {
                let _ = tx.send(());
            }
        }) {
            Ok(watcher) => watcher,
            Err(err) => {
                eprintln!("Failed to start source watcher: {err}");
                return None;
            }
        };

    let mut watched_any = false;

    if cargo_manifest.exists() {
        match watcher.watch(&cargo_manifest, RecursiveMode::NonRecursive) {
            Ok(()) => watched_any = true,
            Err(err) => {
                eprintln!("Failed to watch {}: {err}", cargo_manifest.display());
            }
        }
    }

    if build_script.exists() {
        match watcher.watch(&build_script, RecursiveMode::NonRecursive) {
            Ok(()) => watched_any = true,
            Err(err) => {
                eprintln!("Failed to watch {}: {err}", build_script.display());
            }
        }
    }

    if src_dir.exists() {
        match watcher.watch(&src_dir, RecursiveMode::Recursive) {
            Ok(()) => watched_any = true,
            Err(err) => {
                eprintln!("Failed to watch {}: {err}", src_dir.display());
            }
        }
    }

    if !watched_any {
        eprintln!(
            "Failed to start source watcher: no watchable paths in {}",
            manifest_dir.display()
        );
        return None;
    }

    Some(SourceWatcher {
        _watcher: watcher,
        rx,
    })
}

pub fn source_changed(watcher: &mut Option<SourceWatcher>) -> bool {
    let Some(watcher) = watcher.as_mut() else {
        return false;
    };
    let mut triggered = false;
    while watcher.rx.try_recv().is_ok() {
        triggered = true;
    }
    triggered
}
