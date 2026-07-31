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

Continue a release created by release.sh from a native Windows x64 checkout.
The script lists published semantic versions without successful
local-checks/windows-x86_64 verification, prompts for a version with an
arrow-key menu, checks out its tag, verifies and packages its exact executable,
uploads the Windows assets, verifies them on GitHub, and restores the original
checkout.
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

function Format-WindowsReleaseMenu {
    param(
        [Parameter(Mandatory = $true)] [object[]] $Candidates,
        [Parameter(Mandatory = $true)] [int] $SelectedIndex
    )

    $visibleCount = [Math]::Min($Candidates.Count, 10)
    $windowStart = [Math]::Max(0, $SelectedIndex - [Math]::Floor($visibleCount / 2))
    $windowStart = [Math]::Min($windowStart, $Candidates.Count - $visibleCount)
    $windowEnd = $windowStart + $visibleCount - 1

    $lines = [System.Collections.Generic.List[string]]::new()
    foreach ($index in $windowStart..$windowEnd) {
        $candidate = $Candidates[$index]
        $marker = if ($index -eq $SelectedIndex) { '>' } else { ' ' }
        [void] $lines.Add((
                '{0} {1,-12} Windows status: {2}' -f $marker, $candidate.Tag, $candidate.Status
            ))
    }

    [void] $lines.Add((
            '  Showing {0}-{1} of {2}' -f ($windowStart + 1), ($windowEnd + 1), $Candidates.Count
        ))
    return @($lines)
}

function Write-WindowsReleaseMenu {
    param(
        [Parameter(Mandatory = $true)] [object[]] $Candidates,
        [Parameter(Mandatory = $true)] [int] $SelectedIndex,
        [switch] $Redraw
    )

    $lines = @(Format-WindowsReleaseMenu -Candidates $Candidates -SelectedIndex $SelectedIndex)
    if ($Redraw) {
        $menuTop = [Math]::Max(0, [Console]::CursorTop - $lines.Count)
        [Console]::SetCursorPosition(0, $menuTop)
    }

    $lineWidth = 119
    try {
        $lineWidth = [Math]::Max(1, [Console]::BufferWidth - 1)
    }
    catch {
        # Interactive hosts normally expose a buffer width. The fallback keeps
        # the selector usable in less conventional Windows terminal hosts.
    }

    foreach ($line in $lines) {
        $renderedLine = if ($line.Length -gt $lineWidth) {
            $line.Substring(0, $lineWidth)
        }
        else {
            $line
        }

        [Console]::Write($renderedLine.PadRight($lineWidth))
        [Console]::WriteLine()
    }
}

function Read-WindowsReleaseSelection {
    param(
        [Parameter(Mandatory = $true)] [object[]] $Candidates,
        [scriptblock] $ReadKey,
        [scriptblock] $Render
    )

    if ($Candidates.Count -eq 0) {
        throw 'At least one Windows release candidate is required.'
    }

    if ($null -eq $ReadKey) {
        if ([Console]::IsInputRedirected -or [Console]::IsOutputRedirected) {
            throw 'Windows release selection requires an interactive terminal.'
        }
        $ReadKey = { return [Console]::ReadKey($true) }
    }

    if ($null -eq $Render) {
        $Render = {
            param($Items, $HighlightedIndex, $IsRedraw)
            Write-WindowsReleaseMenu `
                -Candidates $Items `
                -SelectedIndex $HighlightedIndex `
                -Redraw:$IsRedraw
        }
    }

    Write-Host ''
    Write-Host 'Published releases without successful Windows verification:'
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

        if ($key -eq [System.ConsoleKey]::UpArrow) {
            $selectedIndex = ($selectedIndex - 1 + $Candidates.Count) % $Candidates.Count
            & $Render $Candidates $selectedIndex $true
        }
        elseif ($key -eq [System.ConsoleKey]::DownArrow) {
            $selectedIndex = ($selectedIndex + 1) % $Candidates.Count
            & $Render $Candidates $selectedIndex $true
        }
        elseif ($key -eq [System.ConsoleKey]::Enter) {
            Write-Host ''
            return @($selectedIndex)
        }
        elseif ($key -eq [System.ConsoleKey]::Escape) {
            Write-Host ''
            return @()
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

function Get-WindowsReleaseUploadArguments {
    param(
        [Parameter(Mandatory = $true)] [string] $Tag,
        [Parameter(Mandatory = $true)] [string[]] $AssetPaths,
        [Parameter(Mandatory = $true)] [string] $Repository
    )

    # A nested array passed to a [string[]] PowerShell parameter is coerced to
    # one space-joined string. Add each path independently so gh receives three
    # file arguments rather than one nonexistent combined path.
    $arguments = [System.Collections.Generic.List[string]]::new()
    foreach ($argument in @('release', 'upload', $Tag)) {
        [void] $arguments.Add($argument)
    }
    foreach ($assetPath in $AssetPaths) {
        [void] $arguments.Add($assetPath)
    }
    foreach ($argument in @('--repo', $Repository, '--clobber')) {
        [void] $arguments.Add($argument)
    }

    return @($arguments)
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
            -Arguments @(Get-WindowsReleaseUploadArguments `
                -Tag $Candidate.Tag `
                -AssetPaths $assets `
                -Repository $script:RELEASE_GITHUB_REPO)

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
