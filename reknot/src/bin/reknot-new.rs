//! Create a standalone starter without modifying an existing directory.
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

const HELP: &str = "reknot-new PATH [--name NAME] [--sdk-version VERSION | --sdk-path REKNOT_CRATE | --sdk-rev COMMIT [--sdk-git URL]]\n\nCreates a new host/logic/protocol workspace. PATH must not exist.\nDefaults to this CLI's exact SDK version from crates.io, without a source checkout.\nFor an unpublished source candidate, use --sdk-rev with its full Git commit SHA.\n--sdk-git defaults to the SDK repository; --sdk-path selects a local development checkout.\nDoes not run Cargo, initialize Git, or overwrite existing files.\nThen: cd PATH; cargo run\n";

enum SdkSource {
    Registry(String),
    Git { url: String, revision: String },
    Path(PathBuf),
}
struct Options {
    destination: PathBuf,
    name: String,
    sdk: SdkSource,
}
fn exact_version(version: &str) -> bool {
    let (version, build) = version
        .split_once('+')
        .map_or((version, None), |(v, b)| (v, Some(b)));
    let (core, pre) = version
        .split_once('-')
        .map_or((version, None), |(v, p)| (v, Some(p)));
    let parts: Vec<_> = core.split('.').collect();
    let numeric = |p: &str| {
        !p.is_empty()
            && p.bytes().all(|b| b.is_ascii_digit())
            && (p.len() == 1 || !p.starts_with('0'))
    };
    let identifiers = |text: &str, prerelease: bool| {
        text.split('.').all(|p| {
            !p.is_empty()
                && p.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                && (!prerelease || !p.bytes().all(|b| b.is_ascii_digit()) || numeric(p))
        })
    };
    parts.len() == 3
        && parts.iter().all(|p| numeric(p) && p.parse::<u64>().is_ok())
        && pre.is_none_or(|p| identifiers(p, true))
        && build.is_none_or(|b| identifiers(b, false))
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
    let mut sdk_path = None;
    let mut version = None;
    let mut git = None;
    let mut revision = None;
    while let Some(arg) = args.next() {
        let value = args
            .next()
            .ok_or_else(|| format!("{} needs a value", arg.to_string_lossy()))?;
        match arg.to_str() {
            Some("--name") if name.is_none() => {
                name = Some(
                    value
                        .into_string()
                        .map_err(|_| "package name must be UTF-8")?,
                )
            }
            Some("--sdk-path") if sdk_path.is_none() => sdk_path = Some(PathBuf::from(value)),
            Some("--sdk-version") if version.is_none() => {
                version = Some(value.into_string().map_err(|_| "version must be UTF-8")?)
            }
            Some("--sdk-git") if git.is_none() => {
                git = Some(value.into_string().map_err(|_| "Git URL must be UTF-8")?)
            }
            Some("--sdk-rev") if revision.is_none() => {
                revision = Some(value.into_string().map_err(|_| "revision must be UTF-8")?)
            }
            _ => {
                return Err(format!(
                    "unknown or repeated option: {}",
                    arg.to_string_lossy()
                ))
            }
        }
    }
    let name = name
        .or_else(|| destination.file_name()?.to_str().map(str::to_owned))
        .ok_or("use --name with a lowercase ASCII package name")?;
    if name.is_empty()
        || name.len() > 48
        || !name.as_bytes()[0].is_ascii_lowercase()
        || !name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
    {
        return Err("name must start with a lowercase letter and contain 1..=48 lowercase letters, digits, '-' or '_'; use --name for other directory names".into());
    }
    if usize::from(sdk_path.is_some())
        + usize::from(version.is_some())
        + usize::from(git.is_some() || revision.is_some())
        > 1
    {
        return Err(
            "choose exactly one SDK source: --sdk-path, --sdk-version, or --sdk-rev/--sdk-git"
                .into(),
        );
    }
    let sdk = if let Some(path) = sdk_path {
        let path = path
            .canonicalize()
            .map_err(|e| format!("cannot find SDK source at {}: {e}", path.display()))?;
        if !path.join("Cargo.toml").is_file() || !path.join("src/lib.rs").is_file() {
            return Err("--sdk-path must point to the reknot crate directory (containing Cargo.toml and src/lib.rs)".into());
        }
        SdkSource::Path(path)
    } else if git.is_some() || revision.is_some() {
        let revision = revision.ok_or("--sdk-git requires --sdk-rev with a full commit SHA")?;
        if ![40, 64].contains(&revision.len()) || !revision.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err("--sdk-rev must be a full 40- or 64-digit hexadecimal commit SHA".into());
        }
        let url = git.unwrap_or_else(|| env!("CARGO_PKG_REPOSITORY").into());
        if url.is_empty() || url.chars().any(char::is_control) {
            return Err("invalid Git URL".into());
        }
        SdkSource::Git { url, revision }
    } else {
        let version = version.unwrap_or_else(|| env!("CARGO_PKG_VERSION").into());
        if !exact_version(&version) {
            return Err("--sdk-version must be an exact version, such as 0.1.0-rc.2".into());
        }
        SdkSource::Registry(version)
    };
    Ok(Some(Options {
        destination,
        name,
        sdk,
    }))
}
fn sdk_dependency(source: &SdkSource) -> Result<String, String> {
    let quote = |value: &str| serde_json::to_string(value).map_err(|e| e.to_string());
    match source {
        SdkSource::Path(path) => dependency(path),
        SdkSource::Registry(version) => Ok(format!(
            "{{ version = {}, default-features = false, features = [\"typed\"] }}",
            quote(&format!("={version}"))?
        )),
        SdkSource::Git { url, revision } => Ok(format!(
            "{{ git = {}, rev = {}, default-features = false, features = [\"typed\"] }}",
            quote(url)?,
            quote(revision)?
        )),
    }
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
    let sdk = sdk_dependency(&options.sdk)?;
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
        ("protocol/examples/schema.rs", "fn main() -> Result<(), Box<dyn std::error::Error>> {\n    println!(\"{}\", {{crate}}_protocol::module().schema_json()?);\n    Ok(())\n}\n"),
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
        for directory in ["protocol/src", "protocol/examples", "logic/src", "host/src"] {
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
