param(
    [Parameter(Mandatory = $true)]
    [ValidatePattern('^https://')]
    [string]$UpdateUrl,

    [ValidatePattern('^[A-Za-z0-9._-]+$')]
    [string]$Channel = 'win',

    [string]$OutputDir = 'Releases'
)

$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
$manifest = Get-Content (Join-Path $repo 'Cargo.toml') -Raw
$versionMatch = [regex]::Match(
    $manifest,
    '(?ms)\[workspace\.package\].*?^version\s*=\s*"([^"]+)"'
)
if (-not $versionMatch.Success) {
    throw 'Could not read workspace.package.version from Cargo.toml.'
}
$version = $versionMatch.Groups[1].Value

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    throw 'cargo is required.'
}
if (-not (Get-Command vpk -ErrorAction SilentlyContinue)) {
    throw 'vpk is required. Install .NET SDK 8, then run: dotnet tool install -g vpk'
}

$stage = Join-Path $repo 'target\velopack-stage'
$output = if ([IO.Path]::IsPathRooted($OutputDir)) {
    $OutputDir
} else {
    Join-Path $repo $OutputDir
}

Remove-Item $stage -Recurse -Force -ErrorAction SilentlyContinue
New-Item $stage -ItemType Directory | Out-Null
New-Item $output -ItemType Directory -Force | Out-Null

try {
    $env:CODEXBAR_UPDATE_URL = $UpdateUrl
    & cargo build --release -p codexbar-app
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed with exit code $LASTEXITCODE" }

    Copy-Item (Join-Path $repo 'target\release\codexbar-app.exe') $stage

    & vpk pack `
        --packId app.codexbar.windows `
        --packVersion $version `
        --packDir $stage `
        --mainExe codexbar-app.exe `
        --packTitle CodexBar `
        --packAuthors 'CodexBar contributors' `
        --icon (Join-Path $repo 'src-tauri\icons\icon.ico') `
        --aumid app.codexbar.windows `
        --shortcuts StartMenuRoot `
        --channel $Channel `
        --outputDir $output
    if ($LASTEXITCODE -ne 0) { throw "vpk pack failed with exit code $LASTEXITCODE" }
} finally {
    Remove-Item $stage -Recurse -Force -ErrorAction SilentlyContinue
}
