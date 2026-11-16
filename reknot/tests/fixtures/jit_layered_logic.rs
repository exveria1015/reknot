//! SDK-generated logic linked only to the resident support/std pair.
extern crate jit_layered_support as reknot;
use reknot::{GenerationLocal, RequestContext, ResourceId, ServiceError, ServiceKey, Services};
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::Poll;

struct JoinedCallback {
    thread: Option<std::thread::JoinHandle<()>>,
    started: bool,
    panic_adapter: bool,
}
// Audited fixture adapter: registration precedes spawning; join covers the raw
// Waker's last drop AND the remaining logic instructions and thread return.
// Unknown external libraries do not inherit this contract from this test.
unsafe impl reknot::ManagedActivity for JoinedCallback {
    fn request_stop(self: std::pin::Pin<&mut Self>) -> Result<(), ServiceError> {
        Ok(())
    }
    fn poll_join(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Result<(), ServiceError>> {
        assert!(
            !self.panic_adapter,
            "external adapter panic: termination is unknown"
        );
        if !self.started {
            let waker = cx.waker().clone();
            self.started = true;
            self.thread = Some(std::thread::spawn(move || {
                drop(waker);
                reknot::tail_barrier(); // Still executing this logic image.
                std::hint::black_box(VERSION);
            }));
        }
        if self.thread.as_ref().unwrap().is_finished() {
            let result = self.thread.take().unwrap().join();
            return Poll::Ready(result.map_err(|payload| {
                drop(payload);
                ServiceError::PANICKED
            }));
        }
        Poll::Pending
    }
}

const KEY: ServiceKey = ServiceKey::new(0x4c41_5945, 1);
const VERSION: u64 = if cfg!(version_two) { 2 } else { 1 };

#[derive(Default)]
struct Local {
    target: Option<(Services, ResourceId, u64)>,
    count: u64,
    fail: bool,
}
impl Drop for Local {
    fn drop(&mut self) {
        if let Some((services, resource, generation)) = &self.target {
            services
                .call(*resource, KEY, 9, &generation.to_le_bytes(), &mut [])
                .unwrap();
        }
        assert!(!self.fail, "layered local destructor panic");
    }
}
static LOCAL: GenerationLocal<Local> = GenerationLocal::new(Local::default);
static BROKEN: GenerationLocal<u64> = GenerationLocal::new(|| panic!("layered initializer"));

static PAYLOAD_DROPS: AtomicU64 = AtomicU64::new(0);
struct Payload;
impl Drop for Payload {
    fn drop(&mut self) {
        PAYLOAD_DROPS.fetch_add(1, Ordering::Relaxed);
    }
}

#[no_mangle]
pub extern "C" fn layered_payload_drops() -> u64 {
    PAYLOAD_DROPS.load(Ordering::Relaxed)
}

#[cfg(native_tls)]
thread_local! { static FORBIDDEN: std::cell::Cell<u64> = const { std::cell::Cell::new(0) }; }

// No .tls section in this variant: the symbol check must still reject it.
#[cfg(external_tls)]
unsafe extern "C" {
    static _tls_index: u32;
}

#[reknot::reload_handler(KEY)]
async fn handler(ctx: RequestContext) -> Result<(), ServiceError> {
    #[cfg(native_tls)]
    FORBIDDEN.with(|value| value.set(value.get() + 1));
    #[cfg(external_tls)]
    unsafe {
        std::hint::black_box(std::ptr::read_volatile(std::ptr::addr_of!(_tls_index)));
    }
    let total = reknot::parse_total(br#"{"total":7}"#)?;
    ctx.call(3, &reknot::parse_count().to_le_bytes(), &mut [])?;
    let count = LOCAL.with_mut(&ctx, |local| {
        local.target = Some((ctx.services().clone(), ctx.resource(), ctx.generation()));
        local.fail |= ctx.input() == b"fail-drop";
        local.count += 1;
        local.count
    })?;
    ctx.call(0, &count.to_le_bytes(), &mut [])?;
    if ctx.input() == b"checks" {
        assert_eq!(BROKEN.with(&ctx, |_| ()), Err(ServiceError::PANICKED));
        assert_eq!(
            LOCAL.with(&ctx, |_| panic!("layered access")),
            Err::<(), _>(ServiceError::PANICKED)
        );
        assert_eq!(
            LOCAL.with_mut(&ctx, |_| LOCAL.with(&ctx, |_| ()))?,
            Err(ServiceError::BUSY)
        );
        assert_eq!(
            reknot::parse_total(b"invalid json"),
            Err(ServiceError::INVALID_ARGUMENT)
        );
    }
    if ctx.input() == b"panic" {
        std::panic::panic_any(Payload);
    }
    if ctx.input() == b"cancel" {
        let services = ctx.services().clone();
        let resource = ctx.resource();
        let generation = ctx.generation();
        ctx.spawn_child(move |stop| async move {
            stop.cancelled().await;
            services.call(resource, KEY, 6, &generation.to_le_bytes(), &mut [])?;
            Ok(())
        })?;
        ctx.cancelled().await;
    }
    if ctx.input() == b"external" || ctx.input() == b"adapter-panic" {
        ctx.register_activity(JoinedCallback {
            thread: None,
            started: false,
            panic_adapter: ctx.input() == b"adapter-panic",
        })?;
    }
    if ctx.input() == b"reenter" {
        ctx.call(20, &VERSION.to_le_bytes(), &mut [])?;
    }
    if ctx.input() == b"notification" {
        let notification = ctx.host_notification()?;
        let raw = notification.as_raw();
        let bytes = unsafe {
            std::slice::from_raw_parts(
                (&raw as *const reknot::HostWaker).cast::<u8>(),
                std::mem::size_of_val(&raw),
            )
        };
        ctx.call(21, bytes, &mut [])?;
    }
    if ctx.input() == b"wait" {
        let mut yielded = false;
        std::future::poll_fn(|cx| {
            if yielded {
                Poll::Ready(())
            } else {
                yielded = true;
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        })
        .await;
    }
    ctx.call(1, &(VERSION * 100 + total).to_le_bytes(), &mut [])?;
    Ok(())
}
