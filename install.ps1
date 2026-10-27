# winapfs one-command installer. Paste this into PowerShell:
#
#   irm https://raw.githubusercontent.com/ZYRT3CH/winapfs/main/install.ps1 | iex
#
# It downloads the latest winapfs.exe from the official GitHub release, checks
# it against the published SHA-256 checksum, and runs `winapfs install` (which
# installs WinFsp if needed and asks Windows for permission once).
#
# Set WINAPFS_DOWNLOAD_ONLY=1 to download and check the file without installing.

& {
    $ErrorActionPreference = 'Stop'
    $ProgressPreference = 'SilentlyContinue' # the progress bar makes downloads far slower in Windows PowerShell

    if (-not [Environment]::Is64BitOperatingSystem) {
        Write-Host 'winapfs needs 64-bit Windows 10 or 11.' -ForegroundColor Red
        return
    }
    # Older Windows PowerShell may default to TLS versions GitHub refuses.
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

    $base = 'https://github.com/ZYRT3CH/winapfs/releases/latest/download'
    $dir = Join-Path $env:TEMP 'winapfs-setup'
    $exe = Join-Path $dir 'winapfs.exe'
    $sum = Join-Path $dir 'winapfs.exe.sha256'
    New-Item -ItemType Directory -Force -Path $dir | Out-Null

    Write-Host ''
    Write-Host '  winapfs  -  your Mac drive, on Windows' -ForegroundColor Magenta
    Write-Host ''
    Write-Host 'Downloading the latest winapfs.exe from GitHub...'
    try {
        Invoke-WebRequest -Uri "$base/winapfs.exe" -OutFile $exe -UseBasicParsing
        Invoke-WebRequest -Uri "$base/winapfs.exe.sha256" -OutFile $sum -UseBasicParsing
    } catch {
        Write-Host "Could not download winapfs: $($_.Exception.Message)" -ForegroundColor Red
        Write-Host 'Check your internet connection, or download it by hand from https://github.com/ZYRT3CH/winapfs/releases/latest'
        return
    }

    $want = ((Get-Content -Raw $sum).Trim() -split '\s+')[0].ToLower()
    $got = (Get-FileHash -Algorithm SHA256 $exe).Hash.ToLower()
    if ($got -ne $want) {
        Remove-Item -Force $exe, $sum
        Write-Host 'The download does not match its published checksum, so it was deleted and nothing was installed.' -ForegroundColor Red
        Write-Host 'Please try again, and open an issue at https://github.com/ZYRT3CH/winapfs/issues if it keeps happening.'
        return
    }
    Write-Host "Checksum OK ($($got.Substring(0, 16))...)." -ForegroundColor Green

    if ($env:WINAPFS_DOWNLOAD_ONLY) {
        Write-Host "Downloaded and checked: $exe"
        return
    }

    Write-Host ''
    & $exe install
    Remove-Item -Recurse -Force $dir -ErrorAction SilentlyContinue
}
