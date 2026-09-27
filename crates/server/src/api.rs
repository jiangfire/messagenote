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
//! 刻意用朴素的 REST 而不是 WebSocket：同步是"客户端主动推拉"的模型，
//! REST 更好调试（curl 就能复现问题），而实时推送（SSE）等 S4 再说。

use std::sync::Arc;

use axum::extract::{Query, Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};

use messagenote_core::hlc::now_ms;
use messagenote_core::models::{Channel, MessagePage, SearchHit, TagCount, TimelineStats};
use messagenote_core::wire::{
    HealthResponse, PullQuery, PullResponse, PushRequest, PushResponse, SearchQuery, TimelineQuery,
    PROTOCOL_VERSION,
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
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_token,
        ));

    Router::new()
        // 存活探针刻意**不鉴权**：给 Caddy、监控、uptime-kuma 用，
        // 泄露的信息只有"这里有个 MessageNote 服务端"。
        // 代价是它回答不了"我的令牌对不对" —— 所以另有 handshake。
        .route("/api/health", get(health))
        .merge(protected)
        .with_state(state)
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

async fn require_token(
    State(state): State<Arc<AppState>>,
    req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let presented = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");

    if constant_time_eq(presented.as_bytes(), state.token.as_bytes()) {
        Ok(next.run(req).await)
    } else {
        // 记日志但**不回显任何细节**：不告诉对方 token 是对是错、格式对不对。
        tracing::warn!("鉴权失败，已拒绝请求");
        Err(StatusCode::UNAUTHORIZED)
    }
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
