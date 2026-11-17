<#
.SYNOPSIS
  Builds, signs and packages StudioCraft for Windows.

.DESCRIPTION
  Writes to $env:DIST (default: dist/release):
    studiocraft-<version>-windows-<arch>.msi            the per-machine installer (WiX v5)
    studiocraft-<version>-windows-<arch>-portable.zip   studiocraft.exe and studiocraft-cli.exe

  The executables link the C runtime statically, so neither package needs the Visual C++
  redistributable. sign.ps1 signs the executables and the MSI, and leaves them unsigned with a
  warning when no signing secrets are set.

  Needs Rust with the MSVC toolchain and the target, the Windows SDK (signtool.exe), and WiX v5
  (dotnet tool install --global wix --version 5.0.2).

.EXAMPLE
  pwsh packaging/windows/package.ps1 -Arch x64
  pwsh packaging/windows/package.ps1 -Arch arm64 -SkipBuild
#>
param(
  [ValidateSet('x64', 'x86', 'arm64')] [string] $Arch = 'x64',
  [switch] $SkipBuild
)
$ErrorActionPreference = 'Stop'
$Root = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path

function Invoke-Native([string] $What, [scriptblock] $Block) {
  Write-Output "==> $What"
  & $Block
  if ($LASTEXITCODE -ne 0) { throw "$What failed with exit code $LASTEXITCODE" }
}

# [workspace.package] version in the root Cargo.toml; STUDIOCRAFT_VERSION overrides it.
$Version = $env:STUDIOCRAFT_VERSION
if (-not $Version) {
  $inPkg = $false
  foreach ($line in Get-Content (Join-Path $Root 'Cargo.toml')) {
    if ($line -match '^\s*\[') { $inPkg = ($line.Trim() -eq '[workspace.package]'); continue }
    if ($inPkg -and $line -match '^\s*version\s*=\s*"([^"]+)"') { $Version = $Matches[1]; break }
  }
}
if (-not $Version) { throw 'could not read [workspace.package] version from Cargo.toml' }
# An MSI version is numeric (major.minor.build), so a pre-release part is dropped there.
$MsiVersion = ($Version -split '-')[0]

$Target = switch ($Arch) { 'x64' { 'x86_64-pc-windows-msvc' } 'x86' { 'i686-pc-windows-msvc' } 'arm64' { 'aarch64-pc-windows-msvc' } }
$Dist = if ($env:DIST) { $env:DIST } else { Join-Path $Root 'dist\release' }
$TargetDir = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $Root 'target' }
New-Item -ItemType Directory -Force -Path $Dist | Out-Null

Write-Output "StudioCraft $Version for Windows $Arch ($Target)"

if (-not $SkipBuild) {
  # The static C runtime is set for the target only, so build scripts and proc macros keep the default.
  $flagVar = 'CARGO_TARGET_' + ($Target.ToUpper() -replace '-', '_') + '_RUSTFLAGS'
  [Environment]::SetEnvironmentVariable($flagVar, '-C target-feature=+crt-static')
  Invoke-Native "cargo build ($Target)" { cargo build --release --locked -p studiocraft -p studiocraft-cli --target $Target }
}

$Bin = Join-Path $TargetDir "$Target\release"

# Each executable's PE header must name the machine -Arch asks for. The app must use the Windows
# GUI subsystem (2), so no console window opens with it, and the CLI the console subsystem (3).
function Get-PeHeader([string] $Path) {
  $bytes = [System.IO.File]::ReadAllBytes($Path)
  $pe = [BitConverter]::ToInt32($bytes, 0x3C)
  return @{ Machine = [BitConverter]::ToUInt16($bytes, $pe + 4); Subsystem = [BitConverter]::ToUInt16($bytes, $pe + 0x5C) }
}
$Machine = switch ($Arch) { 'x64' { 0x8664 } 'x86' { 0x14C } 'arm64' { 0xAA64 } }
foreach ($check in @(@('studiocraft.exe', 2), @('studiocraft-cli.exe', 3))) {
  $h = Get-PeHeader (Join-Path $Bin $check[0])
  if ($h.Machine -ne $Machine) { throw "$($check[0]) is for machine 0x$('{0:X}' -f $h.Machine), not 0x$('{0:X}' -f $Machine) ($Arch)" }
  if ($h.Subsystem -ne $check[1]) { throw "$($check[0]) has PE subsystem $($h.Subsystem), not $($check[1])" }
  Write-Output "ok $($check[0]): $Arch, PE subsystem $($h.Subsystem)"
}

$Work = Join-Path $TargetDir "windows-package\$Arch"
$Stage = Join-Path $Work 'stage'
Remove-Item -Recurse -Force $Work -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force -Path $Stage | Out-Null
Copy-Item (Join-Path $Bin 'studiocraft.exe'), (Join-Path $Bin 'studiocraft-cli.exe') $Stage
Copy-Item (Join-Path $Root 'LICENSE-MIT'), (Join-Path $Root 'LICENSE-APACHE') $Stage

& (Join-Path $PSScriptRoot 'sign.ps1') (Join-Path $Stage 'studiocraft.exe') (Join-Path $Stage 'studiocraft-cli.exe')

# The installer's icon: an .ico that holds the PNG icons from assets/app-icon/hicolor as they are.
$Icon = Join-Path $Work 'studiocraft.ico'
$sizes = 16, 24, 32, 48, 64, 128, 256
$images = foreach ($s in $sizes) { , [IO.File]::ReadAllBytes((Join-Path $Root "assets\app-icon\hicolor\${s}x${s}\apps\ai.storyteller.studiocraft.png")) }
$ico = New-Object IO.MemoryStream
$w = New-Object IO.BinaryWriter($ico)
$w.Write([UInt16]0); $w.Write([UInt16]1); $w.Write([UInt16]$sizes.Count)
$offset = 6 + 16 * $sizes.Count
for ($i = 0; $i -lt $sizes.Count; $i++) {
  $dim = if ($sizes[$i] -ge 256) { 0 } else { $sizes[$i] }
  $w.Write([byte]$dim); $w.Write([byte]$dim); $w.Write([byte]0); $w.Write([byte]0)
  $w.Write([UInt16]1); $w.Write([UInt16]32)
  $w.Write([UInt32]$images[$i].Length); $w.Write([UInt32]$offset)
  $offset += $images[$i].Length
}
foreach ($img in $images) { $w.Write($img) }
$w.Flush()
[IO.File]::WriteAllBytes($Icon, $ico.ToArray())

# ---- MSI --------------------------------------------------------------------------------------
$Msi = Join-Path $Dist "studiocraft-$Version-windows-$Arch.msi"
Invoke-Native 'WiX UI extension' { wix extension add -g WixToolset.UI.wixext/5.0.2 }
Invoke-Native 'wix build' {
  wix build (Join-Path $PSScriptRoot 'studiocraft.wxs') -arch $Arch -ext WixToolset.UI.wixext `
    -d "Version=$MsiVersion" -d "BinDir=$Stage" -d "IconPath=$Icon" -o $Msi
}
# wix writes its debug symbols (.wixpdb) beside the MSI; they are not release files.
Remove-Item -Force -ErrorAction SilentlyContinue ([IO.Path]::ChangeExtension($Msi, '.wixpdb'))
& (Join-Path $PSScriptRoot 'sign.ps1') $Msi

# ---- portable zip -----------------------------------------------------------------------------
# The updater expects the executables at the top level of the zip.
$Portable = Join-Path $Work 'portable'
New-Item -ItemType Directory -Force -Path $Portable | Out-Null
Copy-Item (Join-Path $Stage '*.exe') $Portable
foreach ($f in 'README.md', 'LICENSE-MIT', 'LICENSE-APACHE') {
  $p = Join-Path $Root $f
  if (Test-Path $p) { Copy-Item $p $Portable }
}
$Zip = Join-Path $Dist "studiocraft-$Version-windows-$Arch-portable.zip"
Remove-Item -Force $Zip -ErrorAction SilentlyContinue
Compress-Archive -Path (Join-Path $Portable '*') -DestinationPath $Zip

# The CLI runs here only when the build is for this machine's architecture.
$HostArch = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString().ToLowerInvariant()
if ($Arch -ne 'arm64' -or $HostArch -eq 'arm64') {
  Invoke-Native 'studiocraft-cli --version' { & (Join-Path $Stage 'studiocraft-cli.exe') --version }
} else {
  Write-Output "skipping studiocraft-cli --version: an $Arch build does not run on this $HostArch machine"
}
Get-Item $Msi, $Zip | Format-Table Name, Length
