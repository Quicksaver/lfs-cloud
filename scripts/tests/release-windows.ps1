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

Invoke-Test 'Multi-select accepts all candidates' {
    $selection = @(ConvertTo-ReleaseSelection -Selection 'all' -CandidateCount 4)
    Assert-Equal '0,1,2,3' ($selection -join ',') 'all selection'
}

Invoke-Test 'Multi-select combines ranges and removes duplicates' {
    $selection = @(ConvertTo-ReleaseSelection -Selection '1, 3-5, 3' -CandidateCount 5)
    Assert-Equal '0,2,3,4' ($selection -join ',') 'range selection'
}

Invoke-Test 'Multi-select rejects out-of-range choices' {
    $threw = $false
    try {
        [void] @(ConvertTo-ReleaseSelection -Selection '1,4' -CandidateCount 3)
    }
    catch {
        $threw = $true
    }
    Assert-True $threw 'out-of-range selection should fail'
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

    $missing = Get-WindowsVerificationStatusStateFromDocument -Document ([pscustomobject] @{ statuses = @() })
    Assert-Equal 'missing' $missing 'missing Windows status'
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
