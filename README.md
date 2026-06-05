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
| **快** | 共用 HTTP client、async 並發抓取、rustls 純 Rust TLS、磁碟快取避免重抓 |
| **抓得到** | 對 JS 動態站,auto 偵測並自動用系統 Chrome headless 補抓 |
| **可控** | streaming body 上限、charset-aware 解碼、URL 安全邊界、`--stats` token 對比 |

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
| `fetch` | `src/fetch.rs` | HTTP 抓取、自動解壓、charset 解碼、streaming 上限、重定向與 URL 安全檢查、重試/退避、per-host 並發與速率限制 |
| `robots` | `src/robots.rs` | (opt-in)抓取/解析/快取 robots.txt,強制 Disallow 規則 |
| `cache` | `src/cache.rs` | 分層快取 fetch HTML 與 headless render DOM,含 TTL、`info`/`prune`/`clear` 維護 |
| `render` | `src/render.rs` | 偵測 JS 動態站,呼叫系統 Chrome `--dump-dom` 補渲染 |
| `extract` | `src/extract.rs` | Readability 萃取正文(article),或整頁 body 擷取(full,去 script/style) |
| `budget` | `src/budget.rs` | 依 token 預算在段落邊界截斷輸出 |
| `convert` | `src/convert.rs` | 乾淨 HTML → 精簡 Markdown,壓縮對齊空格 |
| `structured` | `src/structured.rs` | 連結(解析相對網址)與表格(headers+rows)抽成資料 |
| `actions` | `src/actions.rs` | action tree:links/forms/buttons/downloads + 穩定 action_id(Browser Use) |
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
rustbrowser fetch <url> --links-all                   # 全頁連結(含 nav),供爬蟲

# JS 動態站:auto(預設,自動偵測)/ always(強制)/ off(純 HTTP)
rustbrowser fetch <spa-url> --js always
rustbrowser fetch <spa-url> --js-wait 5000            # 給 JS 更多等待時間(ms)
rustbrowser fetch <spa-url> --js-wait-for ".results"  # 等特定元素出現才抓(CDP)

# token 統計 / 快取與並發控制
rustbrowser fetch <url> --stats
rustbrowser fetch <url> --max-bytes 4194304  # response body 最多保留 4 MiB
rustbrowser fetch <url> --no-cache            # 跳過快取,強制重抓
rustbrowser fetch <url> --cache-ttl 600       # 快取新鮮度改為 10 分鐘(預設 3600)
rustbrowser fetch <urls...> --concurrency 4   # 批次並發上限(預設 8)

# 本機開發伺服器:預設拒絕 loopback,需明確開啟(僅放行 loopback,
# private LAN / link-local / metadata 仍被擋)
rustbrowser fetch http://127.0.0.1:8080 --allow-local

# 禮貌抓取 / 穩健性(對伺服器友善 + 容忍暫時性失敗)
rustbrowser fetch <url> --max-retries 3          # 暫時性失敗(連線/逾時、429、5xx)指數退避重試(預設 2)
rustbrowser fetch <urls...> --per-host-concurrency 2  # 同一 host 最多 2 個並發(預設 4;0=不限)
rustbrowser fetch <urls...> --rate-limit 2       # 每個 host 每秒最多 2 次請求(預設 0=不限速)
rustbrowser fetch <url> --respect-robots         # 遵守該站 robots.txt,跳過被 Disallow 的路徑

# 擷取 profile / token 預算 / 品質診斷
rustbrowser fetch <url> --profile full           # 整個 body(readability 過度刪時用),另有 article(預設)/ metadata
rustbrowser fetch <url> --profile metadata       # 只要 title + 一段摘要(最省 token 的「這頁在講什麼」)
rustbrowser fetch <url> --max-output-tokens 800  # Markdown/text 輸出超過 800 tokens 就截斷
rustbrowser fetch <url> --diagnostics            # 把擷取品質診斷印到 stderr(JSON 格式則帶在 result 裡)
```

磁碟快取的維護子命令:

```powershell
rustbrowser cache info                 # 列出 fetch / render 各自的筆數與占用空間
rustbrowser cache prune --older-than 3600   # 刪除超過 1 小時的舊快取
rustbrowser cache clear                # 清空所有快取(fetch + render)
```

磁碟快取存放於系統 cache 目錄下:

- `rustbrowser/fetch/`:快取原始 HTTP HTML,同一頁之後仍能用不同 selector/format 重新萃取。
- `rustbrowser/render/`:快取 headless 渲染後 DOM,key 包含 URL、JS 模式、等待時間與 wait selector,避免 JS-heavy 頁面每次 cache hit 仍重開 Chrome。

安全邊界:fetch 只允許 `http` / `https`,拒絕 `localhost`、loopback、private IP(含 CGNAT `100.64/10`、`0.0.0.0/8`)、link-local、metadata IP;IPv6 的 IPv4-mapped 與 NAT64(`64:ff9b::/96`)內嵌位址會還原後再檢查;每一段 redirect 後的目標 URL、以及從磁碟快取讀回的 URL,都重新檢查。要連本機開發伺服器可加 `--allow-local`(MCP 為 `allow_local`)—— 這只放行 **loopback**,private LAN、link-local 與 metadata 仍一律被擋,連 redirect 想跳到 `169.254.169.254` 也會被拒。

IP 黑名單的最終把關落在**連線層**:自訂的 reqwest DNS resolver 在解析當下就對每個位址跑同一套黑名單,只把通過的 IP 交給 reqwest 連線(全部被擋就直接拒絕)。因此 reqwest 撥號的就是「已驗證的那個 IP」——**DNS rebinding 已被防護**,不再有「安全檢查」與「實際連線」兩次獨立 DNS 解析、可被低 TTL 或惡意 DNS 在連線當下掉包的縫隙。字面 IP、scheme、localhost 等不需 DNS 的檢查仍在送出請求前先擋一道;TLS 的 SNI 與憑證驗證仍以原始 domain 進行。

## JS 動態站(headless fallback)

對 React/Vue 或任何「JS 才渲染內容」的站,純 HTTP 抓到的是空殼。`--js auto`(預設)會偵測「萃取文字極少 + 頁面 JS 偏重」,**自動**呼叫系統 Chrome/Edge 的 `--headless --dump-dom` 取得渲染後 DOM —— **零編譯依賴**(不綁瀏覽器引擎),只在必要時才用,保持輕量。

- `--js off` 完全不用 headless;`--js always` 強制每頁都 render。
- `--js-wait <ms>` 給慢載入站更多時間;`--js-wait-for "<css>"` 等特定元素出現才抓 —— 走 **CDP**(Chrome DevTools Protocol,透過 tokio-tungstenite 連 `ws://localhost`,無需 TLS),適合「要打 API 才出內容」的站。
- 找不到瀏覽器時可用環境變數 `RUSTBROWSER_CHROME` 指定執行檔路徑。
- **安全**:渲染的是不受信任的網頁,因此 Chrome **sandbox 預設保持開啟**(不再無條件帶 `--no-sandbox`)。在容器或以 root 執行、sandbox 無法初始化時,才用環境變數 `RUSTBROWSER_NO_SANDBOX=1` 明確關閉。渲染後的 DOM 也有 16 MiB 上限,避免惡意或超大頁面撐爆記憶體。

## 禮貌抓取與穩健性

抓取管線內建一層「對伺服器友善 + 容忍暫時性失敗」的網路政策,預設保持輕快、必要時才更謹慎:

- **自動重試(預設開)** — 暫時性失敗(連線/逾時錯誤、`429`、`5xx`)以指數退避 + jitter 重試 `--max-retries` 次(預設 2)。收到 `Retry-After` 會尊重它(上限 60 秒)。**不可重試**的情況絕不重試:SSRF 拒絕、`4xx`(429 除外)、scheme 錯誤。重試用盡後,最後的狀態碼會照常回傳(例如持續 503 就回 503,不是錯誤)。
- **每 host 並發上限(預設 4)** — `--per-host-concurrency` 限制同一 host 同時在飛的請求數,跨整批共用;`0` = 不限。
- **每 host 速率限制(預設關)** — `--rate-limit <每秒次數>` 為同一 host 的請求之間插入最小間隔(例如 `2` = 每 500ms 一次)。預設 `0` 不限速,維持原本的快。
- **robots.txt(opt-in,預設關)** — `--respect-robots` 會抓該 origin 的 `/robots.txt`(走同一套 SSRF 防護、每 host 快取一次),用 [`texting_robots`](https://crates.io/crates/texting_robots) 解析,跳過對本 UA 被 `Disallow` 的路徑。對「指向特定網址的擷取」採 **fail-open**:robots.txt 不存在(4xx)、抓取失敗或 5xx、無法解析時一律放行,不讓壞掉的 robots 端點擋住你明確要的頁面。需要 `robots` feature(預設已含)。

這些限制都是 per-host 且跨整批共用——`fetch_urls` / 多 URL 批次抓同一站時,並發與速率上限會正確套用到整批。

## 擷取 profile、token 預算、品質診斷

把「怎麼擷取、輸出多大、品質如何」變成可控、可觀測:

- **擷取 profile**(`--profile`,MCP `profile`)
  - `article`(預設)—— Readability 主文擷取,去掉 nav/廣告/footer。
  - `full` —— 整個 `<body>`(script/style/noscript 移除),**不過 Readability 過濾**。給 Readability 會過度刪的頁面:參考文件、結構化版面、落地頁。
  - `metadata` —— 只要 title + 一段摘要,最省 token 的「這頁在講什麼」。
  - (給 `--selector` 時 profile 自動讓位給選擇器。)
- **token 預算**(`--max-output-tokens` / MCP `max_output_tokens`)—— Markdown/text 輸出超過預算就截斷並附標記;優先在**段落邊界**切,單段太大時切前綴,標記本身也算進硬上限。token 數用 tiktoken(`stats` feature)精算,否則以字數估算。
- **品質診斷**(`--diagnostics` / MCP `diagnostics`)—— 回報 profile、raw bytes、output chars/tokens、`extraction_ratio`(過低代表可能被過度刪——換 `full`)、連結/表格數、是否用了 headless、是否被截斷,以及 `low_content` 警告。CLI 非 JSON 印到 stderr;JSON 格式則帶在結果裡。

函式庫另外提供 `distill_html(html, base_url, opts)` —— **不抓網路**、直接對手上的 HTML 跑整條擷取管線,供離線評測與「已有 HTML 想萃取」使用。倉庫內附**固定評測集**(`tests/fixtures/` + `tests/eval.rs`):用代表性頁面斷言主文留存、chrome 去除、各 profile 行為與 token 縮減門檻,作為擷取品質的回歸網。

## Browser Use:Actionable Observe(v1.1)

RB 的走向是 **RB-first Browser Use** —— Observe → Act → Verify → Fallback,Chrome 只當 fallback runtime。v1.1 完成第一步「Observe」:除了頁面摘要,RB 還能輸出一棵 **action tree**,告訴 agent 這頁有哪些**可操作**的東西。

- `--actions`(MCP `extract_actions`)抽出 **links / forms / buttons / downloads**,每個給穩定 `action_id`(`link_3`、`form_0.submit`…)。
  - **links** —— 一般導覽連結(可 follow)。
  - **forms** —— `method`、`action`(絕對 URL)、`fields`(name/type/value/options/required)、`submit_id`。可不開瀏覽器直接組 GET/POST。
  - **buttons** —— 表單外的獨立按鈕(多半是 JS 驅動,標示出來供後續 fallback 判斷)。
  - **downloads** —— 指向檔案的連結(從一般 links 分流出來),帶 `filename`。
- `--max-actions <n>` 對每類設上限,避免 action tree 爆量。
- MCP 新工具 **`observe_url`** —— 回傳頁面摘要 + action tree(JSON),專給「接下來能做什麼」的決策用;`fetch_url` / `fetch_urls` 維持相容(用 `extract_actions` 開啟)。

```powershell
rustbrowser fetch https://example.com/search --actions --format json
rustbrowser fetch <url> --actions --max-actions 30
```

> 設計刻意**不做** pixel-level click、不做完整 JS browser、不讓 agent 未經確認就送任意 POST。後續版本(session、表單提交、action loop、Chrome fallback broker)見 [`CHANGELOG.md`](CHANGELOG.md) 與內部路線圖。

## 使用方式:給 Claude Code 用(MCP)

`rustbrowser-mcp` 是 stdio MCP server,暴露三個工具,Claude 呼叫即可拿到精簡內容,**原始 HTML 完全不進對話**:

- **`fetch_url`** — 抓單一頁面
- **`fetch_urls`** — 一次並發抓多個頁面(比反覆呼叫 `fetch_url` 快得多)
- **`observe_url`** — 抓頁面摘要 **+ action tree**(links/forms/buttons/downloads,JSON),給 Browser Use「接下來能做什麼」的決策用

專案根目錄已附 `.mcp.json`(指向 release binary)。或手動註冊:

```powershell
# 專案層級(此專案可用)
claude mcp add rustbrowser -- D:\aiproject\RustBrowser\target\release\rustbrowser-mcp.exe

# 全域(任何專案可用)
claude mcp add --scope user rustbrowser -- D:\aiproject\RustBrowser\target\release\rustbrowser-mcp.exe
```

共同參數:`format`、`selector`、`stats`、`timeout_secs`、`max_bytes`、`no_cache`、`cache_ttl`、`extract_links`、`extract_tables`、`links_full`、`js`(off/auto/always)、`js_wait`、`js_wait_for`、`allow_local`(放行 loopback,預設 false)、`max_retries`(預設 2)、`per_host_concurrency`(預設 4)、`rate_limit`(每秒次數,預設 0=不限)、`respect_robots`(預設 false)、`profile`(article/full/metadata)、`max_output_tokens`、`diagnostics`(預設 false);`fetch_urls` 另有 `urls` 與 `concurrency`。

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

功能可用 Cargo features 拆分:

```powershell
# 完整預設 build:CLI + MCP + 精準 token stats + headless JS
cargo build --release

# 較瘦 CLI build:不編入 MCP、tiktoken-rs、CDP websocket 依賴
cargo build --release --no-default-features --features cli

# 指定完整 feature set
cargo build --release --no-default-features --features "cli mcp stats js robots"
```

CI 會執行 fmt、clippy、build、test、release build,並檢查 release binary 大小:CLI ≤ 10 MiB,MCP ≤ 15 MiB。

## 穩定性與版本 (Stability & versioning)

從 `1.0.0` 起,本專案遵循 [Semantic Versioning](https://semver.org/)。`1.x` 內**保證不破壞**的公開介面:

- **CLI** —— `fetch` 既有旗標與 `cache` 子命令的名稱與語意。
- **MCP** —— `fetch_url` / `fetch_urls` 工具與其參數名稱、型別。
- **函式庫** —— `rustbrowser` crate 的公開 API(`distill`、`distill_many`、`distill_html`、`DistillOptions`、`Distilled` 等)。
- **JSON 輸出** —— `--format json` / MCP `format=json` 之 `Distilled` 結構既有欄位。

**不在 semver 保證內**(可在 minor/patch 改動):

- 精確的 Markdown 文字輸出 —— 擷取與轉換的啟發法會持續改進,內容會變。
- token 估算的絕對數值(tokenizer 是近似)。
- 診斷欄位的新增、`low_content` 等門檻的調整。
- 內部模組、未公開項目、磁碟快取格式。

新增旗標/參數/欄位屬於相容變更(minor)。移除或改名既有者屬於破壞變更,只會在下一個 major。棄用會先在文件標記、保留至少一個 minor 週期。完整逐版變更見 [CHANGELOG.md](CHANGELOG.md);安全防護與威脅模型見 [SECURITY.md](SECURITY.md);凍結介面的完整參考見 [docs/API.md](docs/API.md)。

## 演進路徑

- ✅ **v0.1(MVP)** — 核心管線 + CLI:抓取 → 萃取 → Markdown → token 統計
- ✅ **v0.2** — 磁碟快取、批次並發抓取
- ✅ **v0.3** — MCP server,Claude Code 以原生工具 `fetch_url` / `fetch_urls` 呼叫
- ✅ **v0.4** — headless 自動 fallback(auto 偵測 JS 動態站)+ 連結/表格結構化擷取
- ✅ **v0.5** — 全頁連結擷取 · headless 等待控制(`--js-wait` / CDP `--js-wait-for`)· release 發布自動化
- ✅ **v0.6(強化)** — wiremock 端到端整合測試 · `--allow-local` loopback 豁免 · `cache` 維護子命令 · headless sandbox 預設開啟 + DOM 上限
- ✅ **v0.7(穩健性)** — headless DOM cap 改串流讀取真正限制記憶體 · `cache` 失敗回傳非零 exit code · MCP transport 明確處理斷線/錯誤(乾淨關閉、診斷只進 stderr)
- ✅ **v0.8(禮貌抓取)** — 暫時性失敗自動重試(指數退避 + `Retry-After`)· per-host 並發上限 · per-host 速率限制 · robots.txt(opt-in)
- ✅ **v0.9(擷取品質)** — 擷取 profile(article/full/metadata)· token 預算截斷 · 品質診斷 · `distill_html` 離線管線 + 固定評測集
- ✅ **v1.0(穩定版)** — CLI/MCP schema 凍結(+ 守門測試)· 完整安全文件(`SECURITY.md`)· semver 承諾 + `CHANGELOG.md` · CI 覆蓋 Linux/Windows/macOS · 三平台 release binaries + checksums
- ✅ **v1.1(Actionable Observe)** — action tree(links/forms/buttons/downloads + 穩定 action_id)· MCP `observe_url` 工具 · action token 上限 · action 抽取評測。Browser Use 的第一步:RB 先能告訴 agent「這頁有哪些可操作的東西」

## 技術棧

Rust · tokio · reqwest(rustls) · scraper · dom_smoothie · htmd · tiktoken-rs · clap · rmcp · futures · sha2 · dirs · tokio-tungstenite(CDP) · texting_robots(robots.txt)

---

*這個專案的本質:把「瀏覽網頁」這件事,縮減成 LLM 真正需要的最小資訊集。*
