# Builds the Windows port from a disc image, in one go.
#
#   powershell -ExecutionPolicy Bypass -File tools\build.ps1 [-Iso <image.iso>] [-Out <folder>] [-Dev] [-Force] [-Yes]
#
# Without -Iso a file dialog asks for the image. The result is "The SpongeBob Movie Game.exe"
# with config\ and disc\ beside it, by default in this folder, next to build.bat.
#
# Steps, each skipped when its result is already there and up to date:
#   1. check the tools: Rust (cargo) and the Visual Studio C++ build tools with a Windows
#      SDK. A missing one is offered for download and installed if the answer is yes (or
#      with -Yes, without asking): rustup from rust-lang.org, the Build Tools from Microsoft.
#   2. check the image is The SpongeBob SquarePants Movie, GGVE78 revision 1, the only
#      version the port's addresses are for
#   3. extract the disc into <Out>\disc (crates\recomp\src\bin\extract.rs)
#   4. recompile the game's code into crates\game\src\gen (crates\recomp)
#   5. build the executable (release; -Dev builds the faster-to-compile dev profile)
#   6. put the executable and the config files in <Out>; config files already there are kept
#
# -Force redoes the extraction and the recompilation.

param(
    [string]$Iso,
    [string]$Out,
    [switch]$Dev,
    [switch]$Force,
    [switch]$Yes
)

$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root
if (-not $Out) { $Out = $root }
$exeName = "The SpongeBob Movie Game.exe"

# The game's code on the disc, and its SHA-1 for GGVE78 revision 1.
$elfName = "sb04gc_nm.elf"
$elfSha1 = "2eff21ee1796b124c5200ed8cde09c4d7bc1f45b"
# Where the tools come from.
$rustupUrl = "https://static.rust-lang.org/rustup/dist/x86_64-pc-windows-msvc/rustup-init.exe"
$buildToolsUrl = "https://aka.ms/vs/17/release/vs_BuildTools.exe"

function Step($text) { Write-Host ""; Write-Host "== $text" -ForegroundColor Cyan }
function Fail($text) { Write-Host ""; Write-Host "Build stopped: $text" -ForegroundColor Red; exit 1 }
function Run($exe, [string[]]$arguments) {
    & $exe @arguments
    if ($LASTEXITCODE -ne 0) { Fail "$exe $($arguments -join ' ') failed (exit code $LASTEXITCODE)" }
}
function Sha1($path) { (Get-FileHash -Algorithm SHA1 -LiteralPath $path).Hash.ToLower() }

# Asks a yes or no question; true for yes. -Yes answers yes to all.
function Ask($question) {
    if ($Yes) { Write-Host "$question [Y/N] Y"; return $true }
    while ($true) {
        $answer = (Read-Host "$question [Y/N]").Trim().ToUpper()
        if ($answer -eq "Y" -or $answer -eq "YES") { return $true }
        if ($answer -eq "N" -or $answer -eq "NO") { return $false }
    }
}

function Download($url, $name) {
    $path = Join-Path $env:TEMP $name
    Write-Host "Downloading $url"
    Invoke-WebRequest -Uri $url -OutFile $path -UseBasicParsing
    return $path
}

function Find-Cargo {
    $cargo = Get-Command cargo -ErrorAction SilentlyContinue
    if ($cargo) { return $cargo.Source }
    # Installed but not yet on this window's PATH (rustup was just run).
    $installed = Join-Path $env:USERPROFILE ".cargo\bin\cargo.exe"
    if (Test-Path $installed) {
        $env:Path = "$(Split-Path $installed);$env:Path"
        return $installed
    }
    return $null
}

$vswhere = Join-Path ${env:ProgramFiles(x86)} "Microsoft Visual Studio\Installer\vswhere.exe"
function Find-Msvc {
    if (-not (Test-Path $vswhere)) { return $null }
    return & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
}
function Find-WindowsSdk {
    $libs = Join-Path ${env:ProgramFiles(x86)} "Windows Kits\10\Lib"
    # The SDK's libraries, not just its folder: an installer can leave the folder behind.
    return Get-ChildItem $libs -Directory -ErrorAction SilentlyContinue |
        Where-Object { Test-Path (Join-Path $_.FullName "um\x64\kernel32.lib") } |
        Select-Object -Last 1
}

Step "Checking tools"
$cargo = Find-Cargo
if (-not $cargo) {
    Write-Host "Rust is not installed. It is needed to translate and build the game (about 300 MB)."
    if (-not (Ask "Download and install Rust from rust-lang.org now?")) {
        Fail "Rust is needed. It can also be installed by hand from https://rustup.rs/"
    }
    $installer = Download $rustupUrl "rustup-init.exe"
    Run $installer @("-y", "--profile", "minimal", "--default-host", "x86_64-pc-windows-msvc")
    $cargo = Find-Cargo
    if (-not $cargo) { Fail "Rust was installed but cargo cannot be found; open a new window and run this again." }
}
if (-not (Find-Msvc) -or -not (Find-WindowsSdk)) {
    Write-Host "The Visual Studio C++ build tools and a Windows SDK are needed to build the game (several GB)."
    if (-not (Ask "Download and install them from Microsoft now? (Windows will ask for permission)")) {
        Fail "the C++ build tools are needed. Install 'Desktop development with C++' with a Windows SDK from https://visualstudio.microsoft.com/visual-cpp-build-tools/"
    }
    $installer = Download $buildToolsUrl "vs_BuildTools.exe"
    Write-Host "Installing; this can take a while."
    $arguments = @("--passive", "--wait", "--norestart", "--add", "Microsoft.VisualStudio.Workload.VCTools",
        "--add", "Microsoft.VisualStudio.Component.VC.Tools.x86.x64", "--add", "Microsoft.VisualStudio.Component.Windows11SDK.22621")
    $process = Start-Process -FilePath $installer -ArgumentList $arguments -Wait -PassThru
    # 3010: installed, a restart is wanted.
    if ($process.ExitCode -ne 0 -and $process.ExitCode -ne 3010) { Fail "the Build Tools installer failed (exit code $($process.ExitCode))." }
    if (-not (Find-Msvc) -or -not (Find-WindowsSdk)) { Fail "the Build Tools were installed but cannot be found; restart Windows and run this again." }
    if ($process.ExitCode -eq 3010) { Write-Host "(The installer asks for a restart; building usually works without one.)" }
}
Write-Host "Cargo:  $cargo"
Write-Host "MSVC:   $(Find-Msvc)"
Write-Host "SDK:    $((Find-WindowsSdk).Name)"

Step "Checking the disc image"
if (-not $Iso) {
    Add-Type -AssemblyName System.Windows.Forms
    $dialog = New-Object System.Windows.Forms.OpenFileDialog
    $dialog.Title = "Choose your The SpongeBob SquarePants Movie disc image (GameCube, USA)"
    $dialog.Filter = "GameCube disc images (*.iso;*.gcm)|*.iso;*.gcm|All files (*.*)|*.*"
    if ($dialog.ShowDialog() -ne [System.Windows.Forms.DialogResult]::OK) { Fail "no disc image chosen." }
    $Iso = $dialog.FileName
}
if (-not (Test-Path -LiteralPath $Iso)) { Fail "cannot find $Iso" }
$Iso = (Resolve-Path -LiteralPath $Iso).Path
$stream = [System.IO.File]::OpenRead($Iso)
$header = New-Object byte[] 0x60
[void]$stream.Read($header, 0, $header.Length)
$stream.Close()
$id = [System.Text.Encoding]::ASCII.GetString($header, 0, 6)
$revision = $header[7]
$title = [System.Text.Encoding]::ASCII.GetString($header, 0x20, 0x40).Split([char]0)[0]
Write-Host "Image:  $Iso"
Write-Host "Game:   $title ($id, revision $revision)"
if ($id -ne "GGVE78") { Fail "this is not The SpongeBob SquarePants Movie for GameCube, USA (GGVE78); it is $id." }
if ($revision -ne 1) { Fail "this is revision $revision of the disc; the port is made for revision 1." }

Step "Extracting the disc"
# Builds before 0.1.1 put the game in dist; its disc and saves move next to build.bat.
$old = Join-Path $root "dist"
if ((Test-Path $Out) -and (Resolve-Path $Out).Path -eq $root -and (Test-Path $old)) {
    foreach ($name in @("disc", "saves")) {
        if ((Test-Path (Join-Path $old $name)) -and -not (Test-Path (Join-Path $Out $name))) {
            Move-Item (Join-Path $old $name) (Join-Path $Out $name)
            Write-Host "Moved dist\$name here"
        }
    }
    # Left there it would be found by a search and start without the game's files.
    Remove-Item -Force -ErrorAction SilentlyContinue (Join-Path $old "TSSM.exe")
}
$disc = Join-Path $Out "disc"
$elf = Join-Path $disc "files\$elfName"
if ($Force -or -not (Test-Path $elf) -or (Sha1 $elf) -ne $elfSha1) {
    New-Item -ItemType Directory -Force $Out | Out-Null
    Run $cargo @("run", "-q", "-p", "recomp", "--release", "--bin", "extract", "--", $Iso, $disc)
} else {
    Write-Host "Already extracted to $disc"
}
if ((Sha1 $elf) -ne $elfSha1) { Fail "the game's code on this disc ($elfName) is not the one the port is made for." }

Step "Recompiling the game's code"
$gen = "crates\game\src\gen"
$stamp = Join-Path $gen ".source"
$wanted = "$elfSha1 $(Sha1 'config\hle.txt')"
if ($Force -or -not (Test-Path $stamp) -or (Get-Content $stamp -Raw).Trim() -ne $wanted) {
    Run $cargo @("run", "-p", "recomp", "--release", "--bin", "recomp", "--", $elf, "config\hle.txt", $gen)
    Set-Content -Path $stamp -Value $wanted -Encoding ascii
} else {
    Write-Host "Up to date ($gen)"
}

$buildProfile = if ($Dev) { "debug" } else { "release" }
Step "Building the game ($buildProfile; the first build takes a while)"
if ($Dev) { Run $cargo @("build", "-p", "tssm") } else { Run $cargo @("build", "-p", "tssm", "--release") }

Step "Putting it together in $Out"
Copy-Item -Force "target\$buildProfile\tssm.exe" (Join-Path $Out $exeName)
$config = Join-Path $Out "config"
New-Item -ItemType Directory -Force $config | Out-Null
# Building into this folder: config is already the game's.
$sameConfig = (Resolve-Path $config).Path -eq (Resolve-Path "config").Path
foreach ($file in Get-ChildItem "config" -File) {
    if ($sameConfig) { break }
    $target = Join-Path $config $file.Name
    # The HLE list belongs to this build; the rest are the player's settings.
    if ($file.Name -eq "hle.txt" -or -not (Test-Path $target)) { Copy-Item -Force $file.FullName $target }
}

Write-Host ""
Write-Host "Done. Start $(Join-Path (Resolve-Path $Out) $exeName)" -ForegroundColor Green
Write-Host "Saves are kept in $(Join-Path $Out 'saves') once the game has made one."
