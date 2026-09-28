# 网页端的容器镜像：一个 Caddy，托管静态产物并把 /api/* 反代给服务端。
#
# **同源是刻意的。** 服务端自己不带任何 CORS 头，所以网页端和 API 必须
# 在同一个 origin 下 —— 这个容器就是干这件事的：对外只暴露一个端口。
#
# 它和 deploy/Caddyfile（裸机部署那份）行为一致，区别只是地址来自环境变量。

# ---------------------------------------------------------------- 构建
FROM node:24-slim AS build

WORKDIR /src

# 版本写死，和 .github/workflows 里保持一致。
# 不用 corepack：它在 Node 新版本里正在被移除，写死更稳。
RUN npm install -g pnpm@12.5.1

COPY package.json pnpm-lock.yaml ./
RUN pnpm install --frozen-lockfile

COPY . .
# 会同时跑 tsc --noEmit 和 vite build
RUN pnpm build

# ---------------------------------------------------------------- 运行
FROM caddy:2-alpine

COPY deploy/Caddyfile.container /etc/caddy/Caddyfile
COPY --from=build /src/dist/web.html /srv/web.html
COPY --from=build /src/dist/assets /srv/assets

EXPOSE 80
