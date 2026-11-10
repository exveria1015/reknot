use reknot::{clean_cache, inspect_cache, CacheOptions, CacheStatus};

fn run() -> Result<bool, Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let command = args.next().unwrap_or_else(|| "inspect".into());
    if command == "--help" || command == "-h" {
        println!("reknot-cache [inspect|clean] [--apply] [--root ABSOLUTE_PATH] [--max-sessions N] [--max-entries N]\nDefault: inspect/dry-run. Only managed-v1 sessions are eligible; legacy/unknown/in-use entries are skipped.");
        return Ok(true);
    }
    if command != "inspect" && command != "clean" {
        return Err("expected inspect or clean".into());
    }
    let mut options = CacheOptions::default();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--apply" if command == "clean" => options.apply = true,
            "--root" => {
                options.root = args
                    .next()
                    .ok_or("--root requires an absolute path")?
                    .into()
            }
            "--max-sessions" => {
                options.max_sessions = args
                    .next()
                    .ok_or("--max-sessions requires a number")?
                    .parse()?
            }
            "--max-entries" => {
                options.max_entries = args
                    .next()
                    .ok_or("--max-entries requires a number")?
                    .parse()?
            }
            _ => return Err(format!("unknown/invalid argument: {arg}").into()),
        }
    }
    let report = if command == "inspect" {
        inspect_cache(&options)?
    } else {
        clean_cache(&options)?
    };
    println!(
        "{}: {}",
        if report.applied { "APPLY" } else { "READ ONLY" },
        options.root.display()
    );
    for item in &report.entries {
        println!(
            "{:?}\tfiles={} bytes={} deleted_files={} deleted_bytes={} partial={}\t{}\t{}",
            item.status,
            item.files,
            item.logical_bytes,
            item.deleted_files,
            item.deleted_bytes,
            item.partial,
            item.path.display(),
            item.reason
        );
    }
    println!(
        "files={} logical_bytes={} deleted_files={} deleted_bytes={} partial={}",
        report.files,
        report.logical_bytes,
        report.deleted_files,
        report.deleted_bytes,
        report.partial
    );
    Ok(!report.partial
        && !report
            .entries
            .iter()
            .any(|e| e.status == CacheStatus::PartialFailure))
}
fn main() {
    match run() {
        Ok(true) => (),
        Ok(false) => std::process::exit(2),
        Err(error) => {
            eprintln!("reknot-cache: {error}");
            std::process::exit(1);
        }
    }
}
