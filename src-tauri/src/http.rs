//! 基于 ureq 的同步传输层。
//!
//! 选 ureq 而不是 reqwest 有两个理由：
//! 1. 同步是**阻塞式**的一次推拉，不需要异步运行时，ureq 体积小、依赖少。
//! 2. ureq 默认走 rustls + webpki-roots，**完全不碰 Windows 的 schannel**。
//!    这不只是洁癖：本机的开发沙箱就禁用了 schannel，用系统 TLS 的实现
//!    在这里连不上任何东西，而 rustls 可以。

use std::time::Duration;

use messagenote_core::wire::{
    Change, HealthResponse, PullResponse, PushRequest, PushResponse, PROTOCOL_VERSION,
};

use crate::error::{AppError, AppResult};
use crate::sync::ServerApi;

/// 连接超时。服务端不可达时，后台同步线程会卡这么久才报错。
/// 但它**卡不住界面** —— 网络往返期间不持有数据库锁（见 `sync::LocalStore`），
/// 所以这个值只影响"多久看到失败提示"，不再决定用户打字会不会顿。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(4);

/// 整体超时兜底：防止对端"连上了但不说话"把同步线程永久挂住。
/// 同理，它只占用后台线程，不占用数据库锁。
const OVERALL_TIMEOUT: Duration = Duration::from_secs(30);

pub struct HttpServerApi {
    base: String,
    token: String,
    agent: ureq::Agent,
}

impl HttpServerApi {
    pub fn new(url: &str, token: &str) -> AppResult<Self> {
        let base = url.trim().trim_end_matches('/').to_string();
        if base.is_empty() {
            return Err(AppError::Msg("尚未填写同步服务端地址".into()));
        }
        if !(base.starts_with("http://") || base.starts_with("https://")) {
            return Err(AppError::Msg(
                "同步地址必须以 http:// 或 https:// 开头".into(),
            ));
        }
        if token.trim().is_empty() {
            return Err(AppError::Msg("尚未填写同步令牌".into()));
        }

        let config = ureq::Agent::config_builder()
            .timeout_connect(Some(CONNECT_TIMEOUT))
            .timeout_global(Some(OVERALL_TIMEOUT))
            // 不让 4xx/5xx 直接变成"只有状态码"的错误：服务端在 400/401 里写的
            // 是一句给人看的话（比如"消息 x 指向不存在的频道 y"）。丢掉它之后
            // 用户只能看到 "服务端返回 HTTP 400" —— 对自建服务端的人毫无帮助。
            .http_status_as_error(false)
            .build();

        Ok(Self {
            base,
            token: token.trim().to_string(),
            agent: ureq::Agent::new_with_config(config),
        })
    }

    fn auth_value(&self) -> String {
        format!("Bearer {}", self.token)
    }

    /// 握手：确认地址、令牌、协议版本都对。不落任何数据。
    ///
    /// 注意打的是 `/api/sync/handshake`（需要鉴权）而不是 `/api/health`。
    /// 后者是不鉴权的存活探针，用它做"测试连接"会给出**假成功** ——
    /// 令牌错了也报连上了，然后一同步就 401。
    pub fn handshake(&self) -> AppResult<HealthResponse> {
        let resp = self
            .agent
            .get(format!("{}/api/sync/handshake", self.base))
            .header("Authorization", self.auth_value())
            .call()
            .map_err(map_err)?;

        let out: HealthResponse = read_json(resp)?;

        // 显式比对协议版本，而不是"尽力而为地继续"。
        // 版本不一致时继续同步，可能把字段按错误的语义写进库 ——
        // 那是一种事后极难排查的静默数据损坏。
        if out.protocol != PROTOCOL_VERSION {
            return Err(AppError::Msg(format!(
                "服务端协议版本 {} 与本机 {} 不一致，请同步升级两端",
                out.protocol, PROTOCOL_VERSION
            )));
        }
        Ok(out)
    }
}

impl ServerApi for HttpServerApi {
    fn pull(&self, since: i64, limit: i64) -> AppResult<PullResponse> {
        let resp = self
            .agent
            .get(format!("{}/api/sync/pull", self.base))
            .header("Authorization", self.auth_value())
            .query("since", since.to_string())
            .query("limit", limit.to_string())
            .call()
            .map_err(map_err)?;

        read_json(resp)
    }

    fn push(&self, changes: &[Change]) -> AppResult<PushResponse> {
        let body = PushRequest {
            changes: changes.to_vec(),
        };
        let resp = self
            .agent
            .post(format!("{}/api/sync/push", self.base))
            .header("Authorization", self.auth_value())
            .send_json(&body)
            .map_err(map_err)?;

        read_json(resp)
    }
}

/// 把响应体读成 JSON；非 2xx 时**把服务端那句话带出来**。
///
/// 服务端对各种拒绝都写了一句给人看的原因（见 `ServerError::BadRequest` /
/// `Unauthorized`），而 `ureq` 默认会把 4xx 变成一个只带状态码的错误、
/// 顺手把响应体丢掉。同步失败时用户看到的就只剩 "HTTP 400"，
/// 而真正的原因（哪一条、为什么）明明就在那句被丢掉的话里。
fn read_json<T: serde::de::DeserializeOwned>(
    resp: ureq::http::Response<ureq::Body>,
) -> AppResult<T> {
    let code = resp.status().as_u16();
    if !(200..300).contains(&code) {
        let detail = resp
            .into_body()
            .read_to_string()
            .unwrap_or_default()
            .trim()
            .to_string();

        return Err(if code == 401 {
            AppError::Msg("同步令牌不对（服务端返回 401）".into())
        } else if detail.is_empty() {
            AppError::Msg(format!("服务端返回 HTTP {code}"))
        } else {
            AppError::Msg(format!("服务端返回 HTTP {code}：{detail}"))
        });
    }
    resp.into_body().read_json().map_err(map_err)
}

/// 把网络层的错误翻译成用户能看懂的一句话。
///
/// 注意 `StatusCode` 这两条分支现在基本走不到了 —— agent 配了
/// `http_status_as_error(false)`，4xx/5xx 会在 [`read_json`] 里处理。
/// 留着是为了万一将来有别的调用路径没走那个函数。
fn map_err(e: ureq::Error) -> AppError {
    match e {
        ureq::Error::StatusCode(401) => AppError::Msg("同步令牌不对（服务端返回 401）".into()),
        ureq::Error::StatusCode(code) => AppError::Msg(format!("服务端返回 HTTP {code}")),
        other => AppError::Msg(format!("网络请求失败：{other}")),
    }
}
