# OpenMeetNote 安裝腳本
#
# 為什麼不是 NSIS：Tauri 的 NSIS bundler 在非 Windows 平台上跑不起來
# （它要的 makensis.exe 即使準備好原生版本仍無法被它執行），而這個專案在
# Linux 上交叉編譯。與其為了打包而換整條建置流程，不如用 PowerShell 做到
# 同樣的事 —— 捷徑、解除安裝註冊、升級覆蓋都在下面，而且使用者看得懂
# 它做了什麼。
#
#   powershell -ExecutionPolicy Bypass -File install.ps1
#   powershell -ExecutionPolicy Bypass -File install.ps1 -Uninstall
#
# 裝到使用者目錄不是 Program Files：那裡要提權，而這是個人工具，
# 不需要為了一個不共用的應用去要管理員權限。

param(
    [switch]$Uninstall,
    [string]$Source = $PSScriptRoot
)

$ErrorActionPreference = 'Stop'

$AppName    = 'OpenMeetNote'
$InstallDir = Join-Path $env:LOCALAPPDATA $AppName
$StartMenu  = Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs'
$RegKey     = "HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\$AppName"
$Shortcut   = Join-Path $StartMenu "$AppName.lnk"

function New-Shortcut([string]$Path, [string]$Target, [string]$WorkDir) {
    $shell = New-Object -ComObject WScript.Shell
    $lnk = $shell.CreateShortcut($Path)
    $lnk.TargetPath = $Target
    $lnk.WorkingDirectory = $WorkDir
    $lnk.Description = '本機語音會議記錄'
    $lnk.Save()
}

if ($Uninstall) {
    Write-Host "移除 $AppName…"

    # 先關掉正在跑的程式，否則檔案刪不掉而且會留下半套
    Get-Process -Name 'openmeetnote' -ErrorAction SilentlyContinue | ForEach-Object {
        $_.CloseMainWindow() | Out-Null
        Start-Sleep -Milliseconds 800
        if (-not $_.HasExited) { $_.Kill() }
    }

    if (Test-Path $Shortcut) { Remove-Item $Shortcut -Force }
    if (Test-Path $RegKey)   { Remove-Item $RegKey -Recurse -Force }
    if (Test-Path $InstallDir) { Remove-Item $InstallDir -Recurse -Force }

    # 會議紀錄不刪：那是使用者的資料，不是這個程式的一部分。
    $data = Join-Path $env:APPDATA 'app.openmeetnote.desktop'
    if (Test-Path $data) {
        Write-Host "會議紀錄保留在 $data"
        Write-Host '確定不要了再自行刪除。'
    }
    Write-Host '已移除。'
    return
}

# ── 安裝 ────────────────────────────────────────────────────────────

$exe = Join-Path $Source 'openmeetnote.exe'
if (-not (Test-Path $exe)) {
    throw "找不到 openmeetnote.exe。請在解壓縮後的資料夾裡執行這個腳本。"
}

$models = Join-Path $Source 'models'
if (-not (Test-Path $models)) {
    Write-Warning '找不到 models 資料夾。沒有模型的話錄音會被拒絕並說明缺什麼。'
}

Write-Host "安裝 $AppName 到 $InstallDir…"

# 升級時先關掉舊的：執行中的 exe 覆蓋不了
Get-Process -Name 'openmeetnote' -ErrorAction SilentlyContinue | ForEach-Object {
    $_.CloseMainWindow() | Out-Null
    Start-Sleep -Milliseconds 800
    if (-not $_.HasExited) { $_.Kill() }
}

New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null

# 詞表另外處理：使用者會編輯它，升級不該把他加的專有名詞蓋掉
$vocab = Join-Path $InstallDir 'vocabulary.txt'
$keepVocab = Test-Path $vocab
if ($keepVocab) {
    $vocabBackup = Join-Path $env:TEMP 'omn-vocabulary.bak'
    Copy-Item $vocab $vocabBackup -Force
}

Copy-Item (Join-Path $Source '*') $InstallDir -Recurse -Force -Exclude 'install.ps1'

if ($keepVocab) {
    Copy-Item $vocabBackup $vocab -Force
    Remove-Item $vocabBackup -Force
    Write-Host '保留了原本的 vocabulary.txt。'
}

New-Shortcut -Path $Shortcut -Target (Join-Path $InstallDir 'openmeetnote.exe') -WorkDir $InstallDir

# 註冊到「應用程式與功能」，使用者才找得到移除的入口
$version = (Get-Item (Join-Path $InstallDir 'openmeetnote.exe')).VersionInfo.FileVersion
if (-not $version) { $version = '0.1.0' }
$size = [math]::Round((Get-ChildItem $InstallDir -Recurse | Measure-Object Length -Sum).Sum / 1KB)

New-Item -Path $RegKey -Force | Out-Null
Set-ItemProperty $RegKey 'DisplayName'     $AppName
Set-ItemProperty $RegKey 'DisplayVersion'  $version
Set-ItemProperty $RegKey 'Publisher'       'OpenMeetNote'
Set-ItemProperty $RegKey 'InstallLocation' $InstallDir
Set-ItemProperty $RegKey 'DisplayIcon'     (Join-Path $InstallDir 'openmeetnote.exe')
Set-ItemProperty $RegKey 'EstimatedSize'   $size -Type DWord
Set-ItemProperty $RegKey 'NoModify'        1 -Type DWord
Set-ItemProperty $RegKey 'NoRepair'        1 -Type DWord
Set-ItemProperty $RegKey 'UninstallString' `
    "powershell -ExecutionPolicy Bypass -File `"$InstallDir\install.ps1`" -Uninstall"

Copy-Item (Join-Path $Source 'install.ps1') $InstallDir -Force

Write-Host ''
Write-Host "完成。開始選單裡的「$AppName」可以啟動。"
Write-Host "程式在 $InstallDir"
Write-Host "會議紀錄在 $env:APPDATA\app.openmeetnote.desktop"
Write-Host '移除請用「設定 → 應用程式」，或執行 install.ps1 -Uninstall'
