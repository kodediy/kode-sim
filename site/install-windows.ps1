# kode-sim for Windows 10/11: through WSL 2 (Ubuntu), since the ESP32-P4
# emulator runs on Linux and macOS. Installs WSL and Ubuntu when they are
# missing, runs the Linux installer inside Ubuntu, then opens the Kode Dot in
# Chrome on Windows.
#
#   powershell -ExecutionPolicy Bypass -c "irm https://sim.kode.diy/install-windows.ps1 | iex"
#
# The first time, Windows may ask to restart after installing WSL: run the
# same command again afterwards.

$ErrorActionPreference = "Stop"
$LinuxScript = "https://sim.kode.diy/install-linux.sh"
$Distro = "Ubuntu"

function Say($t) { Write-Host "`n== $t" -ForegroundColor Yellow }

if (-not [Environment]::Is64BitOperatingSystem) { throw "kode-sim needs 64-bit Windows." }

Say "WSL 2"
$wslOk = $false
try { wsl.exe --status *> $null; $wslOk = ($LASTEXITCODE -eq 0) } catch { $wslOk = $false }
if (-not $wslOk) {
    Write-Host "Installing WSL: Windows asks for administrator rights."
    Start-Process wsl.exe -ArgumentList "--install","--no-distribution" -Verb RunAs -Wait
    Write-Host "Restart Windows, then run this command again." -ForegroundColor Cyan
    exit 0
}

Say "Ubuntu"
$distros = (wsl.exe -l -q) -replace "`0", "" | Where-Object { $_ -ne "" }
if (-not ($distros -contains $Distro)) {
    Write-Host "Installing Ubuntu. When it asks, choose a user name and a password for Linux."
    wsl.exe --install -d $Distro
    Write-Host "Ubuntu is set up. Run this command again to continue." -ForegroundColor Cyan
    exit 0
}

Say "Google Chrome"
$chrome = @("$env:ProgramFiles\Google\Chrome\Application\chrome.exe",
            "${env:ProgramFiles(x86)}\Google\Chrome\Application\chrome.exe",
            "$env:LocalAppData\Google\Chrome\Application\chrome.exe") | Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $chrome) {
    try { winget install -e --id Google.Chrome --accept-source-agreements --accept-package-agreements }
    catch { Write-Host "Chrome could not be installed: the Kode Dot opens in your default browser." }
}

Say "kode-sim inside Ubuntu (asks for your Linux password once)"
# The Linux installer does the rest, starts the simulator and opens this
# Windows' Chrome itself; WSL forwards 127.0.0.1 to Windows.
wsl.exe -d $Distro -- bash -lc "curl -fsSL $LinuxScript -o /tmp/kode-sim-install.sh && bash /tmp/kode-sim-install.sh"
