. "$PSScriptRoot\env.ps1"

$glslc = Join-Path (Find-Ndk) 'shader-tools\windows-x86_64\glslc.exe'
$shaders = Join-Path $Root 'src\graphics\vulkan\shaders'
foreach ($source in Get-ChildItem $shaders -Include *.vert, *.frag -Recurse) {
    & $glslc -O $source.FullName -o "$($source.FullName).spv"
    if ($LASTEXITCODE -ne 0) { throw "glslc failed for $($source.Name)" }
    Write-Host ("{0,-10} {1:N0} bytes" -f $source.Name, (Get-Item "$($source.FullName).spv").Length)
}
