# 网页端浏览器端到端测试

在**真实浏览器**里跑一遍网页端：登录 → 记一条 → 检索 → 建频道 → 归档 → 删频道
→ 刷新，全程收集页面报错。

## 为什么需要它

`cargo test` 验的是 Rust 侧；`pnpm build` 验的是能不能编译；用 Node 直接调
`httpApi` 验的是请求形状对不对。这三样都证明不了**"React 挂载了、事件接上了、
页面真的能用"**。

它抓到过的真问题（都是别的手段抓不到的）：

- **删频道会把里面的笔记一起删掉**，而确认框写着"其中的记录会回到收件箱"。
  代码、类型检查、单元测试全都没有提示。
- 网页端的空状态提示"按 Ctrl+Shift+Space 唤起窗口" —— 浏览器里没有那个快捷键。

## 怎么跑

需要一个**同源**的环境（网页端和 `/api` 在同一个 origin），因为服务端不带
CORS 头。`serve.mjs` 就是干这个的：托管 `dist/` 并把 `/api/*` 反代给 Rust 服务端。

```bash
# 1. 构建
pnpm build
cargo build --release -p messagenote-server

# 2. 起服务端（记下令牌）
MESSAGENOTE_TOKEN=<至少32字符> MESSAGENOTE_DB=/tmp/e2e.sqlite \
  MESSAGENOTE_BIND=127.0.0.1:8803 ./target/release/messagenote-server &

# 3. 起同源服务器
node scripts/web-e2e/serve.mjs "$PWD/dist" http://127.0.0.1:8803 8804 &

# 4. 起一个带调试端口的浏览器
msedge --headless=new --disable-gpu --no-first-run \
  --remote-debugging-port=9222 --user-data-dir=/tmp/e2e-profile about:blank &

# 5. 跑
node scripts/web-e2e/browser-e2e.mjs http://127.0.0.1:8804/ <令牌> shots
```

Windows 上 Edge 一般在
`C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe`。Chrome 同理。

截图落在 `shots/`，失败时会指出是哪一条断言。

## 几点实现上的注意

- **`confirm()` 在 headless 下默认自动返回 false。** 不接管的话，所有"确认删除"
  的分支都不会执行，测试会以"什么都没发生"收场。`cdp.mjs` 里接管了，并把弹窗
  文本记下来 —— 于是可以断言"**对话框说的是不是真话**"。
- **往 React 受控输入里填值不能直接赋 `el.value`。** React 在 value 上装了
  setter 拦截，必须用原型上的原生 setter 写，再派发 `input` 事件，否则界面看起来
  填上了、状态其实是空的。
- **测试会先经 API 把服务端数据清空**（`resetServer`），所以可以反复跑。
- **断言要防"空跑"。** 比如"删频道后记录没消失"这条，如果不先把记录真的归档进
  频道，频道里本来就是空的，删了当然也不会少 —— 测试会一路绿过去。第一版就是
  这么写的，直到把记录放进去才红。
