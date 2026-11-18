param(
    [string[]]$Abi = @('all'),
    [switch]$Debug,
    [switch]$Quiet
)

. "$PSScriptRoot\env.ps1"
Use-Toolchain
Set-Location $Root

$profile = if ($Debug) { 'debug' } else { 'release' }

foreach ($name in Resolve-Abi $Abi) {
    $triple = $Targets[$name].Triple
    $arguments = @('build', '--target', $triple)
    if (-not $Debug) { $arguments += '--release' }
    if ($Quiet) { $arguments += '--quiet' }
    & cargo @arguments
    if ($LASTEXITCODE -ne 0) { throw "build failed for $name" }

    $out = Join-Path $Root "out\$name"
    New-Item -ItemType Directory -Force $out | Out-Null
    # Every binary the crate builds goes to out.
    Get-ChildItem (Join-Path $Root "build\$triple\$profile") -File | Where-Object { -not $_.Extension } | Copy-Item -Destination $out -Force
    $size = (Get-Item (Join-Path $out 'android-egui')).Length
    Write-Host ("{0,-12} out\{0}\android-egui  {1:N0} bytes" -f $name, $size)
}
