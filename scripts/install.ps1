#Requires -Version 7.0

[CmdletBinding()]
param(
    [string] $Version = $(if ($env:LFS_CLOUD_INSTALL_VERSION) { $env:LFS_CLOUD_INSTALL_VERSION } else { 'latest' }),
    [string] $InstallDir = $(if ($env:LFS_CLOUD_INSTALL_DIR) { $env:LFS_CLOUD_INSTALL_DIR } else { Join-Path $HOME '.local' 'bin' }),
    [switch] $Force,
    [switch] $DryRun,
    [Alias('h')] [switch] $Help
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Write-InstallerUsage {
    @'
Usage: lfscloud-installer.ps1 [-Version VERSION] [-InstallDir PATH] [-Force] [-DryRun]

Install or update a directly managed LFS Cloud executable. Environment
variables LFS_CLOUD_INSTALL_VERSION and LFS_CLOUD_INSTALL_DIR provide the same
version and directory controls.
'@ | Write-Host
}

function Get-LfsCloudInstallVersion {
    param(
        [Parameter(Mandatory = $true)] [string] $RequestedVersion,
        [Parameter(Mandatory = $true)] [string] $Repository
    )

    if ($RequestedVersion -eq 'latest') {
        $release = Invoke-RestMethod `
            -Headers @{ Accept = 'application/vnd.github+json' } `
            -Uri "https://api.github.com/repos/$Repository/releases/latest"
        $RequestedVersion = [string] $release.tag_name
        if ($RequestedVersion.StartsWith('v')) {
            $RequestedVersion = $RequestedVersion.Substring(1)
        }
    }

    if ($RequestedVersion -notmatch '^\d+\.\d+\.\d+$') {
        throw "Invalid semantic version: $RequestedVersion"
    }
    return $RequestedVersion
}

function Test-LfsCloudDirectInstallReceipt {
    param([Parameter(Mandatory = $true)] [string] $ReceiptPath)

    if (-not (Test-Path -LiteralPath $ReceiptPath -PathType Leaf)) {
        return $false
    }
    return (Get-Content -Raw -LiteralPath $ReceiptPath).Contains('source=direct')
}

function Invoke-LfsCloudInstaller {
    param(
        [Parameter(Mandatory = $true)] [string] $RequestedVersion,
        [Parameter(Mandatory = $true)] [string] $DestinationDirectory,
        [switch] $ReplaceUnmanaged,
        [switch] $Preview
    )

    if (-not [System.Runtime.InteropServices.RuntimeInformation]::IsOSPlatform(
            [System.Runtime.InteropServices.OSPlatform]::Windows
        ) -or
        [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture -ne
        [System.Runtime.InteropServices.Architecture]::X64) {
        throw 'The PowerShell direct installer currently supports Windows x86-64 only.'
    }

    $repository = if ($env:LFS_CLOUD_GITHUB_REPOSITORY) {
        $env:LFS_CLOUD_GITHUB_REPOSITORY
    }
    else {
        'Quicksaver/lfs-cloud'
    }
    $releaseBase = if ($env:LFS_CLOUD_RELEASE_BASE_URL) {
        $env:LFS_CLOUD_RELEASE_BASE_URL.TrimEnd('/')
    }
    else {
        "https://github.com/$repository/releases"
    }
    $resolvedVersion = Get-LfsCloudInstallVersion `
        -RequestedVersion $RequestedVersion `
        -Repository $repository
    $artifact = "lfscloud-v$resolvedVersion-windows-x86_64.zip"
    $downloadUrl = "$releaseBase/download/v$resolvedVersion/$artifact"
    $target = Join-Path $DestinationDirectory 'lfscloud.exe'
    $receipt = Join-Path $DestinationDirectory '.lfscloud-direct-install'

    Write-Host "LFS Cloud $resolvedVersion for Windows/x86-64"
    Write-Host "Source: $downloadUrl"
    Write-Host "Target: $target"
    if ($Preview) {
        return
    }

    if ((Test-Path -LiteralPath $target) -and
        -not (Test-LfsCloudDirectInstallReceipt -ReceiptPath $receipt) -and
        -not $ReplaceUnmanaged) {
        throw "$target already exists and is not managed by this installer; use its package manager or pass -Force."
    }

    $temporaryDirectory = Join-Path `
        ([System.IO.Path]::GetTempPath()) `
        "lfscloud-install-$([guid]::NewGuid().ToString('N'))"
    [void][System.IO.Directory]::CreateDirectory($temporaryDirectory)
    try {
        $archivePath = Join-Path $temporaryDirectory $artifact
        $checksumPath = "$archivePath.sha256"
        Invoke-WebRequest -Uri $downloadUrl -OutFile $archivePath
        Invoke-WebRequest -Uri "$downloadUrl.sha256" -OutFile $checksumPath

        $digest = (Get-FileHash -LiteralPath $archivePath -Algorithm SHA256).Hash.ToLowerInvariant()
        $expectedLine = (Get-Content -Raw -LiteralPath $checksumPath).Trim()
        if ($expectedLine -ne "$digest  $artifact") {
            throw "SHA-256 verification failed for $artifact."
        }

        [System.IO.Compression.ZipFile]::ExtractToDirectory($archivePath, $temporaryDirectory)
        $sourceBinary = Join-Path `
            $temporaryDirectory `
            "lfscloud-v$resolvedVersion-windows-x86_64" `
            'lfscloud.exe'
        if (-not (Test-Path -LiteralPath $sourceBinary -PathType Leaf)) {
            throw 'Release archive does not contain the expected executable.'
        }
        $reportedVersion = @(& $sourceBinary --version 2>&1)
        if ($LASTEXITCODE -ne 0 -or ($reportedVersion -join "`n").Trim() -ne "lfscloud $resolvedVersion") {
            throw 'Downloaded executable reports an unexpected version.'
        }

        [void][System.IO.Directory]::CreateDirectory($DestinationDirectory)
        $stagedTarget = Join-Path $DestinationDirectory ".lfscloud.install.$PID"
        Copy-Item -LiteralPath $sourceBinary -Destination $stagedTarget -Force
        try {
            [System.IO.File]::Move($stagedTarget, $target, $true)
        }
        finally {
            Remove-Item -LiteralPath $stagedTarget -Force -ErrorAction SilentlyContinue
        }
        [System.IO.File]::WriteAllText(
            $receipt,
            "version=$resolvedVersion`nsource=direct`n",
            [System.Text.UTF8Encoding]::new($false)
        )
        Write-Host "Installed $target"
        if (@($env:PATH -split [System.IO.Path]::PathSeparator) -notcontains $DestinationDirectory) {
            Write-Host "Add $DestinationDirectory to PATH before running lfscloud."
        }
    }
    finally {
        Remove-Item -LiteralPath $temporaryDirectory -Recurse -Force -ErrorAction SilentlyContinue
    }
}

if ($Help) {
    Write-InstallerUsage
    exit 0
}

if ($MyInvocation.InvocationName -ne '.') {
    Invoke-LfsCloudInstaller `
        -RequestedVersion $Version `
        -DestinationDirectory $InstallDir `
        -ReplaceUnmanaged:$Force `
        -Preview:$DryRun
}
