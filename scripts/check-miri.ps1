param(
    [string]$Toolchain = 'nightly',
    [ValidateSet('Stacked', 'Tree', 'Both')]
    [string]$BorrowModel = 'Both',
    [uint32]$Seed = 0
)

$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $false
$taskRoot = Split-Path -Parent $PSScriptRoot
$taskLogDir = Join-Path $taskRoot "target/miri-audit/seed-$Seed"
$taskOldMiriFlags = [Environment]::GetEnvironmentVariable('MIRIFLAGS')
$taskResults = @()
$taskModels = if ($BorrowModel -eq 'Both') { @('Stacked', 'Tree') } else { @($BorrowModel) }
$taskGroups = @(
    @{ Name = 'generation'; Args = @('--lib', 'generation::') },
    @{ Name = 'generation-scope'; Args = @('--lib', 'generation_scope::') },
    @{ Name = 'host-notifications'; Args = @('--lib', 'notification::') },
    @{ Name = 'notification-unwind'; Args = @('--test', 'notification_unwind') },
    @{ Name = 'generation-locals'; Args = @('--test', 'generation_locals') },
    @{ Name = 'task'; Args = @('--lib', 'task::') },
    @{ Name = 'host-resources'; Args = @('--test', 'host_resources') },
    @{ Name = 'request-routing'; Args = @('--test', 'request_routing') },
    @{ Name = 'restart'; Args = @('--test', 'restart') },
    @{ Name = 'host-commit'; Args = @('--test', 'host_commit') },
    @{ Name = 'managed-activities'; Args = @('--test', 'managed_activities') },
    @{ Name = 'activity-notifications'; Args = @('--test', 'activity_notifications') },
    @{ Name = 'cancellation'; Args = @('--lib', 'async_service::tests::') },
    @{ Name = 'cancellation-notifications'; Args = @('--test', 'cancellation_notifications') },
    @{ Name = 'cleanup-notifications'; Args = @('--test', 'cleanup_notifications') },
    @{ Name = 'async-services'; Args = @('--test', 'async_services') },
    @{ Name = 'threaded-services'; Args = @('--test', 'threaded_services') },
    @{ Name = 'service-contracts'; Args = @('--test', 'service_contracts') },
    @{ Name = 'typed-handlers'; Args = @('--features', 'typed', '--test', 'typed_handlers') },
    @{ Name = 'typed-cancellation'; Args = @('--features', 'typed', '--test', 'typed_cancellation') },
    @{ Name = 'typed-services'; Args = @('--features', 'typed', '--test', 'typed_services') },
    @{ Name = 'abi-metadata'; Package = 'reknot_abi'; Args = @('--test', 'metadata') },
    @{ Name = 'typed-contracts'; Args = @('--features', 'typed', '--test', 'typed_contracts') },
    @{ Name = 'typed-restart'; Args = @('--features', 'typed', '--test', 'typed_restart') },
    @{ Name = 'typed-bridge'; Args = @('--features', 'typed', '--test', 'typed_bridge') }
)

Push-Location $taskRoot
try {
    New-Item -ItemType Directory -Path $taskLogDir -Force | Out-Null
    $taskRustVersion = & rustc "+$Toolchain" -Vv
    if ($LASTEXITCODE -ne 0) { throw "Rust toolchain '$Toolchain' is not available." }
    $taskSourceFiles = @(
        'Cargo.toml', 'reknot/Cargo.toml', 'reknot_abi/Cargo.toml', 'reknot_macros/Cargo.toml',
        'reknot_abi/src/lib.rs', 'reknot_macros/src/lib.rs', 'reknot/src/lib.rs',
        'reknot/src/internal.rs', 'reknot/src/generation.rs', 'reknot/src/native.rs',
        'reknot/src/generation_scope.rs', 'reknot/src/generation_local.rs',
        'reknot/src/notification.rs', 'reknot/tests/generation_locals.rs',
        'reknot/src/task/future.rs', 'reknot/src/task/waker.rs', 'reknot/src/task/mod.rs',
        'reknot/src/resources.rs', 'reknot/src/request.rs', 'reknot/src/routing.rs',
        'reknot_abi/src/services.rs', 'reknot_macros/src/handler.rs',
        'reknot/src/routing/control.rs', 'reknot/src/routing/limits.rs',
        'reknot/src/routing/restart.rs', 'reknot/tests/restart.rs',
        'reknot/src/retention.rs', 'reknot/src/cache.rs',
        'reknot/src/cache/session.rs', 'reknot/src/cache/permissions.rs',
        'reknot/tests/host_commit.rs',
        'reknot/src/activity.rs', 'reknot/tests/managed_activities.rs',
        'reknot/tests/activity_notifications.rs', 'reknot/src/async_service/tests.rs',
        'reknot/tests/cancellation_notifications.rs', 'reknot/tests/fixtures/cancellation_logic.rs',
        'reknot/tests/cleanup_notifications.rs', 'reknot/tests/notification_unwind.rs',
        'reknot/src/async_service.rs', 'reknot/src/resources/async_ops.rs',
        'reknot/src/thread_service.rs', 'reknot/tests/async_services.rs',
        'reknot/tests/threaded_services.rs',
        'reknot/src/environment.rs', 'reknot/tests/service_contracts.rs',
        'reknot/tests/host_resources.rs', 'reknot/tests/request_routing.rs',
        'reknot/src/typed/mod.rs', 'reknot/src/typed/runtime.rs', 'reknot_macros/src/typed.rs',
        'reknot/src/typed/bridge.rs', 'reknot/tests/typed_bridge.rs',
        'reknot/tests/typed_handlers.rs', 'reknot/tests/typed_services.rs',
        'reknot/tests/typed_cancellation.rs',
        'reknot/src/typed/schema.rs', 'reknot/src/typed/restart.rs',
        'reknot/tests/typed_contracts.rs', 'reknot/tests/typed_restart.rs', 'reknot_abi/tests/metadata.rs',
        'Cargo.lock'
    )
    $taskHashes = foreach ($taskFile in $taskSourceFiles) {
        if (Test-Path -LiteralPath $taskFile) {
            @{ path = $taskFile; sha256 = (Get-FileHash -LiteralPath $taskFile -Algorithm SHA256).Hash }
        }
    }
    @{
        toolchain = $Toolchain
        rustc = $taskRustVersion
        target = 'x86_64-unknown-linux-gnu'
        seed = $Seed
        source_files = @($taskHashes)
    } | ConvertTo-Json -Depth 4 | Set-Content (Join-Path $taskLogDir 'environment.json')

    foreach ($taskModel in $taskModels) {
        $env:MIRIFLAGS = "-Zmiri-backtrace=full -Zmiri-seed=$Seed"
        if ($taskModel -eq 'Tree') { $env:MIRIFLAGS += ' -Zmiri-tree-borrows' }
        foreach ($taskGroup in $taskGroups) {
            $taskLogName = "$($taskModel.ToLowerInvariant())-$($taskGroup.Name).log"
            $taskPackage = if ($taskGroup.Package) { $taskGroup.Package } else { 'reknot' }
            $taskArgs = @(
                "+$Toolchain", 'miri', 'test', '-p', $taskPackage,
                '--no-default-features', '--locked', '--target', 'x86_64-unknown-linux-gnu'
            ) + $taskGroup.Args
            Write-Host "Running $taskModel / $($taskGroup.Name) (seed $Seed)"
            & cargo @taskArgs 2>&1 | Tee-Object -FilePath (Join-Path $taskLogDir $taskLogName)
            $taskResults += [pscustomobject]@{
                model = $taskModel
                group = $taskGroup.Name
                exit_code = $LASTEXITCODE
                log = $taskLogName
            }
        }
    }
    $taskResults | ConvertTo-Json -Depth 3 | Set-Content (Join-Path $taskLogDir 'results.json')
    $taskResults | Format-Table -AutoSize
    $taskFailed = @($taskResults | Where-Object { $_.exit_code -ne 0 }).Count
}
finally {
    [Environment]::SetEnvironmentVariable('MIRIFLAGS', $taskOldMiriFlags)
    Pop-Location
}

if ($taskFailed -gt 0) { exit 1 }
