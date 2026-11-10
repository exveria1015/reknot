use reknot::{HostResource, HostResources, RequestRouter, RunStatus, ServiceError, ServiceKey};
use std::future::Future;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

#[cfg(not(any(feature = "reload", feature = "static-logic")))]
compile_error!("enable reload or static-logic");

const LINE_SESSION: ServiceKey = ServiceKey::new(0x4c49_4e45, 1);

struct Session {
    stream: TcpStream,
    outbox: Vec<Vec<u8>>,
}
impl HostResource for Session {
    fn call(&mut self, operation: u32, input: &[u8], _: &mut [u8]) -> Result<usize, ServiceError> {
        if operation != 0 {
            return Err(ServiceError::UNSUPPORTED_OPERATION);
        }
        if self.outbox.len() >= 16 || input.len() > 65_536 {
            return Err(ServiceError::BUSY);
        }
        self.outbox.push(input.to_vec());
        Ok(0)
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let host = HostResources::new();
    #[cfg(feature = "reload")]
    let (mut router, mut watcher, logic, target, library) = {
        let logic = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../service_logic");
        let target = reknot::development::default_target_dir(&logic);
        if !reknot::development::build_logic("service_logic", &logic, &target)?.success() {
            return Err("initial logic build failed".into());
        }
        let library = reknot::development::library_path_in(&target, "service_logic");
        // SAFETY: the local SDK handler and all manually edited replacements are
        // trusted code obeying the handler's copy/retain and task ownership contracts.
        let router = unsafe { RequestRouter::load(&library, host.clone()) }?;
        let watcher = reknot::development::start_source_watcher(&logic);
        (router, watcher, logic, target, library)
    };
    #[cfg(all(feature = "static-logic", not(feature = "reload")))]
    // SAFETY: generated callbacks are statically linked and own their request data.
    let router = unsafe { RequestRouter::from_api(service_logic::handler_api(), host.clone()) }?;
    let address = std::env::var("REKNOT_ADDR").unwrap_or_else(|_| "127.0.0.1:4000".into());
    let listener = TcpListener::bind(address)?;
    println!(
        "Listening on {}. Send newline-terminated text.",
        listener.local_addr()?
    );

    // Minimal single-connection example: transport stays entirely in the host.
    let (stream, _) = listener.accept()?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let session = host.register(
        LINE_SESSION,
        Session {
            stream,
            outbox: Vec::new(),
        },
    );
    loop {
        let mut line = Vec::new();
        // Bounded framing: a missing newline cannot allocate indefinitely.
        use std::io::Read;
        let mut bounded = reader.by_ref().take(65_537);
        if bounded.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        if line.len() > 65_536 {
            return Err("line exceeds 64 KiB".into());
        }
        let mut task = None;
        loop {
            #[cfg(feature = "reload")]
            if reknot::development::source_changed(&mut watcher) {
                match reknot::development::build_logic("service_logic", &logic, &target) {
                    Ok(status) if status.success() => {
                        // Same trusted-source contract as the initial load.
                        let prepared = unsafe { router.prepare(&library) };
                        match prepared.and_then(|candidate| router.activate(candidate)) {
                            Ok(id) => println!("New admissions use generation {id}"),
                            Err(error) => eprintln!("Keeping old admission: {error}"),
                        }
                    }
                    _ => eprintln!("Build failed; keeping old admission"),
                }
            }
            if task.is_none() {
                let admitted = router.dispatch(session.id(), &line)?;
                println!("Admitted request in generation {}", admitted.generation());
                task = Some(admitted);
            }
            let status =
                Pin::new(task.as_mut().unwrap()).poll(&mut Context::from_waker(Waker::noop()));
            match status {
                Poll::Pending => std::thread::sleep(std::time::Duration::from_millis(10)),
                Poll::Ready(status) => {
                    if status != RunStatus::Done {
                        eprintln!("Request failed: {status:?}");
                    }
                    break;
                }
            }
        }
        let mut session = session.try_borrow_mut()?;
        for reply in std::mem::take(&mut session.outbox) {
            session.stream.write_all(&reply)?;
        }
    }
    Ok(())
}
