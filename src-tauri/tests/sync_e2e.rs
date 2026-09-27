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
///
/// 注意这里**不持有 `db.conn()`** —— `sync_once` 自己按需加锁，
/// 握着 guard 调它会死锁。同样地，真实 HTTP 往返全程也不持锁。
fn sync_until_quiet(db: &Db, api: &HttpServerApi) {
    for _ in 0..40 {
        let r = sync_once(db, api).expect("同步失败");
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

// ---------------------------------------------------------------- 读 API 一致性

/// 只处理 UTF-8 字节的百分号编码。测试里够用了 ——
/// 不是为了做一个 URL 库，是为了让中文标签能塞进查询串。
fn pct(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// 直接打一个 GET 端点，返回原始 JSON。
///
/// 刻意不经过 `HttpServerApi`：它只有同步端点，而这里要的正是
/// "线缆上真实传了什么字节"。
fn get_json(addr: SocketAddr, path: &str) -> serde_json::Value {
    let resp = ureq::get(&format!("http://{addr}{path}"))
        .header("Authorization", &format!("Bearer {TOKEN}"))
        .call()
        .unwrap_or_else(|e| panic!("GET {path} 失败：{e}"));
    resp.into_body().read_json().expect("响应不是合法 JSON")
}

/// **桌面端和服务端对同一批数据必须给出完全一样的浏览与检索结果。**
///
/// 这是"浏览语义下沉到 `messagenote-store`"的全部意义所在。两端跑的是同一份
/// SQL（服务端的表是客户端表的超集），但"共用一份实现"是否真的成立，
/// 只有把同一批数据喂给两个 schema、再逐字段比对才能证明。
///
/// 比对的是 **JSON 本身**（`serde_json::Value`），不是"ids 大致相等"：
/// 字段名、大小写、排序、数值类型任何一处不一致都会让它红。
/// 网页端拿到的就是这个 JSON，它必须和桌面端渲染的是同一份东西。
#[test]
fn desktop_and_server_agree_on_browse_and_search() {
    let addr = start_server();
    let a = db::open_memory("e2e-agree-a").expect("建 A 库");
    let api = api(addr);

    let tag = "水果";
    {
        let conn = a.conn().unwrap();
        let ch = db::create_channel(&conn, "工作").unwrap();
        let m1 = db::append_message(&conn, "苹果和香蕉", None).unwrap();
        db::set_message_tags(&conn, &m1.id, &[tag.into()]).unwrap();
        db::append_message(&conn, "工作里的一条记录", Some(&ch.id)).unwrap();
        db::append_message(&conn, "收件箱里的第二条", None).unwrap();
        // 再来两条会命中同一个词的，用来压检索的排序：
        // bm25 分数相同时 FTS5 按 rowid 返回，而两端的插入顺序必然不同。
        db::append_message(&conn, "苹果派的做法", None).unwrap();
        db::append_message(&conn, "第三个苹果", None).unwrap();
    }

    sync_until_quiet(&a, &api);
    let conn = a.conn().unwrap();

    // ---- 频道 ----
    assert_eq!(
        serde_json::to_value(db::list_channels(&conn).unwrap()).unwrap(),
        get_json(addr, "/api/channels"),
        "频道列表两端必须逐字段一致（含 messageCount 与排序）"
    );

    // ---- 统计 ----
    assert_eq!(
        serde_json::to_value(db::timeline_stats(&conn).unwrap()).unwrap(),
        get_json(addr, "/api/timeline/stats")
    );

    // ---- 标签 ----
    assert_eq!(
        serde_json::to_value(db::list_tags(&conn).unwrap()).unwrap(),
        get_json(addr, "/api/tags")
    );

    // ---- 时间线的四种范围 ----
    let work_id = db::list_channels(&conn)
        .unwrap()
        .into_iter()
        .find(|c| c.name == "工作")
        .expect("应当有工作频道")
        .id;

    assert_eq!(
        serde_json::to_value(db::list_messages(&conn, db::Scope::All, 50, None).unwrap()).unwrap(),
        get_json(addr, "/api/timeline?scope=all&limit=50"),
        "scope=all"
    );
    assert_eq!(
        serde_json::to_value(db::list_messages(&conn, db::Scope::Unfiled, 50, None).unwrap())
            .unwrap(),
        get_json(addr, "/api/timeline?scope=unfiled&limit=50"),
        "scope=unfiled —— 未归档的判定必须和桌面端一致，否则网页端点进去条数不对"
    );
    assert_eq!(
        serde_json::to_value(
            db::list_messages(&conn, db::Scope::Channel(&work_id), 50, None).unwrap()
        )
        .unwrap(),
        get_json(
            addr,
            &format!("/api/timeline?scope=channel&channelId={work_id}&limit=50")
        ),
        "scope=channel"
    );
    assert_eq!(
        serde_json::to_value(db::list_messages(&conn, db::Scope::Tag(tag), 50, None).unwrap())
            .unwrap(),
        get_json(
            addr,
            &format!("/api/timeline?scope=tag&tag={}&limit=50", pct(tag))
        ),
        "scope=tag —— 中文标签要能正确地过查询串"
    );

    // ---- 键集分页 ----
    let first = db::list_messages(&conn, db::Scope::All, 2, None).unwrap();
    let cursor = db::Cursor::before(&first.items[1]);
    assert_eq!(
        serde_json::to_value(db::list_messages(&conn, db::Scope::All, 2, Some(&cursor)).unwrap())
            .unwrap(),
        get_json(
            addr,
            &format!(
                "/api/timeline?scope=all&limit=2&beforeCreatedAt={}&beforeId={}",
                cursor.created_at, cursor.id
            )
        ),
        "往前翻一页的结果两端必须一致"
    );

    // ---- 检索：双字词走 FTS，单字走 LIKE 回退，两条路径都要一致 ----
    for q in ["苹果", "苹", "记录"] {
        assert_eq!(
            serde_json::to_value(db::search(&conn, q, 20).unwrap()).unwrap(),
            get_json(addr, &format!("/api/search?q={}&limit=20", pct(q))),
            "检索「{q}」两端结果必须一致（含命中顺序）"
        );
    }

    // 得确认上面那个排序断言不是空跑：至少有一条查询要命中多个结果
    assert!(
        db::search(&conn, "苹果", 20).unwrap().len() > 1,
        "测试不能空跑：检索的排序只有多命中时才有意义"
    );
}

/// 读端点必须和同步端点一样要求鉴权。
///
/// 这几个端点是**新加的**，而"新加的端点忘了挂 middleware"是最常见的一类
/// 事故 —— 它不会让任何测试变红，只是把用户全部笔记挂在公网上。
#[test]
fn read_endpoints_require_a_token() {
    let addr = start_server();

    for path in [
        "/api/timeline?scope=all",
        "/api/timeline/stats",
        "/api/channels",
        "/api/tags",
        "/api/search?q=x",
    ] {
        match ureq::get(&format!("http://{addr}{path}")).call() {
            Err(ureq::Error::StatusCode(401)) => {}
            Err(e) => panic!("{path} 应当返回 401，实际：{e}"),
            Ok(_) => panic!("{path} 不带令牌竟然成功了"),
        }
    }
}

/// 参数写错要回 400，而且要说清楚错在哪。
///
/// 一律回 500 会让人以为服务端炸了，去查错地方 —— 而这几个端点是给网页端
/// 调用的，参数写错是很正常的开发期现象。
#[test]
fn a_bad_scope_is_a_400_with_a_readable_message() {
    let addr = start_server();

    let auth = format!("Bearer {TOKEN}");

    // 未知 scope
    match ureq::get(&format!("http://{addr}/api/timeline?scope=bogus"))
        .header("Authorization", &auth)
        .call()
    {
        Err(ureq::Error::StatusCode(400)) => {}
        Err(e) => panic!("未知 scope 应当返回 400 而不是 500，实际：{e}"),
        Ok(_) => panic!("未知 scope 竟然被接受了"),
    }

    // scope=channel 缺 channelId
    match ureq::get(&format!("http://{addr}/api/timeline?scope=channel"))
        .header("Authorization", &auth)
        .call()
    {
        Err(ureq::Error::StatusCode(400)) => {}
        Err(e) => panic!("缺 channelId 应当返回 400，实际：{e}"),
        Ok(_) => panic!("缺 channelId 竟然被接受了"),
    }

    // scope=tag 缺 tag
    match ureq::get(&format!("http://{addr}/api/timeline?scope=tag"))
        .header("Authorization", &auth)
        .call()
    {
        Err(ureq::Error::StatusCode(400)) => {}
        Err(e) => panic!("缺 tag 应当返回 400，实际：{e}"),
        Ok(_) => panic!("缺 tag 竟然被接受了"),
    }
}

// ---------------------------------------------------------------- 服务端代笔

/// 发一个写请求（用长期令牌），返回响应 JSON。
fn write_req(
    addr: SocketAddr,
    method: &str,
    path: &str,
    body: Option<&serde_json::Value>,
) -> serde_json::Value {
    write_req_with(addr, method, path, body, TOKEN)
}

/// 同上，但可以指定用哪个凭据 —— 会话那条路径也要能写。
///
/// 刻意按方法分派而不是做成一个万能构造器：ureq 里 `get`/`delete` 和
/// `post`/`put`/`patch` 的 builder 类型不同，只有后者能带 body。
fn write_req_with(
    addr: SocketAddr,
    method: &str,
    path: &str,
    body: Option<&serde_json::Value>,
    bearer_val: &str,
) -> serde_json::Value {
    let url = format!("http://{addr}{path}");
    let auth = format!("Bearer {bearer_val}");

    let resp = match (method, body) {
        ("POST", Some(b)) => ureq::post(&url).header("Authorization", &auth).send_json(b),
        ("PUT", Some(b)) => ureq::put(&url).header("Authorization", &auth).send_json(b),
        ("PATCH", Some(b)) => ureq::patch(&url).header("Authorization", &auth).send_json(b),
        ("DELETE", _) => ureq::delete(&url).header("Authorization", &auth).call(),
        _ => panic!("这里不支持 {method}（或者忘了给 body）"),
    }
    .unwrap_or_else(|e| panic!("{method} {path} 失败：{e}"));

    // 删除类端点回 204，没有 body
    if resp.status().as_u16() == 204 {
        return serde_json::Value::Null;
    }
    resp.into_body().read_json().expect("响应不是合法 JSON")
}

/// **网页端写的东西，桌面端必须能通过同步正常拿到。**
///
/// 这就是"服务端代笔"的全部意义：服务端把自己当一台设备，生成 HLC、写进
/// 变更日志、分配 seq。桌面端不需要知道这条笔记是网页端写的还是另一台桌面端
/// 写的 —— 走的完全是同一条路。也正因如此，裁定权仍然只有 `core::merge`
/// 那一份，浏览器里没有第二套合并规则。
#[test]
fn a_web_write_reaches_the_desktop_through_sync() {
    let addr = start_server();
    let a = db::open_memory("e2e-web-a").expect("建 A 库");
    let b = db::open_memory("e2e-web-b").expect("建 B 库");
    let api = api(addr);

    // ---- 网页端：建频道、记一条 ----
    let ch = write_req(
        addr,
        "POST",
        "/api/channel",
        Some(&serde_json::json!({ "name": "网页建的" })),
    );
    let ch_id = ch["id"].as_str().expect("频道要有 id").to_string();

    let m = write_req(
        addr,
        "POST",
        "/api/message",
        Some(&serde_json::json!({ "body": "网页端记的一条", "channelId": ch_id })),
    );
    let m_id = m["id"].as_str().expect("消息要有 id").to_string();
    assert_eq!(m["body"], "网页端记的一条");

    // ---- 两台桌面端都同步下来 ----
    sync_until_quiet(&a, &api);
    sync_until_quiet(&b, &api);

    for (name, d) in [("A", &a), ("B", &b)] {
        let conn = d.conn().unwrap();
        let bodies: Vec<String> = conn
            .prepare("SELECT body FROM message WHERE deleted_at IS NULL")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert!(
            bodies.iter().any(|x| x == "网页端记的一条"),
            "{name} 端没拿到网页端写的笔记：{bodies:?}"
        );
        assert!(
            db::list_channels(&conn)
                .unwrap()
                .iter()
                .any(|c| c.name == "网页建的"),
            "{name} 端没拿到网页端建的频道"
        );
        assert_eq!(
            db::search(&conn, "网页端", 10).unwrap().len(),
            1,
            "{name} 端的本地检索索引也要跟着建起来"
        );
    }

    assert_eq!(
        snapshot(&a.conn().unwrap()),
        snapshot(&b.conn().unwrap()),
        "代笔写入之后两端仍须收敛"
    );

    // ---- 网页端改正文，桌面端跟着更新 ----
    write_req(
        addr,
        "PATCH",
        &format!("/api/message/{m_id}"),
        Some(&serde_json::json!({ "body": "网页端改过了" })),
    );
    sync_until_quiet(&a, &api);
    {
        let conn = a.conn().unwrap();
        let body: String = conn
            .query_row("SELECT body FROM message WHERE id = ?1", [&m_id], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(body, "网页端改过了");
        assert_eq!(
            db::search(&conn, "改过", 10).unwrap().len(),
            1,
            "改了正文之后，本端索引也要跟着重建"
        );
    }

    // ---- 网页端打标签，桌面端跟着变 ----
    write_req(
        addr,
        "PUT",
        &format!("/api/message/{m_id}/tags"),
        Some(&serde_json::json!({ "tags": ["网页标签"] })),
    );
    sync_until_quiet(&a, &api);
    {
        let conn = a.conn().unwrap();
        let hits = db::search(&conn, "改过", 10).unwrap();
        assert_eq!(
            hits[0].message.tags,
            vec!["网页标签".to_string()],
            "网页端打的标签要出现在桌面端"
        );
    }

    // ---- 网页端删除，桌面端也删掉 ----
    write_req(addr, "DELETE", &format!("/api/message/{m_id}"), None);
    sync_until_quiet(&a, &api);
    sync_until_quiet(&b, &api);
    {
        let conn = a.conn().unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM message WHERE deleted_at IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 0, "网页端删掉的笔记，桌面端也该消失");
    }
    assert_eq!(
        snapshot(&a.conn().unwrap()),
        snapshot(&b.conn().unwrap()),
        "一整轮网页端操作之后两端仍须收敛"
    );
}

// ---------------------------------------------------------------- 会话鉴权

/// 用给定的 Bearer 值发一个 GET，返回 (HTTP 状态码, JSON)。
fn try_get(addr: SocketAddr, path: &str, bearer_val: Option<&str>) -> (u16, serde_json::Value) {
    let mut req = ureq::get(&format!("http://{addr}{path}"));
    if let Some(b) = bearer_val {
        req = req.header("Authorization", &format!("Bearer {b}"));
    }
    match req.call() {
        Ok(r) => {
            let code = r.status().as_u16();
            let v = r.into_body().read_json().unwrap_or(serde_json::Value::Null);
            (code, v)
        }
        Err(ureq::Error::StatusCode(c)) => (c, serde_json::Value::Null),
        Err(e) => panic!("GET {path} 出错：{e}"),
    }
}

/// 用长期令牌登录，返回 (HTTP 状态码, JSON)。
fn login(addr: SocketAddr, token: &str) -> (u16, serde_json::Value) {
    match ureq::post(&format!("http://{addr}/api/session"))
        .send_json(serde_json::json!({ "token": token }))
    {
        Ok(r) => {
            let code = r.status().as_u16();
            let v = r.into_body().read_json().unwrap_or(serde_json::Value::Null);
            (code, v)
        }
        Err(ureq::Error::StatusCode(c)) => (c, serde_json::Value::Null),
        Err(e) => panic!("登录请求出错：{e}"),
    }
}

/// 长期令牌换短期会话，换来的会话能用在**读写**两类端点上。
///
/// 为什么需要这一步：长期令牌放进浏览器的 localStorage，等于把整个库的读写
/// 权限交给任何一次 XSS —— 而正文是要渲染用户 Markdown 的。会话会过期、
/// 能吊销；长期令牌则一直待在用户手里，不必落到浏览器里。
#[test]
fn a_session_token_works_for_reads_and_writes() {
    let addr = start_server();

    // ---- 错的令牌换不到会话 ----
    let (code, _) = login(addr, "wrong-token-aaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    assert_eq!(code, 401, "错的长期令牌必须回 401");

    // ---- 对的令牌换得到 ----
    let (code, body) = login(addr, TOKEN);
    assert_eq!(code, 200, "登录应当成功");
    let session = body["session"].as_str().expect("要有 session").to_string();
    let expires_at = body["expiresAt"].as_i64().expect("要有 expiresAt");
    assert!(expires_at > 0, "过期时间必须存在");

    // ---- 会话能读 ----
    let (code, chans) = try_get(addr, "/api/channels", Some(&session));
    assert_eq!(code, 200, "会话应当能读");
    assert!(
        chans.as_array().is_some_and(|a| !a.is_empty()),
        "服务端种下的收件箱应当在里面"
    );

    // ---- 会话也能写 ----
    let created = write_req_with(addr, "POST", "/api/message", Some(&serde_json::json!({ "body": "用会话写的" })), &session);
    assert_eq!(created["body"], "用会话写的");

    // ---- 没带凭据仍然 401 ----
    let (code, _) = try_get(addr, "/api/channels", None);
    assert_eq!(code, 401, "不带凭据必须被拒绝");
}

/// 退出登录必须**立刻**让会话失效。
///
/// 这正是引入会话机制相对"无状态签名令牌"的收益 —— 签名令牌签发之后到期前
/// 撤不掉，"退出登录"会变成一个骗人的按钮。
#[test]
fn logging_out_revokes_the_session_immediately() {
    let addr = start_server();

    let (_, body) = login(addr, TOKEN);
    let session = body["session"].as_str().unwrap().to_string();
    assert_eq!(try_get(addr, "/api/channels", Some(&session)).0, 200);

    // 退出
    let resp = ureq::delete(&format!("http://{addr}/api/session"))
        .header("Authorization", &format!("Bearer {session}"))
        .call()
        .expect("退出登录应当成功");
    assert_eq!(resp.status().as_u16(), 204);

    assert_eq!(
        try_get(addr, "/api/channels", Some(&session)).0,
        401,
        "退出之后这个会话必须立刻失效"
    );

    // 长期令牌不受影响 —— 它是用户的凭据，不是会话
    assert_eq!(
        try_get(addr, "/api/channels", Some(TOKEN)).0,
        200,
        "退出登录不该把桌面端也踢下线"
    );
}

/// 桌面端的同步客户端仍然可以直接用长期令牌。
///
/// 让它改走登录没有收益：它本来就把令牌存在自己机器的数据库里。
/// 这条测试是为了防止"加了会话之后顺手把长期令牌那条路删掉"。
#[test]
fn the_long_lived_token_still_works_directly() {
    let addr = start_server();

    for path in ["/api/channels", "/api/timeline/stats", "/api/tags"] {
        assert_eq!(
            try_get(addr, path, Some(TOKEN)).0,
            200,
            "{path} 应当接受长期令牌"
        );
    }

    // 同步端点也一样
    let api = api(addr);
    assert!(api.handshake().is_ok());
}



