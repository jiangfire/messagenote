# 自建服务端部署

三种方式，按省事程度排：

1. **Docker Compose**（推荐）—— 一条命令起一个完整的 MessageNote
2. **二进制 + Caddy** —— 不想用容器的话
3. 只跑服务端 —— 只用桌面端同步，不需要网页端

---

## 一、Docker Compose

```bash
git clone https://github.com/jiangfire/messagenote.git
cd messagenote
cp deploy/.env.example deploy/.env
# 编辑 deploy/.env，填一个至少 32 字符的令牌
docker compose --env-file deploy/.env up -d
```

起来之后：

- 浏览器打开 `http://<这台机器>:8080` 就是网页端
- 桌面端的「设置 → 同步」填同一个地址和同一个令牌

### 两个容器分别是什么

| 容器 | 干什么 |
| --- | --- |
| `messagenote-server` | 数据和同步。**不对外暴露端口** —— 只有 web 容器需要连到它 |
| `messagenote-web` | 一个 Caddy：托管网页端产物，并把 `/api/*` 反代给服务端 |

**为什么要分成两个、又只暴露一个端口**：服务端**刻意不带任何 CORS 头**。
把网页端和 API 放在不同的 origin 下，浏览器会直接拦掉所有 `/api` 请求。
同源部署从根上就不需要 CORS，也就没有"允许的来源配错了"这种一闪而过的错。
分开是因为"静态文件"和"有状态服务"的升级节奏不一样；只暴露一个端口是因为
同源这件事不能靠配置去赌。

### 数据在哪

全部状态在 `messagenote-data` 这个卷里的**一个 `.sqlite` 文件**里 ——
**附件（图片）的字节也在里面**。这是故意的：只认单文件的备份工具
（比如 Litestream）会静默漏掉放在数据库外面的字节，等你真去恢复的那天才发现
"笔记都在、图全没了"。

备份：

```bash
# 冷备份最简单
docker compose stop server
docker run --rm -v messagenote_messagenote-data:/data -v "$PWD:/backup" \
  alpine tar czf /backup/messagenote-$(date +%F).tar.gz -C /data .
docker compose start server
```

或者用 Litestream 持续复制那个文件（在线、不用停）。

### 上 TLS

compose 里那个 Caddy 只说 HTTP。公网部署有两条路：

- **前面再放一层反代**（另一个 Caddy / nginx / Cloudflare）终结 TLS，转给它。
- **直接用 `deploy/Caddyfile`**（裸机那份），把域名填进去 —— 它会自动申请证书。

**不管哪条路，反代那一层都要关掉响应缓冲**，否则实时推送整个不工作：

| 反代 | 要加的东西 |
| --- | --- |
| Caddy | `reverse_proxy ... { flush_interval -1 }` |
| nginx | `proxy_buffering off;` |
| Traefik | 默认不缓冲，通常不用改 |

原因：`/api/events` 是一条**永不结束**的 SSE 流，而反代默认攒够缓冲才往下发 ——
永不结束的响应永远攒不满。表现是连接建好了、一条事件都收不到，
**而且两端都不报错**。

### 镜像的可见性

镜像发在 GHCR（`ghcr.io/jiangfire/messagenote-server` / `-web`）。
GHCR 的包**默认是私有的**，即使仓库是公开的。要让别人能直接 `docker pull`，
去 GitHub 的包设置里把可见性改成 public：

`https://github.com/users/jiangfire/packages/container/messagenote-server/settings`

不想用预构建镜像的话，把 `docker-compose.yml` 里的 `build:` 注释打开即可 ——
两个 Dockerfile 都支持从源码构建。

---

## 二、二进制 + Caddy

```bash
# 服务端（静态版，不依赖任何系统库）
V=1.2.0   # 换成你想要的版本
curl -LO "https://github.com/jiangfire/messagenote/releases/download/v$V/messagenote-server_${V}_linux-x64-static"
chmod +x "messagenote-server_${V}_linux-x64-static"
sudo mv "messagenote-server_${V}_linux-x64-static" /usr/local/bin/messagenote-server

# 网页端产物
git clone https://github.com/jiangfire/messagenote.git && cd messagenote
pnpm install && pnpm build
sudo install -d /var/lib/messagenote/web
# **只拷这两样。** dist/index.html 是 Tauri 主窗口的壳，依赖 window.__TAURI__，
# 拷过去在浏览器里只会白屏。
sudo cp -r dist/assets dist/web.html /var/lib/messagenote/web/
```

然后按 `deploy/Caddyfile` 配 Caddy（把 `notes.example.com` 换成你的域名）。
服务用 `deploy/messagenote-server.service` 跑成 systemd 单元。

---

## 三、只跑服务端

```bash
MESSAGENOTE_TOKEN=$(openssl rand -base64 48) \
MESSAGENOTE_BIND=127.0.0.1:8787 \
MESSAGENOTE_DB=/var/lib/messagenote/messagenote.sqlite \
messagenote-server
```

桌面端填这个地址和令牌即可。网页端不需要。

---

## 关于发布产物的校验

每个 Release 都带一份 `SHA256SUMS.txt`。**安装包没有代码签名**，所以
Windows SmartScreen 会拦一道；校验值不能替代代码签名（它证明不了"这是作者发的"），
但它能证明"下载没被篡改"：

```bash
sha256sum -c SHA256SUMS.txt
# Windows:
# certutil -hashfile MessageNote_<版本>_x64-setup.exe SHA256
```

**注意区分两件事**：

| | 保护什么 | 用什么密钥 |
| --- | --- | --- |
| `SHA256SUMS.txt` | 你手动下载的文件没被篡改 | 无（就是个哈希） |
| `.exe.sig` | **自动更新链路**没被劫持 | Tauri 自己的 minisign 密钥对 |

自动更新的签名是**必须的**：更新包是从网上拿的，不校验就等于让任何人
给你换一个 exe。公钥写在 `src-tauri/tauri.conf.json` 里，私钥只在
维护者机器和 GitHub Secrets 里。

---

## 维护者：发一个版本

```bash
# 1. 三处版本号一起改（Cargo.toml / package.json / tauri.conf.json）
# 2. 提交、打 tag、推
git tag -a v1.3.0 -m "..." && git push origin v1.3.0
```
Release workflow 会构建并发布：Windows 安装包 + 免安装版 + 更新签名 +
`latest.json` + 三个平台的服务端二进制 + `SHA256SUMS.txt` + 许可证，
同时把两个容器镜像推到 GHCR。

**`latest.json` 必须在 latest release 上** —— 桌面端配的 endpoint 就是
`releases/latest/download/latest.json`。

### 本地构建必须先设两个环境变量

`tauri.conf.json` 里配了 `createUpdaterArtifacts: true` 和公钥，此时**没有私钥
构建会直接失败**：

```
A public key has been found, but no private key.
```

```powershell
$env:TAURI_SIGNING_PRIVATE_KEY = Get-Content "$env:USERPROFILE\.tauri\messagenote.key" -Raw
# **必须显式设成空串。** 不设的话 tauri 会弹交互式密码提示（在 CI 里就是卡到超时）
$env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD = ""
pnpm tauri build
```

### 自动更新只能从"第一个带它的版本"开始

更新是**客户端主动去查**的。一个装在用户机器上、本身不含更新插件的老版本，
不会因为你发了新版就突然会升级 —— 它得先被人手动装一次新版本，
从那之后才是自动的。

所以：**在自动更新上线之前发布的那些版本（v1.2.0 及更早），用户需要手动装一次
带更新插件的新版本**，之后就不用再管了。
