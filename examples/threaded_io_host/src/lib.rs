//! cargo run -p threaded_io_host --locked
//! Real DLLs and TCP; two local logic executors share one host-owned I/O thread.
use reknot::*;
use std::cell::RefCell;
use std::future::Future;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::pin::Pin;
use std::process::Command;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};
use std::thread;
use std::time::{Duration, Instant};

const IO: ServiceKey = ServiceKey::new(0x5449_4f31, 1);
struct Notify(thread::Thread);
impl Wake for Notify {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}
fn drive<F: Future + Unpin>(mut future: F) -> F::Output {
    let waker = Waker::from(Arc::new(Notify(thread::current())));
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Poll::Ready(result) = Pin::new(&mut future).poll(&mut Context::from_waker(&waker)) {
            return result;
        }
        assert!(Instant::now() < deadline, "I/O timeout");
        thread::park_timeout(Duration::from_millis(10));
    }
}
fn libraries() -> Result<(PathBuf, PathBuf), Box<dyn std::error::Error>> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let target = root
        .join("target/threaded-io-build")
        .join(std::process::id().to_string());
    std::fs::create_dir_all(&target)?;
    let mut paths = Vec::new();
    for (name, version_two) in [("io_v1", false), ("io_v2", true)] {
        let mut cargo = Command::new(option_env!("CARGO").unwrap_or("cargo"));
        cargo
            .current_dir(&root)
            .args([
                "build",
                "--locked",
                "-p",
                "threaded_io_logic",
                "--lib",
                "--no-default-features",
            ])
            .arg("--target-dir")
            .arg(&target);
        if version_two {
            cargo.args(["--features", "version_two"]);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cargo.creation_flags(0x08000000);
        }
        let output = cargo.output()?;
        if !output.status.success() {
            return Err(String::from_utf8_lossy(&output.stderr).into_owned().into());
        }
        let path = target.join(development::library_file_name(name));
        std::fs::copy(
            development::library_path_in(&target, "threaded_io_logic"),
            &path,
        )?;
        paths.push(path);
    }
    Ok((paths.remove(0), paths.remove(0)))
}
struct Session {
    client: ServiceClient,
    replies: Rc<RefCell<Vec<Vec<u8>>>>,
    owner: thread::ThreadId,
}
impl HostResource for Session {
    fn call(&mut self, operation: u32, input: &[u8], _: &mut [u8]) -> Result<usize, ServiceError> {
        assert_eq!(self.owner, thread::current().id());
        if operation != 2 {
            return Err(ServiceError::UNSUPPORTED_OPERATION);
        }
        self.replies.borrow_mut().push(input.to_vec());
        Ok(0)
    }
    fn start_async(
        &mut self,
        operation: u32,
        input: &[u8],
        cancel: CancellationToken,
    ) -> Result<HostServiceFuture, ServiceError> {
        assert_eq!(self.owner, thread::current().id());
        self.client.start_async(operation, input, cancel)
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        assert_eq!(self.owner, thread::current().id());
    }
}

pub fn verify() -> Result<(), Box<dyn std::error::Error>> {
    let (v1, v2) = libraries()?;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let address = listener.local_addr()?;
    let peer = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut count = 0;
        while count < 4 {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    // Windows may inherit the listener's nonblocking mode.
                    stream.set_nonblocking(false).unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(2)))
                        .unwrap();
                    stream
                        .set_write_timeout(Some(Duration::from_secs(2)))
                        .unwrap();
                    let mut bytes = [0; 4];
                    stream.read_exact(&mut bytes).unwrap();
                    assert_eq!(&bytes, b"ping");
                    stream.write_all(b"pong").unwrap();
                    count += 1;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "peer timeout");
                    thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("{error}"),
            }
        }
    });
    let io_owners = Arc::new(Mutex::new(Vec::new()));
    let observed = io_owners.clone();
    let (client, worker) = ThreadService::spawn(
        ThreadServiceLimits {
            capacity: 8,
            ..Default::default()
        },
        move |_, input, cancel| {
            observed.lock().unwrap().push(thread::current().id());
            let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(2))
                .map_err(|_| ServiceError::FAILED)?;
            stream
                .set_read_timeout(Some(Duration::from_millis(20)))
                .map_err(|_| ServiceError::FAILED)?;
            stream
                .set_write_timeout(Some(Duration::from_millis(20)))
                .map_err(|_| ServiceError::FAILED)?;
            stream.write_all(&input).map_err(|_| ServiceError::FAILED)?;
            let mut reply = vec![0; 4];
            let mut read = 0;
            let deadline = Instant::now() + Duration::from_secs(2);
            while read < reply.len() {
                if cancel.is_cancelled() {
                    return Err(ServiceError::CANCELLED);
                }
                match stream.read(&mut reply[read..]) {
                    Ok(0) => return Err(ServiceError::FAILED),
                    Ok(count) => read += count,
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) && Instant::now() < deadline => {}
                    Err(_) => return Err(ServiceError::FAILED),
                }
            }
            Ok(reply)
        },
    )?;
    let mut logic_owners = Vec::new();
    for _ in 0..2 {
        let client = client.clone();
        let old = v1.clone();
        let new = v2.clone();
        logic_owners.push(thread::spawn(move || {
            let owner = thread::current().id();
            let replies = Rc::new(RefCell::new(Vec::new()));
            let resources = HostResources::new();
            let session = resources.register(
                IO,
                Session {
                    client,
                    replies: replies.clone(),
                    owner,
                },
            );
            // Only trusted local workspace DLLs built against this host ABI.
            let mut router = unsafe { RequestRouter::load(old, resources) }.unwrap();
            let mut first = router.dispatch(session.id(), b"ping").unwrap();
            assert!(Pin::new(&mut first)
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending());
            let candidate = unsafe { router.prepare(new) }.unwrap();
            assert_eq!(router.activate(candidate).unwrap(), 2);
            let second = router.dispatch(session.id(), b"ping").unwrap();
            assert_eq!(drive(first), RunStatus::Done);
            assert_eq!(drive(second), RunStatus::Done);
            assert_eq!(
                *replies.borrow(),
                [b"v1:g1:pong".to_vec(), b"v2:g2:pong".to_vec()]
            );
            assert_eq!(router.in_flight(), 0);
            drop(router);
            drop(session);
            owner
        }));
    }
    let owners: Vec<_> = logic_owners
        .into_iter()
        .map(|owner| owner.join().unwrap())
        .collect();
    worker.shutdown();
    drive(worker)?;
    peer.join().unwrap();
    assert_eq!(client.outstanding(), 0);
    let io = io_owners.lock().unwrap();
    assert_eq!(io.len(), 4);
    assert!(io.iter().all(|owner| *owner == io[0]));
    assert!(owners.iter().all(|owner| *owner != io[0]));
    println!("PASS: two logic owners, two real DLL generations, host TCP I/O, bounded queue, owner-thread destruction and worker join");
    Ok(())
}
