$ErrorActionPreference = 'Stop'
$Root = Split-Path $PSScriptRoot -Parent

$Targets = [ordered]@{
    'arm64-v8a'   = @{ Triple = 'aarch64-linux-android';   Clang = 'aarch64-linux-android21-clang.cmd' }
    'armeabi-v7a' = @{ Triple = 'armv7-linux-androideabi'; Clang = 'armv7a-linux-androideabi21-clang.cmd' }
    'x86_64'      = @{ Triple = 'x86_64-linux-android';    Clang = 'x86_64-linux-android21-clang.cmd' }
    'x86'         = @{ Triple = 'i686-linux-android';      Clang = 'i686-linux-android21-clang.cmd' }
}

function Get-Setting([string]$Name) {
    $value = [Environment]::GetEnvironmentVariable($Name, 'Process')
    if (-not $value) { $value = [Environment]::GetEnvironmentVariable($Name, 'User') }
    if (-not $value) { $value = [Environment]::GetEnvironmentVariable($Name, 'Machine') }
    $value
}

# Paths that are this machine's own live in local.properties at the project root, which git
# leaves alone: sdk.dir, ndk.dir, java.home.
function Get-Local([string]$Name) {
    $file = Join-Path $Root 'local.properties'
    if (-not (Test-Path $file)) { return }
    foreach ($line in Get-Content $file) {
        if ($line -match "^\s*$([regex]::Escape($Name))\s*=\s*(.+?)\s*$") { return $Matches[1] }
    }
}

function Use-Rust {
    foreach ($name in 'RUSTUP_HOME', 'CARGO_HOME') {
        $value = Get-Setting $name
        if ($value) { Set-Item "env:$name" $value }
    }
    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
        $cargoHome = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $env:USERPROFILE '.cargo' }
        $env:Path = "$(Join-Path $cargoHome 'bin');$env:Path"
    }
    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) { throw 'cargo was not found; install Rust or set CARGO_HOME' }
    # Panic messages name source files by their paths, which are this machine's own; the
    # project folder and the toolchain's get short names instead, so the binaries carry neither.
    $sysroot = "$(& rustc --print sysroot)".Trim()
    $env:CARGO_ENCODED_RUSTFLAGS = ("--remap-path-prefix=$Root=.", "--remap-path-prefix=$sysroot=rust") -join [char]0x1f
}

function Find-Ndk {
    foreach ($name in 'ANDROID_NDK_HOME', 'ANDROID_NDK_ROOT', 'NDK_HOME') {
        $value = Get-Setting $name
        if ($value -and (Test-Path $value)) { return $value }
    }
    $named = Get-Local 'ndk.dir'
    if ($named -and (Test-Path $named)) { return $named }
    $sdkRoots = @((Get-Setting 'ANDROID_HOME'), (Get-Setting 'ANDROID_SDK_ROOT'), (Get-Local 'sdk.dir'), "$env:LOCALAPPDATA\Android\Sdk") | Where-Object { $_ }
    $found = foreach ($sdk in $sdkRoots) {
        Get-ChildItem (Join-Path $sdk 'ndk') -Directory -ErrorAction SilentlyContinue | Where-Object {
            $properties = Join-Path $_.FullName 'source.properties'
            (Test-Path $properties) -and -not (Select-String -Path $properties -Pattern 'Pkg\.Revision.*-(beta|rc)' -Quiet)
        }
    }
    $best = $found | Sort-Object { [version]$_.Name } -Descending | Select-Object -First 1
    if (-not $best) { throw 'Android NDK was not found; set ANDROID_NDK_HOME, or sdk.dir or ndk.dir in local.properties' }
    $best.FullName
}

function Use-Toolchain {
    Use-Rust
    $script:Ndk = Find-Ndk
    $bin = Join-Path $script:Ndk 'toolchains\llvm\prebuilt\windows-x86_64\bin'
    foreach ($target in $Targets.Values) {
        $key = $target.Triple.ToUpper().Replace('-', '_')
        Set-Item "env:CARGO_TARGET_${key}_LINKER" (Join-Path $bin $target.Clang)
    }
}

function Resolve-Abi([string[]]$Abi) {
    $names = $Abi | ForEach-Object { $_ -split ',' } | Where-Object { $_ }
    if (-not $names -or $names -contains 'all') { $names = @($Targets.Keys) }
    foreach ($name in $names) {
        if (-not $Targets.Contains($name)) { throw "unknown ABI '$name' (expected: $($Targets.Keys -join ', '))" }
    }
    $names
}
