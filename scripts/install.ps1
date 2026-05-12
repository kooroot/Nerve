$ErrorActionPreference = "Stop"

$Repo = if ($env:NERVE_REPO) { $env:NERVE_REPO } else { "kooroot/Nerve" }
$BinName = "nv.exe"
$InstallDir = if ($env:NERVE_INSTALL_DIR) {
    $env:NERVE_INSTALL_DIR
} else {
    Join-Path $env:LOCALAPPDATA "Programs\Nerve"
}

if (-not [Environment]::Is64BitOperatingSystem) {
    throw "Nerve currently ships a Windows x64 binary only."
}

$Target = "x86_64-pc-windows-msvc"
$Asset = "nerve-$Target.zip"
$Url = "https://github.com/$Repo/releases/latest/download/$Asset"
$TempDir = Join-Path ([IO.Path]::GetTempPath()) ("nerve-install-" + [Guid]::NewGuid().ToString("N"))

New-Item -ItemType Directory -Force -Path $TempDir, $InstallDir | Out-Null

try {
    $ArchivePath = Join-Path $TempDir $Asset
    Write-Host "Downloading $Asset..."
    Invoke-WebRequest -Uri $Url -OutFile $ArchivePath

    Expand-Archive -Path $ArchivePath -DestinationPath $TempDir -Force
    $FoundBin = Get-ChildItem -Path $TempDir -Recurse -Filter $BinName | Select-Object -First 1
    if (-not $FoundBin) {
        throw "$BinName was not found in $Asset"
    }

    Copy-Item -Path $FoundBin.FullName -Destination (Join-Path $InstallDir $BinName) -Force

    $UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
    $PathParts = @()
    if ($UserPath) {
        $PathParts = $UserPath -split ';' | Where-Object { $_ }
    }

    if ($PathParts -notcontains $InstallDir) {
        $NewPath = if ($UserPath) { "$InstallDir;$UserPath" } else { $InstallDir }
        [Environment]::SetEnvironmentVariable("Path", $NewPath, "User")
        $env:Path = "$InstallDir;$env:Path"
        Write-Host "Added $InstallDir to the user PATH. Open a new terminal if 'nv' is not found."
    }

    Write-Host "Installed nv to $(Join-Path $InstallDir $BinName)"
    & (Join-Path $InstallDir $BinName) --help | Out-Null
    Write-Host "Run: nv doctor"
}
finally {
    Remove-Item -Path $TempDir -Recurse -Force -ErrorAction SilentlyContinue
}
