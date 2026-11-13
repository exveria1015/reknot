#![cfg(feature = "dev-tools")]
use reknot::development::{source_changed, start_source_watcher, SourceWatcher};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn fixture(with_source: bool) -> PathBuf {
    fixture_at(
        with_source,
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
    )
}
fn fixture_at(with_source: bool, timestamp: u128) -> PathBuf {
    static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);
    let root = loop {
        let sequence = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
        let candidate = std::env::temp_dir().join(format!(
            "reknot-source-watch-{}-{timestamp}-{sequence}",
            std::process::id()
        ));
        // Parallel tests can observe the same clock tick; never reuse an existing fixture.
        match fs::create_dir(&candidate) {
            Ok(()) => break candidate,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => panic!("cannot create fixture {}: {error}", candidate.display()),
        }
    };
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname='watch-probe'\nversion='0.0.0'\n",
    )
    .unwrap();
    if with_source {
        create_source(&root);
    }
    root
}

#[test]
fn simultaneous_fixture_timestamps_do_not_share_source_directories() {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let roots = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..5)
            .map(|_| scope.spawn(|| fixture_at(true, timestamp)))
            .collect();
        workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect::<std::collections::HashSet<_>>()
    });
    assert_eq!(roots.len(), 5);
}
fn create_source(root: &Path) {
    fs::create_dir(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn value() -> u8 { 1 }\n").unwrap();
}
fn watch(root: &Path) -> Option<SourceWatcher> {
    Some(start_source_watcher(root).expect("source watcher must start"))
}
fn settle(watcher: &mut Option<SourceWatcher>) {
    let start = Instant::now();
    let mut quiet = Instant::now();
    while quiet.elapsed() < Duration::from_millis(150) {
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "watcher never becomes quiet"
        );
        if source_changed(watcher) {
            quiet = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn expect_change(watcher: &mut Option<SourceWatcher>, reason: &str) {
    let start = Instant::now();
    while !source_changed(watcher) {
        assert!(start.elapsed() < Duration::from_secs(5), "missed {reason}");
        std::thread::sleep(Duration::from_millis(10));
    }
    settle(watcher);
}
fn save(root: &Path, watcher: &mut Option<SourceWatcher>) {
    // Each later save must be observed separately, after topology events settle.
    for value in [2, 3] {
        fs::write(
            root.join("src/lib.rs"),
            format!("pub fn value() -> u8 {{ {value} }}\n"),
        )
        .unwrap();
        expect_change(watcher, "later source save");
    }
}

#[test]
fn source_watcher_recovers_after_src_removal_and_recreation() {
    let root = fixture(true);
    let mut watcher = watch(&root);
    settle(&mut watcher);
    save(&root, &mut watcher);
    fs::remove_file(root.join("src/lib.rs")).unwrap();
    fs::remove_dir(root.join("src")).unwrap();
    expect_change(&mut watcher, "source removal");
    create_source(&root);
    expect_change(&mut watcher, "source recreation");
    save(&root, &mut watcher);
}

#[test]
fn source_watcher_recovers_after_project_directory_replacement() {
    let root = fixture(true);
    let mut watcher = watch(&root);
    settle(&mut watcher);
    // Windows can reject renaming a parent while a nested directory is open.
    // Remove only this fixture's known files and empty directories instead.
    fs::remove_file(root.join("src/lib.rs")).unwrap();
    fs::remove_dir(root.join("src")).unwrap();
    fs::remove_file(root.join("Cargo.toml")).unwrap();
    fs::remove_dir(&root).unwrap();
    expect_change(&mut watcher, "project removal");
    fs::create_dir(&root).unwrap();
    create_source(&root);
    expect_change(&mut watcher, "project recreation");
    save(&root, &mut watcher);
}

#[test]
fn source_watcher_registers_src_created_after_start() {
    let root = fixture(false);
    let mut watcher = watch(&root);
    settle(&mut watcher);
    create_source(&root);
    expect_change(&mut watcher, "initial source creation");
    save(&root, &mut watcher);
}

#[test]
fn source_watcher_follows_atomic_inputs_and_new_build_script() {
    let root = fixture(true);
    let mut watcher = watch(&root);
    settle(&mut watcher);
    for name in ["Cargo.toml", "build.rs"] {
        let file = root.join(name);
        if !file.exists() {
            fs::write(&file, "fn main() {}\n").unwrap();
            expect_change(&mut watcher, "new build script");
        }
        fs::write(root.join("replacement.tmp"), "// replacement\n").unwrap();
        fs::rename(&file, root.join(format!("{name}.old"))).unwrap();
        fs::rename(root.join("replacement.tmp"), &file).unwrap();
        expect_change(&mut watcher, "input replacement");
        fs::write(&file, "// later edit\n").unwrap();
        expect_change(&mut watcher, "later input save");
    }
}

#[test]
fn source_watcher_ignores_outputs_logs_git_and_unrelated_siblings() {
    let root = fixture(true);
    let mut watcher = watch(&root);
    settle(&mut watcher);
    for directory in ["target", ".git", "src/.git", "logs"] {
        fs::create_dir_all(root.join(directory)).unwrap();
        fs::write(root.join(directory).join("output"), "noise").unwrap();
    }
    fs::write(root.join("output.log"), "noise").unwrap();
    fs::write(root.with_extension("unrelated"), "noise").unwrap();
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(500) {
        assert!(
            !source_changed(&mut watcher),
            "unrelated output must not trigger rebuilds"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    save(&root, &mut watcher);
}
