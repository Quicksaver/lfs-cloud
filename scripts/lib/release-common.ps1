#Requires -Version 7.0

Set-StrictMode -Version Latest

. (Join-Path $PSScriptRoot 'terminal-ui.ps1')

$script:LOCAL_WINDOWS_STATUS_CONTEXT = 'local-checks/windows-x86_64'
$script:RELEASE_REPO_ROOT = ''
$script:RELEASE_BRANCH = ''
$script:RELEASE_SHA = ''
$script:RELEASE_GITHUB_REPO = ''
$script:RELEASE_GITHUB_LOGIN = ''
$script:RELEASE_UI_INITIALIZED = $false

function Initialize-ReleaseUi {
    param(
        [Parameter(Mandatory = $true)] [string] $Prefix,
        [Parameter(Mandatory = $true)] [string] $Section
    )

    ui_set_prefix $Prefix
    ui_set_render_mode 'task_only'
    ui_init
    ui_set_live_section_running $Section
    $script:RELEASE_UI_INITIALIZED = $true
}

function Complete-ReleaseUi {
    if ($script:RELEASE_UI_INITIALIZED) {
        ui_finalize
        $script:RELEASE_UI_INITIALIZED = $false
    }
}

function Invoke-ReleaseStep {
    param(
        [Parameter(Mandatory = $true)] [string] $Message,
        [Parameter(Mandatory = $true)] [string] $Command,
        [string[]] $Arguments = @()
    )

    ui_set_live_task_state 'running' $Message
    if (ui_run_with_live_stdout $Command $Arguments) {
        ui_set_live_task_state 'pass' $Message
        ui_clear_live_task
        pass $Message
        return
    }

    $failureOutput = @(
        ui_get_live_output_tail_lines |
            ForEach-Object { ui_strip_ansi $_ }
    )
    ui_set_live_task_state 'fail' $Message
    ui_clear_live_task
    fail $Message

    $failureMessage = "Verification step failed: $Message"
    if ($failureOutput.Count -gt 0) {
        $failureMessage += "`n$($failureOutput -join "`n")"
    }
    throw $failureMessage
}

function Invoke-ReleaseActionStep {
    param(
        [Parameter(Mandatory = $true)] [string] $Message,
        [Parameter(Mandatory = $true)] [scriptblock] $Action
    )

    ui_set_live_task_state 'running' $Message
    try {
        & $Action
        ui_set_live_task_state 'pass' $Message
        ui_clear_live_task
        pass $Message
    }
    catch {
        ui_set_live_task_state 'fail' $Message
        ui_clear_live_task
        fail $Message
        throw
    }
}

function Write-ReleaseInfo {
    param([Parameter(Mandatory = $true)] [string] $Message)
    info $Message
}

function Write-ReleasePass {
    param([Parameter(Mandatory = $true)] [string] $Message)
    pass $Message
}

function Write-ReleaseWarning {
    param([Parameter(Mandatory = $true)] [string] $Message)
    warn $Message
}

function Assert-ReleaseCommand {
    param([Parameter(Mandatory = $true)] [string] $Name)

    if (-not (Get-Command $Name -ErrorAction SilentlyContinue)) {
        throw "Required command is unavailable: $Name"
    }
}

function Invoke-NativeCapture {
    param(
        [Parameter(Mandatory = $true)] [string] $Command,
        [string[]] $Arguments = @()
    )

    # Native stderr is expected for probes such as `cargo audit --version`.
    # Keep the probe result explicit even when the caller enabled native-error
    # promotion in its own PowerShell session.
    $PSNativeCommandUseErrorActionPreference = $false
    $output = @(& $Command @Arguments 2>&1 | ForEach-Object { $_.ToString() })
    $exitCode = if ($null -eq $LASTEXITCODE) { 0 } else { [int] $LASTEXITCODE }

    return [pscustomobject] @{
        ExitCode = $exitCode
        Output = ($output -join "`n").Trim()
    }
}

function ConvertTo-GitHubRepositorySlug {
    param([Parameter(Mandatory = $true)] [string] $OriginUrl)

    $path = $null
    foreach ($prefix in @(
            'git@github.com:',
            'ssh://git@github.com/',
            'https://github.com/',
            'http://github.com/',
            'git://github.com/'
        )) {
        if ($OriginUrl.StartsWith($prefix, [System.StringComparison]::OrdinalIgnoreCase)) {
            $path = $OriginUrl.Substring($prefix.Length)
            break
        }
    }

    if ([string]::IsNullOrWhiteSpace($path)) {
        return $null
    }

    if ($path.EndsWith('.git', [System.StringComparison]::OrdinalIgnoreCase)) {
        $path = $path.Substring(0, $path.Length - 4)
    }

    $segments = @($path -split '/')
    if ($segments.Count -ne 2) {
        return $null
    }

    if ($segments[0] -notmatch '^[^@\s/:?#]+$' -or $segments[1] -notmatch '^[^@\s/:?#]+$') {
        return $null
    }

    return "$($segments[0])/$($segments[1])"
}

function Initialize-Release {
    param(
        [Parameter(Mandatory = $true)] [string] $StartDirectory,
        [switch] $AllowDetachedHead
    )

    Assert-ReleaseCommand 'git'
    Assert-ReleaseCommand 'gh'

    $rootResult = Invoke-NativeCapture 'git' @('-C', $StartDirectory, 'rev-parse', '--show-toplevel')
    if ($rootResult.ExitCode -ne 0 -or [string]::IsNullOrWhiteSpace($rootResult.Output)) {
        throw 'Could not resolve the repository root.'
    }
    $script:RELEASE_REPO_ROOT = $rootResult.Output

    $authResult = Invoke-NativeCapture 'gh' @('auth', 'status', '--hostname', 'github.com')
    if ($authResult.ExitCode -ne 0) {
        throw "GitHub CLI is not authenticated. Run 'gh auth login' and retry."
    }

    $originResult = Invoke-NativeCapture 'git' @(
        '-C',
        $script:RELEASE_REPO_ROOT,
        'config',
        '--get',
        'remote.origin.url'
    )
    if ($originResult.ExitCode -ne 0) {
        throw 'Could not read the origin remote.'
    }

    $script:RELEASE_GITHUB_REPO = ConvertTo-GitHubRepositorySlug $originResult.Output
    if ([string]::IsNullOrWhiteSpace($script:RELEASE_GITHUB_REPO)) {
        throw 'The origin remote is not a supported GitHub repository URL.'
    }

    $loginResult = Invoke-NativeCapture 'gh' @('api', 'user', '--jq', '.login')
    if ($loginResult.ExitCode -ne 0 -or [string]::IsNullOrWhiteSpace($loginResult.Output)) {
        throw 'Could not resolve the authenticated GitHub login.'
    }
    $script:RELEASE_GITHUB_LOGIN = $loginResult.Output

    $branchResult = Invoke-NativeCapture 'git' @(
        '-C',
        $script:RELEASE_REPO_ROOT,
        'symbolic-ref',
        '--quiet',
        '--short',
        'HEAD'
    )
    $script:RELEASE_BRANCH = ''
    if (($branchResult.ExitCode -ne 0 -or [string]::IsNullOrWhiteSpace($branchResult.Output)) -and
        -not $AllowDetachedHead) {
        throw 'A local branch must be checked out; detached HEAD is not supported.'
    }
    if ($branchResult.ExitCode -eq 0 -and -not [string]::IsNullOrWhiteSpace($branchResult.Output)) {
        $script:RELEASE_BRANCH = $branchResult.Output
    }

    $shaResult = Invoke-NativeCapture 'git' @('-C', $script:RELEASE_REPO_ROOT, 'rev-parse', 'HEAD')
    if ($shaResult.ExitCode -ne 0 -or $shaResult.Output -notmatch '^[0-9a-f]{40}$') {
        throw 'Could not resolve the current commit.'
    }
    $script:RELEASE_SHA = $shaResult.Output
}

function Assert-ReleaseTrackedClean {
    $worktreeResult = Invoke-NativeCapture 'git' @(
        '-C',
        $script:RELEASE_REPO_ROOT,
        'diff',
        '--quiet',
        '--ignore-submodules',
        '--'
    )
    if ($worktreeResult.ExitCode -ne 0) {
        throw 'Tracked working-tree changes must be committed before continuing.'
    }

    $stagedResult = Invoke-NativeCapture 'git' @(
        '-C',
        $script:RELEASE_REPO_ROOT,
        'diff',
        '--cached',
        '--quiet',
        '--ignore-submodules',
        '--'
    )
    if ($stagedResult.ExitCode -ne 0) {
        throw 'Staged changes must be committed before continuing.'
    }
}

function Assert-ReleaseFullyClean {
    $statusResult = Invoke-NativeCapture 'git' @(
        '-C',
        $script:RELEASE_REPO_ROOT,
        'status',
        '--porcelain=v1',
        '--untracked-files=all'
    )
    if ($statusResult.ExitCode -ne 0) {
        throw 'Could not inspect the working tree.'
    }
    if (-not [string]::IsNullOrWhiteSpace($statusResult.Output)) {
        throw 'The working tree must be completely clean before continuing.'
    }
}

function Get-RemoteReleaseTagCommit {
    param([Parameter(Mandatory = $true)] [string] $Tag)

    $peeledReference = "refs/tags/{0}^{{}}" -f $Tag
    $result = Invoke-NativeCapture 'git' @(
        '-C',
        $script:RELEASE_REPO_ROOT,
        'ls-remote',
        '--tags',
        'origin',
        "refs/tags/$Tag",
        $peeledReference
    )
    if ($result.ExitCode -ne 0) {
        throw "Could not read release tag $Tag from origin."
    }

    $directCommit = ''
    $peeledCommit = ''
    foreach ($line in @($result.Output -split "`n")) {
        if ($line -notmatch '^([0-9a-f]{40})\s+(.+)$') {
            continue
        }

        if ($Matches[2] -eq $peeledReference) {
            $peeledCommit = $Matches[1]
        }
        elseif ($Matches[2] -eq "refs/tags/$Tag") {
            $directCommit = $Matches[1]
        }
    }

    if (-not [string]::IsNullOrWhiteSpace($peeledCommit)) {
        return $peeledCommit
    }
    return $directCommit
}

function Assert-ReleaseCurrentCommitForTag {
    param([Parameter(Mandatory = $true)] [string] $Tag)

    $localResult = Invoke-NativeCapture 'git' @(
        '-C',
        $script:RELEASE_REPO_ROOT,
        'rev-list',
        '-n',
        '1',
        "refs/tags/$Tag"
    )
    if ($localResult.ExitCode -ne 0 -or $localResult.Output -ne $script:RELEASE_SHA) {
        throw "Current commit $($script:RELEASE_SHA) is not local release tag $Tag."
    }

    $remoteCommit = Get-RemoteReleaseTagCommit -Tag $Tag
    if ([string]::IsNullOrWhiteSpace($remoteCommit) -or $remoteCommit -ne $script:RELEASE_SHA) {
        if ([string]::IsNullOrWhiteSpace($remoteCommit)) {
            $remoteCommit = 'missing'
        }
        throw "Current commit $($script:RELEASE_SHA) is not origin release tag $Tag ($remoteCommit)."
    }

    $githubResult = Invoke-NativeCapture 'gh' @(
        'api',
        "repos/$($script:RELEASE_GITHUB_REPO)/commits/$($script:RELEASE_SHA)",
        '--jq',
        '.sha'
    )
    if ($githubResult.ExitCode -ne 0 -or $githubResult.Output -ne $script:RELEASE_SHA) {
        throw "GitHub does not report release commit $($script:RELEASE_SHA) for $($script:RELEASE_GITHUB_REPO)."
    }

    Write-ReleasePass "Current commit is release tag $Tag on origin"
}

function Assert-ReleaseCurrentCommitOnOrigin {
    $remoteResult = Invoke-NativeCapture 'git' @(
        '-C',
        $script:RELEASE_REPO_ROOT,
        'ls-remote',
        '--heads',
        'origin',
        "refs/heads/$($script:RELEASE_BRANCH)"
    )
    if ($remoteResult.ExitCode -ne 0) {
        throw "Could not read origin/$($script:RELEASE_BRANCH)."
    }

    $remoteSha = ($remoteResult.Output -split '\s+')[0]
    if ($remoteSha -ne $script:RELEASE_SHA) {
        if ([string]::IsNullOrWhiteSpace($remoteSha)) {
            $remoteSha = 'missing'
        }
        throw "Current commit $($script:RELEASE_SHA) is not exactly origin/$($script:RELEASE_BRANCH) ($remoteSha)."
    }

    $githubResult = Invoke-NativeCapture 'gh' @(
        'api',
        "repos/$($script:RELEASE_GITHUB_REPO)/commits/$($script:RELEASE_SHA)",
        '--jq',
        '.sha'
    )
    if ($githubResult.ExitCode -ne 0 -or $githubResult.Output -ne $script:RELEASE_SHA) {
        throw "GitHub does not report the current commit for $($script:RELEASE_GITHUB_REPO)."
    }

    Write-ReleasePass "Current commit is pushed to origin/$($script:RELEASE_BRANCH)"
}

function Set-ReleaseStatus {
    param(
        [Parameter(Mandatory = $true)] [string] $Commit,
        [Parameter(Mandatory = $true)] [string] $Context,
        [Parameter(Mandatory = $true)]
        [ValidateSet('error', 'failure', 'pending', 'success')]
        [string] $State,
        [Parameter(Mandatory = $true)] [string] $Description
    )

    $result = Invoke-NativeCapture 'gh' @(
        'api',
        '--method',
        'POST',
        "repos/$($script:RELEASE_GITHUB_REPO)/statuses/$Commit",
        '--raw-field',
        "state=$State",
        '--raw-field',
        "context=$Context",
        '--raw-field',
        "description=$Description",
        '--silent'
    )
    if ($result.ExitCode -ne 0) {
        throw "Failed to record GitHub commit status '$Context' as '$State'."
    }
}

function Get-ReleaseVersions {
    param([Parameter(Mandatory = $true)] [string] $RepositoryRoot)

    $cargoPath = Join-Path $RepositoryRoot 'Cargo.toml'
    $packagePath = Join-Path $RepositoryRoot 'package.json'
    $cargoVersion = ''
    $inPackage = $false

    foreach ($line in Get-Content -LiteralPath $cargoPath) {
        if ($line -eq '[package]') {
            $inPackage = $true
            continue
        }
        if ($inPackage -and $line.StartsWith('[')) {
            break
        }
        if ($inPackage -and $line -match '^version\s*=\s*"([^"]+)"\s*$') {
            $cargoVersion = $Matches[1]
            break
        }
    }

    $packageJson = Get-Content -Raw -LiteralPath $packagePath | ConvertFrom-Json
    $packageVersion = if ($null -eq $packageJson.version) { '' } else { [string] $packageJson.version }

    return [pscustomobject] @{
        Cargo = $cargoVersion
        Package = $packageVersion
    }
}

function Get-MatchingReleaseVersion {
    param([Parameter(Mandatory = $true)] [string] $RepositoryRoot)

    $versions = Get-ReleaseVersions -RepositoryRoot $RepositoryRoot
    if ([string]::IsNullOrWhiteSpace($versions.Cargo) -or $versions.Cargo -ne $versions.Package) {
        throw "Cargo.toml version '$($versions.Cargo)' and package.json version '$($versions.Package)' must match."
    }

    return $versions.Cargo
}

function Get-WindowsArtifactPath {
    param(
        [Parameter(Mandatory = $true)] [string] $RepositoryRoot,
        [Parameter(Mandatory = $true)] [string] $Version
    )

    return Join-Path $RepositoryRoot 'dist' "lfscloud-v$Version-windows-x86_64.tar.gz"
}

function Get-WindowsManifestPath {
    param(
        [Parameter(Mandatory = $true)] [string] $RepositoryRoot,
        [Parameter(Mandatory = $true)] [string] $Version
    )

    return Join-Path $RepositoryRoot 'dist' "lfscloud-v$Version-windows-x86_64.build.json"
}

function Write-ArtifactChecksum {
    param([Parameter(Mandatory = $true)] [string] $ArtifactPath)

    $digest = (Get-FileHash -LiteralPath $ArtifactPath -Algorithm SHA256).Hash.ToLowerInvariant()
    $checksumPath = "$ArtifactPath.sha256"
    $contents = "$digest  $([System.IO.Path]::GetFileName($ArtifactPath))`n"
    [System.IO.File]::WriteAllText($checksumPath, $contents, [System.Text.UTF8Encoding]::new($false))
}

function Test-ArtifactChecksum {
    param([Parameter(Mandatory = $true)] [string] $ArtifactPath)

    $checksumPath = "$ArtifactPath.sha256"
    if (-not (Test-Path -LiteralPath $ArtifactPath -PathType Leaf) -or
        -not (Test-Path -LiteralPath $checksumPath -PathType Leaf)) {
        return $false
    }
    if ((Get-Item -LiteralPath $ArtifactPath).Length -eq 0 -or
        (Get-Item -LiteralPath $checksumPath).Length -eq 0) {
        return $false
    }

    $expected = (Get-Content -Raw -LiteralPath $checksumPath).Trim()
    $digest = (Get-FileHash -LiteralPath $ArtifactPath -Algorithm SHA256).Hash.ToLowerInvariant()
    $expectedLine = "$digest  $([System.IO.Path]::GetFileName($ArtifactPath))"
    return $expected -eq $expectedLine
}

function Write-WindowsBuildManifest {
    param(
        [Parameter(Mandatory = $true)] [string] $ArtifactPath,
        [Parameter(Mandatory = $true)] [string] $ManifestPath,
        [Parameter(Mandatory = $true)] [string] $Version,
        [Parameter(Mandatory = $true)] [string] $Commit,
        [Parameter(Mandatory = $true)] [string] $WindowsVersion,
        [Parameter(Mandatory = $true)] [string] $RustcVersion
    )

    $digest = (Get-FileHash -LiteralPath $ArtifactPath -Algorithm SHA256).Hash.ToLowerInvariant()
    $manifest = [ordered] @{
        schema_version = 1
        artifact = [System.IO.Path]::GetFileName($ArtifactPath)
        commit = $Commit
        version = $Version
        target = 'x86_64-pc-windows-msvc'
        windows = $WindowsVersion
        rustc = $RustcVersion
        sha256 = $digest
    }
    $contents = ($manifest | ConvertTo-Json) + "`n"
    [System.IO.File]::WriteAllText($ManifestPath, $contents, [System.Text.UTF8Encoding]::new($false))
}

function Test-WindowsBuildManifest {
    param(
        [Parameter(Mandatory = $true)] [string] $ArtifactPath,
        [Parameter(Mandatory = $true)] [string] $ManifestPath,
        [Parameter(Mandatory = $true)] [string] $Version,
        [Parameter(Mandatory = $true)] [string] $Commit
    )

    if (-not (Test-Path -LiteralPath $ArtifactPath -PathType Leaf) -or
        -not (Test-Path -LiteralPath $ManifestPath -PathType Leaf)) {
        return $false
    }
    if ((Get-Item -LiteralPath $ArtifactPath).Length -eq 0 -or
        (Get-Item -LiteralPath $ManifestPath).Length -eq 0) {
        return $false
    }

    try {
        $manifest = Get-Content -Raw -LiteralPath $ManifestPath | ConvertFrom-Json
        $requiredProperties = @(
            'schema_version',
            'artifact',
            'commit',
            'version',
            'target',
            'windows',
            'rustc',
            'sha256'
        )
        foreach ($property in $requiredProperties) {
            if ($manifest.PSObject.Properties.Name -notcontains $property) {
                return $false
            }
        }

        $digest = (Get-FileHash -LiteralPath $ArtifactPath -Algorithm SHA256).Hash.ToLowerInvariant()
        return (
            $manifest.schema_version -eq 1 -and
            $manifest.artifact -eq [System.IO.Path]::GetFileName($ArtifactPath) -and
            $manifest.commit -eq $Commit -and
            $manifest.version -eq $Version -and
            $manifest.target -eq 'x86_64-pc-windows-msvc' -and
            -not [string]::IsNullOrWhiteSpace([string] $manifest.windows) -and
            -not [string]::IsNullOrWhiteSpace([string] $manifest.rustc) -and
            $manifest.sha256 -eq $digest
        )
    }
    catch {
        return $false
    }
}
