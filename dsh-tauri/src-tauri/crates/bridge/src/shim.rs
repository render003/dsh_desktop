//! 垫片 JS 的嵌入与静态自检。
//!
//! `dist/bridge-shim.js` 是 contracts/bridge-api.md 的页面侧实现（46 方法：44
//! Electron 契约面 + 2 Tauri 原生新增的插件死条目体检/清理），编进二进制后
//! 由 app 层作为 `initialization_script` 注入每个页面。
//! 2026-10 伴随插件裁撤：petWindow 六面 / imagePaste.save / revertFiles 已从
//! 垫片与契约同步移除（码值与方法名不复用）；同月拆 Electron 余额遗留线再移除
//! `refreshBalance` 面与 `balance-changed` → `dsh-balance-changed` 事件转发。

/// 垫片 JS 全文。
pub const BRIDGE_SHIM_JS: &str = include_str!("../dist/bridge-shim.js");

/// 垫片必须覆盖的桥方法/命名空间（与 bridge-api.md §2 对齐的完整性锚点）。
#[cfg(test)]
const REQUIRED_SURFACES: &[&str] = &[
    "appVersion",
    "windowControls.minimize",
    "windowControls.toggleMaximize",
    "windowControls.close",
    "windowControls.isMaximized",
    "windowControls.onMaximizeChange",
    "menu.action",
    "getInfo",
    // 壳层 2026-10-22 起不再发射 notification-jump（通知改由内置插件
    // dsh-notify 在渲染进程发送）；方法位保留以兼容旧页面调用。
    "onNotificationJump",
    "wsl.getConfig",
    "wsl.saveConfig",
    "wsl.recheck",
    "restartService",
    "openPath",
    "openExternal",
    "copyText",
    "getPathForFile",
    "floatWindow.open",
    "floatWindow.close",
    "pluginManager.list",
    "pluginManager.listDeadEntries",
    "pluginManager.removeDeadEntries",
    "pluginManager.setEnabled",
    "pluginManager.uninstall",
    "pluginManager.restore",
    "pluginManager.checkUpdates",
    "pluginManager.update",
    "diagBackup.runDiagnostics",
    "diagBackup.exportBackup",
    "diagBackup.previewRestore",
    "diagBackup.restore",
    "diagBackup.exportDiagnostics",
    "diagBackup.validatePlugins",
    "diagBackup.removeBundle",
    "diagBackup.analyzeOrder",
    "diagBackup.applyOrder",
    "guard.status",
    "guard.check",
    "guard.incident",
    "guard.resolveIncident",
    "recovery.getState",
    "recovery.reload",
    "recovery.restart",
    "recovery.openLogs",
        // 1.0.9 保底进入通道：禁用全部伴随插件后重进（保留内置插件管理页与
        // billion-context gate），壳写 DSH_HOME/safe-mode.flag 后重启 boot 链。
        "recovery.safeMode",
];

/// 垫片源码是否包含某方法名的定义（形如 `name: function` / `name,`）。
#[cfg(test)]
fn defines(surface: &str) -> bool {
    if let Some((ns, method)) = surface.split_once('.') {
        // 命名空间块存在 + 块内方法定义存在（简化为全文出现 `method:`，容差可接受：
        // REQUIRED_SURFACES 是锚点不是解析器）。
        BRIDGE_SHIM_JS.contains(ns)
            && BRIDGE_SHIM_JS.contains(&format!("{method}:"))
    } else {
        BRIDGE_SHIM_JS.contains(&format!("{surface}:")) || BRIDGE_SHIM_JS.contains(&format!("{surface},"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shim_nonempty_and_idempotent_guard() {
        assert!(BRIDGE_SHIM_JS.len() > 4000, "垫片意外地短——疑似被截断");
        assert!(BRIDGE_SHIM_JS.contains("if (window.dshDesktop) return;"), "必须幂等");
        assert!(BRIDGE_SHIM_JS.contains("'use strict'"));
    }

    #[test]
    fn all_surfaces_present() {
        let missing: Vec<&str> = REQUIRED_SURFACES.iter().copied().filter(|s| !defines(s)).collect();
        assert!(missing.is_empty(), "垫片缺失契约方法: {missing:?}");
        assert_eq!(REQUIRED_SURFACES.len(), 45, "契约方法计数（移除 sponsorQr/sponsorWindow 后为 45）");
    }

    #[test]
    fn agent_update_menu_action_retired() {
        for gone in [
            "check-agent-update",
            "check-client-update",
            "install-client-update",
            "toggle-auto-update",
            "sponsorWindow",
            "sponsorQr",
            "请作者喝咖啡",
            "关于 DSH Pack",
            "E_CUT_FEATURE",
        ] {
            assert!(!BRIDGE_SHIM_JS.contains(gone), "已裁撤功能不得残留于垫片: {gone}");
        }
    }

    #[test]
    fn event_names_align_contract() {
        for ev in ["window-maximized", "notification-jump"] {
            assert!(BRIDGE_SHIM_JS.contains(ev), "事件 {ev} 缺失");
        }
        for ev in ["client-update-available", "client-update-progress"] {
            assert!(!BRIDGE_SHIM_JS.contains(ev), "客户端更新事件已裁撤，不得残留: {ev}");
        }
        // 余额遗留线退役反证（contracts/ipc-commands.md §2.4）：垫片既不再监听
        // Tauri 事件 balance-changed，也不再向页面派发 dsh-balance-changed
        // CustomEvent——`balance-changed` 子串同时覆盖两种形态。
        assert!(
            !BRIDGE_SHIM_JS.contains("balance-changed"),
            "余额事件转发已随 Electron 余额线退役，不得残留"
        );
        assert!(
            !BRIDGE_SHIM_JS.contains("refreshBalance"),
            "refreshBalance 面已随 Electron 余额线退役，不得残留"
        );
    }

    #[test]
    fn upstream_channels_align_bridge_crate() {
        // 垫片 invoke 的 command 名必须全部在 bridge::commands 表内（未注册的会运行时失败）。
        let mut invoked: Vec<&str> = Vec::new();
        let mut rest = BRIDGE_SHIM_JS;
        while let Some(pos) = rest.find("call('") {
            rest = &rest[pos + 6..];
            if let Some(end) = rest.find('\'') {
                invoked.push(&rest[..end]);
                rest = &rest[end..];
            } else {
                break;
            }
        }
        // send() 族的四个 fire-and-forget。
        let mut rest = BRIDGE_SHIM_JS;
        while let Some(pos) = rest.find("send('") {
            rest = &rest[pos + 6..];
            if let Some(end) = rest.find('\'') {
                invoked.push(&rest[..end]);
                rest = &rest[end..];
            } else {
                break;
            }
        }
        assert!(!invoked.is_empty(), "未提取到任何 invoke");
        for cmd in invoked {
            assert!(
                crate::commands::CHANNELS.iter().any(|c| c.tauri == cmd && !c.cut),
                "垫片调用的 command {cmd} 不在映射表（或已被裁撤）"
            );
        }
    }

    /// 裁撤锚点（2026-10 伴随插件退场）：宠物窗 / 粘贴图 / 文件还原 / 拖放悬停
    /// 提示层四面已从垫片与契约同步摘除，不得以任何形态复活（方法名、通道名，
    /// 连宠物窗的模式旗标 `__DSH_PET__` 与拖放提示层的 DOM id 都不得残留——
    /// 它们的注入方已不存在，留着分支只会变成永远为假的死判据）。
    #[test]
    fn retired_plugin_surfaces_absent() {
        for gone in [
            "petWindow",
            "pet_state",
            "pet_close",
            "pet_move_to",
            "pet_set_auto_open",
            "__DSH_PET__",
            "imagePaste",
            "image_paste_save",
            "revertFiles",
            "file_revert",
            "client-file-drop",
            "drop_hint",
            "DROP_HINT_ID",
        ] {
            assert!(
                !BRIDGE_SHIM_JS.contains(gone),
                "已裁撤面 {gone} 不得在垫片复活（对应伴随插件已退役）"
            );
        }
    }
}

#[cfg(test)]
mod dialog_polyfill_tests {
    use super::BRIDGE_SHIM_JS;

    /// WebView2 不弹原生 dialog（用户实测 bug 的次因）：垫片必须 polyfill。
    #[test]
    fn native_dialog_polyfill_present() {
        assert!(BRIDGE_SHIM_JS.contains("window.confirm = function () { return true; }"), "confirm 必须放行（删除确认不再恒取消）");
        assert!(BRIDGE_SHIM_JS.contains("window.alert = function (msg)"), "alert 转桥上报（消息不丢）");
        assert!(BRIDGE_SHIM_JS.contains("window.prompt = function () { return null; }"), "prompt 防御性兜底");
        assert!(BRIDGE_SHIM_JS.contains("__dshDialogPolyfilled"), "幂等守卫");
    }
}

#[cfg(test)]
mod window_chrome_tests {
    use super::BRIDGE_SHIM_JS;

    /// 内核页窗口控制条：decorations:false 主窗（Windows）导航到内核 Web UI
    /// 后，页面不认识 data-tauri-drag-region（Electron 用 -webkit-app-region，
    /// WebView2 不支持）→ 不能拖、无窗口按钮（用户实测 bug）。垫片必须注入。
    #[test]
    fn window_chrome_injection_present() {
        assert!(BRIDGE_SHIM_JS.contains("dsh-tauri-chrome"), "控制条特征标记/id 缺失");
        assert!(BRIDGE_SHIM_JS.contains("data-tauri-drag-region"), "拖拽条必须用 Tauri drag-region 机制");
        // 按钮必须走垫片已有的 windowControls 桥方法（window_control 命令）。
        for m in ["windowControls.minimize()", "windowControls.toggleMaximize()", "windowControls.close()"] {
            assert!(BRIDGE_SHIM_JS.contains(m), "按钮缺桥调用 {m}");
        }
        // 最大化/还原图标状态同步。
        assert!(BRIDGE_SHIM_JS.contains("windowControls.isMaximized()"));
        assert!(BRIDGE_SHIM_JS.contains("windowControls.onMaximizeChange"));
        // 内容下推契约：普通流走 padding，fixed 侧边栏（dsh-better-sidebar）读属性。
        assert!(BRIDGE_SHIM_JS.contains("data-dsh-title-bar-height"), "缺 body 下推的属性声明");
        assert!(BRIDGE_SHIM_JS.contains("padding-top:"), "缺 body padding 下推");
    }

    /// 原生标题栏平台门（与 windows.rs decorations 平台门配套，改一侧必须
    /// 同步）：mac/linux 主窗为原生标题栏（mac 红绿灯/全屏钮，linux 防白屏），
    /// 垫片不得注入全宽条（否则双份标题栏 + body 下推破坏布局），降级为 ⋯
    /// 菜单悬浮钮（保住菜单功能面：更新/通知开关/退出等）。
    #[test]
    fn window_chrome_native_title_bar_platform_gate() {
        // 平台判定（UA：Windows UA 不含 Macintosh/Linux 两词，mac/linux 均含其一）。
        assert!(BRIDGE_SHIM_JS.contains("NATIVE_TITLE_BAR"), "缺平台门判定");
        assert!(BRIDGE_SHIM_JS.contains("/(Macintosh|Linux)/.test(navigator.userAgent"), "UA 判定形态漂移");
        // 悬浮钮降级形态存在且为统一入口内分支（浮窗/壳页跳过后先分流）。
        assert!(BRIDGE_SHIM_JS.contains("injectMenuBall"), "缺悬浮钮注入函数");
        assert!(BRIDGE_SHIM_JS.contains("dsh-tauri-menu-ball"), "缺悬浮钮 id");
        assert!(
            BRIDGE_SHIM_JS.contains("if (NATIVE_TITLE_BAR) { injectMenuBall(); return; }"),
            "原生标题栏平台必须在注入条之前分流到悬浮钮"
        );
        // 幂等与自愈重注必须两形态统一防重（悬浮钮被 SPA 摘除后也要能重注）。
        assert!(
            BRIDGE_SHIM_JS.contains("!document.getElementById(BALL_ID)"),
            "幂等/自愈检查必须兼容悬浮钮形态"
        );
        // 点击外关闭必须兼容两种形态（否则悬浮钮形态点钮会误关菜单）。
        assert!(
            BRIDGE_SHIM_JS.contains("document.getElementById(CHROME_ID) || document.getElementById(BALL_ID)"),
            "点击外关闭兼容悬浮钮形态（点钮不得关菜单）"
        );
    }

    /// 控制条只注入内核页：浮窗/壳页各有标题栏，注入会重复遮挡。
    /// （宠物窗分支连同 `__DSH_PET__` 旗标已于 2026-10 随 harness-pet 退役移除，
    /// 其防复活面见上方 retired_plugin_surfaces_absent。）
    #[test]
    fn window_chrome_scoped_to_kernel_page() {
        for marker in ["__DSH_FLOAT__", "loading|recovery|poc"] {
            assert!(BRIDGE_SHIM_JS.contains(marker), "跳过条件缺 {marker}");
        }
        assert!(BRIDGE_SHIM_JS.contains("getElementById(CHROME_ID)"), "幂等检查（先查已存在标记）");
    }

    /// 初始化脚本先于页面脚本（DOM 未建）→ 等 body；内核 SPA 重挂载 → 自愈。
    #[test]
    fn window_chrome_waits_for_body_and_self_heals() {
        assert!(BRIDGE_SHIM_JS.contains("MutationObserver"), "等 body/重挂观察");
        assert!(BRIDGE_SHIM_JS.contains("onBodyReady"), "body 未就绪时不早注入");
        // 全程 try/catch 包裹：注入失败不得影响桥主流程。
        assert!(BRIDGE_SHIM_JS.contains("注入失败不影响页面主流程"));
    }

    /// 双击最大化由 Tauri 内置 drag-region 脚本处理（mousedown detail===2 →
    /// internal_toggle_maximize）；垫片自己再挂 dblclick 监听会双重切换。
    #[test]
    fn window_chrome_no_manual_dblclick_handler() {
        assert!(!BRIDGE_SHIM_JS.contains("'dblclick'"), "双击切换须交给 Tauri 内置脚本，不得自挂监听");
        assert!(!BRIDGE_SHIM_JS.contains("ondblclick"), "同上");
    }

    /// 沉浸式双主题（对齐 Electron 的适配方式）：条颜色全部消费内核
    /// --dsw-alias-* 设计变量（内核按 body[data-ds-dark-theme] 运行时切换，
    /// 变量级联即时跟随）；变量缺失时按 data-dsh-theme 档位兜底（检测
    /// data-ds-dark-theme → 主题 class/属性 → prefers-color-scheme），
    /// 切换有 CSS transition 平滑过渡。
    #[test]
    fn window_chrome_theme_adaptive() {
        for marker in [
            "--dsw-alias-bg-base",     // 内核主题底色变量（像素级跟随）
            "--dsw-alias-label-primary",
            "data-ds-dark-theme",      // 内核暗色标记（检测优先级最高）
            "data-dsh-theme",          // 条上主题档位属性（light/dark 兜底分档）
            "data-dsh-theme=\"light\"", // 浅色档须有白底兜底（内核 light 值）
            "prefers-color-scheme",    // 系统偏好兜底 + matchMedia 监听
            "attributeFilter",         // MutationObserver 观察主题属性变化
            "transition:background-color .25s",
        ] {
            assert!(BRIDGE_SHIM_JS.contains(marker), "主题化缺 {marker}");
        }
    }

    /// 鲸鱼图标：内核 favicon.svg 同源矢量（单 path，viewBox 0 0 50 50，
    /// 首段 M48.8354 是指纹），fill:currentColor 继承标题色 → 随内核主题
    /// 即时反色；替换旧渐变方块。
    #[test]
    fn window_chrome_whale_icon() {
        assert!(BRIDGE_SHIM_JS.contains("M48.8354"), "鲸鱼 path 缺失（内核 favicon 同源）");
        assert!(BRIDGE_SHIM_JS.contains("viewBox: '0 0 50 50'"), "鲸鱼 viewBox 缺失");
        assert!(BRIDGE_SHIM_JS.contains("fill:currentColor"), "鲸鱼须随主题反色");
        assert!(BRIDGE_SHIM_JS.contains(".dch-whale"), "鲸图样式类缺失");
        assert!(!BRIDGE_SHIM_JS.contains("linear-gradient(135deg,#4f7cff"), "渐变方块 logo 应被鲸鱼替换");
    }

    /// 观感对齐 Electron CHROME_CSS：玻璃底（color-mix 半透明 + 模糊饱和）
    /// + 细边框 + 30x28 圆角按钮 + SVG 线性按钮图形（stroke:currentColor）。
    #[test]
    fn window_chrome_electron_visual_alignment() {
        for marker in [
            "color-mix(in srgb,",
            "backdrop-filter:blur(16px) saturate(1.5)",
            "border-radius:8px",
            "place-items:center",
            "stroke:currentColor",
            "letter-spacing:.2px", // Electron .dch-title 同款
        ] {
            assert!(BRIDGE_SHIM_JS.contains(marker), "对齐 Electron 观感缺 {marker}");
        }
        // 最大化/还原图标状态切换（□/❐ 的 SVG 等价物）。
        assert!(BRIDGE_SHIM_JS.contains("data-maximized"), "缺最大化状态图标切换");
    }

    /// ⋯ 下拉菜单（Electron preload renderMenu 复刻）：⋯ 按钮在 min/max/close
    /// 左边；菜单含「检查客户端更新…」（v0.5.3 唯一更新项，双源 GitHub/Gitee
    /// 客户端更新链）/更新源/三开关（两 Electron 键 + toggle-auto-update）/
    /// reload/devtools/fullscreen/open-browser/open-logs/sponsor/about/quit；
    /// **不得再含「检查 dsh 更新」**（npm 内核链已随「内核随客户端分发」退役）；
    /// **不得再含「显示余额/本轮费用」**（toggle-balance，2026-10-16 退役——
    /// 它写壳侧 settings.json，与插件读的内核设置条目是两条互不相通的线，
    /// 开关唯一落点已是 dsh-balance 插件的 Config 设置条目）。
    #[test]
    fn window_chrome_dots_menu_items() {
        for marker in [
            "dch-menu-btn",
            "cx: '2.4'",
            "MENU_ID",
            "menuItemHtml('toggle-close-to-tray'",
            "menuItemHtml('set-custom-icon'",
            "menuItemHtml('reset-custom-icon'",
            "menuItemHtml('reload'",
            "menuItemHtml('devtools'",
            "menuItemHtml('fullscreen'",
            "menuItemHtml('open-browser'",
            "menuItemHtml('open-logs'",
            "menuItemHtml('quit'",
            "' data-act=\"' + act",
            "menu_action",
        ] {
            assert!(BRIDGE_SHIM_JS.contains(marker), "⋯ 菜单缺 {marker}");
        }
        for gone in [
            "检查 dsh 更新",
            "检查客户端更新",
            "更新源（点行内「复制」拷贝地址）",
            "自动安装客户端更新",
            "toggle-auto-update",
            "toggle-balance",
            "显示余额",
            "请作者喝咖啡",
            "关于 DSH Pack",
            "menuItemHtml('sponsor'",
            "menuItemHtml('about'",
        ] {
            assert!(!BRIDGE_SHIM_JS.contains(gone), "已裁撤菜单项不得回潮: {gone}");
        }
    }

    #[test]
    fn window_chrome_dots_menu_interaction() {
        for marker in [
            "'Escape'",
            "bar.contains(e.target)",
            "installMenuHooks",
            "menuHooksInstalled",
            "renderMenu()",
            "plugin:notification|notify",
        ] {
            assert!(BRIDGE_SHIM_JS.contains(marker), "菜单交互缺 {marker}");
        }
        let toggle_seg = BRIDGE_SHIM_JS
            .split("if (act === 'toggle-close-to-tray'")
            .nth(1)
            .and_then(|s| s.split("if (act === 'set-custom-icon'").next())
            .expect("toggle 分支");
        assert!(!toggle_seg.contains("closeMenu()"), "toggle 分支应留在菜单重渲染: {toggle_seg}");
    }

    /// 自愈重注防累积（内存泄漏回归锚点）：内核 SPA 反复摘除 body 直接
    /// 子元素会触发控制条重注，两个累积面必须有守卫——
    ///   a) <head> 里的两份 <style data-for=…> 只注入一次（重注只补条本体）；
    ///   b) 每次重注订阅的 onMaximizeChange(setMaxGlyph) 闭包持有旧条
    ///      maxBtn（已摘除的游离 DOM），旧订阅必须先退订（listeners.maximize
    ///      不得随重注次数线性增长）。
    #[test]
    fn window_chrome_self_heal_does_not_accumulate() {
        let inject_seg = BRIDGE_SHIM_JS
            .split("function injectChromeBar()")
            .nth(1)
            .and_then(|s| s.split("function onBodyReady").next())
            .expect("injectChromeBar 函数段");
        // a) 样式查重：两份样式都必须先 querySelector 再决定 append。
        assert!(
            inject_seg.contains("style[data-for=\"' + CHROME_ID + '\"]"),
            "重注不得重复注入主样式 <style>"
        );
        assert!(
            inject_seg.contains("style[data-for=\"' + CHROME_ID + '-layout\"]"),
            "重注不得重复注入布局样式 <style>"
        );
        // b) maximize 订阅先退旧再挂新。
        assert!(BRIDGE_SHIM_JS.contains("maxGlyphUnsub"), "缺 maximize 订阅退订器");
        assert!(
            inject_seg.contains("maxGlyphUnsub()"),
            "重注前必须退订旧条的 maximize 订阅"
        );
        assert!(
            inject_seg.contains("maxGlyphUnsub = dshDesktop.windowControls.onMaximizeChange(setMaxGlyph)"),
            "新订阅的退订器必须落回 maxGlyphUnsub"
        );
    }
}

/// 事件信封解包回归锚点（tauri-2.11.5 emit_js_script：回调收 {event, payload}）：
/// onEvent 必须先解包 payload 再交 map/消费者——旧代码裸读导致 notification-jump/
/// 更新进度全部字段 undefined（事件链静默失效）。
#[cfg(test)]
mod event_envelope_tests {
    use super::BRIDGE_SHIM_JS;

    #[test]
    fn onevent_unwraps_envelope_payload() {
        let seg = BRIDGE_SHIM_JS
            .split("function onEvent(name, queue, map)")
            .nth(1)
            .and_then(|s| s.split("onEvent('window-maximized'").next())
            .expect("onEvent 函数段");
        assert!(
            seg.contains("ev.payload !== undefined ? ev.payload : ev"),
            "onEvent 必须解包信封（双形态回退）: {seg}"
        );
        // map 收到的必须是解包后的 payload（不得把信封传给 map）。
        assert!(
            seg.contains("map ? map(payload) : payload"),
            "map 消费的是解包后的 payload"
        );
    }
}

/// 外链点击委托（K15 余额充值 / #149 / #162 外链不跳转）回归锚点：
/// <a target="_blank"> http(s) 外链被 WebView2 导航围栏拦掉（on_navigation 只放行
/// 127.0.0.1 / tauri://），垫片必须全局捕获点击、preventDefault 原生导航并改走
/// openExternal（系统默认浏览器）；且不得劫持内核同源 127.0.0.1 内链。
#[cfg(test)]
mod external_link_delegation_tests {
    use super::BRIDGE_SHIM_JS;

    #[test]
    fn delegation_block_present() {
        for marker in [
            "installExternalLinkDelegation",      // 委托安装函数
            "target.toLowerCase() !== '_blank'", // 只拦 _blank
            "openExternal(href)",                 // 走 open_external 桥
            "e.preventDefault()",                 // 阻止原生导航
            "indexOf('http://127.0.0.1')",      // 内核同源放行
            "slice(0, 7) !== 'http://'",        // 只拦 http(s)
            "installExternalLinkDelegation();",   // 自初始化调用
        ] {
            assert!(BRIDGE_SHIM_JS.contains(marker), "外链点击委托缺 {marker}");
        }
    }
}
