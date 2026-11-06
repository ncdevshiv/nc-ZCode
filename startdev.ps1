<#
.SYNOPSIS
    Start the complete ZCode app in development mode on the Rust backend.

.DESCRIPTION
    1. closes the previous ZCode processes holding -ApiPort / -WebPort (only those
       owned by this repository; a foreign listener is reported, never killed);
    2. runs scripts\dev-web.mjs, which builds zcode-server (debug) when a native
       source is newer than the binary, starts it, starts Vite for frontend HMR,
       asserts /healthz plus the Vite -> backend proxy, and restarts only the backend
       whenever native\crates changes.

    The app is then served by Vite (fast HMR) with /api and /ws proxied to the Rust
    backend, which hosts the agent runtime, services, session store, terminal/git/
    workspace channels and the automation scheduler in one process.

.PARAMETER ApiPort
    Rust backend port; Vite proxies /api and /ws here (default 3030).

.PARAMETER WebPort
    Vite dev server port with HMR (default 5173).

.PARAMETER WorkspacePath
    Workspace opened by the backend (defaults to the repository root).

.PARAMETER ZCodeEnv
    test or production endpoints for the frontend bundle (default test).

.PARAMETER DataDir
    ZCODE_DATA_BASE_DIR override; defaults to the shared ~/.zcode data root, so an
    existing account, credential and session history are reused.

.PARAMETER AuthToken
    Require this token on /api/* and /ws* (default: none on loopback).

.PARAMETER NoBuild
    Start the existing binary without running cargo.

.PARAMETER Rebuild
    Force cargo even when the binary looks fresh.

.PARAMETER NoWatch
    Disable the backend hot restart (frontend HMR always stays on).

.EXAMPLE
    .\startdev.ps1

.EXAMPLE
    .\startdev.ps1 -WorkspacePath G:\work\my-app -NoWatch
#>
[CmdletBinding()]
param(
    [int]$ApiPort = 3030,
    [int]$WebPort = 5173,
    [string]$WorkspacePath = '',
    [ValidateSet('', 'test', 'production')]
    [string]$ZCodeEnv = '',
    [string]$DataDir = '',
    [string]$AuthToken = '',
    [switch]$NoBuild,
    [switch]$Rebuild,
    [switch]$NoWatch
)

Set-StrictMode -Version 3.0
$ErrorActionPreference = 'Stop'

. (Join-Path $PSScriptRoot 'scripts\zcode-dev-common.ps1')

$RepoRoot = $PSScriptRoot
$node = Get-ZCodeRequiredCommand -Name 'node' -Why 'run the ZCode development entry'
if ($ZCodeEnv) { $env:ZCODE_ENV = $ZCodeEnv }

Stop-ZCodeStaleProcesses -RepoRoot $RepoRoot -Ports @($ApiPort, $WebPort)

$arguments = @('--api-port', $ApiPort, '--web-port', $WebPort)
if ($WorkspacePath) { $arguments += @('--workspace', $WorkspacePath) }
if ($DataDir) { $arguments += @('--data-dir', $DataDir) }
if ($AuthToken) { $arguments += @('--token', $AuthToken) }
if ($NoBuild) { $arguments += '--no-build' }
if ($Rebuild) { $arguments += '--rebuild' }
if ($NoWatch) { $arguments += '--no-watch' }

exit (Invoke-ZCodeNodeScript -Node $node -ScriptPath (Join-Path $RepoRoot 'scripts\dev-web.mjs') `
    -Arguments $arguments)
