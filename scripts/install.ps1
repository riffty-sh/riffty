# PowerShell installer for riffty (https://github.com/riffty-sh/riffty)
# Usage: powershell -ExecutionPolicy ByPass -c "irm https://github.com/riffty-sh/riffty/releases/latest/download/riffty-installer.ps1 | iex"
$ErrorActionPreference = "Stop"

$Repo = if ($env:RIFFTY_GITHUB_REPO) { $env:RIFFTY_GITHUB_REPO } else { "riffty-sh/riffty" }
$Version = if ($env:RIFFTY_VERSION) { $env:RIFFTY_VERSION } else { "latest" }
$InstallDir = if ($env:RIFFTY_INSTALL_DIR) { $env:RIFFTY_INSTALL_DIR } else { Join-Path $HOME ".riffty\bin" }

$Archive = "riffty-x86_64-pc-windows-msvc.zip"
if ($Version -eq "latest") {
    $Url = "https://github.com/$Repo/releases/latest/download/$Archive"
} else {
    $Url = "https://github.com/$Repo/releases/download/$Version/$Archive"
}

$TmpDir = Join-Path ([System.IO.Path]::GetTempPath()) ([System.Guid]::NewGuid().ToString())
New-Item -ItemType Directory -Force -Path $TmpDir | Out-Null

try {
    $ZipPath = Join-Path $TmpDir $Archive
    Write-Host "Downloading riffty from $Url..."
    Invoke-WebRequest -Uri $Url -OutFile $ZipPath
    Expand-Archive -Path $ZipPath -DestinationPath $TmpDir -Force

    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
    Copy-Item -Path (Join-Path $TmpDir "riffty.exe") -Destination (Join-Path $InstallDir "riffty.exe") -Force
    if (Test-Path (Join-Path $TmpDir "riffty_mcp.exe")) {
        Copy-Item -Path (Join-Path $TmpDir "riffty_mcp.exe") -Destination (Join-Path $InstallDir "riffty_mcp.exe") -Force
    }
    if (Test-Path (Join-Path $TmpDir "riffty-ab.exe")) {
        Copy-Item -Path (Join-Path $TmpDir "riffty-ab.exe") -Destination (Join-Path $InstallDir "riffty-ab.exe") -Force
    }
    if (Test-Path (Join-Path $TmpDir "riffty-java")) {
        $JavaDest = Join-Path $InstallDir "riffty-java"
        if (Test-Path $JavaDest) { Remove-Item -Recurse -Force $JavaDest }
        Copy-Item -Recurse -Force -Path (Join-Path $TmpDir "riffty-java") -Destination $JavaDest
    }
    Write-Host "Installed self-contained riffty bundle (Python, JS/TS, Java Tier 1+2 with bundled JDK 25 runtime) to $InstallDir"
} finally {
    Remove-Item -Recurse -Force $TmpDir -ErrorAction SilentlyContinue
}

