# RB v1.7 規劃 —「從固定集走向真實世界」

> 定案:2026-08-14。狀態:**已定案、待開工**(Joshua 裁決:先存計畫待命)。
> 產出:Opus planner 稽核規劃 + Fable 審查 + Joshua 三項裁決。
> 執行時依 loop-method:每輪紅轉綠、CI 同款驗證全綠 + 真站煙測留證據才算完成。

## 已裁決事項

1. **範圍**:三項全做 — nightly 真站 CI、fallback render 帶 cookie、session outer deadline。
2. **nightly 目標站**:穩定文件站(example.com canary、codex-manual.md 回歸、MDN、docs.rs);不打 SERP、不在 nightly 跑 Chrome fallback。
3. **fallback render 失敗時 `last_fallback` 語意**:維持原樣(保留 `Some(reason)`),用測試釘住「`fallback_reason` 有值必伴隨 `chrome_fallback` 或 `chrome_fallback_failed` log」。
4. **cookie fallback 預設值**:**預設開啟**,`RUSTBROWSER_FALLBACK_NO_COOKIES=1` 關閉。SECURITY.md「cookie 不帶進 fallback browser」承諾改寫,CHANGELOG 用獨立醒目段落記載。

## Goal(可驗證完成標準)

1. `#[ignore]` 真站測試 + 非阻塞 nightly workflow;手動 dispatch 一次全綠留 run 連結。
2. fallback render 以隔離 profile 帶入 session cookies;「登入後 + JS」測試紅轉綠;本機假登入站煙測證明 `used_headless=true` 且內容含登入後才有的字串。
3. `Session::settle()` 原子化 + session handlers wall-clock deadline;cancel 不留不一致狀態(不變式測試);deadline 超時回乾淨錯誤且 session 可續用。
4. `cargo fmt --check` / `clippy -D warnings` / `cargo test` 全綠 + CHANGELOG/版本號 + 真站煙測證據。

## 輪次(執行順序,取工來源 = tasks.md)

| 輪 | 內容 | 改動檔 | 相依 |
|---|---|---|---|
| R1 | `tests/live.rs` + `.github/workflows/nightly-live.yml` | 2 新檔 | 無 |
| R2 | settle 原子性重構(`prepare_settle`/`commit_settle`)+ 不變式測試 | `session.rs`、`tests/session.rs` | 無 |
| R3 | `cookie_provider(Arc<Jar>)` + `cookie_header_for` + cross-origin scope 測試 | `fetch.rs`、`tests/session.rs` | 無 |
| R4 | `render_html_cdp_with`(cookie 注入/readyState probe/temp dir 刪除重試)+ session 接線 + 安全文件改寫 | `render.rs`、`session.rs`、`SECURITY.md`、`README.md`、`docs/API.md` | R2, R3 |
| R5 | `Session::step_budget()` + 四個 MCP session handler 包 deadline + 超時測試 | `session.rs`、`rustbrowser-mcp.rs`、`tests/session.rs` | R2, R4 |

R1 與 R2/R3 可並行派工(executor, Sonnet)。

## 各輪技術方案摘要

### R1 — Nightly 真站 CI

`tests/live.rs`,全掛 `#[ignore = "live network; run with --ignored"]`,斷言**形狀不變式**不斷言文案:

- `live_baseline_canary`(example.com):title/text 非空;失敗=CI 網路壞,非回歸
- `live_codex_manual_markdown_is_not_inflated`(developers.openai.com/codex/codex-manual.md):`output_tokens <= raw_tokens*1.02`、無 `\#`/`\_` 逃逸、`used_headless==false`、<60s — v1.6 結案 issue 回歸鎖
- `live_mdn_article_distills_lean`(MDN HTTP Methods):title 非空、text>500、saved_ratio>0.5
- `live_docs_rs_actions_are_absolute`(docs.rs/reqwest):actions 非空、href 全為絕對 URL
- `live_session_follow_on_real_site`(docs.rs session):observe→follow,current_url 變、redirect_history==2、`last_fallback().is_none()`

共用:`use_cache:false`、`min_request_interval:1s`、每 host 一支測試。

`nightly-live.yml`:`schedule: 0 18 * * *`(02:00 台北)+ `workflow_dispatch`;`cargo test --test live -- --ignored --test-threads=2`;失敗 `gh issue create --label live-regression`(先 list 去重)。**非阻塞是結構性保證**:無 pull_request trigger、不進 required checks;**不用** `continue-on-error`(失敗要紅要開 issue,只是不擋 merge)。

備案:openai 站擋 CI IP → 改 raw.githubusercontent.com 的 .md 做 passthrough 回歸,codex URL 留手動。

### R2 — settle 原子性重構(純重構,零行為變更)

現況 bug 面:`session.rs` settle(:321-412)在 render await 前設 `last_fallback=None`,await 後才 commit 其餘欄位;外層 cancel 留下「舊 snapshot 配 fallback_reason:null」。

重構形狀:計算/提交分離,`&self` 讓編譯器證明 await 期間不可 mutate:

```rust
struct Settled { snapshot, final_url, failure, fallback, pending_log }
struct SettleFailure { error, pending_log }
async fn prepare_settle(&self, ...) -> Result<Settled, SettleFailure>  // 唯一 await 處,&self
fn commit_settle(&mut self, s: Settled)                                 // 純同步,五欄位一次 commit
```

不變式:`current_url`/`last_snapshot`/`redirect_history`/`last_failure`/`last_fallback` 五者同進同退。retry 迴圈的 `log_attempt` 保持在 await 區間(純 append 診斷,cancel 只少 log 不留矛盾)。

測試:`settle_computation_does_not_mutate_session`、`cancelled_step_leaves_session_consistent`(wiremock set_delay + tokio timeout cancel)。誠實註記:舊碼的 bug 窗口需假 Chrome 才能穩定重現,此輪用型別保證+不變式測試取代「製造那一瞬間」。

### R3 — session 讀出 cookies

`fetch.rs`:`.cookie_store(bool)` 改 `.cookie_provider(Arc<reqwest::cookie::Jar>)`(Jar 已實作 CookieStore,`cookies(&url)` 直接給 `Cookie:` header 值;**零新依賴**)。`Fetcher` 加 `cookie_jar` 欄位 + `pub(crate) fn cookie_header_for(&self, url) -> Option<String>`。

限制註記:`Jar::cookies()` 只給 name=value,不帶 Secure/HttpOnly/expiry;僅用於餵隔離 Chrome 渲染同一 URL,不做持久化。

測試(紅轉綠):`session_exposes_cookies_for_current_origin` — Set-Cookie 後 `cookie_header_for` 同 origin 含 `sid=abc`、**不同 origin 回 None**(跨站洩漏防護鎖)。

### R4 — CDP render 注入 cookie + session 接線

`render.rs` additive API:

```rust
#[derive(Default)]
pub struct CdpRender<'a> { pub wait_for: Option<&'a str>, pub cookies: Option<&'a str> }
pub async fn render_html_cdp_with(url, budget, r: CdpRender) -> Result<String>
// 既有 render_html_cdp 保留為薄包裝
```

`cdp_session()` 調整:(1) Page.navigate 前 `Network.enable` + 逐一 `Network.setCookie {name,value,url}`(url 參數讓 Chrome 推導 domain/path);(2) `wait_for: None` 時 probe 用 `document.readyState === "complete"`;(3) teardown 前 `Network.clearBrowserCookies`;(4) TempDirGuard 的 remove_dir_all 加 3 次重試+100ms(Windows 鎖檔;目錄內有 cookie,刪不掉是安全問題)。

`session.rs` render_fallback:有 cookie → `render_html_cdp_with`;無 cookie → 維持 `render_html`(--dump-dom,匿名頁行為零變更)。

安全面(文件必改):cookie 只進隔離 temp profile、render 完 clearBrowserCookies+刪目錄;只送 RB 本來就會送的 origin;**絕不 log cookie 名/值**(operation_log 只記數量);加測試鎖住「session render 不走 render cache」;kill switch `RUSTBROWSER_FALLBACK_NO_COOKIES=1`;SECURITY.md:146 / README.md:238 / docs/API.md session 段的「cookie 不帶進 fallback browser」承諾改寫。

備案:CDP 在 Windows 不穩 → 降級為 `RUSTBROWSER_FALLBACK_COOKIES=1` opt-in(此為備案,預設值已裁決為開啟)。

煙測:**本機**假登入站(POST 登入拿 cookie、受保護頁 JS 寫入 #root),完整 session 流程斷言 fallback_reason 有值、used_headless=true、內容含登入後字串。(不對外部站 confirm=true 送非 GET — 全域紅線。)

### R5 — session outer deadline

`Session::step_budget()`(additive lib API):`(max_action_retries+1) × opts.timeout`,與 `js_wait` 取 max,+15s margin(沿用 MCP handler_budget 公式,rustbrowser-mcp.rs:476-488)。

四個 session handler(session_start :816、session_observe :838、session_follow :854、session_submit_form :886)包 `tokio::time::timeout`;session_start 目前全裸,一併補。**MCP 參數零新增**(由既有 timeout_secs/max_action_retries 推導)。

confirm=true 非 GET 一樣包 deadline,但錯誤訊息必明說「**請求可能已送出,RB 沒收到結果,請勿重試**」且絕不自動重送;同步寫入 docs/API.md 與 SECURITY.md。

測試(紅轉綠):wiremock set_delay 超過 budget → (a) budget+slack 內返回錯誤非永久等待;(b) 同 session 再 observe 正常頁仍成功。用 `timeout_secs=1` 讓測試快。

## Schema freeze 影響

- R1/R2:零。R3:`pub(crate)`,零公開面。
- R4:lib additive(`render_html_cdp_with`+`CdpRender`);MCP/CLI 參數零變更;**安全承諾文字變更**,semver minor 但 CHANGELOG 獨立段落。
- R5:MCP 參數零變更;行為變更「無限等待→budget 內乾淨錯誤」寫 CHANGELOG。
- 加分項(非必做):補 MCP 工具名凍結測試(現有 schema_freeze.rs 只鎖 CLI flags)。

## 這次不做

CDP click/任何互動能力;cookie 持久化/匯出入/讀真實 Chrome profile;panic=abort→unwind(留 backlog,單獨一輪);nightly 進 required checks;nightly 跑 Chrome fallback;`deadline_secs` MCP 參數;stateless `distill()` 帶 cookie;proxy/headful/多分頁/憑證儲存。

## 關鍵位置(規劃時點,執行前重驗)

- `src/session.rs` — settle :321-412、render_fallback :438-445、Session::new :84-122
- `src/render.rs` — cdp_session :317-356、TempDirGuard :249-256
- `src/fetch.rs` — Fetcher :110-116、Fetcher::new :119-151
- `src/bin/rustbrowser-mcp.rs` — handler_budget :476-488、session handlers :816-905
