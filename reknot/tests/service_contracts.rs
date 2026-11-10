use reknot::*;
use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

const PRIMARY: ServiceKey = ServiceKey::new(901, 1);
const COUNT: ServiceKey = ServiceKey::new(902, 1);
const FORMAT: ServiceKey = ServiceKey::new(903, 1);
const CONFIG: ServiceKey = ServiceKey::new(904, 1);
const STATE: ServiceKey = ServiceKey::new(905, 1);
const REQUIREMENTS: &[ServiceRequirement] = &[
    ServiceRequirement::new(1, COUNT),
    ServiceRequirement::new(2, FORMAT),
];

#[derive(Clone)]
struct Config(u32);
impl InitPayload for Config {
    const SCHEMA: ServiceKey = CONFIG;
    fn encode(&self) -> Result<Vec<u8>, ServiceError> {
        Ok(self.0.to_le_bytes().to_vec())
    }
    fn decode(bytes: &[u8]) -> Result<Self, ServiceError> {
        let n = u32::from_le_bytes(
            bytes
                .try_into()
                .map_err(|_| ServiceError::INVALID_ARGUMENT)?,
        );
        assert_ne!(n, 13, "decoder panic");
        if n > 100 {
            return Err(ServiceError::INVALID_ARGUMENT);
        }
        Ok(Self(n))
    }
}
struct Counter(Rc<RefCell<u32>>);
impl HostResource for Counter {
    fn call(&mut self, _: u32, input: &[u8], output: &mut [u8]) -> Result<usize, ServiceError> {
        let delta = u32::from_le_bytes(
            input
                .try_into()
                .map_err(|_| ServiceError::INVALID_ARGUMENT)?,
        );
        *self.0.borrow_mut() += delta;
        output[..4].copy_from_slice(&self.0.borrow().to_le_bytes());
        Ok(4)
    }
}
struct Formatter;
impl HostResource for Formatter {
    fn start_async(
        &mut self,
        _: u32,
        input: &[u8],
        _: CancellationToken,
    ) -> Result<HostServiceFuture, ServiceError> {
        let bytes = input.to_vec();
        Ok(Box::pin(async move { Ok(bytes) }))
    }
}
#[derive(Default)]
struct Session(Vec<Vec<u8>>);
impl HostResource for Session {
    fn call(&mut self, _: u32, input: &[u8], _: &mut [u8]) -> Result<usize, ServiceError> {
        self.0.push(input.to_vec());
        Ok(0)
    }
}
#[reload_handler(PRIMARY, requires = REQUIREMENTS, init = Config, state = STATE)]
async fn root(ctx: RequestContext) -> Result<(), ServiceError> {
    let mut first = true;
    std::future::poll_fn(|cx| {
        if std::mem::take(&mut first) {
            cx.waker().wake_by_ref();
            Poll::Pending
        } else {
            Poll::Ready(())
        }
    })
    .await;
    let config = ctx.initial::<Config>()?;
    let mut count = [0; 4];
    ctx.call_service(1, 0, &config.0.to_le_bytes(), &mut count)?;
    let bytes = ctx.call_service_async(2, 0, &count, 4)?.await?;
    ctx.call(0, &bytes, &mut [])?;
    if ctx.checkpoint().is_some() {
        ctx.cancelled().await;
        ctx.save_checkpoint(STATE, &[1])?;
    }
    Ok(())
}
fn api() -> HandlerApi {
    unsafe { *hot_handler_api() }
}
fn cx() -> Context<'static> {
    Context::from_waker(Waker::noop())
}
fn poll<F: Future + Unpin>(future: &mut F) -> Poll<F::Output> {
    Pin::new(future).poll(&mut cx())
}
fn done(task: &mut RequestTask) -> RunStatus {
    for _ in 0..8 {
        if let Poll::Ready(status) = poll(task) {
            return status;
        }
    }
    panic!("task did not complete")
}
fn environment(counter: ResourceId, formatter: ResourceId, step: u32) -> HandlerEnvironment {
    HandlerEnvironment::default()
        .bind(1, counter)
        .bind(2, formatter)
        .with_initial(InitData::from_value(&Config(step)).unwrap())
}
fn router(host: HostResources, env: HandlerEnvironment) -> RequestRouter {
    unsafe { RequestRouter::from_api_with_environment(api(), host, env, RouterOptions::default()) }
        .unwrap()
}

#[test]
fn drain_pins_old_bindings_and_initial_data_and_allows_async_secondary_service() {
    let host = HostResources::new();
    let primary = host.register(PRIMARY, Session::default());
    let a = Rc::new(RefCell::new(0));
    let b = Rc::new(RefCell::new(0));
    let first = host.register(COUNT, Counter(a.clone()));
    let second = host.register(COUNT, Counter(b.clone()));
    let formatter = host.register(FORMAT, Formatter);
    let mut router = router(host.clone(), environment(first.id(), formatter.id(), 2));
    let mut old = router.dispatch(primary.id(), b"old").unwrap();
    assert!(poll(&mut old).is_pending());
    let new = unsafe {
        router.prepare_api_with_environment(api(), environment(second.id(), formatter.id(), 5))
    }
    .unwrap();
    assert_eq!(router.activate(new).unwrap(), 2);
    let mut new = router.dispatch(primary.id(), b"new").unwrap();
    drop(router);
    drop(host);
    drop(first);
    drop(second);
    drop(formatter);
    assert_eq!(done(&mut old), RunStatus::Done);
    assert_eq!(done(&mut new), RunStatus::Done);
    assert_eq!(*a.borrow(), 2);
    assert_eq!(*b.borrow(), 5);
    assert_eq!(
        primary.try_borrow().unwrap().0,
        [2u32.to_le_bytes(), 5u32.to_le_bytes()]
    );
}

#[test]
fn missing_wrong_version_and_malformed_initial_data_reject_before_any_body_runs() {
    let host = HostResources::new();
    let primary = host.register(PRIMARY, Session::default());
    let count = Rc::new(RefCell::new(0));
    let counter = host.register(COUNT, Counter(count.clone()));
    let wrong = host.register(ServiceKey::new(COUNT.id, 2), Counter(count.clone()));
    let formatter = host.register(FORMAT, Formatter);
    let mut router = router(host.clone(), environment(counter.id(), formatter.id(), 2));
    let missing = HandlerEnvironment::default()
        .bind(1, counter.id())
        .with_initial(InitData::from_value(&Config(1)).unwrap());
    assert!(matches!(
        unsafe { router.prepare_api_with_environment(api(), missing) },
        Err(RouteError::MissingService { binding: 2, .. })
    ));
    assert!(matches!(
        unsafe {
            router.prepare_api_with_environment(api(), environment(wrong.id(), formatter.id(), 1))
        },
        Err(RouteError::ContractMismatch { .. })
    ));
    let mut wrong_schema = environment(counter.id(), formatter.id(), 1);
    wrong_schema.initial.as_mut().unwrap().schema.version += 1;
    assert!(matches!(
        unsafe { router.prepare_api_with_environment(api(), wrong_schema) },
        Err(RouteError::InitSchemaMismatch { .. })
    ));
    let mut malformed = environment(counter.id(), formatter.id(), 1);
    malformed.initial.as_mut().unwrap().bytes = vec![0, 1];
    assert!(matches!(
        unsafe { router.prepare_api_with_environment(api(), malformed) },
        Err(RouteError::InitData(ServiceError::INVALID_ARGUMENT))
    ));
    let mut bad_value = environment(counter.id(), formatter.id(), 1);
    bad_value.initial.as_mut().unwrap().bytes = 101u32.to_le_bytes().to_vec();
    assert!(matches!(
        unsafe { router.prepare_api_with_environment(api(), bad_value) },
        Err(RouteError::InitData(ServiceError::INVALID_ARGUMENT))
    ));
    assert_eq!(*count.borrow(), 0);
    assert!(primary.try_borrow().unwrap().0.is_empty());
    assert!(matches!(
        unsafe {
            router
                .prepare_api_with_environment(api(), environment(counter.id(), formatter.id(), 13))
        },
        Err(RouteError::InitData(ServiceError::PANICKED))
    ));
    assert!(router.is_accepting());
    assert_eq!(router.generation(), 1);
    let mut good = router.dispatch(primary.id(), b"still works").unwrap();
    assert_eq!(done(&mut good), RunStatus::Done);
    assert_eq!(*count.borrow(), 2);
    // The convenience prepare reuses the existing environment.
    let candidate = unsafe { router.prepare_api(api()) }.unwrap();
    router.activate(candidate).unwrap();
}

#[test]
fn revoke_after_prepare_or_admission_does_not_silently_rebind_to_a_replacement() {
    let host = HostResources::new();
    let primary = host.register(PRIMARY, Session::default());
    let count = Rc::new(RefCell::new(0));
    let counter = host.register(COUNT, Counter(count.clone()));
    let formatter = host.register(FORMAT, Formatter);
    let mut router = router(host.clone(), environment(counter.id(), formatter.id(), 1));
    let next = unsafe { router.prepare_api(api()) }.unwrap();
    let mut pending = router.dispatch(primary.id(), b"old").unwrap();
    assert!(host.revoke(counter.id()));
    let replacement = host.register(COUNT, Counter(count.clone()));
    assert_ne!(counter.id(), replacement.id());
    assert!(matches!(
        router.activate(next),
        Err(RouteError::Resource(ServiceError::REVOKED))
    ));
    assert!(matches!(
        router.dispatch(primary.id(), b"bad"),
        Err(RouteError::Resource(ServiceError::REVOKED))
    ));
    assert_eq!(done(&mut pending), RunStatus::Error);
    assert_eq!(*count.borrow(), 0);
    let candidate = unsafe {
        router.prepare_api_with_environment(api(), environment(replacement.id(), formatter.id(), 3))
    }
    .unwrap();
    router.activate(candidate).unwrap();
    let mut task = router.dispatch(primary.id(), b"ok").unwrap();
    assert_eq!(done(&mut task), RunStatus::Done);
    assert_eq!(*count.borrow(), 3);
}

#[test]
fn restart_validates_before_stop_and_checks_again_after_delayed_shutdown() {
    let host = HostResources::new();
    let primary = host.register(PRIMARY, Session::default());
    let count = Rc::new(RefCell::new(0));
    let counter = host.register(COUNT, Counter(count.clone()));
    let formatter = host.register(FORMAT, Formatter);
    let mut router = router(host.clone(), environment(counter.id(), formatter.id(), 1));
    router
        .start_worker(
            WorkerId(1),
            primary.id(),
            b"worker",
            Checkpoint::new(STATE, vec![0]),
        )
        .unwrap();
    for _ in 0..4 {
        let _ = router.poll_workers(&mut cx());
    }
    let candidate = unsafe { router.prepare_api(api()) }.unwrap();
    host.revoke(formatter.id());
    assert!(matches!(
        router.restart(candidate, |_, state, _| Ok(state.clone())),
        Err(RouteError::Resource(ServiceError::REVOKED))
    ));
    assert!(router.is_accepting());
    assert_eq!(router.worker_status(WorkerId(1)), None);
    let replacement = host.register(FORMAT, Formatter);
    let candidate = unsafe {
        router.prepare_api_with_environment(api(), environment(counter.id(), replacement.id(), 2))
    }
    .unwrap();
    let mut restart = router
        .restart(candidate, |_, state, _| Ok(state.clone()))
        .unwrap();
    // Revocation after shutdown begins must also prevent installing the candidate.
    host.revoke(replacement.id());
    assert!(matches!(
        poll(&mut restart),
        Poll::Ready(Err(RouteError::Resource(ServiceError::REVOKED)))
    ));
    drop(restart);
    assert_eq!(router.generation(), 1);
    assert!(!router.is_accepting());
}

#[test]
fn bounded_unique_manifest_and_environment_and_missing_initial_are_enforced() {
    let host = HostResources::new();
    let c = host.register(COUNT, Counter(Rc::default()));
    let f = host.register(FORMAT, Formatter);
    let env = environment(c.id(), f.id(), 1);
    let mut invalid = api();
    invalid.required_count = (MAX_SERVICE_REQUIREMENTS + 1) as u32;
    assert!(matches!(
        unsafe {
            RequestRouter::from_api_with_environment(
                invalid,
                host.clone(),
                env.clone(),
                RouterOptions::default(),
            )
        },
        Err(RouteError::InvalidApi)
    ));
    invalid = api();
    invalid.required_services[1].binding = 1;
    assert!(matches!(
        unsafe {
            RequestRouter::from_api_with_environment(
                invalid,
                host.clone(),
                env.clone(),
                RouterOptions::default(),
            )
        },
        Err(RouteError::InvalidApi)
    ));
    let duplicate = env.clone().bind(1, c.id());
    assert!(matches!(
        unsafe {
            RequestRouter::from_api_with_environment(
                api(),
                host.clone(),
                duplicate,
                RouterOptions::default(),
            )
        },
        Err(RouteError::InvalidEnvironment)
    ));
    assert!(matches!(
        unsafe {
            RequestRouter::from_api_with_environment(
                api(),
                host.clone(),
                HandlerEnvironment::default(),
                RouterOptions::default(),
            )
        },
        Err(RouteError::InitSchemaMismatch { .. })
    ));
    let mut oversized = env;
    oversized.initial.as_mut().unwrap().bytes = vec![0; MAX_INIT_BYTES + 1];
    assert!(matches!(
        unsafe {
            RequestRouter::from_api_with_environment(
                api(),
                host,
                oversized,
                RouterOptions::default(),
            )
        },
        Err(RouteError::InitData(ServiceError::INVALID_ARGUMENT))
    ));
}
