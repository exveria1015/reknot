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
else {
    $taskVswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio/Installer/vswhere.exe'
    if (Test-Path -LiteralPath $taskVswhere) {
        $taskCmake = & $taskVswhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.CMake.Project -find 'Common7/IDE/CommonExtensions/Microsoft/CMake/CMake/bin/cmake.exe' | Select-Object -First 1
    }
    if (-not $taskCmake) { throw 'Install CMake on PATH or the Visual Studio C++ CMake tools' }
}
Push-Location $taskRoot
try {
    $taskLlvm = (Resolve-Path -LiteralPath $LlvmRoot).Path
    $taskZlib = (Resolve-Path -LiteralPath $ZlibSource).Path
    $taskZstd = (Resolve-Path -LiteralPath $ZstdSource).Path
    $taskTesting = if ($Testing) { 'ON' } else { 'OFF' }
    # Let CMake select the installed Visual Studio version or reuse the cached generator.
    & $taskCmake -S native/jit -B $BuildDirectory -A x64 "-DLLVM_ROOT=$taskLlvm" "-DZLIB_SOURCE_DIR=$taskZlib" "-DZSTD_SOURCE_DIR=$taskZstd" "-DREKNOT_JIT_TESTING=$taskTesting"
    if ($LASTEXITCODE -ne 0) { throw 'JIT bridge configuration failed' }
    & $taskCmake --build $BuildDirectory --config Release --parallel 4
    if ($LASTEXITCODE -ne 0) { throw 'JIT bridge build failed' }
    Get-Item -LiteralPath (Join-Path $BuildDirectory 'Release/reknot_jit_bridge.dll') | Select-Object FullName,Length
} finally { Pop-Location }
