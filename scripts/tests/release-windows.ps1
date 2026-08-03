#Requires -Version 7.0

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$script:TestsPassed = 0
$script:TestsFailed = 0

function Assert-Equal {
    param(
        [Parameter(Mandatory = $true)] [AllowNull()] $Expected,
        [Parameter(Mandatory = $true)] [AllowNull()] $Actual,
        [Parameter(Mandatory = $true)] [string] $Message
    )

    if ($Expected -ne $Actual) {
        throw "$Message`: expected '$Expected', got '$Actual'"
    }
}

function Assert-True {
    param(
        [Parameter(Mandatory = $true)] [bool] $Condition,
        [Parameter(Mandatory = $true)] [string] $Message
    )

    if (-not $Condition) {
        throw $Message
    }
}

function Invoke-Test {
    param(
        [Parameter(Mandatory = $true)] [string] $Name,
        [Parameter(Mandatory = $true)] [scriptblock] $Test
    )

    try {
        & $Test
        $script:TestsPassed++
        Write-Host "[release-windows-tests] + PASS $Name"
    }
    catch {
        $script:TestsFailed++
        Write-Host "[release-windows-tests] x FAIL $Name"
        Write-Host "  $($_.Exception.Message)"
    }
}

$continuationScript = Join-Path $PSScriptRoot '..' 'release.ps1'
. $continuationScript

function Invoke-ReleaseSelectionKeys {
    param([Parameter(Mandatory = $true)] [System.ConsoleKey[]] $Keys)

    $keyQueue = [System.Collections.Generic.Queue[System.ConsoleKey]]::new()
    foreach ($key in $Keys) {
        $keyQueue.Enqueue($key)
    }

    $candidates = @(
        [pscustomobject] @{ Tag = 'v3.0.0'; Status = 'missing' },
        [pscustomobject] @{ Tag = 'v2.0.0'; Status = 'failure' },
        [pscustomobject] @{ Tag = 'v1.0.0'; Status = 'pending' }
    )
    $readKey = { return $keyQueue.Dequeue() }.GetNewClosure()
    $render = { param($Items, $SelectedIndex, $Redraw) }.GetNewClosure()

    return @(
        Read-WindowsReleaseSelection `
            -Candidates $candidates `
            -ReadKey $readKey `
            -Render $render
    )
}

Invoke-Test 'Arrow-key selector chooses the highlighted release with Enter' {
    $selection = @(Invoke-ReleaseSelectionKeys -Keys @(
            [System.ConsoleKey]::DownArrow,
            [System.ConsoleKey]::DownArrow,
            [System.ConsoleKey]::Enter
        ))
    Assert-Equal '2' ($selection -join ',') 'selected release index'
}

Invoke-Test 'Arrow-key selector wraps from the first release to the last' {
    $selection = @(Invoke-ReleaseSelectionKeys -Keys @(
            [System.ConsoleKey]::UpArrow,
            [System.ConsoleKey]::Enter
        ))
    Assert-Equal '2' ($selection -join ',') 'wrapped release index'
}

Invoke-Test 'Arrow-key selector cancels with Escape' {
    $selection = @(Invoke-ReleaseSelectionKeys -Keys @([System.ConsoleKey]::Escape))
    Assert-Equal 0 $selection.Count 'cancelled selection count'
}

Invoke-Test 'Requested tag selects one exact Windows release candidate' {
    $empty = @(Select-WindowsReleaseCandidates -Candidates @())
    Assert-Equal 0 $empty.Count 'empty interactive candidate list'

    $candidates = @(
        [pscustomobject] @{ Tag = 'v2.0.0'; Status = 'missing' },
        [pscustomobject] @{ Tag = 'v1.0.0'; Status = 'failure' }
    )
    $selected = @(Select-WindowsReleaseCandidates -Candidates $candidates -RequestedTag 'v1.0.0')
    Assert-Equal 1 $selected.Count 'targeted candidate count'
    Assert-Equal 'v1.0.0' $selected[0].Tag 'targeted candidate tag'

    $threw = $false
    try {
        Select-WindowsReleaseCandidates -Candidates $candidates -RequestedTag 'v3.0.0'
    }
    catch {
        $threw = $true
    }
    Assert-True $threw 'missing targeted draft should fail'
}

Invoke-Test 'Requested tag ignores unrelated drafts before metadata lookup' {
    $originalNativeCapture = (Get-Item Function:Invoke-NativeCapture).ScriptBlock
    $originalTagCommit = (Get-Item Function:Get-RemoteReleaseTagCommit).ScriptBlock
    $originalStatusState = (Get-Item Function:Get-WindowsVerificationStatusState).ScriptBlock
    $script:RequestedCandidateMetadataCalls = [System.Collections.Generic.List[string]]::new()

    try {
        Set-Item Function:Invoke-NativeCapture {
            return [pscustomobject] @{
                ExitCode = 0
                Output = @'
[
  {"tagName":"v2.0.0","isDraft":true,"isPrerelease":false,"publishedAt":null},
  {"tagName":"v1.0.0","isDraft":true,"isPrerelease":false,"publishedAt":null}
]
'@
            }
        }
        Set-Item Function:Get-RemoteReleaseTagCommit {
            param([string] $Tag)
            [void] $script:RequestedCandidateMetadataCalls.Add("tag:$Tag")
            if ($Tag -ne 'v1.0.0') {
                throw "unexpected origin lookup for $Tag"
            }
            return '0123456789abcdef0123456789abcdef01234567'
        }
        Set-Item Function:Get-WindowsVerificationStatusState {
            param([string] $Commit)
            [void] $script:RequestedCandidateMetadataCalls.Add("status:$Commit")
            return 'missing'
        }

        $candidates = @(Get-WindowsReleaseCandidates -RequestedTag 'v1.0.0')
        Assert-Equal 1 $candidates.Count 'targeted discovery candidate count'
        Assert-Equal 'v1.0.0' $candidates[0].Tag 'targeted discovery tag'
        Assert-Equal `
            'tag:v1.0.0,status:0123456789abcdef0123456789abcdef01234567' `
            ($script:RequestedCandidateMetadataCalls -join ',') `
            'targeted discovery metadata lookups'
    }
    finally {
        Set-Item Function:Invoke-NativeCapture $originalNativeCapture
        Set-Item Function:Get-RemoteReleaseTagCommit $originalTagCommit
        Set-Item Function:Get-WindowsVerificationStatusState $originalStatusState
    }
}

Invoke-Test 'Requested tag repairs assets after a successful Windows status' {
    $originalNativeCapture = (Get-Item Function:Invoke-NativeCapture).ScriptBlock
    $originalTagCommit = (Get-Item Function:Get-RemoteReleaseTagCommit).ScriptBlock
    $originalStatusState = (Get-Item Function:Get-WindowsVerificationStatusState).ScriptBlock

    try {
        Set-Item Function:Invoke-NativeCapture {
            return [pscustomobject] @{
                ExitCode = 0
                Output = '[{"tagName":"v1.0.0","isDraft":true,"isPrerelease":false,"publishedAt":null}]'
            }
        }
        Set-Item Function:Get-RemoteReleaseTagCommit {
            return '0123456789abcdef0123456789abcdef01234567'
        }
        Set-Item Function:Get-WindowsVerificationStatusState { return 'success' }

        $candidates = @(Get-WindowsReleaseCandidates -RequestedTag 'v1.0.0')
        Assert-Equal 1 $candidates.Count 'targeted successful candidate count'
        Assert-Equal 'success' $candidates[0].Status 'targeted successful candidate status'
    }
    finally {
        Set-Item Function:Invoke-NativeCapture $originalNativeCapture
        Set-Item Function:Get-RemoteReleaseTagCommit $originalTagCommit
        Set-Item Function:Get-WindowsVerificationStatusState $originalStatusState
    }
}

Invoke-Test 'Windows upload passes each asset path as a distinct argument' {
    $assetPaths = @('archive.tar.gz', 'archive.tar.gz.sha256', 'archive.build.json')
    $arguments = @(Get-WindowsReleaseUploadArguments `
            -Tag 'v1.2.3' `
            -AssetPaths $assetPaths `
            -Repository 'owner/repository')

    Assert-Equal 9 $arguments.Count 'upload argument count'
    Assert-Equal 'archive.tar.gz' $arguments[3] 'archive argument'
    Assert-Equal 'archive.tar.gz.sha256' $arguments[4] 'checksum argument'
    Assert-Equal 'archive.build.json' $arguments[5] 'manifest argument'
    Assert-Equal '--repo' $arguments[6] 'repository flag'
}

Invoke-Test 'Release step failure retains native command output' {
    $originalRunner = (Get-Item Function:ui_run_with_live_stdout).ScriptBlock
    try {
        Set-Item Function:ui_run_with_live_stdout {
            [void] $script:LIVE_OUTPUT_TAIL_LINES.Add('upload failed: fixture reason')
            return $false
        }

        $message = ''
        try {
            Invoke-ReleaseStep -Message 'Upload fixture' -Command 'fixture'
        }
        catch {
            $message = $_.Exception.Message
        }

        Assert-True `
            ($message.Contains('upload failed: fixture reason')) `
            'native failure output should be retained in the exception'
    }
    finally {
        Set-Item Function:ui_run_with_live_stdout $originalRunner
    }
}

Invoke-Test 'Latest Windows status wins and unrelated contexts are ignored' {
    $document = [pscustomobject] @{
        statuses = @(
            [pscustomobject] @{ id = 4; context = 'unrelated'; state = 'success' },
            [pscustomobject] @{ id = 3; context = $script:LOCAL_WINDOWS_STATUS_CONTEXT; state = 'failure' },
            [pscustomobject] @{ id = 9; context = $script:LOCAL_WINDOWS_STATUS_CONTEXT; state = 'pending' }
        )
    }
    $state = Get-WindowsVerificationStatusStateFromDocument -Document $document
    Assert-Equal 'pending' $state 'latest Windows status'
    $document.statuses[2] | Add-Member -NotePropertyName creator -NotePropertyValue ([pscustomobject] @{ login = 'other' })
    $untrusted = Get-WindowsVerificationStatusStateFromDocument `
        -Document $document `
        -TrustedLogin 'fixture'
    Assert-Equal 'untrusted' $untrusted 'unexpected Windows status creator'

    $missing = Get-WindowsVerificationStatusStateFromDocument -Document ([pscustomobject] @{ statuses = @() })
    Assert-Equal 'missing' $missing 'missing Windows status'

    $document.statuses[2].creator = $null
    $nullCreator = Get-WindowsVerificationStatusStateFromDocument `
        -Document $document `
        -TrustedLogin 'fixture'
    Assert-Equal 'untrusted' $nullCreator 'null Windows status creator'

    $document.statuses[2].PSObject.Properties.Remove('creator')
    $missingCreator = Get-WindowsVerificationStatusStateFromDocument `
        -Document $document `
        -TrustedLogin 'fixture'
    Assert-Equal 'untrusted' $missingCreator 'missing Windows status creator'
}

Invoke-Test 'Published assets must match local names, sizes, and SHA-256 digests' {
    $fixtureRoot = Join-Path ([System.IO.Path]::GetTempPath()) "lfscloud-release-assets-$([guid]::NewGuid().ToString('N'))"
    [void] [System.IO.Directory]::CreateDirectory($fixtureRoot)

    try {
        $assetPaths = @()
        foreach ($name in @('binary.tar.gz', 'binary.tar.gz.sha256', 'binary.build.json')) {
            $path = Join-Path $fixtureRoot $name
            [System.IO.File]::WriteAllText($path, "fixture $name")
            $assetPaths += $path
        }

        $releaseAssets = @(
            foreach ($path in $assetPaths) {
                [pscustomobject] @{
                    name = [System.IO.Path]::GetFileName($path)
                    state = 'uploaded'
                    size = (Get-Item -LiteralPath $path).Length
                    digest = "sha256:$((Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant())"
                }
            }
        )
        Assert-WindowsReleaseAssetsPublished `
            -Release ([pscustomobject] @{ assets = $releaseAssets }) `
            -AssetPaths $assetPaths

        $releaseAssets[0].digest = 'sha256:bad'
        $threw = $false
        try {
            Assert-WindowsReleaseAssetsPublished `
                -Release ([pscustomobject] @{ assets = $releaseAssets }) `
                -AssetPaths $assetPaths
        }
        catch {
            $threw = $true
        }
        Assert-True $threw 'digest mismatch should fail'
    }
    finally {
        Remove-Item -LiteralPath $fixtureRoot -Recurse -Force -ErrorAction SilentlyContinue
    }
}

Invoke-Test 'Release checkout returns to the original branch and commit' {
    $fixtureRoot = Join-Path ([System.IO.Path]::GetTempPath()) "lfscloud-release-checkout-$([guid]::NewGuid().ToString('N'))"
    [void] [System.IO.Directory]::CreateDirectory($fixtureRoot)

    try {
        foreach ($arguments in @(
                @('init', '--quiet', '--initial-branch=main', $fixtureRoot),
                @('-C', $fixtureRoot, 'config', 'user.email', 'release-tests@example.com'),
                @('-C', $fixtureRoot, 'config', 'user.name', 'Release Tests')
            )) {
            $result = Invoke-NativeCapture 'git' $arguments
            Assert-Equal 0 $result.ExitCode "git $($arguments -join ' ')"
        }

        [System.IO.File]::WriteAllText((Join-Path $fixtureRoot 'fixture.txt'), 'release')
        foreach ($arguments in @(
                @('-C', $fixtureRoot, 'add', 'fixture.txt'),
                @('-C', $fixtureRoot, 'commit', '--quiet', '--message', 'release'),
                @('-C', $fixtureRoot, 'tag', 'v1.2.3')
            )) {
            $result = Invoke-NativeCapture 'git' $arguments
            Assert-Equal 0 $result.ExitCode "git $($arguments -join ' ')"
        }
        $releaseCommit = (Invoke-NativeCapture 'git' @('-C', $fixtureRoot, 'rev-parse', 'HEAD')).Output

        [System.IO.File]::WriteAllText((Join-Path $fixtureRoot 'after.txt'), 'after release')
        foreach ($arguments in @(
                @('-C', $fixtureRoot, 'add', 'after.txt'),
                @('-C', $fixtureRoot, 'commit', '--quiet', '--message', 'after release')
            )) {
            $result = Invoke-NativeCapture 'git' $arguments
            Assert-Equal 0 $result.ExitCode "git $($arguments -join ' ')"
        }

        $script:RELEASE_REPO_ROOT = $fixtureRoot
        $original = Get-ReleaseCheckoutState
        Switch-ToReleaseCandidate -Candidate ([pscustomobject] @{
                Tag = 'v1.2.3'
                Commit = $releaseCommit
            })
        Restore-ReleaseCheckout -CheckoutState $original

        $branch = (Invoke-NativeCapture 'git' @('-C', $fixtureRoot, 'branch', '--show-current')).Output
        $commit = (Invoke-NativeCapture 'git' @('-C', $fixtureRoot, 'rev-parse', 'HEAD')).Output
        Assert-Equal 'main' $branch 'restored branch'
        Assert-Equal $original.Commit $commit 'restored commit'

        Assert-ReleaseFullyClean
        [System.IO.File]::WriteAllText((Join-Path $fixtureRoot 'untracked.txt'), 'untracked')
        $threw = $false
        try {
            Assert-ReleaseFullyClean
        }
        catch {
            $threw = $true
        }
        Assert-True $threw 'untracked files should block checkout automation'
    }
    finally {
        Remove-Item -LiteralPath $fixtureRoot -Recurse -Force -ErrorAction SilentlyContinue
    }
}

Write-Host "[release-windows-tests] $($script:TestsPassed) passed, $($script:TestsFailed) failed"
if ($script:TestsFailed -ne 0) {
    exit 1
}
