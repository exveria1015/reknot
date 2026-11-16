param([string]$DependenciesDirectory = 'target/jit-deps')
$ErrorActionPreference = 'Stop'
$taskRoot = Split-Path -Parent $PSScriptRoot
$toolsDir = if ([IO.Path]::IsPathRooted($DependenciesDirectory)) {
    [IO.Path]::GetFullPath($DependenciesDirectory)
} else {
    [IO.Path]::GetFullPath((Join-Path $taskRoot $DependenciesDirectory))
}
New-Item -ItemType Directory -Path $toolsDir -Force | Out-Null
$dependencies = @(
    @{ File='llvm-23.1.0-windows-x64.tar.zst'; Folder='clang+llvm-23.1.0-x86_64-pc-windows-msvc'; Url='https://github.com/llvm/llvm-project/releases/download/llvmorg-23.1.0/clang%2Bllvm-23.1.0-x86_64-pc-windows-msvc.tar.zst'; Hash='1AEBF024B959B3835C3BD936DA2FA58CD002C61CCF47FCE1714447B900BD9837' },
    @{ File='zlib-1.3.1.tar.gz'; Folder='zlib-1.3.1'; Url='https://github.com/madler/zlib/releases/download/v1.3.1/zlib-1.3.1.tar.gz'; Hash='9A93B2B7DFDAC77CEBA5A558A580E74667DD6FEDE4585B91EEFB60F03B72DF23' },
    @{ File='zstd-1.5.7.tar.gz'; Folder='zstd-1.5.7'; Url='https://github.com/meta/zstd/releases/download/v1.5.7/zstd-1.5.7.tar.gz'; Hash='EB33E51F49A15E023950CD7825CA74A4A2B43DB8354825AC24FC1B7EE09E6FA3' }
)
foreach ($dep in $dependencies) {
    $archive = Join-Path $toolsDir $dep.File
    if (!(Test-Path -LiteralPath $archive)) {
        & curl.exe --fail --location --output $archive $dep.Url
        if ($LASTEXITCODE -ne 0) { throw "Download failed: $($dep.File)" }
    }
    if ((Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash -ne $dep.Hash) {
        throw "Archive hash mismatch: $archive"
    }
    if (!(Test-Path -LiteralPath (Join-Path $toolsDir $dep.Folder))) {
        # These two test-only symlinks require Windows symlink privileges and
        # are not used to compile the library. No system installation is made.
        & tar.exe -xf $archive -C $toolsDir --exclude 'zstd-1.5.7/tests/cli-tests/bin/unzstd' --exclude 'zstd-1.5.7/tests/cli-tests/bin/zstdcat'
        if ($LASTEXITCODE -ne 0) { throw "Extraction failed: $archive" }
    }
}
