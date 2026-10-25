# Makes the package to hand out: the port's own source and the build tool, and nothing from
# the game. Whoever gets it runs build.bat with their own disc image to make the game.
#
#   powershell -ExecutionPolicy Bypass -File tools\package.ps1 [-Out TSSM-Builder.zip]
#
# Left out: the recompiled game code (crates\game\src\gen), the extracted disc, disc images,
# saves, build output and caches, and this machine's settings (the package gets defaults).

param([string]$Out = "TSSM-Builder.zip")

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

$stage = Join-Path $env:TEMP "tssm-package"
if (Test-Path $stage) { Remove-Item -Recurse -Force $stage }
New-Item -ItemType Directory $stage | Out-Null

foreach ($file in @("build.bat", "Cargo.toml", "Cargo.lock", "rust-toolchain.toml", "CHANGELOG.md", "tools\build.ps1", "tools\README.txt")) {
    $target = Join-Path $stage $file
    New-Item -ItemType Directory -Force (Split-Path $target) | Out-Null
    Copy-Item $file $target
}
Move-Item (Join-Path $stage "tools\README.txt") (Join-Path $stage "README.txt")

# The crates' sources, without anything the recompiler writes.
$generated = (Resolve-Path "crates\game\src").Path + "\gen"
foreach ($file in Get-ChildItem "crates" -Recurse -File) {
    if ($file.FullName.StartsWith($generated)) { continue }
    $target = Join-Path $stage $file.FullName.Substring($root.Length + 1)
    New-Item -ItemType Directory -Force (Split-Path $target) | Out-Null
    Copy-Item $file.FullName $target
}

# Settings as a new player gets them.
$config = Join-Path $stage "config"
New-Item -ItemType Directory $config | Out-Null
foreach ($name in @("controls.txt", "hle.txt", "window.txt")) { Copy-Item "config\$name" $config }
Set-Content -Path (Join-Path $config "fps.txt") -Encoding ascii -Value @(
    '# Frames a second: "display" (the screen''s refresh rate), 60 (as on the console), or any',
    '# number from 20 to 1000. The game moves at the same speed whatever this is. Also set in',
    '# the game: Options, Options, Frame Rate.',
    'display')

if (Test-Path $Out) { Remove-Item -Force $Out }
Compress-Archive -Path (Join-Path $stage "*") -DestinationPath $Out
Remove-Item -Recurse -Force $stage
$size = (Get-Item $Out).Length / 1MB
Write-Host ("Made {0} ({1:N1} MB)" -f (Resolve-Path $Out), $size) -ForegroundColor Green
