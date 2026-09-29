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

use messagenote_core::hlc::Hlc;
use messagenote_core::models::SearchHit;
use messagenote_core::wire::{Change, EntityKind};
use messagenote_lib::db::{self, Db};
use messagenote_lib::http::HttpServerApi;
use messagenote_lib::sync::{sync_once, ServerApi};
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

/// 测试只关心"搜到几条、是哪几条"，不关心分页 —— 统一取第一页。
fn search_hits(conn: &Connection, q: &str, limit: i64) -> rusqlite::Result<Vec<SearchHit>> {
    Ok(db::search_page(conn, q, limit, 0)?.items)
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
    assert_eq!(snap_a, snap_b, "两台设备经真实服务端同步后必须逐字节一致");
    assert!(
        snap_a.iter().any(|s| s.contains("端到端的第一条笔记")),
        "测试不能空跑"
    );
    assert!(
        snap_a.iter().any(|s| s.contains("测试频道")),
        "频道也要同步过去"
    );

    // B 上的中文检索要能用：本地索引是各设备自己重建的，从不参与同步
    let conn = b.conn().unwrap();
    assert_eq!(search_hits(&conn, "端到端", 10).unwrap().len(), 1);
    assert_eq!(search_hits(&conn, "频道", 10).unwrap().len(), 1);

    let dirty: i64 = conn
        .query_row("SELECT COUNT(*) FROM message WHERE dirty = 1", [], |r| {
            r.get(0)
        })
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
        db::append_message(&conn, "这条稍后会被删", None)
            .unwrap()
            .id
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
        search_hits(&conn, "稍后", 10).unwrap().is_empty(),
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
    //
    // 比的是**分页封套本身**（SearchPage），不是裸数组 —— 网页端拿到的就是
    // 这个对象，"还有没有更多"的判定也必须在两端一致。
    for q in ["苹果", "苹", "记录"] {
        assert_eq!(
            serde_json::to_value(db::search_page(&conn, q, 20, 0).unwrap()).unwrap(),
            get_json(addr, &format!("/api/search?q={}&limit=20", pct(q))),
            "检索「{q}」两端结果必须一致（含命中顺序和 hasMore）"
        );
    }

    // ---- 加载更多：翻到第二页，两端同样必须一致 ----
    assert_eq!(
        serde_json::to_value(db::search_page(&conn, "苹果", 1, 1).unwrap()).unwrap(),
        get_json(
            addr,
            &format!("/api/search?q={}&limit=1&offset=1", pct("苹果"))
        ),
        "检索的第二页两端必须一致"
    );

    // 得确认上面那些断言不是空跑：至少有一条查询要命中多个结果
    assert!(
        search_hits(&conn, "苹果", 20).unwrap().len() > 1,
        "测试不能空跑：检索的排序和分页只有多命中时才有意义"
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
        ("PATCH", Some(b)) => ureq::patch(&url)
            .header("Authorization", &auth)
            .send_json(b),
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
            search_hits(&conn, "网页端", 10).unwrap().len(),
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
            search_hits(&conn, "改过", 10).unwrap().len(),
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
        let hits = search_hits(&conn, "改过", 10).unwrap();
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

/// 只取状态码，**绝不读 body**。
///
/// `/api/events` 的 body 是一条故意永不结束的 SSE 流，而且**空闲时一个字节都不发**
/// （要等 15 秒的心跳注释）。`try_get` 走的 `read_json()` 是**流式**解析 —— 它得有
/// 字节才能失败，于是一个本来 1 毫秒的请求会白白花掉整整一个心跳间隔。
/// 详见 `read_one_sse_block` 上面那段。
fn status_only(addr: SocketAddr, path: &str, bearer_val: Option<&str>) -> u16 {
    let mut req = ureq::get(&format!("http://{addr}{path}"));
    if let Some(b) = bearer_val {
        req = req.header("Authorization", &format!("Bearer {b}"));
    }
    match req.call() {
        Ok(r) => r.status().as_u16(),
        Err(ureq::Error::StatusCode(c)) => c,
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
    let created = write_req_with(
        addr,
        "POST",
        "/api/message",
        Some(&serde_json::json!({ "body": "用会话写的" })),
        &session,
    );
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

    // `/api/events` 单独走状态码那条路：它的 body 永不结束，读它的人会卡住。
    assert_eq!(
        status_only(addr, "/api/events", Some(TOKEN)),
        200,
        "/api/events 应当接受长期令牌"
    );
    assert_eq!(
        status_only(addr, "/api/events", None),
        401,
        "/api/events 也必须在鉴权后面 —— 没带凭据时中间件在进 SSE 之前就回绝"
    );

    // 同步端点也一样
    let api = api(addr);
    assert!(api.handshake().is_ok());
}

/// 服务端拒绝时，**它那句解释必须能到用户眼前**。
///
/// `ureq` 默认会把 4xx 变成一个只带状态码的错误，顺手把响应体丢掉 ——
/// 于是用户看到的只有 "HTTP 400"，而"哪一条、为什么"明明就在那句被丢掉的话里。
/// 自建服务端的人不会去看服务端日志。
#[test]
fn a_rejected_push_surfaces_the_servers_explanation() {
    let addr = start_server();
    let http = api(addr);

    // 推一条指向不存在频道的消息：服务端会整批拒绝（引用完整性）
    let orphan = Change {
        seq: None,
        kind: EntityKind::Message,
        id: "orphan".into(),
        hlc: Hlc::new(100, 0, "test-device"),
        deleted: false,
        data: Some(serde_json::json!({
            "channelId": "ch-nope",
            "body": "孤儿消息",
            "createdAt": 100,
            "updatedAt": 100
        })),
    };

    let err = http.push(&[orphan]).expect_err("服务端应当拒绝这条");
    let msg = err.to_string();

    assert!(msg.contains("400"), "应当说明是 400：{msg}");
    assert!(
        msg.contains("ch-nope"),
        "应当把服务端那句话带出来（含具体的频道 id），实际：{msg}"
    );
}

// ---------------------------------------------------------------- 附件

/// 一个最小的"看起来像 PNG"的载荷：只要魔数对，嗅探就会认它。
fn fake_png(payload_len: usize) -> Vec<u8> {
    let mut v = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    // 内容要可区分，否则"拿回来的字节对不对"这个断言就是空跑
    for i in 0..payload_len {
        v.push((i % 251) as u8);
    }
    v
}

fn upload_blob(addr: SocketAddr, bytes: &[u8], bearer_val: &str) -> (u16, serde_json::Value) {
    match ureq::post(&format!("http://{addr}/api/blob"))
        .header("Authorization", &format!("Bearer {bearer_val}"))
        .send(bytes)
    {
        Ok(r) => {
            let code = r.status().as_u16();
            (
                code,
                r.into_body().read_json().unwrap_or(serde_json::Value::Null),
            )
        }
        Err(ureq::Error::StatusCode(c)) => (c, serde_json::Value::Null),
        Err(e) => panic!("上传附件失败：{e}"),
    }
}

/// 下载附件，返回 (状态码, Content-Type, Content-Disposition, X-Content-Type-Options, 字节)。
fn download_blob(
    addr: SocketAddr,
    sha: &str,
    bearer_val: &str,
) -> (u16, String, String, String, Vec<u8>) {
    let call = ureq::get(&format!("http://{addr}/api/blob/{sha}"))
        .header("Authorization", &format!("Bearer {bearer_val}"))
        .call();

    let header_of = |r: &ureq::http::Response<ureq::Body>, name: &str| -> String {
        r.headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string()
    };

    match call {
        Ok(r) => {
            let code = r.status().as_u16();
            let ct = header_of(&r, "content-type");
            let cd = header_of(&r, "content-disposition");
            let nosniff = header_of(&r, "x-content-type-options");
            // **必须显式放宽上限。** ureq 默认只读 10 MB（MAX_BODY_SIZE），
            // 而附件上限是 25 MB —— 用默认值的话大图会失败，而且失败得很安静。
            let body = r
                .into_body()
                .into_with_config()
                .limit(64 * 1024 * 1024)
                .read_to_vec()
                .unwrap_or_default();
            (code, ct, cd, nosniff, body)
        }
        Err(ureq::Error::StatusCode(c)) => (c, String::new(), String::new(), String::new(), vec![]),
        Err(e) => panic!("下载附件失败：{e}"),
    }
}

/// 附件走完一个来回：上传拿身份、下载拿字节、内容一致。
#[test]
fn a_blob_roundtrips_through_http() {
    let addr = start_server();
    let payload = fake_png(4096);

    let (code, up) = upload_blob(addr, &payload, TOKEN);
    assert_eq!(code, 200, "上传应当成功：{up}");
    let sha = up["sha256"].as_str().expect("要有 sha256").to_string();
    assert_eq!(sha.len(), 64);
    assert_eq!(up["size"].as_i64().unwrap(), payload.len() as i64);
    assert_eq!(
        up["mime"].as_str().unwrap(),
        "image/png",
        "类型必须由字节嗅探出来"
    );

    let (code, ct, _, nosniff, got) = download_blob(addr, &sha, TOKEN);
    assert_eq!(code, 200);
    assert_eq!(ct, "image/png");
    assert_eq!(nosniff, "nosniff", "任何响应都要禁止浏览器猜类型");
    assert_eq!(got, payload, "拿回来的字节必须和传上去的一模一样");
}

/// **axum 的默认请求体上限是 2 MB。** 不改的话任何一张真实照片都会被 413 挡掉，
/// 而且报的是"请求体过大"这种和附件八竿子打不着的错 —— 用户只会觉得"传图坏了"。
#[test]
fn a_photo_sized_upload_survives_the_default_body_limit() {
    let addr = start_server();
    let big = fake_png(3 * 1024 * 1024); // 3 MB，刚好越过 axum 的默认 2 MB

    let (code, up) = upload_blob(addr, &big, TOKEN);
    assert_eq!(
        code, 200,
        "3 MB 的上传必须成功 —— 默认请求体上限只有 2 MB，说明没放宽：{up}"
    );

    let sha = up["sha256"].as_str().unwrap().to_string();
    let (code, _, _, _, got) = download_blob(addr, &sha, TOKEN);
    assert_eq!(code, 200);
    assert_eq!(got.len(), big.len(), "大附件的字节数不能少");
    assert_eq!(got, big);
}

/// 上传一个声明成 `image/png` 的 HTML，服务端也只能把它当未知类型 ——
/// 并且下载时明确要求浏览器**下载**而不是渲染。
///
/// 同源部署下这条尤其要紧：网页端和服务端是同一个 origin，
/// 渲染一个上传上来的 HTML 就等于让脚本读走会话。
#[test]
fn the_stored_type_never_comes_from_the_request() {
    let addr = start_server();
    let html = b"<script>fetch('/api/timeline')</script>";

    // 客户端在请求里撒什么谎都没用 —— 服务端根本不读 Content-Type
    let resp = ureq::post(&format!("http://{addr}/api/blob"))
        .header("Authorization", &format!("Bearer {TOKEN}"))
        .header("Content-Type", "image/png")
        .send(&html[..])
        .expect("上传应当成功（存下来是可以的）");
    let up: serde_json::Value = resp.into_body().read_json().unwrap();
    let sha = up["sha256"].as_str().unwrap().to_string();

    assert_eq!(
        up["mime"].as_str().unwrap(),
        "application/octet-stream",
        "认不出来的字节只能得到兜底类型"
    );

    let (code, ct, cd, nosniff, got) = download_blob(addr, &sha, TOKEN);
    assert_eq!(code, 200);
    assert_eq!(ct, "application/octet-stream", "绝不能回 text/html");
    assert_eq!(cd, "attachment", "未知类型必须当下载，不能当可渲染内容");
    assert_eq!(nosniff, "nosniff");
    assert_eq!(got, html);
}

/// 同样一份内容传两次是幂等的，而且 sha 相同（内容寻址 = 天然去重）。
#[test]
fn uploading_the_same_bytes_twice_is_a_no_op() {
    let addr = start_server();
    let payload = fake_png(1024);

    let (_, first) = upload_blob(addr, &payload, TOKEN);
    let (code, second) = upload_blob(addr, &payload, TOKEN);
    assert_eq!(code, 200);
    assert_eq!(
        first["sha256"], second["sha256"],
        "同样的字节必须得到同样的名字"
    );

    // 内容不同则名字必须不同 —— 否则后一份会覆盖前一份
    let (_, other) = upload_blob(addr, &fake_png(1025), TOKEN);
    assert_ne!(first["sha256"], other["sha256"]);
}

/// 取一份服务端没有的附件要回 **404**，不是 500。
///
/// 客户端靠这个状态码区分"服务端也没有，别再重试"和"服务端出错了，等会儿再试"。
/// 回 500 会让下载队列永远卡在同一条上。
#[test]
fn an_absent_blob_is_a_404_not_a_500() {
    let addr = start_server();
    // 格式合法但不存在
    let absent = "0".repeat(64);
    let (code, _, _, _, _) = download_blob(addr, &absent, TOKEN);
    assert_eq!(code, 404);

    // 格式不合法则是 400 —— 这是请求方自己写错了，不是"没有"
    let (code, _, _, _, _) = download_blob(addr, "not-a-sha", TOKEN);
    assert_eq!(code, 400);
}

/// 附件端点和其他端点一样要鉴权。
#[test]
fn blob_endpoints_require_a_token() {
    let addr = start_server();
    let payload = fake_png(64);
    let (_, up) = upload_blob(addr, &payload, TOKEN);
    let sha = up["sha256"].as_str().unwrap().to_string();

    // 无凭据下载
    match ureq::get(&format!("http://{addr}/api/blob/{sha}")).call() {
        Err(ureq::Error::StatusCode(401)) => {}
        other => panic!("无凭据下载附件应当 401，实际：{other:?}"),
    }
    // 无凭据上传
    match ureq::post(&format!("http://{addr}/api/blob")).send(&payload[..]) {
        Err(ureq::Error::StatusCode(401)) => {}
        other => panic!("无凭据上传附件应当 401，实际：{other:?}"),
    }
}

/// 一次问清楚服务端缺哪些附件。
///
/// 没有这个端点，客户端每轮同步只能把本地所有图片盲传一遍。
#[test]
fn missing_blobs_reports_only_what_is_absent() {
    let addr = start_server();
    let have = fake_png(128);
    let (_, up) = upload_blob(addr, &have, TOKEN);
    let have_sha = up["sha256"].as_str().unwrap().to_string();

    let absent_sha = messagenote_core::attachment::sha256_hex(b"never uploaded");
    let bogus = "zzzz".to_string();

    let resp = write_req(
        addr,
        "POST",
        "/api/blob/missing",
        Some(&serde_json::json!({
            "shas": [have_sha, absent_sha, bogus],
        })),
    );
    let missing: Vec<String> = resp["missing"]
        .as_array()
        .expect("要有 missing 数组")
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();

    assert!(
        !missing.contains(&have_sha),
        "服务端已经有了的不该出现在 missing 里"
    );
    assert!(missing.contains(&absent_sha), "没传过的必须报缺");
    assert!(
        missing.contains(&bogus),
        "格式不合法的名字也算缺 —— 让上传方走完整校验路径"
    );
}

// ---------------------------------------------------------------- 附件同步

/// 图片从一台设备走到另一台 —— 这是整个附件功能的验收标准。
#[test]
fn an_image_travels_between_two_devices() {
    let addr = start_server();
    let api = api(addr);
    let a = db::open_memory("device-a").unwrap();
    let b = db::open_memory("device-b").unwrap();

    // A 粘了一张图，正文里引用它
    let png = fake_png(5000);
    let sha = {
        let conn = a.conn().unwrap();
        let sha = db::save_attachment(&conn, &png).unwrap();
        db::append_message(
            &conn,
            &format!(
                "看这张\n\n{}",
                messagenote_core::attachment::image_markdown(&sha, "图")
            ),
            None,
        )
        .unwrap();
        sha
    };

    // A 同步：这一轮里消息推上去、字节也传上去
    let ra = sync_once(&a, &api).unwrap();
    assert!(ra.pushed >= 1, "消息应当推出去了");
    assert_eq!(ra.blobs_up, 1, "附件字节应当跟着传上去了");

    // B 同步一次就该拿到**字节**，而不只是引用
    let rb = sync_once(&b, &api).unwrap();
    assert!(rb.pulled >= 1, "B 应当拉到了那条消息");
    assert_eq!(
        rb.blobs_down, 1,
        "同一轮里就该把图取回来 —— 拉到的正文会登记待下载，然后立刻去取"
    );

    let conn = b.conn().unwrap();
    assert!(
        db::blob::has_blob(&conn, &sha).unwrap(),
        "B 本地应当有这份字节"
    );
    assert_eq!(
        db::read_attachment(&conn, &sha).unwrap().1,
        png,
        "取回来的字节必须一个不差"
    );

    // 正文里的引用也要原样到达 —— 否则界面上会是一张破图
    let body: String = conn
        .query_row(
            "SELECT body FROM message WHERE deleted_at IS NULL ORDER BY created_at LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        messagenote_core::attachment::referenced_shas(&body).contains(&sha),
        "正文里的引用不该在同步中丢掉：{body}"
    );
}

/// 反过来也要通：B 加的图，A 能拿到。
#[test]
fn an_image_travels_back_to_the_other_device() {
    let addr = start_server();
    let api = api(addr);
    let a = db::open_memory("device-a").unwrap();
    let b = db::open_memory("device-b").unwrap();

    let png = fake_png(7777);
    let sha = {
        let conn = b.conn().unwrap();
        let sha = db::save_attachment(&conn, &png).unwrap();
        db::append_message(
            &conn,
            &messagenote_core::attachment::image_markdown(&sha, "反方向"),
            None,
        )
        .unwrap();
        sha
    };

    sync_once(&b, &api).unwrap();
    sync_once(&a, &api).unwrap();

    let conn = a.conn().unwrap();
    assert_eq!(
        db::read_attachment(&conn, &sha).unwrap().1,
        png,
        "反方向的字节也要一模一样"
    );
}

/// **取不到的附件不能阻塞整轮同步。**
///
/// 服务端还没有那份字节是**可以预期**的（上传那台设备可能还没开机）。
/// 如果下载失败直接把错误抛出去，一台设备会因为"某张图暂时不在"而
/// **完全同步不了** —— 笔记、标签、频道全都卡住，用户看到的是"同步坏了"，
/// 而真正的原因只是一张图。
#[test]
fn an_unreachable_attachment_does_not_block_sync() {
    let addr = start_server();
    let api = api(addr);
    let a = db::open_memory("device-a").unwrap();
    let b = db::open_memory("device-b").unwrap();

    // 造一条"引用了一个服务端根本没有的附件"的消息。
    //
    // 手工拼正文而不是走 save_attachment —— 要的正是一个**没有字节**的引用，
    // 模拟"对端提到过这张图，但字节始终没传上来"。
    let ghost = messagenote_core::attachment::sha256_hex(b"this blob was never uploaded");
    {
        let conn = a.conn().unwrap();
        db::append_message(
            &conn,
            &format!(
                "引用了不存在的东西\n\n{}",
                messagenote_core::attachment::reference(&ghost)
            ),
            None,
        )
        .unwrap();
        // 顺手再写一条正常的，用来验证"别的东西照样同步"
        db::append_message(&conn, "这条必须能同步过去", None).unwrap();
    }

    sync_once(&a, &api).unwrap();

    // B 同步：取图会失败，但整轮不能失败
    let rb = sync_once(&b, &api).expect("一张取不到的图不该让整轮同步失败");
    assert_eq!(rb.blobs_down, 0, "取不到就是取不到，不该算成功");
    assert!(rb.pulled >= 2, "两条消息都应当拉到");

    let conn = b.conn().unwrap();
    assert!(
        conn.query_row(
            "SELECT COUNT(*) FROM message WHERE body = '这条必须能同步过去'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap()
            > 0,
        "**别的内容必须照常同步** —— 这就是这条设计的目的"
    );

    // 而且失败会被记账，好让它下次排队尾，不堵住后面能下的东西
    let attempts: i64 = conn
        .query_row(
            "SELECT attempts FROM attachment WHERE sha256 = ?1",
            rusqlite::params![ghost],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(attempts, 1, "失败次数要记下来，否则它会一直占着队首");
}

/// 失败的条目会沉到队列后面去，不会饿死后面能下的。
///
/// 这是 `attempts` 那一列存在的**唯一理由**。没有它，`ORDER BY created_at`
/// 会让同一个取不到的附件每轮都排在第一个，后面的一律轮不到。
#[test]
fn a_repeatedly_failing_attachment_sinks_in_the_queue() {
    let db = db::open_memory("device-q").unwrap();
    let conn = db.conn().unwrap();

    let good = messagenote_core::attachment::sha256_hex(b"good");
    let bad = messagenote_core::attachment::sha256_hex(b"bad");

    // bad 先登记（时间更早），good 后登记
    db::blob::register_placeholder(&conn, &bad, 100).unwrap();
    db::blob::register_placeholder(&conn, &good, 200).unwrap();

    assert_eq!(
        db::pending_downloads(&conn, 1).unwrap(),
        vec![bad.clone()],
        "一开始按时间排，先登记的在前"
    );

    // bad 失败一次之后就该排到后面
    db::note_download_failure(&conn, &bad).unwrap();
    assert_eq!(
        db::pending_downloads(&conn, 1).unwrap(),
        vec![good],
        "失败过的必须沉下去，否则它会永远堵住队首"
    );
}

/// 下载到本地之后**不能**又被当成"待上传"传回去。
///
/// 漏了这一步的话，每台设备都会把从别人那里收到的图再传一遍 ——
/// 服务端幂等所以不会出错，但每轮同步都在白传几 MB。
#[test]
fn a_downloaded_blob_is_not_re_uploaded() {
    let addr = start_server();
    let api = api(addr);
    let a = db::open_memory("device-a").unwrap();
    let b = db::open_memory("device-b").unwrap();

    let png = fake_png(2048);
    let sha = {
        let conn = a.conn().unwrap();
        let sha = db::save_attachment(&conn, &png).unwrap();
        db::append_message(
            &conn,
            &messagenote_core::attachment::image_markdown(&sha, "图"),
            None,
        )
        .unwrap();
        sha
    };

    sync_once(&a, &api).unwrap();
    sync_once(&b, &api).unwrap();
    assert!(db::blob::has_blob(&b.conn().unwrap(), &sha).unwrap());

    // B 再同步一轮：什么都传不上去、也取不下来
    let again = sync_once(&b, &api).unwrap();
    assert_eq!(again.blobs_up, 0, "从服务端取回来的字节不该再传回去");
    assert_eq!(again.blobs_down, 0, "已经有了就不该再取一次");
}

// ---------------------------------------------------------------- 幂等写入

/// 网页端写入是幂等的：同一条请求（同一个 id）重放多少遍，只落**一条**记录，
/// 而且**不产生额外的变更日志条目**。
///
/// 这是 S3 离线队列的地基。没有它，一次网络抖动 = 一条重复记录，
/// 而用户看到的是"我的笔记莫名其妙变多了"，且没有任何地方报错。
///
/// 第二半（变更日志）比第一半更重要：只查"库里几条"会漏掉"服务端每次都
/// 老实写了一遍变更、只是被别的东西挡住了"这种情况 —— 那样别的设备会看到
/// 一次幽灵更新，而本地的条数检查照样通过。
#[test]
fn reposting_the_same_client_id_writes_exactly_one_change() {
    let addr = start_server();
    let id = "e2e-idempotent-1";
    let body = serde_json::json!({ "body": "只该出现一次", "id": id });

    let first = write_req(addr, "POST", "/api/message", Some(&body));
    assert_eq!(first["id"], id, "应当采用客户端给的 id，而不是另生成一个");
    assert_eq!(first["body"], "只该出现一次");

    // 原样重放三次 —— 模拟"响应丢了、客户端重试"
    for round in 0..3 {
        let again = write_req(addr, "POST", "/api/message", Some(&body));
        assert_eq!(again["id"], id, "第 {round} 次重放应当拿回同一条");
        assert_eq!(again["body"], "只该出现一次");
    }

    // 库里只有一条
    let (_, timeline) = try_get(addr, "/api/timeline?scope=all&limit=100", Some(TOKEN));
    assert_eq!(
        timeline["items"].as_array().unwrap().len(),
        1,
        "重放不该产生第二条记录：{timeline}"
    );

    // 变更日志里也只有一条 —— 否则别的设备会看到一次幽灵更新
    let (_, pull) = try_get(addr, "/api/sync/pull?since=0&limit=200", Some(TOKEN));
    let ids: Vec<&str> = pull["changes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["kind"] == "message")
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        vec![id],
        "变更日志里只该有一条消息变更，重放不能往里追加"
    );
}

/// **重放不能把用户删掉的记录复活。**
///
/// 判"这个 id 用过没有"时必须**不管墓碑**。判成"有没有没被删的那一行"的话，
/// "删除 → 队列重放"就会把记录变回来，而删除是用户明确表达过的意图。
#[test]
fn replaying_a_create_for_a_deleted_message_does_not_resurrect_it() {
    let addr = start_server();
    let id = "e2e-idempotent-2";
    let body = serde_json::json!({ "body": "准备被删掉的", "id": id });

    write_req(addr, "POST", "/api/message", Some(&body));
    write_req(addr, "DELETE", &format!("/api/message/{id}"), None);

    let (_, after_delete) = try_get(addr, "/api/timeline?scope=all&limit=100", Some(TOKEN));
    assert_eq!(
        after_delete["items"].as_array().unwrap().len(),
        0,
        "先确认真的删掉了"
    );

    // 队列重放
    write_req(addr, "POST", "/api/message", Some(&body));

    let (_, after_replay) = try_get(addr, "/api/timeline?scope=all&limit=100", Some(TOKEN));
    assert_eq!(
        after_replay["items"].as_array().unwrap().len(),
        0,
        "重放**不能**让删掉的记录复活：{after_replay}"
    );
}

/// 形状不合法的幂等键要拒绝（400），不能当成"没给"。
///
/// 当成"没给"的话，一个传空串的客户端会得到**非幂等**的写入，而它以为自己
/// 传了幂等键 —— 最坏的一类失败：行为与契约不符，且不报错。
#[test]
fn a_malformed_client_id_is_a_400_not_a_silent_new_id() {
    let addr = start_server();
    for bad in ["", "   ", &"z".repeat(129)] {
        match ureq::post(&format!("http://{addr}/api/message"))
            .header("Authorization", &format!("Bearer {TOKEN}"))
            .send_json(serde_json::json!({ "body": "x", "id": bad }))
        {
            Err(ureq::Error::StatusCode(400)) => {}
            other => panic!("不合法的 id 应当 400，实际：{other:?}"),
        }
    }

    let (_, timeline) = try_get(addr, "/api/timeline?scope=all&limit=100", Some(TOKEN));
    assert_eq!(
        timeline["items"].as_array().unwrap().len(),
        0,
        "被拒绝的请求一条也不该落库"
    );
}

// ---------------------------------------------------------------- 实时推送

/// 连上 `/api/events` 并读到**第一个完整事件块**，然后返回它。
///
/// 用带显式 `timeout_recv_body` 的独立 agent：SSE 永远不会结束，
/// 万一事件没等到，至少会在这个窗口之后报错而不是无限挂住。
///
/// ## 那个「多花一个心跳间隔」的开销：已查清（2026-09）
///
/// 这里原先记着一条猜测 —— "只要这两个 SSE 测试在，整个测试二进制就要多花一个
/// 心跳间隔（15 秒）才退出"，并排除了"测试本身慢"和"`ureq` 全局 agent 留连接"。
/// **猜测是错的**：把这两条测试整个 `--skip` 掉，二进制仍然是 15.12 秒。
///
/// 真正的来源是 `the_long_lived_token_still_works_directly`：它把 `/api/events`
/// 和另外三个 JSON 端点一起放进 `try_get`，而 `try_get` 会 `read_json()`。
/// `read_json()` 是**流式**解析 —— 它得有字节才能失败，而这条 SSE 流在空闲时
/// **一个字节都不发**，要等 15 秒的心跳注释才吐出第一个 `:`，于是解析器到那才
/// 报 `expected value`，又被 `unwrap_or(Null)` 吞掉。实测三段耗时：
///
/// ```text
/// /api/channels        2.1ms
/// /api/timeline/stats  1.0ms
/// /api/tags            1.2ms
/// /api/events          响应头 0.95ms  →  body 15.0102s（Err: expected value:1:1）
/// ```
///
/// 这也解释了当初那个"改心跳就跟着变"的观察：延迟严格等于 `KEEPALIVE_INTERVAL`，
/// 因为那正是第一个字节到达的时刻。修法是 `status_only()` —— 对这个端点只看状态码。
///
/// 教训：**"读整个 body"这个动作不能用在一条故意不结束的流上**，而它失败的方式
/// 是"慢 15 秒且测试通过"，不是报错。
fn read_one_sse_block(addr: SocketAddr) -> Vec<String> {
    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            // 只要在这个窗口内没等到心跳就当连接坏了。它比服务端的心跳间隔
            // 长，所以正常情况下永远不会触发。
            .timeout_recv_body(Some(std::time::Duration::from_secs(20)))
            .build(),
    );

    let resp = agent
        .get(&format!("http://{addr}/api/events"))
        .header("Authorization", &format!("Bearer {TOKEN}"))
        .call()
        .expect("应当能建立 SSE 连接");
    assert_eq!(resp.status().as_u16(), 200);

    // 用 reader() 而不是 read_to_string()：SSE 是长连接，永远不会结束，
    // 而 read_to_* 那条路还有 10 MB 上限。
    let body = resp.into_body();
    let mut lines = std::io::BufReader::new(body.into_with_config().reader());
    let mut line = String::new();
    let mut block: Vec<String> = Vec::new();

    for _ in 0..100 {
        line.clear();
        match std::io::BufRead::read_line(&mut lines, &mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let t = line.trim().to_string();
        if t.is_empty() {
            // 空行 = 一个事件块结束
            if !block.is_empty() {
                break;
            }
            continue;
        }
        block.push(t);
    }
    block
}

/// 有变更时，连着的客户端**立刻**收到推送 —— 而不是等下一次轮询。
#[test]
fn a_write_pushes_an_event_to_connected_clients() {
    let addr = start_server();

    // **订阅必须在写入之前建立。** 广播是"当时有谁在听就给谁"，
    // 反过来的话接收者为空、事件静默丢掉，测试会超时而不是失败得清楚。
    let reader = std::thread::spawn(move || read_one_sse_block(addr));

    // 给订阅一点时间真的建立起来
    std::thread::sleep(std::time::Duration::from_millis(300));

    write_req(
        addr,
        "POST",
        "/api/message",
        Some(&serde_json::json!({ "body": "触发一次推送" })),
    );

    let block = reader.join().expect("读线程不该 panic");
    assert!(
        block.iter().any(|l| l == "event: changed"),
        "写入之后应当立刻收到推送，实际读到：{block:?}"
    );
}

/// 推送的内容是"有东西变了"，**不是**变更本身。
///
/// 这条断言的价值在于拦住将来有人"顺手"把变更内容塞进事件里：
/// 那样就得在这里重写一遍"哪些变更该发给谁"，而拉取那套游标逻辑
/// 已经有测试守着了。两份实现对同一件事给出不同答案，是同步 bug 的温床。
#[test]
fn the_push_event_carries_no_payload() {
    let addr = start_server();

    let reader = std::thread::spawn(move || read_one_sse_block(addr));
    std::thread::sleep(std::time::Duration::from_millis(300));
    write_req(
        addr,
        "POST",
        "/api/message",
        Some(&serde_json::json!({ "body": "看看事件里带了什么" })),
    );

    let block = reader.join().expect("读线程不该 panic");
    let data_lines: Vec<&String> = block.iter().filter(|l| l.starts_with("data:")).collect();
    assert_eq!(data_lines.len(), 1, "事件块应当只有一行 data：{block:?}");
    assert_eq!(
        data_lines[0].trim(),
        "data: 1",
        "data 只该是个信号，不该夹带变更内容：{block:?}"
    );
    for l in &block {
        assert!(
            !l.contains("看看事件里带了什么"),
            "推送里不该出现记录正文：{block:?}"
        );
    }
}
