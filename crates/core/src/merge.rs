//! 同步合并决策。
//!
//! 这是"谁赢"的唯一裁定处。桌面端拉取时用它，推送被服务端拒绝时也用它 ——
//! 同一套规则必须同时覆盖这两条路径，否则会出现"拉取时留了副本、
//! 推送时却静默丢了"这种不对称行为。
//!
//! ## 一条不可动摇的原则
//!
//! **绝不静默丢弃用户写下的内容。**
//!
//! 逐字段 LWW 的代价是并发编辑必然有一方落败。在一个"个人记忆"工具里，
//! 少一条笔记比多一条重复内容严重得多 —— 重复内容用户一眼能看出并删掉，
//! 而消失的笔记用户根本不知道自己丢过什么。
//!
//! 所以当远端胜出、而本地存在**未同步且内容不同**的版本时，强制保留副本。

use crate::hlc::Hlc;

/// 判定冲突副本时要打的标签名。
///
/// 用标签而不是特殊表：用户可以直接用现成的标签筛选功能找到所有冲突副本，
/// 不需要为它单做一个界面。
pub const CONFLICT_TAG: &str = "冲突副本";

/// 本地某一行的状态。
pub struct LocalState<'a> {
    pub hlc: &'a Hlc,
    /// 本地有尚未被服务端确认的修改
    pub dirty: bool,
    /// 本地内容与远端内容是否不同。
    /// 由调用方按实体各自的字段判定（消息比 body，频道比 name……），
    /// 内核不关心具体是哪个字段。
    pub differs: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    /// 直接采用远端版本
    ApplyRemote,
    /// 保持本地不动（远端更旧，或本就是同一个变更）
    KeepLocal,
    /// 远端胜出，但必须先保留本地副本再覆盖
    ApplyRemoteKeepCopy,
}

/// 裁定远端版本与本地版本的关系。
pub fn resolve(remote: &Hlc, local: Option<LocalState<'_>>) -> Resolution {
    let Some(local) = local else {
        // 本地没有这一行，直接落地
        return Resolution::ApplyRemote;
    };

    match remote.cmp(local.hlc) {
        // 远端在因果序上更新
        std::cmp::Ordering::Greater => {
            if local.dirty && local.differs {
                // 本地那一版从未被服务端见过，且内容和胜出者不同 —— 不能丢
                Resolution::ApplyRemoteKeepCopy
            } else {
                Resolution::ApplyRemote
            }
        }
        // 同一个变更被重复投递（重试、重复拉取）。
        // 必须在这里返回 KeepLocal 而不是重新应用：同步的幂等性全靠这一条。
        std::cmp::Ordering::Equal => Resolution::KeepLocal,
        // 本地更新，稍后会被推上去
        std::cmp::Ordering::Less => Resolution::KeepLocal,
    }
}

/// 生成冲突副本的正文。
///
/// 刻意带上醒目的说明：如果只是原样复制一份，用户会以为是自己重复写了，
/// 反而不知道该删哪一条。
pub fn conflict_body(original: &str) -> String {
    format!("⚠️ 同步冲突副本（本地这版编辑未能自动合并，下面保留原始内容）\n\n{original}")
}

/// 服务端的裁定规则：`incoming` 是否应当覆盖已存的 `stored`。
///
/// 刻意和客户端的 [`resolve`] 放在一起。服务端和客户端必须对"谁更新"
/// 得出完全一致的结论 —— 如果服务端用一套规则、客户端用另一套，
/// 两边会各自认为自己的版本胜出，最终**收敛不到同一个状态**，
/// 而且不会报任何错。
pub fn should_accept_push(incoming: &Hlc, stored: Option<&Hlc>) -> bool {
    match stored {
        None => true,
        // 严格大于才接受：相等的 HLC 是同一个变更被重复推送，
        // 此时必须返回"未接受 + 当前版本"，让客户端走幂等路径。
        Some(s) => incoming > s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hlc::Hlc;

    fn local<'a>(hlc: &'a Hlc, dirty: bool, differs: bool) -> Option<LocalState<'a>> {
        Some(LocalState {
            hlc,
            dirty,
            differs,
        })
    }

    #[test]
    fn missing_local_row_applies_remote() {
        let remote = Hlc::new(100, 0, "a");
        assert_eq!(resolve(&remote, None), Resolution::ApplyRemote);
    }

    #[test]
    fn duplicate_delivery_is_idempotent() {
        // 这是整个同步正确性的基石：同一条变更重复到达必须是空操作
        let h = Hlc::new(100, 0, "a");
        assert_eq!(
            resolve(&h, local(&h, false, false)),
            Resolution::KeepLocal,
            "HLC 相等说明是同一个变更，重复应用会破坏幂等"
        );
        assert_eq!(resolve(&h, local(&h, true, true)), Resolution::KeepLocal);
    }

    #[test]
    fn remote_newer_with_clean_local_just_applies() {
        let remote = Hlc::new(200, 0, "a");
        let mine = Hlc::new(100, 0, "b");
        assert_eq!(
            resolve(&remote, local(&mine, false, true)),
            Resolution::ApplyRemote,
            "本地没有未同步改动，没有东西会丢"
        );
    }

    #[test]
    fn remote_newer_over_dirty_local_keeps_a_copy() {
        let remote = Hlc::new(200, 0, "a");
        let mine = Hlc::new(100, 0, "b");
        assert_eq!(
            resolve(&remote, local(&mine, true, true)),
            Resolution::ApplyRemoteKeepCopy,
            "本地这一版服务端没见过，覆盖前必须留副本"
        );
    }

    #[test]
    fn dirty_local_with_identical_content_needs_no_copy() {
        let remote = Hlc::new(200, 0, "a");
        let mine = Hlc::new(100, 0, "b");
        assert_eq!(
            resolve(&remote, local(&mine, true, false)),
            Resolution::ApplyRemote,
            "内容一样，留副本只会制造无意义的重复条目"
        );
    }

    #[test]
    fn local_newer_keeps_local() {
        let remote = Hlc::new(100, 0, "a");
        let mine = Hlc::new(200, 0, "b");
        assert_eq!(resolve(&remote, local(&mine, true, true)), Resolution::KeepLocal);
    }

    #[test]
    fn conflict_body_marks_the_copy_visibly() {
        let body = conflict_body("原本的内容");
        assert!(body.contains("冲突副本"));
        assert!(body.contains("原本的内容"));
    }

    #[test]
    fn push_is_accepted_only_when_strictly_newer() {
        let older = Hlc::new(100, 0, "a");
        let newer = Hlc::new(200, 0, "a");

        assert!(should_accept_push(&newer, None), "服务端没有这一行时必须接受");
        assert!(should_accept_push(&newer, Some(&older)));
        assert!(
            !should_accept_push(&older, Some(&newer)),
            "更旧的不该覆盖更新的"
        );
        assert!(
            !should_accept_push(&newer, Some(&newer)),
            "相等的 HLC 是重复推送，必须拒绝以保持幂等"
        );
    }
}
