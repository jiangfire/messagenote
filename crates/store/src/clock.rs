//! `meta(key, value)` 表访问，以及混合逻辑时钟的推进。
//!
//! ## 为什么这个必须共享
//!
//! [`clock_next`] 决定每条变更拿到什么时间戳，而时间戳决定"谁更新"。
//! 两端各写一份的话，两边会各自认为自己的版本胜出，最终**收敛不到同一个状态，
//! 而且不报任何错**。这跟 `core::merge` 被共享是同一个理由 ——
//! 裁定规则和喂给它的时钟必须是同一套。
//!
//! ## 对表的要求
//!
//! ```sql
//! CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
//! ```
//! 客户端一直有这张表；服务端是为了代笔写入才加的。

use rusqlite::{params, Connection, OptionalExtension};

use messagenote_core::hlc::Hlc;

const DEVICE_KEY: &str = "device_id";
const WALL_KEY: &str = "hlc_wall";
const COUNTER_KEY: &str = "hlc_counter";

/// 读一个 meta 值。
pub fn get(conn: &Connection, key: &str) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT value FROM meta WHERE key = ?1",
        params![key],
        |r| r.get(0),
    )
    .optional()
}

/// 写一个 meta 值。
pub fn set(conn: &Connection, key: &str, value: &str) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

/// 本机设备 id。没设置过就是空串。
pub fn device_id(conn: &Connection) -> rusqlite::Result<String> {
    Ok(get(conn, DEVICE_KEY)?.unwrap_or_default())
}

/// 给这份库分配一个设备 id（已经有了就不动）。
///
/// 每台设备 —— **包括代笔的服务端** —— 都必须有自己唯一的 id。HLC 是
/// `(wall, counter, device)` 三元组，`device` 那一层是用来打破平局的：
/// 两台设备共用同一个 id 时，同一毫秒的两次写入会被判成"同一个变更"，
/// 后一次会被幂等路径静默丢弃。
pub fn ensure_device_id(conn: &Connection) -> rusqlite::Result<String> {
    if let Some(id) = get(conn, DEVICE_KEY)? {
        if !id.is_empty() {
            return Ok(id);
        }
    }
    let id = uuid::Uuid::now_v7().to_string();
    set(conn, DEVICE_KEY, &id)?;
    Ok(id)
}

pub fn read_clock(conn: &Connection, device: &str) -> rusqlite::Result<Hlc> {
    let wall = get(conn, WALL_KEY)?
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(0);
    let counter = get(conn, COUNTER_KEY)?
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(0);
    Ok(Hlc::new(wall, counter, device))
}

pub fn write_clock(conn: &Connection, hlc: &Hlc) -> rusqlite::Result<()> {
    set(conn, WALL_KEY, &hlc.wall.to_string())?;
    set(conn, COUNTER_KEY, &hlc.counter.to_string())?;
    Ok(())
}

/// 推进本机时钟，返回本次变更应使用的时间戳。
///
/// **必须在调用方的同一个事务内调用**：时钟前进和数据落盘要么一起成功、
/// 要么一起失败。崩在中间会让重启后的时钟回退，此后写出的每一条都比已有的
/// 更旧、在裁定里持续判负 —— 表现成"我写的东西全都不见了"，且不报任何错。
pub fn clock_next(conn: &Connection) -> rusqlite::Result<Hlc> {
    let device = device_id(conn)?;
    let mut hlc = read_clock(conn, &device)?;
    hlc.tick(messagenote_core::now_ms());
    write_clock(conn, &hlc)?;
    Ok(hlc)
}

/// 观察到远端时间戳后校正本机时钟。
///
/// 收到比本机更超前的时间戳时把时钟拉前，避免此后持续落后、每次冲突都输。
pub fn clock_observe(conn: &Connection, remote: &Hlc) -> rusqlite::Result<()> {
    let device = device_id(conn)?;
    let mut hlc = read_clock(conn, &device)?;
    hlc.observe(remote, messagenote_core::now_ms());
    write_clock(conn, &hlc)
}
