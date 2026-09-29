# 桌面端实机验证

[`scripts/web-e2e/`](../web-e2e/README.md) 验的是**网页端**（真实浏览器 + CDP）。
这里验的是**桌面端** —— 那些 `cargo test` 和浏览器 E2E 都覆盖不到的地方。

## 为什么需要它

`cargo test` 验的是 Rust 侧的库；`pnpm build` 只证明能编译；浏览器 E2E 跑的是
网页端。三者都证明不了**桌面端特有的那几段**：

- **Tauri 命令的接缝**：Rust 那边是 `utc_offset_minutes`，传到 JS 是
  `utcOffsetMinutes`，写错就得到一句"缺少参数" —— 而 `cargo test` 全绿，
  因为它根本没经过 IPC 这一层。
- **WebView2 的真实行为**：真实剪贴板粘贴、`tauri::ipc::Response` 的原始字节通道。
- **界面刷新**：同步把远端变更落库之后，界面到底跟不跟着动。

这套东西抓到过三个真问题：

1. **界面不刷新** —— 远端变更 0.28 秒就进了本地库，界面上 70 秒都不出现
   （桌面端的 `ApiProvider` 漏了 `subscribeChanges`）。
2. **白等 15 秒** —— 一条测试用 `read_json()` 去读 `/api/events`，
   而那是故意永不结束的流，空闲时一个字节都不发。
3. **偶尔要等 42.9 秒** —— SSE 退避把"空闲期被掐掉的**健康**连接"判成连接失败。

## 前提

1. **应用要用 `pnpm tauri build --no-bundle` 构建。**
   `cargo build` 出来的 debug 版连的是 `devUrl`（Vite 开发服务器），不先跑
   `pnpm dev` 的话窗口能开、标题也对、前台探测也通过 —— 但里面是一张
   `ERR_CONNECTION_REFUSED` 错误页：没有 React、没有输入框，于是合成按键
   全部落空，**而所有护栏都"通过"了**。
2. **带 WebView2 调试端口启动**，CDP 才连得上：

   ```powershell
   $env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS = '--remote-debugging-port=9222'
   .\target\release\messagenote.exe
   ```

3. `python` 在 PATH 里（只用标准库），Node 24+（自带全局 `WebSocket`）。

## 工具

| 文件 | 干什么 |
| --- | --- |
| `cdp-eval.mjs` | 极简 CDP 求值器：`node cdp-eval.mjs '<url正则>' '<JS表达式>'`。**其它脚本都靠它**。窗口靠 URL 区分：主窗口是 `tauri\.localhost/$`，浮层是 `capture\.html`。 |
| `paste-verify.ps1` + `paste-db.py` | 合成真实全局快捷键 + `Ctrl+V` 粘一张 12×12 的 PNG，再查库断言"字节的 sha256 == 正文里写的 sha"且"解出来真的是 12×12"。**跑之前保存剪贴板、跑完还原。** |
| `export-verify.mjs` + `export-db.py` | 用应用自己的写命令造数据 → 调 `export_markdown` → 断言文件树、front-matter、以及**附件相对路径真能取到那份字节**（不是"字符串长这样"）。 |
| `copy-verify.mjs` | 单条复制：断言图被内联成 data URI，并在 Node 这边**独立解码**再和原字节逐字节比。 |
| `sse-latency.mjs` + `sse-latency-db.py` | 量"服务端写一条 → 桌面端**界面**（或本地库）多久看到"，用来分辨 SSE 和 45 秒轮询兜底哪条在起作用。 |
| `capture.ps1` / `shot-overlay-live.ps1` | 截主窗口 / 截捕获浮层，用来**看**界面到底显示了什么。 |

## 怎么跑

```powershell
# 前端 + 桌面端（内嵌 dist、用生产 CSP）
pnpm build
pnpm tauri build --no-bundle

# 带调试端口启动
$env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS = '--remote-debugging-port=9222'
Start-Process .\target\release\messagenote.exe

# 单条复制（不用起服务端）
node scripts\desktop-verify\copy-verify.mjs

# 全量导出
node scripts\desktop-verify\export-verify.mjs
python scripts\desktop-verify\export-db.py

# 粘贴图片。**会占桌面约 5 秒并抢前台焦点**，所以脚本自带护栏：
# 只有在确认浮层已经可见**且处于前台**之后才发后续按键，否则立刻中止
# （exit 2），不会把内容误敲进你当时在用的窗口。
pwsh -NoProfile -File scripts\desktop-verify\paste-verify.ps1
```

产物和输出都落在仓库根的 `.scratch/` 下（gitignore）。`export-*` 那对可以用
`--work <目录>` / 第一个参数换工作目录，想跑两次对比时有用。

## 三个坑（都踩过，写在这里免得下一个人再踩）

- **截图在 Tauri v2 上不可信。** DirectComposition，`CopyFromScreen` 抓出来是
  空白 —— README 里也写过。能看见真相的是 CDP：直接读 DOM。
- **`/api/events` 不能"读完整个 body"。** 它是故意永不结束的流，空闲时一个字节
  都不发；`read_json()` 这类接口会一直等到 15 秒的心跳注释才报错。**只看状态码。**
- **别用点按钮的方式验带原生对话框的路径。** 选目录会弹 Windows 原生对话框，
  CDP 驱动不了；那一段只能人点。用 `__TAURI_INTERNALS__.invoke` 直接调命令，
  验的是**命令接缝**（也就是最容易错的那一层），并把这个边界说清楚，
  而不是假装验过了。
