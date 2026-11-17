//! Rust producer for checked layered COFF/ELF artifacts. Requires Cargo/rustup
//! and the platform linker, but no PowerShell or Python process.
use super::development::{capture, command, read_limit};
use crate::watch_inputs::normalize;
use object::Object;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Build one logic object and capture its resident std/support foundation.
#[derive(Clone, Debug)]
pub struct BuildOptions {
    pub manifest_path: PathBuf,
    pub package: String,
    pub support_target: String,
    pub target_directory: PathBuf,
    /// New output path; existing outputs are never overwritten.
    pub output: PathBuf,
    pub cache_directory: PathBuf,
    pub toolchain: String,
    pub features: Vec<String>,
    pub no_default_features: bool,
    pub offline: bool,
    pub optimization: u32,
    /// Default Cargo dev optimization for resident support/dependencies. Package
    /// overrides still apply. None inherits Cargo configuration; logic uses
    /// `optimization` independently. Defaults to Some(1).
    pub support_optimization: Option<u32>,
    pub debug_info: String,
    pub foundation_manifest: Option<PathBuf>,
    pub max_cache_bytes: u64,
}
impl BuildOptions {
    pub fn new(
        manifest: impl Into<PathBuf>,
        package: impl Into<String>,
        support: impl Into<String>,
        cache: impl Into<PathBuf>,
        output: impl Into<PathBuf>,
    ) -> Self {
        let cache = cache.into();
        Self {
            manifest_path: manifest.into(),
            package: package.into(),
            support_target: support.into(),
            target_directory: cache.join("cargo"),
            output: output.into(),
            cache_directory: cache,
            toolchain: "stable".into(),
            features: Vec::new(),
            no_default_features: false,
            offline: false,
            optimization: 0,
            support_optimization: Some(1),
            debug_info: "0".into(),
            foundation_manifest: None,
            max_cache_bytes: 4 * 1024 * 1024 * 1024,
        }
    }
}
#[derive(Debug, serde::Serialize)]
pub struct BuildResult {
    pub manifest: PathBuf,
    pub manifest_sha256: String,
    pub object_sha256: String,
    pub reused: bool,
    pub build_ms: f64,
    pub lock_wait_ms: f64,
}
#[derive(Debug)]
pub struct BuildError {
    pub message: String,
    pub cache_full: bool,
}
impl std::fmt::Display for BuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for BuildError {}
impl From<String> for BuildError {
    fn from(message: String) -> Self {
        Self {
            message,
            cache_full: false,
        }
    }
}
impl From<std::io::Error> for BuildError {
    fn from(error: std::io::Error) -> Self {
        error.to_string().into()
    }
}
fn require(ok: bool, message: &str) -> Result<(), BuildError> {
    if ok {
        Ok(())
    } else {
        Err(message.to_owned().into())
    }
}
fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn absolute(path: &Path) -> Result<PathBuf, BuildError> {
    Ok(normalize(&std::path::absolute(path)?))
}

/// Runs trusted Cargo projects/build scripts. Does not load the generated code.
/// Publishes immutable content-addressed files and a new manifest on success.
pub fn build(options: &BuildOptions) -> Result<BuildResult, BuildError> {
    prepare_with_environment(options, &std::env::vars_os().collect())?
        .build(options.foundation_manifest.as_deref())
}
/// One attempt owns its writer lock and the exact inputs used for discovery and
/// generation. Consuming this value prevents reuse across edits or retries.
pub(super) struct PreparedBuild<'a> {
    builder: Builder<'a>,
    compiler: String,
    compiler_info: String,
    metadata: String,
    _writer_lock: File,
    started: Instant,
    lock_wait_ms: f64,
}
pub(super) enum BuildOutcome {
    Built(BuildResult),
    Superseded(&'static str),
}
impl PreparedBuild<'_> {
    pub(super) fn metadata(&self) -> &[u8] {
        self.metadata.as_bytes()
    }
    pub(super) fn compiler_info(&self) -> &[u8] {
        self.compiler_info.as_bytes()
    }
    pub(super) fn environment(&self) -> &BTreeMap<OsString, OsString> {
        &self.builder.environment
    }
    pub(super) fn build(
        self,
        foundation_manifest: Option<&Path>,
    ) -> Result<BuildResult, BuildError> {
        match self.build_while_current(foundation_manifest, &|_| true)? {
            BuildOutcome::Built(result) => Ok(result),
            BuildOutcome::Superseded(_) => unreachable!("standalone builds are never superseded"),
        }
    }
    /// Stop only between stages. An already running Cargo process is joined;
    /// once publication starts, finish the immutable output/manifest pair.
    pub(super) fn build_while_current(
        self,
        foundation_manifest: Option<&Path>,
        is_current: &dyn Fn(&'static str) -> bool,
    ) -> Result<BuildOutcome, BuildError> {
        let mut outcome = self.builder.build(
            &self.compiler,
            &self.compiler_info,
            &self.metadata,
            foundation_manifest,
            is_current,
        )?;
        if let BuildOutcome::Built(result) = &mut outcome {
            result.lock_wait_ms = self.lock_wait_ms;
            result.build_ms = self.started.elapsed().as_secs_f64() * 1000.0;
        }
        Ok(outcome)
    }
}
fn rustflags() -> &'static str {
    if cfg!(target_os = "linux") {
        "-C prefer-dynamic=yes -C relocation-model=pic -C force-unwind-tables=yes -C panic=unwind -C lto=off"
    } else {
        "-C prefer-dynamic=yes -C force-unwind-tables=yes -C panic=unwind -C lto=off"
    }
}
pub(super) fn prepare_with_environment<'a>(
    options: &'a BuildOptions,
    environment: &BTreeMap<OsString, OsString>,
) -> Result<PreparedBuild<'a>, BuildError> {
    let started = Instant::now();
    require(
        cfg!(any(
            all(windows, target_arch = "x86_64", target_env = "msvc"),
            all(
                target_os = "linux",
                target_arch = "x86_64",
                target_env = "gnu"
            )
        )),
        "layered builds require Windows x64 MSVC or Linux x86_64 GNU",
    )?;
    require(
        !options.package.is_empty()
            && !options.support_target.is_empty()
            && !options.support_target.contains(['/', '\\'])
            && options.optimization <= 3
            && options.support_optimization.is_none_or(|level| level <= 3)
            && ["0", "1", "2", "line-tables-only"].contains(&options.debug_info.as_str())
            && options.max_cache_bytes > 0,
        "invalid layered build options",
    )?;
    for name in ["RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER"] {
        require(
            environment
                .get(OsStr::new(name))
                .is_none_or(|v| v.is_empty()),
            "compiler wrappers need their own provenance contract",
        )?;
    }
    let mut builder = Builder {
        options,
        manifest: normalize(&options.manifest_path.canonicalize()?),
        environment: environment.clone(),
        target: absolute(&options.target_directory)?,
        cache: absolute(&options.cache_directory)?,
        output: absolute(&options.output)?,
    };
    fs::create_dir_all(&builder.target)?;
    fs::create_dir_all(&builder.cache)?;
    fs::create_dir_all(
        builder
            .output
            .parent()
            .ok_or("output needs a parent directory".to_owned())?,
    )?;
    let lock_start = Instant::now();
    let lock = writer_lock(&builder.target)?;
    let lock_wait_ms = lock_start.elapsed().as_secs_f64() * 1000.0;
    let compiler = builder.run(
        "rustup",
        &[
            "which".into(),
            "--toolchain".into(),
            options.toolchain.clone().into(),
            "rustc".into(),
        ],
        false,
    )?;
    let compiler = compiler.as_str().unwrap().to_owned();
    let compiler_info = builder.run(&compiler, &["-vV".into()], false)?;
    let compiler_info = compiler_info.as_str().unwrap().to_owned();
    let target = super::artifact::native_target();
    require(
        compiler_info.contains(&format!("host: {target}")),
        "selected compiler has the wrong host target",
    )?;
    builder
        .environment
        .insert("RUSTC".into(), compiler.clone().into());
    builder
        .environment
        .insert("RUSTFLAGS".into(), rustflags().into());
    builder
        .environment
        .insert("CARGO_INCREMENTAL".into(), "0".into());
    if let Some(level) = options.support_optimization {
        builder.environment.insert(
            "CARGO_PROFILE_DEV_OPT_LEVEL".into(),
            level.to_string().into(),
        );
    }
    builder
        .environment
        .remove(OsStr::new("CARGO_ENCODED_RUSTFLAGS"));
    // Metadata and generation must resolve Cargo build-directory templates and
    // compiler information under the same actual output directory.
    builder
        .environment
        .insert("CARGO_TARGET_DIR".into(), builder.target.clone().into());
    let mut args = vec![
        "--format-version".into(),
        "1".into(),
        "--filter-platform".into(),
        target.into(),
    ];
    args.extend(builder.features(true));
    let metadata = builder.cargo("metadata", args, false)?;
    Ok(PreparedBuild {
        builder,
        compiler,
        compiler_info,
        metadata: metadata.as_str().unwrap().to_owned(),
        _writer_lock: lock,
        started,
        lock_wait_ms,
    })
}
struct Builder<'a> {
    options: &'a BuildOptions,
    manifest: PathBuf,
    environment: BTreeMap<OsString, OsString>,
    target: PathBuf,
    cache: PathBuf,
    output: PathBuf,
}
impl Builder<'_> {
    fn run(
        &self,
        program: impl AsRef<OsStr>,
        args: &[OsString],
        records: bool,
    ) -> Result<Value, BuildError> {
        let mut c = command(program.as_ref(), &self.environment, &self.manifest);
        c.args(args);
        let (ok, bytes, stderr) = capture(c, 16 * 1024 * 1024)?;
        let mut diagnostics = stderr;
        let text = String::from_utf8(bytes).map_err(|e| e.to_string())?;
        let value = if records {
            let mut messages = Vec::new();
            for line in text.lines().filter(|l| !l.trim().is_empty()) {
                let message: Value = serde_json::from_str(line).map_err(|e| e.to_string())?;
                if message["reason"] == "compiler-message" {
                    diagnostics.push_str(message["message"]["rendered"].as_str().unwrap_or(""));
                }
                messages.push(message);
            }
            Value::Array(messages)
        } else {
            Value::String(text.trim().replace("\r\n", "\n"))
        };
        require(
            ok,
            &format!(
                "{} failed\n{diagnostics}",
                program.as_ref().to_string_lossy()
            ),
        )?;
        Ok(value)
    }
    fn cargo(
        &self,
        operation: &str,
        extra: Vec<OsString>,
        records: bool,
    ) -> Result<Value, BuildError> {
        let mut args: Vec<OsString> = vec![
            format!("+{}", self.options.toolchain).into(),
            operation.into(),
            "--locked".into(),
            "--manifest-path".into(),
            self.manifest.clone().into(),
        ];
        if self.options.offline {
            args.push("--offline".into());
        }
        args.extend(extra);
        self.run("cargo", &args, records)
    }
    fn features(&self, metadata: bool) -> Vec<OsString> {
        let mut result = Vec::new();
        if self.options.no_default_features {
            result.push("--no-default-features".into());
        }
        if !self.options.features.is_empty() {
            result.push("--features".into());
            result.push(
                self.options
                    .features
                    .iter()
                    .map(|f| {
                        if metadata && !f.contains('/') {
                            format!("{}/{f}", self.options.package)
                        } else {
                            f.clone()
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(",")
                    .into(),
            );
        }
        result
    }
    fn capacity(&self, additional: u64) -> Result<(), BuildError> {
        fn size(dir: &Path, seen: &mut HashSet<PathBuf>) -> Result<u64, std::io::Error> {
            if !seen.insert(dir.canonicalize()?) {
                return Ok(0);
            }
            let mut bytes = 0u64;
            for entry in fs::read_dir(dir)? {
                let path = entry?.path();
                // Follow links as before, but query each path only once. Do not
                // reuse directory-enumeration metadata across Cargo's writes.
                let metadata = path.metadata()?;
                bytes = bytes.saturating_add(if metadata.is_dir() {
                    size(&path, seen)?
                } else {
                    metadata.len()
                });
            }
            Ok(bytes)
        }
        if size(&self.cache, &mut HashSet::new())?.saturating_add(additional)
            >= self.options.max_cache_bytes
        {
            return Err(BuildError {
                cache_full: true,
                message: "layered cache capacity reached; no automatic deletion is performed"
                    .into(),
            });
        }
        Ok(())
    }
    fn build(
        &self,
        compiler: &str,
        info: &str,
        metadata: &str,
        foundation_manifest: Option<&Path>,
        is_current: &dyn Fn(&'static str) -> bool,
    ) -> Result<BuildOutcome, BuildError> {
        if !is_current("before_foundation") {
            return Ok(BuildOutcome::Superseded("before_foundation"));
        }
        let a = self.options;
        let target = super::artifact::native_target();
        let elf = cfg!(target_os = "linux");
        let flags = rustflags();
        let mut manifest = self.output.as_os_str().to_owned();
        manifest.push(".json");
        let manifest = PathBuf::from(manifest);
        require(
            !self.output.exists() && !manifest.exists(),
            "build output must be new; choose a new output path",
        )?;
        self.capacity(0)?;
        let libdir = self.run(
            compiler,
            &[
                "--print".into(),
                "target-libdir".into(),
                "--target".into(),
                target.into(),
            ],
            false,
        )?;
        let stds = fs::read_dir(libdir.as_str().unwrap())?
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| {
                let name = p.file_name().unwrap().to_string_lossy();
                name.starts_with(if elf { "libstd-" } else { "std-" })
                    && name.ends_with(if elf { ".so" } else { ".dll" })
            })
            .collect::<Vec<_>>();
        require(stds.len() == 1, "expected one matching Rust std library")?;
        let std = Frozen::read(&stds[0], 256 * 1024 * 1024)?;
        let metadata: Value = serde_json::from_str(metadata).map_err(|e| e.to_string())?;
        let packages = metadata["packages"]
            .as_array()
            .ok_or("missing Cargo packages".to_owned())?;
        let logic = packages
            .iter()
            .filter(|p| p["name"] == a.package)
            .collect::<Vec<_>>();
        let support = packages
            .iter()
            .filter(|p| {
                p["targets"].as_array().is_some_and(|ts| {
                    ts.iter().any(|t| {
                        t["name"] == a.support_target
                            && t["crate_types"]
                                .as_array()
                                .is_some_and(|v| v.iter().any(|v| v == "dylib"))
                    })
                })
            })
            .collect::<Vec<_>>();
        require(
            logic.len() == 1 && support.len() == 1,
            "select one logic package and one support dylib target",
        )?;
        let logic_id = logic[0]["id"]
            .as_str()
            .ok_or("missing logic package ID".to_owned())?;
        let support_id = support[0]["id"]
            .as_str()
            .ok_or("missing support package ID".to_owned())?;
        let nodes = metadata["resolve"]["nodes"]
            .as_array()
            .ok_or("missing Cargo resolve graph".to_owned())?;
        let mut needed = HashSet::from([logic_id.to_owned()]);
        loop {
            let count = needed.len();
            for node in nodes {
                if node["id"].as_str().is_some_and(|id| needed.contains(id)) {
                    for dep in node["dependencies"]
                        .as_array()
                        .ok_or("missing dependencies".to_owned())?
                    {
                        needed.insert(
                            dep.as_str()
                                .ok_or("invalid package ID".to_owned())?
                                .to_owned(),
                        );
                    }
                }
            }
            if count == needed.len() {
                break;
            }
        }
        require(
            logic_id != support_id && needed.contains(support_id),
            "support must be a dependency of logic",
        )?;
        let lock_path = Path::new(
            metadata["workspace_root"]
                .as_str()
                .ok_or("missing workspace root".to_owned())?,
        )
        .join("Cargo.lock");
        let lock = Frozen::read(&lock_path, 64 * 1024 * 1024)?;
        let support_path = self.target.join(target).join("debug").join(if elf {
            format!("lib{}.so", a.support_target)
        } else {
            format!("{}.dll", a.support_target)
        });
        let build_args: Vec<OsString> = vec![
            "--target".into(),
            target.into(),
            "--target-dir".into(),
            self.target.clone().into(),
            "--message-format".into(),
            "json-render-diagnostics".into(),
        ];
        let prior: Option<Value> = foundation_manifest
            .map(|path| {
                let bytes =
                    super::artifact::read_manifest_bytes(path).map_err(|e| e.to_string())?;
                serde_json::from_slice(&bytes).map_err(|e| e.to_string())
            })
            .transpose()?;
        if let Some(prior) = &prior {
            let foundation = &prior["foundation"];
            let std_record = library_record(foundation, "std", elf);
            require(
                prior["schema"] == if elf { 3 } else { 2 }
                    && prior["profile"] == "layered-rust-library-object"
                    && foundation["target"] == target
                    && foundation["rustc"] == info
                    && foundation["rustflags"] == flags
                    && std_record["sha256"] == hash(&std.bytes)
                    && std_record["bytes"] == std.bytes.len() as u64
                    && foundation["cargo_lock_sha256"] == hash(&lock.bytes)
                    && foundation["support_build"]["package_id"] == support_id,
                "foundation reuse identity differs",
            )?;
        }
        let (staging, reserved) = Temporary::create(&self.output, if elf { "o" } else { "obj" })?;
        drop(reserved);
        let mut args = build_args;
        args.extend([
            "--package".into(),
            logic_id.into(),
            "--lib".into(),
            "--crate-type".into(),
            "lib".into(),
        ]);
        args.extend(self.features(false));
        args.extend([
            "--".into(),
            "-C".into(),
            "codegen-units=1".into(),
            "-C".into(),
            "lto=off".into(),
            "-C".into(),
            "panic=unwind".into(),
            "-C".into(),
            format!("opt-level={}", a.optimization).into(),
            "-C".into(),
            format!("debuginfo={}", a.debug_info).into(),
        ]);
        let mut emit = OsString::from("--emit=obj=");
        emit.push(&staging.0);
        args.push(emit);
        if !is_current("before_logic_cargo") {
            return Ok(BuildOutcome::Superseded("before_logic_cargo"));
        }
        // Cargo metadata can unify features from unrelated workspace members.
        // Capture the support artifact from the same compilation as the logic
        // object, after Cargo is free to replace its writable outputs.
        let generated = self.cargo("rustc", args, true);
        // A superseded compile error must not trigger a foundation-reuse retry.
        if !is_current("after_logic_cargo") {
            return Ok(BuildOutcome::Superseded("after_logic_cargo"));
        }
        let support_build = support_build(&generated?, support_id, &support_path)?;
        let support = Frozen::read(&support_path, 256 * 1024 * 1024)?;
        if let Some(prior) = &prior {
            let foundation = &prior["foundation"];
            let record = library_record(foundation, "support", elf);
            require(
                foundation["support_build"] == support_build
                    && record["sha256"] == hash(&support.bytes)
                    && record["bytes"] == support.bytes.len() as u64,
                "logic's support build differs from the requested foundation",
            )?;
        }
        std.verify()?;
        support.verify()?;
        lock.verify()?;
        require(
            self.run(compiler, &["-vV".into()], false)? == info,
            "compiler identity changed during build",
        )?;
        let object = read_limit(&staging.0, 64 * 1024 * 1024)?;
        let parsed = object::File::parse(object.as_slice()).map_err(|e| e.to_string())?;
        require(
            parsed.architecture() == object::Architecture::X86_64
                && parsed.kind() == object::ObjectKind::Relocatable
                && parsed.format()
                    == if elf {
                        object::BinaryFormat::Elf
                    } else {
                        object::BinaryFormat::Coff
                    },
            "expected an x86_64 relocatable COFF/ELF object",
        )?;
        self.capacity((std.bytes.len() + support.bytes.len() + 2 * object.len()) as u64)?;
        if !is_current("before_publication") {
            return Ok(BuildOutcome::Superseded("before_publication"));
        }
        let foundation_key =
            hash(format!("{}\n{}\n{}", hash(&std.bytes), hash(&support.bytes), info).as_bytes());
        let directory = self.cache.join("foundation").join(foundation_key);
        let mut foundation = json!({"rustc":info,"target":target,"handler_abi":5,"panic":"unwind","tls":"generation-local","prefer_dynamic":true,"rustflags":flags,"cargo_lock_sha256":hash(&lock.bytes),"support_build":support_build});
        foundation[if elf { "std_library" } else { "std_dll" }] =
            immutable(&directory.join(stds[0].file_name().unwrap()), &std.bytes)?;
        foundation[if elf {
            "support_library"
        } else {
            "support_dll"
        }] = immutable(
            &directory.join(support_path.file_name().unwrap()),
            &support.bytes,
        )?;
        let object_hash = hash(&object);
        let artifact = immutable(
            &self.cache.join("objects").join(&object_hash).join(if elf {
                "logic.o"
            } else {
                "logic.obj"
            }),
            &object,
        )?;
        let record = json!({"schema":if elf {3} else {2},"profile":"layered-rust-library-object","foundation":foundation,"logic":{"artifact":artifact,"crate_type":"lib","codegen_units":1,"lto":"off","opt_level":a.optimization,"debuginfo":a.debug_info}});
        let bytes = serde_json::to_vec_pretty(&record).map_err(|e| e.to_string())?;
        publish(&self.output, &object)?;
        publish(&manifest, &bytes)?;
        Ok(BuildOutcome::Built(BuildResult {
            manifest,
            manifest_sha256: hash(&bytes),
            object_sha256: object_hash,
            reused: prior.is_some(),
            build_ms: 0.0,
            lock_wait_ms: 0.0,
        }))
    }
}
fn library_record<'a>(foundation: &'a Value, name: &str, elf: bool) -> &'a Value {
    &foundation[format!("{name}_{}", if elf { "library" } else { "dll" })]
}
fn support_build(records: &Value, id: &str, path: &Path) -> Result<Value, BuildError> {
    let matches = records
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| {
            r["reason"] == "compiler-artifact"
                && r["package_id"] == id
                && r["target"]["crate_types"]
                    .as_array()
                    .is_some_and(|v| v.iter().any(|v| v == "dylib"))
        })
        .collect::<Vec<_>>();
    require(
        matches.len() == 1,
        "expected one Cargo support dylib artifact",
    )?;
    let artifact = matches[0];
    let files = artifact["filenames"]
        .as_array()
        .ok_or("missing artifact paths".to_owned())?
        .iter()
        .filter_map(Value::as_str)
        .filter(|p| {
            p.ends_with(if cfg!(target_os = "linux") {
                ".so"
            } else {
                ".dll"
            })
        })
        .collect::<Vec<_>>();
    require(
        files.len() == 1 && normalize(Path::new(files[0])) == normalize(path),
        "support must be the actual Cargo output",
    )?;
    let profile = &artifact["profile"];
    require(profile["test"] == false, "test profiles are unsupported")?;
    let mut features = artifact["features"]
        .as_array()
        .ok_or("missing artifact features".to_owned())?
        .clone();
    features.sort_by(|a, b| a.as_str().cmp(&b.as_str()));
    Ok(
        json!({"package_id":id,"features":features,"opt_level":profile["opt_level"],"debuginfo":profile["debuginfo"],"debug_assertions":profile["debug_assertions"],"overflow_checks":profile["overflow_checks"]}),
    )
}
struct Frozen {
    path: PathBuf,
    bytes: Vec<u8>,
    _file: File,
}
impl Frozen {
    fn read(path: &Path, limit: usize) -> Result<Self, BuildError> {
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.share_mode(1);
        }
        let file = options.open(path)?;
        use std::io::Read;
        let mut bytes = Vec::new();
        (&file).take(limit as u64 + 1).read_to_end(&mut bytes)?;
        require(
            !bytes.is_empty() && bytes.len() <= limit,
            "foundation input is empty or oversized",
        )?;
        Ok(Self {
            path: path.into(),
            bytes,
            _file: file,
        })
    }
    fn verify(&self) -> Result<(), BuildError> {
        require(
            read_limit(&self.path, self.bytes.len())? == self.bytes,
            "foundation changed during logic build",
        )
    }
}
fn writer_lock(target: &Path) -> Result<File, BuildError> {
    let started = Instant::now();
    loop {
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.share_mode(0);
        }
        match options.open(target.join(".reknot-layered-writer.lock")) {
            Ok(file) => match file.try_lock() {
                Ok(()) => return Ok(file),
                Err(std::fs::TryLockError::WouldBlock) => {}
                Err(std::fs::TryLockError::Error(e)) => return Err(e.into()),
            },
            Err(e) if cfg!(windows) && matches!(e.raw_os_error(), Some(32 | 33)) => {}
            Err(e) => return Err(e.into()),
        }
        require(
            started.elapsed() < Duration::from_secs(120),
            "timed out waiting for Cargo writer lock",
        )?;
        std::thread::sleep(Duration::from_millis(50));
    }
}
struct Temporary(PathBuf);
impl Temporary {
    fn create(path: &Path, extension: &str) -> Result<(Self, File), BuildError> {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        loop {
            let mut name = path.as_os_str().to_owned();
            name.push(format!(
                ".{}-{}.{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed),
                extension
            ));
            let path = PathBuf::from(name);
            match File::create_new(&path) {
                Ok(file) => return Ok((Self(path), file)),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            }
        }
    }
}
impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}
fn publish(path: &Path, bytes: &[u8]) -> Result<(), BuildError> {
    fs::create_dir_all(
        path.parent()
            .ok_or("publication needs a parent directory".to_owned())?,
    )?;
    let (temporary, mut file) = Temporary::create(path, "tmp")?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    fs::hard_link(&temporary.0, path)?;
    Ok(())
}
fn immutable(path: &Path, bytes: &[u8]) -> Result<Value, BuildError> {
    if !path.exists() {
        if let Err(e) = publish(path, bytes) {
            if !path.exists() {
                return Err(e);
            }
        }
    }
    require(
        read_limit(path, bytes.len())? == bytes,
        "immutable artifact cache differs",
    )?;
    Ok(json!({"path":path,"sha256":hash(bytes),"bytes":bytes.len()}))
}

#[cfg(all(test, not(miri)))]
#[path = "jit_builder_cancel_tests.rs"]
mod cancellation_tests;

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    pub(super) fn directory() -> PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("reknot-producer-{}-{stamp}", std::process::id()));
        fs::create_dir(&dir).unwrap();
        dir
    }
    #[test]
    fn publication_never_overwrites_and_immutable_cache_is_verified() {
        let dir = directory();
        let path = dir.join("artifact");
        publish(&path, b"first").unwrap();
        assert!(publish(&path, b"second").is_err());
        immutable(&path, b"first").unwrap();
        assert!(immutable(&path, b"second").is_err());
        assert_eq!(fs::read(&path).unwrap(), b"first");
        fs::remove_file(path).unwrap();
        fs::remove_dir(dir).unwrap();
    }
    fn check_capacity(cache: &Path, limit: u64, additional: u64) -> Result<(), BuildError> {
        let mut options = BuildOptions::new("Cargo.toml", "logic", "support", cache, "unused");
        options.max_cache_bytes = limit;
        Builder {
            options: &options,
            manifest: "Cargo.toml".into(),
            environment: BTreeMap::new(),
            target: cache.into(),
            cache: cache.into(),
            output: "unused".into(),
        }
        .capacity(additional)
    }
    #[test]
    fn capacity_rechecks_growth_shrinkage_and_exact_limit() {
        let dir = directory();
        let nested = dir.join("nested");
        fs::create_dir(&nested).unwrap();
        fs::write(dir.join("first"), b"1234").unwrap();
        let file = nested.join("second");
        fs::write(&file, b"123").unwrap();
        assert!(check_capacity(&dir, 8, 0).is_ok());
        assert!(check_capacity(&dir, 7, 0).unwrap_err().cache_full);
        assert!(check_capacity(&dir, 8, 1).unwrap_err().cache_full);
        fs::write(&file, b"1234567890").unwrap();
        assert!(check_capacity(&dir, 14, 0).unwrap_err().cache_full);
        assert!(check_capacity(&dir, 15, 0).is_ok());
        fs::write(&file, b"1").unwrap();
        assert!(check_capacity(&dir, 6, 0).is_ok());
        assert!(
            check_capacity(&dir, u64::MAX, u64::MAX)
                .unwrap_err()
                .cache_full
        );
        fs::remove_file(file).unwrap();
        fs::remove_file(dir.join("first")).unwrap();
        fs::remove_dir(nested).unwrap();
        fs::remove_dir(dir).unwrap();
    }
    #[cfg(unix)]
    #[test]
    fn capacity_follows_links_detects_cycles_and_rejects_broken_links() {
        use std::os::unix::fs::symlink;
        let dir = directory();
        let nested = dir.join("nested");
        fs::create_dir(&nested).unwrap();
        fs::write(nested.join("file"), b"1234").unwrap();
        symlink(&nested, dir.join("alias")).unwrap();
        symlink(&dir, nested.join("cycle")).unwrap();
        assert!(check_capacity(&dir, 5, 0).is_ok());
        assert!(check_capacity(&dir, 4, 0).unwrap_err().cache_full);
        symlink(nested.join("file"), dir.join("file-link")).unwrap();
        assert!(check_capacity(&dir, 8, 0).unwrap_err().cache_full);
        assert!(check_capacity(&dir, 9, 0).is_ok());
        fs::remove_file(nested.join("file")).unwrap();
        let error = check_capacity(&dir, 100, 0).unwrap_err();
        assert!(!error.cache_full);
        for path in [
            dir.join("alias"),
            nested.join("cycle"),
            dir.join("file-link"),
        ] {
            fs::remove_file(path).unwrap();
        }
        fs::remove_dir(nested).unwrap();
        fs::remove_dir(dir).unwrap();
    }
    #[test]
    fn writer_lock_serializes_producers_for_the_same_cargo_output() {
        let dir = directory();
        let first = writer_lock(&dir).unwrap();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (tx, rx) = std::sync::mpsc::channel();
        let other = dir.clone();
        let thread = std::thread::spawn(move || {
            ready_tx.send(()).unwrap();
            let _guard = writer_lock(&other).unwrap();
            tx.send(()).unwrap();
        });
        ready_rx.recv().unwrap();
        assert!(matches!(
            rx.recv_timeout(Duration::from_millis(60)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));
        drop(first);
        rx.recv_timeout(Duration::from_secs(5)).unwrap();
        thread.join().unwrap();
        fs::remove_file(dir.join(".reknot-layered-writer.lock")).unwrap();
        fs::remove_dir(dir).unwrap();
    }
}
