//! Host-only bounded message passing. No unsafe Send/Sync and no DLL closures.
use crate::{AsyncLimits, CancellationToken, HostResource, HostServiceFuture, ServiceError};
use std::collections::VecDeque;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Condvar, Mutex, Weak,
};
use std::task::{Context, Poll, Waker};
use std::thread::{self, JoinHandle, ThreadId};

#[derive(Clone, Copy, Debug)]
pub struct ThreadServiceLimits {
    pub capacity: usize,
    pub max_input_bytes: usize,
    pub max_output_bytes: usize,
}
impl Default for ThreadServiceLimits {
    fn default() -> Self {
        let limits = AsyncLimits::default();
        Self {
            capacity: limits.max_operations,
            max_input_bytes: limits.max_input_bytes,
            max_output_bytes: limits.max_output_bytes,
        }
    }
}
struct Reply {
    result: Option<Result<Vec<u8>, ServiceError>>,
    waker: Option<Waker>,
}
struct JobState {
    cancel: CancellationToken,
    reply: Mutex<Reply>,
    shared: Weak<Shared>,
}
impl JobState {
    fn finish(&self, result: Result<Vec<u8>, ServiceError>) {
        let waker = {
            let mut reply = self.reply.lock().unwrap();
            reply.result = Some(result);
            reply.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}
impl Drop for JobState {
    fn drop(&mut self) {
        if let Some(shared) = self.shared.upgrade() {
            shared.outstanding.fetch_sub(1, Ordering::AcqRel);
        }
    }
}
struct Job {
    operation: u32,
    input: Vec<u8>,
    state: Arc<JobState>,
}
struct Queue {
    waiting: VecDeque<Job>,
    running: Option<Arc<JobState>>,
    closed: bool,
}
struct Exit {
    done: bool,
    waker: Option<Waker>,
}
struct Shared {
    queue: Mutex<Queue>,
    ready: Condvar,
    exit: Mutex<Exit>,
    outstanding: AtomicUsize,
    limits: ThreadServiceLimits,
}
impl Shared {
    fn shutdown(&self) {
        let states = {
            let mut queue = self.queue.lock().unwrap();
            queue.closed = true;
            queue
                .waiting
                .iter()
                .map(|job| job.state.clone())
                .chain(queue.running.iter().cloned())
                .collect::<Vec<_>>()
        };
        for state in states {
            state.cancel.cancel();
        }
        self.ready.notify_all();
    }
}
/// Safe host message endpoint. May be cloned/moved to other host threads, then
/// registered in their own local HostResources. It contains no FFI handles.
#[derive(Clone)]
pub struct ServiceClient(Arc<Shared>);
impl ServiceClient {
    pub fn outstanding(&self) -> usize {
        self.0.outstanding.load(Ordering::Acquire)
    }
    pub fn is_closed(&self) -> bool {
        self.0.queue.lock().unwrap().closed
    }
}
struct JobFuture {
    state: Arc<JobState>,
    completed: bool,
    _local: PhantomData<Rc<()>>,
}
impl Future for JobFuture {
    type Output = Result<Vec<u8>, ServiceError>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let new = cx.waker().clone();
        let (result, old) = {
            let mut reply = self.state.reply.lock().unwrap();
            let old = reply.waker.replace(new);
            (reply.result.take(), old)
        };
        drop(old);
        if let Some(result) = result {
            self.completed = true;
            Poll::Ready(result)
        } else {
            Poll::Pending
        }
    }
}
impl Drop for JobFuture {
    fn drop(&mut self) {
        if !self.completed {
            self.state.cancel.cancel();
        }
    }
}
impl HostResource for ServiceClient {
    fn start_async(
        &mut self,
        operation: u32,
        input: &[u8],
        cancel: CancellationToken,
    ) -> Result<HostServiceFuture, ServiceError> {
        let shared = &self.0;
        if input.len() > shared.limits.max_input_bytes {
            return Err(ServiceError::INVALID_ARGUMENT);
        }
        let mut queue = shared.queue.lock().unwrap();
        if queue.closed {
            return Err(ServiceError::CLOSED);
        }
        shared
            .outstanding
            .try_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                if count < shared.limits.capacity {
                    Some(count + 1)
                } else {
                    None
                }
            })
            .map_err(|_| ServiceError::BUSY)?;
        let state = Arc::new(JobState {
            cancel,
            reply: Mutex::new(Reply {
                result: None,
                waker: None,
            }),
            shared: Arc::downgrade(shared),
        });
        queue.waiting.push_back(Job {
            operation,
            input: input.to_vec(),
            state: state.clone(),
        });
        drop(queue);
        shared.ready.notify_one();
        Ok(Box::pin(JobFuture {
            state,
            completed: false,
            _local: PhantomData,
        }))
    }
}
/// Owner-thread join handle. Shutdown cooperatively cancels queued/running I/O;
/// awaiting this handle joins the worker. Drop requests shutdown but does not join.
///
/// ```compile_fail
/// fn require_send<T: Send>() {}
/// require_send::<reknot::ServiceWorker>();
/// ```
pub struct ServiceWorker {
    shared: Arc<Shared>,
    handle: Option<JoinHandle<()>>,
    owner: ThreadId,
    _local: PhantomData<Rc<()>>,
}
impl ServiceWorker {
    pub fn shutdown(&self) {
        assert_eq!(self.owner, thread::current().id());
        self.shared.shutdown();
    }
}
impl Future for ServiceWorker {
    type Output = Result<(), ServiceError>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        assert_eq!(self.owner, thread::current().id());
        let Some(handle) = &self.handle else {
            return Poll::Ready(Ok(()));
        };
        let new = cx.waker().clone();
        let (done, old) = {
            let mut exit = self.shared.exit.lock().unwrap();
            let old = exit.waker.replace(new);
            (exit.done, old)
        };
        drop(old);
        if handle.is_finished() {
            let result = self.handle.take().unwrap().join();
            Poll::Ready(result.map_err(|_| ServiceError::PANICKED))
        } else {
            // A notification just before thread return must not lose the final join.
            if done {
                cx.waker().wake_by_ref();
            }
            Poll::Pending
        }
    }
}
impl Drop for ServiceWorker {
    fn drop(&mut self) {
        assert_eq!(self.owner, thread::current().id());
        self.shared.shutdown();
    }
}
pub struct ThreadService;
impl ThreadService {
    /// Runs only trusted host code. Accepted jobs are FIFO and non-preemptive;
    /// handlers must bound I/O waits and observe cancellation. Capacity includes
    /// queued, running and completed-but-unconsumed replies, across all clients.
    pub fn spawn(
        limits: ThreadServiceLimits,
        mut handler: impl FnMut(u32, Vec<u8>, CancellationToken) -> Result<Vec<u8>, ServiceError>
            + Send
            + 'static,
    ) -> std::io::Result<(ServiceClient, ServiceWorker)> {
        AsyncLimits {
            max_operations: limits.capacity,
            max_input_bytes: limits.max_input_bytes,
            max_output_bytes: limits.max_output_bytes,
        }
        .validate()
        .map_err(std::io::Error::other)?;
        let shared = Arc::new(Shared {
            queue: Mutex::new(Queue {
                waiting: VecDeque::new(),
                running: None,
                closed: false,
            }),
            ready: Condvar::new(),
            exit: Mutex::new(Exit {
                done: false,
                waker: None,
            }),
            outstanding: AtomicUsize::new(0),
            limits,
        });
        let worker = shared.clone();
        let handle = thread::Builder::new()
            .name("reknot-service".into())
            .spawn(move || {
                loop {
                    let job = {
                        let mut queue = worker.queue.lock().unwrap();
                        while queue.waiting.is_empty() && !queue.closed {
                            queue = worker.ready.wait(queue).unwrap();
                        }
                        let Some(job) = queue.waiting.pop_front() else {
                            break;
                        };
                        queue.running = Some(job.state.clone());
                        job
                    };
                    let result = if job.state.cancel.is_cancelled() {
                        Err(ServiceError::CANCELLED)
                    } else {
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            handler(job.operation, job.input, job.state.cancel.clone())
                        }))
                        .unwrap_or(Err(ServiceError::PANICKED))
                    };
                    let result = if job.state.cancel.is_cancelled() {
                        Err(ServiceError::CANCELLED)
                    } else {
                        result.and_then(|bytes| {
                            if bytes.len() <= worker.limits.max_output_bytes {
                                Ok(bytes)
                            } else {
                                Err(ServiceError::INVALID_ARGUMENT)
                            }
                        })
                    };
                    worker.queue.lock().unwrap().running = None;
                    job.state.finish(result);
                }
                let waker = {
                    let mut exit = worker.exit.lock().unwrap();
                    exit.done = true;
                    exit.waker.take()
                };
                if let Some(waker) = waker {
                    waker.wake();
                }
            })?;
        Ok((
            ServiceClient(shared.clone()),
            ServiceWorker {
                shared,
                handle: Some(handle),
                owner: thread::current().id(),
                _local: PhantomData,
            },
        ))
    }
}
