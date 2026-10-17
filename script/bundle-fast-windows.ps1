[CmdletBinding()]
Param(
    [Parameter()][Alias('a')][string]$Architecture,
    [Parameter()][Alias('j')][string]$Jobs,
    [Parameter()][Alias('i')][switch]$Install,
    [Parameter()][Alias('h')][switch]$Help
)

# Fast local Windows installer (Zed-<arch>.exe).
#
# One build of the editor binaries (zed + cli + auto_update_helper) plus the
# tiny explorer_command_injector, then Inno Setup -> installer. The full
# script/bundle-windows.ps1 additionally builds remote_server, which is a
# second, large build (its feature set diverges from zed's, so gpui and its
# dependents are recompiled). Skipping that is where most of the time is saved.
#
# Also skipped vs the full script: debug-symbol zipping and the Sentry upload
# (both CI-only concerns) and code signing (no secrets locally; the installer
# is unsigned, which is fine for a local build).
#
# Run from the repository root. Release channel comes from
# crates/zed/RELEASE_CHANNEL (dev -> "Zed Dev", stable -> "Zed").
# Limit build parallelism with -Jobs, e.g. -Jobs 4.

. "$PSScriptRoot/lib/workspace.ps1"

# https://stackoverflow.com/questions/57949031/powershell-script-stops-if-program-fails-like-bash-set-o-errexit
$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $true

function Get-VSArch {
    param([string]$Arch)
    switch ($Arch) {
        "x86_64" { "amd64" }
        "aarch64" { "arm64" }
    }
}

$OSArchitecture = switch ([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture) {
    "X64" { "x86_64" }
    "Arm64" { "aarch64" }
    default { throw "Unsupported architecture" }
}

$Architecture = if ($Architecture) { $Architecture } else { $OSArchitecture }
$target = "$Architecture-pc-windows-msvc"
$CargoOutDir = "./target/$target/release"

if ($Help) {
    Write-Output "Usage: bundle-fast-windows.ps1 [options]"
    Write-Output "Build a local Windows installer for Zed.\n"
    Write-Output "Options:"
    Write-Output "  -Architecture, -a  Which architecture to build (x86_64 or aarch64)"
    Write-Output "  -Jobs, -j           Cargo build parallelism (e.g. -Jobs 4)"
    Write-Output "  -Install, -i        Run the installer after building"
    Write-Output "  -Help, -h           Show this help message"
    exit 0
}

# Enter the MSVC dev environment so cargo can find link.exe.
$VSInstallDir = "C:\Program Files\Microsoft Visual Studio\2022\Community"
$vsDevShell = "$VSInstallDir\Common7\Tools\Launch-VsDevShell.ps1"
if (-not (Test-Path $vsDevShell)) {
    throw "Could not find the Visual Studio dev shell at '$vsDevShell'. Install Visual Studio 2022 Community with the C++ workload."
}
Push-Location
& $vsDevShell -Arch (Get-VSArch -Arch $Architecture) -HostArch (Get-VSArch -Arch $OSArchitecture)
Pop-Location

# `msvc_spectre_libs` (reached via languages -> pet) links Microsoft's
# Spectre-mitigated CRT from the lib\spectre\<arch> folder of whichever MSVC
# toolset it finds. Visual Studio's default toolset can be older than the one
# that has the spectre libs installed, which makes the build fail with
# "No spectre-mitigated libs were found".
#
# The toolset is selected for this script's child processes only (like a VS
# "Platform Toolset" for a project); the VS installation is not modified.
# find-msvc-tools looks up cl.exe on PATH first (find_msvc_environment runs
# before the VCToolsVersion path), so the lever is PATH: put the spectre-capable
# toolset's bin directory in front. VCToolsVersion is set as well for any code
# path that consults it.
$spectreArch = if ($Architecture -eq "aarch64") { "arm64" } else { "x64" }
$msvcToolsRoot = Join-Path $VSInstallDir "VC\Tools\MSVC"
$spectreToolsets = @(
    Get-ChildItem -Path $msvcToolsRoot -Directory -ErrorAction SilentlyContinue |
    Where-Object { Test-Path (Join-Path $_.FullName "lib\spectre\$spectreArch\libcmt.lib") } |
    Sort-Object -Property @{ Expression = { [version]$_.Name }; Descending = $true }
)
if ($spectreToolsets.Count -eq 0) {
    throw "No MSVC toolset under '$msvcToolsRoot' has Spectre-mitigated libs for '$spectreArch'. Install the 'Spectre-mitigated libs' component for the C++ workload in Visual Studio Installer."
}
$spectreToolset = $spectreToolsets[0].Name
$hostBinDir = if ($OSArchitecture -eq "aarch64") { "Hostarm64" } else { "Hostx64" }
$toolsetBin = Join-Path $msvcToolsRoot "$spectreToolset\bin\$hostBinDir\$spectreArch"
if (-not (Test-Path (Join-Path $toolsetBin "cl.exe"))) {
    throw "Expected cl.exe under '$toolsetBin' but it is missing."
}
if ($env:VCToolsVersion -ne $spectreToolset) {
    Write-Output "Selecting MSVC toolset $spectreToolset (VS default was '$($env:VCToolsVersion)', which has no spectre-mitigated libs for $spectreArch)"
}
$env:VCToolsVersion = $spectreToolset
$env:PATH = "$toolsetBin;$env:PATH"

function Find-Iscc {
    # Prefer an ISCC already on PATH, then the standard Inno Setup 6 install
    # locations (both the machine-wide and per-user directories), then the
    # registry as a fallback.
    $onPath = Get-Command "ISCC.exe" -ErrorAction SilentlyContinue
    if ($onPath) { return $onPath.Source }

    $candidates = @(
        "C:\Program Files (x86)\Inno Setup 6\ISCC.exe",
        "C:\Program Files\Inno Setup 6\ISCC.exe",
        (Join-Path $env:LOCALAPPDATA "Programs\Inno Setup 6\ISCC.exe")
    )
    foreach ($candidate in $candidates) {
        if (Test-Path $candidate) { return $candidate }
    }

    $regPaths = @(
        "HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\Inno Setup 6_is1",
        "HKCU:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\Inno Setup 6_is1"
    )
    foreach ($regPath in $regPaths) {
        $entry = Get-ItemProperty -Path $regPath -ErrorAction SilentlyContinue
        if ($entry -and $entry.InstallLocation) {
            $iscc = Join-Path $entry.InstallLocation "ISCC.exe"
            if (Test-Path $iscc) { return $iscc }
        }
    }

    throw "Inno Setup 6 (ISCC.exe) not found. Install it with: winget install JRSoftware.InnoSetup"
}

function Find-MakeAppx {
    # Newest Windows SDK's makeappx.exe; prefer the x64 host tool.
    $sdkBin = "C:\Program Files (x86)\Windows Kits\10\bin"
    if (-not (Test-Path $sdkBin)) {
        throw "Windows Kits not found at '$sdkBin'. Install the Windows SDK."
    }
    $tools = Get-ChildItem -Path $sdkBin -Filter "makeappx.exe" -Recurse -ErrorAction SilentlyContinue |
        Sort-Object FullName -Descending
    if (-not $tools) {
        throw "makeappx.exe not found under '$sdkBin'. Install the Windows SDK."
    }
    return ($tools | Where-Object { $_.DirectoryName -like "*\x64" } | Select-Object -First 1).FullName
}

$iscc = Find-Iscc
$makeAppx = Find-MakeAppx

ParseZedWorkspace
$innoDir = "$env:ZED_WORKSPACE\inno\$Architecture"
$releaseVersion = $env:RELEASE_VERSION

Push-Location -Path crates/zed
$channel = Get-Content "RELEASE_CHANNEL"
$env:ZED_RELEASE_CHANNEL = $channel
$env:RELEASE_CHANNEL = $channel
Pop-Location

Write-Output "Building the installer for channel '$channel' ($target) with Inno Setup at $iscc"
if ($channel -ne "dev") {
    # Mirrors script/bundle-fast-mac: a non-dev build shares its data directory
    # and CLI socket with the installed Zed app (e.g. db/0-<channel>) and can
    # migrate those databases. Keep crates/zed/RELEASE_CHANNEL at 'dev' locally.
    Write-Warning "WARNING: channel is '$channel', not 'dev'. A non-dev bundle shares its data directory and CLI socket with the installed Zed app (db/0-$channel, zed-$channel.sock) and may migrate those databases."
}

# --- Build binaries (one heavy build; remote_server is intentionally skipped) ---

rustup target add $target

$buildArgs = @('--config', '.cargo/bundle-config.toml', 'build', '--release', '--target', $target)
$buildArgs += @('--package', 'zed', '--package', 'cli', '--package', 'auto_update_helper')
if ($Jobs) { $buildArgs += @('--jobs', $Jobs) }
& cargo @buildArgs

$injectorFeatures = switch ($channel) {
    "stable" { @('--features', 'stable', '--no-default-features') }
    "preview" { @('--features', 'preview', '--no-default-features') }
    default { @() }
}
$injectorArgs = @('--config', '.cargo/bundle-config.toml', 'build', '--release', '--target', $target, '--package', 'explorer_command_injector') + $injectorFeatures
if ($Jobs) { $injectorArgs += @('--jobs', $Jobs) }
& cargo @injectorArgs

# --- Stage the installer resources ---

if (Test-Path "$innoDir") {
    Remove-Item -Path "$innoDir" -Recurse -Force
}
New-Item -Path "$innoDir" -ItemType Directory -Force | Out-Null
Copy-Item -Path "$env:ZED_WORKSPACE\crates\zed\resources\windows\*" -Destination "$innoDir" -Recurse -Force
New-Item -Path "$innoDir\make_appx" -ItemType Directory -Force | Out-Null
New-Item -Path "$innoDir\appx" -ItemType Directory -Force | Out-Null
New-Item -Path "$innoDir\bin" -ItemType Directory -Force | Out-Null
New-Item -Path "$innoDir\tools" -ItemType Directory -Force | Out-Null

Copy-Item -Path ".\$CargoOutDir\zed.exe" -Destination "$innoDir\Zed.exe" -Force
Copy-Item -Path ".\$CargoOutDir\cli.exe" -Destination "$innoDir\cli.exe" -Force
Copy-Item -Path ".\$CargoOutDir\auto_update_helper.exe" -Destination "$innoDir\auto_update_helper.exe" -Force
Copy-Item -Path ".\$CargoOutDir\explorer_command_injector.dll" -Destination "$innoDir\zed_explorer_command_injector.dll" -Force

# --- Pack the Explorer shell-extension .appx ---

$manifestFile = switch ($channel) {
    "stable" { "$env:ZED_WORKSPACE\crates\explorer_command_injector\AppxManifest.xml" }
    "preview" { "$env:ZED_WORKSPACE\crates\explorer_command_injector\AppxManifest-Preview.xml" }
    default { "$env:ZED_WORKSPACE\crates\explorer_command_injector\AppxManifest-Nightly.xml" }
}
Copy-Item -Path "$manifestFile" -Destination "$innoDir\make_appx\AppxManifest.xml"
& $makeAppx pack /d "$innoDir\make_appx" /p "$innoDir\zed_explorer_command_injector.appx" /nv

# --- Download the ConPTY and AMD GPU dependencies (x64 only needs the GPU lib) ---
# The versions are read from the tracked script/bundle-windows.ps1 (updated by
# upstream merges) rather than hardcoded here, so an upstream version bump is
# picked up automatically instead of silently drifting. The version is part of
# each cache filename, so a bump also forces a fresh download. The extracted
# payloads are kept as a cache: files are COPIED into the staging dir (not
# moved), so a re-run skips both the download and the extract.
$upstreamBundleScript = Join-Path $env:ZED_WORKSPACE "script\bundle-windows.ps1"
if (-not (Test-Path $upstreamBundleScript)) {
    throw "Cannot find '$upstreamBundleScript' to read the ConPTY/AGS versions from."
}
$upstreamText = Get-Content -Path $upstreamBundleScript -Raw

$conptyMatch = [regex]::Match($upstreamText, 'https://github\.com/microsoft/terminal/releases/download/[^"'']+\.nupkg')
if (-not $conptyMatch.Success) {
    throw "Could not find the ConPTY URL in '$upstreamBundleScript'. Its structure may have changed upstream; update bundle-fast-windows.ps1 to match."
}
$conptyUrl = $conptyMatch.Value
$conptyNupkg = ".\" + (($conptyUrl -split '/') | Select-Object -Last 1)
$conptyRuntimeDir = if ($Architecture -eq "aarch64") { "win-arm64" } else { "win-x64" }
$conptyDll = ".\conpty\runtimes\$conptyRuntimeDir\native\conpty.dll"
if (-not (Test-Path $conptyNupkg)) {
    Write-Output "Downloading ConPTY ($(($conptyUrl -split '/') | Select-Object -Last 1))..."
    Invoke-WebRequest -Uri $conptyUrl -OutFile $conptyNupkg
}
if (-not (Test-Path $conptyDll)) {
    Write-Output "Extracting ConPTY..."
    Remove-Item -Path ".\conpty" -Recurse -Force -ErrorAction SilentlyContinue
    # Windows PowerShell 5.1's Expand-Archive only accepts a .zip extension, so
    # extract the .nupkg (which is a zip archive) through a .zip copy.
    $conptyZip = ".\Microsoft.Windows.Console.ConPTY.zip"
    Copy-Item -Path $conptyNupkg -Destination $conptyZip -Force
    Expand-Archive -Path $conptyZip -DestinationPath ".\conpty" -Force
    Remove-Item -Path $conptyZip -Force
}

if ($Architecture -eq "aarch64") {
    New-Item -Type Directory -Path "$innoDir\arm64" -Force | Out-Null
    Copy-Item -Path ".\conpty\build\native\runtimes\arm64\OpenConsole.exe" -Destination "$innoDir\arm64\OpenConsole.exe" -Force
    Copy-Item -Path ".\conpty\runtimes\win-arm64\native\conpty.dll" -Destination "$innoDir\conpty.dll" -Force
} else {
    $agsMatch = [regex]::Match($upstreamText, 'https://codeload\.github\.com/GPUOpen-LibrariesAndSDKs/AGS_SDK/zip/refs/tags/[^"'']+')
    if (-not $agsMatch.Success) {
        throw "Could not find the AGS SDK URL in '$upstreamBundleScript'. Its structure may have changed upstream; update bundle-fast-windows.ps1 to match."
    }
    $agsUrl = $agsMatch.Value
    $agsTag = ($agsUrl -split '/') | Select-Object -Last 1              # e.g. v6.3.0
    $agsZip = ".\AGS_SDK_$agsTag.zip"                                   # upstream's ".\AGS_SDK_v6.3.0.zip"
    $agsDir = ".\AGS_SDK-$($agsTag.TrimStart('v'))"                     # GitHub extracts tag v6.3.0 to AGS_SDK-6.3.0
    $agsDll = "$agsDir\ags_lib\lib\amd_ags_x64.dll"
    if (-not (Test-Path $agsZip)) {
        Write-Output "Downloading AMD GPU Services ($agsTag)..."
        Invoke-WebRequest -Uri $agsUrl -OutFile $agsZip
    }
    if (-not (Test-Path $agsDll)) {
        Write-Output "Extracting AMD GPU Services..."
        Expand-Archive -Path $agsZip -DestinationPath "." -Force
    }

    New-Item -Type Directory -Path "$innoDir\x64" -Force | Out-Null
    New-Item -Type Directory -Path "$innoDir\arm64" -Force | Out-Null
    Copy-Item -Path $agsDll -Destination "$innoDir\amd_ags_x64.dll" -Force
    Copy-Item -Path ".\conpty\build\native\runtimes\x64\OpenConsole.exe" -Destination "$innoDir\x64\OpenConsole.exe" -Force
    Copy-Item -Path ".\conpty\build\native\runtimes\arm64\OpenConsole.exe" -Destination "$innoDir\arm64\OpenConsole.exe" -Force
    Copy-Item -Path ".\conpty\runtimes\win-x64\native\conpty.dll" -Destination "$innoDir\conpty.dll" -Force
}

# --- Lay the staged files out the way zed.iss expects ---

Move-Item -Path "$innoDir\zed_explorer_command_injector.appx" -Destination "$innoDir\appx\zed_explorer_command_injector.appx" -Force
Move-Item -Path "$innoDir\zed_explorer_command_injector.dll" -Destination "$innoDir\appx\zed_explorer_command_injector.dll" -Force
Move-Item -Path "$innoDir\cli.exe" -Destination "$innoDir\bin\zed.exe" -Force
Move-Item -Path "$innoDir\zed.sh" -Destination "$innoDir\bin\zed" -Force
Move-Item -Path "$innoDir\auto_update_helper.exe" -Destination "$innoDir\tools\auto_update_helper.exe" -Force

# --- Compile the installer with Inno Setup ---

$appId = switch ($channel) {
    "stable" { "{{2DB0DA96-CA55-49BB-AF4F-64AF36A86712}" }
    "preview" { "{{F70E4811-D0E2-4D88-AC99-D63752799F95}" }
    "nightly" { "{{1BDB21D3-14E7-433C-843C-9C97382B2FE0}" }
    "dev" { "{{8357632E-24A4-4F32-BA97-E575B4D1FE5D}" }
    default { Write-Error "can't bundle installer for $channel."; exit 1 }
}
$appIconName = switch ($channel) {
    "stable" { "app-icon" }
    "preview" { "app-icon-preview" }
    "nightly" { "app-icon-nightly" }
    "dev" { "app-icon-dev" }
}
$appName = switch ($channel) {
    "stable" { "Zed" }
    "preview" { "Zed Preview" }
    "nightly" { "Zed Nightly" }
    "dev" { "Zed Dev" }
}
$appDisplayName = $appName
$appSetupName = "Zed-$Architecture"
# The mutex name here should match the mutex name in crates\zed\src\zed\windows_only_instance.rs
$appMutex = switch ($channel) {
    "stable" { "Zed-Stable-Instance-Mutex" }
    "preview" { "Zed-Preview-Instance-Mutex" }
    "nightly" { "Zed-Nightly-Instance-Mutex" }
    "dev" { "Zed-Dev-Instance-Mutex" }
}
$appExeName = "Zed"
$regValueName = switch ($channel) {
    "stable" { "Zed" }
    "preview" { "ZedPreview" }
    "nightly" { "ZedNightly" }
    "dev" { "ZedDev" }
}
$appUserId = switch ($channel) {
    "stable" { "ZedIndustries.Zed" }
    "preview" { "ZedIndustries.Zed.Preview" }
    "nightly" { "ZedIndustries.Zed.Nightly" }
    "dev" { "ZedIndustries.Zed.Dev" }
}
$appShellNameShort = switch ($channel) {
    "stable" { "Z&ed" }
    "preview" { "Z&ed Preview" }
    "nightly" { "Z&ed Editor Nightly" }
    "dev" { "Z&ed Dev" }
}
$appAppxFullName = switch ($channel) {
    "stable" { "ZedIndustries.Zed_1.0.0.0_neutral__japxn1gcva8rg" }
    "preview" { "ZedIndustries.Zed.Preview_1.0.0.0_neutral__japxn1gcva8rg" }
    "nightly" { "ZedIndustries.Zed.Nightly_1.0.0.0_neutral__japxn1gcva8rg" }
    "dev" { "ZedIndustries.Zed.Dev_1.0.0.0_neutral__japxn1gcva8rg" }
}

$definitions = @{
    "AppId"          = $appId
    "AppIconName"    = $appIconName
    "OutputDir"      = "$env:ZED_WORKSPACE\target"
    "AppSetupName"   = $appSetupName
    "AppName"        = $appName
    "AppDisplayName" = $appDisplayName
    "RegValueName"   = $regValueName
    "AppMutex"       = $appMutex
    "AppExeName"     = $appExeName
    "ResourcesDir"   = "$innoDir"
    "ShellNameShort" = $appShellNameShort
    "AppUserId"      = $appUserId
    "Version"        = "$releaseVersion"
    "SourceDir"      = "$env:ZED_WORKSPACE"
    "AppxFullName"   = $appAppxFullName
}

$defs = @()
foreach ($key in $definitions.Keys) {
    $defs += "/d$key=`"$($definitions[$key])`""
}

$issFilePath = "$innoDir\zed.iss"
$innoArgs = @($issFilePath) + $defs
Write-Output "Running Inno Setup..."
$process = Start-Process -FilePath $iscc -ArgumentList $innoArgs -NoNewWindow -Wait -PassThru
if ($process.ExitCode -ne 0) {
    Write-Error "Inno Setup failed with exit code $($process.ExitCode)"
    exit 1
}

$setupPath = "$env:ZED_WORKSPACE\target\$appSetupName.exe"
Write-Output "Installer built: $setupPath"

if ($Install) {
    Write-Output "Installing $appDisplayName..."
    Start-Process -FilePath $setupPath
}
