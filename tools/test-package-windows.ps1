$ErrorActionPreference = 'Stop'
$fixture = Join-Path ([IO.Path]::GetTempPath()) ('codexbar-package-test-' + [guid]::NewGuid().ToString('N'))
$previousUpdateUrl = $env:CODEXBAR_UPDATE_URL
$previousLocation = Get-Location
$scriptPath = Join-Path $fixture 'tools/package-windows.ps1'

function Assert([bool]$Condition, [string]$Message) {
    if (-not $Condition) { throw $Message }
}
function Expect-Failure([scriptblock]$Action, [string]$Message) {
    $caught = $null
    try { & $Action } catch { $caught = $_ }
    Assert ($null -ne $caught -and $caught.ToString().Contains($Message)) "Expected failure: $Message"
}
function cargo {
    $global:packageTestBuilds++
    $global:LASTEXITCODE = $global:packageTestBuildExit
}
function vpk {
    $global:packageTestPackArgs = @($args)
    $global:LASTEXITCODE = $global:packageTestPackExit
}

try {
    New-Item -ItemType Directory -Path (Join-Path $fixture 'tools'), (Join-Path $fixture 'target/release'), (Join-Path $fixture 'target/velopack-stage') | Out-Null
    Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'package-windows.ps1') -Destination $scriptPath
    Set-Content -LiteralPath (Join-Path $fixture 'Cargo.toml') -Value "[workspace.package]`nversion = `"0.1.0`""
    Set-Content -LiteralPath (Join-Path $fixture 'target/release/codexbar-app.exe') -Value 'fixture'
    Set-Content -LiteralPath (Join-Path $fixture 'target/velopack-stage/keep.txt') -Value 'existing data'
    Set-Content -LiteralPath (Join-Path $fixture 'test certificate.pfx') -Value 'fixture'
    Set-Location -LiteralPath $fixture
    $env:CODEXBAR_UPDATE_URL = 'https://example.invalid/original'
    $global:packageTestBuilds = 0
    $global:packageTestBuildExit = 0
    $global:packageTestPackExit = 0
    $options = @{ UpdateUrl = 'http://127.0.0.1:18764/'; SigningPfx = ''; SigningPassword = '' }

    Expect-Failure { & $scriptPath @options -RequireSignature } 'Signing is required'
    Expect-Failure { & $scriptPath -UpdateUrl $options.UpdateUrl -SigningPfx 'missing.pfx' -SigningPassword 'test' } 'certificate not found'
    Expect-Failure { & $scriptPath -UpdateUrl $options.UpdateUrl -SigningPfx 'test certificate.pfx' -SigningPassword '' } 'PASSWORD is required'
    Expect-Failure { & $scriptPath -UpdateUrl 'http://untrusted.example/' } 'UpdateUrl'
    Assert ($global:packageTestBuilds -eq 0) 'Invalid inputs must fail before building'

    & $scriptPath @options
    Assert (-not ($global:packageTestPackArgs -contains '--signParams')) 'Unsigned builds must omit signing arguments'
    Assert ($env:CODEXBAR_UPDATE_URL -eq 'https://example.invalid/original') 'Update URL must be restored'
    Assert (Test-Path -LiteralPath 'target/velopack-stage/keep.txt') 'Existing staging data must survive'
    Assert (@(Get-ChildItem -LiteralPath 'target' -Directory -Filter 'velopack-stage-*').Count -eq 0) 'Temporary staging must be removed'

    & $scriptPath -UpdateUrl $options.UpdateUrl -SigningPfx 'test certificate.pfx' -SigningPassword 'test"quote\' -RequireSignature
    $signIndex = [array]::IndexOf($global:packageTestPackArgs, '--signParams')
    $signParams = $global:packageTestPackArgs[$signIndex + 1]
    Assert ($signParams.Contains((Join-Path $fixture 'test certificate.pfx'))) 'Certificate path must be absolute'
    Assert ($signParams.Contains('/p "test\"quote\\"')) 'Quotes and trailing backslashes must be escaped'

    $global:packageTestBuildExit = 7
    Expect-Failure { & $scriptPath @options } 'cargo build failed with exit code 7'
    $global:packageTestBuildExit = 0
    $global:packageTestPackExit = 8
    Expect-Failure { & $scriptPath @options } 'vpk pack failed with exit code 8'
    Assert ($env:CODEXBAR_UPDATE_URL -eq 'https://example.invalid/original') 'Failures must restore the update URL'
    Assert (@(Get-ChildItem -LiteralPath 'target' -Directory -Filter 'velopack-stage-*').Count -eq 0) 'Failures must remove temporary staging'
    Write-Output 'PASS: input validation, unsigned/signed arguments, staging preservation, environment restoration, and build/pack failures'
} finally {
    Set-Location $previousLocation
    $env:CODEXBAR_UPDATE_URL = $previousUpdateUrl
    $resolved = [IO.Path]::GetFullPath($fixture)
    $tempRoot = [IO.Path]::GetFullPath([IO.Path]::GetTempPath()).TrimEnd('\') + '\'
    if ($resolved.StartsWith($tempRoot, [StringComparison]::OrdinalIgnoreCase)) {
        Remove-Item -LiteralPath $resolved -Recurse -Force
    }
    Remove-Variable -Name packageTestBuilds, packageTestBuildExit, packageTestPackExit, packageTestPackArgs -Scope Global -ErrorAction SilentlyContinue
}
