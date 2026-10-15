# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Firelock, LLC

# Exercise actual release binaries and native worker authority in an isolated
# home. Lock obstruction consumes the worker before any HTTP request. Markers
# and object identities are created exclusively by the actual native commands.
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$OutputRoot,
    [Parameter(Mandatory = $true)]
    [string]$KinCliPath,
    [Parameter(Mandatory = $true)]
    [string]$KinDaemonPath,
    [string]$InstallerPath = (Join-Path $PSScriptRoot 'install.ps1')
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$global:LASTEXITCODE = 0
if ([Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT) {
    throw 'Native Windows worker controls require Windows.'
}
$OutputRoot = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($OutputRoot)
if (Test-Path -LiteralPath $OutputRoot) { throw 'OutputRoot must be new.' }
New-Item -ItemType Directory -Path $OutputRoot | Out-Null
$KinCliPath = (Resolve-Path -LiteralPath $KinCliPath).Path
$KinDaemonPath = (Resolve-Path -LiteralPath $KinDaemonPath).Path

# Load only the production argument, synchronous transport and native-dispatch
# helpers. No installer entry point or wrapper filesystem cleanup is invoked.
$Tokens = $null
$ParseErrors = $null
$Ast = [System.Management.Automation.Language.Parser]::ParseFile($InstallerPath, [ref]$Tokens, [ref]$ParseErrors)
if (@($ParseErrors).Count -ne 0) { throw "Installer parse failed: $ParseErrors" }
foreach ($HelperName in @('Normalize-KinRuntimePath', 'ConvertTo-KinWindowsArgument',
                          'Invoke-KinWindowsProcess', 'Invoke-KinWindowsWorkerCleanup')) {
    $Definitions = @($Ast.FindAll({
        param($Node)
        $Node -is [System.Management.Automation.Language.FunctionDefinitionAst] -and $Node.Name -ceq $HelperName
    }, $true))
    if ($Definitions.Count -ne 1) { throw "Expected exactly one production helper $HelperName." }
    Invoke-Expression $Definitions[0].Extent.Text
}

$FixtureHome = Join-Path $OutputRoot 'managed home with spaces'
$FixtureBin = Join-Path $FixtureHome 'bin'
New-Item -ItemType Directory -Path $FixtureBin -Force | Out-Null
New-Item -ItemType Directory -Path (Join-Path $FixtureHome 'lib') | Out-Null
$CanonicalCli = Join-Path $FixtureBin 'kin.exe'
$CanonicalDaemon = Join-Path $FixtureBin 'kin-daemon.exe'
Copy-Item -LiteralPath $KinCliPath -Destination $CanonicalCli
Copy-Item -LiteralPath $KinDaemonPath -Destination $CanonicalDaemon
$PairHashes = @{
    cli = (Get-FileHash -LiteralPath $CanonicalCli -Algorithm SHA256).Hash
    daemon = (Get-FileHash -LiteralPath $CanonicalDaemon -Algorithm SHA256).Hash
}
$Rows = [Collections.Generic.List[object]]::new()
$SavedEnvironment = @{}
foreach ($VariableName in @('KIN_HOME', 'KIN_DIR', 'KIN_RUNTIME_MODE', 'KIN_DAEMON_BIN', 'KIN_MANAGED_BIN',
                            'KIN_BINARY_PATH', 'KIN_MCP_KIN_BINARY', 'KIN_DAEMON_URL', 'KIN_VERSION', 'KIN_BASE_URL')) {
    $SavedEnvironment[$VariableName] = [Environment]::GetEnvironmentVariable($VariableName, 'Process')
    Remove-Item -LiteralPath "Env:$VariableName" -ErrorAction SilentlyContinue
}
$env:KIN_HOME = $FixtureHome
$env:KIN_RUNTIME_MODE = 'managed'

function Assert-Control([bool]$Condition, [string]$Message) {
    if (-not $Condition) { throw $Message }
}
function Run-Control([string]$ControlName, [scriptblock]$Test) {
    & $Test
    $Rows.Add([pscustomobject]@{ name = $ControlName; status = 'PASS' })
}
function Prepare-RealWorker {
    $Prepared = Invoke-KinWindowsProcess $CanonicalCli @('update', '--prepare-windows-worker') $FixtureHome -CaptureOutput
    Assert-Control ($Prepared.ExitCode -eq 0) "Native prepare failed: $($Prepared.Stderr)"
    $Worker = $Prepared.Stdout | ConvertFrom-Json
    Assert-Control ($Worker.schema -ceq 'kin.windows-update-worker.v1') 'Native prepare schema mismatch.'
    $LockPath = Normalize-KinRuntimePath $Worker.authority_lock_file
    $LockFile = Get-Item -LiteralPath $LockPath
    Assert-Control (-not $LockFile.PSIsContainer -and
        -not ($LockFile.Attributes -band [IO.FileAttributes]::ReparsePoint)) 'Reported native lock is not a real file.'
    Assert-Control ((Normalize-KinRuntimePath $LockFile.DirectoryName) -eq (Normalize-KinRuntimePath $OutputRoot)) 'Reported native lock is outside the fixture parent.'
    Assert-Control ($LockFile.Name -match '^\.kin-install-authority-[0-9a-f]{64}\.lock$') 'Reported native lock basename mismatch.'
    return [pscustomobject]@{
        Worker = $Worker
        Directory = [IO.Path]::GetDirectoryName((Normalize-KinRuntimePath $Worker.worker_executable))
        LockPath = $LockPath
        HandoffHash = (Get-FileHash -LiteralPath (Normalize-KinRuntimePath $Worker.handoff_record) -Algorithm SHA256).Hash.ToLowerInvariant()
        Result = $null
    }
}
function Consume-RealWorkerOffline($Fixture) {
    $SavedLock = Join-Path $OutputRoot ('owned-lock-' + [Guid]::NewGuid().ToString('N'))
    Move-Item -LiteralPath $Fixture.LockPath -Destination $SavedLock
    New-Item -ItemType Directory -Path $Fixture.LockPath | Out-Null
    try {
        $Fixture.Result = Invoke-KinWindowsProcess $Fixture.Worker.worker_executable @(
            'update', '--windows-worker', $Fixture.Worker.handoff_record, '--apply'
        ) $Fixture.Directory -CaptureOutput
        Assert-Control ($Fixture.Result.ExitCode -ne 0) 'Owned lock obstruction did not fail the real worker.'
        $Claim = Get-Content -LiteralPath (Join-Path $Fixture.Directory 'claimed') -Raw | ConvertFrom-Json
        Assert-Control ($Claim.schema -ceq 'kin.windows-update-worker-consumed.v1') 'Actual worker did not emit the consumed schema.'
        Assert-Control ($Claim.worker_process.pid -eq $Fixture.Result.ProcessId) 'Consumed identity does not match waited child.'
        Assert-Control ($Claim.handoff_sha256 -ceq $Fixture.HandoffHash) 'Consumed identity does not bind original handoff.'
        return $Fixture
    } finally {
        # Remove only the empty obstruction created by this fixture, then put
        # back the identical native-owned lock. No recursive deletion is used.
        [IO.Directory]::Delete($Fixture.LockPath, $false)
        Move-Item -LiteralPath $SavedLock -Destination $Fixture.LockPath
    }
}
function Clean-RealWorker($Fixture) {
    return Invoke-KinWindowsWorkerCleanup $CanonicalCli $Fixture.Worker $Fixture.Result $Fixture.HandoffHash $FixtureHome
}
function Assert-Retained($Fixture) {
    Assert-Control (Test-Path -LiteralPath (Normalize-KinRuntimePath $Fixture.Worker.worker_executable)) 'Worker helper was unexpectedly removed.'
    Assert-Control (Test-Path -LiteralPath (Normalize-KinRuntimePath $Fixture.Worker.handoff_record)) 'Handoff authority was unexpectedly removed.'
}
function Assert-Cleaned($Fixture) {
    Assert-Control (Clean-RealWorker $Fixture) 'Actual native cleanup did not remove the consumed handoff.'
    Assert-Control (-not (Test-Path -LiteralPath $Fixture.Directory)) 'Native cleanup left the consumed directory.'
}

try {
    $Status = Invoke-KinWindowsProcess $CanonicalCli @('runtime', 'status', '--json') $FixtureHome -CaptureOutput
    Assert-Control ($Status.ExitCode -eq 0) "Actual candidate pair is not ready: $($Status.Stdout) $($Status.Stderr)"
    $InitialStatus = $Status.Stdout | ConvertFrom-Json
    Assert-Control ($InitialStatus.ready -eq $true -and $InitialStatus.pair_matches -eq $true -and
        $InitialStatus.mode -ceq 'managed' -and $InitialStatus.cli.source_dirty -eq $false) 'Candidate pair does not have clean managed identity.'

    Run-Control 'real-consumed-failure-cleans-with-original-exit-preserved' {
        $Fixture = Consume-RealWorkerOffline (Prepare-RealWorker)
        $OriginalExit = $Fixture.Result.ExitCode
        Assert-Cleaned $Fixture
        Assert-Control ($Fixture.Result.ExitCode -eq $OriginalExit -and $OriginalExit -ne 0) 'Cleanup rewrote a failed update result.'
    }
    Run-Control 'inherited-native-error-policy-cannot-hide-returned-child' {
        $PreviousNativePolicy = Get-Variable PSNativeCommandUseErrorActionPreference -ErrorAction SilentlyContinue
        $SavedNativePolicy = if ($null -ne $PreviousNativePolicy) { $PreviousNativePolicy.Value } else { $null }
        try {
            $PSNativeCommandUseErrorActionPreference = $true
            $Fixture = Consume-RealWorkerOffline (Prepare-RealWorker)
            Assert-Control ($Fixture.Result.ExitCode -ne 0 -and $Fixture.Result.ProcessId -gt 0) 'Inherited policy hid native completion.'
            Assert-Cleaned $Fixture
        } finally {
            if ($null -ne $PreviousNativePolicy) { $PSNativeCommandUseErrorActionPreference = $SavedNativePolicy }
            else { Remove-Variable PSNativeCommandUseErrorActionPreference -ErrorAction SilentlyContinue }
        }
    }
    Run-Control 'unconsumed-native-handoff-is-preserved' {
        $Fixture = Prepare-RealWorker
        $Cleanup = Invoke-KinWindowsProcess $CanonicalCli @('update', '--cleanup-windows-worker', $Fixture.Worker.handoff_record) $FixtureHome -CaptureOutput
        Assert-Control ($Cleanup.ExitCode -ne 0) 'Native cleanup accepted an unconsumed handoff.'
        Assert-Retained $Fixture
        $Fixture = Consume-RealWorkerOffline $Fixture
        Assert-Cleaned $Fixture
    }
    $BackupFenceName = '.update-backup-' + [Guid]::NewGuid().ToString()
    foreach ($FenceName in @($BackupFenceName, 'update-restart-ack-required.json', 'update-mcp-repair-pending.json')) {
        Run-Control "native-recovery-fence-$FenceName-preserves" {
            $Fixture = Consume-RealWorkerOffline (Prepare-RealWorker)
            $FencePath = Join-Path $FixtureHome $FenceName
            [IO.File]::WriteAllText($FencePath, 'malformed fixture authority')
            try {
                Assert-Control (-not (Clean-RealWorker $Fixture)) 'Native cleanup ignored a recovery fence.'
                Assert-Retained $Fixture
                Assert-Control ([IO.File]::ReadAllText($FencePath) -ceq 'malformed fixture authority') 'Recovery fence was altered.'
            } finally { Remove-Item -LiteralPath $FencePath }
            Assert-Cleaned $Fixture
        }
    }
    Run-Control 'unknown-private-entry-preserves-whole-native-directory' {
        $Fixture = Consume-RealWorkerOffline (Prepare-RealWorker)
        $UnknownPath = Join-Path $Fixture.Directory 'unknown.txt'
        [IO.File]::WriteAllText($UnknownPath, 'unknown fixture contents')
        try {
            Assert-Control (-not (Clean-RealWorker $Fixture)) 'Unknown private entry was ignored.'
            Assert-Retained $Fixture
            Assert-Control ([IO.File]::ReadAllText($UnknownPath) -ceq 'unknown fixture contents') 'Unknown entry was altered.'
        } finally { Remove-Item -LiteralPath $UnknownPath }
        Assert-Cleaned $Fixture
    }
    foreach ($ComponentName in @('worker_executable', 'handoff_record')) {
        Run-Control "equal-bytes-replaced-native-$ComponentName-is-preserved" {
            $Fixture = Consume-RealWorkerOffline (Prepare-RealWorker)
            $OriginalPath = Normalize-KinRuntimePath $Fixture.Worker.$ComponentName
            $OriginalObject = Join-Path $OutputRoot ('owned-original-' + [Guid]::NewGuid().ToString('N'))
            Move-Item -LiteralPath $OriginalPath -Destination $OriginalObject
            Copy-Item -LiteralPath $OriginalObject -Destination $OriginalPath
            try {
                Assert-Control (-not (Clean-RealWorker $Fixture)) 'Equal bytes hid a changed native object.'
                Assert-Retained $Fixture
            } finally {
                Remove-Item -LiteralPath $OriginalPath
                Move-Item -LiteralPath $OriginalObject -Destination $OriginalPath
            }
            Assert-Cleaned $Fixture
        }
    }
    Run-Control 'caller-waited-pid-and-record-hash-bind-native-dispatch' {
        $Fixture = Consume-RealWorkerOffline (Prepare-RealWorker)
        $WrongResult = [pscustomobject]@{ ExitCode = $Fixture.Result.ExitCode; ProcessId = $Fixture.Result.ProcessId + 4 }
        Assert-Control (-not (Invoke-KinWindowsWorkerCleanup $CanonicalCli $Fixture.Worker $WrongResult $Fixture.HandoffHash $FixtureHome)) 'Caller accepted another worker PID.'
        Assert-Control (-not (Invoke-KinWindowsWorkerCleanup $CanonicalCli $Fixture.Worker $Fixture.Result ('0' * 64) $FixtureHome)) 'Caller accepted another handoff hash.'
        Assert-Retained $Fixture
        Assert-Cleaned $Fixture
    }
    Assert-Control ((Get-FileHash -LiteralPath $CanonicalCli -Algorithm SHA256).Hash -ceq $PairHashes.cli) 'Offline controls changed canonical CLI.'
    Assert-Control ((Get-FileHash -LiteralPath $CanonicalDaemon -Algorithm SHA256).Hash -ceq $PairHashes.daemon) 'Offline controls changed canonical daemon.'
    $Receipt = [ordered]@{
        schema = 'kin.windows-worker-cleanup-controls.v2'
        installer_sha256 = (Get-FileHash -LiteralPath $InstallerPath -Algorithm SHA256).Hash.ToLowerInvariant()
        powershell_version = $PSVersionTable.PSVersion.ToString()
        cli_sha256 = $PairHashes.cli.ToLowerInvariant()
        daemon_sha256 = $PairHashes.daemon.ToLowerInvariant()
        runtime_identity = $InitialStatus
        controls = @($Rows)
        release_install_qualification = $false
    }
    $Receipt | ConvertTo-Json -Depth 20 | Set-Content -LiteralPath (Join-Path $OutputRoot 'receipt.json')
    Write-Host "Native Windows copied-worker controls passed: $($Rows.Count)"
} finally {
    foreach ($VariableName in $SavedEnvironment.Keys) {
        [Environment]::SetEnvironmentVariable($VariableName, $SavedEnvironment[$VariableName], 'Process')
    }
}
