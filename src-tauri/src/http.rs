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

        let out: HealthResponse = resp.into_body().read_json().map_err(map_err)?;

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

        resp.into_body().read_json().map_err(map_err)
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

        resp.into_body().read_json().map_err(map_err)
    }
}

/// 把网络层的错误翻译成用户能看懂的一句话。
///
/// 尤其是 401：它只有一种含义（令牌不对），但裸的错误码对用户毫无帮助。
fn map_err(e: ureq::Error) -> AppError {
    match e {
        ureq::Error::StatusCode(401) => {
            AppError::Msg("同步令牌不对（服务端返回 401）".into())
        }
        ureq::Error::StatusCode(code) => AppError::Msg(format!("服务端返回 HTTP {code}")),
        other => AppError::Msg(format!("网络请求失败：{other}")),
    }
}
