# RustBrowser

> 為 LLM(Claude)設計的輕量網頁內容萃取器 —— 不是瀏覽器,而是「省 token 的撈取管線」。

## 這是什麼

讓 AI agent 抓取網路資料時,**不必啟動 Chrome**。Chrome 會渲染 DOM、執行 JS、載入圖片/CSS/字型/廣告/追蹤腳本——這些對「讀取內容」毫無幫助,只消耗 CPU、記憶體與時間。

更糟的是 token:一個典型網頁原始 HTML 常 100~300 KB,塞滿標籤與 inline script,但真正有用的正文往往只有 3~8 KB。把整包 HTML 餵給 LLM,token 直接爆炸。

RustBrowser 用一條精簡管線解決這件事:

```
URL → 輕量 HTTP 抓取(無瀏覽器引擎)
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

Release 執行檔約 **7.6 MB** 單一靜態檔;熱路徑抓取+萃取(含網路)約 **0.5 秒**。對比 Chrome 動輒數百 MB 記憶體與秒級啟動,這是數量級的差距。

## 設計目標

| 目標 | 做法 |
|---|---|
| **省 token** | 正文萃取 + HTML→Markdown + 空格壓縮,只保留語義內容 |
| **省硬體** | Rust 無 GC、低記憶體;純 HTTP,不開瀏覽器引擎 |
| **快** | async 抓取、連線複用、rustls 純 Rust TLS |
| **可量化** | `--stats` 附帶「原始 vs 輸出 token」對比 |

## 架構

核心邏輯放在 library,包裝層共用:

```
┌─────────────────────────────────────────────┐
│  包裝層(共用核心)                              │
│  • CLI         rustbrowser fetch <url>         │
│  • MCP server  rustbrowser-mcp(Claude 原生呼叫)│
├─────────────────────────────────────────────┤
│  核心 library (src/lib.rs)  distill()          │
│  • fetch    HTTP 抓取(reqwest + rustls)        │
│  • extract  正文萃取(dom_smoothie / Readability)│
│  • convert  HTML → Markdown(htmd)+ 空格壓縮     │
│  • tokens   token 估算與節省統計(tiktoken)      │
└─────────────────────────────────────────────┘
```

## 模組職責

| 模組 | 檔案 | 職責 |
|---|---|---|
| `fetch` | `src/fetch.rs` | HTTP 抓取、自動解壓、逾時、重定向、User-Agent |
| `extract` | `src/extract.rs` | Readability 萃取正文,移除 nav/廣告/footer/script |
| `convert` | `src/convert.rs` | 乾淨 HTML → 精簡 Markdown,壓縮對齊空格 |
| `tokens` | `src/tokens.rs` | 估算原始/輸出 token,回報節省比例 |
| `cli` | `src/cli.rs` | clap 參數定義 |
| (lib) | `src/lib.rs` | `distill()` 串接整條管線 |
| MCP | `src/bin/rustbrowser-mcp.rs` | MCP server,暴露 `fetch_url` 工具 |

## 使用方式:CLI

```powershell
# 抓取單頁,輸出精簡 Markdown
rustbrowser fetch https://example.com/article

# 只要正文純文字
rustbrowser fetch <url> --format text

# 結構化 JSON(標題 / 正文 / 統計)
rustbrowser fetch <url> --format json

# 只抓特定 CSS 區塊
rustbrowser fetch <url> --selector "main article"

# 顯示 token 節省統計(印到 stderr)
rustbrowser fetch <url> --stats
```

## 使用方式:給 Claude Code 用(MCP)

`rustbrowser-mcp` 是一個 stdio MCP server,暴露單一工具 `fetch_url`。Claude 呼叫它就能拿到精簡內容,**原始 HTML 完全不進對話**。

專案根目錄已附 `.mcp.json`(指向 release binary)。或手動註冊:

```powershell
# 專案層級(此專案可用)
claude mcp add rustbrowser -- D:\aiproject\RustBrowser\target\release\rustbrowser-mcp.exe

# 全域(任何專案可用)
claude mcp add --scope user rustbrowser -- D:\aiproject\RustBrowser\target\release\rustbrowser-mcp.exe
```

`fetch_url` 參數:`url`(必填)、`format`(markdown/text/json)、`selector`(CSS)、`stats`(bool)、`timeout_secs`。

## 從原始碼編譯

本機處於受限網路(schannel 無法做 TLS 憑證撤銷檢查),已內建兩項處理:

- `.cargo/config.toml` 設 `http.check-revoke = false`,讓 cargo 能存取 crates.io。
- TLS 用 rustls(`rustls-tls-*-roots`),其 ring 後端需要 **NASM**;可攜版已放在 `.tools/nasm-2.16.03/`。

編譯時把 cargo 與 nasm 都加進 PATH:

```powershell
$env:Path = "$env:USERPROFILE\.cargo\bin;$PWD\.tools\nasm-2.16.03;$env:Path"
cargo build --release
```

產出兩個 binary:`target/release/rustbrowser.exe`(CLI)與 `rustbrowser-mcp.exe`(MCP server)。

## 演進路徑

- ✅ **v0.1(MVP)** — 核心管線 + CLI:抓取 → 萃取 → Markdown → token 統計
- ✅ **v0.3** — MCP server,Claude Code 以原生工具 `fetch_url` 呼叫
- ⬜ **v0.2** — 磁碟快取、批次並發抓取、連結/表格結構化擷取
- ⬜ **v0.4** — headless 渲染 fallback(僅針對必須跑 JS 的 SPA),預設關閉

## 技術棧

Rust · tokio · reqwest(rustls) · scraper · dom_smoothie · htmd · tiktoken-rs · clap · rmcp

---

*這個專案的本質:把「瀏覽網頁」這件事,縮減成 LLM 真正需要的最小資訊集。*
