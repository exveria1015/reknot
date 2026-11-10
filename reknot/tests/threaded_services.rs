use reknot::*;
use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    mpsc, Arc, Condvar, Mutex,
};
use std::task::{Context, Poll, Wake, Waker};
use std::thread;
use std::time::{Duration, Instant};

struct Notify {
    thread: thread::Thread,
    wakes: Arc<AtomicUsize>,
}
impl Wake for Notify {
    fn wake(self: Arc<Self>) {
        self.wakes.fetch_add(1, Ordering::Relaxed);
        self.thread.unpark();
    }
}
fn block_on<F: Future + Unpin>(mut future: F) -> F::Output {
    let waker = Waker::from(Arc::new(Notify {
        thread: thread::current(),
        wakes: Arc::default(),
    }));
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Poll::Ready(result) = Pin::new(&mut future).poll(&mut Context::from_waker(&waker)) {
            return result;
        }
        assert!(Instant::now() < deadline, "service timeout");
        thread::park_timeout(Duration::from_millis(10));
    }
}

#[test]
fn fifo_bounded_queue_cancellation_skips_io_and_completion_notifies_owner() {
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let worker_gate = gate.clone();
    let (started_tx, started_rx) = mpsc::channel();
    let (mut client, worker) = ThreadService::spawn(
        ThreadServiceLimits {
            capacity: 3,
            ..Default::default()
        },
        move |op, input, _| {
            started_tx.send(op).unwrap();
            if op == 1 {
                let (lock, ready) = &*worker_gate;
                let mut open = lock.lock().unwrap();
                while !*open {
                    open = ready.wait(open).unwrap();
                }
            }
            Ok(input)
        },
    )
    .unwrap();
    let mut first = client
        .start_async(1, b"first", CancellationToken::default())
        .unwrap();
    assert_eq!(started_rx.recv_timeout(Duration::from_secs(5)).unwrap(), 1);
    let second = client
        .start_async(2, b"second", CancellationToken::default())
        .unwrap();
    let third = client
        .start_async(3, b"cancel", CancellationToken::default())
        .unwrap();
    assert_eq!(client.outstanding(), 3);
    assert!(matches!(
        client.start_async(4, b"full", CancellationToken::default()),
        Err(ServiceError::BUSY)
    ));
    let wakes = Arc::new(AtomicUsize::new(0));
    let waker = Waker::from(Arc::new(Notify {
        thread: thread::current(),
        wakes: wakes.clone(),
    }));
    assert!(first
        .as_mut()
        .poll(&mut Context::from_waker(&waker))
        .is_pending());
    drop(third); // queued cancellation; accepted job still owns its bounded slot
    *gate.0.lock().unwrap() = true;
    gate.1.notify_all();
    let deadline = Instant::now() + Duration::from_secs(5);
    while wakes.load(Ordering::Relaxed) == 0 {
        assert!(
            Instant::now() < deadline,
            "completion did not wake its owner"
        );
        thread::park_timeout(Duration::from_millis(1));
    }
    assert_eq!(block_on(first).unwrap(), b"first");
    assert_eq!(block_on(second).unwrap(), b"second");
    assert_eq!(started_rx.recv_timeout(Duration::from_secs(5)).unwrap(), 2);
    assert!(started_rx.try_recv().is_err()); // job 3 did not execute
    assert!(wakes.load(Ordering::Relaxed) > 0);
    worker.shutdown();
    assert_eq!(block_on(worker), Ok(()));
    assert_eq!(client.outstanding(), 0);
    assert!(client.is_closed());
    assert!(matches!(
        client.start_async(0, b"", CancellationToken::default()),
        Err(ServiceError::CLOSED)
    ));
}

#[test]
fn completed_but_unconsumed_reply_keeps_capacity_and_panicked_handler_is_terminal() {
    let (tx, rx) = mpsc::channel();
    let (mut client, worker) = ThreadService::spawn(
        ThreadServiceLimits {
            capacity: 1,
            ..Default::default()
        },
        move |op, input, _| {
            tx.send(()).unwrap();
            if op == 1 {
                panic!("host I/O handler panic");
            }
            Ok(input)
        },
    )
    .unwrap();
    let first = client
        .start_async(0, b"reply", CancellationToken::default())
        .unwrap();
    rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(client.outstanding(), 1);
    assert!(matches!(
        client.start_async(0, b"full", CancellationToken::default()),
        Err(ServiceError::BUSY)
    ));
    assert_eq!(block_on(first).unwrap(), b"reply");
    // Worker completion and future destruction can race; wait for the last owner.
    while client.outstanding() != 0 {
        thread::yield_now();
    }
    let failure = client
        .start_async(1, b"", CancellationToken::default())
        .unwrap();
    assert_eq!(block_on(failure), Err(ServiceError::PANICKED));
    worker.shutdown();
    assert_eq!(block_on(worker), Ok(()));
}

#[test]
fn shutdown_cancels_running_and_queued_jobs_and_joins_on_creator_thread() {
    let (tx, rx) = mpsc::channel();
    let (mut client, worker) = ThreadService::spawn(Default::default(), move |_, _, cancel| {
        tx.send(()).unwrap();
        while !cancel.is_cancelled() {
            thread::yield_now();
        }
        Ok(b"discard after cancellation".to_vec())
    })
    .unwrap();
    let running = client
        .start_async(0, b"", CancellationToken::default())
        .unwrap();
    rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let queued = client
        .start_async(1, b"", CancellationToken::default())
        .unwrap();
    worker.shutdown();
    assert_eq!(block_on(running), Err(ServiceError::CANCELLED));
    assert_eq!(block_on(queued), Err(ServiceError::CANCELLED));
    assert_eq!(block_on(worker), Ok(()));
    assert!(rx.try_recv().is_err());
}

const KEY: ServiceKey = ServiceKey::new(0x5448_5244, 1);
struct LocalService {
    client: ServiceClient,
    reply: Rc<RefCell<Vec<u8>>>,
    owner: thread::ThreadId,
}
impl HostResource for LocalService {
    fn call(&mut self, _: u32, bytes: &[u8], _: &mut [u8]) -> Result<usize, ServiceError> {
        assert_eq!(self.owner, thread::current().id());
        *self.reply.borrow_mut() = bytes.to_vec();
        Ok(0)
    }
    fn start_async(
        &mut self,
        op: u32,
        bytes: &[u8],
        cancel: CancellationToken,
    ) -> Result<HostServiceFuture, ServiceError> {
        assert_eq!(self.owner, thread::current().id());
        self.client.start_async(op, bytes, cancel)
    }
}
impl Drop for LocalService {
    fn drop(&mut self) {
        assert_eq!(self.owner, thread::current().id());
    }
}
#[reload_handler(KEY)]
async fn logic(ctx: RequestContext) -> Result<(), ServiceError> {
    let reply = ctx.call_async(0, ctx.input(), 64)?.await?;
    ctx.call(1, &reply, &mut [])?;
    Ok(())
}

#[test]
fn two_logic_owner_threads_share_only_safe_host_client_and_io_owner() {
    fn require_send_sync<T: Send + Sync>() {}
    require_send_sync::<ServiceClient>();
    let io_threads = Arc::new(Mutex::new(Vec::new()));
    let observed = io_threads.clone();
    let (client, worker) = ThreadService::spawn(Default::default(), move |_, input, _| {
        observed.lock().unwrap().push(thread::current().id());
        Ok(input)
    })
    .unwrap();
    let mut owners = Vec::new();
    for input in [b"owner-a".to_vec(), b"owner-b".to_vec()] {
        let client = client.clone();
        owners.push(thread::spawn(move || {
            let owner = thread::current().id();
            let reply = Rc::new(RefCell::new(Vec::new()));
            let resources = HostResources::new();
            let local = resources.register(
                KEY,
                LocalService {
                    client,
                    reply: reply.clone(),
                    owner,
                },
            );
            let router = unsafe { RequestRouter::from_api(*hot_handler_api(), resources) }.unwrap();
            let task = router.dispatch(local.id(), &input).unwrap();
            assert_eq!(block_on(task), RunStatus::Done);
            assert_eq!(*reply.borrow(), input);
            assert_eq!(router.in_flight(), 0);
            drop(router);
            drop(local);
            owner
        }));
    }
    let owners: Vec<_> = owners
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect();
    worker.shutdown();
    block_on(worker).unwrap();
    let io = io_threads.lock().unwrap();
    assert_eq!(io.len(), 2);
    assert_eq!(io[0], io[1]);
    assert!(owners.iter().all(|owner| *owner != io[0]));
    assert_eq!(client.outstanding(), 0);
}

#[cfg(not(miri))]
#[test]
fn real_tcp_io_stays_in_host_worker_and_returns_to_logic_owner() {
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let peer = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut input = [0; 4];
        stream.read_exact(&mut input).unwrap();
        assert_eq!(&input, b"ping");
        stream.write_all(b"pong").unwrap();
    });
    let (mut client, worker) = ThreadService::spawn(Default::default(), move |_, input, cancel| {
        let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(2))
            .map_err(|_| ServiceError::FAILED)?;
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .map_err(|_| ServiceError::FAILED)?;
        if cancel.is_cancelled() {
            return Err(ServiceError::CANCELLED);
        }
        stream.write_all(&input).map_err(|_| ServiceError::FAILED)?;
        let mut result = vec![0; 4];
        stream
            .read_exact(&mut result)
            .map_err(|_| ServiceError::FAILED)?;
        Ok(result)
    })
    .unwrap();
    let request = client
        .start_async(0, b"ping", CancellationToken::default())
        .unwrap();
    assert_eq!(block_on(request).unwrap(), b"pong");
    worker.shutdown();
    block_on(worker).unwrap();
    peer.join().unwrap();
}

#[cfg(not(miri))]
#[test]
fn cancel_during_real_tcp_read_waits_for_worker_ack_without_replaying_write() {
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (received, input_seen) = mpsc::channel();
    let peer = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut input = [0; 4];
        stream.read_exact(&mut input).unwrap();
        assert_eq!(&input, b"ping");
        received.send(()).unwrap();
        let mut next = [0];
        assert_eq!(stream.read(&mut next).unwrap(), 0); // cancelled I/O closed its socket
    });
    let writes = Arc::new(AtomicUsize::new(0));
    let observed = writes.clone();
    let (client, worker) = ThreadService::spawn(Default::default(), move |_, input, cancel| {
        let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(2))
            .map_err(|_| ServiceError::FAILED)?;
        stream
            .set_write_timeout(Some(Duration::from_secs(1)))
            .map_err(|_| ServiceError::FAILED)?;
        stream
            .set_read_timeout(Some(Duration::from_millis(20)))
            .map_err(|_| ServiceError::FAILED)?;
        stream.write_all(&input).map_err(|_| ServiceError::FAILED)?;
        observed.fetch_add(1, Ordering::Relaxed);
        let mut byte = [0];
        loop {
            if cancel.is_cancelled() {
                return Err(ServiceError::CANCELLED);
            }
            match stream.read(&mut byte) {
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                _ => return Err(ServiceError::FAILED),
            }
        }
    })
    .unwrap();
    let resources = HostResources::new();
    let reply = Rc::new(RefCell::new(Vec::new()));
    let service = resources.register(
        KEY,
        LocalService {
            client,
            reply: reply.clone(),
            owner: thread::current().id(),
        },
    );
    let router = unsafe { RequestRouter::from_api(*hot_handler_api(), resources) }.unwrap();
    let mut task = router.dispatch(service.id(), b"ping").unwrap();
    assert!(Pin::new(&mut task)
        .poll(&mut Context::from_waker(Waker::noop()))
        .is_pending());
    input_seen.recv_timeout(Duration::from_secs(5)).unwrap();
    drop(task);
    assert_eq!(router.in_flight(), 1);
    block_on(Box::pin(std::future::poll_fn(poll_cancelled_requests)));
    assert_eq!(router.in_flight(), 0);
    assert!(reply.borrow().is_empty());
    worker.shutdown();
    block_on(worker).unwrap();
    peer.join().unwrap();
    assert_eq!(writes.load(Ordering::Relaxed), 1); // cancellation is not a rollback or retry
}
