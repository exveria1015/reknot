#requires -Version 7.0
param(
    [Parameter(Mandatory=$true)][string]$ManifestPath,
    [Parameter(Mandatory=$true)][string]$Package,
    [Parameter(Mandatory=$true)][string]$TargetDirectory,
    [Parameter(Mandatory=$true)][string]$SupportDll,
    [Parameter(Mandatory=$true)][string]$Output,
    [string]$Toolchain = 'stable',
    [string]$FoundationManifest = '',
    [string]$ReceiptPath = '',
    [string]$CacheDirectory = '',
    [string]$SessionId = '',
    [UInt64]$Revision = 0,
    [UInt64]$JobId = 0,
    [string]$FoundationBuildKey = '',
    [UInt64]$MaxCacheBytes = 4294967296,
    [ValidateRange(0,3)][int]$Optimization = 0,
    [ValidateSet('0','1','2','line-tables-only')][string]$DebugInfo = '0',
    [string[]]$Features = @(),
    [switch]$NoDefaultFeatures,
    [switch]$Offline
)
$ErrorActionPreference = 'Stop'
$Features = @($Features | ForEach-Object { $_ -split ',' } | Where-Object { $_ })

function Get-LocalPath([string]$FilePath) {
    $taskFullPath = [IO.Path]::GetFullPath($FilePath)
    if ($taskFullPath.StartsWith('\\?\UNC\')) { return '\\' + $taskFullPath.Substring(8) }
    if ($taskFullPath.StartsWith('\\?\')) { return $taskFullPath.Substring(4) }
    $taskFullPath
}

function Invoke-CargoJson([string[]]$CommandArguments) {
    $taskLines = @(& cargo @CommandArguments)
    if ($LASTEXITCODE -ne 0) { throw 'Cargo failed; no manifest will be published' }
    foreach ($taskLine in $taskLines) {
        $taskMessage = $taskLine | ConvertFrom-Json
        if ($taskMessage.reason -eq 'compiler-message' -and $taskMessage.message.rendered) {
            [Console]::Error.Write($taskMessage.message.rendered)
        }
        $taskMessage
    }
}
function Open-Frozen([string]$FilePath) {
    [IO.File]::Open($FilePath, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
}
function Get-StreamHash([IO.Stream]$Stream) {
    $Stream.Position = 0
    $taskHasher = [Security.Cryptography.SHA256]::Create()
    try { $taskHash = [Convert]::ToHexString($taskHasher.ComputeHash($Stream)).ToLowerInvariant() }
    finally { $taskHasher.Dispose(); $Stream.Position = 0 }
    $taskHash
}
function Save-Immutable([IO.Stream]$Stream, [string]$Destination, [string]$ExpectedHash) {
    [IO.Directory]::CreateDirectory([IO.Path]::GetDirectoryName($Destination)) | Out-Null
    if (![IO.File]::Exists($Destination)) {
        $taskTemporary = $Destination + '.' + [Guid]::NewGuid().ToString('N') + '.tmp'
        try {
            $taskWriter = [IO.File]::Open($taskTemporary, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
            try { $Stream.Position = 0; $Stream.CopyTo($taskWriter); $taskWriter.Flush($true) }
            finally { $taskWriter.Dispose(); $Stream.Position = 0 }
            try { [IO.File]::Move($taskTemporary, $Destination, $false) }
            catch { if (![IO.File]::Exists($Destination)) { throw } }
        } finally { if ([IO.File]::Exists($taskTemporary)) { [IO.File]::Delete($taskTemporary) } }
    }
    $taskExisting = Open-Frozen $Destination
    try {
        if ((Get-StreamHash $taskExisting) -ne $ExpectedHash -or $taskExisting.Length -ne $Stream.Length) {
            throw "Immutable artifact cache differs: $Destination"
        }
    } finally { $taskExisting.Dispose() }
    $Destination
}
function Get-DllArtifact($Records, [string]$PackageId) {
    $taskMatches = @($Records | Where-Object {
        $_.reason -eq 'compiler-artifact' -and $_.package_id -eq $PackageId -and $_.target.crate_types -contains 'dylib'
    })
    if ($taskMatches.Count -ne 1) { throw 'Expected one Cargo support dylib artifact' }
    $taskMatches[0]
}

function Write-Receipt($Record) {
    if (!$ReceiptPath) { return }
    $taskReceipt = Get-LocalPath $ReceiptPath
    [IO.Directory]::CreateDirectory([IO.Path]::GetDirectoryName($taskReceipt)) | Out-Null
    $taskTempReceipt = "$taskReceipt.$([Guid]::NewGuid().ToString('N')).tmp"
    try {
        [IO.File]::WriteAllText($taskTempReceipt, ($Record | ConvertTo-Json -Depth 8), [Text.UTF8Encoding]::new($false))
        [IO.File]::Move($taskTempReceipt, $taskReceipt, $false)
    } finally { if ([IO.File]::Exists($taskTempReceipt)) { [IO.File]::Delete($taskTempReceipt) } }
}
function Assert-CacheCapacity([string]$Directory, [UInt64]$Additional = 0) {
    $taskUsed = (Get-ChildItem -LiteralPath $Directory -Recurse -File -ErrorAction Stop | Measure-Object Length -Sum).Sum
    if ($taskUsed + $Additional -ge $MaxCacheBytes) {
        $script:taskFailureCode = 'CacheFull'
        throw 'Layered cache capacity reached; no automatic deletion is performed'
    }
}
$taskFailureCode = 'BuildFailed'
$taskWriterLock = $null
$taskOldLocation = Get-Location
try {

$taskManifest = Get-LocalPath (Resolve-Path -LiteralPath $ManifestPath).ProviderPath
$taskTarget = Get-LocalPath $TargetDirectory
$taskSupport = Get-LocalPath $SupportDll
$taskOutput = Get-LocalPath $Output
$taskParent = [IO.Path]::GetDirectoryName($taskOutput)
[IO.Directory]::CreateDirectory($taskParent) | Out-Null
$taskCache = if ($CacheDirectory) { Get-LocalPath $CacheDirectory } else { $taskParent }
[IO.Directory]::CreateDirectory($taskCache) | Out-Null
if ($ReceiptPath -and ([IO.File]::Exists($taskOutput) -or [IO.File]::Exists("$taskOutput.json"))) {
    throw 'A development job output must be unique'
}
# Serialize all cooperating builders for this mutable Cargo target, including
# metadata, support capture and immutable publication. Wait only in the worker.
[IO.Directory]::CreateDirectory($taskTarget) | Out-Null
$taskLockWait = [Diagnostics.Stopwatch]::StartNew()
while ($null -eq $taskWriterLock) {
    try { $taskWriterLock = [IO.File]::Open((Join-Path $taskTarget '.reknot-layered-writer.lock'), [IO.FileMode]::OpenOrCreate, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None) }
    catch [IO.IOException] {
        if ($taskLockWait.Elapsed.TotalSeconds -gt 120) { throw 'Timed out waiting for layered Cargo writer lock' }
        Start-Sleep -Milliseconds 50
    }
}
$taskLockWait.Stop()
Assert-CacheCapacity $taskCache
Set-Location -LiteralPath ([IO.Path]::GetDirectoryName($taskManifest))
$taskStaging = $taskOutput + '.' + [Guid]::NewGuid().ToString('N') + '.obj'
$taskCompiler = & rustup which --toolchain $Toolchain rustc
if ($LASTEXITCODE -ne 0) { throw 'Cannot resolve the selected compiler' }
$taskInfo = @(& $taskCompiler -vV) -join "`n"
if ($LASTEXITCODE -ne 0 -or $taskInfo -notmatch 'host: x86_64-pc-windows-msvc') {
    throw 'The layered builder requires an x86_64-pc-windows-msvc compiler'
}
$taskLibDir = & $taskCompiler --print target-libdir --target x86_64-pc-windows-msvc
if ($LASTEXITCODE -ne 0) { throw 'Cannot locate the compiler runtime' }
$taskStd = @(Get-ChildItem -LiteralPath $taskLibDir -Filter 'std-*.dll' -File)
if ($taskStd.Count -ne 1) { throw 'Expected exactly one matching Rust std DLL' }
$taskCommonFlags = '-C prefer-dynamic=yes -C force-unwind-tables=yes -C panic=unwind -C lto=off'
$taskSavedEnvironment = @{}
foreach ($taskName in @('RUSTC','RUSTFLAGS','CARGO_ENCODED_RUSTFLAGS','CARGO_INCREMENTAL')) {
    $taskSavedEnvironment[$taskName] = [Environment]::GetEnvironmentVariable($taskName, 'Process')
}
$taskHandles = [Collections.Generic.List[IDisposable]]::new()
$taskWatch = [Diagnostics.Stopwatch]::StartNew()
try {
    $env:RUSTC = $taskCompiler
    $env:RUSTFLAGS = $taskCommonFlags
    Remove-Item Env:CARGO_ENCODED_RUSTFLAGS -ErrorAction SilentlyContinue
    $env:CARGO_INCREMENTAL = '0'
    if ($env:RUSTC_WRAPPER -or $env:RUSTC_WORKSPACE_WRAPPER) {
        throw 'Compiler wrappers need their own provenance contract; this builder does not accept them'
    }
    $taskStdFile = Open-Frozen $taskStd[0].FullName
    $taskHandles.Add($taskStdFile)
    $taskStdHash = Get-StreamHash $taskStdFile
    $taskBaseArgs = @("+$Toolchain", '--locked', '--manifest-path', $taskManifest)
    if ($Offline) { $taskBaseArgs += '--offline' }
    $taskBuildBase = $taskBaseArgs[1..($taskBaseArgs.Count-1)] + @('--target','x86_64-pc-windows-msvc','--target-dir',$taskTarget,'--message-format','json-render-diagnostics')
    if ($FoundationManifest) {
        $taskPrior = Get-Content -Raw -LiteralPath $FoundationManifest | ConvertFrom-Json
        if ($taskPrior.schema -ne 2 -or $taskPrior.profile -ne 'layered-rust-library-object' -or
            $taskPrior.foundation.rustc.Replace("`r`n", "`n").Trim() -ne $taskInfo -or
            $taskPrior.foundation.rustflags -ne $taskCommonFlags -or
            $taskPrior.foundation.std_dll.sha256 -ne $taskStdHash -or
            $taskPrior.foundation.std_dll.bytes -ne $taskStdFile.Length) {
            throw 'The requested foundation differs from this compiler/std/profile; rebuild the foundation and restart the host'
        }
        $taskWorkspaceManifest = & cargo "+$Toolchain" locate-project --workspace --message-format plain --manifest-path $taskManifest
        if ($LASTEXITCODE -ne 0) { throw 'Cannot locate the Cargo workspace' }
        $taskLock = Open-Frozen (Join-Path ([IO.Path]::GetDirectoryName($taskWorkspaceManifest)) 'Cargo.lock')
        $taskHandles.Add($taskLock)
        $taskLockHash = Get-StreamHash $taskLock
        if ($taskLockHash -ne $taskPrior.foundation.cargo_lock_sha256) { throw 'Cargo.lock changed; rebuild the foundation and restart the host' }
        $taskSupportId = $taskPrior.foundation.support_build.package_id
        $taskLogicId = $Package
    } else {
        $taskMetadataArgs = @($taskBaseArgs[0], 'metadata') + $taskBaseArgs[1..($taskBaseArgs.Count-1)] + @('--format-version','1','--filter-platform','x86_64-pc-windows-msvc')
        if ($NoDefaultFeatures) { $taskMetadataArgs += '--no-default-features' }
        if ($Features.Count -gt 0) {
            $taskMetadataFeatures = @($Features | ForEach-Object { if ($_.Contains('/')) { $_ } else { "$Package/$_" } })
            $taskMetadataArgs += @('--features', ($taskMetadataFeatures -join ','))
        }
        $taskMetadata = @(Invoke-CargoJson $taskMetadataArgs)[0]
        $taskLogicPackages = @($taskMetadata.packages | Where-Object { $_.name -eq $Package })
        $taskSupportName = [IO.Path]::GetFileNameWithoutExtension($taskSupport)
        $taskSupportPackages = @($taskMetadata.packages | Where-Object {
            @($_.targets | Where-Object { $_.name -eq $taskSupportName -and $_.crate_types -contains 'dylib' }).Count -eq 1
        })
        if ($taskLogicPackages.Count -ne 1 -or $taskSupportPackages.Count -ne 1) {
            throw 'Select one logic package and one support dylib target from the Cargo dependency graph'
        }
        $taskLogicId = $taskLogicPackages[0].id
        $taskSupportId = $taskSupportPackages[0].id
        $taskNodes = @{}
        foreach ($taskNode in $taskMetadata.resolve.nodes) { $taskNodes[$taskNode.id] = $taskNode }
        $taskPending = [Collections.Generic.Stack[string]]::new()
        $taskSeen = [Collections.Generic.HashSet[string]]::new()
        $taskPending.Push($taskLogicId)
        while ($taskPending.Count -gt 0) {
            $taskId = $taskPending.Pop()
            if ($taskSeen.Add($taskId)) { foreach ($taskDependency in $taskNodes[$taskId].deps) { $taskPending.Push($taskDependency.pkg) } }
        }
        if ($taskSupportId -eq $taskLogicId -or !$taskSeen.Contains($taskSupportId)) {
            throw 'The selected support dylib is not a dependency of the logic package'
        }
        $taskLock = Open-Frozen (Join-Path $taskMetadata.workspace_root 'Cargo.lock')
        $taskHandles.Add($taskLock)
        $taskLockHash = Get-StreamHash $taskLock
    }
    # Resolve support features in the same Cargo invocation that compiles logic.
    $taskFlags = @('-C','codegen-units=1','-C','lto=off','-C','panic=unwind',
        '-C',"opt-level=$Optimization",'-C',"debuginfo=$DebugInfo","--emit=obj=$taskStaging")
    $taskBuildLogic = @($taskBaseArgs[0], 'rustc') + $taskBuildBase + @('--package',$taskLogicId,'--lib','--crate-type','lib')
    if ($NoDefaultFeatures) { $taskBuildLogic += '--no-default-features' }
    if ($Features.Count -gt 0) { $taskBuildLogic += @('--features', ($Features -join ',')) }
    $taskBuildLogic += @('--') + $taskFlags
    $taskLogicArtifacts = @(Invoke-CargoJson $taskBuildLogic)
    $taskSupportArtifact = Get-DllArtifact $taskLogicArtifacts $taskSupportId
    $taskDlls = @($taskSupportArtifact.filenames | Where-Object { [IO.Path]::GetExtension($_) -eq '.dll' })
    if ($taskDlls.Count -ne 1 -or (Get-LocalPath $taskDlls[0]) -ne $taskSupport -or $taskSupportArtifact.profile.test) {
        throw 'SupportDll must be the actual DLL reported by Cargo for the selected dependency'
    }
    $taskSupportFile = Open-Frozen $taskDlls[0]
    $taskHandles.Add($taskSupportFile)
    $taskSupportHash = Get-StreamHash $taskSupportFile
    if ($FoundationManifest) {
        $taskExpected = $taskPrior.foundation.support_build
        $taskProfile = $taskSupportArtifact.profile
        if ($taskSupportHash -ne $taskPrior.foundation.support_dll.sha256 -or
            $taskSupportFile.Length -ne $taskPrior.foundation.support_dll.bytes -or
            (($taskSupportArtifact.features | Sort-Object) -join ',') -ne (($taskExpected.features | Sort-Object) -join ',') -or
            $taskProfile.opt_level -ne $taskExpected.opt_level -or $taskProfile.debuginfo -ne $taskExpected.debuginfo -or
            $taskProfile.debug_assertions -ne $taskExpected.debug_assertions -or $taskProfile.overflow_checks -ne $taskExpected.overflow_checks) {
            throw 'Logic support build differs from the requested foundation; no manifest will be published'
        }
    }
    if ((@(& $taskCompiler -vV) -join "`n") -ne $taskInfo) { throw 'Compiler identity changed during build' }
    $taskObjectFile = Open-Frozen $taskStaging
    $taskHandles.Add($taskObjectFile)
    $taskObjectHash = Get-StreamHash $taskObjectFile
    $taskKeyBytes = [Text.Encoding]::UTF8.GetBytes("$taskStdHash`n$taskSupportHash`n$taskInfo")
    $taskKey = [Convert]::ToHexString([Security.Cryptography.SHA256]::HashData($taskKeyBytes)).ToLowerInvariant()
    Assert-CacheCapacity $taskCache ($taskStdFile.Length + $taskSupportFile.Length + 2 * $taskObjectFile.Length)
    $taskFoundationDir = Join-Path $taskCache "foundation/$taskKey"
    $taskStdCopy = Save-Immutable $taskStdFile (Join-Path $taskFoundationDir $taskStd[0].Name) $taskStdHash
    $taskSupportCopy = Save-Immutable $taskSupportFile (Join-Path $taskFoundationDir ([IO.Path]::GetFileName($taskSupport))) $taskSupportHash
    $taskObjectCopy = Save-Immutable $taskObjectFile (Join-Path $taskCache "objects/$taskObjectHash/logic.obj") $taskObjectHash
    $taskRecord = [ordered]@{
        schema = 2
        profile = 'layered-rust-library-object'
        foundation = [ordered]@{
            rustc = $taskInfo
            target = 'x86_64-pc-windows-msvc'
            handler_abi = 5
            panic = 'unwind'
            tls = 'generation-local'
            prefer_dynamic = $true
            rustflags = $taskCommonFlags
            cargo_lock_sha256 = $taskLockHash
            support_build = [ordered]@{
                package_id = $taskSupportId
                features = @($taskSupportArtifact.features | Sort-Object)
                opt_level = $taskSupportArtifact.profile.opt_level
                debuginfo = $taskSupportArtifact.profile.debuginfo
                debug_assertions = $taskSupportArtifact.profile.debug_assertions
                overflow_checks = $taskSupportArtifact.profile.overflow_checks
            }
            std_dll = [ordered]@{ path=$taskStdCopy; sha256=$taskStdHash; bytes=$taskStdFile.Length }
            support_dll = [ordered]@{ path=$taskSupportCopy; sha256=$taskSupportHash; bytes=$taskSupportFile.Length }
        }
        logic = [ordered]@{
            artifact = [ordered]@{ path=$taskObjectCopy; sha256=$taskObjectHash; bytes=$taskObjectFile.Length }
            crate_type = 'lib'
            codegen_units = 1
            lto = 'off'
            opt_level = $Optimization
            debuginfo = $DebugInfo
        }
    }
    # The manifest references immutable content-named files. Publishing a new
    # manifest cannot mix a new foundation with a stale or overwritten object.
    $taskManifestTemp = $taskOutput + '.' + [Guid]::NewGuid().ToString('N') + '.json'
    try {
        [IO.File]::WriteAllText($taskManifestTemp, ($taskRecord | ConvertTo-Json -Depth 8), [Text.UTF8Encoding]::new($false))
        [IO.File]::Copy($taskObjectCopy, $taskOutput, $true)
        [IO.File]::Move($taskManifestTemp, "$taskOutput.json", $true)
    } finally { if ([IO.File]::Exists($taskManifestTemp)) { [IO.File]::Delete($taskManifestTemp) } }
    $taskWatch.Stop()
    Write-Receipt ([ordered]@{
        version = 1; status = 'Built'; session = $SessionId; revision = $Revision; job = $JobId
        foundation_build_key = $FoundationBuildKey; reused = [bool]$FoundationManifest
        manifest = "$taskOutput.json"; manifest_sha256 = (Get-FileHash -LiteralPath "$taskOutput.json" -Algorithm SHA256).Hash.ToLowerInvariant()
        object_sha256 = $taskObjectHash; build_ms = $taskWatch.Elapsed.TotalMilliseconds
        lock_wait_ms = $taskLockWait.Elapsed.TotalMilliseconds
    })
    [Console]::Error.WriteLine("Layered build and identity capture: {0:N1} ms", $taskWatch.Elapsed.TotalMilliseconds)
    $taskRecord | ConvertTo-Json -Depth 8
} finally {
    foreach ($taskHandle in $taskHandles) { $taskHandle.Dispose() }
    foreach ($taskName in $taskSavedEnvironment.Keys) {
        [Environment]::SetEnvironmentVariable($taskName, $taskSavedEnvironment[$taskName], 'Process')
    }
    if ([IO.File]::Exists($taskStaging)) { [IO.File]::Delete($taskStaging) }
}
} catch {
    Write-Receipt ([ordered]@{ version = 1; status = $taskFailureCode; session = $SessionId
        revision = $Revision; job = $JobId; message = $_.Exception.Message })
    throw
} finally {
    if ($taskWriterLock) { $taskWriterLock.Dispose() }
    Set-Location -LiteralPath $taskOldLocation.Path
}
