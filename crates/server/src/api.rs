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

use std::sync::Arc;

use axum::extract::{Path, Query, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::{get, patch, post, put};
use axum::{Json, Router};

use messagenote_core::hlc::now_ms;
use messagenote_core::models::{Channel, Message, MessagePage, SearchHit, TagCount, TimelineStats};
use messagenote_core::wire::{
    CreateChannelRequest, CreateMessageRequest, EditMessageRequest, HealthResponse, LoginRequest,
    MoveMessageRequest, PullQuery, PullResponse, PushRequest, PushResponse, RenameChannelRequest,
    SearchQuery, SessionResponse, SetTagsRequest, TimelineQuery, PROTOCOL_VERSION,
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

pub struct AppState {
    pub store: Store,
    /// 单用户场景下就是一个长期凭据。多用户才需要账号体系。
    pub token: String,
}

pub fn router(state: Arc<AppState>) -> Router {
    let protected = Router::new()
        .route("/api/sync/handshake", get(handshake))
        .route("/api/sync/pull", get(pull))
        .route("/api/sync/push", post(push))
        .route("/api/timeline", get(timeline))
        .route("/api/timeline/stats", get(timeline_stats))
        .route("/api/channels", get(channels))
        .route("/api/tags", get(tags))
        .route("/api/search", get(search))
        // 写入。全部由服务端代笔 —— 见文件头的说明。
        .route("/api/message", post(create_message))
        .route(
            "/api/message/{id}",
            patch(edit_message).delete(remove_message),
        )
        .route("/api/message/{id}/move", post(move_message))
        .route("/api/message/{id}/tags", put(set_message_tags))
        .route("/api/channel", post(create_channel))
        .route(
            "/api/channel/{id}",
            patch(rename_channel).delete(remove_channel),
        )
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_token,
        ));

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

    // `||` 短路：长期令牌命中时不会白查一次数据库
    let ok = constant_time_eq(presented.as_bytes(), state.token.as_bytes())
        || state.store.session_is_valid(presented).unwrap_or(false);

    if ok {
        Ok(next.run(req).await)
    } else {
        // 记日志但**不回显任何细节**：不告诉对方凭据是对是错、格式对不对。
        tracing::warn!("鉴权失败，已拒绝请求");
        Err(StatusCode::UNAUTHORIZED)
    }
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

/// `beforeCreatedAt` 和 `beforeId` **必须一起给**。
///
/// 只给时间戳等于退回单键游标：同一毫秒内写入的多条会被整批跳过，
/// 往前翻时凭空少掉一段，而且不报错。
fn cursor_of(q: &TimelineQuery) -> Option<Cursor> {
    match (&q.before_created_at, &q.before_id) {
        (Some(created_at), Some(id)) => Some(Cursor {
            created_at: *created_at,
            id: id.clone(),
        }),
        _ => None,
    }
}

async fn timeline(
    State(state): State<Arc<AppState>>,
    Query(q): Query<TimelineQuery>,
) -> ServerResult<Json<MessagePage>> {
    let scope = scope_of(&q)?;
    let cursor = cursor_of(&q);
    Ok(Json(state.store.list_messages(
        scope,
        q.limit.unwrap_or(DEFAULT_TIMELINE_LIMIT),
        cursor.as_ref(),
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
) -> ServerResult<Json<Vec<SearchHit>>> {
    Ok(Json(
        state
            .store
            .search(&q.q, q.limit.unwrap_or(DEFAULT_SEARCH_LIMIT))?,
    ))
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
    Ok(Json(
        state
            .store
            .create_message(&req.body, req.channel_id.as_deref())?,
    ))
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
