#![cfg(all(not(miri), feature = "dev-tools"))]
use reknot::*;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

// Unix fork/exec in another test can transiently duplicate a locked descriptor
// until exec closes it. Isolate parent setups; deliberate process races remain
// inside each test, including the competing-cleaner test below.
static PARENT_SETUP: std::sync::Mutex<()> = std::sync::Mutex::new(());
fn serial() -> std::sync::MutexGuard<'static, ()> {
    PARENT_SETUP.lock().unwrap_or_else(|e| e.into_inner())
}

fn fixture() -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("cache-lifecycle-tests");
    fs::create_dir_all(&root).unwrap();
    root.canonicalize().unwrap().join(format!(
        "{}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}
fn directory(path: &Path) {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path).unwrap();
}
fn options(root: &Path) -> CacheOptions {
    CacheOptions {
        root: root.to_path_buf(),
        apply: true,
        ..CacheOptions::default()
    }
}
fn build(root: &Path, name: &str, source: &str, flags: &[&str]) -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let path = root.join(development::library_file_name(name));
    let mut command = Command::new("rustc");
    command
        .arg(manifest.join("tests/fixtures").join(source))
        .args([
            "--edition=2021",
            "--crate-type=cdylib",
            "--crate-name",
            name,
            "-Awarnings",
        ])
        .arg("-o")
        .arg(&path);
    for flag in flags {
        command.args(["--cfg", flag]);
    }
    let result = command.output().unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    path
}
struct HostChild {
    child: Child,
}
impl HostChild {
    fn start(area: &Path, case: &str) -> Self {
        directory(area);
        let good = build(area, "cache_handler", "handler.rs", &[]);
        build(area, "cache_bad", "handler.rs", &["bad_abi"]);
        build(area, "cache_wrong", "handler.rs", &["wrong_service"]);
        let cache = area.join("cache");
        let ready = area.join("ready");
        if ready.exists() {
            fs::remove_file(&ready).unwrap();
        }
        let child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "cache_host_child", "--ignored", "--nocapture"])
            .env("REKNOT_CACHE_ROOT", &cache)
            .env("REKNOT_CACHE_TEST_LIBRARY", &good)
            .env("REKNOT_CACHE_TEST_CASE", case)
            .env("REKNOT_CACHE_TEST_READY", &ready)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut result = Self { child };
        let deadline = Instant::now() + Duration::from_secs(25);
        while !ready.exists() {
            assert!(
                result.child.try_wait().unwrap().is_none(),
                "child failed before ready: {:?}",
                result.finish_output()
            );
            assert!(Instant::now() < deadline, "child did not become ready");
            std::thread::sleep(Duration::from_millis(10));
        }
        result
    }
    fn finish_output(&mut self) -> String {
        let mut output = String::new();
        if let Some(mut stream) = self.child.stdout.take() {
            use std::io::Read;
            stream.read_to_string(&mut output).unwrap();
        }
        if let Some(mut stream) = self.child.stderr.take() {
            use std::io::Read;
            stream.read_to_string(&mut output).unwrap();
        }
        output
    }
    fn stop(&mut self, kill: bool) {
        if kill {
            self.child.kill().unwrap();
        } else {
            self.child
                .stdin
                .as_mut()
                .unwrap()
                .write_all(b"quit\n")
                .unwrap();
        }
        let status = self.child.wait().unwrap();
        if !kill {
            assert!(status.success(), "{}", self.finish_output());
        }
    }
}
impl Drop for HostChild {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

#[reload_handler(ServiceKey::new(7, 1))]
async fn static_handler(_: RequestContext) -> Result<(), ServiceError> {
    Ok(())
}

#[test]
#[ignore = "subprocess entry, invoked only by the parent tests"]
fn cache_host_child() {
    let Ok(library) = std::env::var("REKNOT_CACHE_TEST_LIBRARY") else {
        return;
    };
    let library = PathBuf::from(library);
    let area = library.parent().unwrap();
    let root = PathBuf::from(std::env::var_os("REKNOT_CACHE_ROOT").unwrap());
    let case = std::env::var("REKNOT_CACHE_TEST_CASE").unwrap();
    if case == "busy" {
        let error = unsafe { RequestRouter::load(&library, HostResources::new()) }
            .err()
            .unwrap();
        assert!(
            matches!(error,RouteError::Load(LoadError::Io(e)) if e.kind()==std::io::ErrorKind::WouldBlock)
        );
        fs::write(
            std::env::var_os("REKNOT_CACHE_TEST_READY").unwrap(),
            b"ready",
        )
        .unwrap();
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line).unwrap();
        return;
    }
    assert_eq!(retention_snapshot().tracked_native_images, 0);
    let budget = GenerationBudget::new(10).unwrap();
    let router = unsafe {
        RequestRouter::from_api_with_options(
            *hot_handler_api(),
            HostResources::new(),
            RouterOptions {
                generations: budget.clone(),
                ..RouterOptions::default()
            },
        )
    }
    .unwrap();
    drop(router);
    assert_eq!(budget.used(), 1);
    assert_eq!(retention_snapshot().tracked_native_images, 0);
    let mut router = unsafe { RequestRouter::load(&library, HostResources::new()) }.unwrap();
    let a = unsafe { router.prepare(&library) }.unwrap();
    let b = unsafe { router.prepare(&library) }.unwrap();
    assert!(
        unsafe { router.prepare(area.join(development::library_file_name("cache_bad"))) }.is_err()
    );
    assert!(
        unsafe { router.prepare(area.join(development::library_file_name("cache_wrong"))) }
            .is_err()
    );
    assert!(unsafe { router.prepare(area.join("missing.dll")) }.is_err());
    let invalid = area.join("invalid.bin");
    fs::write(&invalid, b"not native code").unwrap();
    assert!(unsafe { router.prepare(&invalid) }.is_err());
    let snapshot = retention_snapshot();
    assert_eq!(snapshot.tracked_native_images, 5);
    assert_eq!(
        snapshot
            .images
            .iter()
            .filter(|v| v.state == ImageState::Prepared)
            .count(),
        2
    );
    assert_eq!(
        snapshot
            .images
            .iter()
            .filter(|v| v.outcome == ImageOutcome::Rejected)
            .count(),
        2
    );
    assert_eq!(
        snapshot
            .images
            .iter()
            .map(|v| v.id)
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        5
    );
    assert!(snapshot.recorded_copy_bytes > 0);
    assert_eq!(snapshot.open_failures, 1);
    assert_eq!(snapshot.copy_failures, 1);
    router.activate(a).unwrap();
    drop(b);
    drop(router);
    let snapshot = retention_snapshot();
    assert_eq!(snapshot.tracked_native_images, 5);
    assert!(snapshot
        .images
        .iter()
        .all(|v| v.state == ImageState::Retired));
    assert_eq!(
        snapshot
            .images
            .iter()
            .filter(|v| v.outcome == ImageOutcome::Abandoned)
            .count(),
        1
    );
    let report = clean_cache(&options(&root)).unwrap();
    assert_eq!(report.entries.len(), 1);
    assert_eq!(report.entries[0].status, CacheStatus::InUse);
    assert_eq!(report.deleted_files, 0);
    assert_eq!(budget.used(), 1);
    if case == "inheritance" {
        // Deliberately outlives this host, with its own 20-second exit bound.
        #[allow(clippy::zombie_processes)]
        let _descendant = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "cache_descendant", "--ignored"])
            .env("REKNOT_CACHE_DESCENDANT_AREA", area)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !area.join("descendant-ready").exists() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    fs::write(
        std::env::var_os("REKNOT_CACHE_TEST_READY").unwrap(),
        b"ready",
    )
    .unwrap();
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line).unwrap();
}

#[test]
#[ignore = "child process used to test noninheritance of the host's lease handle"]
fn cache_descendant() {
    let Some(area) = std::env::var_os("REKNOT_CACHE_DESCENDANT_AREA") else {
        return;
    };
    let area = PathBuf::from(area);
    fs::write(area.join("descendant-ready"), []).unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while !area.join("descendant-stop").exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    fs::write(area.join("descendant-exited"), []).unwrap();
}

#[test]
fn namespace_contention_and_child_handle_noninheritance_are_cross_process() {
    let _serial = serial();
    let area = exited_area();
    let root = area.join("cache");
    let guard = OpenOptions::new()
        .read(true)
        .write(true)
        .open(root.join("managed-v1/namespace.lock"))
        .unwrap();
    guard.try_lock().unwrap();
    let mut busy = HostChild::start(&area, "busy");
    busy.stop(false);
    drop(guard);
    let other = fixture();
    let mut host = HostChild::start(&other, "inheritance");
    host.stop(false);
    assert!(!other.join("descendant-exited").exists());
    let result = clean_cache(&options(&other.join("cache"))).unwrap();
    fs::write(other.join("descendant-stop"), []).unwrap();
    assert_eq!(
        result.deleted_files, 5,
        "a spawned descendant must not inherit the lease"
    );
}

#[cfg(windows)]
#[test]
fn failed_file_delete_keeps_trash_retryable_and_reports_actual_progress() {
    let _serial = serial();
    use std::os::windows::fs::OpenOptionsExt;
    let area = exited_area();
    let root = area.join("cache");
    let opt = options(&root);
    let trash = synthetic(&root, 40, true);
    let blocked = trash.join("copies/0000000000000002.dll");
    fs::write(&blocked, b"locked").unwrap();
    let guard = OpenOptions::new()
        .read(true)
        .share_mode(1)
        .open(&blocked)
        .unwrap();
    let result = clean_cache(&opt).unwrap();
    assert_eq!(result.deleted_files, 1);
    assert!(result.partial);
    assert_eq!(result.entries[0].status, CacheStatus::PartialFailure);
    assert!(trash.join("session.meta").exists());
    drop(guard);
    let result = clean_cache(&opt).unwrap();
    assert_eq!(result.deleted_files, 1);
    assert!(!trash.exists());
}

#[cfg(unix)]
#[test]
fn shared_writable_cache_root_is_refused_without_changing_permissions() {
    let _serial = serial();
    use std::os::unix::fs::PermissionsExt;
    let area = fixture();
    directory(&area);
    fs::set_permissions(&area, fs::Permissions::from_mode(0o777)).unwrap();
    assert_eq!(
        clean_cache(&options(&area)).unwrap_err().kind(),
        std::io::ErrorKind::PermissionDenied
    );
    assert_eq!(
        fs::metadata(&area).unwrap().permissions().mode() & 0o777,
        0o777
    );
}

#[test]
fn real_dll_retention_and_live_normal_and_forced_exit_cleanup() {
    let _serial = serial();
    for kill in [false, true] {
        let area = fixture();
        let mut host = HostChild::start(&area, "lifecycle");
        let opt = options(&area.join("cache"));
        let before = inspect_cache(&opt).unwrap();
        assert_eq!(before.entries[0].status, CacheStatus::InUse);
        assert_eq!(before.files, 5);
        assert!(before.logical_bytes > 0);
        assert_eq!(clean_cache(&opt).unwrap().deleted_files, 0);
        host.stop(kill);
        let before = inspect_cache(&opt).unwrap();
        assert_eq!(before.entries[0].status, CacheStatus::Eligible);
        let mut dry = opt.clone();
        dry.apply = false;
        assert_eq!(clean_cache(&dry).unwrap().deleted_files, 0);
        assert!(before.entries[0].path.exists());
        let result = clean_cache(&opt).unwrap();
        assert_eq!(result.deleted_files, 5);
        assert_eq!(result.deleted_bytes, before.logical_bytes);
        assert_eq!(result.entries[0].status, CacheStatus::Deleted);
        assert!(!result.partial);
        assert!(inspect_cache(&opt).unwrap().entries.is_empty());
    }
}

fn synthetic(root: &Path, index: u32, trash: bool) -> PathBuf {
    // Establish the managed namespace using a real exited host first.
    let managed = root.join("managed-v1");
    let id = format!("s{:08x}-{:032x}-{:016x}", std::process::id(), index, 1);
    let path = managed
        .join(if trash { "trash" } else { "sessions" })
        .join(&id);
    directory(&path.join("copies"));
    fs::write(path.join("lease.lock"), []).unwrap();
    fs::write(
        path.join("session.meta"),
        format!(
            "flow-reloader-cache-v1\nid={id}\npid={}\ncreated=1\n",
            std::process::id()
        ),
    )
    .unwrap();
    fs::write(path.join("copies/0000000000000001.dll"), b"copy").unwrap();
    path
}
fn exited_area() -> PathBuf {
    let area = fixture();
    let mut host = HostChild::start(&area, "fixtures");
    host.stop(false);
    clean_cache(&options(&area.join("cache"))).unwrap();
    area
}

#[test]
fn corrupt_unknown_legacy_bounds_namespace_and_dry_run_revalidation() {
    let _serial = serial();
    let area = exited_area();
    let root = area.join("cache");
    let opt = options(&root);
    let good = synthetic(&root, 1, false);
    let unknown = synthetic(&root, 2, false);
    let corrupt = synthetic(&root, 3, false);
    let missing = synthetic(&root, 4, false);
    let oversized = synthetic(&root, 5, false);
    fs::write(unknown.join("keep.me"), b"user data").unwrap();
    fs::write(corrupt.join("session.meta"), b"unknown version").unwrap();
    fs::remove_file(missing.join("lease.lock")).unwrap();
    fs::write(oversized.join("session.meta"), vec![b'a'; 514]).unwrap();
    let legacy = root.join("12345");
    directory(&legacy);
    fs::write(legacy.join("old.dll"), b"legacy").unwrap();
    let before = inspect_cache(&opt).unwrap();
    assert!(before
        .entries
        .iter()
        .any(|v| v.path == good && v.status == CacheStatus::Eligible));
    // Change a formerly eligible entry after inspection; apply must not trust the plan.
    let lease = OpenOptions::new()
        .read(true)
        .write(true)
        .open(good.join("lease.lock"))
        .unwrap();
    lease.try_lock().unwrap();
    let result = clean_cache(&opt).unwrap();
    assert_eq!(result.deleted_files, 0);
    assert!(result
        .entries
        .iter()
        .any(|v| v.path == good && v.status == CacheStatus::InUse));
    for path in [&unknown, &corrupt, &missing, &oversized, &legacy] {
        assert!(path.exists());
    }
    drop(lease);
    let result = clean_cache(&opt).unwrap();
    assert_eq!(result.deleted_files, 1);
    let bounded = CacheOptions {
        max_entries: 1,
        ..opt.clone()
    };
    assert!(clean_cache(&bounded).unwrap().partial);
    let guard = OpenOptions::new()
        .read(true)
        .write(true)
        .open(root.join("managed-v1/namespace.lock"))
        .unwrap();
    guard.try_lock().unwrap();
    assert_eq!(
        clean_cache(&opt).unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    drop(guard);
    let absent = area.join("absent");
    assert!(inspect_cache(&options(&absent)).unwrap().entries.is_empty());
    assert!(!absent.exists());
}

#[test]
fn trash_resume_and_untrusted_links_never_follow_external_paths() {
    let _serial = serial();
    let area = exited_area();
    let root = area.join("cache");
    let opt = options(&root);
    let trash = synthetic(&root, 10, true);
    fs::remove_file(trash.join("copies/0000000000000001.dll")).unwrap();
    let outer = area.join("external");
    directory(&outer);
    fs::write(outer.join("sentinel"), b"keep").unwrap();
    let candidate = synthetic(&root, 11, false);
    fs::remove_file(candidate.join("copies/0000000000000001.dll")).unwrap();
    fs::remove_dir(candidate.join("copies")).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&outer, candidate.join("copies")).unwrap();
    #[cfg(windows)]
    {
        let result = Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(candidate.join("copies"))
            .arg(&outer)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
    let result = clean_cache(&opt).unwrap();
    assert!(result
        .entries
        .iter()
        .any(|v| v.status == CacheStatus::Deleted));
    assert!(!trash.exists());
    assert!(candidate.exists());
    assert_eq!(fs::read(outer.join("sentinel")).unwrap(), b"keep");
    // Refuse a linked cache root too.
    assert!(inspect_cache(&options(&candidate.join("copies"))).is_err());
}

#[test]
fn cli_is_read_only_by_default_and_apply_uses_owned_sessions() {
    let _serial = serial();
    let area = exited_area();
    let root = area.join("cache");
    let path = synthetic(&root, 20, false);
    let binary = env!("CARGO_BIN_EXE_reknot-cache");
    for mode in ["inspect", "clean"] {
        let output = Command::new(binary)
            .arg(mode)
            .arg("--root")
            .arg(&root)
            .output()
            .unwrap();
        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).contains("READ ONLY"));
        assert!(path.exists());
    }
    let output = Command::new(binary)
        .args(["clean", "--apply", "--root"])
        .arg(&root)
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(!path.exists());
    assert!(!Command::new(binary)
        .args(["inspect", "--apply"])
        .output()
        .unwrap()
        .status
        .success());
    assert!(!Command::new(binary)
        .args(["inspect", "--root", "relative-nonexistent-cache"])
        .output()
        .unwrap()
        .status
        .success());
}

#[cfg(windows)]
#[test]
fn unrelated_write_acl_is_refused_without_repairing_existing_root() {
    let _serial = serial();
    let area = fixture();
    directory(&area);
    let grant = Command::new("icacls")
        .arg(&area)
        .args(["/grant", "*S-1-1-0:(OI)(CI)M"])
        .output()
        .unwrap();
    assert!(grant.status.success());
    let result = clean_cache(&options(&area));
    let restore = Command::new("icacls")
        .arg(&area)
        .args(["/remove:g", "*S-1-1-0"])
        .output()
        .unwrap();
    assert!(restore.status.success());
    assert_eq!(
        result.unwrap_err().kind(),
        std::io::ErrorKind::PermissionDenied
    );
    assert!(!area.join("managed-v1").exists());
}

#[test]
fn renamed_cache_cli_accepts_legacy_environment_and_prefers_reknot_root() {
    let _serial = serial();
    let area = exited_area();
    let root = area.join("cache");
    let legacy_session = synthetic(&root, 60, false);
    let binary = env!("CARGO_BIN_EXE_reknot-cache");
    for new_name in [false, true] {
        let mut command = Command::new(binary);
        command.env_remove("REKNOT_CACHE_ROOT");
        if new_name {
            command
                .env("REKNOT_CACHE_ROOT", &root)
                .env("FLOW_RELOAD_CACHE_ROOT", "invalid-relative-cache");
        } else {
            command.env("FLOW_RELOAD_CACHE_ROOT", &root);
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains(&format!("READ ONLY: {}", root.display())));
        assert!(stdout.contains("Eligible"));
        assert!(legacy_session.exists());
    }
}

#[test]
fn competing_cleaner_processes_delete_each_copy_at_most_once() {
    let _serial = serial();
    let area = exited_area();
    let root = area.join("cache");
    let path = synthetic(&root, 50, false);
    for id in 2..=256 {
        fs::write(path.join("copies").join(format!("{id:016x}.dll")), b"copy").unwrap();
    }
    let spawn = || {
        Command::new(env!("CARGO_BIN_EXE_reknot-cache"))
            .args(["clean", "--apply", "--root"])
            .arg(&root)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    };
    let first = spawn();
    let second = spawn();
    let outputs = [
        first.wait_with_output().unwrap(),
        second.wait_with_output().unwrap(),
    ];
    let mut deleted = 0;
    for output in outputs {
        if output.status.success() {
            let stdout = String::from_utf8(output.stdout).unwrap();
            deleted += stdout
                .lines()
                .last()
                .unwrap()
                .split_whitespace()
                .find_map(|word| word.strip_prefix("deleted_files="))
                .unwrap()
                .parse::<u64>()
                .unwrap();
        } else {
            assert_eq!(output.status.code(), Some(1));
            assert!(String::from_utf8_lossy(&output.stderr).contains("busy"));
        }
    }
    assert_eq!(deleted, 256);
    assert!(!path.exists());
    assert!(inspect_cache(&options(&root)).unwrap().entries.is_empty());
}
