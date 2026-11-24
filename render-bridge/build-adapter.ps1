param(
    [string]$GodotHeaders,
    [string]$OutputDirectory
)
$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
$build = Join-Path $repo 'build/godot-renderer'
if (-not $OutputDirectory) { $OutputDirectory = Join-Path $repo 'build/Brotato-Mod' }
$configure = @('-S',$PSScriptRoot,'-B',$build,'-A','x64')
if ($GodotHeaders) {
    $headers = (Resolve-Path -LiteralPath $GodotHeaders).Path
    $configure += "-DGODOT_HEADERS:PATH=$headers"
}
& cmake @configure
if ($LASTEXITCODE) { throw 'GDNative 插件配置失败' }
& cmake --build $build --config Release
if ($LASTEXITCODE) { throw 'GDNative 插件编译失败' }

$mods = New-Item -ItemType Directory -Path (Join-Path $OutputDirectory 'mods') -Force
$zip = Join-Path $mods.FullName 'LanStream-Offscreen.zip'
$temp = $zip + '.' + [Guid]::NewGuid().ToString('N') + '.tmp'
Add-Type -AssemblyName System.IO.Compression.FileSystem
$archive = [IO.Compression.ZipFile]::Open($temp, [IO.Compression.ZipArchiveMode]::Create)
try {
    foreach ($file in @('mod_main.gd','manifest.json')) {
        [IO.Compression.ZipFileExtensions]::CreateEntryFromFile($archive,
            (Join-Path $PSScriptRoot "mod/$file"),"mods-unpacked/LanStream-Offscreen/$file") | Out-Null
    }
} finally { $archive.Dispose() }
[IO.File]::Move($temp, $zip, $true)
Copy-Item -LiteralPath (Join-Path $build 'Release/LanStreamRenderer.dll') -Destination $OutputDirectory
Get-Item -LiteralPath $zip,(Join-Path $OutputDirectory 'LanStreamRenderer.dll') | Select-Object Name,Length
