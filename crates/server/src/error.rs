use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;

#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error("数据库错误：{0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("文件系统错误：{0}")]
    Io(#[from] std::io::Error),

    #[error("{0}")]
    Core(#[from] messagenote_core::error::CoreError),

    /// 请求本身有问题（未知的 scope、缺参数、批次过大……）。
    ///
    /// **这一类可以回显细节。** 它描述的是对方自己发来的东西，不涉及服务端
    /// 内部结构 —— 和下面那条"细节只进日志"的规矩针对的不是一回事。
    /// 一律回 500 会让人以为服务端炸了，从而去查错地方。
    #[error("{0}")]
    BadRequest(String),

    /// 凭据不对、会话不存在或已过期。
    ///
    /// 单独一类是为了让它回 **401** 而不是 400 —— 客户端（尤其是网页端）
    /// 要靠状态码区分"我要重新登录"和"我这个请求写错了"。
    #[error("{0}")]
    Unauthorized(String),

    /// **服务端代笔时撞上了更新版本**（比如另一台设备刚改过同一条）。
    ///
    /// 单独一类是为了让它回 **409** 而不是 500 —— 这两种情况在客户端那里的
    /// 处置完全相反：409 是"重新读一次再决定怎么办"，而 500 是"服务端坏了，
    /// 等会儿再试"。回 500 会让用户以为笔记系统出故障了，而实际只需要刷新。
    ///
    /// 和 [`BadRequest`](Self::BadRequest) 分开也是这个理由：请求本身没有错，
    /// 错的是**当前状态**。409 的字面意思就是"状态冲突"。
    #[error("{0}")]
    Conflict(String),

    /// 东西不在这儿。
    ///
    /// 单独一类是为了回 **404**：附件是按 sha 取字节的，客户端要靠这个状态码
    /// 区分"这份附件服务端也没有，我不该再重试"和"服务端出错了，等会儿再试"。
    /// 一律回 500 会让下载队列**永远重试一个不存在的附件**。
    #[error("{0}")]
    NotFound(String),

    #[error("{0}")]
    Msg(String),
}

/// 面向客户端的错误响应。
///
/// **内部错误的细节只进日志，不进响应体。** 这是一个暴露在公网上、装着用户
/// 全部笔记的服务，把 SQL 错误原文回给请求方等于白送一份内部结构情报。
impl IntoResponse for ServerError {
    fn into_response(self) -> Response {
        match self {
            // 客户端自己发错的东西，说清楚比藏起来有用
            ServerError::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg).into_response(),
            ServerError::Unauthorized(msg) => (StatusCode::UNAUTHORIZED, msg).into_response(),
            ServerError::Conflict(msg) => (StatusCode::CONFLICT, msg).into_response(),
            ServerError::NotFound(msg) => (StatusCode::NOT_FOUND, msg).into_response(),
            other => {
                tracing::error!(error = %other, "请求处理失败");
                (StatusCode::INTERNAL_SERVER_ERROR, "服务端内部错误").into_response()
            }
        }
    }
}

impl Serialize for ServerError {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

pub type ServerResult<T> = Result<T, ServerError>;

impl ServerError {
    /// 请求方自己发错了东西。会原样回给客户端（400）。
    pub fn bad_request(text: impl Into<String>) -> Self {
        ServerError::BadRequest(text.into())
    }

    /// 找不到。会回 404。
    pub fn not_found(text: impl Into<String>) -> Self {
        ServerError::NotFound(text.into())
    }

    /// 状态冲突（代笔撞上了更新版本）。会回 409，细节原样带给客户端。
    pub fn conflict(text: impl Into<String>) -> Self {
        ServerError::Conflict(text.into())
    }
}
