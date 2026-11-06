<#
.SYNOPSIS
    Windows process hygiene shared by startdev.ps1 and startprod.ps1.

.DESCRIPTION
    The product orchestration (build, launch, HMR, hot restart, health checks) lives
    in the cross-platform Node entries scripts\dev-web.mjs and scripts\prod-web.mjs.
    This file only owns what cannot be done portably: finding the ZCode processes that
    hold our ports and stopping them. Every stop is scoped to this repository - a
    process is ours only when its name is a zcode-* binary, its command line references
    this checkout, or its image lives under native\target. Foreign listeners are
    reported and left running instead of being killed.
#>

Set-StrictMode -Version 3.0

$script:ZCodeOwnedProcessNames = @('zcode-server', 'zcode-host', 'zcode-shell')

function Write-ZCodeStep {
    param([string]$Message)
    [Console]::Out.WriteLine("[zcode] $Message")
}

function Stop-ZCodeFail {
    param([string]$Message)
    throw $Message
}

function Get-ZCodeRequiredCommand {
    param([string]$Name, [string]$Why)
    $command = Get-Command $Name -ErrorAction SilentlyContinue
    if (-not $command) { Stop-ZCodeFail "$Name is not on PATH but it is required to $Why" }
    if ($command.Source -match '\.ps1$') {
        # Start-Process and & cannot exec a .ps1 shim; prefer the matching .cmd/.exe.
        $alternative = Get-Command "$Name.cmd" -ErrorAction SilentlyContinue
        if ($alternative -and $alternative.Source) { return $alternative.Source }
    }
    return $command.Source
}

function Get-ZCodePortOwnerPids {
    param([int[]]$Ports)
    $owners = @()
    foreach ($port in $Ports) {
        $connections = @(Get-NetTCPConnection -State Listen -LocalPort $port -ErrorAction SilentlyContinue)
        foreach ($connection in $connections) { $owners += [int]$connection.OwningProcess }
    }
    return @($owners | Sort-Object -Unique)
}

function Resolve-ZCodeProcessOwnership {
    param([int[]]$Pids, [string]$RepoRoot)
    $root = $RepoRoot.TrimEnd('\', '/')
    $patterns = @([regex]::Escape($root), [regex]::Escape($root.Replace('\', '/')))
    $rows = @()
    foreach ($processId in $Pids) {
        if ($processId -le 4) { continue }
        $info = Get-CimInstance Win32_Process -Filter "ProcessId = $processId" -ErrorAction SilentlyContinue
        if (-not $info) { continue }
        $name = [System.IO.Path]::GetFileNameWithoutExtension([string]$info.Name)
        $commandLine = [string]$info.CommandLine
        $ownedByCommandLine = $false
        foreach ($pattern in $patterns) {
            if ($commandLine -and $commandLine -match $pattern) { $ownedByCommandLine = $true; break }
        }
        $rows += [pscustomobject]@{
            ProcessId   = $processId
            Name        = [string]$info.Name
            CommandLine = $commandLine
            Mine        = (($script:ZCodeOwnedProcessNames -contains $name) -or $ownedByCommandLine)
        }
    }
    return $rows
}

function Get-ZCodeStalePids {
    param([string]$RepoRoot, [int[]]$Ports)
    $candidates = @(Get-ZCodePortOwnerPids -Ports $Ports)
    $targetRoot = Join-Path $RepoRoot 'native\target'
    foreach ($process in @(Get-Process -ErrorAction SilentlyContinue)) {
        $path = $null
        try { $path = $process.Path } catch { $path = $null }
        if ($path -and $path.StartsWith($targetRoot, [System.StringComparison]::OrdinalIgnoreCase)) {
            $candidates += [int]$process.Id
        }
    }
    return Resolve-ZCodeProcessOwnership -Pids @($candidates | Where-Object { $_ -gt 4 } | Sort-Object -Unique) -RepoRoot $RepoRoot
}

function Wait-ZCodePortReleased {
    param([int]$Port, [int]$TimeoutSeconds = 10)
    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    while ((Get-Date) -lt $deadline) {
        if (-not @(Get-NetTCPConnection -State Listen -LocalPort $Port -ErrorAction SilentlyContinue).Count) {
            return $true
        }
        Start-Sleep -Milliseconds 150
    }
    return $false
}

function Stop-ZCodeProcessTree {
    param([int]$ProcessId)
    foreach ($child in @(Get-CimInstance Win32_Process -Filter "ParentProcessId = $ProcessId" -ErrorAction SilentlyContinue)) {
        Stop-ZCodeProcessTree -ProcessId ([int]$child.ProcessId)
    }
    Stop-Process -Id $ProcessId -Force -ErrorAction SilentlyContinue
}

function Stop-ZCodeStaleProcesses {
    param([string]$RepoRoot, [int[]]$Ports)
    $rows = @(Get-ZCodeStalePids -RepoRoot $RepoRoot -Ports $Ports)
    if ($rows.Count -eq 0) {
        Write-ZCodeStep "ports $($Ports -join ', ') are free"
        return
    }
    $foreign = @()
    foreach ($row in $rows) {
        if (-not $row.Mine) {
            $foreign += $row
            Write-ZCodeStep "port held by a process outside this repository: pid $($row.ProcessId) $($row.Name)"
            continue
        }
        Write-ZCodeStep "stopping previous ZCode process pid $($row.ProcessId) ($($row.Name))"
        Stop-ZCodeProcessTree -ProcessId ([int]$row.ProcessId)
    }
    if ($foreign.Count -gt 0) {
        Stop-ZCodeFail ('Refusing to kill processes that do not belong to this repository: ' +
            (($foreign | ForEach-Object { "pid $($_.ProcessId) $($_.Name)" }) -join '; ') +
            '. Stop them yourself or pass a different -ApiPort/-WebPort/-Port.')
    }
    foreach ($port in $Ports) {
        if (-not (Wait-ZCodePortReleased -Port $port)) {
            Stop-ZCodeFail "port $port is still in use after stopping the previous ZCode processes"
        }
    }
}

function Invoke-ZCodeNodeScript {
    param([string]$Node, [string]$ScriptPath, [string[]]$Arguments)
    Write-ZCodeStep "node $([System.IO.Path]::GetFileName($ScriptPath)) $($Arguments -join ' ')"
    & $Node $ScriptPath @Arguments
    return $LASTEXITCODE
}
