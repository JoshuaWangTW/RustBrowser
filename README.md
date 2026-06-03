# RustBrowser

> 為 LLM(Claude)設計的輕量網頁內容萃取器 —— 不是瀏覽器,而是「省 token 的撈取管線」。

## 這是什麼

讓 AI agent 抓取網路資料時,**不必啟動 Chrome**。Chrome 會渲染 DOM、執行 JS、載入圖片/CSS/字型/廣告/追蹤腳本——這些對「讀取內容」毫無幫助,只消耗 CPU、記憶體與時間。

更糟的是 token:一個典型網頁原始 HTML 常 100~300 KB,塞滿標籤與 inline script,但真正有用的正文往往只有 3~8 KB。把整包 HTML 餵給 LLM,token 直接爆炸。

RustBrowser 用一條精簡管線解決這件事:

```
URL → 輕量 HTTP 抓取(無瀏覽器引擎)
    → (必要時)headless 補渲染 JS 動態站
    → Readability 萃取正文(移除 nav / 廣告 / footer / script)
    → 轉成精簡 Markdown
    → 輸出給 LLM(token 通常砍掉 75~98%)
```

## 實測數據

| 頁面 | 原始 tokens | 萃取後 | 節省 |
|---|---|---|---|
| example.com | 152 | 29 | **80.9%** |
| Rust 官方文件頁 | 8,925 | 154 | **98.3%** |
| Wikipedia(Rust 條目,重雜訊) | 189,506 | 46,047 | **75.7%** |

Release 執行檔約 **8 MB** 單一靜態檔;熱路徑抓取+萃取(含網路)約 **0.5 秒**,**快取命中再快 ~5 倍**。對比 Chrome 動輒數百 MB 記憶體與秒級啟動,這是數量級的差距。

## 設計目標

| 目標 | 做法 |
|---|---|
| **省 token** | 正文萃取 + HTML→Markdown + 空格壓縮,只保留語義內容 |
| **省硬體** | Rust 無 GC、低記憶體;預設純 HTTP,不開瀏覽器引擎 |
| **快** | async 並發抓取、rustls 純 Rust TLS、磁碟快取避免重抓 |
| **抓得到** | 對 JS 動態站,auto 偵測並自動用系統 Chrome headless 補抓 |
| **可量化** | `--stats` 附帶「原始 vs 輸出 token」對比 |

## 架構

核心邏輯放在 library,包裝層共用:

```
┌─────────────────────────────────────────────┐
│  包裝層(共用核心)                              │
│  • CLI         rustbrowser fetch <url...>      │
│  • MCP server  rustbrowser-mcp(Claude 原生呼叫)│
├─────────────────────────────────────────────┤
│  核心 library (src/lib.rs)                     │
│  distill() 單頁 · distill_many() 並發批次       │
│  • fetch      HTTP 抓取(reqwest + rustls)      │
│  • cache      磁碟快取(SHA256 鍵 + TTL)        │
│  • render     headless fallback(系統 Chrome)   │
│  • extract    正文萃取(dom_smoothie)           │
│  • convert    HTML → Markdown(htmd)+ 空格壓縮   │
│  • structured 連結/表格抽成結構化資料           │
│  • tokens     token 估算與節省統計(tiktoken)   │
└─────────────────────────────────────────────┘
```

## 模組職責

| 模組 | 檔案 | 職責 |
|---|---|---|
| `fetch` | `src/fetch.rs` | HTTP 抓取、自動解壓、逾時、重定向、User-Agent |
| `cache` | `src/cache.rs` | 以 URL 的 SHA256 為鍵,把 fetch 結果存本地,含 TTL |
| `render` | `src/render.rs` | 偵測 JS 動態站,呼叫系統 Chrome `--dump-dom` 補渲染 |
| `extract` | `src/extract.rs` | Readability 萃取正文,移除 nav/廣告/footer/script |
| `convert` | `src/convert.rs` | 乾淨 HTML → 精簡 Markdown,壓縮對齊空格 |
| `structured` | `src/structured.rs` | 連結(解析相對網址)與表格(headers+rows)抽成資料 |
| `tokens` | `src/tokens.rs` | 估算原始/輸出 token,回報節省比例 |
| (lib) | `src/lib.rs` | `distill()` / `distill_many()` 串接整條管線 |
| MCP | `src/bin/rustbrowser-mcp.rs` | MCP server,暴露 `fetch_url` / `fetch_urls` 工具 |

## 使用方式:CLI

```powershell
# 抓取單頁,輸出精簡 Markdown
rustbrowser fetch https://example.com/article

# 一次抓多個 URL(並發批次)
rustbrowser fetch https://a.com https://b.com https://c.com

# 純文字 / 結構化 JSON / 只抓特定 CSS 區塊
rustbrowser fetch <url> --format text
rustbrowser fetch <url> --format json
rustbrowser fetch <url> --selector "main article"

# 結構化擷取連結 / 表格(搭 --format json 最完整)
rustbrowser fetch <url> --links --tables --format json

# JS 動態站:auto(預設,自動偵測)/ always(強制)/ off(純 HTTP)
rustbrowser fetch <spa-url> --js always

# token 統計 / 快取與並發控制
rustbrowser fetch <url> --stats
rustbrowser fetch <url> --no-cache            # 跳過快取,強制重抓
rustbrowser fetch <url> --cache-ttl 600       # 快取新鮮度改為 10 分鐘(預設 3600)
rustbrowser fetch <urls...> --concurrency 4   # 批次並發上限(預設 8)
```

磁碟快取存放於系統 cache 目錄下的 `rustbrowser/fetch/`;快取的是**原始 HTML**,所以同一頁之後仍能用不同 selector/format 重新萃取,省下的是最貴的網路往返。

## JS 動態站(headless fallback)

對 React/Vue 或任何「JS 才渲染內容」的站,純 HTTP 抓到的是空殼。`--js auto`(預設)會偵測「萃取文字極少 + 頁面 JS 偏重」,**自動**呼叫系統 Chrome/Edge 的 `--headless --dump-dom` 取得渲染後 DOM —— **零編譯依賴**(不綁瀏覽器引擎),只在必要時才用,保持輕量。

- `--js off` 完全不用 headless;`--js always` 強制每頁都 render。
- 找不到瀏覽器時可用環境變數 `RUSTBROWSER_CHROME` 指定執行檔路徑。

## 使用方式:給 Claude Code 用(MCP)

`rustbrowser-mcp` 是 stdio MCP server,暴露兩個工具,Claude 呼叫即可拿到精簡內容,**原始 HTML 完全不進對話**:

- **`fetch_url`** — 抓單一頁面
- **`fetch_urls`** — 一次並發抓多個頁面(比反覆呼叫 `fetch_url` 快得多)

專案根目錄已附 `.mcp.json`(指向 release binary)。或手動註冊:

```powershell
# 專案層級(此專案可用)
claude mcp add rustbrowser -- D:\aiproject\RustBrowser\target\release\rustbrowser-mcp.exe

# 全域(任何專案可用)
claude mcp add --scope user rustbrowser -- D:\aiproject\RustBrowser\target\release\rustbrowser-mcp.exe
```

共同參數:`format`、`selector`、`stats`、`timeout_secs`、`no_cache`、`cache_ttl`、`extract_links`、`extract_tables`、`js`(off/auto/always);`fetch_urls` 另有 `urls` 與 `concurrency`。

## 從原始碼編譯

> 一般網路環境:`cargo build --release` 即可。

本機處於受限網路(schannel 無法做 TLS 憑證撤銷檢查),需要兩項本機設定 —— 範本見 `.cargo/config.toml.example` 與 `.mcp.json.example`:

- 複製 `.cargo/config.toml.example` → `.cargo/config.toml`(設 `check-revoke = false`,讓 cargo 能存取 crates.io)。
- TLS 用 rustls,其 ring 後端在 Windows 需要 **NASM**(CI 用 `ilammy/setup-nasm`;本機可放可攜版於 `.tools/`)。

```powershell
$env:Path = "$env:USERPROFILE\.cargo\bin;$PWD\.tools\nasm-2.16.03;$env:Path"
cargo build --release
```

產出兩個 binary:`target/release/rustbrowser.exe`(CLI)與 `rustbrowser-mcp.exe`(MCP server)。headless fallback 直接編入,執行期才需系統 Chrome/Edge。

## 演進路徑

- ✅ **v0.1(MVP)** — 核心管線 + CLI:抓取 → 萃取 → Markdown → token 統計
- ✅ **v0.2** — 磁碟快取、批次並發抓取
- ✅ **v0.3** — MCP server,Claude Code 以原生工具 `fetch_url` / `fetch_urls` 呼叫
- ✅ **v0.4** — headless 自動 fallback(auto 偵測 JS 動態站)+ 連結/表格結構化擷取

## 技術棧

Rust · tokio · reqwest(rustls) · scraper · dom_smoothie · htmd · tiktoken-rs · clap · rmcp · futures · sha2 · dirs

---

*這個專案的本質:把「瀏覽網頁」這件事,縮減成 LLM 真正需要的最小資訊集。*
