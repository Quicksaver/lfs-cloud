#Requires -Version 7.0

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$publisher = Join-Path $PSScriptRoot '..' 'publish.ps1'
. $publisher

$document = [pscustomobject] @{
    statuses = @(
        [pscustomobject] @{
            id = 1
            context = $script:LOCAL_MACOS_STATUS_CONTEXT
            state = 'failure'
            creator = [pscustomobject] @{ login = 'fixture' }
        },
        [pscustomobject] @{
            id = 2
            context = $script:LOCAL_MACOS_STATUS_CONTEXT
            state = 'success'
            creator = [pscustomobject] @{ login = 'fixture' }
        }
    )
}
if ((Get-TrustedStatusState -Document $document -Context $script:LOCAL_MACOS_STATUS_CONTEXT -TrustedLogin fixture) -ne 'success') {
    throw 'Latest trusted status did not win.'
}
if ((Get-TrustedStatusState -Document $document -Context $script:LOCAL_MACOS_STATUS_CONTEXT -TrustedLogin other) -ne 'untrusted') {
    throw 'Unexpected status creator was trusted.'
}

$queue = [System.Collections.Generic.Queue[System.ConsoleKey]]::new()
$queue.Enqueue([System.ConsoleKey]::DownArrow)
$queue.Enqueue([System.ConsoleKey]::Enter)
$candidates = @(
    [pscustomobject] @{ Tag = 'v2.0.0' },
    [pscustomobject] @{ Tag = 'v1.0.0' }
)
$readKey = { $queue.Dequeue() }.GetNewClosure()
$render = { param($Items, $Index, $Redraw) }.GetNewClosure()
$selected = Read-PublishReleaseSelection `
    -Candidates $candidates `
    -ReadKey $readKey `
    -Render $render
if ($selected.Tag -ne 'v1.0.0') {
    throw 'Arrow-key publish selector returned the wrong candidate.'
}

$assets = @(Get-ExpectedReleaseAssetNames -Version '1.2.3')
foreach ($required in @(
        'lfscloud-v1.2.3-windows-x86_64.zip',
        'lfscloud_1.2.3_amd64.deb',
        'lfscloud_1.2.3_amd64.build.json',
        'lfscloud-installer.sh',
        'lfscloud-installer.ps1.sha256'
    )) {
    if ($assets -notcontains $required) {
        throw "Expected release asset was omitted: $required"
    }
}

$formula = New-HomebrewFormulaText `
    -Version '1.2.3' `
    -MacSha256 ('a' * 64) `
    -LinuxX64Sha256 ('b' * 64) `
    -LinuxArm64Sha256 ('c' * 64)
if (-not $formula.Contains('releases/download/v1.2.3/lfscloud-v1.2.3-macos-arm64.tar.gz') -or
    -not $formula.Contains('assert_equal "lfscloud #{version}"')) {
    throw 'Homebrew formula did not contain the expected versioned release contract.'
}

$fixtureRoot = Join-Path ([System.IO.Path]::GetTempPath()) "lfscloud-winget-test-$([guid]::NewGuid().ToString('N'))"
try {
    New-WinGetManifests `
        -Version '1.2.3' `
        -InstallerSha256 ('d' * 64) `
        -Directory $fixtureRoot
    $installerManifest = Get-Content -Raw -LiteralPath (Join-Path $fixtureRoot 'Quicksaver.LFSCloud.installer.yaml')
    if (-not $installerManifest.Contains('NestedInstallerType: portable') -or
        -not $installerManifest.Contains('PortableCommandAlias: lfscloud') -or
        -not $installerManifest.Contains(('D' * 64))) {
        throw 'WinGet portable manifest is incomplete.'
    }
}
finally {
    Remove-Item -LiteralPath $fixtureRoot -Recurse -Force -ErrorAction SilentlyContinue
}

Write-Host '[publish-release-tests] 9 passed, 0 failed'
