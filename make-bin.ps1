# 產出可一次燒錄整片 flash 的單一韌體映像。
#
#   .\make-bin.ps1                  → dist\NANA-<版本>.bin
#   .\make-bin.ps1 -Flash COM8      → 產出後直接燒錄(會問過才動手)
#
# ⚠️ 這顆 BIN 是從位址 0x0 開始的整片 flash 映像,涵蓋 NVS 分割區(0x9000)。
#    燒它會把 NVS 一併清空 —— WiFi、Gemini 金鑰、Telegram 綁定、記憶、提醒全沒。
#    這是全新板子要的行為(一次到位、開機直接進配網模式)。
#    要更新「已經設定好」的板子,請改用 .\flash.ps1,那個只寫 app 分割區。
param(
    [string]$Flash = "",
    [string]$FlashSize = "16mb"
)

# 刻意不設 $ErrorActionPreference = "Stop"：espflash 會把版本更新提示寫到 stderr，
# 在 Stop 模式下 PowerShell 會把那行當成終止錯誤，明明成功卻中斷。
# 這支腳本每一步都自己檢查 $LASTEXITCODE，比攔 stderr 可靠。
$bin = "target\xtensa-esp32s3-espidf\release\esp32"

if (-Not (Test-Path $bin)) {
    Write-Host "找不到編譯產物，先跑 .\build.ps1" -ForegroundColor Red
    exit 1
}

# 版本號取自 Cargo.toml(FW_VERSION 也是讀同一個來源,不會對不上)
$version = (Select-String -Path Cargo.toml -Pattern '^version = "(.+)"' | Select-Object -First 1).Matches.Groups[1].Value
if (-Not $version) { $version = "unknown" }

if (-Not (Test-Path dist)) { New-Item -ItemType Directory dist | Out-Null }
$out = "dist\NANA-v$version.bin"

Write-Host "產出單檔映像 $out ..." -ForegroundColor Cyan
# --merge      把 bootloader + 分割表 + app 合成一個檔
# --skip-padding  不要補滿到 16MB(否則檔案會是 16MB,實際只用 3MB)
espflash save-image --chip esp32s3 --flash-size $FlashSize --merge --skip-padding `
    --partition-table partitions.csv $bin $out

if ($LASTEXITCODE -ne 0) {
    Write-Host "產出失敗 (exit $LASTEXITCODE)" -ForegroundColor Red
    exit $LASTEXITCODE
}

# espflash --merge 只合 bootloader + 分割表 + app，**不含 data 分割區**。
# 語音喚醒模型住在 partitions.csv 的 model 分割區，沒補進去的話燒完會看到
# 「ESP-SR init failed: No WakeNet model found in partition」，而使用者多半
# 會以為是麥克風接錯或喚醒詞念錯，不會想到模型根本沒燒進去。
# python 不在 PATH 的話，下面的注入只會印紅字然後「繼續」，而 $LASTEXITCODE 還是
# 前一步 espflash 留下的 0 —— 結果是印出「完成」但映像根本沒有模型。先擋掉。
if (-not (Get-Command python -ErrorAction SilentlyContinue)) {
    Write-Host "找不到 python，無法併入語音喚醒模型，也無法驗證映像。" -ForegroundColor Red
    Write-Host "請先安裝 Python 或把它加進 PATH。" -ForegroundColor Red
    exit 1
}

$models = @(Get-ChildItem -Path target -Filter srmodels.bin -Recurse -ErrorAction SilentlyContinue)
if ($models.Count -eq 0) {
    Write-Host "找不到 srmodels.bin — 產出的映像語音喚醒會壞掉，中止。" -ForegroundColor Red
    exit 1
}
if ($models.Count -gt 1) {
    # 切換過 ESP-IDF 版本會留下舊產物，默默取第一個等於隨機挑一份模型燒進去
    Write-Host "找到 $($models.Count) 份 srmodels.bin，無法判斷該用哪一份：" -ForegroundColor Red
    $models | ForEach-Object { Write-Host "  $($_.FullName)" }
    Write-Host "請清掉舊的建置產物（例如 cargo clean）後重試。" -ForegroundColor Red
    exit 1
}
$model = $models[0]

Write-Host "併入語音喚醒模型 ..." -ForegroundColor Cyan
python tools\inject_partition.py $out --from-partitions partitions.csv `
    --partition model --data-file $model.FullName
if ($LASTEXITCODE -ne 0) {
    Write-Host "模型併入失敗 (exit $LASTEXITCODE)" -ForegroundColor Red
    exit $LASTEXITCODE
}

# 本地打包也要驗，不能只有 CI 驗 —— 否則手動打出來的映像完全沒有防護網
Write-Host "驗證映像版面 ..." -ForegroundColor Cyan
python tools\verify_image.py $out partitions.csv $model.FullName
if ($LASTEXITCODE -ne 0) {
    Write-Host "映像驗證失敗，不要拿這個檔去燒。" -ForegroundColor Red
    exit $LASTEXITCODE
}

$mb = [math]::Round((Get-Item $out).Length / 1MB, 2)
Write-Host "完成：$out  ($mb MB)" -ForegroundColor Green
Write-Host ""
Write-Host "燒到全新板子：" -ForegroundColor Yellow
Write-Host "  espflash write-bin -p COM8 0x0 $out"
Write-Host ""
Write-Host "更新已設定好的板子(保留 WiFi/金鑰/記憶)：" -ForegroundColor Yellow
Write-Host "  .\flash.ps1 COM8"

if ($Flash) {
    Write-Host ""
    Write-Host "⚠️  即將燒錄 $Flash，這會清空該板的 NVS（WiFi/金鑰/Telegram 綁定/記憶全部消失）" -ForegroundColor Red
    $ans = Read-Host "確定要繼續嗎？輸入 yes 才會執行"
    if ($ans -ne "yes") {
        Write-Host "已取消。" -ForegroundColor Yellow
        exit 0
    }
    espflash write-bin -p $Flash 0x0 $out
}
