//! 混合逻辑时钟（Hybrid Logical Clock，HLC）。
//!
//! ## 为什么不能直接用物理时间
//!
//! 多设备同步要回答"同一行两边都改了，谁赢"。用物理时间戳最直观，但
//! 设备时钟会偏。某台设备快 5 分钟，它的每次修改都会赢过另一台设备
//! **此后 5 分钟内**的所有修改 —— 这不是偶发错误，是持续性的单向压制。
//!
//! ## 为什么不能只用逻辑计数器
//!
//! 纯逻辑时钟（Lamport）能保证因果序，但它丢掉了"大概是什么时候"这个信息，
//! 而"越新的修改赢"恰好符合用户直觉：你刚在两台设备上各改了一版，
//! 你期望较晚的那次生效。
//!
//! ## HLC 的做法
//!
//! 时间戳是一个三元组 `(wall, counter, device)`：
//!
//! - `wall` 大致跟随物理时间，但不允许回退
//! - `counter` 吸收"同一毫秒内的多次事件"以及"对端时钟更快"这两种情况
//! - `device` 只是同 wall 同 counter 时的确定性打破平局手段
//!
//! 关键在 [`Hlc::observe`]：收到远端时间戳时把本地时钟**拉前**到两者最大值。
//! 于是时钟偏差最多造成一次性的影响，会被立刻校正掉，不会永久积累。
//! 这是 HLC 相比裸物理时间戳的本质优势。

use serde::{Deserialize, Serialize};
use std::cmp::Ordering;

/// 当前 Unix 毫秒时间戳。
///
/// 放在内核里是为了让桌面端和服务端用同一个实现 —— 时间来源不一致
/// 本身就是一类难查的 bug。
pub fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Hlc {
    /// 物理时间分量（毫秒），单调不回退
    pub wall: i64,
    /// 逻辑计数器，用来吸收同毫秒事件与对端时钟偏差
    pub counter: u32,
    /// 产生这个时间戳的设备 id，仅用于确定性打破平局
    pub device: String,
}

impl Hlc {
    pub fn new(wall: i64, counter: u32, device: impl Into<String>) -> Self {
        Self {
            wall,
            counter,
            device: device.into(),
        }
    }

    /// "从未同步过"的哨兵值。任何真实时间戳都大于它。
    pub fn zero() -> Self {
        Self::new(i64::MIN, 0, String::new())
    }

    pub fn is_zero(&self) -> bool {
        self.wall == i64::MIN
    }

    /// 本地发生了事件，推进时钟。
    ///
    /// 物理时间前进就重置 counter；没前进（同一毫秒内多次写入，或物理时钟
    /// 回拨）就靠 counter 递增维持单调性。
    pub fn tick(&mut self, physical_ms: i64) {
        if physical_ms > self.wall {
            self.wall = physical_ms;
            self.counter = 0;
        } else {
            self.counter = self.counter.saturating_add(1);
        }
    }

    /// 收到远端时间戳，按 HLC 规则校正本地时钟。
    pub fn observe(&mut self, remote: &Hlc, physical_ms: i64) {
        let wall = physical_ms.max(self.wall).max(remote.wall);
        let counter = if wall == self.wall && wall == remote.wall {
            // 三者相同：取两个 counter 的较大值再加一
            self.counter.max(remote.counter).saturating_add(1)
        } else if wall == self.wall {
            // 本地或远端与本地同 wall，但物理时间没有更超前
            self.counter.saturating_add(1)
        } else if wall == remote.wall {
            // 远端更超前：跟到它后面
            remote.counter.saturating_add(1)
        } else {
            // 物理时间走到了所有已知事件之前，counter 归零
            0
        };
        self.wall = wall;
        self.counter = counter;
    }
}

/// 全序比较：先比 wall，再比 counter，最后用 device 确定性打破平局。
///
/// `device` 这一层是必需的：两台设备完全可能在同一毫秒各写一次。
/// 如果不打破平局，两边会各自认为"我的更新"，最终收敛到不同状态 ——
/// 这是同步 bug 里最难查的一类。
impl Ord for Hlc {
    fn cmp(&self, other: &Self) -> Ordering {
        self.wall
            .cmp(&other.wall)
            .then_with(|| self.counter.cmp(&other.counter))
            .then_with(|| self.device.cmp(&other.device))
    }
}

impl PartialOrd for Hlc {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_millisecond_bumps_counter() {
        let mut c = Hlc::new(1000, 0, "a");
        c.tick(1000);
        assert_eq!(c.counter, 1);
        c.tick(1000);
        assert_eq!(c.counter, 2);
        assert_eq!(c.wall, 1000, "物理时间没前进时 wall 不应变");
    }

    #[test]
    fn physical_advance_resets_counter() {
        let mut c = Hlc::new(1000, 5, "a");
        c.tick(2000);
        assert_eq!((c.wall, c.counter), (2000, 0));
    }

    #[test]
    fn physical_clock_going_backwards_still_yields_monotonic_hlc() {
        // 用户机器上 NTP 校正导致时钟回拨：HLC 必须仍然单调递增
        let mut c = Hlc::new(5000, 0, "a");
        c.tick(1000);
        assert_eq!(c.wall, 5000, "wall 不允许回退");
        assert_eq!(c.counter, 1);
    }

    #[test]
    fn observe_pulls_local_clock_forward() {
        let mut c = Hlc::new(1000, 0, "a");
        // 远端时钟快 5 秒。本地必须被拉前，否则会持续落后、每次冲突都输
        c.observe(&Hlc::new(6000, 3, "b"), 1000);
        assert_eq!(c.wall, 6000);
        assert_eq!(c.counter, 4);
    }

    #[test]
    fn observe_with_physical_ahead_resets_counter() {
        let mut c = Hlc::new(1000, 9, "a");
        c.observe(&Hlc::new(1500, 2, "b"), 5000);
        assert_eq!((c.wall, c.counter), (5000, 0));
    }

    #[test]
    fn observe_with_equal_walls_takes_max_counter_plus_one() {
        let mut c = Hlc::new(1000, 7, "a");
        c.observe(&Hlc::new(1000, 3, "b"), 500);
        assert_eq!((c.wall, c.counter), (1000, 8));
    }

    #[test]
    fn wall_dominates_counter() {
        assert!(Hlc::new(200, 0, "a") > Hlc::new(100, 99, "z"));
    }

    #[test]
    fn ties_are_broken_deterministically_by_device() {
        let a = Hlc::new(100, 0, "aaa");
        let b = Hlc::new(100, 0, "bbb");
        // 两端独立比较必须得出同一个结论，否则会各自认为"我的更新"
        assert_eq!(a.cmp(&b), Ordering::Less);
        assert_eq!(b.cmp(&a), Ordering::Greater);
        assert_eq!(a.cmp(&a), Ordering::Equal);
    }

    #[test]
    fn zero_is_older_than_anything_real() {
        assert!(Hlc::zero() < Hlc::new(0, 0, ""));
        assert!(Hlc::zero().is_zero());
    }
}
