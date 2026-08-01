#Requires -Version 7.0

[CmdletBinding()]
param(
    [Alias('h', 'Help')] [switch] $ShowHelp
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$publisherScriptDirectory = Split-Path -Parent $PSCommandPath
. (Join-Path $publisherScriptDirectory 'lib' 'release-common.ps1')

$script:DISTRIBUTION_DIRECT_CONTEXT = 'distribution/direct-installer'
$script:DISTRIBUTION_HOMEBREW_CONTEXT = 'distribution/homebrew'
$script:DISTRIBUTION_APT_CONTEXT = 'distribution/apt'
$script:DISTRIBUTION_WINGET_CONTEXT = 'distribution/winget-submitted'
$script:REQUIRED_RELEASE_CONTEXTS = @(
    $script:LOCAL_MACOS_STATUS_CONTEXT,
    $script:LOCAL_LINUX_X86_64_STATUS_CONTEXT,
    $script:LOCAL_LINUX_ARM64_STATUS_CONTEXT,
    $script:LOCAL_WINDOWS_STATUS_CONTEXT
)
$script:DISTRIBUTION_CONTEXTS = @(
    $script:DISTRIBUTION_DIRECT_CONTEXT,
    $script:DISTRIBUTION_HOMEBREW_CONTEXT,
    $script:DISTRIBUTION_APT_CONTEXT,
    $script:DISTRIBUTION_WINGET_CONTEXT
)

function Write-PublishUsage {
    @'
Usage: pwsh ./scripts/publish.ps1

List semantic draft releases whose macOS, Linux, and Windows local checks are
green, prompt for one version, verify every remote release asset, enable GitHub
release immutability, publish the draft, and distribute that exact release to:

  - the direct shell and PowerShell installer URLs
  - Quicksaver/homebrew-tap (configurable)
  - a configured Cloudsmith Debian repository
  - the WinGet Community repository through a manifest pull request

Published immutable releases with incomplete distribution statuses are also
listed so interrupted publication can be resumed safely.

Required while the APT channel is incomplete:
  LFS_CLOUD_APT_CLOUDSMITH_TARGET=OWNER/REPOSITORY/DISTRO/VERSION

Optional environment:
  LFS_CLOUD_HOMEBREW_TAP_REPO=OWNER/homebrew-TAP
'@ | Write-Host
}

function Get-PublishReleaseDocument {
    param([Parameter(Mandatory = $true)] [string] $Tag)

    $result = Invoke-NativeCapture 'gh' @(
        'release',
        'view',
        $Tag,
        '--repo',
        $script:RELEASE_GITHUB_REPO,
        '--json',
        'assets,isDraft,isImmutable,isPrerelease,publishedAt,tagName,url'
    )
    if ($result.ExitCode -ne 0) {
        throw "Could not read GitHub release $Tag."
    }
    try {
        return $result.Output | ConvertFrom-Json
    }
    catch {
        throw "GitHub returned invalid release JSON for $Tag."
    }
}

function Get-CommitStatusDocument {
    param([Parameter(Mandatory = $true)] [string] $Commit)

    $result = Invoke-NativeCapture 'gh' @(
        'api',
        "repos/$($script:RELEASE_GITHUB_REPO)/commits/$Commit/status?per_page=100"
    )
    if ($result.ExitCode -ne 0) {
        throw "Could not read commit statuses for $Commit."
    }
    try {
        return $result.Output | ConvertFrom-Json
    }
    catch {
        throw "GitHub returned invalid commit-status JSON for $Commit."
    }
}

function Get-LatestStatusRecordFromDocument {
    param(
        [Parameter(Mandatory = $true)] $Document,
        [Parameter(Mandatory = $true)] [string] $Context
    )

    $records = @(
        $Document.statuses |
            Where-Object { $_.context -eq $Context } |
            Sort-Object -Property id -Descending
    )
    if ($records.Count -eq 0) {
        return $null
    }
    return $records[0]
}

function Get-TrustedStatusState {
    param(
        [Parameter(Mandatory = $true)] $Document,
        [Parameter(Mandatory = $true)] [string] $Context,
        [Parameter(Mandatory = $true)] [string] $TrustedLogin
    )

    $record = Get-LatestStatusRecordFromDocument -Document $Document -Context $Context
    if ($null -eq $record) {
        return 'missing'
    }
    if ([string] $record.creator.login -ne $TrustedLogin) {
        return 'untrusted'
    }
    return [string] $record.state
}

function Get-PublishReleaseCandidates {
    $result = Invoke-NativeCapture 'gh' @(
        'release',
        'list',
        '--repo',
        $script:RELEASE_GITHUB_REPO,
        '--limit',
        '1000',
        '--json',
        'tagName,isDraft,isImmutable,isPrerelease,publishedAt'
    )
    if ($result.ExitCode -ne 0) {
        throw 'Could not list GitHub releases.'
    }
    try {
        $releases = @($result.Output | ConvertFrom-Json)
    }
    catch {
        throw 'GitHub returned invalid release-list JSON.'
    }

    $candidates = [System.Collections.Generic.List[object]]::new()
    foreach ($release in $releases) {
        $tag = [string] $release.tagName
        if ([bool] $release.isPrerelease -or $tag -notmatch '^v(\d+\.\d+\.\d+)$') {
            continue
        }
        $versionText = $Matches[1]
        $commit = Get-RemoteReleaseTagCommit -Tag $tag
        if ([string]::IsNullOrWhiteSpace($commit)) {
            throw "Release $tag does not have a matching tag on origin."
        }
        $statuses = Get-CommitStatusDocument -Commit $commit
        $requiredStates = [ordered] @{}
        $requiredAreGreen = $true
        foreach ($context in $script:REQUIRED_RELEASE_CONTEXTS) {
            $state = Get-TrustedStatusState `
                -Document $statuses `
                -Context $context `
                -TrustedLogin $script:RELEASE_GITHUB_LOGIN
            $requiredStates[$context] = $state
            if ($state -ne 'success') {
                $requiredAreGreen = $false
            }
        }
        if (-not $requiredAreGreen) {
            continue
        }

        $distributionStates = [ordered] @{}
        $distributionComplete = $true
        foreach ($context in $script:DISTRIBUTION_CONTEXTS) {
            $state = Get-TrustedStatusState `
                -Document $statuses `
                -Context $context `
                -TrustedLogin $script:RELEASE_GITHUB_LOGIN
            $distributionStates[$context] = $state
            if ($state -ne 'success') {
                $distributionComplete = $false
            }
        }

        $isDraft = [bool] $release.isDraft
        $isImmutable = [bool] $release.isImmutable
        if (-not $isDraft -and (-not $isImmutable -or $distributionComplete)) {
            continue
        }

        [void] $candidates.Add([pscustomobject] @{
                Version = [version] $versionText
                VersionText = $versionText
                Tag = $tag
                Commit = $commit
                IsDraft = $isDraft
                IsImmutable = $isImmutable
                DistributionStates = $distributionStates
            })
    }

    return @(
        $candidates |
            Sort-Object -Property @{ Expression = { $_.Version }; Descending = $true }
    )
}

function Format-PublishCandidate {
    param([Parameter(Mandatory = $true)] $Candidate)

    $stage = if ($Candidate.IsDraft) { 'draft' } else { 'resume' }
    return '{0,-12} {1,-6} direct:{2} brew:{3} apt:{4} winget:{5}' -f `
        $Candidate.Tag,
        $stage,
        $Candidate.DistributionStates[$script:DISTRIBUTION_DIRECT_CONTEXT],
        $Candidate.DistributionStates[$script:DISTRIBUTION_HOMEBREW_CONTEXT],
        $Candidate.DistributionStates[$script:DISTRIBUTION_APT_CONTEXT],
        $Candidate.DistributionStates[$script:DISTRIBUTION_WINGET_CONTEXT]
}

function Read-PublishReleaseSelection {
    param(
        [Parameter(Mandatory = $true)] [object[]] $Candidates,
        [scriptblock] $ReadKey,
        [scriptblock] $Render
    )

    if ($Candidates.Count -eq 0) {
        throw 'At least one publishable release candidate is required.'
    }
    if ($null -eq $ReadKey) {
        if ([Console]::IsInputRedirected -or [Console]::IsOutputRedirected) {
            throw 'Release publication requires an interactive terminal.'
        }
        $ReadKey = { return [Console]::ReadKey($true) }
    }
    if ($null -eq $Render) {
        $Render = {
            param($Items, $SelectedIndex, $IsRedraw)
            if ($IsRedraw) {
                [Console]::SetCursorPosition(0, [Math]::Max(0, [Console]::CursorTop - $Items.Count))
            }
            for ($index = 0; $index -lt $Items.Count; $index++) {
                $marker = if ($index -eq $SelectedIndex) { '>' } else { ' ' }
                [Console]::WriteLine("$marker $(Format-PublishCandidate -Candidate $Items[$index])")
            }
        }
    }

    Write-Host ''
    Write-Host 'Verified releases ready to publish or resume:'
    Write-Host 'Use Up/Down to navigate, Enter to select, or Escape to cancel.'
    $selectedIndex = 0
    & $Render $Candidates $selectedIndex $false
    while ($true) {
        $keyInfo = & $ReadKey
        $key = if ($keyInfo -is [System.ConsoleKeyInfo]) {
            $keyInfo.Key
        }
        else {
            [System.ConsoleKey] $keyInfo
        }
        switch ($key) {
            UpArrow {
                $selectedIndex = ($selectedIndex - 1 + $Candidates.Count) % $Candidates.Count
                & $Render $Candidates $selectedIndex $true
            }
            DownArrow {
                $selectedIndex = ($selectedIndex + 1) % $Candidates.Count
                & $Render $Candidates $selectedIndex $true
            }
            Enter {
                Write-Host ''
                return $Candidates[$selectedIndex]
            }
            Escape {
                Write-Host ''
                return $null
            }
        }
    }
}

function Get-ExpectedReleaseAssetNames {
    param([Parameter(Mandatory = $true)] [string] $Version)

    $artifacts = @(
        "lfscloud-v$Version-macos-arm64.tar.gz",
        "lfscloud-v$Version-linux-x86_64-musl.tar.gz",
        "lfscloud-v$Version-linux-arm64-musl.tar.gz",
        "lfscloud_$($Version)_amd64.deb",
        "lfscloud_$($Version)_arm64.deb",
        "lfscloud-v$Version-windows-x86_64.zip"
    )
    $names = [System.Collections.Generic.List[string]]::new()
    foreach ($artifact in $artifacts) {
        [void] $names.Add($artifact)
        [void] $names.Add("$artifact.sha256")
        [void] $names.Add(($artifact -replace '\.(tar\.gz|zip|deb)$', '.build.json'))
    }
    foreach ($installer in @('lfscloud-installer.sh', 'lfscloud-installer.ps1')) {
        [void] $names.Add($installer)
        [void] $names.Add("$installer.sha256")
    }
    return @($names)
}

function Test-GenericBuildManifest {
    param(
        [Parameter(Mandatory = $true)] [string] $ArtifactPath,
        [Parameter(Mandatory = $true)] [string] $ManifestPath,
        [Parameter(Mandatory = $true)] [string] $Version,
        [Parameter(Mandatory = $true)] [string] $Commit,
        [Parameter(Mandatory = $true)] [hashtable] $ExpectedProperties
    )

    if (-not (Test-Path -LiteralPath $ArtifactPath -PathType Leaf) -or
        -not (Test-Path -LiteralPath $ManifestPath -PathType Leaf)) {
        return $false
    }
    try {
        $manifest = Get-Content -Raw -LiteralPath $ManifestPath | ConvertFrom-Json
    }
    catch {
        return $false
    }
    $digest = (Get-FileHash -LiteralPath $ArtifactPath -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($manifest.schema_version -ne 1 -or
        [string] $manifest.artifact -ne [System.IO.Path]::GetFileName($ArtifactPath) -or
        [string] $manifest.commit -ne $Commit -or
        [string] $manifest.version -ne $Version -or
        [string] $manifest.sha256 -ne $digest) {
        return $false
    }
    foreach ($property in $ExpectedProperties.Keys) {
        if ($manifest.PSObject.Properties.Name -notcontains $property -or
            [string] $manifest.$property -ne [string] $ExpectedProperties[$property]) {
            return $false
        }
    }
    return $true
}

function Assert-DownloadedReleaseAssets {
    param(
        [Parameter(Mandatory = $true)] $Candidate,
        [Parameter(Mandatory = $true)] $Release,
        [Parameter(Mandatory = $true)] [string] $Directory
    )

    $expectedNames = @(Get-ExpectedReleaseAssetNames -Version $Candidate.VersionText)
    foreach ($name in $expectedNames) {
        $path = Join-Path $Directory $name
        if (-not (Test-Path -LiteralPath $path -PathType Leaf) -or (Get-Item $path).Length -eq 0) {
            throw "Downloaded release is missing required asset $name."
        }
        $remote = @($Release.assets | Where-Object { $_.name -eq $name })
        if ($remote.Count -ne 1) {
            throw "GitHub release does not contain exactly one asset named $name."
        }
        $digest = (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant()
        if ([string] $remote[0].digest -ne "sha256:$digest" -or
            [long] $remote[0].size -ne (Get-Item $path).Length) {
            throw "GitHub release asset $name does not match the downloaded bytes."
        }
    }

    $version = $Candidate.VersionText
    $commit = $Candidate.Commit
    $artifactExpectations = @(
        @("lfscloud-v$version-macos-arm64.tar.gz", @{ target = 'aarch64-apple-darwin' }),
        @("lfscloud-v$version-linux-x86_64-musl.tar.gz", @{ target = 'x86_64-unknown-linux-musl'; container_arch = 'x86_64' }),
        @("lfscloud-v$version-linux-arm64-musl.tar.gz", @{ target = 'aarch64-unknown-linux-musl'; container_arch = 'aarch64' }),
        @("lfscloud_$($version)_amd64.deb", @{ target = 'x86_64-unknown-linux-musl'; architecture = 'amd64'; package_format = 'deb' }),
        @("lfscloud_$($version)_arm64.deb", @{ target = 'aarch64-unknown-linux-musl'; architecture = 'arm64'; package_format = 'deb' })
    )
    foreach ($expectation in $artifactExpectations) {
        $artifactName = [string] $expectation[0]
        $artifactPath = Join-Path $Directory $artifactName
        if (-not (Test-ArtifactChecksum -ArtifactPath $artifactPath)) {
            throw "Release checksum is invalid for $artifactName."
        }
        $manifestPath = Join-Path $Directory ($artifactName -replace '\.(tar\.gz|deb)$', '.build.json')
        if (-not (Test-GenericBuildManifest `
                    -ArtifactPath $artifactPath `
                    -ManifestPath $manifestPath `
                    -Version $version `
                    -Commit $commit `
                    -ExpectedProperties $expectation[1])) {
            throw "Build manifest is invalid for $artifactName."
        }
    }

    $windowsArtifact = Join-Path $Directory "lfscloud-v$version-windows-x86_64.zip"
    if (-not (Test-ArtifactChecksum -ArtifactPath $windowsArtifact) -or
        -not (Test-WindowsBuildManifest `
                -ArtifactPath $windowsArtifact `
                -ManifestPath (Join-Path $Directory "lfscloud-v$version-windows-x86_64.build.json") `
                -Version $version `
                -Commit $commit)) {
        throw 'Windows release checksum or build manifest is invalid.'
    }
    foreach ($installer in @('lfscloud-installer.sh', 'lfscloud-installer.ps1')) {
        if (-not (Test-ArtifactChecksum -ArtifactPath (Join-Path $Directory $installer))) {
            throw "Direct installer checksum is invalid for $installer."
        }
    }
}

function New-HomebrewFormulaText {
    param(
        [Parameter(Mandatory = $true)] [string] $Version,
        [Parameter(Mandatory = $true)] [string] $MacSha256,
        [Parameter(Mandatory = $true)] [string] $LinuxX64Sha256,
        [Parameter(Mandatory = $true)] [string] $LinuxArm64Sha256
    )

    $template = @'
class Lfscloud < Formula
  desc "Git LFS-compatible server and CLI for user-controlled storage"
  homepage "https://github.com/Quicksaver/lfs-cloud"
  version "@VERSION@"
  license "MIT"

  if OS.mac?
    url "https://github.com/Quicksaver/lfs-cloud/releases/download/v@VERSION@/lfscloud-v@VERSION@-macos-arm64.tar.gz"
    sha256 "@MAC_SHA@"
    depends_on arch: :arm64
  elsif Hardware::CPU.arm?
    url "https://github.com/Quicksaver/lfs-cloud/releases/download/v@VERSION@/lfscloud-v@VERSION@-linux-arm64-musl.tar.gz"
    sha256 "@LINUX_ARM_SHA@"
  else
    url "https://github.com/Quicksaver/lfs-cloud/releases/download/v@VERSION@/lfscloud-v@VERSION@-linux-x86_64-musl.tar.gz"
    sha256 "@LINUX_X64_SHA@"
  end

  def install
    bin.install "lfscloud"
  end

  test do
    assert_equal "lfscloud #{version}", shell_output("#{bin}/lfscloud --version").strip
  end
end
'@
    $formula = $template.Replace('@VERSION@', $Version).
        Replace('@MAC_SHA@', $MacSha256).
        Replace('@LINUX_X64_SHA@', $LinuxX64Sha256).
        Replace('@LINUX_ARM_SHA@', $LinuxArm64Sha256)
    return "$($formula.TrimEnd())`n"
}

function New-WinGetManifests {
    param(
        [Parameter(Mandatory = $true)] [string] $Version,
        [Parameter(Mandatory = $true)] [string] $InstallerSha256,
        [Parameter(Mandatory = $true)] [string] $Directory
    )

    [void][System.IO.Directory]::CreateDirectory($Directory)
    $versionManifest = @"
PackageIdentifier: Quicksaver.LFSCloud
PackageVersion: $Version
DefaultLocale: en-US
ManifestType: version
ManifestVersion: 1.12.0
"@
    $installerManifest = @"
PackageIdentifier: Quicksaver.LFSCloud
PackageVersion: $Version
InstallerType: zip
NestedInstallerType: portable
Commands:
- lfscloud
ReleaseDate: $([DateTime]::UtcNow.ToString('yyyy-MM-dd'))
Installers:
- Architecture: x64
  NestedInstallerFiles:
  - RelativeFilePath: lfscloud-v$Version-windows-x86_64/lfscloud.exe
    PortableCommandAlias: lfscloud
  InstallerUrl: https://github.com/Quicksaver/lfs-cloud/releases/download/v$Version/lfscloud-v$Version-windows-x86_64.zip
  InstallerSha256: $($InstallerSha256.ToUpperInvariant())
ManifestType: installer
ManifestVersion: 1.12.0
"@
    $localeManifest = @"
PackageIdentifier: Quicksaver.LFSCloud
PackageVersion: $Version
PackageLocale: en-US
Publisher: Quicksaver
PublisherUrl: https://github.com/Quicksaver
PackageName: LFS Cloud
PackageUrl: https://github.com/Quicksaver/lfs-cloud
License: MIT
LicenseUrl: https://github.com/Quicksaver/lfs-cloud/blob/v$Version/LICENSE
ShortDescription: Git LFS-compatible server and CLI for user-controlled storage.
Tags:
- git
- git-lfs
- lfs
ReleaseNotesUrl: https://github.com/Quicksaver/lfs-cloud/releases/tag/v$Version
ManifestType: defaultLocale
ManifestVersion: 1.12.0
"@
    [System.IO.File]::WriteAllText(
        (Join-Path $Directory 'Quicksaver.LFSCloud.yaml'),
        $versionManifest,
        [System.Text.UTF8Encoding]::new($false)
    )
    [System.IO.File]::WriteAllText(
        (Join-Path $Directory 'Quicksaver.LFSCloud.installer.yaml'),
        $installerManifest,
        [System.Text.UTF8Encoding]::new($false)
    )
    [System.IO.File]::WriteAllText(
        (Join-Path $Directory 'Quicksaver.LFSCloud.locale.en-US.yaml'),
        $localeManifest,
        [System.Text.UTF8Encoding]::new($false)
    )
}

function Invoke-DistributionAction {
    param(
        [Parameter(Mandatory = $true)] $Candidate,
        [Parameter(Mandatory = $true)] [string] $Context,
        [Parameter(Mandatory = $true)] [string] $Description,
        [Parameter(Mandatory = $true)] [scriptblock] $Action
    )

    if ($Candidate.DistributionStates[$Context] -eq 'success') {
        Write-ReleasePass "$Context is already successful for $($Candidate.Tag)"
        return $true
    }
    Set-ReleaseStatus -Commit $Candidate.Commit -Context $Context -State pending -Description "$Description is running"
    try {
        & $Action
        Set-ReleaseStatus -Commit $Candidate.Commit -Context $Context -State success -Description "$Description succeeded"
        Write-ReleasePass "$Description succeeded"
        return $true
    }
    catch {
        try {
            Set-ReleaseStatus -Commit $Candidate.Commit -Context $Context -State failure -Description "$Description failed"
        }
        catch {
            Write-ReleaseWarning "Could not record failure status for $Context."
        }
        Write-ReleaseWarning "$Description failed: $($_.Exception.Message)"
        return $false
    }
}

function Publish-HomebrewFormula {
    param(
        [Parameter(Mandatory = $true)] [string] $FormulaPath,
        [Parameter(Mandatory = $true)] [string] $Tag
    )

    Assert-ReleaseCommand 'brew'
    $tapRepository = if ($env:LFS_CLOUD_HOMEBREW_TAP_REPO) {
        $env:LFS_CLOUD_HOMEBREW_TAP_REPO
    }
    else {
        'Quicksaver/homebrew-tap'
    }
    if ($tapRepository -notmatch '^([^/]+)/homebrew-([^/]+)$') {
        throw 'The Homebrew tap repository must use OWNER/homebrew-TAP form.'
    }
    $tapName = "$($Matches[1])/$($Matches[2])"
    Invoke-ReleaseStep 'Register the Homebrew tap' 'brew' @('tap', $tapName)
    $tapPathResult = Invoke-NativeCapture 'brew' @('--repository', $tapName)
    if ($tapPathResult.ExitCode -ne 0 -or [string]::IsNullOrWhiteSpace($tapPathResult.Output)) {
        throw "Could not resolve the local checkout for Homebrew tap $tapName."
    }
    $tapPath = $tapPathResult.Output
    $initialStatus = Invoke-NativeCapture 'git' @('-C', $tapPath, 'status', '--porcelain=v1')
    if ($initialStatus.ExitCode -ne 0 -or -not [string]::IsNullOrWhiteSpace($initialStatus.Output)) {
        throw "Homebrew tap $tapName must have a clean local checkout before publication."
    }
    Invoke-ReleaseStep 'Update the Homebrew tap checkout' 'git' @('-C', $tapPath, 'pull', '--ff-only')
    $formulaDirectory = Join-Path $tapPath 'Formula'
    [void][System.IO.Directory]::CreateDirectory($formulaDirectory)
    $tapFormulaPath = Join-Path $formulaDirectory 'lfscloud.rb'
    Copy-Item -LiteralPath $FormulaPath -Destination $tapFormulaPath -Force
    Invoke-ReleaseStep 'Check the Homebrew formula style' 'brew' @('style', '--formula', $tapFormulaPath)
    Invoke-ReleaseStep 'Fetch the Homebrew release archive' 'brew' @('fetch', '--force', '--formula', $tapFormulaPath)
    $status = Invoke-NativeCapture 'git' @('-C', $tapPath, 'status', '--porcelain=v1')
    if ($status.ExitCode -ne 0) {
        throw 'Could not inspect the Homebrew tap checkout.'
    }
    if ([string]::IsNullOrWhiteSpace($status.Output)) {
        return
    }
    foreach ($arguments in @(
            @('-C', $tapPath, 'config', 'user.name', 'LFS Cloud Publisher'),
            @('-C', $tapPath, 'config', 'user.email', 'support@quicksaver.dev'),
            @('-C', $tapPath, 'add', 'Formula/lfscloud.rb'),
            @('-C', $tapPath, 'commit', '--message', "Publish LFS Cloud $Tag"),
            @('-C', $tapPath, 'push', 'origin', 'HEAD')
        )) {
        $result = Invoke-NativeCapture 'git' $arguments
        if ($result.ExitCode -ne 0) {
            throw "Homebrew tap command failed: git $($arguments -join ' ')`n$($result.Output)"
        }
    }
}

function Publish-DebianPackages {
    param(
        [Parameter(Mandatory = $true)] [string] $AssetDirectory,
        [Parameter(Mandatory = $true)] [string] $Version
    )

    Assert-ReleaseCommand 'cloudsmith'
    if ([string]::IsNullOrWhiteSpace($env:LFS_CLOUD_APT_CLOUDSMITH_TARGET)) {
        throw 'LFS_CLOUD_APT_CLOUDSMITH_TARGET must identify OWNER/REPOSITORY/DISTRO/VERSION.'
    }
    foreach ($architecture in @('amd64', 'arm64')) {
        $package = Join-Path $AssetDirectory "lfscloud_$($Version)_$architecture.deb"
        Invoke-ReleaseStep `
            "Publish the Debian $architecture package" `
            'cloudsmith' `
            @('push', 'deb', $env:LFS_CLOUD_APT_CLOUDSMITH_TARGET, $package, '--republish')
    }
}

function Publish-WinGetManifests {
    param(
        [Parameter(Mandatory = $true)] [string] $ManifestDirectory,
        [Parameter(Mandatory = $true)] [string] $Version,
        [Parameter(Mandatory = $true)] [string] $TemporaryRoot
    )

    $branch = "lfscloud-$Version"
    $existing = Invoke-NativeCapture 'gh' @(
        'pr', 'list', '--repo', 'microsoft/winget-pkgs', '--state', 'all',
        '--head', "$($script:RELEASE_GITHUB_LOGIN):$branch", '--json', 'url', '--jq', '.[0].url // empty'
    )
    if ($existing.ExitCode -eq 0 -and -not [string]::IsNullOrWhiteSpace($existing.Output)) {
        Write-ReleaseInfo "WinGet pull request already exists: $($existing.Output)"
        return
    }

    $fork = "$($script:RELEASE_GITHUB_LOGIN)/winget-pkgs"
    $forkView = Invoke-NativeCapture 'gh' @('repo', 'view', $fork, '--json', 'name')
    if ($forkView.ExitCode -ne 0) {
        Invoke-ReleaseStep 'Create the WinGet repository fork' 'gh' @(
            'repo', 'fork', 'microsoft/winget-pkgs', '--clone=false', '--default-branch-only'
        )
    }
    $defaultBranch = Invoke-NativeCapture 'gh' @(
        'repo', 'view', 'microsoft/winget-pkgs', '--json', 'defaultBranchRef', '--jq', '.defaultBranchRef.name'
    )
    if ($defaultBranch.ExitCode -ne 0 -or [string]::IsNullOrWhiteSpace($defaultBranch.Output)) {
        throw 'Could not resolve the WinGet repository default branch.'
    }

    $checkout = Join-Path $TemporaryRoot 'winget-pkgs'
    Invoke-ReleaseStep 'Clone the WinGet fork metadata' 'gh' @(
        'repo', 'clone', $fork, $checkout, '--', '--filter=blob:none', '--no-checkout'
    )
    $manifestRelativePath = "manifests/q/Quicksaver/LFSCloud/$Version"
    foreach ($arguments in @(
            @('-C', $checkout, 'remote', 'add', 'upstream', 'https://github.com/microsoft/winget-pkgs.git'),
            @('-C', $checkout, 'sparse-checkout', 'init', '--no-cone'),
            @('-C', $checkout, 'sparse-checkout', 'set', $manifestRelativePath),
            @('-C', $checkout, 'fetch', '--depth=1', 'upstream', $defaultBranch.Output),
            @('-C', $checkout, 'switch', '--create', $branch, "upstream/$($defaultBranch.Output)")
        )) {
        $command = Invoke-NativeCapture 'git' $arguments
        if ($command.ExitCode -ne 0) {
            throw "WinGet checkout command failed: git $($arguments -join ' ')`n$($command.Output)"
        }
    }
    $targetDirectory = Join-Path $checkout $manifestRelativePath
    [void][System.IO.Directory]::CreateDirectory($targetDirectory)
    Copy-Item -Path (Join-Path $ManifestDirectory '*.yaml') -Destination $targetDirectory -Force
    foreach ($arguments in @(
            @('-C', $checkout, 'config', 'user.name', 'LFS Cloud Publisher'),
            @('-C', $checkout, 'config', 'user.email', 'support@quicksaver.dev'),
            @('-C', $checkout, 'add', $manifestRelativePath),
            @('-C', $checkout, 'commit', '--message', "New version: Quicksaver.LFSCloud version $Version")
        )) {
        $command = Invoke-NativeCapture 'git' $arguments
        if ($command.ExitCode -ne 0) {
            throw "WinGet publication command failed: git $($arguments -join ' ')`n$($command.Output)"
        }
    }
    $remoteBranch = Invoke-NativeCapture 'git' @(
        'ls-remote', "https://github.com/$fork.git", "refs/heads/$branch"
    )
    if ($remoteBranch.ExitCode -ne 0) {
        throw 'Could not inspect the WinGet publication branch.'
    }
    $remoteBranchCommit = @($remoteBranch.Output -split "`n")[0] -replace '\s.*$', ''
    $pushArguments = @('-C', $checkout, 'push', '--set-upstream')
    if ($remoteBranchCommit -match '^[0-9a-f]{40}$') {
        $pushArguments += "--force-with-lease=refs/heads/$branch`:$remoteBranchCommit"
    }
    $pushArguments += @('origin', "HEAD:refs/heads/$branch")
    $push = Invoke-NativeCapture 'git' $pushArguments
    if ($push.ExitCode -ne 0) {
        throw "WinGet branch publication failed.`n$($push.Output)"
    }
    $pr = Invoke-NativeCapture 'gh' @(
        'pr', 'create', '--repo', 'microsoft/winget-pkgs', '--base', $defaultBranch.Output,
        '--head', "$($script:RELEASE_GITHUB_LOGIN):$branch",
        '--title', "New version: Quicksaver.LFSCloud version $Version",
        '--body', "Automated local submission for Quicksaver.LFSCloud $Version."
    )
    if ($pr.ExitCode -ne 0 -or [string]::IsNullOrWhiteSpace($pr.Output)) {
        throw "Could not create the WinGet manifest pull request.`n$($pr.Output)"
    }
    Write-ReleaseInfo "WinGet pull request: $($pr.Output)"
}

function Publish-DirectInstallers {
    param(
        [Parameter(Mandatory = $true)] [string] $AssetDirectory,
        [Parameter(Mandatory = $true)] [string] $Tag
    )

    foreach ($installer in @('lfscloud-installer.sh', 'lfscloud-installer.ps1')) {
        $destination = Join-Path $AssetDirectory "public-$installer"
        $url = "https://github.com/$($script:RELEASE_GITHUB_REPO)/releases/download/$Tag/$installer"
        Invoke-WebRequest -Uri $url -OutFile $destination
        $publishedDigest = (Get-FileHash -LiteralPath $destination -Algorithm SHA256).Hash.ToLowerInvariant()
        $localDigest = (Get-FileHash -LiteralPath (Join-Path $AssetDirectory $installer) -Algorithm SHA256).Hash.ToLowerInvariant()
        if ($publishedDigest -ne $localDigest) {
            throw "Published direct installer $installer does not match the verified draft asset."
        }
    }
}

function Invoke-ReleasePublisher {
    $temporaryRoot = ''
    $exitCode = 0
    try {
        Initialize-Release -StartDirectory $publisherScriptDirectory
        Set-Location -LiteralPath $script:RELEASE_REPO_ROOT
        Assert-ReleaseFullyClean
        foreach ($command in @('git', 'gh')) {
            Assert-ReleaseCommand $command
        }

        $candidates = @(Get-PublishReleaseCandidates)
        if ($candidates.Count -eq 0) {
            Write-ReleasePass 'No fully verified draft or incomplete immutable release is ready to publish.'
            return 0
        }
        $candidate = Read-PublishReleaseSelection -Candidates $candidates
        if ($null -eq $candidate) {
            Write-ReleaseInfo 'Release publication cancelled.'
            return 0
        }

        if ($candidate.DistributionStates[$script:DISTRIBUTION_HOMEBREW_CONTEXT] -ne 'success') {
            Assert-ReleaseCommand 'brew'
            $tapRepository = if ($env:LFS_CLOUD_HOMEBREW_TAP_REPO) {
                $env:LFS_CLOUD_HOMEBREW_TAP_REPO
            }
            else {
                'Quicksaver/homebrew-tap'
            }
            $tapCheck = Invoke-NativeCapture 'gh' @('repo', 'view', $tapRepository, '--json', 'nameWithOwner')
            if ($tapCheck.ExitCode -ne 0) {
                throw "Homebrew tap repository is unavailable: $tapRepository"
            }
        }
        if ($candidate.DistributionStates[$script:DISTRIBUTION_APT_CONTEXT] -ne 'success') {
            Assert-ReleaseCommand 'cloudsmith'
            if ([string]::IsNullOrWhiteSpace($env:LFS_CLOUD_APT_CLOUDSMITH_TARGET)) {
                throw 'Set LFS_CLOUD_APT_CLOUDSMITH_TARGET before publishing APT packages.'
            }
        }

        $temporaryRoot = Join-Path `
            ([System.IO.Path]::GetTempPath()) `
            "lfscloud-publish-$([guid]::NewGuid().ToString('N'))"
        $assetDirectory = Join-Path $temporaryRoot 'assets'
        [void][System.IO.Directory]::CreateDirectory($assetDirectory)
        Invoke-ReleaseStep 'Download every draft release asset' 'gh' @(
            'release', 'download', $candidate.Tag, '--repo', $script:RELEASE_GITHUB_REPO, '--dir', $assetDirectory
        )
        $release = Get-PublishReleaseDocument -Tag $candidate.Tag
        Invoke-ReleaseActionStep 'Verify checksums, manifests, and remote asset digests' {
            Assert-DownloadedReleaseAssets -Candidate $candidate -Release $release -Directory $assetDirectory
        }

        $formulaPath = Join-Path $temporaryRoot 'lfscloud.rb'
        $macDigest = (Get-FileHash `
                (Join-Path $assetDirectory "lfscloud-v$($candidate.VersionText)-macos-arm64.tar.gz") `
                -Algorithm SHA256).Hash.ToLowerInvariant()
        $linuxX64Digest = (Get-FileHash `
                (Join-Path $assetDirectory "lfscloud-v$($candidate.VersionText)-linux-x86_64-musl.tar.gz") `
                -Algorithm SHA256).Hash.ToLowerInvariant()
        $linuxArmDigest = (Get-FileHash `
                (Join-Path $assetDirectory "lfscloud-v$($candidate.VersionText)-linux-arm64-musl.tar.gz") `
                -Algorithm SHA256).Hash.ToLowerInvariant()
        [System.IO.File]::WriteAllText(
            $formulaPath,
            (New-HomebrewFormulaText `
                    -Version $candidate.VersionText `
                    -MacSha256 $macDigest `
                    -LinuxX64Sha256 $linuxX64Digest `
                    -LinuxArm64Sha256 $linuxArmDigest),
            [System.Text.UTF8Encoding]::new($false)
        )
        $wingetDirectory = Join-Path $temporaryRoot 'winget-manifests'
        $windowsDigest = (Get-FileHash `
                (Join-Path $assetDirectory "lfscloud-v$($candidate.VersionText)-windows-x86_64.zip") `
                -Algorithm SHA256).Hash.ToLowerInvariant()
        New-WinGetManifests `
            -Version $candidate.VersionText `
            -InstallerSha256 $windowsDigest `
            -Directory $wingetDirectory

        if ($candidate.IsDraft) {
            Write-Host ''
            Write-Host "Publish $($candidate.Tag)?"
            Write-Host 'This will make its tag and assets immutable, publish Homebrew and APT metadata,'
            Write-Host 'and submit a WinGet Community repository pull request.'
            $confirmation = Read-Host "Type 'publish $($candidate.Tag)' to continue"
            if ($confirmation -ne "publish $($candidate.Tag)") {
                Write-ReleaseInfo 'Release publication cancelled.'
                return 0
            }

            Invoke-ReleaseStep 'Enable immutable GitHub releases' 'gh' @(
                'api', '--method', 'PUT',
                '--header', 'X-GitHub-Api-Version: 2026-03-10',
                "repos/$($script:RELEASE_GITHUB_REPO)/immutable-releases"
            )
            Invoke-ReleaseStep "Publish immutable release $($candidate.Tag)" 'gh' @(
                'release', 'edit', $candidate.Tag, '--repo', $script:RELEASE_GITHUB_REPO, '--draft=false', '--latest'
            )
            $release = Get-PublishReleaseDocument -Tag $candidate.Tag
            if ([bool] $release.isDraft -or -not [bool] $release.isImmutable) {
                throw "GitHub release $($candidate.Tag) was not published as immutable."
            }
        }

        Invoke-ReleaseStep "Verify the immutable release attestation for $($candidate.Tag)" 'gh' @(
            'release', 'verify', $candidate.Tag, '--repo', $script:RELEASE_GITHUB_REPO
        )

        $results = @()
        $results += Invoke-DistributionAction `
            -Candidate $candidate `
            -Context $script:DISTRIBUTION_DIRECT_CONTEXT `
            -Description "Direct installer publication for $($candidate.Tag)" `
            -Action { Publish-DirectInstallers -AssetDirectory $assetDirectory -Tag $candidate.Tag }
        $results += Invoke-DistributionAction `
            -Candidate $candidate `
            -Context $script:DISTRIBUTION_HOMEBREW_CONTEXT `
            -Description "Homebrew publication for $($candidate.Tag)" `
            -Action { Publish-HomebrewFormula -FormulaPath $formulaPath -Tag $candidate.Tag }
        $results += Invoke-DistributionAction `
            -Candidate $candidate `
            -Context $script:DISTRIBUTION_APT_CONTEXT `
            -Description "APT publication for $($candidate.Tag)" `
            -Action { Publish-DebianPackages -AssetDirectory $assetDirectory -Version $candidate.VersionText }
        $results += Invoke-DistributionAction `
            -Candidate $candidate `
            -Context $script:DISTRIBUTION_WINGET_CONTEXT `
            -Description "WinGet submission for $($candidate.Tag)" `
            -Action { Publish-WinGetManifests -ManifestDirectory $wingetDirectory -Version $candidate.VersionText -TemporaryRoot $temporaryRoot }
        if (@($results | Where-Object { -not $_ }).Count -gt 0) {
            throw 'One or more distribution channels failed; rerun release:publish to resume them.'
        }
        Write-ReleasePass "Published and distributed $($candidate.Tag)"
    }
    catch {
        $exitCode = 1
        Write-Error $_.Exception.Message
    }
    finally {
        if (-not [string]::IsNullOrWhiteSpace($temporaryRoot) -and
            (Test-Path -LiteralPath $temporaryRoot -PathType Container)) {
            Remove-Item -LiteralPath $temporaryRoot -Recurse -Force -ErrorAction SilentlyContinue
        }
    }
    return $exitCode
}

if ($MyInvocation.InvocationName -ne '.') {
    if ($ShowHelp) {
        Write-PublishUsage
        exit 0
    }
    exit (Invoke-ReleasePublisher)
}
