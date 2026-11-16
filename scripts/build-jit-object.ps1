param(
    [Parameter(Mandatory=$true)][string]$Source,
    [Parameter(Mandatory=$true)][string]$Output,
    [string]$Toolchain = 'nightly',
    [ValidateRange(0,3)][int]$Optimization = 1,
    [string[]]$Cfg = @()
)
$ErrorActionPreference = 'Stop'
$taskSource = (Resolve-Path -LiteralPath $Source).Path
$taskOutput = [IO.Path]::GetFullPath($Output)
$taskInfo = & rustc "+$Toolchain" -vV
if ($LASTEXITCODE -ne 0) { throw 'Cannot read the selected Rust compiler' }
if (($taskInfo -join "`n") -notmatch 'host: x86_64-pc-windows-msvc') {
    throw 'The JIT object builder requires an x86_64-pc-windows-msvc compiler'
}
$taskLibDir = & rustc "+$Toolchain" --print target-libdir
if ($LASTEXITCODE -ne 0) { throw 'Cannot locate the compiler runtime' }
$taskStd = @(Get-ChildItem -LiteralPath $taskLibDir -Filter 'std-*.dll' -File)
if ($taskStd.Count -ne 1) { throw 'Expected exactly one matching Rust std DLL' }
New-Item -ItemType Directory -Force -Path (Split-Path -Parent $taskOutput) | Out-Null
$taskFlags = @('--crate-name', 'reknot_jit_logic', '--crate-type', 'cdylib', '--edition=2021',
    '--emit=obj', '-C', 'codegen-units=1', '-C', "opt-level=$Optimization", '-C', 'debuginfo=2',
    '-C', 'prefer-dynamic=yes', '-C', 'panic=unwind', '-C', 'force-unwind-tables=yes',
    '-Z', 'tls-model=emulated')
foreach ($taskCfg in $Cfg) { $taskFlags += @('--cfg', $taskCfg) }
& rustc "+$Toolchain" @taskFlags $taskSource -o $taskOutput
if ($LASTEXITCODE -ne 0) { throw 'COFF compilation failed; emulated TLS requires nightly Rust' }
$taskManifest = [ordered]@{
    schema = 1
    target = 'x86_64-pc-windows-msvc'
    rustc = $taskInfo -join "`n"
    rust_std_dll = $taskStd[0].FullName
    rust_std_sha256 = (Get-FileHash -LiteralPath $taskStd[0].FullName -Algorithm SHA256).Hash
    object = $taskOutput
    object_sha256 = (Get-FileHash -LiteralPath $taskOutput -Algorithm SHA256).Hash
    flags = $taskFlags
}
$taskManifest | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath "$taskOutput.json" -Encoding utf8
$taskManifest | ConvertTo-Json -Depth 4
