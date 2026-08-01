#Requires -Version 7.0

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$installer = Join-Path $PSScriptRoot '..' 'install.ps1'
. $installer

if ((Get-LfsCloudInstallVersion -RequestedVersion '1.2.3' -Repository 'fixture/repo') -ne '1.2.3') {
    throw 'Explicit semantic version was not preserved.'
}
$invalidFailed = $false
try {
    Get-LfsCloudInstallVersion -RequestedVersion '1.2' -Repository 'fixture/repo'
}
catch {
    $invalidFailed = $true
}
if (-not $invalidFailed) {
    throw 'Invalid semantic version was accepted.'
}

$fixtureRoot = Join-Path ([System.IO.Path]::GetTempPath()) "lfscloud-installer-ps-$([guid]::NewGuid().ToString('N'))"
[void][System.IO.Directory]::CreateDirectory($fixtureRoot)
try {
    $receipt = Join-Path $fixtureRoot '.lfscloud-direct-install'
    [System.IO.File]::WriteAllText($receipt, "version=1.2.3`nsource=direct`n")
    if (-not (Test-LfsCloudDirectInstallReceipt -ReceiptPath $receipt)) {
        throw 'Direct-install receipt was not recognized.'
    }
    [System.IO.File]::WriteAllText($receipt, "source=package-manager`n")
    if (Test-LfsCloudDirectInstallReceipt -ReceiptPath $receipt) {
        throw 'Unmanaged receipt was recognized as direct.'
    }
}
finally {
    Remove-Item -LiteralPath $fixtureRoot -Recurse -Force -ErrorAction SilentlyContinue
}

Write-Host '[install-powershell-tests] 4 passed, 0 failed'
