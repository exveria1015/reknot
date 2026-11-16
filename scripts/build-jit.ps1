param(
    [Parameter(Mandatory=$true)][string]$LlvmRoot,
    [Parameter(Mandatory=$true)][string]$ZlibSource,
    [Parameter(Mandatory=$true)][string]$ZstdSource,
    [string]$BuildDirectory = 'target/jit-bridge',
    [switch]$Testing
)
$ErrorActionPreference = 'Stop'
$taskRoot = Split-Path -Parent $PSScriptRoot
$taskCmake = Get-Command cmake -ErrorAction SilentlyContinue
if ($taskCmake) { $taskCmake = $taskCmake.Source }
else { $taskCmake = 'C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\Common7\IDE\CommonExtensions\Microsoft\CMake\CMake\bin\cmake.exe' }
Push-Location $taskRoot
try {
    $taskLlvm = (Resolve-Path -LiteralPath $LlvmRoot).Path
    $taskZlib = (Resolve-Path -LiteralPath $ZlibSource).Path
    $taskZstd = (Resolve-Path -LiteralPath $ZstdSource).Path
    $taskTesting = if ($Testing) { 'ON' } else { 'OFF' }
    & $taskCmake -S native/jit -B $BuildDirectory -G 'Visual Studio 17 2022' -A x64 "-DLLVM_ROOT=$taskLlvm" "-DZLIB_SOURCE_DIR=$taskZlib" "-DZSTD_SOURCE_DIR=$taskZstd" "-DREKNOT_JIT_TESTING=$taskTesting"
    if ($LASTEXITCODE -ne 0) { throw 'JIT bridge configuration failed' }
    & $taskCmake --build $BuildDirectory --config Release --parallel 4
    if ($LASTEXITCODE -ne 0) { throw 'JIT bridge build failed' }
    Get-Item -LiteralPath (Join-Path $BuildDirectory 'Release/reknot_jit_bridge.dll') | Select-Object FullName,Length
} finally { Pop-Location }
