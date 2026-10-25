# Builds the Android package and installs it on the phone connected by USB.
#
#   tools\android.ps1              build, package, install, start
#   tools\android.ps1 -PushDisc    also copy the extracted disc (disc\sys and disc\files, about
#                                  800 MB) to the phone; needed once
#   tools\android.ps1 -NoRun       do not start the game afterwards
#   tools\android.ps1 -Release     optimised build (much slower to compile)
#
# Needs the Android SDK with an NDK, build-tools and a platform (Android Studio installs them
# under %LOCALAPPDATA%\Android\Sdk), the Rust target aarch64-linux-android, Java on the PATH
# (for apksigner) and USB debugging enabled on the phone.
#
# On the phone the game's folder is the app's private one, reached with
#   adb shell run-as com.campi.tssm ls files
# It holds disc\ (game data), log.txt (what the desktop build prints), env.txt (debug
# variables, NAME=value per line) and screenshots\.
param([switch]$PushDisc, [switch]$NoRun, [switch]$Release)

$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
$package = 'com.campi.tssm'
$staging = '/data/local/tmp/tssm_disc'

$sdk = if ($env:ANDROID_HOME) { $env:ANDROID_HOME } else { "$env:LOCALAPPDATA\Android\Sdk" }
$latest = { param($dir) (Get-ChildItem "$sdk\$dir" -Directory | Sort-Object { [regex]::Replace($_.Name, '\d+', { $args[0].Value.PadLeft(6, '0') }) } | Select-Object -Last 1).FullName }
$ndk = & $latest 'ndk'
$tools = & $latest 'build-tools'
$platform = (Get-ChildItem "$sdk\platforms" -Directory | Where-Object { Test-Path "$($_.FullName)\android.jar" } | Sort-Object Name | Select-Object -Last 1).FullName
$adb = "$sdk\platform-tools\adb.exe"
$bin = "$ndk\toolchains\llvm\prebuilt\windows-x86_64\bin"

function Check($what) { if ($LASTEXITCODE -ne 0) { throw "$what failed" } }

# Build the shared library.
$env:CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER = "$bin\aarch64-linux-android28-clang.cmd"
$env:CARGO_TARGET_AARCH64_LINUX_ANDROID_AR = "$bin\llvm-ar.exe"
$profile = if ($Release) { 'release' } else { 'debug' }
$flags = @()
if ($Release) { $flags += '--release' }
Push-Location $root
try { cargo build -p tssm-android --target aarch64-linux-android @flags; Check 'cargo build' } finally { Pop-Location }
$out = "$root\target\aarch64-linux-android\$profile"

# Package: manifest, then the library (stripped of debug information, which the phone cannot
# use and which makes it several times larger), then align and sign with the debug key.
$stage = "$out\apk"
Remove-Item $stage -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force "$stage\lib\arm64-v8a" | Out-Null
& "$bin\llvm-strip.exe" --strip-debug -o "$stage\lib\arm64-v8a\libtssm_android.so" "$out\libtssm_android.so"; Check 'strip'
& "$tools\aapt2.exe" link -o "$stage\unaligned.apk" --manifest "$root\crates\android\AndroidManifest.xml" -I "$platform\android.jar"; Check 'aapt2'
Push-Location $stage
try { & "$tools\aapt.exe" add unaligned.apk lib/arm64-v8a/libtssm_android.so | Out-Null; Check 'aapt add' } finally { Pop-Location }
$apk = "$out\tssm.apk"
& "$tools\zipalign.exe" -f -P 16 4 "$stage\unaligned.apk" $apk; Check 'zipalign'
$keystore = "$env:USERPROFILE\.android\debug.keystore"
if (-not (Test-Path $keystore)) {
    keytool -genkeypair -keystore $keystore -storepass android -keypass android -alias androiddebugkey -keyalg RSA -keysize 2048 -validity 10000 -dname 'CN=Android Debug,O=Android,C=US'; Check 'keytool'
}
& "$tools\apksigner.bat" sign --ks $keystore --ks-pass pass:android --key-pass pass:android $apk; Check 'apksigner'
"Built $apk ({0:N0} MB)" -f ((Get-Item $apk).Length / 1MB)

# Install.
& $adb install -r $apk; Check 'adb install'
if ($PushDisc) {
    # The app cannot read what adb writes to shared storage, and adb cannot write to the
    # app's folder: copy to a staging folder, then have the app's own user take it from there.
    & $adb shell "rm -rf $staging && mkdir -p $staging"
    & $adb push "$root\disc\sys" "$staging/"; Check 'adb push'
    & $adb push "$root\disc\files" "$staging/"; Check 'adb push'
    & $adb shell "chmod -R a+rX $staging && run-as $package sh -c 'mkdir -p files && rm -rf files/disc && cp -r $staging files/disc'"; Check 'copying the disc into the app'
    & $adb shell "rm -rf $staging"
}
if (-not $NoRun) {
    & $adb shell am start -n "$package/android.app.NativeActivity"; Check 'adb start'
}
