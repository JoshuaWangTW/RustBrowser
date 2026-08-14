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
- [x] `CdpRender` + `render_html_cdp_with`(既有 API 保留為包裝)
- [x] `cdp_session`:Network.setCookie(navigate 前)/ readyState probe / clearBrowserCookies / TempDirGuard 刪除重試
- [x] `render_fallback`:有 cookie 走 CDP、無 cookie 維持 --dump-dom;`fallback_render_mode` 純函式 + 單元測試
- [x] kill switch `RUSTBROWSER_FALLBACK_NO_COOKIES=1`(預設開啟 — Joshua 已裁決)
- [x] 測試鎖住 `session_never_uses_the_on_disk_cache`
- [x] SECURITY.md / README.md / docs/API.md 承諾改寫(CHANGELOG 留收斂輪)
- [x] 煙測:`tests/smoke_login.rs`(#[ignore],wiremock 假登入站 + 真 Chrome)——正向:cookie 注入後 render 出登入內容 SECRET-DASHBOARD-42;反向:kill switch 開時如預期拿到匿名 PLEASE-LOG-IN 而紅。2026-08-14 實跑證據留存

> R4 落差(來源:R4 煙測):rendered DOM 的 inline `<script>` 文字會漏進 markdown
> (失敗訊息裡看到 script 原文)。distill 品質項,不影響 v1.7 goal,未排程——
> 候選 v1.8 擷取品質輪。

## R5 — session outer deadline(依賴 R2、R4)
- [x] `Session::step_budget()`(即時計算不存欄位——偏離 plan 但更簡,公式單處)
- [x] 四個 session handler 包 timeout(含 session_start);非 GET 錯誤訊息「可能已送出,勿重試」
- [x] 紅測試:503+Retry-After 卡重試 sleep → 17.24s 觸發 budget、乾淨錯誤、同 session 可續用(拔 wrapper 驗證過真紅:30.24s 普通 503)
- [x] docs/API.md + SECURITY.md 補 deadline 語意
- [ ] (加分)MCP 工具名凍結測試(未做,非必做)

## 收斂
- [x] CHANGELOG 1.7.0(安全預設變更獨立段落)+ 版本號 1.7.0 + README 演進路徑
- [x] verify-rb 全流程(fmt/clippy/test 150 passed)+ release binary 真站煙測(MDN 蒸餾正常;raw README.md passthrough raw 2675 = output 2675 精確直出)
- [x] 專案記憶收斂(決策/成果/踩坑)
- [ ] PR merge 後:GitHub `workflow_dispatch` 跑一次 nightly-live 留 run 連結(R1 尾巴)
