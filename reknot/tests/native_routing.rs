#![cfg(all(not(miri), feature = "dev-tools"))]
use reknot::{
    HostResource, HostResources, RequestRouter, RequestTask, RouteError, RunStatus, ServiceError,
    ServiceKey,
};
use std::future::Future;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Command;
use std::task::{Context, Poll, Waker};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Default)]
struct CallbackSession {
    release: bool,
    events: Vec<Vec<u8>>,
}
impl HostResource for CallbackSession {
    fn call(
        &mut self,
        operation: u32,
        input: &[u8],
        output: &mut [u8],
    ) -> Result<usize, ServiceError> {
        if operation == 1 {
            output[0] = u8::from(self.release);
            Ok(1)
        } else {
            if self.events.last().map(Vec::as_slice) != Some(input) {
                self.events.push(input.to_vec());
            }
            Ok(0)
        }
    }
}
fn has_event(session: &reknot::Resource<CallbackSession>, event: &[u8]) -> bool {
    session
        .try_borrow()
        .unwrap()
        .events
        .iter()
        .any(|value| value == event)
}
fn drive_until(mut condition: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !condition() {
        assert!(
            std::time::Instant::now() < deadline,
            "native callback timeout"
        );
        std::thread::yield_now();
    }
}

#[test]
fn native_callback_state_outlives_root_and_router_until_external_return_and_join() {
    let fixture = Fixture::new();
    let library = fixture.build("callback_join", &["managed_activity"]);
    let resources = HostResources::new();
    let session = resources.register(ServiceKey::new(7, 1), CallbackSession::default());
    let mut router = unsafe { RequestRouter::load(&library, resources) }.unwrap();
    let mut task = router.dispatch(session.id(), b"callback").unwrap();
    let mut cx = Context::from_waker(Waker::noop());
    drive_until(|| {
        assert!(Pin::new(&mut task).poll(&mut cx).is_pending());
        has_event(&session, b"callback_active")
    });
    assert_eq!(task.status(), None);
    let candidate = unsafe { router.prepare(&library) }.unwrap();
    router.activate(candidate).unwrap();
    drop(task);
    assert_eq!(router.in_flight(), 1);
    let candidate = unsafe { router.prepare(&library) }.unwrap();
    let mut restart = router
        .restart(candidate, |_, old, _| Ok(old.clone()))
        .unwrap();
    assert!(Pin::new(&mut restart).poll(&mut cx).is_pending());
    drop(restart);
    assert!(!has_event(&session, b"callback_joined"));
    assert!(!has_event(&session, b"activity_drop"));
    drop(router);
    session.try_borrow_mut().unwrap().release = true;
    drive_until(|| reknot::poll_cancelled_requests(&mut cx).is_ready());
    let events = &session.try_borrow().unwrap().events;
    let joined = events
        .iter()
        .position(|event| event == b"callback_joined")
        .unwrap();
    let dropped = events
        .iter()
        .position(|event| event == b"activity_drop")
        .unwrap();
    assert!(joined < dropped);
}

#[test]
fn native_failed_unsubscribe_does_not_certify_quiescence_or_allow_restart() {
    let fixture = Fixture::new();
    let library = fixture.build(
        "callback_failure",
        &["managed_activity", "activity_stop_fails"],
    );
    let resources = HostResources::new();
    let session = resources.register(ServiceKey::new(7, 1), CallbackSession::default());
    let mut router = unsafe { RequestRouter::load(&library, resources) }.unwrap();
    let mut task = router.dispatch(session.id(), b"callback").unwrap();
    let mut cx = Context::from_waker(Waker::noop());
    assert!(Pin::new(&mut task).poll(&mut cx).is_pending());
    assert!(Pin::new(&mut task).poll(&mut cx).is_pending());
    assert_eq!(task.activity_error(), Some(ServiceError::FAILED));
    assert_eq!(task.activity_count(), 1);
    drop(task);
    let candidate = unsafe { router.prepare(&library) }.unwrap();
    let mut restart = router
        .restart(candidate, |_, old, _| Ok(old.clone()))
        .unwrap();
    assert!(Pin::new(&mut restart).poll(&mut cx).is_pending());
    assert!(!has_event(&session, b"activity_drop"));
    session.try_borrow_mut().unwrap().release = true;
    drive_until(|| match Pin::new(&mut restart).poll(&mut cx) {
        Poll::Pending => false,
        Poll::Ready(result) => {
            assert!(matches!(
                result,
                Err(RouteError::ActivityFailed { generation: 1 })
            ));
            true
        }
    });
    drop(restart);
    assert_eq!(router.generation(), 1);
    assert_eq!(router.in_flight(), 0);
    assert!(!router.is_accepting());
    assert!(has_event(&session, b"callback_joined"));
    assert!(has_event(&session, b"activity_drop"));
}

#[test]
fn legacy_handler_abi_cannot_silently_skip_registered_activity_contract() {
    let fixture = Fixture::new();
    let library = fixture.build("legacy_handler", &["legacy_handler_abi"]);
    assert!(matches!(
        unsafe { RequestRouter::load(library, HostResources::new()) },
        Err(RouteError::Abi { version: 2, .. })
    ));
}

#[test]
fn previous_handler_abi_is_rejected_before_accessing_services_extension() {
    let fixture = Fixture::new();
    let library = fixture.build("previous_handler", &["previous_handler_abi"]);
    assert!(matches!(
        unsafe { RequestRouter::load(library, HostResources::new()) },
        Err(RouteError::Abi { version: 3, .. })
    ));
}

#[test]
fn version_four_header_is_rejected_without_reading_extended_manifest() {
    let fixture = Fixture::new();
    let library = fixture.build("old_header_only", &["header_only_v4"]);
    let error = unsafe { RequestRouter::load(library, HostResources::new()) }
        .err()
        .expect("legacy ABI must be rejected");
    assert!(
        matches!(
            error,
            RouteError::Abi {
                version: 4,
                size: 8
            }
        ),
        "unexpected legacy-ABI error: {error:?}"
    );
}

struct Fixture(PathBuf);
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
impl Fixture {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let fixture = Self(
            std::env::temp_dir().join(format!("reknot_restart_{}_{nonce}", std::process::id())),
        );
        std::fs::create_dir(&fixture.0).unwrap();
        fixture
    }
    fn build(&self, name: &str, flags: &[&str]) -> PathBuf {
        let path = self.0.join(reknot::development::library_file_name(name));
        let mut command = Command::new("rustc");
        command
            .args([
                "--crate-type",
                "cdylib",
                "--edition",
                "2021",
                "--crate-name",
                name,
            ])
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/handler.rs"))
            .arg("-o")
            .arg(&path);
        for flag in flags {
            command.arg("--cfg").arg(flag);
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        path
    }
}

#[test]
fn native_restart_migrates_byte_schema_and_budget_includes_failed_loads() {
    use reknot::{Checkpoint, GenerationBudget, RouterOptions, WorkerId};
    #[derive(Default)]
    struct Record(Vec<Vec<u8>>);
    impl HostResource for Record {
        fn call(&mut self, _: u32, input: &[u8], _: &mut [u8]) -> Result<usize, ServiceError> {
            self.0.push(input.to_vec());
            Ok(0)
        }
    }
    let fixture = Fixture::new();
    let old = fixture.build("state_old", &["restart_worker"]);
    let new = fixture.build("state_new", &["restart_worker", "version_two"]);
    let host = HostResources::new();
    let record = host.register(ServiceKey::new(7, 1), Record::default());
    let budget = GenerationBudget::new(3).unwrap();
    let options = RouterOptions {
        generations: budget.clone(),
        max_checkpoint_bytes: 16,
    };
    let mut router = unsafe { RequestRouter::load_with_options(&old, host, options) }.unwrap();
    router
        .start_worker(
            WorkerId(1),
            record.id(),
            b"worker",
            Checkpoint::new(ServiceKey::new(100, 1), vec![3]),
        )
        .unwrap();
    let mut cx = Context::from_waker(Waker::noop());
    assert!(router.poll_workers(&mut cx).is_pending());
    assert!(matches!(
        unsafe { router.prepare(fixture.0.join("missing.dll")) },
        Err(RouteError::Load(_))
    ));
    assert_eq!(budget.used(), 2);
    let candidate = unsafe { router.prepare(&new) }.unwrap();
    let mut restart = router
        .restart(candidate, |_, state, target| {
            assert_eq!(state.bytes, [4]);
            Ok(Checkpoint::new(
                target,
                (state.bytes[0] as u16 * 10).to_le_bytes().to_vec(),
            ))
        })
        .unwrap();
    assert!(matches!(
        Pin::new(&mut restart).poll(&mut cx),
        Poll::Ready(Ok(2))
    ));
    drop(restart);
    assert!(router.poll_workers(&mut cx).is_pending());
    assert_eq!(
        record.try_borrow().unwrap().0,
        [b"v1:3".to_vec(), b"v2:40".to_vec()]
    );
    // Rejected before opening another image. The running worker is untouched.
    assert!(matches!(
        unsafe { router.prepare(&new) },
        Err(RouteError::GenerationLimit { limit: 3 })
    ));
    assert_eq!(router.generation(), 2);
    assert!(router.is_accepting());
    assert_eq!(router.in_flight(), 1);
    assert_eq!(budget.used(), 3);
}

struct Session {
    stream: TcpStream,
    outbox: Vec<Vec<u8>>,
}
impl HostResource for Session {
    fn call(&mut self, _: u32, input: &[u8], _: &mut [u8]) -> Result<usize, ServiceError> {
        self.outbox.push(input.to_vec()); // Nonblocking; only the host does socket IO.
        Ok(0)
    }
}
fn poll(task: &mut RequestTask) -> Poll<RunStatus> {
    Pin::new(task).poll(&mut Context::from_waker(Waker::noop()))
}

#[test]
fn same_tcp_connection_survives_native_drain_cutover_and_failed_preparation() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let fixture = Fixture(
        std::env::temp_dir().join(format!("reknot_handler_{}_{nonce}", std::process::id())),
    );
    std::fs::create_dir(&fixture.0).unwrap();
    let old_path = fixture.build("handler_old", &[]);
    let new_path = fixture.build("handler_new", &["version_two"]);
    let bad_path = fixture.build("handler_bad", &["bad_abi"]);
    let wrong_path = fixture.build("handler_wrong", &["wrong_service"]);

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let (stream, _) = listener.accept().unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    drop(listener); // This test cannot silently reconnect.
    let host = HostResources::new();
    let session = host.register(
        ServiceKey::new(7, 1),
        Session {
            stream,
            outbox: Vec::new(),
        },
    );
    let mut router = unsafe { RequestRouter::load(&old_path, host.clone()) }.unwrap();

    client.write_all(b"old\n").unwrap();
    let mut input = [0; 4];
    session
        .try_borrow_mut()
        .unwrap()
        .stream
        .read_exact(&mut input)
        .unwrap();
    let mut old = router.dispatch(session.id(), &input).unwrap();
    assert_eq!(poll(&mut old), Poll::Pending);
    assert!(matches!(
        unsafe { router.prepare(&bad_path) },
        Err(RouteError::Abi { .. })
    ));
    assert!(matches!(
        unsafe { router.prepare(&wrong_path) },
        Err(RouteError::ContractMismatch { .. })
    ));
    assert_eq!(router.generation(), 1);
    let candidate = unsafe { router.prepare(&new_path) }.unwrap();
    assert_eq!(router.generation(), 1);
    assert!(session.try_borrow().unwrap().outbox.is_empty());
    assert_eq!(router.activate(candidate).unwrap(), 2);
    assert_eq!(poll(&mut old), Poll::Ready(RunStatus::Done));
    assert_eq!(router.in_flight(), 0);
    {
        let mut session = session.try_borrow_mut().unwrap();
        let reply = session.outbox.remove(0);
        session.stream.write_all(&reply).unwrap();
    }
    let mut response = [0; 7];
    client.read_exact(&mut response).unwrap();
    assert_eq!(&response, b"v1:old\n");

    client.write_all(b"new\n").unwrap();
    session
        .try_borrow_mut()
        .unwrap()
        .stream
        .read_exact(&mut input)
        .unwrap();
    let mut new = router.dispatch(session.id(), &input).unwrap();
    assert_eq!(new.generation(), 2);
    assert_eq!(poll(&mut new), Poll::Pending);
    drop(router);
    drop(host);
    assert_eq!(poll(&mut new), Poll::Ready(RunStatus::Done));
    {
        let mut session = session.try_borrow_mut().unwrap();
        let reply = session.outbox.remove(0);
        session.stream.write_all(&reply).unwrap();
    }
    client.read_exact(&mut response).unwrap();
    assert_eq!(&response, b"v2:new\n");
}
