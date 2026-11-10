//! Create a standalone starter without modifying an existing directory.
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

const HELP: &str = "reknot-new PATH [--name NAME] [--sdk-path REKNOT_CRATE]\n\nCreates a new host/logic/protocol workspace. PATH must not exist.\nNAME defaults to the directory name (lowercase letters, digits, '-' or '_').\nUses this SDK source checkout by default; --sdk-path overrides its location.\nDoes not run Cargo, initialize Git, or overwrite existing files.\nThen: cd PATH; cargo run\n";
struct Options {
    destination: PathBuf,
    name: String,
    sdk: PathBuf,
}
fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Option<Options>, String> {
    let mut args = args.into_iter();
    let first = args.next().ok_or_else(|| HELP.to_owned())?;
    if first == "--help" || first == "-h" {
        return Ok(None);
    }
    if first.to_string_lossy().starts_with('-') {
        return Err("provide a destination directory first; see reknot-new --help".into());
    }
    let destination = PathBuf::from(first);
    let mut name = None;
    let mut sdk = None;
    while let Some(arg) = args.next() {
        if arg == "--name" && name.is_none() {
            name = Some(
                args.next()
                    .ok_or("--name needs a package name")?
                    .into_string()
                    .map_err(|_| "package name must be UTF-8")?,
            );
        } else if arg == "--sdk-path" && sdk.is_none() {
            sdk = Some(PathBuf::from(
                args.next()
                    .ok_or("--sdk-path needs the reknot crate directory")?,
            ));
        } else {
            return Err(format!(
                "unknown or repeated option: {}",
                arg.to_string_lossy()
            ));
        }
    }
    let name = name
        .or_else(|| destination.file_name()?.to_str().map(str::to_owned))
        .ok_or("use --name with a lowercase ASCII package name")?;
    if name.is_empty()
        || name.len() > 48
        || !name.as_bytes()[0].is_ascii_lowercase()
        || !name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-' || byte == b'_'
        })
    {
        return Err("name must start with a lowercase letter and contain 1..=48 lowercase letters, digits, '-' or '_'; use --name for other directory names".into());
    }
    let sdk = sdk.unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")));
    let sdk = sdk.canonicalize().map_err(|error| {
        format!(
            "cannot find SDK source at {}: {error}; use --sdk-path",
            sdk.display()
        )
    })?;
    if !sdk.join("Cargo.toml").is_file() || !sdk.join("src/lib.rs").is_file() {
        return Err("--sdk-path must point to the reknot crate directory (containing Cargo.toml and src/lib.rs)".into());
    }
    Ok(Some(Options {
        destination,
        name,
        sdk,
    }))
}
fn dependency(path: &Path) -> Result<String, String> {
    let path = path
        .to_str()
        .ok_or("SDK source path must be UTF-8 for a Cargo manifest")?;
    #[cfg(windows)]
    let path = {
        let normalized = if let Some(path) = path.strip_prefix("\\\\?\\UNC\\") {
            format!("//{path}")
        } else {
            path.strip_prefix("\\\\?\\").unwrap_or(path).to_owned()
        };
        normalized.replace('\\', "/")
    };
    // JSON string escaping also produces a valid TOML basic string here. This
    // text is written to a manifest; it is never interpolated into a shell.
    let quoted = serde_json::to_string(&path).map_err(|error| error.to_string())?;
    Ok(format!("{{ path = {quoted}, version = \"={}\", default-features = false, features = [\"typed\"] }}", env!("CARGO_PKG_VERSION")))
}
fn create(options: Options) -> Result<PathBuf, String> {
    let sdk = dependency(&options.sdk)?;
    let crate_name = options.name.replace('-', "_");
    let render = |text: &str| {
        text.replace("{{package}}", &options.name)
            .replace("{{crate}}", &crate_name)
            .replace("{{msrv}}", env!("CARGO_PKG_RUST_VERSION"))
            .replace("{{sdk}}", &sdk)
    };
    let files = [
        ("Cargo.toml", include_str!("../templates/workspace.toml.in")),
        (
            "protocol/Cargo.toml",
            include_str!("../templates/protocol.toml.in"),
        ),
        (
            "protocol/src/lib.rs",
            include_str!("../templates/protocol.rs.in"),
        ),
        (
            "logic/Cargo.toml",
            include_str!("../templates/logic.toml.in"),
        ),
        ("logic/src/lib.rs", include_str!("../templates/logic.rs.in")),
        ("host/Cargo.toml", include_str!("../templates/host.toml.in")),
        ("host/src/main.rs", include_str!("../templates/host.rs.in")),
        ("README.md", include_str!("../templates/README.md.in")),
        (".gitignore", "/target/\n"),
    ];
    let files = files
        .into_iter()
        .map(|(path, text)| (path, render(text)))
        .collect::<Vec<_>>();
    // Claim the destination before any write. Even an existing empty directory
    // is refused. No recursive cleanup can erase a concurrent user's files.
    fs::create_dir(&options.destination).map_err(|error| format!("cannot create {}: {error}; choose a new directory (existing directories are never replaced)", options.destination.display()))?;
    let write = || -> std::io::Result<()> {
        for directory in ["protocol/src", "logic/src", "host/src"] {
            fs::create_dir_all(options.destination.join(directory))?;
        }
        for (relative, contents) in files {
            let mut file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(options.destination.join(relative))?;
            file.write_all(contents.as_bytes())?;
        }
        Ok(())
    };
    write().map_err(|error| format!("generation stopped in {}: {error}; inspect the partial project before retrying with a new directory", options.destination.display()))?;
    Ok(options.destination)
}
fn main() {
    let result = parse(std::env::args_os().skip(1)).and_then(|options| match options {
        Some(options) => create(options).map(|path| {
            println!("Created {}\n\nOpen that directory and run: cargo run\nEdit logic/src/lib.rs and save to reload. See README.md for setup and static mode.", path.display());
        }),
        None => { print!("{HELP}"); Ok(()) }
    });
    if let Err(error) = result {
        eprintln!("reknot-new: {error}");
        std::process::exit(1);
    }
}
