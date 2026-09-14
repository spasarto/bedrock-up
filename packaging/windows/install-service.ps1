<#
.SYNOPSIS
    Installs bedrock-supervisor as a Windows service using NSSM.

.DESCRIPTION
    A convenience wrapper around `nssm install` / `nssm set`. NSSM (not
    included) must already be on PATH: https://nssm.cc/download

    bedrock-supervisor is a normal console app, not a native Windows
    service — it has no SCM control handler. NSSM bridges that gap by
    running it as a child process and translating service stop requests
    into a Ctrl+C event, which the `ctrlc` handler inside bedrock-supervisor
    treats the same as SIGTERM on Linux: warn players, `stop`, wait for the
    world to save, then exit.

    See docs/deployment.md for the full flag reference and the tradeoffs
    against a native `windows-service` SCM handler (not implemented).

.EXAMPLE
    ./install-service.ps1 -ServerPath C:\minecraft -DownloadType windows
#>
param(
    [string]$ServiceName = "BedrockSupervisor",
    [Parameter(Mandatory = $true)]
    [string]$ServerPath,
    [ValidateSet("windows", "linux", "preview-windows", "preview-linux", "server-jar")]
    [string]$DownloadType = "windows",
    [string]$SupervisorExe = "C:\Program Files\bedrock-up\bedrock-supervisor.exe",
    # Must exceed the countdown (--warn-at) plus --stop-timeout, in
    # milliseconds, or NSSM escalates to a hard kill before the world saves.
    [int]$StopTimeoutMs = 170000
)

if (-not (Get-Command nssm -ErrorAction SilentlyContinue)) {
    throw "nssm was not found on PATH. Download it from https://nssm.cc/download"
}

nssm install $ServiceName $SupervisorExe
nssm set $ServiceName AppParameters "--download-type $DownloadType --server-path `"$ServerPath`""
nssm set $ServiceName AppDirectory (Split-Path $SupervisorExe)
nssm set $ServiceName AppStdout "$ServerPath\supervisor-stdout.log"
nssm set $ServiceName AppStderr "$ServerPath\supervisor-stderr.log"

# Ask nicely first (Ctrl+C), and give the graceful stop sequence room to
# finish before NSSM moves on to WM_CLOSE / TerminateProcess.
nssm set $ServiceName AppStopMethodConsole $StopTimeoutMs
nssm set $ServiceName AppStopMethodWindow $StopTimeoutMs

Write-Host "Service '$ServiceName' installed. Start it with: nssm start $ServiceName"
