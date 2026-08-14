# v1.7 tasks(loop-method 取工檔;驗證發現的落差追加於此,標明來源輪次)

> 每輪:紅測試 → 實作 → `cargo fmt --check` + `clippy -D warnings` + `cargo test` 全綠 → 勾選。
> 完成定義與技術細節見 [plan.md](plan.md)。

## R1 — Nightly 真站 CI(可與 R2/R3 並行)
- [x] `tests/live.rs`:5 支 `#[ignore]` 真站測試(canary/passthrough/MDN/docs.rs actions/docs.rs session)
- [x] `.github/workflows/nightly-live.yml`:schedule+dispatch、失敗開 issue(去重)、無 PR trigger
- [x] 本機 `cargo test --test live -- --ignored` 全綠(2026-08-14,5/5)
- [ ] GitHub `workflow_dispatch` 一次全綠,留 run 連結(← 需 workflow 進 master,收斂階段做)

> R1 落差(來源:R1 真站實跑):codex-manual.md 現回 `application/octet-stream`,不進 passthrough——
> 已記 RB_FETCH_ISSUES.md(2026-08-14),回歸鎖改用 tag-pinned raw.githubusercontent README.md。
> 潛在後續(未排程):extension sniff(`.md`+octet-stream → passthrough),需單獨裁決。

## R2 — settle 原子性重構(可與 R1/R3 並行)
- [x] `prepare_settle(&self)` / `commit_settle(&mut self)` / `Settled` / `SettleFailure`
- [x] 測試 `settle_computation_does_not_mutate_session`
- [x] 測試 `cancelled_step_leaves_session_consistent`(wiremock delay + timeout cancel)
- [x] 測試釘住裁決語意:`fallback_reason_is_always_backed_by_a_log_entry`
- [x] 既有測試全綠(2026-08-14 fmt/clippy/test 全綠,零行為變更)

## R3 — cookie 讀取(可與 R1/R2 並行)
- [x] 紅測試 `session_exposes_cookies_for_current_origin`(含 cross-origin 回 None;在 src/session.rs #[cfg(test)],因 pub(crate) 對外部 test crate 不可見)
- [x] `Fetcher` 改 `cookie_provider(Arc<Jar>)` + `cookie_header_for`(暫 #[allow(dead_code)],R4 接線後移除)

## R4 — CDP cookie render + 接線 + 安全文件(依賴 R2、R3)
- [ ] `CdpRender` + `render_html_cdp_with`(既有 API 保留為包裝)
- [ ] `cdp_session`:Network.setCookie(navigate 前)/ readyState probe / clearBrowserCookies / TempDirGuard 刪除重試
- [ ] `render_fallback`:有 cookie 走 CDP、無 cookie 維持 --dump-dom;`fallback_render_mode` 純函式 + 單元測試
- [ ] kill switch `RUSTBROWSER_FALLBACK_NO_COOKIES=1`(預設開啟 — Joshua 已裁決)
- [ ] 測試鎖住「session render 不入 render cache」
- [ ] SECURITY.md / README.md / docs/API.md 承諾改寫 + CHANGELOG 獨立段落
- [ ] 煙測:本機假登入站完整 session 流程,留證據

## R5 — session outer deadline(依賴 R2、R4)
- [ ] `Session::step_budget()`(沿用 handler_budget 公式)
- [ ] 四個 session handler 包 timeout(含 session_start);非 GET 錯誤訊息「可能已送出,勿重試」
- [ ] 紅測試:超 budget 返回乾淨錯誤 + 同 session 可續用
- [ ] docs/API.md + SECURITY.md 補 deadline 語意
- [ ] (加分)MCP 工具名凍結測試

## 收斂
- [ ] CHANGELOG 1.7.0 + 版本號 + README 演進路徑
- [ ] verify-rb 全流程 + 真站煙測證據
- [ ] 專案記憶收斂(決策/成果/踩坑)
