//! 端到端同步测试。
//!
//! 验证单元测试证明不了的那些环节：
//! - HTTP 层的序列化/反序列化（camelCase 字段名、`Option` 缺省、墓碑表示法）
//! - Bearer 鉴权真的在拦请求
//! - 真实的 axum 路由与查询参数解析
//! - 两个**独立**的客户端库经过服务端之后逐字节收敛
//!
//! `src/sync.rs` 里的单元测试用的是内存假服务端，它能证明合并语义是对的；
//! 但它证明不了"线缆上跑的东西和内存里那个假货长得一样"。字段名少一个、
//! 大小写差一点，单测全绿而实机全挂 —— 这类问题只能靠这一层抓。

use std::net::SocketAddr;
use std::sync::Arc;

use rusqlite::Connection;

use messagenote_lib::db::{self, Db};
use messagenote_lib::http::HttpServerApi;
use messagenote_lib::sync::sync_once;
use messagenote_server::{AppState, Store};

const TOKEN: &str = "e2e-test-token-0123456789abcdefghijklmnop";

/// 在专用线程 + 专用 tokio runtime 上跑一个真实的 axum 服务端，绑临时端口。
///
/// 必须分开线程：测试主体用的是**阻塞式** ureq，和 tokio worker 挤在
/// 同一条线程上会互相堵死。
fn start_server() -> SocketAddr {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("建 tokio runtime");
        rt.block_on(async move {
            let store = Store::in_memory().expect("建服务端库");
            let state = Arc::new(AppState {
                store,
                token: TOKEN.to_string(),
            });
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("绑定临时端口");
            let addr = listener.local_addr().expect("读监听地址");
            // 先把地址交出去，测试才知道该连哪儿
            tx.send(addr).expect("回传地址");
            messagenote_server::serve(listener, state)
                .await
                .expect("服务端异常退出");
        });
    });
    rx.recv().expect("等待服务端就绪")
}

fn api(addr: SocketAddr) -> HttpServerApi {
    HttpServerApi::new(&format!("http://{addr}"), TOKEN).expect("建 HTTP 客户端")
}

/// 跑到静止：某一轮既没推也没拉。
fn sync_until_quiet(db: &Db, api: &HttpServerApi) {
    let conn = db.conn().expect("取连接");
    for _ in 0..40 {
        let r = sync_once(&conn, api).expect("同步失败");
        if r.pushed == 0 && r.pulled == 0 {
            return;
        }
    }
    panic!("同步没有收敛到静止");
}

/// 规范化快照，用来断言两台设备状态完全一致。
///
/// 刻意包含 `deleted_at` 和 HLC：只比"可见内容"的话，墓碑时间和时钟状态的
/// 发散会被漏掉 —— 而那恰好是最难查的一类同步 bug。
fn snapshot(conn: &Connection) -> Vec<String> {
    let queries = [
        "SELECT id, name, kind, sort_order, created_at, updated_at, deleted_at,
                hlc_wall, hlc_counter, device_id
           FROM channel ORDER BY id",
        "SELECT id, channel_id, body, created_at, updated_at, deleted_at,
                hlc_wall, hlc_counter, device_id
           FROM message ORDER BY id",
        "SELECT name, created_at, updated_at, deleted_at, hlc_wall, hlc_counter, device_id
           FROM tag ORDER BY name",
        "SELECT message_id, tag_name, created_at, updated_at, deleted_at,
                hlc_wall, hlc_counter, device_id
           FROM message_tag ORDER BY message_id, tag_name",
    ];

    let mut out = Vec::new();
    for sql in queries {
        let mut stmt = conn.prepare(sql).expect("准备快照查询");
        let cols = stmt.column_count();
        let rows = stmt
            .query_map([], |r| {
                let mut cells = Vec::with_capacity(cols);
                for i in 0..cols {
                    let v: rusqlite::types::Value = r.get(i)?;
                    cells.push(format!("{v:?}"));
                }
                Ok(cells.join("|"))
            })
            .expect("查询快照");
        for row in rows {
            out.push(row.expect("读快照行"));
        }
    }
    out.sort();
    out
}

#[test]
fn two_clients_converge_through_a_real_http_server() {
    let addr = start_server();
    let a = db::open_memory("e2e-device-a").expect("建 A 库");
    let b = db::open_memory("e2e-device-b").expect("建 B 库");
    let api = api(addr);

    // A 端写入：一条带标签的消息 + 一个频道 + 频道里的一条
    {
        let conn = a.conn().unwrap();
        let m = db::append_message(&conn, "端到端的第一条笔记", None).unwrap();
        db::set_message_tags(&conn, &m.id, &["端到端".into()]).unwrap();
        let ch = db::create_channel(&conn, "测试频道").unwrap();
        db::append_message(&conn, "频道里的记录", Some(&ch.id)).unwrap();
    }

    sync_until_quiet(&a, &api);
    sync_until_quiet(&b, &api);
    sync_until_quiet(&a, &api);

    let snap_a = snapshot(&a.conn().unwrap());
    let snap_b = snapshot(&b.conn().unwrap());
    assert_eq!(
        snap_a, snap_b,
        "两台设备经真实服务端同步后必须逐字节一致"
    );
    assert!(
        snap_a.iter().any(|s| s.contains("端到端的第一条笔记")),
        "测试不能空跑"
    );
    assert!(snap_a.iter().any(|s| s.contains("测试频道")), "频道也要同步过去");

    // B 上的中文检索要能用：本地索引是各设备自己重建的，从不参与同步
    let conn = b.conn().unwrap();
    assert_eq!(db::search(&conn, "端到端", 10).unwrap().len(), 1);
    assert_eq!(db::search(&conn, "频道", 10).unwrap().len(), 1);

    let dirty: i64 = conn
        .query_row("SELECT COUNT(*) FROM message WHERE dirty = 1", [], |r| r.get(0))
        .unwrap();
    assert_eq!(dirty, 0, "同步到静止后不该残留待上传的行");
}

#[test]
fn writes_flow_in_both_directions() {
    let addr = start_server();
    let a = db::open_memory("e2e-bidi-a").expect("建 A 库");
    let b = db::open_memory("e2e-bidi-b").expect("建 B 库");
    let api = api(addr);

    {
        let conn = a.conn().unwrap();
        db::append_message(&conn, "A 写的", None).unwrap();
    }
    {
        let conn = b.conn().unwrap();
        db::append_message(&conn, "B 写的", None).unwrap();
    }

    // 交替同步，制造"两边进度不一致"的真实时序
    sync_until_quiet(&a, &api);
    sync_until_quiet(&b, &api);
    sync_until_quiet(&a, &api);

    assert_eq!(
        snapshot(&a.conn().unwrap()),
        snapshot(&b.conn().unwrap()),
        "双向写入后必须收敛"
    );

    let conn = a.conn().unwrap();
    let bodies: Vec<String> = conn
        .prepare("SELECT body FROM message WHERE deleted_at IS NULL ORDER BY body")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .filter_map(|r| r.ok())
        .collect();
    assert_eq!(bodies, vec!["A 写的".to_string(), "B 写的".to_string()]);
}

#[test]
fn a_deletion_propagates_as_a_tombstone() {
    let addr = start_server();
    let a = db::open_memory("e2e-del-a").expect("建 A 库");
    let b = db::open_memory("e2e-del-b").expect("建 B 库");
    let api = api(addr);

    let id = {
        let conn = a.conn().unwrap();
        db::append_message(&conn, "这条稍后会被删", None).unwrap().id
    };
    sync_until_quiet(&a, &api);
    sync_until_quiet(&b, &api);

    {
        let conn = b.conn().unwrap();
        db::delete_message(&conn, &id).unwrap();
    }
    sync_until_quiet(&b, &api);
    sync_until_quiet(&a, &api);

    assert_eq!(
        snapshot(&a.conn().unwrap()),
        snapshot(&b.conn().unwrap()),
        "删除必须同步过去，且两台设备的墓碑时间也要一致"
    );
    let conn = a.conn().unwrap();
    assert!(
        db::list_messages(&conn, db::Scope::All, 50, None)
            .unwrap()
            .items
            .is_empty(),
        "A 上这条也应该消失"
    );
    assert!(
        db::search(&conn, "稍后", 10).unwrap().is_empty(),
        "检索索引也要跟着摘掉"
    );
}

#[test]
fn a_wrong_token_is_rejected_with_a_clear_message() {
    let addr = start_server();
    let bad = HttpServerApi::new(
        &format!("http://{addr}"),
        "wrong-token-aaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    )
    .expect("建客户端");

    // 打鉴权过的握手端点，而不是不鉴权的 /api/health ——
    // 否则错误令牌也会"测试连接成功"，然后一同步就 401。
    let err = bad.handshake().expect_err("错误令牌必须被拒绝");
    let msg = err.to_string();
    assert!(
        msg.contains("401"),
        "应当明确告诉用户是令牌问题，而不是抛出难以理解的网络错误。实际：{msg}"
    );
}

#[test]
fn a_long_offline_device_catches_up_through_http() {
    let addr = start_server();
    let a = db::open_memory("e2e-off-a").expect("建 A 库");
    let b = db::open_memory("e2e-off-b").expect("建 B 库");
    let api = api(addr);

    // B 离线期间攒内容
    {
        let conn = b.conn().unwrap();
        for i in 0..15 {
            db::append_message(&conn, &format!("离线记录 {i}"), None).unwrap();
        }
    }

    // A 在此期间同步了很多轮
    for i in 0..15 {
        {
            let conn = a.conn().unwrap();
            db::append_message(&conn, &format!("在线记录 {i}"), None).unwrap();
        }
        sync_until_quiet(&a, &api);
    }

    // B 重连
    sync_until_quiet(&b, &api);
    sync_until_quiet(&a, &api);

    assert_eq!(
        snapshot(&a.conn().unwrap()),
        snapshot(&b.conn().unwrap()),
        "长时间离线后重连也必须收敛"
    );

    let conn = a.conn().unwrap();
    let n: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM message WHERE deleted_at IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 30, "离线期间的 15 条和在线期间的 15 条都要在");
}
