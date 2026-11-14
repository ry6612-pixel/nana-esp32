# 開機自動啟動語音代理（讓娜娜一直是自然女聲，不會退回 Google 機械音）
#
#   .\tools\start-tts-proxy.ps1 -Install    註冊成登入時自動啟動
#   .\tools\start-tts-proxy.ps1 -Uninstall  取消
#   .\tools\start-tts-proxy.ps1             直接前景跑一次（測試用）
#
# 背景：代理沒跑的時候韌體會自動退回 Google 語音（不會啞掉），
# 但音色明顯機械感重——使用者會覺得「有時候聲音很棒、有時候很 AI」，
# 差別就在這支有沒有在跑。
param(
    [switch]$Install,
    [switch]$Uninstall,
    [string]$Pitch = "+15Hz",
    [string]$Rate  = "+50%"
)

$here    = Split-Path -Parent $MyInvocation.MyCommand.Path
$proxy   = Join-Path $here "tts_proxy.py"
$taskName = "SerialSoul-TTS-Proxy"

if ($Uninstall) {
    Unregister-ScheduledTask -TaskName $taskName -Confirm:$false -ErrorAction SilentlyContinue
    Write-Host "已取消自動啟動" -ForegroundColor Green
    exit 0
}

if ($Install) {
    # 刻意用 python.exe 而不是 pythonw.exe。
    # 實測發現 Windows 防火牆對這兩個執行檔的規則不同：pythonw.exe 在 Public
    # 網路設定檔是「封鎖」、python.exe 是「允許」。用 pythonw 的話代理跑得起來、
    # 本機連得到，但**裝置連不進來**（裝置端看到 ESP_ERR_HTTP_CONNECT），
    # 症狀是語音時好時壞，很難聯想到防火牆。
    $py = (Get-Command python).Source
    # 視窗最小化而不是隱藏（python.exe 一定會有主控台視窗）
    $action  = New-ScheduledTaskAction -Execute $py -Argument "`"$proxy`" --pitch $Pitch --rate $Rate"
    $trigger = New-ScheduledTaskTrigger -AtLogOn
    $settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries `
                    -DontStopIfGoingOnBatteries -RestartCount 3 -RestartInterval (New-TimeSpan -Minutes 1)
    Register-ScheduledTask -TaskName $taskName -Action $action -Trigger $trigger `
        -Settings $settings -Description "SerialSoul 語音代理（自然女聲）" -Force | Out-Null
    Start-ScheduledTask -TaskName $taskName
    Write-Host "已註冊並啟動：$taskName" -ForegroundColor Green
    Write-Host "確認：netstat -an | findstr 5050"
    exit 0
}

python "$proxy" --pitch $Pitch --rate $Rate
