//! A host-only queue. FFI tasks and DLL allocations never enter shared state.
use super::runtime::RawCall;
use super::*;
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll, Waker};

#[derive(Clone, Copy, Debug)]
pub struct BridgeOptions {
    /// Queued + running + completed-but-unconsumed requests. Cancelled running
    /// requests retain their slot until registered cleanup has actually joined.
    pub capacity: usize,
    /// Maximum requests this driver polls concurrently. Admission is FIFO;
    /// completion need not be. The ModuleBuilder's separate limit still applies.
    pub max_in_flight: usize,
}
impl Default for BridgeOptions {
    fn default() -> Self {
        Self {
            capacity: 256,
            max_in_flight: 64,
        }
    }
}
struct Reply {
    result: Option<Result<Vec<u8>, Error>>,
    waker: Option<Waker>,
}
struct RequestState {
    reply: Mutex<Reply>,
    cancelled: AtomicBool,
    generation: AtomicU64,
    shared: Weak<Shared>,
}
impl RequestState {
    fn finish(&self, result: Result<Vec<u8>, Error>) -> Option<Waker> {
        {
            let mut reply = self.reply.lock().unwrap();
            reply.result = Some(result);
            reply.waker.take()
        }
    }
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        if let Some(shared) = self.shared.upgrade() {
            shared.wake_driver();
        }
    }
}
impl Drop for RequestState {
    fn drop(&mut self) {
        if let Some(shared) = self.shared.upgrade() {
            shared.outstanding.fetch_sub(1, Ordering::AcqRel);
        }
    }
}
struct Job {
    handler: HandlerSpec,
    input: Vec<u8>,
    state: Arc<RequestState>,
}
struct Queue {
    waiting: VecDeque<Job>,
    driver: Option<Waker>,
    closed: bool,
    cancel: bool,
    exit: Option<bool>, // true: joined; false: driver abandoned
    waiters: Vec<Weak<Mutex<Option<Waker>>>>,
}
struct Shared {
    queue: Mutex<Queue>,
    module: Module,
    options: BridgeOptions,
    clients: AtomicUsize,
    outstanding: AtomicUsize,
}
impl Shared {
    fn wake_driver(&self) {
        let wake = self.queue.lock().unwrap().driver.take();
        if let Some(wake) = wake {
            wake.wake();
        }
    }
    fn exit(&self, joined: bool) -> Vec<Waker> {
        let (waiters, driver) = {
            let mut queue = self.queue.lock().unwrap();
            queue.closed = true;
            queue.exit = Some(joined);
            (std::mem::take(&mut queue.waiters), queue.driver.take())
        };
        drop(driver);
        waiters
            .into_iter()
            .filter_map(|weak| weak.upgrade())
            .filter_map(|waiter| waiter.lock().unwrap().take())
            .collect()
    }
}

/// Send + Sync endpoint for resident framework code. Contains only owned bytes,
/// host errors and host Wakers; the module itself remains on its owner thread.
pub struct ModuleClient(Arc<Shared>);
impl Clone for ModuleClient {
    fn clone(&self) -> Self {
        self.0.clients.fetch_add(1, Ordering::Relaxed);
        Self(self.0.clone())
    }
}
impl Drop for ModuleClient {
    fn drop(&mut self) {
        if self.0.clients.fetch_sub(1, Ordering::AcqRel) == 1 {
            // Last sender closes admission but lets accepted work finish.
            let wake = {
                let mut queue = self.0.queue.lock().unwrap();
                queue.closed = true;
                queue.driver.take()
            };
            if let Some(wake) = wake {
                wake.wake();
            }
        }
    }
}
impl ModuleClient {
    pub fn handler<I, O>(&self, handler: Handler<I, O>) -> Result<HandlerClient<I, O>, Error>
    where
        I: Serialize + DeserializeOwned + 'static,
        O: Serialize + DeserializeOwned + 'static,
    {
        if !self.0.module.handlers.contains(&handler.descriptor()) {
            return Err(Error::Configuration(format!(
                "handler {} v{} is not registered in module {} with these input/output types",
                handler.name, handler.version, self.0.module.name
            )));
        }
        Ok(HandlerClient {
            client: self.clone(),
            handler,
        })
    }
    pub fn outstanding(&self) -> usize {
        self.0.outstanding.load(Ordering::Acquire)
    }
    pub fn is_closed(&self) -> bool {
        self.0.queue.lock().unwrap().closed
    }
    /// Close admission now and await all accepted work. Keep driving the owner.
    pub fn close(&self) -> BridgeShutdown {
        self.end(false)
    }
    /// Close admission and request cancellation now. Awaiting confirms registered
    /// cleanup has joined, provided the owner driver is still being polled.
    pub fn shutdown(&self) -> BridgeShutdown {
        self.end(true)
    }
    fn end(&self, cancel: bool) -> BridgeShutdown {
        let waiter = Arc::new(Mutex::new(None));
        let wake = {
            let mut queue = self.0.queue.lock().unwrap();
            queue.closed = true;
            queue.cancel |= cancel;
            queue.waiters.retain(|waiter| waiter.strong_count() > 0);
            queue.waiters.push(Arc::downgrade(&waiter));
            queue.driver.take()
        };
        if let Some(wake) = wake {
            wake.wake();
        }
        BridgeShutdown {
            shared: self.0.clone(),
            waiter,
        }
    }
    fn submit(&self, handler: HandlerSpec, input: Vec<u8>) -> Result<Arc<RequestState>, Error> {
        let (state, wake) = {
            let mut queue = self.0.queue.lock().unwrap();
            if queue.closed {
                return Err(Error::BridgeClosed);
            }
            self.0
                .outstanding
                .try_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                    if count < self.0.options.capacity {
                        count.checked_add(1)
                    } else {
                        None
                    }
                })
                .map_err(|_| Error::Busy)?;
            let state = Arc::new(RequestState {
                reply: Mutex::new(Reply {
                    result: None,
                    waker: None,
                }),
                cancelled: AtomicBool::new(false),
                generation: AtomicU64::new(0),
                shared: Arc::downgrade(&self.0),
            });
            queue.waiting.push_back(Job {
                handler,
                input,
                state: state.clone(),
            });
            (state, queue.driver.take())
        };
        if let Some(wake) = wake {
            wake.wake();
        }
        Ok(state)
    }
}

pub struct HandlerClient<I, O> {
    client: ModuleClient,
    handler: Handler<I, O>,
}
impl<I, O> Clone for HandlerClient<I, O> {
    fn clone(&self) -> Self {
        Self {
            client: self.client.clone(),
            handler: self.handler,
        }
    }
}
impl<I: Serialize + DeserializeOwned + 'static, O: Serialize + DeserializeOwned + 'static>
    HandlerClient<I, O>
{
    /// Enqueue owned bytes immediately. Generation is selected when the owner
    /// dequeues/admits the event, not when this cross-thread call enters the queue.
    /// Dropping the returned future requests cancellation without replaying work.
    pub fn call(&self, input: I) -> RemoteCall<O> {
        let result = encode(&input, MAX_MESSAGE_BYTES)
            .and_then(|bytes| self.client.submit(self.handler.descriptor(), bytes));
        let (state, error) = match result {
            Ok(state) => (Some(state), None),
            Err(error) => (None, Some(error)),
        };
        RemoteCall {
            state,
            error,
            marker: PhantomData,
            completed: false,
        }
    }
}
/// A Send future. Input/output conversion runs on the caller's thread. Only
/// the resident owner driver ever constructs, polls or destroys DLL tasks.
pub struct RemoteCall<O> {
    state: Option<Arc<RequestState>>,
    error: Option<Error>,
    marker: PhantomData<fn() -> O>,
    completed: bool,
}
impl<O> RemoteCall<O> {
    pub fn generation(&self) -> Option<u64> {
        let value = self.state.as_ref()?.generation.load(Ordering::Acquire);
        (value != 0).then_some(value)
    }
    /// Request cancellation; continue awaiting to observe completion/cleanup.
    /// A result that already completed may win the race with cancellation.
    pub fn cancel(&self) {
        if let Some(state) = &self.state {
            state.cancel();
        }
    }
}
impl<O: DeserializeOwned> Future for RemoteCall<O> {
    type Output = Result<O, Error>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        assert!(!self.completed, "RemoteCall polled after completion");
        if let Some(error) = self.error.take() {
            self.completed = true;
            return Poll::Ready(Err(error));
        }
        let new = cx.waker().clone();
        let (result, old) = {
            let mut reply = self.state.as_ref().unwrap().reply.lock().unwrap();
            let result = reply.result.take();
            let old = if result.is_some() {
                reply.waker.take()
            } else {
                reply.waker.replace(new)
            };
            (result, old)
        };
        drop(old);
        let Some(result) = result else {
            return Poll::Pending;
        };
        self.completed = true;
        self.state.take();
        Poll::Ready(result.and_then(|bytes| {
            serde_json::from_slice(&bytes).map_err(|error| Error::Codec(error.to_string()))
        }))
    }
}
impl<O> Drop for RemoteCall<O> {
    fn drop(&mut self) {
        if let Some(state) = &self.state {
            let old = state.reply.lock().unwrap().waker.take();
            drop(old);
            if !self.completed {
                state.cancel();
            }
        }
    }
}
/// Completes after the owner driver has joined accepted requests and registered
/// cleanup. Driver abandonment returns BridgeClosed, never a successful join.
pub struct BridgeShutdown {
    shared: Arc<Shared>,
    waiter: Arc<Mutex<Option<Waker>>>,
}
impl Future for BridgeShutdown {
    type Output = Result<(), Error>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let new = cx.waker().clone();
        let old = self.waiter.lock().unwrap().replace(new);
        drop(old);
        let exit = self.shared.queue.lock().unwrap().exit;
        match exit {
            Some(joined) => {
                let old = self.waiter.lock().unwrap().take();
                drop(old);
                Poll::Ready(if joined {
                    Ok(())
                } else {
                    Err(Error::BridgeClosed)
                })
            }
            None => Poll::Pending,
        }
    }
}

struct Running {
    call: RawCall,
    state: Arc<RequestState>,
}
/// !Send owner-side future. Poll on the module's thread. It exits after clients
/// close admission and accepted work has joined. Prefer close/shutdown to Drop:
/// dropping this driver parks unfinished cleanup in `ModuleHost::poll_cleanup`.
///
/// ```compile_fail
/// fn require_send<T: Send>() {}
/// require_send::<reknot::typed::BridgeDriver>();
/// ```
pub struct BridgeDriver {
    host: ModuleHost,
    shared: Arc<Shared>,
    running: Vec<Running>,
    finished: bool,
}
impl ModuleHost {
    /// Create an owned-message bridge for multithreaded event frameworks. Keep
    /// polling its driver locally; endpoints alone do not run native code.
    pub fn bridge(&self, options: BridgeOptions) -> Result<(ModuleClient, BridgeDriver), Error> {
        if options.capacity == 0
            || options.max_in_flight == 0
            || options.max_in_flight > options.capacity
        {
            return Err(Error::Configuration(
                "bridge limits must be positive, with max_in_flight <= capacity".into(),
            ));
        }
        let shared = Arc::new(Shared {
            queue: Mutex::new(Queue {
                waiting: VecDeque::new(),
                driver: None,
                closed: false,
                cancel: false,
                exit: None,
                waiters: Vec::new(),
            }),
            module: self.module().clone(),
            options,
            clients: AtomicUsize::new(1),
            outstanding: AtomicUsize::new(0),
        });
        Ok((
            ModuleClient(shared.clone()),
            BridgeDriver {
                host: self.clone(),
                shared,
                running: Vec::new(),
                finished: false,
            },
        ))
    }
}
impl Future for BridgeDriver {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.finished {
            return Poll::Ready(());
        }
        let new = cx.waker().clone();
        let (jobs, cancel, old) = {
            let mut queue = self.shared.queue.lock().unwrap();
            let old = queue.driver.replace(new);
            let mut available = self.shared.options.max_in_flight - self.running.len();
            let mut jobs = Vec::new();
            // Release cancelled queued work even when running slots are full.
            // FIFO is preserved for requests that still need admission.
            let waiting = std::mem::take(&mut queue.waiting);
            for job in waiting {
                if queue.cancel || job.state.cancelled.load(Ordering::Acquire) {
                    jobs.push(job);
                } else if available > 0 {
                    available -= 1;
                    jobs.push(job);
                } else {
                    queue.waiting.push_back(job);
                }
            }
            (jobs, queue.cancel, old)
        };
        drop(old);
        let mut wakes = Vec::new();
        for job in jobs {
            if cancel || job.state.cancelled.load(Ordering::Acquire) {
                wakes.extend(job.state.finish(Err(Error::Cancelled)));
                continue;
            }
            let call = self.host.call_raw(&job.handler, job.input);
            if let Some(generation) = call.generation() {
                job.state.generation.store(generation, Ordering::Release);
            }
            self.running.push(Running {
                call,
                state: job.state,
            });
        }
        self.running.retain_mut(|running| {
            if cancel || running.state.cancelled.load(Ordering::Acquire) {
                running.call.cancel();
            }
            match Pin::new(&mut running.call).poll(cx) {
                Poll::Ready(result) => {
                    wakes.extend(running.state.finish(result));
                    false
                }
                Poll::Pending => true,
            }
        });
        let (finish, more) = {
            let queue = self.shared.queue.lock().unwrap();
            (
                queue.closed && queue.waiting.is_empty() && self.running.is_empty(),
                !queue.waiting.is_empty() && self.running.len() < self.shared.options.max_in_flight,
            )
        };
        let status = if finish {
            self.finished = true;
            wakes.extend(self.shared.exit(true));
            Poll::Ready(())
        } else {
            if more {
                cx.waker().wake_by_ref();
            }
            Poll::Pending
        };
        // Retire completed work before invoking arbitrary host executor Wakers.
        for wake in wakes {
            wake.wake();
        }
        status
    }
}
impl Drop for BridgeDriver {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        let jobs = {
            let mut queue = self.shared.queue.lock().unwrap();
            queue.closed = true;
            std::mem::take(&mut queue.waiting)
        };
        let mut wakes = Vec::new();
        for job in jobs {
            wakes.extend(job.state.finish(Err(Error::BridgeClosed)));
        }
        for running in self.running.drain(..) {
            drop(running.call);
            wakes.extend(running.state.finish(Err(Error::BridgeClosed)));
        }
        wakes.extend(self.shared.exit(false));
        for wake in wakes {
            wake.wake();
        }
    }
}
