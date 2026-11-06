# Round-robin benchmark of disk-usage tools on the same target (Windows).
#   .\bench-scan.ps1 -Target 'C:\Users\me\AppData' -Runs 3
# Each tool gets one warmup run, then -Runs interleaved runs. Reports wall min/median,
# total CPU min, and peak working set per tool. Methodology: docs/benchmarks.md.
param(
    [string] $Target = 'C:\Users\me',
    [int] $Runs = 3,
    [string] $Out = 'bench-results.txt',
    [string] $Cleanmcp = 'cleanmcp.exe'
)
$ErrorActionPreference = 'Stop'
$tools = @(
    @{ Name = 'cleanmcp';     Exe = $Cleanmcp; Args = "tree `"$Target`" -d 1 -q" },
    @{ Name = 'dua';         Exe = 'dua.exe';  Args = "`"$Target`"" },
    @{ Name = 'dust';        Exe = 'dust.exe'; Args = "-d 1 -n 30 -c `"$Target`"" },
    @{ Name = 'gdu';         Exe = 'gdu.exe';  Args = "-n --no-progress `"$Target`"" }
)

function Run-Once([hashtable] $t) {
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $t.Exe
    $psi.Arguments = $t.Args
    $psi.UseShellExecute = $false
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $sw = [Diagnostics.Stopwatch]::StartNew()
    $p = [Diagnostics.Process]::Start($psi)
    $out = $p.StandardOutput.ReadToEndAsync()
    $err = $p.StandardError.ReadToEndAsync()
    $peak = 0
    while (-not $p.WaitForExit(25)) {
        try { $p.Refresh(); if ($p.PeakWorkingSet64 -gt $peak) { $peak = $p.PeakWorkingSet64 } } catch {}
    }
    $p.WaitForExit()
    $sw.Stop()
    if ($p.ExitCode -ne 0) { throw "$($t.Name) exit $($p.ExitCode): $($err.Result.Substring(0, [Math]::Min(300, $err.Result.Length)))" }
    [pscustomobject]@{ Wall = $sw.Elapsed.TotalSeconds; Cpu = $p.TotalProcessorTime.TotalSeconds; PeakMB = [math]::Round($peak / 1MB) }
}

foreach ($t in $tools) { Run-Once $t | Out-Null }   # warmup: hot OS file cache for everyone

$results = @{}
foreach ($t in $tools) { $results[$t.Name] = @() }
for ($i = 0; $i -lt $Runs; $i++) {
    foreach ($t in $tools) { $results[$t.Name] += ,(Run-Once $t) }
}

$lines = @("target: $Target   runs: $Runs (after 1 warmup each)   $(Get-Date -Format o)")
foreach ($t in $tools) {
    $r = $results[$t.Name]
    $walls = ($r | ForEach-Object { $_.Wall } | Sort-Object)
    $cpus  = ($r | ForEach-Object { $_.Cpu } | Sort-Object)
    $med = $walls[[int][math]::Floor(($walls.Count - 1) / 2)]
    $peak = ($r | ForEach-Object { $_.PeakMB } | Measure-Object -Maximum).Maximum
    $lines += ("{0,-10} wall min {1,7:N2}s  median {2,7:N2}s  cpu min {3,7:N2}s  peak {4,6} MB   [{5}]" -f `
        $t.Name, $walls[0], $med, $cpus[0], $peak, (($r | ForEach-Object { '{0:N2}' -f $_.Wall }) -join ' '))
}
$lines | Tee-Object -FilePath $Out
