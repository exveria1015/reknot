param([switch]$Offline)

$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $false
$taskRoot = Split-Path -Parent $PSScriptRoot
$taskOldCacheRoot = [Environment]::GetEnvironmentVariable('REKNOT_CACHE_ROOT')
[string[]]$taskResolution = @()
if ($Offline) { $taskResolution += '--offline' }

function Invoke-CheckedCargo {
    param([string[]]$CargoArguments)
    & cargo @CargoArguments
    if ($LASTEXITCODE -ne 0) {
        throw "cargo $($CargoArguments -join ' ') failed with exit code $LASTEXITCODE"
    }
}

Push-Location $taskRoot
try {
    $taskMetadataJson = & cargo metadata --no-deps --format-version 1 --locked @taskResolution
    if ($LASTEXITCODE -ne 0) { throw 'Cannot read package metadata' }
    $taskMetadata = $taskMetadataJson | ConvertFrom-Json
    $taskNames = @('reknot_abi', 'reknot_macros', 'reknot')
    $taskPackages = @($taskMetadata.packages | Where-Object { $_.name -in $taskNames })
    if ($taskPackages.Count -ne 3) { throw 'Cannot identify all three SDK packages' }

    Invoke-CheckedCargo (@(
        'package', '-p', 'reknot_abi', '-p', 'reknot_macros', '-p', 'reknot',
        '--allow-dirty', '--no-verify', '--locked'
    ) + $taskResolution)

    # Nested fixture builds must leave room for Cargo's intermediate paths and
    # MSVC's path limit. New-Item refuses an existing directory; never reuse one.
    $taskStage = Join-Path $taskMetadata.target_directory ('package-checks/' + [guid]::NewGuid().ToString('N').Substring(0, 8))
    New-Item -ItemType Directory -Path $taskStage | Out-Null
    $taskMembers = @()
    $taskPatches = @()
    $taskArchives = @()
    foreach ($taskPackage in $taskPackages) {
        $taskDirectory = "$($taskPackage.name)-$($taskPackage.version)"
        $taskArchive = Join-Path $taskMetadata.target_directory "package/$taskDirectory.crate"
        & tar -xzf $taskArchive -C $taskStage
        if ($LASTEXITCODE -ne 0) { throw "Cannot extract $taskArchive" }
        foreach ($taskRequired in @('Cargo.toml', 'README.md', 'LICENSE-MIT', 'LICENSE-APACHE')) {
            if (-not (Test-Path -LiteralPath (Join-Path $taskStage "$taskDirectory/$taskRequired"))) {
                throw "$taskDirectory is missing $taskRequired"
            }
        }
        $taskMembers += '"' + $taskDirectory + '"'
        $taskPatches += "$($taskPackage.name) = { path = `"$taskDirectory`" }"
        $taskArchives += @{
            name = $taskPackage.name
            version = $taskPackage.version
            sha256 = (Get-FileHash -LiteralPath $taskArchive -Algorithm SHA256).Hash
        }
    }

    $taskMembers += '"consumer"'
    $taskWorkspace = @"
[workspace]
members = [$($taskMembers -join ', ')]
resolver = "2"

[patch.crates-io]
$($taskPatches -join "`n")
"@
    Set-Content -LiteralPath (Join-Path $taskStage 'Cargo.toml') -Value $taskWorkspace -Encoding utf8
    # Standalone SDK fixtures create their own workspace below target/tmp.
    # Cargo config keeps their transitive SDK dependencies on these archives too.
    $taskConfigDirectory = Join-Path $taskStage '.cargo'
    New-Item -ItemType Directory -Path $taskConfigDirectory -Force | Out-Null
    $taskConfig = "[patch.crates-io]`n" + ($taskPatches -join "`n")
    Set-Content -LiteralPath (Join-Path $taskConfigDirectory 'config.toml') -Value $taskConfig -Encoding utf8
    Copy-Item -LiteralPath (Join-Path $taskRoot 'Cargo.lock') -Destination (Join-Path $taskStage 'Cargo.lock')
    $taskConsumer = Join-Path $taskStage 'consumer/src'
    New-Item -ItemType Directory -Path $taskConsumer -Force | Out-Null
    $taskVersion = ($taskPackages | Where-Object { $_.name -eq 'reknot' }).version
    $taskConsumerManifest = @"
[package]
name = "package_consumer"
version = "0.0.0"
edition = "2021"
publish = false

[dependencies]
reknot = { version = "=$taskVersion", default-features = false }
"@
    Set-Content -LiteralPath (Join-Path $taskStage 'consumer/Cargo.toml') -Value $taskConsumerManifest -Encoding utf8
    $taskConsumerSource = @'
use reknot::{HostResource, HostResources, RequestContext, RequestRouter, RunStatus, ServiceError, ServiceKey};
use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

const KEY: ServiceKey = ServiceKey::new(0x5041_434b, 1);
struct Sink(Rc<RefCell<Vec<u8>>>);
impl HostResource for Sink {
    fn call(&mut self, _: u32, input: &[u8], _: &mut [u8]) -> Result<usize, ServiceError> {
        self.0.borrow_mut().extend_from_slice(input);
        Ok(0)
    }
}

#[reknot::reload_handler(KEY)]
async fn handle(ctx: RequestContext) -> Result<(), ServiceError> {
    ctx.call(0, ctx.input(), &mut [])?;
    Ok(())
}

fn main() {
    let received = Rc::new(RefCell::new(Vec::new()));
    let resources = HostResources::new();
    let sink = resources.register(KEY, Sink(received.clone()));
    // All callbacks and their statically linked code live for the process.
    let router = unsafe { RequestRouter::from_api(handler_api(), resources) }.unwrap();
    let mut request = router.dispatch(sink.id(), b"packaged SDK").unwrap();
    assert_eq!(Pin::new(&mut request).poll(&mut Context::from_waker(Waker::noop())), Poll::Ready(RunStatus::Done));
    assert_eq!(&*received.borrow(), b"packaged SDK");
}
'@
    Set-Content -LiteralPath (Join-Path $taskConsumer 'main.rs') -Value $taskConsumerSource -Encoding utf8
    $taskManifest = Join-Path $taskStage 'Cargo.toml'
    $env:REKNOT_CACHE_ROOT = Join-Path $taskStage 'native-cache'
    # The copied workspace lock is pruned for the extracted packages and consumer.
    Invoke-CheckedCargo (@(
        'check', '--manifest-path', $taskManifest, '--workspace', '--all-targets', '--all-features'
    ) + $taskResolution)
    Invoke-CheckedCargo (@(
        'check', '--manifest-path', $taskManifest, '-p', 'reknot',
        '--all-targets', '--no-default-features', '--locked'
    ) + $taskResolution)
    Invoke-CheckedCargo (@(
        'test', '--manifest-path', $taskManifest, '-p', 'reknot', '--test', 'native_routing',
        '--locked'
    ) + $taskResolution + @(
        '--', 'version_four_header_is_rejected_without_reading_extended_manifest', '--exact'
    ))
    Invoke-CheckedCargo (@(
        'test', '--manifest-path', $taskManifest, '-p', 'reknot', '--test', 'native_reclamation',
        '--locked'
    ) + $taskResolution + @('--', '--test-threads=1'))
    Invoke-CheckedCargo (@(
        'test', '--manifest-path', $taskManifest, '-p', 'reknot', '--test', 'native_managed_generation',
        '--locked'
    ) + $taskResolution + @('--', '--test-threads=1'))
    Invoke-CheckedCargo (@(
        'test', '--manifest-path', $taskManifest, '-p', 'reknot',
        '--test', 'cancellation_notifications', '--test', 'generation_locals', '--test', 'native_cancellation',
        '--test', 'cleanup_notifications', '--test', 'notification_unwind', '--test', 'source_watcher',
        '--locked'
    ) + $taskResolution + @('--', '--test-threads=1'))
    Invoke-CheckedCargo (@(
        'test', '--manifest-path', $taskManifest, '-p', 'reknot', '--test', 'native_typed', '--locked'
    ) + $taskResolution + @('--', 'native_watch_', '--ignored', '--test-threads=1'))
    Invoke-CheckedCargo (@(
        'test', '--manifest-path', $taskManifest, '-p', 'reknot', '--no-default-features',
        '--features', 'typed', '--test', 'typed_handlers', '--test', 'typed_services',
        '--test', 'typed_bridge', '--test', 'typed_contracts', '--test', 'typed_restart',
        '--test', 'typed_cancellation', '--test', 'scaffold', '--locked'
    ) + $taskResolution)
    $taskScaffoldFilter = if ($Offline) { @('--', 'generated_', '--ignored', '--test-threads=1') } else { @('--', '--ignored', '--test-threads=1') }
    Invoke-CheckedCargo (@(
        'test', '--manifest-path', $taskManifest, '-p', 'reknot', '--test', 'scaffold', '--locked'
    ) + $taskResolution + $taskScaffoldFilter)
    Invoke-CheckedCargo (@(
        'run', '--manifest-path', $taskManifest, '-p', 'package_consumer', '--locked'
    ) + $taskResolution)
    @{
        archives = $taskArchives
        extracted_workspace = $taskStage
        checks = @('all-targets/all-features', 'all-targets/no-default-features', 'standalone ABI DLL', 'explicit native reclamation', 'managed generation cleanup', 'composed cancellation, legacy ABI rejection and native reclamation', 'nested cleanup notification and host Waker destructor panic', 'low-level and typed watch registration after directory replacement', 'separate intermediate output exclusion and source-overlap rejection', 'typed handlers/services/bridge/contracts/restart/cancellation', 'generated static and native project', 'generated notification-driven console and cancellation', 'static consumer')
    } | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath (Join-Path $taskStage 'result.json') -Encoding utf8
    Write-Host "Verified extracted SDK packages and consumer: $taskStage"
} finally {
    [Environment]::SetEnvironmentVariable('REKNOT_CACHE_ROOT', $taskOldCacheRoot)
    Pop-Location
}
