<#
.SYNOPSIS
    Start the complete ZCode app from production build artifacts on the Rust backend.

.DESCRIPTION
    1. closes the previous ZCode processes holding -Port (only those owned by this
       repository - this includes a running startdev.ps1 session, so production can
       take the port over);
    2. runs scripts\prod-web.mjs, which builds zcode-server in release mode when a
       native source is newer than the binary, builds the frontend bundle with Vite,
       and starts the backend with --static-dir so one process serves the SPA, /api,
       /ws and the channel RPC registry. No Node server is part of the running app.
    3. the entry asserts the real product surface before reporting success: /healthz
       with a non-empty channel registry, /api/server-info v1, index.html and the
       hashed entry asset it references.

.PARAMETER Port
    Port the Rust backend binds and serves the app from (default 3030).

.PARAMETER HostAddress
    Bind address; anything but loopback requires -AuthToken.

.PARAMETER WorkspacePath
    Workspace opened by the backend (defaults to the repository root).

.PARAMETER ZCodeEnv
    test or production endpoints baked into the frontend bundle (default production).

.PARAMETER DataDir
    ZCODE_DATA_BASE_DIR override; defaults to the shared ~/.zcode data root.

.PARAMETER AuthToken
    Token required on /api/* and /ws* (query ?token= or cookie).

.PARAMETER Open
    Open the app in the default browser once it is healthy.

.PARAMETER NoBuild
    Start existing artifacts without running cargo or Vite.

.PARAMETER Rebuild
    Force both builds even when the artifacts look fresh.

.EXAMPLE
    .\startprod.ps1

.EXAMPLE
    .\startprod.ps1 -Port 8080 -AuthToken (Read-Host -AsSecureString) # see notes on plain-text token
#>
[CmdletBinding()]
param(
    [int]$Port = 3030,
    [string]$HostAddress = '127.0.0.1',
    [string]$WorkspacePath = '',
    [ValidateSet('', 'test', 'production')]
    [string]$ZCodeEnv = '',
    [string]$DataDir = '',
    [string]$AuthToken = '',
    [switch]$Open,
    [switch]$NoBuild,
    [switch]$Rebuild
)

Set-StrictMode -Version 3.0
$ErrorActionPreference = 'Stop'

. (Join-Path $PSScriptRoot 'scripts\zcode-dev-common.ps1')

$RepoRoot = $PSScriptRoot
$node = Get-ZCodeRequiredCommand -Name 'node' -Why 'run the ZCode production entry'
if ($ZCodeEnv) { $env:ZCODE_ENV = $ZCodeEnv }

Stop-ZCodeStaleProcesses -RepoRoot $RepoRoot -Ports @($Port)

$arguments = @('--port', $Port, '--host', $HostAddress)
if ($WorkspacePath) { $arguments += @('--workspace', $WorkspacePath) }
if ($DataDir) { $arguments += @('--data-dir', $DataDir) }
if ($AuthToken) { $arguments += @('--token', $AuthToken) }
if ($Open) { $arguments += '--open' }
if ($NoBuild) { $arguments += '--no-build' }
if ($Rebuild) { $arguments += '--rebuild' }

exit (Invoke-ZCodeNodeScript -Node $node -ScriptPath (Join-Path $RepoRoot 'scripts\prod-web.mjs') `
    -Arguments $arguments)
