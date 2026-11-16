use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc, Arc,
};

#[derive(Clone, Default, PartialEq, Eq)]
pub(crate) struct Inputs {
    pub(crate) files: HashSet<PathBuf>,
    pub(crate) directories: HashSet<PathBuf>,
    pub(crate) rust_directories: HashSet<PathBuf>,
    pub(crate) excluded: HashSet<PathBuf>,
}
impl Inputs {
    pub(crate) fn exclude_outputs(&mut self, outputs: &[PathBuf]) -> Result<(), String> {
        self.excluded.extend(outputs.iter().cloned());
        // Resolve inputs and outputs together: either side can use a different
        // spelling from the filesystem, including a previously absent root.
        for paths in [
            &mut self.files,
            &mut self.directories,
            &mut self.rust_directories,
            &mut self.excluded,
        ] {
            *paths = std::mem::take(paths)
                .into_iter()
                .map(|path| normalize(&path))
                .collect();
        }
        if outputs.iter().map(|path| normalize(path)).any(|output| {
            self.directories
                .iter()
                .any(|dir| output.starts_with(dir) || dir.starts_with(&output))
                || self
                    .rust_directories
                    .iter()
                    .any(|dir| dir.starts_with(&output))
        }) || self
            .files
            .iter()
            .any(|file| self.excluded.iter().any(|dir| file.starts_with(dir)))
        {
            return Err("Cargo output directory overlaps source inputs; move build output outside source directories".into());
        }
        Ok(())
    }

    pub(crate) fn excludes(&self, path: &Path) -> bool {
        self.excluded.iter().any(|dir| path.starts_with(dir))
            || path.components().any(|part| part.as_os_str() == ".git")
    }

    pub(crate) fn watch_roots(&self) -> impl Iterator<Item = &Path> {
        self.files
            .iter()
            .filter_map(|path| path.parent())
            .chain(self.directories.iter().map(PathBuf::as_path))
            .chain(self.rust_directories.iter().map(PathBuf::as_path))
    }

    pub(crate) fn needs_watch_refresh(&self, path: &Path) -> bool {
        !self.excludes(path) && self.watch_roots().any(|root| root.starts_with(path))
    }

    pub(crate) fn contains(&self, path: &Path) -> bool {
        // Explicit source inputs, never target/, cache copies, logs or metadata.
        if self.excludes(path) {
            return false;
        }
        self.files.contains(path)
            || self.directories.iter().any(|dir| path.starts_with(dir))
            || ((path.extension().is_none() || path.extension().is_some_and(|ext| ext == "rs"))
                && self
                    .rust_directories
                    .iter()
                    .any(|dir| path.starts_with(dir)))
    }
}
impl Inputs {
    pub(crate) fn is_logic_input(&self, path: &Path, logic: &Self) -> bool {
        logic.contains(path) && (!self.files.contains(path) || logic.files.contains(path))
    }
    #[cfg_attr(not(all(feature = "dev-tools", feature = "typed")), allow(dead_code))]
    pub(crate) fn watch(
        &self,
        revision: Arc<AtomicU64>,
        watch_refresh: Arc<AtomicBool>,
    ) -> Result<(RecommendedWatcher, mpsc::Receiver<String>), String> {
        self.watch_classified(
            revision,
            watch_refresh,
            Arc::new(AtomicBool::new(false)),
            Self::default(),
        )
    }
    pub(crate) fn watch_classified(
        &self,
        revision: Arc<AtomicU64>,
        watch_refresh: Arc<AtomicBool>,
        foundation_dirty: Arc<AtomicBool>,
        logic: Self,
    ) -> Result<(RecommendedWatcher, mpsc::Receiver<String>), String> {
        let inputs = self.clone();
        let (tx, rx) = mpsc::sync_channel(1);
        let mut watcher =
            notify::recommended_watcher(move |event: Result<notify::Event, notify::Error>| {
                match event {
                    Ok(event) if event.need_rescan() => {
                        watch_refresh.store(true, Ordering::Release);
                        foundation_dirty.store(true, Ordering::Release);
                        revision.fetch_add(1, Ordering::Release);
                        let _ = tx.try_send("watcher requested a full input rescan".into());
                    }
                    Ok(event)
                        if matches!(
                            event.kind,
                            EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_)
                        ) =>
                    {
                        // Ignore known output paths before canonicalize opens
                        // them on Windows. Cargo can be renaming a temporary
                        // output directory at the same time. Unmatched spelling
                        // still goes through normalization and the filters below.
                        let paths: Vec<_> = event
                            .paths
                            .iter()
                            .filter(|path| !inputs.excludes(path))
                            .map(|path| normalize(path))
                            .collect();
                        let topology_changed =
                            matches!(
                                event.kind,
                                EventKind::Create(_)
                                    | EventKind::Remove(_)
                                    | EventKind::Modify(notify::event::ModifyKind::Name(_))
                            ) && paths.iter().any(|path| inputs.needs_watch_refresh(path));
                        if topology_changed {
                            watch_refresh.store(true, Ordering::Release);
                        }
                        if topology_changed
                            || paths.iter().any(|path| {
                                // Windows also reports a parent's last-write
                                // change for edits under an excluded directory.
                                // Directory metadata alone is not a source edit.
                                inputs.contains(path)
                                    && !(matches!(event.kind, EventKind::Modify(kind)
                                        if !matches!(kind, notify::event::ModifyKind::Name(_)))
                                        && path.is_dir())
                            })
                        {
                            if topology_changed
                                || paths.iter().any(|p| {
                                    inputs.contains(p) && !inputs.is_logic_input(p, &logic)
                                })
                            {
                                foundation_dirty.store(true, Ordering::Release);
                            }
                            revision.fetch_add(1, Ordering::Release);
                        }
                    }
                    Err(error) => {
                        watch_refresh.store(true, Ordering::Release);
                        foundation_dirty.store(true, Ordering::Release);
                        revision.fetch_add(1, Ordering::Release);
                        let _ = tx.try_send(error.to_string());
                    }
                    _ => {}
                }
            })
            .map_err(|e| format!("cannot start source watcher: {e}"))?;
        // Keep nonrecursive ancestor watches so absent roots can be created and
        // removed/replaced roots can be registered again. Register recursive
        // roots last so overlapping watches cannot downgrade their coverage.
        let parents: HashSet<_> = self.watch_roots().flat_map(Path::ancestors).collect();
        for parent in parents {
            if parent.is_dir() {
                watcher
                    .watch(parent, RecursiveMode::NonRecursive)
                    .map_err(|e| format!("cannot watch {}: {e}", parent.display()))?;
            }
        }
        for directory in self.directories.union(&self.rust_directories) {
            if directory.is_dir() {
                watcher
                    .watch(directory, RecursiveMode::Recursive)
                    .map_err(|e| format!("cannot watch {}: {e}", directory.display()))?;
            }
        }
        Ok((watcher, rx))
    }
}
#[cfg(not(windows))]
pub(crate) fn normalize(path: &Path) -> PathBuf {
    path.to_path_buf()
}

#[cfg(windows)]
pub(crate) fn normalize(path: &Path) -> PathBuf {
    use std::path::{Component, Prefix};
    // Match Cargo and notify using filesystem spelling, without assuming every
    // Windows directory is case-insensitive. Missing/removed paths still need
    // filtering, so resolve the longest existing ancestor and keep the suffix.
    let path = path
        .ancestors()
        .find_map(|ancestor| {
            ancestor.canonicalize().ok().map(|mut resolved| {
                resolved.extend(path.strip_prefix(ancestor).unwrap().components());
                resolved
            })
        })
        .unwrap_or_else(|| path.to_path_buf());
    // Cargo needs ordinary drive/UNC paths. Keep OsStr components intact rather
    // than round-tripping filenames through a lossy Unicode conversion.
    let mut components = path.components();
    if let Some(Component::Prefix(prefix)) = components.next() {
        let root = match prefix.kind() {
            Prefix::VerbatimDisk(drive) => Some(PathBuf::from(format!("{}:", char::from(drive)))),
            Prefix::VerbatimUNC(server, share) => {
                let mut root = std::ffi::OsString::from(r"\\");
                root.push(server);
                root.push(r"\");
                root.push(share);
                Some(PathBuf::from(root))
            }
            _ => None,
        };
        if let Some(mut root) = root {
            root.extend(components);
            return root;
        }
    }
    path
}
