//! ⋯ 菜单动作分发（`menu_action`，bridge-api.md §2.3 的 act 枚举）+
//! settings 单键开关 helper。
//!
//! 历史裁撤：
//! - `check-agent-update`（npm 内核 latest 比对链）于 v0.5.3 退役，对应错误码
//!   E_AGENT_UPDATE_NETWORK 在 contracts/error-codes.md 标「已退役」（码值不复用）；
//! - 客户端自更新链（`check-client-update` / `install-client-update` /
//!   `toggle-auto-update`）及 `about` 菜单动作于 2026-11-22 在 `custom` 分支整体裁撤。

use bridge::BridgeError;
use tauri::{AppHandle, Manager};

use crate::AppState;

use super::common::{open_http_url, open_in_explorer, terr};

#[tauri::command]
pub async fn menu_action(action: String, payload: Option<serde_json::Value>, app: AppHandle) -> Result<serde_json::Value, BridgeError> {
    match action.as_str() {
        "open-logs" => {
            let dir = shell_core::DshPaths::resolve().logs;
            let _ = std::fs::create_dir_all(&dir);
            open_in_explorer(&dir)
        }
        "open-browser" => {
            let url = payload
                .and_then(|p| p.get("url").and_then(|v| v.as_str()).map(String::from))
                .unwrap_or_else(|| {
                    app.state::<AppState>()
                        .supervisor
                        .lock().unwrap_or_else(|p| p.into_inner())
                        .clone()
                        .and_then(|s| s.kernel_url())
                        .unwrap_or_else(|| "http://127.0.0.1".into())
                });
            open_http_url(&url)
        }
        "reload" => {
            // Electron reloadMainWindow 语义：当前页软重载（内核 SPA 状态丢失可接受）。
            let win = main_window(&app)?;
            win.eval("try{location.reload()}catch(e){}").map_err(terr)?;
            Ok(serde_json::Value::Null)
        }
        "devtools" => {
            // open_devtools 仅 debug 构建可用（release 无 devtools feature）。
            #[cfg(debug_assertions)]
            {
                let win = main_window(&app)?;
                win.open_devtools();
                Ok(serde_json::json!({ "ok": true }))
            }
            #[cfg(not(debug_assertions))]
            {
                Ok(serde_json::json!({ "ok": false, "error": "开发者工具仅开发版可用" }))
            }
        }
        "fullscreen" => {
            let win = main_window(&app)?;
            let now = win.is_fullscreen().map_err(terr)?;
            win.set_fullscreen(!now).map_err(terr)?;
            Ok(serde_json::json!({ "fullscreen": !now }))
        }
        "quit" => {
            // 托盘「退出」同语义（lib.rs setup_tray）：先同步杀内核树（shutdown，
            // Job Object），再 exit(0)——RunEvent::Exit 再做锁与收尾。
            if let Some(state) = app.try_state::<AppState>() {
                if let Some(sv) = state.supervisor.lock().unwrap_or_else(|p| p.into_inner()).clone() {
                    sv.shutdown();
                }
            }
            app.exit(0);
            Ok(serde_json::Value::Null)
        }
        "toggle-close-to-tray" => {
            let key = toggle_key(&action).ok_or_else(|| BridgeError::invalid_arg(format!("未知菜单动作：{action}")))?;
            let state = app.state::<AppState>();
            let store = shell_core::SettingsStore::new(state.paths.settings.clone());
            let next = toggle_setting(&store, key).map_err(|e| BridgeError::internal(e.0))?;
            // 单键返回（垫片 merge 进菜单 state 后重渲染）。
            let mut out = serde_json::Map::new();
            out.insert(key.to_string(), serde_json::json!(next));
            Ok(serde_json::Value::Object(out))
        }
        // 自定义桌面客户端图标（⋯ 菜单「自定义图标…」/「恢复默认图标」）：
        // 菜单动作（非独立 bridge 通道），壳侧实现见 super::icon。payload 由
        // 垫片按 <input type=file> 读 bytes 后以 base64 data URL 传入。
        "set-custom-icon" => {
            let payload = payload
                .as_ref()
                .ok_or_else(|| BridgeError::invalid_arg("缺自定义图标数据"))?;
            let format = super::icon::set_custom_icon(&app, payload)?;
            Ok(serde_json::json!({ "ok": true, "format": format }))
        }
        "reset-custom-icon" => {
            super::icon::reset_custom_icon(&app)?;
            Ok(serde_json::json!({ "ok": true }))
        }
        other => Err(BridgeError::invalid_arg(format!("未知菜单动作：{other}"))),
    }
}

/// 主窗句柄（⋯ 菜单动作多数作用于主窗）。
pub(super) fn main_window(app: &AppHandle) -> Result<tauri::WebviewWindow, BridgeError> {
    app.get_webview_window("main").ok_or_else(|| BridgeError::not_found("主窗不存在"))
}

/// 菜单 toggle 动作 → settings.json 键（closeToTray 与 Electron updater
/// loadSettings 同键）。
///
/// 返回 Option：未知动作宁可显式报错，也不静默落到某个键上写坏配置。
/// toggle-notify 已随壳层回合完成通知下线移除（2026-10-22）；
/// toggle-balance 已退役（2026-10-16）；
/// toggle-auto-update 已随客户端自更新链裁撤移除（2026-11-22）。
fn toggle_key(action: &str) -> Option<&'static str> {
    match action {
        "toggle-close-to-tray" => Some("closeToTray"),
        _ => None,
    }
}

/// 读 settings.json 布尔键（显式缺省）。损坏/缺失回落 default。
pub(super) fn setting_bool_or(store: &shell_core::SettingsStore, key: &str, default: bool) -> bool {
    store.get(key).ok().flatten().and_then(|v| v.as_bool()).unwrap_or(default)
}

/// 读 settings.json 布尔键（Electron `s.x !== false` 缺省 true 同口径）。
pub(super) fn setting_bool(store: &shell_core::SettingsStore, key: &str) -> bool {
    setting_bool_or(store, key, true)
}

/// 读-改-写布尔开关（Electron toggle-* 语义）：取反写回，返回新值。
fn toggle_setting(store: &shell_core::SettingsStore, key: &str) -> Result<bool, shell_core::settings::SettingsError> {
    let next = !setting_bool(store, key);
    store.set(key, serde_json::json!(next))?;
    Ok(next)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ⋯ 菜单 toggle：读-改-写 settings 往返（缺省 true → false → true），
    /// 读-改-写不破坏同文件其他键，损坏形态（非布尔值）回落缺省。
    #[test]
    fn menu_toggle_setting_roundtrip() {
        let mut path = std::env::temp_dir();
        path.push(format!("dsh-cmd-toggle-{}-{}.json", std::process::id(), line!()));
        let _ = std::fs::remove_file(&path);
        let store = shell_core::SettingsStore::new(&path);
        // 缺省 true（Electron `s.x !== false` 同口径）。
        assert!(setting_bool(&store, "closeToTray"));
        // 翻转并持久化：true → false。
        assert!(!toggle_setting(&store, "closeToTray").unwrap());
        assert_eq!(store.get("closeToTray").unwrap(), Some(serde_json::json!(false)));
        // 显式 false 再翻：false → true（读文件真值，非内存态）。
        assert!(toggle_setting(&store, "closeToTray").unwrap());
        assert_eq!(store.get("closeToTray").unwrap(), Some(serde_json::json!(true)));
        // 读-改-写不破坏同文件其他键。
        store.set("lastWebPort", serde_json::json!(51731)).unwrap();
        toggle_setting(&store, "closeToTray").unwrap();
        assert_eq!(store.get("lastWebPort").unwrap(), Some(serde_json::json!(51731)));
        // 非布尔值（损坏形态）回落缺省 true，toggle 后写回正常布尔。
        store.set("closeToTray", serde_json::json!("oops")).unwrap();
        assert!(setting_bool(&store, "closeToTray"));
        assert!(!toggle_setting(&store, "closeToTray").unwrap());
        let _ = std::fs::remove_file(&path);
    }

    /// 菜单 toggle 动作 → settings.json 键映射。
    /// 已退役动作（toggle-notify / toggle-balance / toggle-auto-update）→ None：
    /// 不得静默落到别的键。
    #[test]
    fn menu_toggle_key_mapping() {
        assert_eq!(toggle_key("toggle-notify"), None);
        assert_eq!(toggle_key("toggle-close-to-tray"), Some("closeToTray"));
        assert_eq!(toggle_key("toggle-auto-update"), None);
        assert_eq!(toggle_key("toggle-balance"), None);
        assert_eq!(toggle_key("no-such-action"), None);
    }

    /// 菜单 quit 语义 = 托盘退出（lib.rs setup_tray 同款）：先 supervisor
    /// .shutdown（同步杀树）再 exit(0)。源码形态断言（WebviewWindow/AppHandle
    /// 无法在单测构造），防「顺手改成直接 exit」回退——那会留内核孤儿进程。
    #[test]
    fn menu_quit_shutdown_before_exit_shape() {
        let src = include_str!("menu.rs");
        let seg = src
            .split("\"quit\" =>")
            .nth(1)
            .and_then(|s| s.split("\"toggle-close-to-tray\"").next())
            .expect("quit 分支");
        let sh = seg.find("sv.shutdown()").expect("必须先同步杀内核树");
        let ex = seg.find("app.exit(0)").expect("必须退出进程");
        assert!(sh < ex, "先 shutdown 后 exit（Job Object 杀树语义）: {seg}");
    }

    /// 退役菜单动作与更新链锚点：不得回潮。
    #[test]
    fn retired_menu_actions_and_update_chains_absent() {
        let src = include_str!("menu.rs");
        let code = src
            .split("#[tauri::command]")
            .nth(1)
            .and_then(|s| s.split("#[cfg(test)]").next())
            .expect("实现段（menu_action 起、测试前止）");
        for gone in [
            "\"check-agent-update\" =>",
            "\"check-client-update\" =>",
            "\"install-client-update\" =>",
            "\"toggle-auto-update\" =>",
            "\"about\" =>",
            "updater_client",
            "npm_latest_version",
            "http_get_version",
            "extract_json_version",
            "fn compare_versions",
            "E_AGENT_UPDATE_NETWORK",
        ] {
            assert!(!code.contains(gone), "退役链残留在 menu.rs: {gone}");
        }
    }
}
