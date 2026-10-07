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
| `fake-server.mjs` | 替身服务端：只实现桌面端真正会打的 5 个端点，用环境变量控制行为（心跳间隔、上传必失败、握手故意延迟）。**下面 R1/R7/R8 三套都靠它。** |
| `sse-lifetime.mjs` | **R1**：量 SSE 连接活了多久。替身把心跳调成 8 秒、观察 40 秒 —— 修复前 25 秒必被 `timeout_recv_body` 的总预算掐断。 |
| `sync-starve-verify.mjs` | **R7**：让 `/api/blob` 全部返 500，断言拉取的消息**仍然进了本地库**。 |
| `mainthread-verify.mjs` | **R8**：让 `test_sync_connection` 打到延迟 8 秒的端点，期间从 CDP 连续探测界面，量中位延迟。 |
| `paste-verify.ps1` + `paste-db.py` | 合成真实全局快捷键 + `Ctrl+V` 粘一张 12×12 的 PNG，再查库断言"字节的 sha256 == 正文里写的 sha"且"解出来真的是 12×12"。**跑之前保存剪贴板、跑完还原。** |
| `export-verify.mjs` + `export-db.py` | 用应用自己的写命令造数据 → 调 `export_markdown`（全量 + 四种筛选）→ 断言文件树、front-matter、**附件相对路径真能取到那份字节**（不是"字符串长这样"）、筛选的区间端点含不含在内、以及导出面板本身（菜单 → 面板 → 控件 → 日期写反被拦住）。 |
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

# 导出（全量 + 筛选）与导出面板
node scripts\desktop-verify\export-verify.mjs
python scripts\desktop-verify\export-db.py

# 粘贴图片。**会占桌面约 5 秒并抢前台焦点**，所以脚本自带护栏：
# 只有在确认浮层已经可见**且处于前台**之后才发后续按键，否则立刻中止
# （exit 2），不会把内容误敲进你当时在用的窗口。
pwsh -NoProfile -File scripts\desktop-verify\paste-verify.ps1
```

产物和输出都落在仓库根的 `.scratch/` 下（gitignore）。`export-*` 那对可以用
`--work <目录>` / 第一个参数换工作目录，想跑两次对比时有用。

## R1 / R7 / R8：三套同步相关的真机验证

这三项都出在 `src-tauri/src/sync.rs` 与 `sse.rs`，`cargo test` 里有对应的
单元测试，但**都覆盖不到真机这一段**：真实 HTTP 往返、真实 ureq 超时语义、
真实主线程。所以各配了一套脚本。

三套共用一个**替身服务端**（`fake-server.mjs`）。不用真服务端是因为它们
各自需要真服务端给不了的东西：观察连接活了多久、让上传失败但拉取成功、
让握手慢下来。替身只实现桌面端真正会打的那 5 个端点。

```powershell
# R1：SSE 连接必须活过 25 秒。替身把心跳调成 8 秒，于是"活过 34 秒"= 两次心跳
$env:MESSAGENOTE_TOKEN = 'sse-verify-token-0123456789abcdefghijklmnop'
$env:HEARTBEAT_MS = '8000'
node scripts\desktop-verify\fake-server.mjs     # 前台或另开一个窗口
node scripts\desktop-verify\sse-lifetime.mjs

# R7：上传全败时拉取不能被饿死。FAIL_BLOB=1 让 /api/blob 全部返 500
$env:FAIL_BLOB = '1'
node scripts\desktop-verify\fake-server.mjs
node scripts\desktop-verify\sync-starve-verify.mjs

# R8：慢命令飞行期间界面不能冻结。SLOW_MS 让握手端点故意慢 8 秒
$env:FAIL_BLOB = '0'; $env:SLOW_MS = '8000'
node scripts\desktop-verify\fake-server.mjs
node scripts\desktop-verify\mainthread-verify.mjs
```

三套脚本都是**只读 DOM / 查库 + 计时**，不合成按键、不抢前台焦点，可以和
其它验证共存，也可以在应用开着的时候随时跑。

## 六个坑（都踩过，写在这里免得下一个人再踩）

- **截图在 Tauri v2 上不可信。** DirectComposition，`CopyFromScreen` 抓出来是
  空白 —— README 里也写过。能看见真相的是 CDP：直接读 DOM。
- **`/api/events` 不能"读完整个 body"。** 它是故意永不结束的流，空闲时一个字节
  都不发；`read_json()` 这类接口会一直等到 15 秒的心跳注释才报错。**只看状态码。**
- **别用点按钮的方式验带原生对话框的路径。** 选目录会弹 Windows 原生对话框，
  CDP 驱动不了；那一段只能人点。用 `__TAURI_INTERNALS__.invoke` 直接调命令，
  验的是**命令接缝**（也就是最容易错的那一层），并把这个边界说清楚，
  而不是假装验过了。**面板本身**（不含那个对话框）是可以点着验的：
  `export-verify.mjs` 里第二段就是点 ⋯ → 点「导出记录…」→ 改控件 → 读 DOM。
  给 React 的受控输入赋值要走原生 setter（`HTMLInputElement.prototype` 上的
  那个 `value` setter），直接 `el.value = x` 它收不到，React 记着自己写进去的值。
- **替身服务端在 pull 响应里推 `changed` 会形成反馈回路。** 拉取是被动读取，
  给订阅者推信号就变成「拉取 → 唤醒同步 → 再拉取」。实测 60 秒内 16000+ 次
  pull，顺带耗尽本机临时端口，客户端报 `os error 10048` —— 验证结论全被污染。
  真服务端也只在**写入**时推。替身因此在 pull 上加了速率闸门（>200 次就拒答），
  好让回路响亮地失败，而不是给出一个假的结论。
- **拉下来的消息必须落在本地真实存在的频道里。** `message.channel_id` 有外键
  约束，指向不存在的频道时整条 INSERT 失败 —— 于是"拉取执行了"却查不到任何
  痕迹，脚本会误判成"pull 被饿死"。替身默认用 `inbox`，可用 `CHANNEL_ID` 改。
- **判读标记必须每轮唯一。** R7 最初用固定的 `R7-PULL-MARKER`，而上一轮的
  消息还留在库里、且固定 id + HLC 幂等意味着再拉也不会更新它 —— 脚本于是拿着
  残留判了一个**假通过**（那一轮 `pulled: 0` 却"通过"了）。现在替身每次启动
  生成一个 `runId` 编进正文，脚本在触发前先断言它此刻不在库里。
  另外 R7 判的是**库**而不是界面：那一轮同步的最终状态是失败，前端拿到失败
  状态就不重载时间线，于是"库里有了、界面上没出现"——那是正确行为。
- **R8 的长命令得真的长。** 起初用 `export_markdown`，在这个小库上只要 33 毫秒，
  压根占不住主线程，"没冻结"是因为命令太短而不是因为修好了。换成
  `test_sync_connection` 打到延迟 8 秒的握手端点，结论才站得住。
