param([switch]$SkipBuild, [string]$Executable)
$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path $PSScriptRoot -Parent
Push-Location $projectRoot
try {
    $version = [regex]::Match((Get-Content Cargo.toml -Raw), '(?m)^version = "([^"]+)"').Groups[1].Value
    if (-not $SkipBuild) {
        $env:CARGO_BUILD_JOBS = '1'
        cargo build --locked --release --bin daozai
        if ($LASTEXITCODE -ne 0) { throw 'Release build failed' }
    }
    if (-not $Executable) {
        $targetDirectory = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $projectRoot 'target' }
        $Executable = Join-Path $targetDirectory 'release\daozai.exe'
    }
    $binary = Get-Item -LiteralPath $Executable
    if ($binary.VersionInfo.FileVersion -ne $version -and $binary.VersionInfo.FileVersion -ne "$version.0") { throw 'Executable version mismatch' }
    $packageName = "Daozai-v$version-windows-x64"
    $outputDirectory = Join-Path $projectRoot 'dist'
    $stagingDirectory = Join-Path $outputDirectory $packageName
    if (Test-Path -LiteralPath $stagingDirectory) { throw 'Package directory already exists' }
    New-Item -ItemType Directory -Path $stagingDirectory -Force | Out-Null
    Copy-Item -LiteralPath $binary.FullName -Destination (Join-Path $stagingDirectory 'daozai.exe')
    @('installer','docs','README.md','LICENSE','THIRD_PARTY_NOTICES.txt','CHANGELOG.md','CONTRIBUTING.md','SECURITY.md','安装岛仔.cmd') | ForEach-Object {
        Copy-Item -LiteralPath (Join-Path $projectRoot $_) -Destination $stagingDirectory -Recurse
    }
    New-Item -ItemType Directory -Path (Join-Path $stagingDirectory 'assets') | Out-Null
    Copy-Item -LiteralPath 'assets\daozai-logo.png' -Destination (Join-Path $stagingDirectory 'assets')
    $archive = Join-Path $outputDirectory "$packageName.zip"
    Compress-Archive -LiteralPath $stagingDirectory -DestinationPath $archive
    $checksum = (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLowerInvariant()
    "$checksum  $packageName.zip" | Set-Content -LiteralPath (Join-Path $outputDirectory 'SHA256SUMS.txt') -Encoding ascii
    Write-Output $archive
} finally { Pop-Location }
