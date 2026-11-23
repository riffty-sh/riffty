# Verified PowerShell installer for riffty (https://github.com/riffty-sh/riffty)
# Usage: powershell -ExecutionPolicy ByPass -c "irm https://github.com/riffty-sh/riffty/releases/latest/download/riffty-installer.ps1 | iex"
$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"

$Repo = if ($env:RIFFTY_GITHUB_REPO) { $env:RIFFTY_GITHUB_REPO } else { "riffty-sh/riffty" }
$Version = if ($env:RIFFTY_VERSION) { $env:RIFFTY_VERSION } else { "latest" }
$InstallDir = if ($env:RIFFTY_INSTALL_DIR) { $env:RIFFTY_INSTALL_DIR } else { Join-Path $HOME ".riffty\bin" }

$Archive = "riffty-x86_64-pc-windows-msvc.zip"
$ShaFile = "$Archive.sha256"
if ($env:RIFFTY_BASE_URL) {
    $BaseUrl = $env:RIFFTY_BASE_URL.TrimEnd("/")
} elseif ($Version -eq "latest") {
    $BaseUrl = "https://github.com/$Repo/releases/latest/download"
} else {
    $BaseUrl = "https://github.com/$Repo/releases/download/$Version"
}

$TmpDir = Join-Path ([System.IO.Path]::GetTempPath()) ([System.Guid]::NewGuid().ToString())
New-Item -ItemType Directory -Force -Path $TmpDir | Out-Null

try {
    $ZipPath = Join-Path $TmpDir $Archive
    $ShaPath = Join-Path $TmpDir $ShaFile
    Write-Host "Downloading riffty from $BaseUrl/$Archive..."
    Invoke-WebRequest -Uri "$BaseUrl/$Archive" -OutFile $ZipPath
    Invoke-WebRequest -Uri "$BaseUrl/$ShaFile" -OutFile $ShaPath

    $ExpectedSha = ((Get-Content $ShaPath -Raw).Trim() -split "\s+")[0].ToLower()
    $ActualSha = (Get-FileHash $ZipPath -Algorithm SHA256).Hash.ToLower()
    if ($ExpectedSha -ne $ActualSha) {
        throw "SHA-256 verification failed for $Archive! Expected $ExpectedSha, got $ActualSha"
    }
    Write-Host "Verified SHA-256 ($ActualSha)."

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
    if ($env:RIFFTY_NO_MODIFY_PATH -ne "1") {
        $CurrentPaths = ($env:PATH -split ";") | Where-Object { $_ -ne "" }
        if ($CurrentPaths -notcontains $InstallDir) {
            $env:PATH = "$InstallDir;$env:PATH"
        }
        $UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
        $UserPaths = if ($UserPath) { ($UserPath -split ";") | Where-Object { $_ -ne "" } } else { @() }
        if ($UserPaths -notcontains $InstallDir) {
            $NewUserPath = if ($UserPath) { "$InstallDir;$UserPath" } else { $InstallDir }
            [Environment]::SetEnvironmentVariable("Path", $NewUserPath, "User")
            Write-Host "Added $InstallDir to your User PATH."
        }
    }
    Write-Host "Installed verified riffty to $InstallDir (Python & JS/TS built-in; Java Tier 2 auto-hydrates on first .java check or via 'riffty plugin install java')."
} finally {
    Remove-Item -Recurse -Force $TmpDir -ErrorAction SilentlyContinue
}
