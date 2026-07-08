---
name: verify-rb
description: RustBrowser 改動宣告完成前的驗證流程（build/test/clippy + 真實煙測）。任何 code 改動要宣告完成、或使用者說「驗證 RB」「verify rb」時執行。
---

# Verify RB

宣告完成的唯一途徑。全部通過才算過；任一步失敗就回去修，不降級標準。

## 1. 環境（每個 shell 都要先設）

```powershell
$env:Path = "$env:USERPROFILE\.cargo\bin;D:\aiproject\RustBrowser\.tools\nasm-2.16.03;$env:Path"
$env:CARGO_HTTP_CHECK_REVOKE = "false"
```

## 2. 靜態檢查（= CI 同款）

```powershell
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
```

## 3. 測試

```powershell
cargo test
```

全部 suite（unit / integration / session / eval / schema_freeze / benchmark）0 failed 才算綠。

## 4. 真實煙測（動過 fetch / render / MCP surface 時）

```powershell
cargo build --release
# CLI 真站煙測
target\release\rustbrowser.exe https://example.com --stats
# MCP 煙測（動過 MCP surface 時）：以 stdio 打 initialize + fetch_url
```

## 5. 證據

回報必附：測試 summary 行（`test result: ok. N passed`）+ 煙測輸出片段。
「應該可以了」不是證據；「實測 X → 得到 Y」才是。
