# RustBrowser 執行準則

> 每個 session 適用。RB = RustBrowser（僅為互動別名，不改 code 層命名）。互動慣例與 issue 記錄模板見 [AGENTS.md](AGENTS.md)。

## 模型分工 × Loop（固定準則）

- **Fable 5 = 大腦（主迴圈）**：定可驗證的 goal、盤點排序、拆小輪、派工、每輪驗證、收斂記錄。判斷不外包。
- **Opus = 派工／規劃層**：稽核、架構、規劃、審查一律派 Opus agents（`architect` / `planner` / `critic` / `code-reviewer`，model: opus）。
- **Sonnet 5 = 執行層**：規格已明確的實作派 `executor`（model: sonnet），每輪 1–3 筆、改動面小、可獨立驗證。
- **Loop 直到 goal**：依 loop-method——goal 未達成且仍能推進就繼續下一輪；停止條件 = goal 全數驗證通過，或連續 2 輪無法推進（此時回報卡點，不硬衝）。
- 例外：單檔小改動 Fable 直接做，不為小事開 agent。

**宣告完成的唯一標準**：跑完下方驗證流程且全綠 + 真實驗證留證據。絕不因「edit 成功」宣告完成。

## Build 環境（Windows 受限網路，每個 shell 都要）

```powershell
$env:Path = "$env:USERPROFILE\.cargo\bin;D:\aiproject\RustBrowser\.tools\nasm-2.16.03;$env:Path"
$env:CARGO_HTTP_CHECK_REVOKE = "false"
```

- TLS 走 rustls 雙根憑證（`rustls-tls-webpki-roots` + `rustls-tls-native-roots`），**不要改回 native-tls、也不要只留單一 rustls feature**（會 UnknownIssuer）。
- 不引入需要 CMake 的依賴（aws-lc-rs 系禁止）；能用現有依賴／stdlib 就不加新套件——輕量是產品定位。

## 驗證流程（= CI 同款，宣告完成前必跑）

1. `cargo fmt --all -- --check`
2. `cargo clippy --all-targets -- -D warnings`
3. `cargo test`（integration / session / eval / schema_freeze / benchmark 全綠）
4. 動過 fetch/render/MCP surface：用 release binary 對真實網站煙測一次，貼輸出片段當證據。

詳細步驟編碼於 `.claude/skills/verify-rb/SKILL.md`。

## 安全紅線（優先不要做）

- 不做 pixel-level click；不做完整 JS browser；不讓 LLM 未確認執行任意 POST；不把 RB 做成 Chrome clone。
- 公開 surface 受 semver 凍結（`docs/API.md` + `tests/schema_freeze.rs`）：additive 可以，breaking = major。

## 慣例

- Commit 中文描述 `<type>: <描述>`；feature branch → PR → merge（repo 既有模式）。
- RB 抓取問題記錄進 `RB_FETCH_ISSUES.md`；修好後在該 issue 下補記結案。
- `gh` 偶發 401：`$env:GH_TOKEN = (gh auth token)` 後重試；merge 走 REST（`gh api -X PUT .../pulls/N/merge`）較穩。
