#!/usr/bin/env bash
# 打包成可散布的資料夾：exe + DLL + 前端資源 + 安裝腳本。
#
# 模型不進來：那超過一 GB，而且使用者換模型時不該重裝整個程式。
# 安裝腳本會檢查 models/ 是否存在並提醒。
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TARGET="$ROOT/src-tauri/target/x86_64-pc-windows-msvc/release"
OUT="${1:-$ROOT/dist-win}"

[[ -f "$TARGET/openmeetnote.exe" ]] || {
  echo "找不到 openmeetnote.exe，先跑 pnpm tauri build --runner cargo-xwin --target x86_64-pc-windows-msvc --no-bundle"
  exit 1
}

rm -rf "$OUT" && mkdir -p "$OUT"
cp "$TARGET/openmeetnote.exe" "$OUT/"
# sherpa-onnx 與 onnxruntime 是動態連結，少一個就跑不起來
cp "$TARGET"/*.dll "$OUT/" 2>/dev/null || true
cp "$ROOT/installer/install.ps1" "$OUT/"

cat > "$OUT/vocabulary.txt" <<'TXT'
# 會議詞表：每行一組「錯誤=正確」，# 開頭是註解。
# 轉錄引擎對台灣特有的人名、機關名、專有名詞普遍認不準，在這裡補上即可。
# 存檔後下次開始錄音就生效，不需要重開程式。

招委=召委
希臘雅=西拉雅
雙向元=雙橡園
TXT

cat > "$OUT/README.txt" <<'TXT'
OpenMeetNote

安裝：
  在這個資料夾按右鍵開啟 PowerShell，執行
    powershell -ExecutionPolicy Bypass -File install.ps1

模型：
  錄音與摘要需要本機模型，合計約 1.1 GB，不隨程式散布。
  把 models 資料夾放進安裝目錄（%LOCALAPPDATA%\OpenMeetNote），需要：
    models\ggml-large-v3-turbo-q5_0.bin           定稿
    models\sherpa-onnx-paraformer-zh-...\          即時稿
    models\sherpa-onnx-punct-ct-transformer\       標點
    models\silero_vad.onnx                         語音偵測
    models\speaker-embedding.onnx                  語者辨識（可選）

摘要：
  由本機的 Claude Code 或 Codex CLI 產生，不需要 API 金鑰，
  但那支 CLI 要先登入。在設定頁選擇要用哪一個。

移除：
  設定 → 應用程式 → OpenMeetNote，或 install.ps1 -Uninstall
  會議紀錄不會被刪，它在 %APPDATA%\app.openmeetnote.desktop
TXT

echo "打包完成：$OUT"
du -sh "$OUT"
ls "$OUT"
