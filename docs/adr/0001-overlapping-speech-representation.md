# ADR 0001:重疊發言以片段層級為主、span 表為輔

- 狀態:已決定
- 日期:2026-08-01
- 相關:BLUEPRINT.md §11、§18、§8.2

## 背景

藍圖 §18 把「重疊發言的表示法」列為必須在 M2 建立 schema 之前決定的事項,理由是
拖到 M3 才處理會同時重寫 schema 與引用模型。

兩個選項:片段層級的單一 `speaker_id`,或詞與語句層級的語者指派。

片段層級在本機與遠端同時發言、或遠端多人交疊時會遺失資訊。詞層級能表達那些情況,
但要求每個 STT Adapter 都產出細粒度指派,而第一版 Adapter 未必做得到。

## 決定

兩者都保留,但只有一個是必填。

`transcript_segment_revisions.speaker_id` 維持片段層級,是主投影。
另加 `transcript_segment_speaker_spans` 表,鍵為 `(meeting_id, segment_id, revision, span_index)`,
放詞或語句層級的指派。

沒有 span 列 = 用片段層級的 `speaker_id`。有 span 列 = 細粒度指派。

## 理由

引用本來就以 `meeting_time_ms` 區間定位(§11),兩種粒度都不改變引用模型。
因此之後補上細粒度是**加資料**,不是改 schema,§18 擔心的「同時重寫 schema 與引用模型」不會發生。

選 superset 而不是二選一,是因為兩個選項的成本不對稱:先做片段層級之後要升級,是一次
schema migration 加引用模型重寫;先留 span 表,升級只是開始寫入那張表。

## 後果

- span 表目前沒有生產者,是空的。這是預期狀態,不是未完成的工作。
- 讀取端必須處理「有 span」與「沒有 span」兩種情況。目前只有片段層級的讀取路徑存在。
- §8.2 的回音重疊群組(`overlap_group_id`)與這個決定正交,兩者可以並存。
