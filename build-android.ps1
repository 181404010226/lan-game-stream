param(
    [Parameter(Mandatory)][string]$Exe,
    [Parameter(Mandatory)][ValidateLength(1,40)][string]$GameName,
    [Parameter(Mandatory)][ValidatePattern('^[a-z][a-z0-9_]{0,31}$')][string]$AppSlug,
    [string]$HostConfig,
    [string]$HostHint = '',
    [switch]$Xbox,
    [ValidatePattern('^(?:[1-9]\d{0,2}:[1-9]\d{0,2})?$')][string]$NativeAspect,
    [ValidateRange(1,65535)][int]$Port = 24865,
    [ValidateSet('arm64-v8a','x86_64')][string[]]$Abi = @('arm64-v8a','x86_64'),
    [string]$Sdk,
    [string]$Jdk,
    [string]$Output
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

function Invoke-Tool([string]$Program, [string[]]$Arguments) {
    & $Program @Arguments
    if ($LASTEXITCODE -ne 0) { throw "$(Split-Path -Leaf $Program) 失败，退出码 $LASTEXITCODE" }
}

if (-not $Sdk) {
    $Sdk = @($env:ANDROID_SDK_ROOT, $env:ANDROID_HOME, 'D:\Android\Sdk', (Join-Path $env:LOCALAPPDATA 'Android\Sdk')) |
        Where-Object { $_ -and (Test-Path -LiteralPath (Join-Path $_ 'platforms\android-35\android.jar')) } |
        Select-Object -First 1
}
if (-not $Jdk) {
    $Jdk = @($env:JAVA_HOME, 'D:\Java\MicrosoftJDK17') |
        Where-Object { $_ -and (Test-Path -LiteralPath (Join-Path $_ 'bin\javac.exe')) } |
        Select-Object -First 1
}
if (-not $Sdk -or -not $Jdk) { throw '找不到 Android SDK 35 或 JDK 17。请传入 -Sdk 和 -Jdk。' }

$buildTools = Get-ChildItem -LiteralPath (Join-Path $Sdk 'build-tools') -Directory |
    Where-Object { $_.Name -match '^\d+\.\d+\.\d+$' -and [version]$_.Name -ge [version]'35.0.0' } |
    Sort-Object { [version]$_.Name } -Descending | Select-Object -First 1
$ndk = Get-ChildItem -LiteralPath (Join-Path $Sdk 'ndk') -Directory |
    Where-Object { $_.Name -match '^\d+\.\d+\.\d+$' } |
    Sort-Object { [version]$_.Name } -Descending | Select-Object -First 1
if (-not $ndk -or -not $buildTools) { throw 'Android SDK 需要 NDK 和 Build Tools 35 或更新版本。' }

$tools = $buildTools.FullName
$clang = Join-Path $ndk.FullName 'toolchains\llvm\prebuilt\windows-x86_64\bin\clang.exe'
$androidJar = Join-Path $Sdk 'platforms\android-35\android.jar'
$java = Join-Path $Jdk 'bin\java.exe'
$javac = Join-Path $Jdk 'bin\javac.exe'
$jar = Join-Path $Jdk 'bin\jar.exe'
$keytool = Join-Path $Jdk 'bin\keytool.exe'
$root = $PSScriptRoot
$work = Join-Path $root 'build'
$gameExe = Get-Item -LiteralPath $Exe
if ($gameExe.PSIsContainer -or $gameExe.Extension -ne '.exe') { throw '-Exe 需要指向游戏的 EXE 文件。' }
if (-not $Output) { $Output = Join-Path $work "$AppSlug.apk" }
if (-not $HostConfig) { $HostConfig = Join-Path $work 'lan-stream-games.json' }
$Output = [IO.Path]::GetFullPath($Output)
$HostConfig = [IO.Path]::GetFullPath($HostConfig)
$bindings = @()
if (Test-Path -LiteralPath $HostConfig) {
    $bindings = @(Get-Content -LiteralPath $HostConfig -Raw -Encoding utf8 | ConvertFrom-Json -AsHashtable)
}
$bound = $bindings | Where-Object { $_.id -eq $AppSlug } | Select-Object -First 1
if ($bound -and $bound.exe -ne $gameExe.FullName) { throw '此 APK 标识已经绑定另一个 EXE，请换一个 -AppSlug。' }
if ($bindings | Where-Object { $_.exe -eq $gameExe.FullName -and $_.id -ne $AppSlug }) { throw '该 EXE 已经绑定其他 APK 标识，请沿用原有 -AppSlug。' }
if ($bound) {
    $secret = $bound.key
} else {
    $random = [byte[]]::new(32)
    [Security.Cryptography.RandomNumberGenerator]::Fill($random)
    $secret = [Convert]::ToHexString($random).ToLowerInvariant()
}
if ($secret -notmatch '^[a-fA-F0-9]{64}$') { throw '已有游戏的配对密钥格式有误。' }
$xboxEnabled = if ($PSBoundParameters.ContainsKey('Xbox')) { [bool]$Xbox } elseif ($bound) { [bool]$bound['xbox'] } else { $false }
$xboxLiteral = $xboxEnabled.ToString().ToLowerInvariant()
$aspect = if ($PSBoundParameters.ContainsKey('NativeAspect')) { $NativeAspect } elseif ($bound) { $bound['native_aspect'] } else { $null }
if ($aspect) {
    $size = $aspect.Split(':')
    $ratio = [double]$size[0] / [double]$size[1]
    if ($ratio -lt 1.3 -or $ratio -gt 3.0) { throw '游戏画幅需要在 1.3:1 到 3:1 之间。' }
}
$updatedBindings = @($bindings | Where-Object { $_.id -ne $AppSlug })
$updated = [ordered]@{ id = $AppSlug; title = $GameName; exe = $gameExe.FullName; key = $secret; xbox = $xboxEnabled }
if ($aspect) { $updated['native_aspect'] = $aspect }
$updatedBindings += $updated
$applicationId = "dev.lanstream.mobile.$AppSlug"
$stage = Join-Path 'build' "apk-$([Guid]::NewGuid().ToString('N'))"
$generated = Join-Path $stage 'generated'
$classes = Join-Path $stage 'classes'
$dex = Join-Path $stage 'dex'
New-Item -ItemType Directory -Force -Path (Join-Path $root $generated),(Join-Path $root $classes),(Join-Path $root $dex),(Split-Path -Parent $Output),(Split-Path -Parent $HostConfig) | Out-Null

Push-Location -LiteralPath $root
try {
    $libraries = [ordered]@{}
    foreach ($architecture in $Abi) {
        $target = if ($architecture -eq 'arm64-v8a') { 'aarch64-linux-android' } else { 'x86_64-linux-android' }
        $prefix = 'CARGO_TARGET_' + $target.ToUpperInvariant().Replace('-','_')
        [Environment]::SetEnvironmentVariable("${prefix}_LINKER", $clang, 'Process')
        [Environment]::SetEnvironmentVariable("${prefix}_RUSTFLAGS", "-Clink-arg=--target=${target}26 -Clink-arg=-Wl,-z,max-page-size=16384", 'Process')
        Invoke-Tool 'cargo' @('build', '--locked', '-p', 'lan-stream-android', '--release', '--target', $target)
        $libraries[$architecture] = Join-Path $root "target\$target\release\liblan_stream.so"
    }

    $resources = Join-Path $stage 'resources.zip'
    $unaligned = Join-Path $stage 'unaligned.apk'
    $aligned = Join-Path $stage 'aligned.apk'
    $signed = Join-Path $stage 'signed.apk'
    $classJar = Join-Path $stage 'classes.jar'
    $manifest = Join-Path $stage 'AndroidManifest.xml'
    $manifestText = (Get-Content -LiteralPath 'android\AndroidManifest.xml' -Raw -Encoding utf8).
        Replace('package="dev.lanstream.mobile"', "package=`"$applicationId`"").
        Replace('android:label="局域网串玩"', ('android:label="' + [Security.SecurityElement]::Escape($GameName) + '串玩"'))
    [IO.File]::WriteAllText((Join-Path $root $manifest), $manifestText)
    $javaPackage = Join-Path $generated 'dev\lanstream\mobile'
    New-Item -ItemType Directory -Force -Path $javaPackage | Out-Null
    $titleLiteral = ConvertTo-Json -InputObject $GameName -Compress
    $addressLiteral = ConvertTo-Json -InputObject $HostHint -Compress
    $bindingJava = @"
package dev.lanstream.mobile;

final class GameBinding {
    static final String ID = "$AppSlug";
    static final String TITLE = $titleLiteral;
    static final String HOST_HINT = $addressLiteral;
    static final String KEY = "$secret";
    static final int PORT = $Port;
    static final boolean XBOX = $xboxLiteral;
}
"@
    [IO.File]::WriteAllText((Join-Path $root (Join-Path $javaPackage 'GameBinding.java')), $bindingJava)
    Invoke-Tool (Join-Path $tools 'aapt2.exe') @('compile','--dir','android\res','-o',$resources)
    Invoke-Tool (Join-Path $tools 'aapt2.exe') @('link','-I',$androidJar,'--manifest',$manifest,'--custom-package','dev.lanstream.mobile','--java',$generated,'-o',$unaligned,$resources)
    $sources = @(Get-ChildItem -LiteralPath 'android\java',$generated -Recurse -Filter '*.java' | ForEach-Object { [IO.Path]::GetRelativePath($root, $_.FullName) })
    Invoke-Tool $javac (@('-encoding','UTF-8','--release','8','-classpath',$androidJar,'-d',$classes) + $sources)
    Invoke-Tool $jar @('cf',$classJar,'-C',$classes,'.')
    Invoke-Tool $java @('-cp',(Join-Path $tools 'lib\d8.jar'),'com.android.tools.r8.D8','--release','--min-api','26','--lib',$androidJar,'--output',$dex,$classJar)

    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $archive = [IO.Compression.ZipFile]::Open((Join-Path $root $unaligned), [IO.Compression.ZipArchiveMode]::Update)
    try {
        [IO.Compression.ZipFileExtensions]::CreateEntryFromFile($archive, (Join-Path $root (Join-Path $dex 'classes.dex')), 'classes.dex', [IO.Compression.CompressionLevel]::Optimal) | Out-Null
        foreach ($architecture in $libraries.Keys) {
            [IO.Compression.ZipFileExtensions]::CreateEntryFromFile($archive, $libraries[$architecture], "lib/$architecture/liblan_stream.so", [IO.Compression.CompressionLevel]::NoCompression) | Out-Null
        }
    } finally { $archive.Dispose() }

    $keystore = Join-Path $work 'lan-stream-signing.p12'
    if (-not (Test-Path -LiteralPath $keystore)) {
        Invoke-Tool $keytool @('-genkeypair','-alias','lan-stream','-keyalg','RSA','-keysize','2048','-validity','10000','-storetype','PKCS12',
            '-keystore',$keystore,'-storepass','android','-keypass','android','-dname','CN=LAN Game Stream, O=Local Build')
    }
    $signer = Join-Path $tools 'lib\apksigner.jar'
    Invoke-Tool (Join-Path $tools 'zipalign.exe') @('-P','16','-f','4',$unaligned,$aligned)
    Invoke-Tool $java @('-jar',$signer,'sign','--ks',$keystore,'--ks-pass','pass:android','--ks-key-alias','lan-stream','--out',$signed,$aligned)
    Invoke-Tool $java @('-jar',$signer,'verify','--verbose',$signed)
    Invoke-Tool (Join-Path $tools 'zipalign.exe') @('-c','-P','16','4',$signed)
    $configTemp = $HostConfig + '.' + [Guid]::NewGuid().ToString('N') + '.tmp'
    [IO.File]::WriteAllText($configTemp, (ConvertTo-Json -InputObject $updatedBindings -Depth 4))
    [IO.File]::Move($configTemp, $HostConfig, $true)
    Copy-Item -LiteralPath $signed -Destination $Output
    Get-Item -LiteralPath $Output | Select-Object FullName,Length
} finally { Pop-Location }
