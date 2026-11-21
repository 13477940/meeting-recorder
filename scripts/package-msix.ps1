# 編譯 Windows 版並封裝成 MSIX（Microsoft Store／側載用的套件格式）。
# 需要：Rust（rust-toolchain.toml 指定版本）、Visual Studio C++ Build Tools、CMake、Windows 10/11 SDK（makeappx、signtool）。
#
# 側載測試（自簽憑證）：
#   ./scripts/package-msix.ps1 -SelfSign
#   會產生 dist\MeetingRecorder_<版本>_x64.msix 與 dist\MeetingRecorder-test.cer；
#   先把 .cer 匯入「本機電腦\受信任的人」，再雙擊 .msix 安裝。
# 上架 Microsoft Store：
#   ./scripts/package-msix.ps1 -Name "<Partner Center 的 Package/Identity/Name>" -Publisher "<Package/Identity/Publisher>" -PublisherDisplay "<發行者顯示名稱>"
#   不簽章，直接把 .msix 上傳到 Partner Center，由商店簽章。
param(
  [string]$Name = "MeetingRecorder",
  [string]$Publisher = "CN=MeetingRecorder Dev",
  [string]$PublisherDisplay = "MeetingRecorder",
  [ValidateSet("x64", "arm64")] [string]$Arch = "x64",
  [switch]$SelfSign,
  [switch]$NoLocalAsr
)
$ErrorActionPreference = "Stop"
Set-Location (Join-Path $PSScriptRoot "..")

$target = if ($Arch -eq "arm64") { "aarch64-pc-windows-msvc" } else { "x86_64-pc-windows-msvc" }
$cargoArgs = @("build", "--release", "--target", $target)
if ($NoLocalAsr) { $cargoArgs += "--no-default-features" }
& cargo @cargoArgs
if ($LASTEXITCODE -ne 0) { throw "cargo build 失敗" }

# Cargo 版本 0.1.0 → MSIX 版本 0.1.0.0（第四碼必須是 0 才能上架）
$ver = (Select-String -Path Cargo.toml -Pattern '^version\s*=\s*"([^"]+)"' | Select-Object -First 1).Matches[0].Groups[1].Value
$msixVer = "$ver.0"

# 找 Windows SDK 工具
$sdkBin = Get-ChildItem "${env:ProgramFiles(x86)}\Windows Kits\10\bin" -Directory |
  Where-Object { $_.Name -match '^10\.' } | Sort-Object Name -Descending | Select-Object -First 1
if (-not $sdkBin) { throw "找不到 Windows 10/11 SDK" }
$makeappx = Join-Path $sdkBin.FullName "x64\makeappx.exe"
$signtool = Join-Path $sdkBin.FullName "x64\signtool.exe"

$stage = "target\msix-stage"
Remove-Item $stage -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force -Path "$stage\Assets" | Out-Null
Copy-Item "target\$target\release\meeting-recorder.exe" $stage
Copy-Item "packaging\windows\Assets\*" "$stage\Assets"
Copy-Item LICENSE $stage
(Get-Content "packaging\windows\AppxManifest.xml" -Raw -Encoding UTF8).
  Replace("__NAME__", $Name).Replace("__PUBLISHER__", $Publisher).Replace("__PUBLISHER_DISPLAY__", $PublisherDisplay).
  Replace("__VERSION__", $msixVer).Replace("__ARCH__", $Arch) |
  Set-Content "$stage\AppxManifest.xml" -Encoding UTF8

New-Item -ItemType Directory -Force -Path dist | Out-Null
$msix = "dist\MeetingRecorder_${msixVer}_$Arch.msix"
& $makeappx pack /o /d $stage /p $msix
if ($LASTEXITCODE -ne 0) { throw "makeappx 失敗" }

if ($SelfSign) {
  # 測試用自簽憑證；Subject 必須和 manifest 的 Publisher 完全相同
  $cert = New-SelfSignedCertificate -Type Custom -Subject $Publisher -KeyUsage DigitalSignature -FriendlyName "MeetingRecorder test" `
    -CertStoreLocation "Cert:\CurrentUser\My" -TextExtension @("2.5.29.37={text}1.3.6.1.5.5.7.3.3", "2.5.29.19={text}")
  & $signtool sign /fd SHA256 /sha1 $cert.Thumbprint /s My $msix
  if ($LASTEXITCODE -ne 0) { throw "signtool 失敗" }
  Export-Certificate -Cert $cert -FilePath "dist\MeetingRecorder-test.cer" | Out-Null
  Write-Host "已用測試憑證簽章；安裝前先把 dist\MeetingRecorder-test.cer 匯入「本機電腦\受信任的人」"
}
Write-Host "完成：$msix"
