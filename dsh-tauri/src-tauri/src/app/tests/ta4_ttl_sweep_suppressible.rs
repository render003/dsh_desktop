//! TA4 回归锁定（形态级）——lib.rs `suppressible` 窗口×URL 联合矩阵形态面。
//!
//! （原「balance turn-end 30s 节流 × 四路交互」随 Electron 余额遗留线拆除；
//! 原「updater TTL 清扫边界」随客户端自更新链裁撤移除，2026-11-22。）

const LIB_RS: &str = include_str!("../src/lib.rs");

// ---------------------------------------------------------------------------
// X1b/RV3 P1-2：渲染抑制 suppressible（窗口 × URL 漂移）联合矩阵
// ---------------------------------------------------------------------------

/// 联合判定形态：suppressible = 窗口内 && URL 未变。矩阵语义：
/// | since_ms      | url_changed | suppressible |
/// | 0 / 89_999    | false       | true（抑制，探针） |
/// | 0 / 89_999    | true        | false（漂移即换页） |
/// | >= 90_000     | 任意        | false（过窗换页） |
/// lib.rs 内已有窗口表单测（should_suppress_kernel_nav），本测试锁联合表达式
/// 与 URL 锚点更新（真实换页必须记录 LAST_KERNEL_NAV_URL）。
#[test]
fn suppressible_joint_predicate_shape() {
    let seg = LIB_RS
        .split("fn suppressible(")
        .nth(1)
        .and_then(|s| s.split("fn kernel_ready_navigate").next())
        .expect("suppressible 函数体");
    assert!(
        seg.contains("should_suppress_kernel_nav(since_last_nav_ms) && !url_changed"),
        "联合判定必须是 窗口内 && URL未变（矩阵中枢）: {seg}"
    );
    // 窗口常量 90s（89_999 抑制 / 90_000 放行边界由常量 + `<` 决定）。
    assert!(LIB_RS.contains("KERNEL_NAV_SUPPRESS_WINDOW_MS: u64 = 90_000"), "90s 窗口常量");
    assert!(LIB_RS.contains("since_last_nav_ms < KERNEL_NAV_SUPPRESS_WINDOW_MS"), "窗口判定严格 <（恰 90s 放行）");
    // URL 漂移检测：与上次真实换页的 URL 比较（Some 且不同 → changed）。
    let nav = LIB_RS
        .split("fn kernel_ready_navigate")
        .nth(1)
        .and_then(|s| s.split("fn route_one_event").next())
        .expect("kernel_ready_navigate 函数体");
    assert!(
        nav.contains("guard.as_deref().is_some_and(|prev| prev != url)"),
        "URL 变更判定 = 与 LAST_KERNEL_NAV_URL 锚点不同"
    );
    assert!(nav.contains("LAST_KERNEL_NAV_MS.store"), "真实换页更新时刻锚点");
    assert!(nav.contains("*LAST_KERNEL_NAV_URL.lock().unwrap_or_else(|p| p.into_inner()) = Some(url.clone())"), "真实换页更新 URL 锚点（漂移检测基准）");
}
