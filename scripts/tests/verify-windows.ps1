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
        Write-Host "[verify-windows-tests] + PASS $Name"
    }
    catch {
        $script:TestsFailed++
        Write-Host "[verify-windows-tests] x FAIL $Name"
        Write-Host "  $($_.Exception.Message)"
    }
}

$commonScript = Join-Path $PSScriptRoot '..' 'lib' 'release-common.ps1'
. $commonScript
$verifierScript = Join-Path $PSScriptRoot '..' 'local' 'verify-windows.ps1'
. $verifierScript

Invoke-Test 'Current host satisfies the Windows x86-64 preflight' {
    Assert-WindowsX64Host
}

Invoke-Test 'GitHub origin URLs resolve to owner/repository' {
    Assert-Equal 'owner/repo' (ConvertTo-GitHubRepositorySlug 'https://github.com/owner/repo.git') 'HTTPS origin'
    Assert-Equal 'owner/repo' (ConvertTo-GitHubRepositorySlug 'git@github.com:owner/repo.git') 'scp-style SSH origin'
    Assert-Equal 'owner/repo' (ConvertTo-GitHubRepositorySlug 'ssh://git@github.com/owner/repo.git') 'SSH origin'
}

Invoke-Test 'Unsupported origin URLs are rejected' {
    foreach ($origin in @(
            'https://gitlab.com/owner/repo.git',
            'https://token@github.com/owner/repo.git',
            'https://github.com/owner/repo/extra'
        )) {
        Assert-Equal $null (ConvertTo-GitHubRepositorySlug $origin) "unsupported origin $origin"
    }
}

Invoke-Test 'Native command probes preserve nonzero exit codes' {
    $result = Invoke-NativeCapture 'pwsh' @('-NoProfile', '-Command', 'Write-Output probe-output; exit 7')
    Assert-Equal 7 $result.ExitCode 'probe exit code'
    Assert-Equal 'probe-output' $result.Output 'probe output'
}

Invoke-Test 'Cargo and package versions are read independently' {
    $fixtureRoot = Join-Path ([System.IO.Path]::GetTempPath()) "lfscloud-version-test-$([guid]::NewGuid().ToString('N'))"
    [void][System.IO.Directory]::CreateDirectory($fixtureRoot)

    try {
        [System.IO.File]::WriteAllText(
            (Join-Path $fixtureRoot 'Cargo.toml'),
            "[package]`nname = `"fixture`"`nversion = `"1.2.3`"`n`n[dependencies]`n"
        )
        [System.IO.File]::WriteAllText(
            (Join-Path $fixtureRoot 'package.json'),
            "{`"name`":`"fixture`",`"version`":`"1.2.3`"}"
        )

        $versions = Get-ReleaseVersions -RepositoryRoot $fixtureRoot
        Assert-Equal '1.2.3' $versions.Cargo 'Cargo version'
        Assert-Equal '1.2.3' $versions.Package 'package.json version'
    }
    finally {
        Remove-Item -LiteralPath $fixtureRoot -Recurse -Force -ErrorAction SilentlyContinue
    }
}

Invoke-Test 'Windows release artifact uses the ZIP format' {
    $artifact = Get-WindowsArtifactPath -RepositoryRoot 'C:\fixture' -Version '1.2.3'
    Assert-Equal `
        'lfscloud-v1.2.3-windows-x86_64.zip' `
        ([System.IO.Path]::GetFileName($artifact)) `
        'Windows artifact name'
}

Invoke-Test 'Windows build manifest is bound to artifact, digest, version, and commit' {
    $fixtureRoot = Join-Path ([System.IO.Path]::GetTempPath()) "lfscloud-manifest-test-$([guid]::NewGuid().ToString('N'))"
    [void][System.IO.Directory]::CreateDirectory($fixtureRoot)

    try {
        $artifact = Join-Path $fixtureRoot 'lfscloud-v1.2.3-windows-x86_64.zip'
        $manifest = Join-Path $fixtureRoot 'lfscloud-v1.2.3-windows-x86_64.build.json'
        [System.IO.File]::WriteAllText($artifact, 'verified artifact bytes')
        $digest = (Get-FileHash -LiteralPath $artifact -Algorithm SHA256).Hash.ToLowerInvariant()
        @{
            schema_version = 1
            artifact = [System.IO.Path]::GetFileName($artifact)
            commit = 'abc123'
            version = '1.2.3'
            target = 'x86_64-pc-windows-msvc'
            windows = '10.0.26200'
            rustc = 'rustc 1.88.0'
            sha256 = $digest
        } | ConvertTo-Json | Set-Content -LiteralPath $manifest -Encoding utf8NoBOM

        Assert-True (Test-WindowsBuildManifest `
                -ArtifactPath $artifact `
                -ManifestPath $manifest `
                -Version '1.2.3' `
                -Commit 'abc123') 'valid manifest should pass'
        Assert-True (-not (Test-WindowsBuildManifest `
                    -ArtifactPath $artifact `
                    -ManifestPath $manifest `
                    -Version '1.2.3' `
                    -Commit 'different')) 'wrong commit should fail'
    }
    finally {
        Remove-Item -LiteralPath $fixtureRoot -Recurse -Force -ErrorAction SilentlyContinue
    }
}

Invoke-Test 'Windows package contains the verified binary and documentation' {
    $fixtureRoot = Join-Path ([System.IO.Path]::GetTempPath()) "lfscloud-package-test-$([guid]::NewGuid().ToString('N'))"
    [void][System.IO.Directory]::CreateDirectory((Join-Path $fixtureRoot 'docs'))

    try {
        foreach ($file in @(
                'LICENSE',
                'README.md',
                'docs/configuration.md',
                'docs/install-release.md',
                'lfscloud.exe'
            )) {
            $path = Join-Path $fixtureRoot $file
            [System.IO.File]::WriteAllText($path, "fixture $file")
        }

        $legacyArtifact = Join-Path $fixtureRoot 'dist/lfscloud-v1.2.3-windows-x86_64.tar.gz'
        [void][System.IO.Directory]::CreateDirectory((Split-Path -Parent $legacyArtifact))
        [System.IO.File]::WriteAllText($legacyArtifact, 'stale archive')
        [System.IO.File]::WriteAllText("$legacyArtifact.sha256", 'stale checksum')

        $packageStage = ''
        $package = New-WindowsReleasePackage `
            -RepositoryRoot $fixtureRoot `
            -ReleaseBinary (Join-Path $fixtureRoot 'lfscloud.exe') `
            -Version '1.2.3' `
            -Commit 'abc123' `
            -WindowsVersion '10.0.26200' `
            -RustcVersion 'rustc 1.88.0' `
            -PackageStage ([ref] $packageStage)

        Assert-True (Test-Path -LiteralPath $package.Artifact -PathType Leaf) 'archive should exist'
        Assert-True (Test-ArtifactChecksum -ArtifactPath $package.Artifact) 'checksum should validate'
        Assert-True (Test-WindowsBuildManifest `
                -ArtifactPath $package.Artifact `
                -ManifestPath $package.Manifest `
                -Version '1.2.3' `
                -Commit 'abc123') 'manifest should validate'
        Assert-True (-not (Test-Path -LiteralPath $legacyArtifact)) 'legacy tar archive should be removed'
        Assert-True (-not (Test-Path -LiteralPath "$legacyArtifact.sha256")) 'legacy tar checksum should be removed'

        $archive = [System.IO.Compression.ZipFile]::OpenRead($package.Artifact)
        try {
            $archiveEntries = @($archive.Entries | ForEach-Object { $_.FullName })
            foreach ($entry in @(
                    'lfscloud-v1.2.3-windows-x86_64/lfscloud.exe',
                    'lfscloud-v1.2.3-windows-x86_64/LICENSE',
                    'lfscloud-v1.2.3-windows-x86_64/README.md',
                    'lfscloud-v1.2.3-windows-x86_64/docs/configuration.md',
                    'lfscloud-v1.2.3-windows-x86_64/docs/install-release.md'
                )) {
                Assert-True ($archiveEntries -contains $entry) "archive should contain $entry"
            }
        }
        finally {
            $archive.Dispose()
        }
        Assert-Equal '' $packageStage 'package staging directory should be cleared'
    }
    finally {
        Remove-Item -LiteralPath $fixtureRoot -Recurse -Force -ErrorAction SilentlyContinue
    }
}

Write-Host "[verify-windows-tests] $($script:TestsPassed) passed, $($script:TestsFailed) failed"
if ($script:TestsFailed -ne 0) {
    exit 1
}
