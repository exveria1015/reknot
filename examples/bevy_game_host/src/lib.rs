//! A headless Bevy game: only the host touches ECS; trusted DLLs send copied commands.
pub mod build;

use bevy::prelude::*;
use bevy_game_protocol::{Reply, Request, CLOCK, GAME, MAX_MESSAGE, MAX_QUEUE, PUBLISH, TICKS};
use reknot::{
    Checkpoint, HostResource, HostResources, PreparedHandler, RequestRouter, RequestTask,
    ResourceId, RouteError, RunStatus, ServiceError, WorkerId,
};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};
use std::thread::ThreadId;

const WORKER: WorkerId = WorkerId(1);
const MAX_JOBS: usize = 8;

#[derive(Component, Debug, Default)]
pub struct Actor {
    pub position: i64,
    // Increment when authoritative input changes the state used by a job.
    pub revision: u64,
}

#[derive(Default, Debug)]
pub struct Stats {
    pub applied_jobs: u64,
    pub old_generation: u64,
    pub missing_entity: u64,
    pub stale_revision: u64,
    pub rejected_admission: u64,
    pub completed_jobs: u64,
    pub service_calls: u64,
    pub started: Vec<(u64, u64)>,
    pub stopped: Vec<(u64, u64)>,
    pub cutovers: Vec<(u64, u64)>,
}

#[derive(Default)]
struct Mailbox {
    frame: u64,
    calls: u64,
    replies: VecDeque<Reply>,
}

struct GameService {
    owner: ThreadId,
    mailbox: Rc<RefCell<Mailbox>>,
}
impl HostResource for GameService {
    fn call(&mut self, op: u32, input: &[u8], output: &mut [u8]) -> Result<usize, ServiceError> {
        assert_eq!(self.owner, std::thread::current().id());
        let mut mailbox = self.mailbox.borrow_mut();
        mailbox.calls += 1;
        match op {
            CLOCK => {
                if output.len() < 8 {
                    return Err(ServiceError::BUFFER_TOO_SMALL);
                }
                output[..8].copy_from_slice(&mailbox.frame.to_le_bytes());
                Ok(8)
            }
            PUBLISH => {
                if input.len() > MAX_MESSAGE {
                    return Err(ServiceError::INVALID_ARGUMENT);
                }
                if mailbox.replies.len() >= MAX_QUEUE {
                    return Err(ServiceError::BUSY);
                }
                let reply =
                    serde_json::from_slice(input).map_err(|_| ServiceError::INVALID_ARGUMENT)?;
                mailbox.replies.push_back(reply);
                Ok(0)
            }
            _ => Err(ServiceError::UNSUPPORTED_OPERATION),
        }
    }
}
impl Drop for GameService {
    fn drop(&mut self) {
        assert_eq!(self.owner, std::thread::current().id());
    }
}

#[derive(Clone, Copy)]
pub enum Mode {
    Drain,
    Restart,
}

type Transition = Pin<Box<dyn Future<Output = (RequestRouter, Result<u64, RouteError>)>>>;

// Rc and the router deliberately make this !Send/!Sync. Bevy must keep it on the owner thread.
pub struct GameHost {
    owner: ThreadId,
    router: Option<RequestRouter>,
    transition: Option<Transition>,
    candidate: Option<(PreparedHandler, Mode)>,
    mailbox: Rc<RefCell<Mailbox>>,
    service: ResourceId,
    jobs: Vec<RequestTask>,
    requests: Vec<Vec<u8>>,
    actor: Entity,
    pub frame: u64,
    pub generation: u64,
    pub worker_ticks: u64,
    pub last_step: i64,
    pub stats: Stats,
    pub error: Option<String>,
    pub restart_pending_frames: u64,
    boundary_generation: u64,
    pub builds: Option<build::LiveBuild>,
}

impl GameHost {
    /// # Safety
    /// The native library must be trusted and obey RequestRouter::load's contracts.
    pub unsafe fn load(path: &Path, actor: Entity) -> Result<Self, Box<dyn std::error::Error>> {
        let owner = std::thread::current().id();
        let mailbox = Rc::new(RefCell::new(Mailbox::default()));
        let resources = HostResources::new();
        let service = resources
            .register(
                GAME,
                GameService {
                    owner,
                    mailbox: mailbox.clone(),
                },
            )
            .id();
        let mut router = unsafe { RequestRouter::load(path, resources) }?;
        router.start_worker(
            WORKER,
            service,
            &serde_json::to_vec(&Request::Worker)?,
            Checkpoint::new(TICKS, 0u64.to_le_bytes().to_vec()),
        )?;
        Ok(Self {
            owner,
            router: Some(router),
            transition: None,
            candidate: None,
            mailbox,
            service,
            jobs: Vec::new(),
            requests: Vec::new(),
            actor,
            frame: 0,
            generation: 1,
            worker_ticks: 0,
            last_step: 0,
            stats: Stats::default(),
            error: None,
            restart_pending_frames: 0,
            boundary_generation: 1,
            builds: None,
        })
    }

    /// # Safety
    /// The candidate obeys the same trusted native module contract as load.
    pub unsafe fn prepare(&mut self, path: &Path, mode: Mode) -> Result<(), RouteError> {
        self.assert_owner();
        if self.candidate.is_some() || self.transition.is_some() {
            return Err(RouteError::RestartInProgress);
        }
        // Preparation is inert; installation happens only in PreUpdate.
        let candidate = unsafe { self.router.as_ref().unwrap().prepare(path) }?;
        self.candidate = Some((candidate, mode));
        Ok(())
    }

    pub fn request_job(&mut self, entity: Entity, revision: u64, delay_frames: u64) {
        self.assert_owner();
        if self.requests.len() + self.jobs.len() >= MAX_JOBS || self.transition.is_some() {
            self.stats.rejected_admission += 1;
            return;
        }
        self.requests.push(
            serde_json::to_vec(&Request::Job {
                entity: entity.to_bits(),
                revision,
                delay_frames,
            })
            .unwrap(),
        );
    }

    pub fn restarting(&self) -> bool {
        self.transition.is_some()
    }

    pub fn stop_worker(&self) {
        self.assert_owner();
        if let Some(router) = &self.router {
            router.stop_worker(WORKER);
        }
    }

    pub fn worker_status(&self) -> Option<RunStatus> {
        self.router
            .as_ref()
            .and_then(|router| router.worker_status(WORKER))
    }

    pub fn worker_activity_count(&self) -> Option<usize> {
        self.router
            .as_ref()
            .and_then(|router| router.worker_activity_count(WORKER))
    }

    fn assert_owner(&self) {
        assert_eq!(self.owner, std::thread::current().id());
    }

    fn begin_frame(&mut self) {
        self.assert_owner();
        self.frame += 1;
        self.mailbox.borrow_mut().frame = self.frame;
        if let Some(mut builds) = self.builds.take() {
            if !self.restarting() && self.candidate.is_none() {
                match builds.poll() {
                    // This sample builds only the user's local trusted workspace crate.
                    Ok(Some(path)) => {
                        if let Err(error) = unsafe { self.prepare(&path, Mode::Restart) } {
                            eprintln!("candidate rejected: {error}");
                        }
                    }
                    Ok(None) => {}
                    Err(error) => eprintln!("build failed; game stays on current code: {error}"),
                }
            }
            self.builds = Some(builds);
        }
        let mut cx = Context::from_waker(Waker::noop());
        if let Some((candidate, mode)) = self.candidate.take() {
            let mut router = self.router.take().unwrap();
            match mode {
                Mode::Drain => {
                    match router.activate(candidate) {
                        Ok(generation) => self.committed(generation),
                        Err(error) => self.error = Some(error.to_string()),
                    }
                    self.router = Some(router);
                }
                Mode::Restart => {
                    // Move the router into an owning async block. No self-reference, lifetime
                    // extension, blocking wait or unsafe Send is needed across game frames.
                    self.transition = Some(Box::pin(async move {
                        let result = match router.restart(candidate, |_, old, schema| {
                            if old.schema != schema {
                                return Err(ServiceError::CONTRACT_MISMATCH);
                            }
                            Ok(old.clone())
                        }) {
                            Ok(restart) => restart.await,
                            Err(error) => Err(error),
                        };
                        (router, result)
                    }));
                }
            }
        }
        if let Some(transition) = &mut self.transition {
            match transition.as_mut().poll(&mut cx) {
                Poll::Ready((router, result)) => {
                    self.transition = None;
                    self.router = Some(router);
                    match result {
                        Ok(generation) => self.committed(generation),
                        Err(error) => self.error = Some(error.to_string()),
                    }
                }
                Poll::Pending => self.restart_pending_frames += 1,
            }
        }
        self.boundary_generation = self.generation;
    }

    fn committed(&mut self, generation: u64) {
        self.generation = generation;
        self.stats.cutovers.push((self.frame, generation));
    }

    fn drive(&mut self) {
        self.assert_owner();
        let mut cx = Context::from_waker(Waker::noop());
        if let Some(router) = &mut self.router {
            for request in self.requests.drain(..) {
                match router.dispatch(self.service, &request) {
                    Ok(task) => self.jobs.push(task),
                    Err(_) => self.stats.rejected_admission += 1,
                }
            }
            let _ = router.poll_workers(&mut cx);
            if router.worker_status(WORKER) == Some(RunStatus::Error) {
                self.error = Some("worker failed".into());
            }
        } else {
            self.stats.rejected_admission += self.requests.len() as u64;
            self.requests.clear();
        }
        // Ordinary tasks are owned here, including roots stopped by Restart.
        // Poll them even while the transition borrows the router.
        self.jobs
            .retain_mut(|task| match Pin::new(task).poll(&mut cx) {
                Poll::Pending => true,
                Poll::Ready(status) => {
                    self.stats.completed_jobs += 1;
                    if status != RunStatus::Done {
                        self.error = Some("job failed".into());
                    }
                    false
                }
            });
        assert_eq!(self.generation, self.boundary_generation);
    }
}
impl Drop for GameHost {
    fn drop(&mut self) {
        self.assert_owner();
    }
}

/// Creates a real Bevy App with its normal frame schedules, without GPU/window dependencies.
///
/// # Safety
/// The native library must obey GameHost::load's contract.
pub unsafe fn game_app(path: &Path) -> Result<(App, Entity), Box<dyn std::error::Error>> {
    let mut app = App::new();
    app.add_plugins(
        MinimalPlugins.set(bevy::app::ScheduleRunnerPlugin::run_loop(
            std::time::Duration::from_secs_f64(1.0 / 60.0),
        )),
    );
    let actor = app.world_mut().spawn(Actor::default()).id();
    app.insert_non_send(unsafe { GameHost::load(path, actor) }?);
    app.add_systems(PreUpdate, begin_frame)
        .add_systems(Update, drive)
        .add_systems(PostUpdate, apply_replies);
    Ok((app, actor))
}

fn begin_frame(mut host: NonSendMut<GameHost>) {
    host.begin_frame();
}
fn drive(mut host: NonSendMut<GameHost>) {
    host.drive();
}

fn apply_replies(mut host: NonSendMut<GameHost>, mut actors: Query<&mut Actor>) {
    host.assert_owner();
    assert_eq!(host.generation, host.boundary_generation);
    let replies = std::mem::take(&mut host.mailbox.borrow_mut().replies);
    let calls = host.mailbox.borrow().calls;
    host.stats.service_calls = calls;
    for reply in replies {
        match reply {
            Reply::Started { generation, ticks } => host.stats.started.push((generation, ticks)),
            Reply::Stopped { generation, ticks } => host.stats.stopped.push((generation, ticks)),
            Reply::Step {
                generation,
                ticks,
                delta,
            } => {
                // During Drain the registered worker intentionally stays on old code.
                // Its results stop being authoritative as soon as the generation changes.
                if generation != host.generation {
                    continue;
                }
                if let Ok(mut actor) = actors.get_mut(host.actor) {
                    actor.position += delta;
                    host.worker_ticks = ticks;
                    host.last_step = delta;
                }
            }
            Reply::Job {
                generation,
                entity,
                revision,
                delta,
            } => {
                if generation != host.generation {
                    host.stats.old_generation += 1;
                    continue;
                }
                // Entity::to_bits includes Bevy's incarnation, not only its slot index.
                let Ok(mut actor) = actors.get_mut(Entity::from_bits(entity)) else {
                    host.stats.missing_entity += 1;
                    continue;
                };
                if actor.revision != revision {
                    host.stats.stale_revision += 1;
                    continue;
                }
                actor.position += delta;
                actor.revision += 1;
                host.stats.applied_jobs += 1;
            }
        }
    }
}

/// Executable assertions against two independently built native logic variants.
///
/// # Safety
/// Both paths must contain the trusted sample logic built for this host ABI.
pub unsafe fn verify(slow: &Path, fast: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let (mut app, actor) = unsafe { game_app(slow) }?;
    app.update();
    assert_eq!(app.world().get::<Actor>(actor).unwrap().position, 1);
    let host = app.world().non_send::<GameHost>();
    assert_eq!(host.stats.started, [(1, 0)]);
    assert_eq!(host.worker_activity_count(), Some(1));

    // A delayed old-generation result finishes after Drain, while new jobs use fast code.
    app.world_mut()
        .non_send_mut::<GameHost>()
        .request_job(actor, 0, 3);
    app.update();
    let before = app.world().get::<Actor>(actor).unwrap().position;
    unsafe {
        app.world_mut()
            .non_send_mut::<GameHost>()
            .prepare(fast, Mode::Drain)
    }?;
    assert_eq!(app.world().non_send::<GameHost>().generation, 1);
    app.update();
    assert_eq!(app.world().non_send::<GameHost>().generation, 2);
    assert_eq!(app.world().get::<Actor>(actor).unwrap().position, before);
    app.world_mut()
        .non_send_mut::<GameHost>()
        .request_job(actor, 0, 0);
    app.update();
    app.update();
    let host = app.world().non_send::<GameHost>();
    assert_eq!(host.stats.old_generation, 1);
    assert_eq!(host.stats.applied_jobs, 1);
    assert_eq!(host.stats.completed_jobs, 2);
    assert_eq!(
        app.world().get::<Actor>(actor).unwrap().position,
        before + 30
    );

    // Same-generation results still need revision and entity-incarnation validation.
    app.world_mut()
        .non_send_mut::<GameHost>()
        .request_job(actor, 0, 0);
    let deleted = app.world_mut().spawn(Actor::default()).id();
    app.world_mut()
        .non_send_mut::<GameHost>()
        .request_job(deleted, 0, 2);
    app.update();
    // Default allocation need not reuse a just-freed slot. Bevy's explicit lifecycle
    // API gives us the next incarnation without returning the slot to its allocator.
    let replacement = app.world_mut().despawn_no_free(deleted).unwrap();
    app.world_mut()
        .spawn_at(replacement, Actor::default())
        .unwrap();
    assert_eq!(deleted.index(), replacement.index());
    assert_ne!(deleted, replacement);
    app.update();
    app.update();
    assert_eq!(app.world().non_send::<GameHost>().stats.stale_revision, 1);
    assert_eq!(app.world().non_send::<GameHost>().stats.missing_entity, 1);
    assert_eq!(app.world().get::<Actor>(replacement).unwrap().position, 0);

    // Restart must stop the old Drain worker AND an externally owned ordinary task.
    app.world_mut()
        .non_send_mut::<GameHost>()
        .request_job(actor, 1, 100);
    app.update();
    unsafe {
        app.world_mut()
            .non_send_mut::<GameHost>()
            .prepare(fast, Mode::Restart)
    }?;
    app.update();
    assert!(app.world().non_send::<GameHost>().restarting());
    assert_eq!(app.world().non_send::<GameHost>().generation, 2);
    app.world_mut()
        .non_send_mut::<GameHost>()
        .request_job(actor, 1, 0);
    app.update();
    assert!(app.world().non_send::<GameHost>().restarting());
    app.update();
    let host = app.world().non_send::<GameHost>();
    assert!(!host.restarting());
    assert_eq!(host.generation, 3);
    assert!(host.restart_pending_frames >= 2);
    assert_eq!(host.stats.rejected_admission, 1);
    assert_eq!(host.stats.applied_jobs, 1); // cancelled job was neither applied nor replayed
    assert_eq!(host.stats.completed_jobs, 5);
    let stopped = host.stats.stopped[0];
    assert_eq!(stopped.0, 1);
    assert_eq!(host.stats.started[1], (3, stopped.1));
    assert_eq!(host.worker_ticks, stopped.1 + 1);
    assert_eq!(host.last_step, 3);
    assert_eq!(host.worker_activity_count(), Some(1));
    assert!(host.stats.service_calls > 0);
    assert!(host.error.is_none(), "{:?}", host.error);

    // A failed load cannot change admission or the active generation.
    assert!(unsafe {
        app.world_mut()
            .non_send_mut::<GameHost>()
            .prepare(&fast.with_extension("missing"), Mode::Restart)
    }
    .is_err());
    app.world_mut()
        .non_send_mut::<GameHost>()
        .request_job(actor, 1, 0);
    app.update();
    assert_eq!(app.world().non_send::<GameHost>().generation, 3);
    assert_eq!(app.world().non_send::<GameHost>().stats.applied_jobs, 2);
    app.world().non_send::<GameHost>().stop_worker();
    for _ in 0..4 {
        app.update();
    }
    assert_eq!(
        app.world().non_send::<GameHost>().worker_status(),
        Some(RunStatus::Done)
    );
    assert_eq!(
        app.world().non_send::<GameHost>().worker_activity_count(),
        Some(0)
    );
    let host = app.world().non_send::<GameHost>();
    let frames = host.frame;
    let cutovers = host.stats.cutovers.clone();
    drop(app); // Includes affinity assertions in GameHost and the retained host service.
    println!("PASS: Drain rejects old jobs; revision/entity checks; multi-frame Restart restores checkpoint; failed load preserves admission; owner-thread calls/poll/drop; frames={frames} cutovers={cutovers:?}");
    Ok(())
}
