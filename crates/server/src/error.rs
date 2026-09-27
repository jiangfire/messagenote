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
}
