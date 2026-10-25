# Builds the Android package with ALL game disc assets embedded completely inside it.
# Output file is self-contained and handles automated path compilation.
#
#   tools\androidapkkk.ps1              build standalone package
#   tools\androidapkkk.ps1 -Release     optimised standalone build
#
param([switch]$PushDisc, [switch]$NoRun, [switch]$Release)

$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
$package = 'com.campi.tssm'

$sdk = if ($env:ANDROID_HOME) { $env:ANDROID_HOME } else { "$env:LOCALAPPDATA\Android\Sdk" }
$latest = { param($dir) (Get-ChildItem "$sdk\$dir" -Directory | Sort-Object { [regex]::Replace($_.Name, '\d+', { $args.Value.PadLeft(6, '0') }) } | Select-Object -Last 1).FullName }
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

# Clear and stage package directory layout
$stage = "$out\apk"
Remove-Item $stage -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force "$stage\assets" | Out-Null
New-Item -ItemType Directory -Force "$stage\lib\arm64-v8a" | Out-Null

# Copy game files directly into standard Android Assets directory root prior to compilation link step
Copy-Item -Path "$root\disc" -Destination "$stage\assets\disc" -Recurse -Force

# 1. Strip symbols out of target binary layout
& "$bin\llvm-strip.exe" --strip-debug -o "$stage\lib\arm64-v8a\libtssm_android.so" "$out\libtssm_android.so"; Check 'strip'

# 2. Compile assets natively. The "-0" flag tells Android Studio tools to leave game files raw 
# so the standard Rust file-system query commands can read them direct from memory.
& "$tools\aapt2.exe" link -o "$stage\unaligned.apk" --manifest "$root\crates\android\AndroidManifest.xml" -I "$platform\android.jar" -A "$stage\assets" -0 "" -v; Check 'aapt2 compilation'

# 3. Inject library archive into manifest package
Push-Location $stage
try { 
    & "$tools\aapt.exe" add unaligned.apk lib/arm64-v8a/libtssm_android.so | Out-Null; Check 'aapt add'
} finally { Pop-Location }

# 4. Final alignment checks and developer signatures
$apk = "$out\tssm.apk"
& "$tools\zipalign.exe" -f -P 16 4 "$stage\unaligned.apk" $apk; Check 'zipalign'
$keystore = "$env:USERPROFILE\.android\debug.keystore"
if (-not (Test-Path $keystore)) {
    keytool -genkeypair -keystore $keystore -storepass android -keypass android -alias androiddebugkey -keyalg RSA -keysize 2048 -validity 10000 -dname 'CN=Android Debug,O=Android,C=US'; Check 'keytool'
}
& "$tools\apksigner.bat" sign --ks $keystore --ks-pass pass:android --key-pass pass:android $apk; Check 'apksigner'
"Built Master Package: $apk ({0:N0} MB)" -f ((Get-Item $apk).Length / 1MB)
