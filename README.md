# OpenMeetNote

OpenMeetNote 是一套以 Windows 與 macOS 為目標的個人語音會議應用。它在本機擷取系統音訊與麥克風、持續產生即時逐字稿，並允許使用者在不中斷錄音的情況下建立摘要快照。

最終成果不受固定會議分類或文件模板限制。Agent Loop 會根據會議內容、人工筆記、附件與本輪使用者 Prompt，自行規劃並驗證適合的 HTML 成果文件。

## 目前狀態

目前完成產品需求整理、工程藍圖及 HTML 互動原型；桌面應用程式尚未開始實作。

- [完整工程藍圖](BLUEPRINT.md)
- [HTML 互動原型](docs/prototype.html)

## 已確認範圍

- Windows 與 macOS 桌面應用，不支援 Android。
- 同時擷取系統音訊與麥克風，不使用會議 Bot。
- 錄音期間持續顯示串流逐字稿。
- 隨時建立摘要快照，不停止錄音或轉錄。
- 語者預設為「語者 1／語者 2」，明確自我介紹只建立待確認名稱。
- 主要使用繁體中文，保留技術與商業英文詞彙。
- 人工筆記納入摘要，且優先級高於一般逐字稿片段。
- 個人使用、不登入、不跨裝置同步，資料預設保存在本機。
- 支援 BYOK，GUI 設定可被作業系統環境變數覆寫。
- Agent Loop 依會議證據與使用者 Prompt 動態規劃成果，不建立「會議類型 → 固定文件」規則。

## 預定技術方向

- Tauri 2
- React 與 TypeScript
- Rust 核心
- SQLite
- macOS ScreenCaptureKit
- Windows WASAPI Loopback
- 可替換的 STT 與 LLM Adapter

技術方向仍須依雙平台音訊 PoC、延遲量測與供應商能力驗證後定案。

## 授權

尚未決定。若後續移植 MIT 專案程式碼，必須保留對應授權與著作權聲明。

