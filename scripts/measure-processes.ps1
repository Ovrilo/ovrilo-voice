<#
.SYNOPSIS
  Samples CPU and memory of running processes (default: voice-spike) for ADR-0009 baselines.

.DESCRIPTION
  CPU % is normalized to total logical processors (100% = whole machine).
  Private bytes = committed private memory; working set = resident memory.
  Record the machine, build profile and scenario next to the output; numbers are only
  comparable on the same hardware.

.EXAMPLE
  ./scripts/measure-processes.ps1 -Name voice-spike -Seconds 60
#>
param(
    [string]$Name = "voice-spike",
    [int]$Seconds = 60,
    [int]$IntervalSeconds = 1
)

$ErrorActionPreference = "Stop"
$cores = [Environment]::ProcessorCount
$samples = @{}
$previous = @{}

for ($elapsed = 0; $elapsed -lt $Seconds; $elapsed += $IntervalSeconds) {
    $now = Get-Date
    foreach ($process in Get-Process -Name $Name -ErrorAction SilentlyContinue) {
        $id = $process.Id
        if ($previous.ContainsKey($id)) {
            $cpuSeconds = ($process.TotalProcessorTime - $previous[$id].Cpu).TotalSeconds
            $wallSeconds = ($now - $previous[$id].Time).TotalSeconds
            if (-not $samples.ContainsKey($id)) { $samples[$id] = New-Object System.Collections.ArrayList }
            [void]$samples[$id].Add([pscustomobject]@{
                CpuPercent = 100 * $cpuSeconds / $wallSeconds / $cores
                PrivateMB  = $process.PrivateMemorySize64 / 1MB
                WorkingMB  = $process.WorkingSet64 / 1MB
            })
        }
        $previous[$id] = @{ Cpu = $process.TotalProcessorTime; Time = $now }
    }
    Start-Sleep -Seconds $IntervalSeconds
}

if ($samples.Count -eq 0) {
    Write-Error "No running process named '$Name' was sampled."
}

"Machine: $((Get-CimInstance Win32_Processor | Select-Object -First 1).Name.Trim()), $cores logical processors"
foreach ($id in $samples.Keys) {
    $s = $samples[$id]
    $cpu = $s | Measure-Object CpuPercent -Average -Maximum
    $private = $s | Measure-Object PrivateMB -Average -Maximum
    $working = $s | Measure-Object WorkingMB -Average -Maximum
    "{0} pid {1}: {2} samples | CPU avg {3:N2}% max {4:N2}% | private avg {5:N1} MB max {6:N1} MB | working set avg {7:N1} MB max {8:N1} MB" -f `
        $Name, $id, $s.Count, $cpu.Average, $cpu.Maximum, $private.Average, $private.Maximum, $working.Average, $working.Maximum
}
