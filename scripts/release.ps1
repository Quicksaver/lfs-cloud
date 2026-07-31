#Requires -Version 7.0

[CmdletBinding()]
param(
    [Alias('h', 'Help')] [switch] $ShowHelp
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$continuationScriptDirectory = Split-Path -Parent $PSCommandPath
. (Join-Path $continuationScriptDirectory 'local' 'verify-windows.ps1')

function Write-Usage {
    @'
Usage: pwsh ./scripts/release.ps1

Continue one or more releases created by release.sh from a native Windows x64
checkout. The script lists published semantic versions without successful
local-checks/windows-x86_64 verification, prompts for multiple versions, checks
out each tag, verifies and packages its exact executable, uploads the Windows
assets, verifies them on GitHub, and restores the original checkout.
'@ | Write-Host
}

function Get-WindowsVerificationStatusStateFromDocument {
    param([Parameter(Mandatory = $true)] $Document)

    $matchingStatuses = @(
        $Document.statuses |
            Where-Object { $_.context -eq $script:LOCAL_WINDOWS_STATUS_CONTEXT } |
            Sort-Object -Property id -Descending
    )
    if ($matchingStatuses.Count -eq 0) {
        return 'missing'
    }

    return [string] $matchingStatuses[0].state
}

function Get-WindowsVerificationStatusState {
    param([Parameter(Mandatory = $true)] [string] $Commit)

    $result = Invoke-NativeCapture 'gh' @(
        'api',
        "repos/$($script:RELEASE_GITHUB_REPO)/commits/$Commit/status?per_page=100"
    )
    if ($result.ExitCode -ne 0) {
        throw "Could not read commit statuses for $Commit."
    }

    try {
        $document = $result.Output | ConvertFrom-Json
    }
    catch {
        throw "GitHub returned invalid commit-status JSON for $Commit."
    }

    return Get-WindowsVerificationStatusStateFromDocument -Document $document
}

function Get-WindowsReleaseCandidates {
    $result = Invoke-NativeCapture 'gh' @(
        'release',
        'list',
        '--repo',
        $script:RELEASE_GITHUB_REPO,
        '--exclude-drafts',
        '--limit',
        '1000',
        '--json',
        'tagName,isDraft,isPrerelease,publishedAt'
    )
    if ($result.ExitCode -ne 0) {
        throw 'Could not list published GitHub releases.'
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
        if ([bool] $release.isDraft -or $tag -notmatch '^v(\d+\.\d+\.\d+)$') {
            continue
        }

        $commit = Get-RemoteReleaseTagCommit -Tag $tag
        if ([string]::IsNullOrWhiteSpace($commit)) {
            throw "Published release $tag does not have a matching tag on origin."
        }

        $status = Get-WindowsVerificationStatusState -Commit $commit
        if ($status -eq 'success') {
            continue
        }

        [void] $candidates.Add([pscustomobject] @{
                Version = [version] $Matches[1]
                VersionText = $Matches[1]
                Tag = $tag
                Commit = $commit
                Status = $status
                PublishedAt = [string] $release.publishedAt
            })
    }

    return @(
        $candidates |
            Sort-Object -Property @{ Expression = { $_.Version }; Descending = $true }
    )
}

function ConvertTo-ReleaseSelection {
    param(
        [AllowEmptyString()] [string] $Selection,
        [Parameter(Mandatory = $true)] [ValidateRange(1, [int]::MaxValue)] [int] $CandidateCount
    )

    if ([string]::IsNullOrWhiteSpace($Selection)) {
        return @()
    }

    $normalized = $Selection.Trim().ToLowerInvariant()
    if ($normalized -eq 'all') {
        return @(0..($CandidateCount - 1))
    }

    $indices = [System.Collections.Generic.HashSet[int]]::new()
    foreach ($rawToken in @($normalized -split ',')) {
        $token = $rawToken.Trim()
        if ($token -match '^(\d+)$') {
            $start = [int] $Matches[1]
            $end = $start
        }
        elseif ($token -match '^(\d+)\s*-\s*(\d+)$') {
            $start = [int] $Matches[1]
            $end = [int] $Matches[2]
            if ($start -gt $end) {
                throw "Invalid descending selection range: $token"
            }
        }
        else {
            throw "Invalid selection token: $token"
        }

        if ($start -lt 1 -or $end -gt $CandidateCount) {
            throw "Selection $token is outside the available range 1-$CandidateCount."
        }

        foreach ($number in $start..$end) {
            [void] $indices.Add($number - 1)
        }
    }

    return @($indices | Sort-Object)
}

function Read-WindowsReleaseSelection {
    param([Parameter(Mandatory = $true)] [object[]] $Candidates)

    Write-Host ''
    Write-Host 'Published releases without successful Windows verification:'
    for ($index = 0; $index -lt $Candidates.Count; $index++) {
        $candidate = $Candidates[$index]
        Write-Host ("  [{0}] {1}  Windows status: {2}" -f ($index + 1), $candidate.Tag, $candidate.Status)
    }
    Write-Host ''

    while ($true) {
        $answer = Read-Host 'Select releases (for example 1,3-4 or all; blank cancels)'
        try {
            return @(
                ConvertTo-ReleaseSelection `
                    -Selection $answer `
                    -CandidateCount $Candidates.Count
            )
        }
        catch {
            Write-ReleaseWarning $_.Exception.Message
        }
    }
}

function Get-ReleaseCheckoutState {
    $shaResult = Invoke-NativeCapture 'git' @(
        '-C',
        $script:RELEASE_REPO_ROOT,
        'rev-parse',
        'HEAD'
    )
    if ($shaResult.ExitCode -ne 0 -or $shaResult.Output -notmatch '^[0-9a-f]{40}$') {
        throw 'Could not record the original checkout commit.'
    }

    $branchResult = Invoke-NativeCapture 'git' @(
        '-C',
        $script:RELEASE_REPO_ROOT,
        'symbolic-ref',
        '--quiet',
        '--short',
        'HEAD'
    )
    $branch = if ($branchResult.ExitCode -eq 0) { $branchResult.Output } else { '' }

    return [pscustomobject] @{
        Branch = $branch
        Commit = $shaResult.Output
    }
}

function Switch-ToReleaseCandidate {
    param([Parameter(Mandatory = $true)] $Candidate)

    $localResult = Invoke-NativeCapture 'git' @(
        '-C',
        $script:RELEASE_REPO_ROOT,
        'rev-list',
        '-n',
        '1',
        "refs/tags/$($Candidate.Tag)"
    )
    if ($localResult.ExitCode -ne 0 -or $localResult.Output -ne $Candidate.Commit) {
        throw "Local tag $($Candidate.Tag) does not match origin commit $($Candidate.Commit)."
    }

    $switchResult = Invoke-NativeCapture 'git' @(
        '-C',
        $script:RELEASE_REPO_ROOT,
        'switch',
        '--detach',
        "refs/tags/$($Candidate.Tag)"
    )
    if ($switchResult.ExitCode -ne 0) {
        throw "Could not check out release $($Candidate.Tag)."
    }

    Write-ReleasePass "Checked out $($Candidate.Tag) at $($Candidate.Commit)"
}

function Restore-ReleaseCheckout {
    param([Parameter(Mandatory = $true)] $CheckoutState)

    $arguments = @('-C', $script:RELEASE_REPO_ROOT, 'switch')
    if ([string]::IsNullOrWhiteSpace([string] $CheckoutState.Branch)) {
        $arguments += @('--detach', [string] $CheckoutState.Commit)
    }
    else {
        $arguments += [string] $CheckoutState.Branch
    }

    $switchResult = Invoke-NativeCapture 'git' $arguments
    if ($switchResult.ExitCode -ne 0) {
        throw 'Could not restore the original checkout.'
    }

    $shaResult = Invoke-NativeCapture 'git' @(
        '-C',
        $script:RELEASE_REPO_ROOT,
        'rev-parse',
        'HEAD'
    )
    if ($shaResult.ExitCode -ne 0 -or $shaResult.Output -ne $CheckoutState.Commit) {
        throw "Restored checkout does not point to original commit $($CheckoutState.Commit)."
    }

    Write-ReleasePass "Restored original checkout at $($CheckoutState.Commit)"
}

function Get-GitHubReleaseDocument {
    param([Parameter(Mandatory = $true)] [string] $Tag)

    $result = Invoke-NativeCapture 'gh' @(
        'release',
        'view',
        $Tag,
        '--repo',
        $script:RELEASE_GITHUB_REPO,
        '--json',
        'assets,isDraft,isImmutable,tagName,url'
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

function Assert-WindowsReleaseAssetsPublished {
    param(
        [Parameter(Mandatory = $true)] $Release,
        [Parameter(Mandatory = $true)] [string[]] $AssetPaths
    )

    foreach ($assetPath in $AssetPaths) {
        $name = [System.IO.Path]::GetFileName($assetPath)
        $matchingAssets = @($Release.assets | Where-Object { $_.name -eq $name })
        if ($matchingAssets.Count -ne 1) {
            throw "GitHub release does not contain exactly one asset named $name."
        }

        $remoteAsset = $matchingAssets[0]
        $localFile = Get-Item -LiteralPath $assetPath
        $localDigest = (Get-FileHash -LiteralPath $assetPath -Algorithm SHA256).Hash.ToLowerInvariant()
        if ($remoteAsset.state -ne 'uploaded' -or
            [long] $remoteAsset.size -ne $localFile.Length -or
            [string] $remoteAsset.digest -ne "sha256:$localDigest") {
            throw "GitHub release asset $name does not match the verified local file."
        }
    }
}

function Publish-WindowsReleaseAssets {
    param([Parameter(Mandatory = $true)] $Candidate)

    $artifact = Get-WindowsArtifactPath `
        -RepositoryRoot $script:RELEASE_REPO_ROOT `
        -Version $Candidate.VersionText
    $manifest = Get-WindowsManifestPath `
        -RepositoryRoot $script:RELEASE_REPO_ROOT `
        -Version $Candidate.VersionText
    $assets = @($artifact, "$artifact.sha256", $manifest)

    if (-not (Test-ArtifactChecksum -ArtifactPath $artifact)) {
        throw "Windows artifact checksum is invalid for $($Candidate.Tag)."
    }
    if (-not (Test-WindowsBuildManifest `
                -ArtifactPath $artifact `
                -ManifestPath $manifest `
                -Version $Candidate.VersionText `
                -Commit $Candidate.Commit)) {
        throw "Windows build manifest is invalid for $($Candidate.Tag)."
    }

    $release = Get-GitHubReleaseDocument -Tag $Candidate.Tag
    if ([bool] $release.isDraft -or $release.tagName -ne $Candidate.Tag) {
        throw "GitHub release $($Candidate.Tag) is not published for the selected tag."
    }
    if ([bool] $release.isImmutable) {
        throw "GitHub release $($Candidate.Tag) is immutable and cannot accept Windows assets."
    }

    Initialize-ReleaseUi '[release-windows]' "Publish Windows release $($Candidate.Tag)"
    try {
        Invoke-ReleaseStep `
            -Message "Upload Windows assets to $($Candidate.Tag)" `
            -Command 'gh' `
            -Arguments @(
                'release',
                'upload',
                $Candidate.Tag,
                $assets,
                '--repo',
                $script:RELEASE_GITHUB_REPO,
                '--clobber'
            )

        Invoke-ReleaseActionStep 'Verify published Windows assets' {
            $publishedRelease = Get-GitHubReleaseDocument -Tag $Candidate.Tag
            Assert-WindowsReleaseAssetsPublished `
                -Release $publishedRelease `
                -AssetPaths $assets
        }

        Invoke-ReleaseActionStep 'Record Windows release as successful' {
            Set-ReleaseStatus `
                -Commit $Candidate.Commit `
                -Context $script:LOCAL_WINDOWS_STATUS_CONTEXT `
                -State 'success' `
                -Description "Windows $($Candidate.VersionText) x86-64 release published"
        }

        Write-ReleasePass "Published Windows assets to $($release.url)"
    }
    catch {
        $failureMessage = $_.Exception.Message
        try {
            Set-ReleaseStatus `
                -Commit $Candidate.Commit `
                -Context $script:LOCAL_WINDOWS_STATUS_CONTEXT `
                -State 'failure' `
                -Description "Windows $($Candidate.VersionText) x86-64 release publication failed"
        }
        catch {
            Write-ReleaseWarning "Failed to record Windows publication failure for $($Candidate.Tag)."
        }
        throw $failureMessage
    }
    finally {
        Complete-ReleaseUi
    }
}

function Invoke-WindowsReleaseContinuation {
    $originalLocation = Get-Location
    $checkoutState = $null
    $failedTags = [System.Collections.Generic.List[string]]::new()
    $exitCode = 0

    try {
        Assert-WindowsX64Host
        Initialize-Release -StartDirectory $continuationScriptDirectory
        Set-Location -LiteralPath $script:RELEASE_REPO_ROOT
        Assert-ReleaseFullyClean
        $checkoutState = Get-ReleaseCheckoutState

        $fetchResult = Invoke-NativeCapture 'git' @(
            '-C',
            $script:RELEASE_REPO_ROOT,
            'fetch',
            '--quiet',
            'origin',
            '--tags'
        )
        if ($fetchResult.ExitCode -ne 0) {
            throw 'Could not fetch release tags from origin.'
        }

        $candidates = @(Get-WindowsReleaseCandidates)
        if ($candidates.Count -eq 0) {
            Write-ReleasePass 'Every published semantic release already has successful Windows verification.'
        }
        else {
            $selectedIndices = @(Read-WindowsReleaseSelection -Candidates $candidates)
            if ($selectedIndices.Count -eq 0) {
                Write-ReleaseInfo 'Windows release continuation cancelled.'
            }
            else {
                foreach ($index in $selectedIndices) {
                    $candidate = $candidates[$index]
                    try {
                        Switch-ToReleaseCandidate -Candidate $candidate
                        $verificationExit = Invoke-WindowsVerification `
                            -ReleaseTag $candidate.Tag `
                            -DeferSuccessStatus
                        if ($verificationExit -ne 0) {
                            throw "Windows verification failed for $($candidate.Tag)."
                        }

                        Publish-WindowsReleaseAssets -Candidate $candidate
                    }
                    catch {
                        [void] $failedTags.Add($candidate.Tag)
                        Write-ReleaseWarning "$($candidate.Tag) failed: $($_.Exception.Message)"
                    }
                }
            }
        }

        if ($failedTags.Count -gt 0) {
            Write-ReleaseWarning "Windows release continuation failed for: $($failedTags -join ', ')"
            $exitCode = 1
        }
    }
    catch {
        fail $_.Exception.Message
        $exitCode = 1
    }
    finally {
        if ($null -ne $checkoutState) {
            try {
                Restore-ReleaseCheckout -CheckoutState $checkoutState
            }
            catch {
                fail $_.Exception.Message
                $exitCode = 1
            }
        }
        Set-Location -LiteralPath $originalLocation
    }

    return $exitCode
}

if ($MyInvocation.InvocationName -ne '.') {
    if ($ShowHelp) {
        Write-Usage
        exit 0
    }

    exit (Invoke-WindowsReleaseContinuation)
}
