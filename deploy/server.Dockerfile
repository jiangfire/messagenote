# 同步服务端的容器镜像。
#
# **一个容器 = 一个进程 + 一个可挂载的卷**。数据全在那个卷里的一个 .sqlite
# 文件里 —— 备份就是把那个文件复制走（或者用 Litestream 跟它），
# 这也正是附件字节存进数据库而不是放目录的原因。
#
# 多阶段：构建阶段带整套 Rust 工具链，运行阶段只留一个二进制。

# ---------------------------------------------------------------- 构建
FROM rust:1-slim-bookworm AS build

# 不用额外装 gcc：`rust:slim` 自带 C 编译器，rusqlite 的 bundled SQLite
# 在这里编得过（这是实测的，不是猜的 —— 第一次镜像构建就是这么过的）。
WORKDIR /src

# 整份源码一起拷。`.dockerignore` 已经把 target/、node_modules/、dist/ 排除了，
# 所以上下文很小。
#
# **不做"先拷清单、再拷源码"那套缓存分层。** 试过的写法是"先拷各成员清单 →
# 编一遍依赖 → 删掉 src-tauri → 再拷真源码"，但删掉 src-tauri 会让 workspace
# 解析失败（根 Cargo.toml 的 members 里有它），第二次构建直接报
# "failed to load manifest for workspace member"。为省一层缓存把构建搞得
# 这么脆不值得 —— 代价只是改一行源码要重编一遍依赖。
COPY . .

RUN cargo build --release --locked -p messagenote-server

# ---------------------------------------------------------------- 运行
FROM debian:bookworm-slim

# 不以 root 跑。这个进程对外的口子是公网，能少一个 root 就少一个。
RUN useradd --system --uid 10001 --no-create-home messagenote

COPY --from=build /src/target/release/messagenote-server /usr/local/bin/messagenote-server

# 默认监听所有网卡（容器里的 127.0.0.1 只有容器自己看得到）。
# **TLS 不在这里终结** —— 前面要有一层反代，见 deploy/README.md。
ENV MESSAGENOTE_BIND=0.0.0.0:8787
ENV MESSAGENOTE_DB=/data/messagenote.sqlite

RUN mkdir -p /data && chown messagenote /data
VOLUME ["/data"]
USER messagenote
EXPOSE 8787

# 容器里的 1 号进程必须自己处理信号。服务端直接跑在 ENTRYPOINT 上，
# 不套 shell —— 套了的话 SIGTERM 打到 shell 上，进程收不到、停不下来。
ENTRYPOINT ["messagenote-server"]
