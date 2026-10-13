# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Firelock, LLC
<#+.SYNOPSIS
Exercise the real Windows installer against a local release mirror.
.DESCRIPTION
Downloads the reviewed v0.8.6 ZIP and its public checksum once, then tests the
candidate install.ps1 in child pwsh processes using file:// paths with spaces.
The receipt covers installer boundaries against stable binaries. Candidate
binaries still require their own RC Build and release qualification.
User PATH changes require a disposable GitHub-hosted Windows x86_64 account.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidateNotNullOrEmpty()]
    [string]$OutputRoot,

    [ValidateNotNullOrEmpty()]
    [string]$InstallerPath = (Join-Path $PSScriptRoot "install.ps1")
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

if (
    -not [System.Runtime.InteropServices.RuntimeInformation]::IsOSPlatform(
        [System.Runtime.InteropServices.OSPlatform]::Windows
    ) -or
    [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString() -cne "X64" -or
    [System.Runtime.InteropServices.RuntimeInformation]::ProcessArchitecture.ToString() -cne "X64"
) {
    throw "The mirror installer proof requires native Windows x86_64 PowerShell."
}
if ($env:GITHUB_ACTIONS -cne "true" -or $env:RUNNER_ENVIRONMENT -cne "github-hosted") {
    throw "The mirror installer proof requires a disposable GitHub-hosted account."
}
if ($PSVersionTable.PSVersion.Major -lt 7) {
    throw "The mirror installer proof requires PowerShell 7 and child pwsh."
}

$OutputRoot = [System.IO.Path]::GetFullPath($OutputRoot)
$InstallerPath = (Resolve-Path -LiteralPath $InstallerPath).Path
if (Test-Path -LiteralPath $OutputRoot) {
    throw "OutputRoot must be new; refusing to replace earlier proof: $OutputRoot"
}
$Ancestor = [System.IO.DirectoryInfo]::new($OutputRoot)
while ($null -ne $Ancestor) {
    if (Test-Path -LiteralPath (Join-Path $Ancestor.FullName ".git")) {
        throw "OutputRoot must be outside Git repositories."
    }
    $Ancestor = $Ancestor.Parent
}

$Pwsh = (Get-Command pwsh -CommandType Application -ErrorAction Stop).Source
$ArchiveName = "kin-windows-x86_64.zip"
$Version = "0.8.6"
$ExpectedArchiveHash = "b40bb7d72b2313dceaf806b76c58dd8c7b06cbf62a2a57cdd21d7a31f228e392"
$ReleaseBase = "https://github.com/kinlab-ai/kin/releases/download/v$Version"
$Utf8 = [System.Text.UTF8Encoding]::new($false)

function Write-Json {
    param([string]$Path, [object]$Value)
    [System.IO.File]::WriteAllText($Path, ($Value | ConvertTo-Json -Depth 20) + "`n", $Utf8)
}

function Get-Sha256 {
    param([string]$Path)
    return (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
}

function Invoke-CapturedProcess {
    param(
        [string]$Executable,
        [string[]]$Arguments,
        [string]$CaptureDirectory,
        [string]$WorkingDirectory
    )
    New-Item -ItemType Directory -Path $CaptureDirectory | Out-Null
    $Stdout = Join-Path $CaptureDirectory "stdout.txt"
    $Stderr = Join-Path $CaptureDirectory "stderr.txt"
    $Child = $null
    $Record = [ordered]@{
        executable = $Executable
        arguments = @($Arguments)
        exit_code = $null
        timed_out = $false
    }
    try {
        $Child = Start-Process -FilePath $Executable -ArgumentList $Arguments `
            -WorkingDirectory $WorkingDirectory -NoNewWindow -PassThru `
            -RedirectStandardOutput $Stdout -RedirectStandardError $Stderr
        if (-not $Child.WaitForExit(120000)) {
            $Record.timed_out = $true
            $Child.Kill($true)
        }
        $Child.WaitForExit()
        $Child.Refresh()
        $Record.exit_code = [int]$Child.ExitCode
    } catch {
        $Record["error"] = $_.Exception.Message
        if ($null -ne $Child -and -not $Child.HasExited) {
            $Child.Kill($true)
            $Child.WaitForExit()
        }
    } finally {
        foreach ($Path in @($Stdout, $Stderr)) {
            if (-not (Test-Path -LiteralPath $Path)) {
                [System.IO.File]::WriteAllText($Path, "", $Utf8)
            }
        }
        Write-Json (Join-Path $CaptureDirectory "exit.json") $Record
        if ($null -ne $Child) { $Child.Dispose() }
    }
    return [pscustomobject]@{
        ExitCode = $Record.exit_code
        TimedOut = $Record.timed_out
        Stdout = [System.IO.File]::ReadAllText($Stdout)
        Stderr = [System.IO.File]::ReadAllText($Stderr)
    }
}

function Get-ZipComponents {
    param([string]$Archive)
    $Zip = [System.IO.Compression.ZipFile]::OpenRead($Archive)
    $Components = [ordered]@{}
    try {
        foreach ($Name in @("kin.exe", "kin-daemon.exe")) {
            $Entries = @($Zip.Entries | Where-Object { $_.FullName -ceq $Name })
            if ($Entries.Count -ne 1 -or $Entries[0].Length -le 0) {
                throw "The reviewed ZIP must carry exactly one nonempty $Name."
            }
            $Stream = $Entries[0].Open()
            $Hasher = [System.Security.Cryptography.SHA256]::Create()
            try {
                $Hash = [System.BitConverter]::ToString($Hasher.ComputeHash($Stream)).Replace("-", "").ToLowerInvariant()
                $Components[$Name] = [ordered]@{
                    sha256 = $Hash
                    size_bytes = [long]$Entries[0].Length
                }
            } finally {
                $Hasher.Dispose()
                $Stream.Dispose()
            }
        }
    } finally {
        $Zip.Dispose()
    }
    return $Components
}

function Invoke-InstallCase {
    param([System.Collections.IDictionary]$Case)
    $Capture = Join-Path $OutputRoot $Case.name
    $CaseScratch = Join-Path $Scratch $Case.name
    New-Item -ItemType Directory -Path $Capture | Out-Null
    New-Item -ItemType Directory -Path $CaseScratch | Out-Null
    $Mirror = Join-Path $CaseScratch "mirror with spaces"
    $Download = Join-Path $Mirror "download/v$Version"
    New-Item -ItemType Directory -Path $Download -Force | Out-Null
    $MirrorArchive = Join-Path $Download $ArchiveName
    $InstallHome = Join-Path $CaseScratch "install home with spaces"
    $InstallerTemp = Join-Path $CaseScratch "installer temp with spaces"
    New-Item -ItemType Directory -Path $InstallerTemp | Out-Null
    Copy-Item -LiteralPath $Case.archive -Destination $MirrorArchive
    $CaseHash = Get-Sha256 $MirrorArchive
    $Sidecar = "$MirrorArchive.sha256"
    switch ($Case.checksum) {
        "published" { Copy-Item -LiteralPath $SourceChecksum -Destination $Sidecar }
        "missing" { }
        "malformed" { [System.IO.File]::WriteAllText($Sidecar, "not-a-sha  $ArchiveName`n", $Utf8) }
        "wrong" { [System.IO.File]::WriteAllText($Sidecar, (("0" * 64) + "  $ArchiveName`n"), $Utf8) }
        "mutated" { [System.IO.File]::WriteAllText($Sidecar, "$CaseHash  $ArchiveName`n", $Utf8) }
        default { throw "Unknown checksum fixture: $($Case.checksum)" }
    }
    $MirrorUri = [Uri]::new($Mirror + [System.IO.Path]::DirectorySeparatorChar).AbsoluteUri.TrimEnd("/")
    $Row = [ordered]@{
        name = $Case.name
        status = "FAIL"
        archive_sha256 = $CaseHash
        checksum_fixture = $Case.checksum
        checksum_sha256 = if (Test-Path -LiteralPath $Sidecar) { Get-Sha256 $Sidecar } else { $null }
        mirror_uri = $MirrorUri
        expected_success = $Case.success
        installer_exit_code = $null
        installed = [ordered]@{}
    }
    try {
        [Environment]::SetEnvironmentVariable("KIN_HOME", $InstallHome, "Process")
        [Environment]::SetEnvironmentVariable("KIN_DIR", $InstallHome, "Process")
        [Environment]::SetEnvironmentVariable("KIN_VERSION", $Version, "Process")
        [Environment]::SetEnvironmentVariable("KIN_NO_SETUP", "1", "Process")
        [Environment]::SetEnvironmentVariable("KIN_BASE_URL", $MirrorUri, "Process")
        [Environment]::SetEnvironmentVariable("TEMP", $InstallerTemp, "Process")
        [Environment]::SetEnvironmentVariable("TMP", $InstallerTemp, "Process")
        # Start-Process joins its argument array on Windows; quote the real
        # script path so a source checkout with spaces is supported too.
        $QuotedInstaller = '"{0}"' -f $InstallerPath.Replace('"', '\"')
        $Child = Invoke-CapturedProcess $Pwsh @(
            "-NoLogo", "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass",
            "-File", $QuotedInstaller
        ) (Join-Path $Capture "installer") $CaseScratch
        $Row.installer_exit_code = $Child.ExitCode
        $Row["installer_timed_out"] = $Child.TimedOut
        $Bin = Join-Path $InstallHome "bin"
        if (Test-Path -LiteralPath $Bin) {
            foreach ($File in @(Get-ChildItem -LiteralPath $Bin -File)) {
                $Row.installed[$File.Name] = [ordered]@{
                    sha256 = Get-Sha256 $File.FullName
                    size_bytes = [long]$File.Length
                }
            }
        }
        if ($Child.TimedOut -or $null -eq $Child.ExitCode) {
            throw "The installer did not finish within its deadline."
        }
        if ($Case.success) {
            if ($Child.ExitCode -ne 0) { throw "Valid file mirror install failed with exit $($Child.ExitCode)." }
            foreach ($Name in @("kin.exe", "kin-daemon.exe")) {
                if (-not $Row.installed.Contains($Name)) { throw "Valid mirror install omitted $Name." }
                if (
                    $Row.installed[$Name].sha256 -cne $OriginalComponents[$Name].sha256 -or
                    $Row.installed[$Name].size_bytes -ne $OriginalComponents[$Name].size_bytes
                ) {
                    throw "Installed $Name bytes differ from the reviewed ZIP entry."
                }
                $InstalledPath = Join-Path $Bin $Name
                $VersionResult = Invoke-CapturedProcess $InstalledPath @("--version") `
                    (Join-Path $Capture "$Name-version") $CaseScratch
                $Row.installed[$Name]["version_exit_code"] = $VersionResult.ExitCode
                $Row.installed[$Name]["version_text"] = $VersionResult.Stdout.Trim()
                $Program = [regex]::Escape(($Name -replace '\.exe$', ''))
                if (
                    $VersionResult.TimedOut -or $VersionResult.ExitCode -ne 0 -or
                    $VersionResult.Stdout.Trim() -cnotmatch "^$Program 0\.8\.6(?: \([^\r\n]*\))?$"
                ) {
                    throw "Installed $Name did not report the stable baseline version."
                }
            }
        } else {
            if ($Child.ExitCode -eq 0) { throw "Invalid mirror install unexpectedly succeeded." }
            if ($Row.installed.Count -ne 0) { throw "Invalid mirror install left installed binaries." }
            if (($Child.Stdout + $Child.Stderr) -notmatch [regex]::Escape($Case.diagnostic)) {
                throw "Invalid mirror failed without its expected boundary diagnostic: $($Case.diagnostic)"
            }
            $Row["expected_diagnostic"] = $Case.diagnostic
        }
        $Row.status = "PASS"
    } catch {
        $Row["error"] = $_.Exception.Message
    } finally {
        # The installer can mutate HKCU PATH even though the install directory
        # is isolated. Restore it between cases and again at harness teardown.
        [Environment]::SetEnvironmentVariable("Path", $OriginalUserPath, "User")
        Write-Json (Join-Path $Capture "case.json") $Row
    }
    return $Row
}

New-Item -ItemType Directory -Path $OutputRoot | Out-Null
$Scratch = Join-Path $OutputRoot "scratch"
New-Item -ItemType Directory -Path $Scratch | Out-Null
$OriginalUserPath = [Environment]::GetEnvironmentVariable("Path", "User")
$SavedEnvironment = @{}
foreach ($Name in @("KIN_HOME", "KIN_DIR", "KIN_VERSION", "KIN_NO_SETUP", "KIN_BASE_URL", "TEMP", "TMP")) {
    $SavedEnvironment[$Name] = [Environment]::GetEnvironmentVariable($Name, "Process")
}
$Result = [ordered]@{
    schema = "kin.windows-installer-boundary.v1"
    coverage = "installer-boundary"
    status = "FAIL"
    baseline = [ordered]@{ tag = "v0.8.6"; version = $Version; archive_sha256 = $ExpectedArchiveHash }
    installer = [ordered]@{ path = $InstallerPath; sha256 = Get-Sha256 $InstallerPath }
    host_scope = "disposable GitHub-hosted native Windows x86_64 account"
    scope = "real installer file-mirror/checksum/archive boundaries using stable released bytes; candidate binaries require separate qualification"
    cases = [System.Collections.Generic.List[object]]::new()
    environment_restored = $false
    user_path_restored = $false
    scratch_removed = $false
}
$Succeeded = $false
try {
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $SourceArchive = Join-Path $Scratch $ArchiveName
    $SourceChecksum = Join-Path $OutputRoot "$ArchiveName.sha256"
    Invoke-WebRequest -Uri "$ReleaseBase/$ArchiveName" -OutFile $SourceArchive -UseBasicParsing
    Invoke-WebRequest -Uri "$ReleaseBase/$ArchiveName.sha256" -OutFile $SourceChecksum -UseBasicParsing
    $ActualArchiveHash = Get-Sha256 $SourceArchive
    $Result["source_archive"] = [ordered]@{
        url = "$ReleaseBase/$ArchiveName"
        sha256 = $ActualArchiveHash
        size_bytes = [long](Get-Item -LiteralPath $SourceArchive).Length
        retained = $true
    }
    $Result["source_checksum"] = [ordered]@{
        url = "$ReleaseBase/$ArchiveName.sha256"
        sha256 = Get-Sha256 $SourceChecksum
        captured_file = "$ArchiveName.sha256"
    }
    if ($ActualArchiveHash -cne $ExpectedArchiveHash) {
        throw "The published stable ZIP differs from the reviewed baseline hash."
    }
    $PublicChecksum = [System.IO.File]::ReadAllText($SourceChecksum)
    $ChecksumPattern = '^' + $ExpectedArchiveHash + '[ \t]+\*?' + [regex]::Escape($ArchiveName) + '\r?\n?$'
    if ($PublicChecksum -cnotmatch $ChecksumPattern) {
        throw "The public stable sidecar does not bind the reviewed ZIP hash and name."
    }
    $OriginalComponents = Get-ZipComponents $SourceArchive
    $Result.source_archive["components"] = $OriginalComponents
    # Delete the actual daemon ZIP entry, preserving the other real release
    # bytes. Its updated valid checksum isolates archive-shape refusal.
    $MissingDaemonArchive = Join-Path $Scratch "missing-daemon.zip"
    Copy-Item -LiteralPath $SourceArchive -Destination $MissingDaemonArchive
    $Mutant = [System.IO.Compression.ZipFile]::Open($MissingDaemonArchive, [System.IO.Compression.ZipArchiveMode]::Update)
    try {
        $Entry = $Mutant.GetEntry("kin-daemon.exe")
        if ($null -eq $Entry) { throw "The reviewed ZIP has no daemon entry to remove." }
        $Entry.Delete()
    } finally {
        $Mutant.Dispose()
    }
    $Cases = @(
        @{ name = "valid-file-mirror"; archive = $SourceArchive; checksum = "published"; success = $true; diagnostic = "" },
        @{ name = "missing-sidecar"; archive = $SourceArchive; checksum = "missing"; success = $false; diagnostic = "Could not download checksum file" },
        @{ name = "malformed-sidecar"; archive = $SourceArchive; checksum = "malformed"; success = $false; diagnostic = "Checksum file is invalid" },
        @{ name = "wrong-sidecar-hash"; archive = $SourceArchive; checksum = "wrong"; success = $false; diagnostic = "SHA-256 checksum mismatch" },
        @{ name = "missing-daemon"; archive = $MissingDaemonArchive; checksum = "mutated"; success = $false; diagnostic = "kin-daemon.exe missing from the downloaded archive" }
    )
    foreach ($Case in $Cases) {
        $Row = Invoke-InstallCase $Case
        $Result.cases.Add($Row)
    }
    if (@($Result.cases | Where-Object { $_.status -cne "PASS" }).Count -gt 0) {
        throw "At least one actual installer boundary case failed; see case captures."
    }
    if ((Get-Sha256 $InstallerPath) -cne $Result.installer.sha256) {
        throw "Candidate installer bytes changed during the proof."
    }
    $Succeeded = $true
    $Result.status = "PASS"
} catch {
    $Result["error"] = $_.Exception.Message
    Write-Host "FAIL: $($_.Exception.Message)"
} finally {
    $CleanupErrors = [System.Collections.Generic.List[string]]::new()
    foreach ($Name in $SavedEnvironment.Keys) {
        try {
            if ($null -eq $SavedEnvironment[$Name]) {
                Remove-Item -LiteralPath "Env:$Name" -ErrorAction SilentlyContinue
                if (Test-Path -LiteralPath "Env:$Name") {
                    throw "Could not remove originally absent process environment variable $Name."
                }
            } else {
                [Environment]::SetEnvironmentVariable($Name, $SavedEnvironment[$Name], "Process")
                if ([Environment]::GetEnvironmentVariable($Name, "Process") -cne $SavedEnvironment[$Name]) {
                    throw "Could not restore process environment variable $Name."
                }
            }
        } catch {
            $CleanupErrors.Add($_.Exception.Message)
        }
    }
    $Result.environment_restored = $CleanupErrors.Count -eq 0
    try {
        [Environment]::SetEnvironmentVariable("Path", $OriginalUserPath, "User")
        if ([Environment]::GetEnvironmentVariable("Path", "User") -cne $OriginalUserPath) {
            throw "Could not restore the original user PATH."
        }
        $Result.user_path_restored = $true
    } catch {
        $CleanupErrors.Add($_.Exception.Message)
    }
    try {
        Remove-Item -LiteralPath $Scratch -Recurse -Force
        $Result.scratch_removed = $true
        if ($Result.Contains("source_archive")) { $Result.source_archive.retained = $false }
    } catch {
        $CleanupErrors.Add($_.Exception.Message)
    }
    if ($CleanupErrors.Count -gt 0) {
        $Succeeded = $false
        $Result.status = "FAIL"
        $Result["cleanup_error"] = $CleanupErrors -join "; "
    }
    Write-Json (Join-Path $OutputRoot "result.json") $Result
}
if (-not $Succeeded) { exit 1 }
# Expected negative child exits must not leak into the calling Actions shell.
$global:LASTEXITCODE = 0
Write-Host "PASS: five real installer boundary cases for stable v$Version; captures: $OutputRoot"
