//! Identity-checked artifacts; hashes establish byte identity, not code safety.
use super::{CoffEngine, CoffLimits, JitError, JitLoadMetrics};
use crate::LoadedHandler;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Instant;

const MAX_MANIFEST_BYTES: usize = 256 * 1024;
pub(super) const MAX_DLL_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct FileRecord {
    pub path: PathBuf,
    pub sha256: String,
    pub bytes: u64,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct SupportBuild {
    package_id: String,
    features: Vec<String>,
    opt_level: String,
    debuginfo: u32,
    debug_assertions: bool,
    overflow_checks: bool,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Foundation {
    pub rustc: String,
    target: String,
    handler_abi: u32,
    panic: String,
    tls: String,
    prefer_dynamic: bool,
    rustflags: String,
    cargo_lock_sha256: String,
    support_build: SupportBuild,
    pub std_dll: FileRecord,
    pub support_dll: FileRecord,
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Logic {
    artifact: FileRecord,
    crate_type: String,
    codegen_units: u32,
    lto: String,
    opt_level: u32,
    debuginfo: String,
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema: u32,
    profile: String,
    foundation: Foundation,
    logic: Logic,
}

pub(super) fn identity_error(message: impl Into<String>) -> JitError {
    JitError::Identity(message.into())
}
fn manifest_error(message: impl Into<String>) -> JitError {
    JitError::Manifest(message.into())
}
pub(super) fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn normalize_hash(value: &mut str) -> Result<(), JitError> {
    if value.len() != 64 || !value.bytes().all(|c| c.is_ascii_hexdigit()) {
        return Err(manifest_error("expected a SHA-256 hex digest"));
    }
    value.make_ascii_lowercase();
    Ok(())
}
fn read_bounded(path: &Path, limit: usize) -> Result<Vec<u8>, JitError> {
    let mut bytes = Vec::new();
    File::open(path)
        .map_err(JitError::Io)?
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(JitError::Io)?;
    if bytes.len() > limit {
        return Err(JitError::ObjectTooLarge { limit });
    }
    Ok(bytes)
}
fn validate_file(record: &mut FileRecord, limit: u64) -> Result<(), JitError> {
    if !record.path.is_absolute()
        || record.path.to_string_lossy().contains('\0')
        || record.bytes == 0
        || record.bytes > limit
    {
        return Err(manifest_error(
            "artifact needs an absolute path and a bounded nonzero size",
        ));
    }
    normalize_hash(&mut record.sha256)
}
impl Manifest {
    fn validate(&mut self, object_limit: usize) -> Result<(), JitError> {
        let f = &mut self.foundation;
        if self.schema != 2
            || self.profile != "layered-rust-library-object"
            || f.target != "x86_64-pc-windows-msvc"
            || f.handler_abi != 5
            || f.panic != "unwind"
            || f.tls != "generation-local"
            || !f.prefer_dynamic
            || !f.rustc.starts_with("rustc ")
            || !f.rustc.contains("commit-hash: ")
            || !f.rustc.contains("host: x86_64-pc-windows-msvc")
            || f.rustflags
                != "-C prefer-dynamic=yes -C force-unwind-tables=yes -C panic=unwind -C lto=off"
            || self.logic.crate_type != "lib"
            || self.logic.codegen_units != 1
            || self.logic.lto != "off"
            || self.logic.opt_level > 3
            || !["0", "1", "2", "line-tables-only"].contains(&self.logic.debuginfo.as_str())
        {
            return Err(manifest_error(
                "unsupported schema, compiler or layered codegen profile",
            ));
        }
        normalize_hash(&mut f.cargo_lock_sha256)?;
        f.rustc = f.rustc.replace("\r\n", "\n").trim().to_owned();
        f.support_build.features.sort();
        if f.support_build.package_id.is_empty()
            || f.support_build.features.windows(2).any(|p| p[0] == p[1])
        {
            return Err(manifest_error("invalid support package/features"));
        }
        validate_file(&mut f.std_dll, MAX_DLL_BYTES)?;
        validate_file(&mut f.support_dll, MAX_DLL_BYTES)?;
        validate_file(&mut self.logic.artifact, object_limit as u64)?;
        let std_name = dll_name(&f.std_dll)?;
        let support_name = dll_name(&f.support_dll)?;
        if !std_name.starts_with("std-") || std_name == support_name {
            return Err(manifest_error("invalid std/support DLL names"));
        }
        Ok(())
    }
}
pub(super) fn dll_name(file: &FileRecord) -> Result<String, JitError> {
    let name = file
        .path
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| manifest_error("invalid DLL filename"))?
        .to_ascii_lowercase();
    if !name.ends_with(".dll") {
        return Err(manifest_error("expected a DLL filename"));
    }
    Ok(name)
}
impl Foundation {
    fn id(&self) -> Result<String, JitError> {
        // Paths are locators, not identity. Equivalent immutable copies can be
        // reused. DLL basenames remain significant to Windows import binding.
        let mut value = serde_json::to_value(self).map_err(|e| manifest_error(e.to_string()))?;
        value["std_dll"]["path"] = dll_name(&self.std_dll)?.into();
        value["support_dll"]["path"] = dll_name(&self.support_dll)?.into();
        Ok(digest(
            &serde_json::to_vec(&value).map_err(|e| manifest_error(e.to_string()))?,
        ))
    }
}

/// A bounded schema-2 manifest plus the exact COFF bytes whose digest it names.
/// Reading and inspecting this value executes no code. DLL bytes are checked and
/// pinned when opening a LayeredEngine. A manifest is not an authenticity proof.
/// ```compile_fail
/// use reknot::jit::LayeredArtifact;
/// let artifact = LayeredArtifact::read("logic.obj.json").unwrap();
/// let trusted = artifact.into_trusted(); // Native-code trust still needs unsafe.
/// ```
pub struct LayeredArtifact {
    manifest: Manifest,
    foundation_id: String,
    object: Box<[u8]>,
}
impl LayeredArtifact {
    /// Read with the default 32 MiB object limit. Schema 1 is intentionally rejected.
    pub fn read(path: impl AsRef<Path>) -> Result<Self, JitError> {
        Self::read_with_limit(path, super::super::JitLimits::default().max_object_bytes)
    }
    pub fn read_with_limit(
        path: impl AsRef<Path>,
        max_object_bytes: usize,
    ) -> Result<Self, JitError> {
        if max_object_bytes == 0 || max_object_bytes >= isize::MAX as usize {
            return Err(JitError::InvalidLimits);
        }
        let bytes = read_bounded(path.as_ref(), MAX_MANIFEST_BYTES).map_err(|e| match e {
            JitError::ObjectTooLarge { .. } => manifest_error("manifest exceeds 256 KiB"),
            other => other,
        })?;
        Self::from_manifest_bytes(&bytes, max_object_bytes)
    }
    pub(crate) fn from_manifest_bytes(
        bytes: &[u8],
        max_object_bytes: usize,
    ) -> Result<Self, JitError> {
        if bytes.len() > MAX_MANIFEST_BYTES
            || max_object_bytes == 0
            || max_object_bytes >= isize::MAX as usize
        {
            return Err(JitError::InvalidLimits);
        }
        let bytes = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes);
        let mut manifest: Manifest =
            serde_json::from_slice(bytes).map_err(|e| manifest_error(e.to_string()))?;
        manifest.validate(max_object_bytes)?;
        let object = read_bounded(&manifest.logic.artifact.path, max_object_bytes)?;
        if object.len() as u64 != manifest.logic.artifact.bytes
            || digest(&object) != manifest.logic.artifact.sha256
        {
            return Err(identity_error(
                "logic size/SHA-256 differs from its build manifest",
            ));
        }
        let foundation_id = manifest.foundation.id()?;
        Ok(Self {
            manifest,
            foundation_id,
            object: object.into_boxed_slice(),
        })
    }
    pub fn foundation_id(&self) -> &str {
        &self.foundation_id
    }
    pub fn object_sha256(&self) -> &str {
        &self.manifest.logic.artifact.sha256
    }

    /// Accept the native-code/provenance contract for these immutable bytes.
    /// Matching this artifact to a running foundation is subsequently checked by
    /// safe APIs; the caller need not manually compare std/support identities.
    ///
    /// # Safety
    /// Trust the producer and these artifacts, including DLL initializers and
    /// dependencies. The manifest must truthfully describe the compiler and
    /// foundation against which the object was built. All code must obey the
    /// layered Handler/callback/lifetime contracts in `CoffEngine::load`.
    /// Neither hashes nor ABI headers prove those properties. Native code and
    /// other unsafe loaders must not tamper with the process loader state or
    /// introduce conflicting DLLs during initialization. The process loading
    /// configuration (including .local/SxS redirection) and dependency resolution
    /// must be trusted and must not redirect these DLLs. Loaded paths are checked
    /// after initialization; that check cannot undo redirected initializers.
    /// Arbitrary code cannot
    /// acquire this trust merely by supplying a matching, self-authored manifest.
    pub unsafe fn into_trusted(self) -> TrustedLayeredArtifact {
        TrustedLayeredArtifact(self)
    }
}

/// Native-code trust is attached to the captured object and its declared
/// foundation. Fields are private; safe code cannot substitute paths or bytes.
pub struct TrustedLayeredArtifact(LayeredArtifact);
impl TrustedLayeredArtifact {
    pub fn foundation_id(&self) -> &str {
        self.0.foundation_id()
    }
    /// Accept the managed-v1 lifetime profile, in addition to native-code trust.
    ///
    /// # Safety
    /// From entry onward, all logic code/data/vtables/destructors and panic payloads
    /// must be confined to Router-owned calls, generation locals, managed children,
    /// or individually audited ManagedActivity adapters. No unmanaged references
    /// may escape into support, hooks, atexit, dynamic TLS, threads or runtimes.
    /// External adapters must join through the FINAL return of every callback and
    /// logic Waker operation, destroy their values while leased, and remain Pending
    /// if quiescence is uncertain. HostNotification is the supported external wake
    /// path. These obligations include dependencies and validation/entry failures.
    /// A hash, manifest flag, timeout or reference-count observation is not proof.
    ///
    /// ```compile_fail
    /// fn managed(a: reknot::jit::TrustedLayeredArtifact) {
    ///     let _ = a.into_managed(); // An additional unsafe contract is required.
    /// }
    /// ```
    pub unsafe fn into_managed(self) -> ManagedLayeredArtifact {
        ManagedLayeredArtifact(self)
    }
}

/// Version-one managed lifetime contract, inseparable from these captured bytes.
pub struct ManagedLayeredArtifact(TrustedLayeredArtifact);
impl ManagedLayeredArtifact {
    pub fn foundation_id(&self) -> &str {
        self.0.foundation_id()
    }
    pub fn profile_version(&self) -> u32 {
        1
    }
}

/// Identity-checked Rust COFF session. One verified resident foundation is allowed
/// per process. Changed foundations or externally preloaded DLLs require restart.
/// The first open pins DLL files/directories until process exit. Existing raw
/// CoffEngine/JitEngine entry points remain available as separate unsafe APIs.
/// ```compile_fail
/// use reknot::jit::{CoffLimits, LayeredArtifact, LayeredEngine};
/// let artifact = LayeredArtifact::read("logic.obj.json").unwrap();
/// let engine = LayeredEngine::open(&artifact, CoffLimits::default());
/// ```
/// ```compile_fail
/// use reknot::jit::{LayeredArtifact, LayeredEngine};
/// fn reload(engine: &mut LayeredEngine, untrusted: LayeredArtifact) {
///     engine.load(untrusted);
/// }
/// ```
pub struct LayeredEngine {
    engine: CoffEngine,
    foundation_id: String,
}

impl LayeredEngine {
    /// Validate/pin both DLLs before initialization. Reuses an identical verified
    /// foundation; never initializes a changed one in this process.
    pub fn open(artifact: &TrustedLayeredArtifact, limits: CoffLimits) -> Result<Self, JitError> {
        if limits.load.max_generations == 0
            || limits.load.max_object_bytes == 0
            || limits.load.max_object_bytes >= isize::MAX as usize
            || limits.max_image_bytes == 0
            || limits.max_image_bytes > i32::MAX as usize
            || limits.max_retained_images == 0
            || limits.max_retained_bytes == 0
        {
            return Err(JitError::InvalidLimits);
        }
        if artifact.0.object.len() > limits.load.max_object_bytes {
            return Err(JitError::ObjectTooLarge {
                limit: limits.load.max_object_bytes,
            });
        }
        #[cfg(all(windows, target_arch = "x86_64", target_env = "msvc"))]
        {
            let runtime = super::platform::open_verified_foundation(
                &artifact.0.manifest.foundation,
                artifact.foundation_id(),
            )?;
            Ok(Self {
                engine: CoffEngine {
                    runtime,
                    limits,
                    attempts: 0,
                    keep_loaded_attempts: 0,
                    last_image: None,
                    last_metrics: None,
                    #[cfg(test)]
                    last_symbols: Default::default(),
                    _owner_thread: Default::default(),
                },
                foundation_id: artifact.foundation_id().to_owned(),
            })
        }
        #[cfg(not(all(windows, target_arch = "x86_64", target_env = "msvc")))]
        {
            let _ = artifact;
            Err(JitError::UnsupportedTarget)
        }
    }

    /// Consume trusted, captured bytes. Identity/profile failures cannot execute
    /// the object or change the active router. Attempts are not refunded.
    pub fn load(&mut self, artifact: TrustedLayeredArtifact) -> Result<LoadedHandler, JitError> {
        self.load_inner(artifact, super::CoffPolicy::KeepLoaded)
    }
    /// Load under the managed lifetime contract. Live image/byte limits apply;
    /// this does not consume the cumulative KeepLoaded or Router attempt budgets.
    pub fn load_managed(
        &mut self,
        artifact: ManagedLayeredArtifact,
    ) -> Result<LoadedHandler, JitError> {
        self.load_inner(artifact.0, super::CoffPolicy::Managed)
    }
    fn load_inner(
        &mut self,
        artifact: TrustedLayeredArtifact,
        policy: super::CoffPolicy,
    ) -> Result<LoadedHandler, JitError> {
        self.engine.begin_load(policy)?;
        if artifact.foundation_id() != self.foundation_id {
            return Err(identity_error(
                "logic foundation differs from the resident session; restart required",
            ));
        }
        #[cfg(all(windows, target_arch = "x86_64", target_env = "msvc"))]
        {
            unsafe {
                super::platform::load_bytes(
                    &mut self.engine,
                    &artifact.0.object,
                    Instant::now(),
                    &mut JitLoadMetrics::default(),
                    policy,
                    Some(&self.foundation_id),
                    Some(&artifact.0.manifest.logic.artifact.sha256),
                )
            }
        }
        #[cfg(not(all(windows, target_arch = "x86_64", target_env = "msvc")))]
        {
            let _ = (artifact, policy, Instant::now());
            Err(JitError::UnsupportedTarget)
        }
    }
    pub fn foundation_id(&self) -> &str {
        &self.foundation_id
    }
    /// Total attempts across both policies, including failures; diagnostic only.
    pub fn load_attempts(&self) -> usize {
        self.engine.load_attempts()
    }
    /// Attempts charged against CoffLimits::load.max_generations.
    pub fn keep_loaded_attempts(&self) -> usize {
        self.engine.keep_loaded_attempts()
    }
    pub fn last_image_id(&self) -> Option<super::CoffImageId> {
        self.engine.last_image_id()
    }

    #[cfg(feature = "jit-dev")]
    pub(crate) fn required_image_bytes(
        &mut self,
        artifact: &TrustedLayeredArtifact,
    ) -> Result<usize, JitError> {
        if artifact.foundation_id() != self.foundation_id {
            return Err(identity_error(
                "logic foundation differs from the resident session",
            ));
        }
        if artifact.0.object.len() > self.engine.limits.load.max_object_bytes {
            return Err(JitError::ObjectTooLarge {
                limit: self.engine.limits.load.max_object_bytes,
            });
        }
        #[cfg(all(windows, target_arch = "x86_64", target_env = "msvc"))]
        {
            super::platform::required_image_bytes(&mut self.engine, &artifact.0.object)
        }
        #[cfg(not(all(windows, target_arch = "x86_64", target_env = "msvc")))]
        {
            Err(JitError::UnsupportedTarget)
        }
    }
    #[cfg(all(test, windows, target_arch = "x86_64", target_env = "msvc"))]
    pub(crate) fn test_unwind_address(&self) -> usize {
        self.engine
            .last_symbols
            .values()
            .copied()
            .find(|address| {
                let mut base = 0;
                !unsafe {
                    windows_sys::Win32::System::Diagnostics::Debug::RtlLookupFunctionEntry(
                        *address as u64,
                        &mut base,
                        std::ptr::null_mut(),
                    )
                }
                .is_null()
            })
            .expect("fixture needs a non-leaf function with registered unwind metadata")
    }
    /// Artifact read/hash verification happens before load and is not included.
    pub fn last_load_metrics(&self) -> Option<JitLoadMetrics> {
        self.engine.last_load_metrics()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn fixture() -> (PathBuf, Manifest) {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../target/layered-identity-unit")
            .join(format!(
                "{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(dir).unwrap();
        let object = b"an immutable object snapshot, not executable in this test";
        std::fs::write(dir.join("logic.obj"), object).unwrap();
        let record = |name: &str, bytes: &[u8]| FileRecord {
            path: dir.join(name),
            sha256: digest(bytes),
            bytes: bytes.len() as u64,
        };
        let manifest = Manifest {
            schema: 2,
            profile: "layered-rust-library-object".into(),
            foundation: Foundation {
                rustc: "rustc 1.99.1\ncommit-hash: test\nhost: x86_64-pc-windows-msvc".into(),
                target: "x86_64-pc-windows-msvc".into(),
                handler_abi: 5,
                panic: "unwind".into(),
                tls: "generation-local".into(),
                prefer_dynamic: true,
                rustflags:
                    "-C prefer-dynamic=yes -C force-unwind-tables=yes -C panic=unwind -C lto=off"
                        .into(),
                cargo_lock_sha256: digest(b"lock"),
                support_build: SupportBuild {
                    package_id: "support@1".into(),
                    features: vec!["b".into(), "a".into()],
                    opt_level: "1".into(),
                    debuginfo: 2,
                    debug_assertions: true,
                    overflow_checks: true,
                },
                std_dll: record("std-compiler.dll", b"std"),
                support_dll: record("support.dll", b"support"),
            },
            logic: Logic {
                artifact: record("logic.obj", object),
                crate_type: "lib".into(),
                codegen_units: 1,
                lto: "off".into(),
                opt_level: 0,
                debuginfo: "0".into(),
            },
        };
        (dir.join("logic.obj.json"), manifest)
    }
    fn write(path: &Path, manifest: &Manifest) {
        std::fs::write(path, serde_json::to_vec(manifest).unwrap()).unwrap();
    }
    #[test]
    fn captured_bytes_cannot_be_replaced_through_a_path() {
        let (path, manifest) = fixture();
        write(&path, &manifest);
        let artifact = LayeredArtifact::read(&path).unwrap();
        let original = artifact.object.to_vec();
        std::fs::write(&manifest.logic.artifact.path, b"replacement").unwrap();
        assert!(matches!(
            LayeredArtifact::read(&path),
            Err(JitError::Identity(_))
        ));
        assert_eq!(&*artifact.object, original);
        assert_eq!(digest(&artifact.object), artifact.object_sha256());
    }
    #[test]
    fn foundation_identity_tracks_builds_but_not_locator_or_logic_optimization() {
        let (path, mut manifest) = fixture();
        write(&path, &manifest);
        let first = LayeredArtifact::read(&path).unwrap();
        manifest.foundation.std_dll.path = path.parent().unwrap().join("another/std-compiler.dll");
        manifest.foundation.support_build.features.reverse();
        manifest.logic.opt_level = 3;
        manifest.logic.debuginfo = "2".into();
        write(&path, &manifest);
        assert_eq!(
            first.foundation_id(),
            LayeredArtifact::read(&path).unwrap().foundation_id()
        );
        let original = serde_json::to_value(&manifest).unwrap();
        for field in [
            "std_dll",
            "support_dll",
            "rustc",
            "cargo_lock_sha256",
            "support_build",
        ] {
            let mut changed = original.clone();
            match field {
                "std_dll" | "support_dll" => {
                    changed["foundation"][field]["sha256"] = digest(b"different").into()
                }
                "rustc" => {
                    changed["foundation"][field] =
                        "rustc 1.99.1\ncommit-hash: different\nhost: x86_64-pc-windows-msvc".into()
                }
                "cargo_lock_sha256" => changed["foundation"][field] = digest(b"different").into(),
                _ => changed["foundation"][field]["features"] = serde_json::json!(["other"]),
            }
            std::fs::write(&path, serde_json::to_vec(&changed).unwrap()).unwrap();
            assert_ne!(
                first.foundation_id(),
                LayeredArtifact::read(&path).unwrap().foundation_id(),
                "{field}"
            );
        }
    }
    #[test]
    fn unsupported_profiles_schema_and_unknown_fields_are_rejected() {
        let (path, manifest) = fixture();
        let original = serde_json::to_value(manifest).unwrap();
        for (pointer, replacement) in [
            ("/schema", serde_json::json!(1)),
            (
                "/foundation/target",
                serde_json::json!("x86_64-unknown-linux-gnu"),
            ),
            ("/foundation/panic", serde_json::json!("abort")),
            ("/foundation/handler_abi", serde_json::json!(4)),
            ("/foundation/tls", serde_json::json!("native")),
            ("/logic/codegen_units", serde_json::json!(2)),
            ("/logic/lto", serde_json::json!("thin")),
            ("/foundation/std_dll/sha256", serde_json::json!("bad")),
        ] {
            let mut changed = original.clone();
            *changed.pointer_mut(pointer).unwrap() = replacement;
            std::fs::write(&path, serde_json::to_vec(&changed).unwrap()).unwrap();
            assert!(
                matches!(LayeredArtifact::read(&path), Err(JitError::Manifest(_))),
                "{pointer}"
            );
        }
        let mut changed = original;
        changed["unexpected"] = true.into();
        std::fs::write(&path, serde_json::to_vec(&changed).unwrap()).unwrap();
        assert!(matches!(
            LayeredArtifact::read(&path),
            Err(JitError::Manifest(_))
        ));
    }
    #[test]
    fn hashes_sizes_and_resource_limits_are_checked_without_executing_code() {
        let (path, mut manifest) = fixture();
        manifest.logic.artifact.sha256 = digest(b"wrong");
        write(&path, &manifest);
        assert!(matches!(
            LayeredArtifact::read(&path),
            Err(JitError::Identity(_))
        ));
        assert!(matches!(
            LayeredArtifact::read_with_limit(&path, 0),
            Err(JitError::InvalidLimits)
        ));
        manifest.logic.artifact.bytes = u64::MAX;
        write(&path, &manifest);
        assert!(matches!(
            LayeredArtifact::read(&path),
            Err(JitError::Manifest(_))
        ));
        std::fs::write(&path, vec![b' '; MAX_MANIFEST_BYTES + 1]).unwrap();
        assert!(matches!(
            LayeredArtifact::read(&path),
            Err(JitError::Manifest(_))
        ));
    }
}
