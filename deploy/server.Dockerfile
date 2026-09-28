# 同步服务端的容器镜像。
#
# **一个容器 = 一个进程 + 一个可挂载的卷**。数据全在那个卷里的一个 .sqlite
# 文件里 —— 备份就是把那个文件复制走（或者用 Litestream 跟它），
# 这也正是附件字节存进数据库而不是放目录的原因。
#
# 多阶段：构建阶段带整套 Rust 工具链，运行阶段只留一个二进制。

# ---------------------------------------------------------------- 构建
FROM rust:1-slim-bookworm AS build

# rusqlite 用的是 bundled 特性（SQLite 源码一起编），所以需要一个 C 编译器。
# rust:slim 镜像里已经有 gcc，这里不用额外装。
WORKDIR /src

# 先只复制清单，让依赖层能被缓存 —— 改一行源码不该重编 400 个 crate。
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY src-tauri/Cargo.toml ./src-tauri/Cargo.toml

# 上面那步复制的 crates 只有清单不够，真正的构建要全部源码。
# 分两次 COPY 是为了让依赖编译这一层在"只改源码"时命中缓存。
RUN mkdir -p src-tauri/src && echo 'fn main() {}' > src-tauri/src/main.rs \
    && echo '' > src-tauri/src/lib.rs \
    && cargo build --release --locked -p messagenote-server \
    && rm -rf src-tauri

COPY crates ./crates
# 源码变了，但依赖没变 —— 这一次只会重编我们自己的 crate。
RUN touch crates/server/src/main.rs && cargo build --release --locked -p messagenote-server

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
