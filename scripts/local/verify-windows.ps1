#Requires -Version 7.0

[CmdletBinding()]
param(
    [Alias('h')]
    [switch] $Help
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$scriptDirectory = Split-Path -Parent $PSCommandPath
. (Join-Path $scriptDirectory '..' 'lib' 'release-common.ps1')

function Write-Usage {
    @'
Usage: pwsh ./scripts/local/verify-windows.ps1

Run the complete deterministic Windows x86-64 verification with the active
system Rust toolchain and post the local-checks/windows-x86_64 status to the
pushed commit.
'@ | Write-Host
}

function Assert-WindowsX64Host {
    $hostIsWindows = [System.Runtime.InteropServices.RuntimeInformation]::IsOSPlatform(
        [System.Runtime.InteropServices.OSPlatform]::Windows
    )
    $architecture = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture
    if (-not $hostIsWindows -or $architecture -ne [System.Runtime.InteropServices.Architecture]::X64) {
        throw 'Local Windows verification requires an x86-64 Windows host.'
    }
}

function New-WindowsReleasePackage {
    param(
        [Parameter(Mandatory = $true)] [string] $RepositoryRoot,
        [Parameter(Mandatory = $true)] [string] $ReleaseBinary,
        [Parameter(Mandatory = $true)] [string] $Version,
        [Parameter(Mandatory = $true)] [string] $Commit,
        [Parameter(Mandatory = $true)] [string] $WindowsVersion,
        [Parameter(Mandatory = $true)] [string] $RustcVersion,
        [Parameter(Mandatory = $true)] [ref] $PackageStage
    )

    $artifact = Get-WindowsArtifactPath -RepositoryRoot $RepositoryRoot -Version $Version
    $manifest = Get-WindowsManifestPath -RepositoryRoot $RepositoryRoot -Version $Version
    $distDirectory = Split-Path -Parent $artifact
    [void][System.IO.Directory]::CreateDirectory($distDirectory)

    foreach ($existingPath in @($artifact, "$artifact.sha256", $manifest)) {
        Remove-Item -LiteralPath $existingPath -Force -ErrorAction SilentlyContinue
    }

    $packageName = "lfscloud-v$Version-windows-x86_64"
    $PackageStage.Value = Join-Path $distDirectory ".package.$([guid]::NewGuid().ToString('N'))"
    $packageRoot = Join-Path $PackageStage.Value $packageName
    $docsDirectory = Join-Path $packageRoot 'docs'
    [void][System.IO.Directory]::CreateDirectory($docsDirectory)

    Copy-Item -LiteralPath $ReleaseBinary -Destination (Join-Path $packageRoot 'lfscloud.exe')
    Copy-Item -LiteralPath (Join-Path $RepositoryRoot 'LICENSE') -Destination $packageRoot
    Copy-Item -LiteralPath (Join-Path $RepositoryRoot 'README.md') -Destination $packageRoot
    Copy-Item `
        -LiteralPath (Join-Path $RepositoryRoot 'docs' 'configuration.md') `
        -Destination $docsDirectory
    Copy-Item `
        -LiteralPath (Join-Path $RepositoryRoot 'docs' 'install-release.md') `
        -Destination $docsDirectory

    Invoke-ReleaseStep `
        -Message 'Create the Windows release archive' `
        -Command 'tar' `
        -Arguments @('-czf', $artifact, '-C', $PackageStage.Value, $packageName)

    Remove-Item -LiteralPath $PackageStage.Value -Recurse -Force
    $PackageStage.Value = ''

    Write-ArtifactChecksum -ArtifactPath $artifact
    if (-not (Test-ArtifactChecksum -ArtifactPath $artifact)) {
        throw 'Release artifact checksum validation failed.'
    }

    Write-WindowsBuildManifest `
        -ArtifactPath $artifact `
        -ManifestPath $manifest `
        -Version $Version `
        -Commit $Commit `
        -WindowsVersion $WindowsVersion `
        -RustcVersion $RustcVersion
    if (-not (Test-WindowsBuildManifest `
                -ArtifactPath $artifact `
                -ManifestPath $manifest `
                -Version $Version `
                -Commit $Commit)) {
        throw 'Windows build manifest does not match the verified commit and artifact.'
    }

    return [pscustomobject] @{
        Artifact = $artifact
        Checksum = "$artifact.sha256"
        Manifest = $manifest
    }
}

function Invoke-WindowsVerification {
    Initialize-ReleaseUi '[verify-windows]' 'Verify Windows x86-64 release'

    $statusStarted = $false
    $checksPassed = $false
    $statusFinalized = $false
    $packageStage = ''
    $exitCode = 0
    $package = $null
    $windowsVersion = [System.Environment]::OSVersion.Version.ToString()

    try {
        # Keep platform rejection ahead of GitHub authentication, origin reads,
        # status writes, dependency installation, and verification commands.
        Assert-WindowsX64Host

        Initialize-Release -StartDirectory $scriptDirectory
        Set-Location -LiteralPath $script:RELEASE_REPO_ROOT

        Assert-ReleaseTrackedClean
        Assert-ReleaseCurrentCommitOnOrigin

        Invoke-ReleaseActionStep 'Record local Windows verification as pending' {
            Set-ReleaseStatus `
                -Commit $script:RELEASE_SHA `
                -Context $script:LOCAL_WINDOWS_STATUS_CONTEXT `
                -State 'pending' `
                -Description "Local Windows $windowsVersion x86-64 checks are running"
        }
        $statusStarted = $true

        foreach ($command in @('cargo', 'node', 'rustc', 'tar', 'yarn')) {
            Assert-ReleaseCommand $command
        }

        $env:CARGO_BUILD_TARGET = 'x86_64-pc-windows-msvc'
        $env:CARGO_TERM_COLOR = 'never'

        Invoke-ReleaseStep 'Install repository tooling' 'yarn' @('install', '--immutable')
        Invoke-ReleaseStep 'Verify Git LFS' 'git' @('lfs', 'version')
        Invoke-ReleaseStep 'Check Rust formatting' 'cargo' @('fmt', '--all', '--', '--check')
        Invoke-ReleaseStep 'Check Rust lints' 'cargo' @('clippy', '--all-targets', '--', '-D', 'warnings')
        Invoke-ReleaseStep `
            'Run automated Rust tests' `
            'cargo' `
            @('test', '--all-targets', '--', '--test-threads=1')
        Invoke-ReleaseStep 'Run Rust documentation tests' 'cargo' @('test', '--doc')
        Invoke-ReleaseStep 'Build the release binary' 'cargo' @('build', '--release')

        $auditVersion = Invoke-NativeCapture 'cargo' @('audit', '--version')
        if ($auditVersion.ExitCode -ne 0 -or
            $auditVersion.Output -notmatch '(^|\s)0\.22\.2($|\s)') {
            Invoke-ReleaseStep `
                'Install cargo-audit 0.22.2' `
                'cargo' `
                @('install', 'cargo-audit', '--locked', '--version', '0.22.2')
        }

        Invoke-ReleaseStep 'Audit locked Rust dependencies' 'cargo' @('audit')
        Invoke-ReleaseStep 'Check repository formatting' 'yarn' @('lint:check')

        $releaseBinary = Join-Path `
            $script:RELEASE_REPO_ROOT `
            'target' `
            $env:CARGO_BUILD_TARGET `
            'release' `
            'lfscloud.exe'
        $env:LFS_CLOUD_SMOKE_BINARY = $releaseBinary
        $env:LFS_CLOUD_SMOKE_SKIP_CARGO_TESTS = '1'
        Invoke-ReleaseStep `
            'Run smoke tests against the exact release binary' `
            'node' `
            @(
                '--no-warnings',
                '--experimental-strip-types',
                '.agents/skills/smoke-test/scripts/smoke-test.ts'
            )

        $version = Get-MatchingReleaseVersion -RepositoryRoot $script:RELEASE_REPO_ROOT
        $binaryVersion = Invoke-NativeCapture $releaseBinary @('--version')
        if ($binaryVersion.ExitCode -ne 0 -or $binaryVersion.Output -ne "lfscloud $version") {
            throw "Release binary version does not match package version $version."
        }

        $rustcVersion = Invoke-NativeCapture 'rustc' @('--version')
        if ($rustcVersion.ExitCode -ne 0 -or [string]::IsNullOrWhiteSpace($rustcVersion.Output)) {
            throw 'Could not resolve the active Rust compiler version.'
        }

        $package = Invoke-ReleaseActionStep 'Package the verified Windows release binary' {
            New-WindowsReleasePackage `
                -RepositoryRoot $script:RELEASE_REPO_ROOT `
                -ReleaseBinary $releaseBinary `
                -Version $version `
                -Commit $script:RELEASE_SHA `
                -WindowsVersion $windowsVersion `
                -RustcVersion $rustcVersion.Output `
                -PackageStage ([ref] $packageStage)
        }

        $checksPassed = $true
        Invoke-ReleaseActionStep 'Record local Windows verification as successful' {
            Set-ReleaseStatus `
                -Commit $script:RELEASE_SHA `
                -Context $script:LOCAL_WINDOWS_STATUS_CONTEXT `
                -State 'success' `
                -Description "Local Windows $windowsVersion x86-64 checks passed"
        }
        $statusFinalized = $true

        Write-ReleasePass "Local Windows verification passed for $($script:RELEASE_SHA)"
        Write-ReleaseInfo "Artifact: $($package.Artifact)"
        Write-ReleaseInfo "Checksum: $($package.Checksum)"
        Write-ReleaseInfo "Build manifest: $($package.Manifest)"
    }
    catch {
        $exitCode = 1
        fail $_.Exception.Message
    }
    finally {
        if (-not [string]::IsNullOrWhiteSpace($packageStage) -and
            (Test-Path -LiteralPath $packageStage -PathType Container)) {
            Remove-Item -LiteralPath $packageStage -Recurse -Force -ErrorAction SilentlyContinue
        }

        if ($statusStarted -and -not $checksPassed -and -not $statusFinalized) {
            try {
                Set-ReleaseStatus `
                    -Commit $script:RELEASE_SHA `
                    -Context $script:LOCAL_WINDOWS_STATUS_CONTEXT `
                    -State 'failure' `
                    -Description "Local Windows $windowsVersion x86-64 checks failed"
            }
            catch {
                Write-ReleaseWarning 'Failed to record the local Windows failure status'
            }
        }

        Complete-ReleaseUi
    }

    return $exitCode
}

if ($MyInvocation.InvocationName -ne '.') {
    if ($Help) {
        Write-Usage
        exit 0
    }

    exit (Invoke-WindowsVerification)
}
