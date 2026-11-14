# Prerequisites and expected events: scripts/README.md.
[CmdletBinding()]
param()

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$repoRoot = Split-Path -Parent $PSScriptRoot

function Find-Binary {
    param([string]$Name, [string]$Override)
    if ($Override) {
        if (-not (Test-Path -LiteralPath $Override -PathType Leaf)) {
            throw "$Name override not found: $Override"
        }
        return $Override
    }
    foreach ($configuration in 'release', 'debug') {
        foreach ($suffix in '', '.exe') {
            $candidate = "$repoRoot/target/$configuration/$Name$suffix"
            if (Test-Path -LiteralPath $candidate -PathType Leaf) { return $candidate }
        }
    }
    throw "$Name not found under $repoRoot\target. Build both binaries first:`n  cargo build --release -p handfast-daemon --bin handfastd --bin hfctl`nSet HANDFASTD or HFCTL to use another path."
}

function Read-Log {
    param([string]$Path)
    $share = [System.IO.FileShare]::ReadWrite -bor [System.IO.FileShare]::Delete
    try {
        $stream = [System.IO.File]::Open($Path, [System.IO.FileMode]::Open, [System.IO.FileAccess]::Read, $share)
    } catch {
        return ''
    }
    $reader = [System.IO.StreamReader]::new($stream)
    try { return $reader.ReadToEnd() } finally { $reader.Dispose() }
}

$process = $null

try {
    $daemon = Find-Binary -Name 'handfastd' -Override $env:HANDFASTD

    $runDir = Join-Path ([System.IO.Path]::GetTempPath()) ("handfast-interop-{0}" -f [guid]::NewGuid().ToString('N').Substring(0, 8))
    $dataDir = Join-Path $runDir 'data'
    $incomingDir = Join-Path $runDir 'incoming'
    $log = Join-Path $runDir 'handfastd.log'
    $stdoutLog = Join-Path $runDir 'handfastd.stdout.log'
    New-Item -ItemType Directory -Path $dataDir, $incomingDir -Force | Out-Null

    $identityOutput = & $daemon --print-identity --data-dir $dataDir
    if ($LASTEXITCODE -ne 0) {
        throw "handfastd --print-identity failed with exit code $LASTEXITCODE. State dir: $dataDir"
    }
    $identityJson = $identityOutput -join "`n"
    $identity = $identityJson | ConvertFrom-Json
    $deviceId = $identity.body.deviceId
    if (-not $deviceId) { throw 'could not read body.deviceId from --print-identity output' }

    $daemonArgs = @('--data-dir', $dataDir, '--incoming-dir', $incomingDir)
    $quotedArgs = $daemonArgs | ForEach-Object { if ($_ -match '\s') { '"' + $_ + '"' } else { $_ } }
    $env:RUST_LOG = 'debug'
    $process = Start-Process -FilePath $daemon -ArgumentList $quotedArgs -RedirectStandardOutput $stdoutLog -RedirectStandardError $log -NoNewWindow -PassThru

    $started = $false
    for ($i = 0; $i -lt 300; $i++) {
        if ((Read-Log $log) -match 'discovery listening') { $started = $true; break }
        if ($process.HasExited) { break }
        Start-Sleep -Milliseconds 100
    }
    if (-not $started) {
        [Console]::Error.WriteLine("handfastd did not start. Log: $log")
        $tail = (Read-Log $log).TrimEnd() -split "`r?`n" | Select-Object -Last 30
        if ($tail) { [Console]::Error.WriteLine(($tail -join "`n")) }
        throw 'handfastd did not start.'
    }

    $logText = Read-Log $log
    $tcpPort = if ($logText -match 'TCP listener bound tcp_port=(\d+)') { $Matches[1] } else { 'unknown' }
    $fingerprint = if ($logText -match 'TLS identity loaded fingerprint=([0-9a-f]+)') { $Matches[1] } else { 'unknown' }

    if ([System.Environment]::OSVersion.Platform -eq [System.PlatformID]::Win32NT) {
        $sid = [System.Security.Principal.WindowsIdentity]::GetCurrent().User.Value
        $controlPath = "\\.\pipe\handfastd-$sid"
    } elseif ([string]::IsNullOrEmpty($env:XDG_RUNTIME_DIR)) {
        $controlPath = Join-Path $dataDir 'handfastd.sock'
    } else {
        $controlPath = Join-Path (Join-Path $env:XDG_RUNTIME_DIR 'handfast') 'handfastd.sock'
    }

    Write-Output ''
    Write-Output 'Handfast share interop run'
    Write-Output "  device id:      $deviceId"
    Write-Output "  certificate:    $fingerprint"
    Write-Output "  bound TCP port: $tcpPort"
    Write-Output "  control path:   $controlPath"
    Write-Output "  data dir:       $dataDir"
    Write-Output "  log:            $log"
    Write-Output "  incoming dir:   $incomingDir"
    $incomingNote = "Received files appear in $incomingDir with no .part file left."

    Write-Output ''
    Write-Output 'Local identity packet. body.tcpPort is the configured default, not the bound port:'
    Write-Output ''
    Write-Output $identityJson
    Write-Output ''
    Write-Output 'Pair both peers before this run. The link checklists are in docs/INTEROP.md.'
    Write-Output 'Watch the log in a second terminal:'
    Write-Output ''
    Write-Output "  Get-Content -LiteralPath '$log' -Wait"
    Write-Output ''
    Write-Output 'Inbound from Android (KDE Connect app, Share to Handfast):'
    Write-Output '  1. Share a photo, then text, then an https URL.'
    Write-Output '  2. Share an empty file and a file larger than 100 MiB.'
    Write-Output '  3. Cancel one transfer, then share two files at once.'
    Write-Output '  Expect share.request with the name, size and payload port, then'
    Write-Output '  ShareProgress and ShareCompleted. Text and URL lines show a length only.'
    Write-Output "  $incomingNote"
    Write-Output ''
    Write-Output 'Inbound from the desktop KDE Connect client:'
    Write-Output '  1. Share a file, then text, then an https URL.'
    Write-Output '  2. Queue a second file behind a large first file.'
    Write-Output '  Expect the same log lines, both files in order, and no partial file.'
    Write-Output '  The payload TLS version is TLS 1.2 for Android and TLS 1.3 for the desktop.'
    Write-Output ''
    $hfctl = $null
    try {
        $hfctl = Find-Binary -Name 'hfctl' -Override $env:HFCTL
    } catch {
        if ($env:HFCTL) { throw }
    }
    if ($hfctl) {
        Write-Output 'Outbound to a paired peer:'
        Write-Output ''
        Write-Output "  & '$hfctl' share --device <peer-id> --data-dir '$dataDir' <file>"
        Write-Output ''
        Write-Output '  Expect "sent <n> bytes to <peer-id>" and exit 0. A remote or IO error'
        Write-Output '  exits 1, a usage error exits 2.'
        Write-Output ''
    } else {
        Write-Output 'Outbound step skipped because hfctl is missing. Build it or set HFCTL. The inbound steps are unaffected.'
        Write-Output ''
    }

    Read-Host 'Press Enter to stop handfastd' | Out-Null
}
catch {
    [Console]::Error.WriteLine("interop-share: $($_.Exception.Message)")
    exit 1
}
finally {
    if ($process -and -not $process.HasExited) {
        # The daemon stops on SIGINT. Windows has no way to send it to one process.
        if ([System.Environment]::OSVersion.Platform -eq [System.PlatformID]::Unix) {
            try { & /bin/kill -INT $process.Id } catch { }
            $process.WaitForExit(10000) | Out-Null
        }
        if (-not $process.HasExited) {
            try { $process.Kill() } catch { }
            $process.WaitForExit(5000) | Out-Null
        }
    }
}

Write-Output ''
Write-Output 'handfastd stopped.'
Write-Output "  log:            $log"
Write-Output "  data dir:       $dataDir"
Write-Output "  incoming dir:   $incomingDir"
Write-Output ''
Write-Output 'Received files:'
Get-ChildItem -LiteralPath $incomingDir -Force |
    Format-Table Length, LastWriteTime, Name -AutoSize |
    Out-String |
    Write-Output
Write-Output ''
Write-Output 'Packet capture for a failed step:'
Write-Output "  sudo tcpdump -i any -s 0 -w $runDir/share.pcap 'tcp portrange 1716-1764'"
Write-Output 'Do not attach state.db, it holds the private key.'
Write-Output "Remove the run dir when the evidence is filed: Remove-Item -Recurse -Force '$runDir'"
