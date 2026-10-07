//! HTTP 接口层。
//!
//! 同步（桌面端用）：
//! - `GET  /api/health`         探活（不需要 token，泄露的信息为零）
//! - `GET  /api/sync/handshake` 鉴权过的握手
//! - `GET  /api/sync/pull`      拉取变更
//! - `POST /api/sync/push`      推送变更
//!
//! 读取（网页端用）：
//! - `GET  /api/timeline`        时间线，scope + 键集游标
//! - `GET  /api/timeline/stats`  侧边栏那两个计数
//! - `GET  /api/channels`
//! - `GET  /api/tags`
//! - `GET  /api/search`
//!
//! 这些读取全部走 `messagenote-store`，和桌面端是**同一份**查询实现 ——
//! 网页端看到的不是"另一个长得像的时间线"。
//!
//! 写入（网页端用，**由服务端代笔**）：
//! - `POST   /api/message`              记一条
//! - `PATCH  /api/message/{id}`         改正文
//! - `DELETE /api/message/{id}`
//! - `POST   /api/message/{id}/move`    换频道
//! - `PUT    /api/message/{id}/tags`    整体替换标签
//! - `POST   /api/channel`              建频道
//! - `PATCH  /api/channel/{id}`         改名
//! - `DELETE /api/channel/{id}`         删频道（连同里面的消息）
//!
//! 代笔的意思是：服务端以**一台设备的身份**生成 HLC、写进变更日志、分配 seq，
//! 桌面端下次同步照常拉到。浏览器因此完全不需要 HLC、合并和冲突处理 ——
//! 裁定权仍然只有 `core::merge` 那一份。代价是网页端写入需要联网。
//!
//! 刻意用朴素的 REST 而不是 WebSocket：同步是"客户端主动推拉"的模型，
//! REST 更好调试（curl 就能复现问题），而实时推送（SSE）等 S4 再说。

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post, put};
use axum::{Json, Router};
use futures_util::stream::{self, Stream};
use tokio::sync::broadcast::error::RecvError;

use messagenote_core::hlc::now_ms;
use messagenote_core::models::{
    Channel, Message, MessagePage, SearchPage, TagCount, TimelineStats,
};
use messagenote_core::wire::{
    BlobMissingRequest, BlobMissingResponse, BlobResponse, CreateChannelRequest,
    CreateMessageRequest, EditMessageRequest, HealthResponse, LoginRequest, MoveMessageRequest,
    PullQuery, PullResponse, PushRequest, PushResponse, RenameChannelRequest, SearchQuery,
    SessionResponse, SetTagsRequest, TimelineQuery, PROTOCOL_VERSION,
};
use messagenote_store::{Cursor, Scope};

use crate::error::{ServerError, ServerResult};
use crate::store::Store;

/// 单批变更数上限。防止一次请求把内存吃满（这个服务面向公网，
/// 不能假设请求方一定是自己的客户端）。
const MAX_BATCH: usize = 1000;

/// 时间线一页的默认条数。和桌面端 `list_timeline` 的默认值对齐 ——
/// 同一个时间线在两端翻页的手感不该不一样。
const DEFAULT_TIMELINE_LIMIT: i64 = 120;

/// 检索默认条数。同样和桌面端对齐。
const DEFAULT_SEARCH_LIMIT: i64 = 60;

/// 一次能问多少个 sha 的"你有没有"。
const MAX_BLOB_QUERY: usize = 2000;

/// SSE 心跳间隔。
///
/// 比任何一个中间层的空闲超时都要短。Caddy 默认没有空闲超时，但反代和 NAT
/// 常常有 30~60 秒的，所以取 15 秒。
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);

/// 附件上传的体积上限，和 `core::attachment` 里的那个保持一致。
///
/// axum 的默认请求体上限是 **2 MB**，不改的话任何一张真实照片都会被 413 挡掉，
/// 而且报的是"请求体过大"这种和附件八竿子打不着的错。
const MAX_BLOB_BODY: usize = messagenote_core::attachment::MAX_ATTACHMENT_BYTES;

/// JSON 请求体的上限。
///
/// **axum 的默认是 2 MB，而这个默认值在这里是有害的**，两个后果：
///
/// - `/api/sync/push` 一次推一整批变更（桌面端 `BATCH` = 400 条）。一批变更的
///   JSON 总量越过 2 MB 就 413，而桌面端会**原批重试** —— 于是这台设备的同步
///   永久循环在"推 → 413 → 再推"里，再也出不去。服务端 `MAX_BATCH = 1000`
///   在这个限制下也永远达不到。
/// - `/api/message` 一条超大正文直接 413，报的还是"请求体过大"这种和
///   "内容太长了"八竿子打不着的错。
///
/// 上限取正文上限 [`normalize::MAX_BODY_CHARS`] 的两倍再留些余量：正文按
/// 字符算（64K 汉字 = 192 KB），加上 JSON 转义、字段名和一批变更的元数据，
/// 留出足够的空间又不至于让请求体无上限地涨。
/// 上限按「一批变更里装得下现实中的笔记」来定：`BATCH` 是 400 条，
/// 一条正文上限 64K 字符。全按上限算会是 76 MB，那属于病态情况；
/// 现实里一条笔记几百到几千字，400 条也就几 MB。取 32 MB 给足余量，
/// 同时不至于让请求体无上限地涨。
const MAX_JSON_BODY: usize = 32 * 1024 * 1024;

pub struct AppState {
    pub store: Store,
    /// 单用户场景下就是一个长期凭据。多用户才需要账号体系。
    pub token: String,
}

pub fn router(state: Arc<AppState>) -> Router {
    let protected = Router::new()
        .route("/api/sync/handshake", get(handshake))
        .route("/api/sync/pull", get(pull))
        // push 一批 JSON，必须放开请求体上限 —— 见 MAX_JSON_BODY。
        .route(
            "/api/sync/push",
            post(push).layer(DefaultBodyLimit::max(MAX_JSON_BODY)),
        )
        .route("/api/timeline", get(timeline))
        .route("/api/timeline/stats", get(timeline_stats))
        .route("/api/channels", get(channels))
        .route("/api/tags", get(tags))
        .route("/api/search", get(search))
        // 写入。全部由服务端代笔 —— 见文件头的说明。
        // 写端点也要放开请求体上限：一条长笔记不该撞上 axum 那个 2 MB 的默认值。
        .route(
            "/api/message",
            post(create_message).layer(DefaultBodyLimit::max(MAX_JSON_BODY)),
        )
        .route(
            "/api/message/{id}",
            patch(edit_message)
                .delete(remove_message)
                .layer(DefaultBodyLimit::max(MAX_JSON_BODY)),
        )
        .route(
            "/api/message/{id}/move",
            post(move_message).layer(DefaultBodyLimit::max(MAX_JSON_BODY)),
        )
        .route(
            "/api/message/{id}/tags",
            put(set_message_tags).layer(DefaultBodyLimit::max(MAX_JSON_BODY)),
        )
        .route("/api/channel", post(create_channel))
        .route(
            "/api/channel/{id}",
            patch(rename_channel).delete(remove_channel),
        )
        // 附件。上传要放宽请求体上限 —— axum 默认只给 2 MB，
        // 不改的话任何一张真实照片都会被挡掉。
        .route(
            "/api/blob",
            post(upload_blob).layer(DefaultBodyLimit::max(MAX_BLOB_BODY)),
        )
        .route("/api/blob/missing", post(missing_blobs))
        .route("/api/blob/{sha256}", get(download_blob))
        // 网页端导出。**挂在鉴权后面**：它能读走整个库，是这个服务上最重的
        // 一个只读动作，绝不能匿名。
        .route("/api/export.zip", get(export_zip))
        // 实时推送。挂在鉴权后面，所以客户端必须能带 Authorization 头 ——
        // 浏览器的 EventSource **不能**自定义请求头，这就是网页端改用
        // fetch + 手动解析 SSE 的原因（见 web 端的 sse.ts）。
        .route("/api/events", get(events))
        .route_layer(middleware::from_fn_with_state(state.clone(), require_token));

    Router::new()
        // 存活探针刻意**不鉴权**：给 Caddy、监控、uptime-kuma 用，
        // 泄露的信息只有"这里有个 MessageNote 服务端"。
        // 代价是它回答不了"我的令牌对不对" —— 所以另有 handshake。
        .route("/api/health", get(health))
        // 登录**不能**挂在鉴权中间件后面 —— 它就是鉴权本身。
        // 它在处理函数内部用常量时间比较校验长期令牌。
        .route("/api/session", post(login).delete(logout))
        .merge(protected)
        .with_state(state)
}

/// 从请求头里取出 Bearer 凭据。取不到就是空串。
fn bearer(headers: &HeaderMap) -> &str {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("")
}

/// 定长比较，避免通过响应时间逐字节猜 token。
///
/// 长度本身会在比较前就泄露，但 token 长度不是秘密，这里只防内容泄露。
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// 会话续期之后把这个头带回给客户端。
///
/// 网页端把它写进 localStorage —— 见 `store::session_is_valid` 的说明：
/// 服务端在滑动续期，但客户端只看自己存的那个值，于是**活跃用户每 7 天
/// 仍��被踢回登录页**，而且很可能正在写到一半。
const HEADER_SESSION_EXPIRES: &str = "X-Session-Expires";

/// 鉴权：**长期令牌**（桌面端的同步客户端）或**短期会话**（网页端）都放行。
///
/// 两条路并存是刻意的：桌面端本来就把令牌存在自己机器的数据库里，让它改走
/// 登录没有收益；网页端才需要"会过期、能吊销"的凭据。
async fn require_token(
    State(state): State<Arc<AppState>>,
    req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let presented = bearer(req.headers());

    // 长期令牌优先：它不需要查库（桌面端每轮同步都要打这几个端点），
    // 也不该收到续期头 —— 它没有过期时间，桌面端自己管。
    if constant_time_eq(presented.as_bytes(), state.token.as_bytes()) {
        return Ok(next.run(req).await);
    }

    // 会话这条路拿到的是**续期之后**的过期时刻（None = 无效）。
    let Some(expires) = state.store.session_is_valid(presented).unwrap_or(None) else {
        // 记日志但**不回显任何细节**：不告诉对方凭据是对是错、格式对不对。
        tracing::warn!("鉴权失败，已拒绝请求");
        return Err(StatusCode::UNAUTHORIZED);
    };

    let mut res = next.run(req).await;
    // 客户端拿它更新本地那份过期时间 —— 不带的话，滑动续期对它不可见。
    if let Ok(v) = expires.to_string().parse() {
        res.headers_mut().insert(HEADER_SESSION_EXPIRES, v);
    }
    Ok(res)
}

// ---------------------------------------------------------------- 会话

/// 用长期令牌换一个短期会话。
async fn login(
    State(state): State<Arc<AppState>>,
    Json(req): Json<LoginRequest>,
) -> ServerResult<Json<SessionResponse>> {
    if !constant_time_eq(req.token.as_bytes(), state.token.as_bytes()) {
        // 和中间件一样不透露细节，但要记日志 —— 这可能是有人在试令牌
        tracing::warn!("登录失败：长期令牌不对");
        return Err(ServerError::Unauthorized("令牌不对".into()));
    }
    Ok(Json(state.store.create_session()?))
}

/// 退出登录：吊销当前会话。
///
/// 一律回 204，不告诉调用方"这个会话到底存不存在" —— 那是一个没必要的
/// 探测面。长期令牌不受影响（它是用户的凭据，不是会话）。
async fn logout(State(state): State<Arc<AppState>>, headers: HeaderMap) -> StatusCode {
    let presented = bearer(&headers);
    if !presented.is_empty() && !constant_time_eq(presented.as_bytes(), state.token.as_bytes()) {
        let _ = state.store.drop_session(presented);
    }
    StatusCode::NO_CONTENT
}

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse {
        ok: true,
        protocol: PROTOCOL_VERSION,
        server_time_ms: now_ms(),
    })
}

/// 导出用的查询参数。
///
/// 全是 `Option`：全空就是全量导出，和桌面端「四个都不填」的含义逐字一致。
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExportParams {
    channel_id: Option<String>,
    tag: Option<String>,
    from_ms: Option<i64>,
    to_ms: Option<i64>,
    /// 前端时区相对 UTC 的偏移（**东为正**）。
    ///
    /// 服务端**不猜**用户在哪：猜错的话文件名里的本地时间就是错的，而那恰恰是
    /// 用户扫文件名时最依赖的一列。不给就按 UTC 算，并在摘要里说明。
    #[serde(default)]
    utc_offset_minutes: Option<i32>,
}

/// 导出整个库（或筛选后的一部分）为一个 zip。
///
/// **走 GET 而不是 POST**：它是幂等的只读动作，浏览器可以直接把它当成下载链接
/// （`a[download]` + `Authorization` 头走不了，所以实际由前端 fetch 下来再存）。
/// 返回的是文件不是 JSON，所以**摘要走响应头**（`X-Export-Messages` 等）——
/// 正好也是桌面端 `ExportSummary` 那几个字段，两端对得上。
///
/// 超大导出回 413 且带一句可操作的建议，见 [`export::MAX_EXPORT_BYTES`]。
async fn export_zip(
    State(state): State<Arc<AppState>>,
    Query(q): Query<ExportParams>,
) -> ServerResult<Response> {
    let query = crate::export::ExportQuery {
        channel_id: q.channel_id,
        tag: q.tag,
        from_ms: q.from_ms,
        to_ms: q.to_ms,
        utc_offset_minutes: q.utc_offset_minutes.unwrap_or(0),
    };
    // 走 `gated_` 而不是裸 `build_zip`：并发闸门在这里，
    // 免得"忘了限流"变成一次无声的回归。
    let out = crate::export::gated_build_zip(&state.store, &query).await?;

    let file_name = crate::export::zip_file_name(&query);
    Ok((
        [
            (
                axum::http::header::CONTENT_TYPE,
                "application/zip".to_string(),
            ),
            (
                axum::http::header::CONTENT_DISPOSITION,
                // 文件名是 ASCII（`messagenote-2026-10-07-0912.zip`），所以
                // 不需要 RFC 5987 那套 `filename*` 转义。
                format!("attachment; filename=\"{file_name}\""),
            ),
            (
                axum::http::header::HeaderName::from_static("x-export-messages"),
                out.messages.to_string(),
            ),
            (
                axum::http::header::HeaderName::from_static("x-export-attachments"),
                out.attachments.to_string(),
            ),
            (
                axum::http::header::HeaderName::from_static("x-export-missing-attachments"),
                out.missing_attachments.to_string(),
            ),
        ],
        out.bytes,
    )
        .into_response())
}

/// 鉴权过的握手：确认地址、令牌、协议版本三样都对，且不落任何数据。
///
/// 客户端的"测试连接"必须打这个端点而不是 `/api/health`。
/// 否则会出现"测试连接成功、一同步就 401"这种让人完全摸不着头脑的现象 ——
/// 用户的令牌明明是错的，界面却告诉他连上了。
async fn handshake() -> Json<HealthResponse> {
    health().await
}

async fn pull(
    State(state): State<Arc<AppState>>,
    Query(q): Query<PullQuery>,
) -> ServerResult<Json<PullResponse>> {
    Ok(Json(state.store.pull(q.since, q.limit.clamp(1, 1000))?))
}

async fn push(
    State(state): State<Arc<AppState>>,
    Json(req): Json<PushRequest>,
) -> ServerResult<Json<PushResponse>> {
    if req.changes.len() > MAX_BATCH {
        return Err(ServerError::BadRequest(format!(
            "单批变更数 {} 超过上限 {MAX_BATCH}",
            req.changes.len()
        )));
    }
    Ok(Json(state.store.push(&req.changes)?))
}

// ---------------------------------------------------------------- 读取

/// 解析筛选范围。规则来自共享存储层，桌面端命令层用的是同一份。
fn scope_of(q: &TimelineQuery) -> ServerResult<Scope<'_>> {
    Scope::parse(&q.scope, q.channel_id.as_deref(), q.tag.as_deref())
        .map_err(|e| ServerError::BadRequest(e.to_string()))
}

/// `beforeCreatedAt` 和 `beforeId` **必须一起给**，只给一个要回 400。
///
/// 只给时间戳等于退回单键游标：同一毫秒内写入的多条会被整批跳过，
/// 往前翻时凭空少掉一段，而且**不报错** —— 用户只会觉得"有些记录不见了"，
/// 而这个项目最不能接受的就是笔记无声无息地消失。
///
/// 原来这里静默当"没有游标"（回到第一页）：更隐蔽，因为用户看到的是
/// 「点加载更多，结果又回到了开头」，而没有任何一处提示参数写错了。
fn cursor_of(q: &TimelineQuery) -> ServerResult<Option<Cursor>> {
    match (&q.before_created_at, &q.before_id) {
        (Some(created_at), Some(id)) => Ok(Some(Cursor {
            created_at: *created_at,
            id: id.clone(),
        })),
        (None, None) => Ok(None),
        _ => Err(ServerError::BadRequest(
            "beforeCreatedAt 和 beforeId 必须成对给出：只给一个会让往前翻漏掉记录"
                .into(),
        )),
    }
}

async fn timeline(
    State(state): State<Arc<AppState>>,
    Query(q): Query<TimelineQuery>,
) -> ServerResult<Json<MessagePage>> {
    let scope = scope_of(&q)?;
    let cursor = cursor_of(&q)?;
    Ok(Json(state.store.list_messages(
        scope,
        q.limit.unwrap_or(DEFAULT_TIMELINE_LIMIT),
        cursor.as_ref(),
        q.since,
    )?))
}

async fn timeline_stats(State(state): State<Arc<AppState>>) -> ServerResult<Json<TimelineStats>> {
    Ok(Json(state.store.timeline_stats()?))
}

async fn channels(State(state): State<Arc<AppState>>) -> ServerResult<Json<Vec<Channel>>> {
    Ok(Json(state.store.list_channels()?))
}

async fn tags(State(state): State<Arc<AppState>>) -> ServerResult<Json<Vec<TagCount>>> {
    Ok(Json(state.store.list_tags()?))
}

async fn search(
    State(state): State<Arc<AppState>>,
    Query(q): Query<SearchQuery>,
) -> ServerResult<Json<SearchPage>> {
    Ok(Json(state.store.search(
        &q.q,
        q.limit.unwrap_or(DEFAULT_SEARCH_LIMIT),
        q.offset.unwrap_or(0),
    )?))
}

// ---------------------------------------------------------------- 实时推送
//
// 客户端原先靠 45 秒轮询（桌面端）或用户手动刷新（网页端）。多设备同时开着
// 的时候这个延迟是能感觉到的：手机上记一笔，电脑上要等半分钟才出现。

/// 有变更就往下推一个信号。
///
/// **只推"有东西变了"，不推内容。** 客户端收到之后走既有的拉取路径。
/// 推内容就得在这里重新实现一遍"哪些变更该发给谁"，而拉取那套游标逻辑
/// 已经有测试守着了 —— 两份实现对同一件事给出不同答案，
/// 是同步类 bug 最经典的来源。
async fn events(
    State(state): State<Arc<AppState>>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = state.store.subscribe();

    let stream = stream::unfold(rx, |mut rx| async move {
        match rx.recv().await {
            // 订阅者落后太多时信号会被丢，但丢的是**次数**、不是**变化本身** ——
            // 它照样得去拉一次，所以和正常情况发一样的东西。
            Ok(()) | Err(RecvError::Lagged(_)) => {
                Some((Ok(Event::default().event("changed").data("1")), rx))
            }
            Err(RecvError::Closed) => None,
        }
    });

    // 心跳。没有它的话，中间任何一层（Caddy、反代、NAT）都会在空闲时悄悄
    // 掐掉连接，而客户端看起来只是"再也不更新了" —— 一个不报错的失败。
    Sse::new(stream).keep_alive(KeepAlive::new().interval(KEEPALIVE_INTERVAL))
}

// ---------------------------------------------------------------- 附件
//
// 附件**不走变更日志**：内容寻址意味着它不可变（换图必然换 sha），
// 没有"两边都改了谁赢"这件事要解决。对端是从正文里的 `attachment:<sha>`
// 发现它的，然后来这里取字节。详见 messagenote_core::attachment。

/// 上传附件字节。
///
/// **客户端不报 sha，服务端对收到的字节现算。** 上传方因此不必先本地哈希 ——
/// 浏览器里 `crypto.subtle` 在非安全上下文下根本不存在，而自建服务端常常
/// 就是明文 HTTP 的内网地址。已经在本地算过哈希的客户端（桌面端）可以拿
/// 返回值核对，那正好能抓出自己的哈希 bug。
async fn upload_blob(
    State(state): State<Arc<AppState>>,
    body: Bytes,
) -> ServerResult<Json<BlobResponse>> {
    if body.is_empty() {
        return Err(ServerError::bad_request("附件内容是空的"));
    }

    // **类型只由字节决定**，绝不采信调用方声明的 Content-Type。
    // 同源部署下，一个声明成 text/html 的上传文件就是存储型 XSS ——
    // 网页端和服务端是同一个 origin，脚本能直接读走会话。
    let mime = messagenote_core::attachment::resolve_mime(&body);
    let sha256 = messagenote_core::attachment::sha256_hex(&body);

    let fresh = state.store.put_blob(&sha256, &body).await?;
    if !fresh {
        tracing::debug!(sha256, "这份附件服务端已经有了，跳过写入");
    }

    Ok(Json(BlobResponse {
        sha256,
        size: body.len() as i64,
        mime: mime.to_string(),
    }))
}

/// 一次问清楚哪些附件服务端没有。没有它，客户端每轮同步只能盲传。
async fn missing_blobs(
    State(state): State<Arc<AppState>>,
    Json(req): Json<BlobMissingRequest>,
) -> ServerResult<Json<BlobMissingResponse>> {
    if req.shas.len() > MAX_BLOB_QUERY {
        return Err(ServerError::bad_request(format!(
            "一次最多问 {MAX_BLOB_QUERY} 个 sha"
        )));
    }
    Ok(Json(BlobMissingResponse {
        missing: state.store.missing_blobs(&req.shas).await?,
    }))
}

/// 下载附件字节。
async fn download_blob(
    State(state): State<Arc<AppState>>,
    Path(sha256): Path<String>,
) -> ServerResult<Response> {
    if !messagenote_core::attachment::is_sha256(&sha256) {
        return Err(ServerError::bad_request("sha256 格式不对"));
    }

    let Some((mime, bytes)) = state.store.get_blob(&sha256).await? else {
        // 404 而不是 500：客户端要靠它区分"服务端也没有，别再重试"
        // 和"服务端出错了，等会儿再试"。回 500 会让下载队列永远卡在同一条。
        return Err(ServerError::not_found("服务端没有这份附件"));
    };

    let displayable = messagenote_core::attachment::is_displayable_image(&mime);
    let mut resp = Response::new(Body::from(bytes));

    let headers = resp.headers_mut();
    // 类型来自**存进去时对字节的嗅探**，不是任何请求方说了算的东西
    if let Ok(value) = HeaderValue::from_str(&mime) {
        headers.insert(header::CONTENT_TYPE, value);
    }
    // 内容寻址 = 这个名字下的字节永远不会变，浏览器可以永久缓存
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=31536000, immutable"),
    );
    // 即使 Content-Type 被中间人改掉，也禁止浏览器去猜成可执行/可渲染的类型
    headers.insert(
        HeaderName::from_static("x-content-type-options"),
        HeaderValue::from_static("nosniff"),
    );
    if !displayable {
        // 认不出来的类型只当下载，绝不当成可渲染内容
        headers.insert(
            header::CONTENT_DISPOSITION,
            HeaderValue::from_static("attachment"),
        );
    }
    Ok(resp)
}

// ---------------------------------------------------------------- 写入
//
// 全部由服务端代笔：它把自己当一台设备。下面这些函数只做参数搬运 ——
// 真正的规则（校验、HLC、seq、索引维护、墓碑时间）都在 `Store` 里，
// 和 `push` 共用同一条路径。

async fn create_message(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateMessageRequest>,
) -> ServerResult<Json<Message>> {
    Ok(Json(state.store.create_message(
        &req.body,
        req.channel_id.as_deref(),
        req.id.as_deref(),
    )?))
}

async fn edit_message(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<EditMessageRequest>,
) -> ServerResult<Json<Message>> {
    Ok(Json(state.store.edit_message(&id, &req.body)?))
}

/// 删除回 204：没什么可回的，而且客户端本来就要重新拉一次时间线。
async fn remove_message(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ServerResult<StatusCode> {
    state.store.remove_message(&id)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn move_message(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<MoveMessageRequest>,
) -> ServerResult<StatusCode> {
    state.store.move_message(&id, &req.channel_id)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn set_message_tags(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<SetTagsRequest>,
) -> ServerResult<StatusCode> {
    state.store.set_message_tags(&id, &req.tags)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn create_channel(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateChannelRequest>,
) -> ServerResult<Json<Channel>> {
    Ok(Json(state.store.create_channel(&req.name)?))
}

async fn rename_channel(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<RenameChannelRequest>,
) -> ServerResult<StatusCode> {
    state.store.rename_channel(&id, &req.name)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn remove_channel(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ServerResult<StatusCode> {
    state.store.remove_channel(&id)?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_time_eq_matches_normal_equality() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(!constant_time_eq(b"", b"a"));
        assert!(constant_time_eq(b"", b""));
    }
}
