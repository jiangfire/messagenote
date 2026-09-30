//! MessageNote 同步服务端入口。

use std::path::Path;
use std::sync::Arc;

use messagenote_server::{api, AppState, Store};
use tracing_subscriber::EnvFilter;

/// token 的最小长度。这是保护"你全部笔记"的唯一凭据，
/// 允许一个 `123456` 等于把库公开在公网上。
const MIN_TOKEN_LEN: usize = 32;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let db_path =
        std::env::var("MESSAGENOTE_DB").unwrap_or_else(|_| "messagenote-server.sqlite".into());

    let token = std::env::var("MESSAGENOTE_TOKEN")
        .map_err(|_| "必须设置环境变量 MESSAGENOTE_TOKEN（至少 32 个字符）")?;
    if token.len() < MIN_TOKEN_LEN {
        return Err(format!(
            "MESSAGENOTE_TOKEN 太短（当前 {} 字符，至少需要 {MIN_TOKEN_LEN}）",
            token.len()
        )
        .into());
    }

    // 默认只绑环回地址。这个进程说的是 **HTTP 明文**，必须由反向代理
    // （Caddy）终结 TLS 再转发过来。默认绑 0.0.0.0 会让人一不留神就把
    // 装着全部笔记的明文端口暴露到公网。
    let bind = std::env::var("MESSAGENOTE_BIND").unwrap_or_else(|_| "127.0.0.1:8787".into());

    // 附件字节放哪儿：默认跟着 SQLite 走，设了 MESSAGENOTE_S3_BUCKET 就是对象存储。
    // 见 `messagenote_server::Blobs`（那里列了全部变量）。
    let blobs = messagenote_server::Blobs::from_env()?;
    let blobs_where = if blobs.is_s3() {
        "S3（对象存储）"
    } else {
        "SQLite"
    };

    let store = Store::open_with_blobs(Path::new(&db_path), blobs)?;
    let state = Arc::new(AppState { store, token });

    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!(
        db = %db_path,
        addr = %listener.local_addr()?,
        blobs = blobs_where,
        "MessageNote 同步服务端已启动（请确保前面有 TLS 反向代理）"
    );

    axum::serve(listener, api::router(state))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
            tracing::info!("收到中断信号，正在退出");
        })
        .await?;

    Ok(())
}
