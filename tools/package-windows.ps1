param(
    # HTTPS for anything shippable. Loopback HTTP is allowed only so the install/upgrade/
    # rollback loop can be exercised against a local feed before a real host exists; the URL
    # is compiled into the binary, so a test build can never be mistaken for a release one.
    [Parameter(Mandatory = $true)]
    [ValidatePattern('^(https://|http://(127\.0\.0\.1|localhost)(:\d+)?(/|$))')]
    [string]$UpdateUrl,

    [ValidatePattern('^[A-Za-z0-9._-]+$')]
    [string]$Channel = 'win',

    [string]$OutputDir = 'Releases',

    # Uploads the packed artifacts to GitHub Releases. The token comes from the environment
    # (GITHUB_TOKEN, or gh's own store) so it never reaches the command line or shell history.
    [switch]$Publish,

    # Publishes the release immediately instead of leaving it as a draft. A draft is
    # invisible to the updater, so this is what actually opens the feed to installed apps.
    [switch]$NoDraft
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

    if ($Publish) {
        if ($UpdateUrl -notmatch '^https://github\.com/[^/]+/[^/]+/?$') {
            throw "-Publish needs -UpdateUrl to be the repository URL, e.g. https://github.com/owner/repo. Got: $UpdateUrl"
        }
        $repoUrl = $UpdateUrl.TrimEnd('/')

        $token = $env:GITHUB_TOKEN
        if (-not $token -and (Get-Command gh -ErrorAction SilentlyContinue)) {
            $token = (& gh auth token 2>$null)
        }
        if (-not $token) {
            throw 'Set GITHUB_TOKEN, or sign in with `gh auth login`, before using -Publish.'
        }

        & vpk upload github `
            --outputDir $output `
            --channel $Channel `
            --repoUrl $repoUrl `
            --token $token `
            --tag "v$version" `
            --releaseName "CodexBar $version" `
            --publish ([bool]$NoDraft) `
            --merge $true
        if ($LASTEXITCODE -ne 0) { throw "vpk upload github failed with exit code $LASTEXITCODE" }
    }
} finally {
    Remove-Item $stage -Recurse -Force -ErrorAction SilentlyContinue
}
