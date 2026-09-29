//! 服务端推送（SSE）的订阅。
//!
//! 桌面端原先固定 45 秒轮询一次。多设备同时开着时那个延迟是能感觉到的：
//! 手机上记一笔，电脑上要等半分钟。这里连一条 SSE，服务端一有写入就把
//! 同步线程叫醒。
//!
//! **轮询保留，两者不是二选一。** 中间任何一层（反代、NAT、笔记本合盖再
//! 打开）都可能把这条长连接悄悄掐掉，而"掐掉了"和"没有新数据"在客户端
//! 看起来一模一样。所以推送负责快，轮询负责兜底 —— 少了轮询，
//! 这条连接一死，同步就**永久停摆**且不报错。
//!
//! ## 为什么不能设整体超时
//!
//! SSE 是一条**故意不结束**的响应。`timeout_global` 会在固定时间之后把它
//! 掐掉，表现是每隔一段时间重连一次、中间那些推送全丢。这里只设
//! `timeout_recv_body`：它约束的是"两次读到数据之间最长隔多久"，而服务端
//! 每 15 秒发一次心跳，所以正常情况下永远不会触发；真触发了就说明这条
//! 连接已经名存实亡，断开重连是对的。

use std::io::{BufRead, BufReader};
use std::time::Duration;

use tauri::{AppHandle, Manager};

use crate::db::{self, Db};
use crate::sync_worker::SyncWorker;

/// 连接超时。和同步那边保持一致。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(4);

/// 两次读到数据之间最多等多久。
///
/// 服务端每 15 秒一次心跳，所以正常情况下永远不触发。触发就意味着这条连接
/// 已经死了 —— 反代把它掐了，或者机器睡了一觉之后 socket 成了半开的。
const READ_TIMEOUT: Duration = Duration::from_secs(25);

const RETRY_BASE: Duration = Duration::from_secs(2);
const RETRY_MAX: Duration = Duration::from_secs(60);

/// 一次订阅尝试的结果。
///
/// **为什么要把"连上了"和"读到事件了"分成两件事**：退避要回答的只有
/// "地址和令牌对不对"。原先这两件事被混成了同一个返回值（"读到过 `data:` 帧
/// 没有"），于是**一条健康的连接如果在空闲期被掐掉** —— 没有任何写入，
/// 15 秒一次的心跳又不算数据 —— 它会被判成"连接失败"，退避一路翻倍到 60 秒，
/// 那段时间里的推送全丢。
///
/// 而"空闲期被掐掉"恰恰是**最常见**的情形：反代/NAT 掐的从来不是活跃连接，
/// 是闲置的那些。这个 bug 的表现因此是"偶尔要等半分钟才同步"，
/// 而不是"同步坏了" —— 最难查的那一类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Connected {
    /// 连上了（HTTP 2xx）。地址和令牌是对的。
    Established,
    /// 压根没连上：网络不通、令牌不对、DNS 挂了……
    Failed,
}

/// 这次连接之后，**下一次**重连前要等多长。
fn next_wait(current: Duration, outcome: Connected) -> Duration {
    match outcome {
        // 连上了就说明配置是对的，下次照最短间隔重试。
        Connected::Established => RETRY_BASE,
        // 连不上：翻倍退避，免得服务端关着的时候每 2 秒敲一次门。
        Connected::Failed => (current * 2).min(RETRY_MAX),
    }
}

/// 起一条常驻线程维护订阅。应用生命周期内只有一条。
pub fn spawn(app: AppHandle) {
    std::thread::spawn(move || {
        let mut backoff = RETRY_BASE;
        loop {
            let Some((url, token)) = current_config(&app) else {
                // 还没配同步服务端。等着 —— 用户随时可能去设置里填上。
                std::thread::sleep(RETRY_BASE);
                continue;
            };

            // 连上过就说明地址和令牌是对的，退避重置；否则一路退到上限，
            // 免得服务端关着的时候每 2 秒敲一次门。
            let outcome = subscribe(&app, &url, &token);
            let wait = backoff;
            backoff = next_wait(backoff, outcome);
            std::thread::sleep(wait);
        }
    });
}

/// 连一次，读到断开或配置变化为止。
///
/// 返回**有没有连上**（而不是"有没有读到事件"）—— 这个区别决定了退避会不会
/// 把一条健康的空闲连接当成失败。见 `Connected`。
fn subscribe(app: &AppHandle, url: &str, token: &str) -> Connected {
    connect(url, token, || wake(app), || config_changed(app, url, token))
}

/// 和 `AppHandle` 无关的那一半：建连接、一直读、把事件交给回调。
///
/// 拆出来是为了让**这条路径能被真实服务端测**
/// （见 `an_event_from_a_real_server_reaches_the_callback`）。它值得被单独测，
/// 是因为同类错误刚刚在端到端代理里发生过一次：连接建立得好好的、
/// 一条事件都收不到，而**两端都不报错**。
fn connect(
    url: &str,
    token: &str,
    on_event: impl FnMut(),
    should_stop: impl FnMut() -> bool,
) -> Connected {
    let endpoint = format!("{}/api/events", url.trim_end_matches('/'));

    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .timeout_connect(Some(CONNECT_TIMEOUT))
            // **整体超时必须关掉。** 见文件头：SSE 是故意不结束的响应，
            // 整体超时会把好好的连接定时掐断。
            .timeout_global(None)
            .timeout_recv_body(Some(READ_TIMEOUT))
            .build(),
    );

    let resp = match agent
        .get(&endpoint)
        .header("Authorization", &format!("Bearer {token}"))
        .header("Accept", "text/event-stream")
        .call()
    {
        Ok(r) => r,
        Err(_) => return Connected::Failed,
    };
    if !(200..300).contains(&resp.status().as_u16()) {
        return Connected::Failed;
    }

    // **走到这里就说明地址和令牌都是对的**，从这一刻起退避就该重置 ——
    // 这条流之后读多久、有没有读到事件，都和"配置对不对"无关。
    // 读的返回值刻意丢掉：它过去被当成"这次连接成不成功"，正是那个 bug。
    pump(
        &mut BufReader::new(resp.into_body().into_with_config().reader()),
        on_event,
        should_stop,
    );
    Connected::Established
}

/// 逐行读 SSE 帧，读到断开或该停为止。
///
/// 抽出来单独测：这一段是纯字符串处理，而它出错的表现是**静默收不到推送** ——
/// 没有报错，只是界面一直不动。这种错误值得一个不用起网络就能跑的测试。
///
/// **刻意不返回"读到过事件没有"。** 那个值以前被当成"这次连接成不成功"，
/// 于是空闲期被掐掉的健康连接会被判成失败、退避翻倍（见 `Connected`）。
/// 谁要是需要"有没有事件"，用 `on_event` 自己数。
///
/// - `on_event`：收到一个带 data 的帧（也就是真事件）
/// - `should_stop`：在**心跳帧**上问一次"还该继续吗"。只在这里问是因为
///   它每 15 秒才来一次，频率够低；每个事件都去查一次配置是白费数据库锁。
fn pump<R: BufRead>(
    reader: &mut R,
    mut on_event: impl FnMut(),
    mut should_stop: impl FnMut() -> bool,
) {
    let mut line = String::new();

    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }

        // 行是按 `\n` 切的，所以 CRLF 会在行尾留下一个 `\r`。
        // 这里**不需要**特意去掉它：下面全部用 `starts_with` 判断，
        // 多一个尾随字符不影响结论。（哪天改成精确比较就得补上 ——
        // `crlf_line_endings_work` 会把那次改动钉出来。）
        let t = line.as_str();

        // `:` 开头是注释 —— 心跳就是它。**不能当成事件**，
        // 否则每 15 秒就会白白触发一次同步。
        if t.starts_with(':') {
            if should_stop() {
                return;
            }
            continue;
        }

        if t.starts_with("data:") {
            on_event();
        }
        // 其余（`event:`、空行）忽略：我们只有一种事件，内容也不关心。
    }
}

/// 把同步线程叫醒。
///
/// 复用 `SyncWorker` 已有的唤醒通道，而不是自己再搞一套同步锁：
/// 工作线程是**单线程循环**，天然不会有两个同步并发跑。这正是
/// "让跑得快的那个去叫醒它"而不是"两个线程各自同步"的原因 ——
/// 后者会让两次 `take_pending_batch` 取到同一批变更、推两遍。
fn wake(app: &AppHandle) {
    if let Some(worker) = app.try_state::<SyncWorker>() {
        worker.trigger();
    }
}

/// 同步配置被改过了吗（换了服务器或令牌）。
///
/// 不处理这件事的话，用户在设置里换掉服务端之后，这条长连接会**一直连着
/// 旧的**，而界面看起来一切正常 —— 推送收不到，只剩轮询慢慢兜着。
fn config_changed(app: &AppHandle, url: &str, token: &str) -> bool {
    match current_config(app) {
        Some((u, t)) => u != url || t != token,
        None => true,
    }
}

fn current_config(app: &AppHandle) -> Option<(String, String)> {
    let db = app.try_state::<Db>()?;
    let conn = db.try_conn()?;
    let cfg = db::get_sync_config(&conn).ok()?;
    if cfg.url.is_empty() || cfg.token.is_empty() {
        return None;
    }
    Some((cfg.url, cfg.token))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    const TOKEN: &str = "sse-test-token-0123456789abcdefghijklmnop";

    /// 在专用线程 + 专用 tokio runtime 上跑一个真实服务端，绑临时端口。
    ///
    /// 必须分开线程：测试主体用的是**阻塞式** ureq，和 tokio worker
    /// 挤在同一条线程上会互相堵死。
    fn start_server() -> SocketAddr {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("建 tokio runtime");
            rt.block_on(async move {
                let state = Arc::new(messagenote_server::AppState {
                    store: messagenote_server::Store::in_memory().expect("建服务端库"),
                    token: TOKEN.to_string(),
                });
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                    .await
                    .expect("绑定临时端口");
                tx.send(listener.local_addr().expect("读监听地址"))
                    .expect("回传地址");
                messagenote_server::serve(listener, state)
                    .await
                    .expect("服务端异常退出");
            });
        });
        rx.recv().expect("等待服务端就绪")
    }

    /// 真实 HTTP + 真实流式响应：ureq 那边的配置能不能建立一条长连接、
    /// 并真的把事件送到回调。
    ///
    /// 这条测试值得存在，是因为同类错误刚刚在端到端代理里发生过一次：
    /// 连接建立得好好的、一条事件都收不到，而**两端都不报错**。
    /// 上面那些 `pump` 的单元测试只覆盖了字符串解析那一半，
    /// 覆盖不到"流根本没流过来"。
    #[test]
    fn an_event_from_a_real_server_reaches_the_callback() {
        let addr = start_server();
        let hits = Arc::new(AtomicUsize::new(0));

        let h = Arc::clone(&hits);
        std::thread::spawn(move || {
            let _ = connect(
                &format!("http://{addr}"),
                TOKEN,
                move || {
                    h.fetch_add(1, Ordering::SeqCst);
                },
                // 永不主动停 —— 这条连接由测试进程结束来收场
                || false,
            );
        });

        // 等订阅真的建立起来。反过来的话广播时没有接收者，事件静默丢掉。
        std::thread::sleep(Duration::from_millis(300));

        // 从"别处"写一条，模拟另一台设备
        ureq::post(&format!("http://{addr}/api/message"))
            .header("Authorization", &format!("Bearer {TOKEN}"))
            .send_json(serde_json::json!({ "body": "叫醒它" }))
            .expect("写入应当成功");

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while hits.load(Ordering::SeqCst) == 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }

        assert!(
            hits.load(Ordering::SeqCst) > 0,
            "真实服务端推过来的事件没有到达回调 —— 连接建好了却收不到东西，\
             正是流被缓冲住、或者超时配置写错时的表现"
        );
    }

    fn feed(input: &str, stop_after: usize) -> (usize, usize) {
        let events = Arc::new(AtomicUsize::new(0));
        let beats = Arc::new(AtomicUsize::new(0));

        let e = Arc::clone(&events);
        let b = Arc::clone(&beats);
        pump(
            &mut Cursor::new(input.as_bytes().to_vec()),
            move || {
                e.fetch_add(1, Ordering::SeqCst);
            },
            move || {
                let n = b.fetch_add(1, Ordering::SeqCst) + 1;
                n > stop_after
            },
        );

        (events.load(Ordering::SeqCst), beats.load(Ordering::SeqCst))
    }

    /// **退避看的是"连上了没有"，不是"读到事件了没有"。**
    ///
    /// 守的是一个很难复现、也很难看出来的 bug：空闲期被反代/NAT 掐掉的**健康**
    /// 连接会被判成失败，退避一路翻倍到 60 秒 —— 表现是"偶尔要等半分钟才同步"，
    /// 而不是"同步坏了"。反向验证过：把 `Established` 也按失败算
    /// （返回 `current * 2`），第一条断言立刻红。
    #[test]
    fn a_connection_that_came_up_resets_the_backoff_even_with_no_events() {
        // 连上过 → 下次照最短间隔重试，**和这一轮有没有事件无关**。
        // 注意它给的是"退到上限"的当前值：健康连接不该被这个值拖累。
        assert_eq!(next_wait(RETRY_MAX, Connected::Established), RETRY_BASE);

        // 连不上 → 逐次翻倍，封顶在 RETRY_MAX
        assert_eq!(
            next_wait(RETRY_BASE, Connected::Failed),
            Duration::from_secs(4)
        );
        assert_eq!(
            next_wait(Duration::from_secs(4), Connected::Failed),
            Duration::from_secs(8)
        );
        assert_eq!(
            next_wait(Duration::from_secs(40), Connected::Failed),
            RETRY_MAX
        );
        assert_eq!(
            next_wait(RETRY_MAX, Connected::Failed),
            RETRY_MAX,
            "到顶之后不能再涨"
        );
    }

    /// 正常帧：`event:` + `data:` + 空行，算**一个**事件。
    #[test]
    fn a_normal_frame_is_one_event() {
        let (events, _) = feed("event: changed\ndata: 1\n\n", usize::MAX);
        assert_eq!(events, 1);
    }

    /// **心跳不能算事件。**
    ///
    /// 当成事件的话，每 15 秒就会白白触发一次完整同步 —— 而它看起来
    /// 完全正常，只是流量和耗电悄悄上去了。
    #[test]
    fn keepalive_comments_are_not_events() {
        let (events, beats) = feed(": keep-alive\n\n: keep-alive\n\n", usize::MAX);
        assert_eq!(events, 0, "注释帧不该触发同步");
        assert_eq!(beats, 2, "但它是检查配置变化的时机");
    }

    /// 连着的多个事件都要收到。
    #[test]
    fn every_data_frame_fires() {
        let (events, _) = feed(
            "event: changed\ndata: 1\n\nevent: changed\ndata: 1\n\n",
            usize::MAX,
        );
        assert_eq!(events, 2);
    }

    /// `\r\n` 行尾也要认。规范允许，而某些反代会改写行尾。
    ///
    /// 这条测试钉的是一个**前提**：判断用的是 `starts_with`，所以行尾多一个
    /// `\r` 不影响结论。哪天有人改成精确比较（`t == "data: 1"`），它会立刻红。
    #[test]
    fn crlf_line_endings_work() {
        let (events, beats) = feed(
            ": keep-alive\r\n\r\nevent: changed\r\ndata: 1\r\n\r\n",
            usize::MAX,
        );
        assert_eq!(events, 1, "CRLF 的 data 帧照样要算一个事件");
        assert_eq!(beats, 1, "CRLF 的心跳照样要能被认出来");
    }

    /// 流结束就返回，不死等。
    #[test]
    fn the_end_of_the_stream_returns() {
        let (events, _) = feed("event: changed\ndata: 1\n\n", usize::MAX);
        assert_eq!(events, 1, "读完就返回（Cursor 会 EOF）");
    }

    /// 配置改了就在下一个心跳上收手 —— 换服务端之后不该一直连着旧的。
    #[test]
    fn a_config_change_stops_the_stream() {
        let input = ": keep-alive\n\n: keep-alive\n\n: keep-alive\n\n";
        let (_, beats) = feed(input, 1);
        assert_eq!(
            beats, 2,
            "第一次心跳返回 false（继续），第二次返回 true（停下）"
        );
    }
}
