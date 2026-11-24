//! ⋯ 菜单动作分发（`menu_action`，bridge-api.md §2.3 的 act 枚举）+
//! settings 单键开关 helper。
//!
//! 历史裁撤：
//! - `check-agent-update`（npm 内核 latest 比对链）于 v0.5.3 退役，对应错误码
//!   E_AGENT_UPDATE_NETWORK 在 contracts/error-codes.md 标「已退役」（码值不复用）；
//! - 客户端自更新链（`check-client-update` / `install-client-update` /
//!   `toggle-auto-update`）及 `about` 菜单动作于 2026-11-22 在 `custom` 分支整体裁撤。

use bridge::BridgeError;
use tauri::{AppHandle, Emitter, Manager};

use crate::AppState;

use super::common::{navigate_main, open_http_url, open_in_explorer, terr, NoWindow};

/// 防止并发触发多次 agent 安装/回退。
static AGENT_MUTATE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
        "restart-kernel" => {
            let state = app.state::<AppState>();
            let sv = state
                .supervisor
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone()
                .ok_or_else(|| BridgeError::internal("supervisor 未初始化"))?;
            sv.refresh_active_agent();
            restart_agent_kernel(&app, &sv);
            Ok(serde_json::json!({ "ok": true, "restarted": true }))
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
        "agent-check-update" => {
            let app_c = app.clone();
            let raw = tauri::async_runtime::spawn_blocking(move || run_agent_check(&app_c))
                .await
                .map_err(|e| BridgeError::internal(format!("agent 检查更新线程异常：{e}")))??;
            Ok(serde_json::json!({
                "ok": raw.get("ok").and_then(|v| v.as_bool()).unwrap_or(false),
                "currentVersion": raw.get("currentVersion").and_then(|v| v.as_str()).unwrap_or("unknown"),
                "latestVersion": raw.get("latestVersion").and_then(|v| v.as_str()).unwrap_or(""),
                "hasUpdate": raw.get("hasUpdate").and_then(|v| v.as_bool()).unwrap_or(false),
                "agentSource": raw.get("agentSource").and_then(|v| v.as_str()).unwrap_or("bundled"),
                "registry": raw.get("registry").and_then(|v| v.as_str()).unwrap_or(""),
                "error": raw.get("error").cloned().unwrap_or(serde_json::Value::Null),
            }))
        }
        "agent-install-update" => {
            let req_ver = payload
                .as_ref()
                .and_then(|p| p.get("version").and_then(|v| v.as_str()))
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
            let req_reg = payload
                .as_ref()
                .and_then(|p| p.get("registry").and_then(|v| v.as_str()))
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
            let app_c = app.clone();
            let res = tauri::async_runtime::spawn_blocking(move || {
                run_agent_install(&app_c, req_ver, req_reg)
            })
            .await
            .map_err(|e| BridgeError::internal(format!("agent 安装更新线程异常：{e}")))??;
            Ok(serde_json::json!({
                "ok": res.ok,
                "version": res.version,
                "agentSource": res.agent_source,
                "restarted": res.restarted,
                "patchWarning": res.patch_warning,
                "error": res.error,
            }))
        }
        "agent-reset-bundled" => {
            let app_c = app.clone();
            let res = tauri::async_runtime::spawn_blocking(move || run_agent_reset_bundled(&app_c))
                .await
                .map_err(|e| BridgeError::internal(format!("agent 恢复内置线程异常：{e}")))??;
            Ok(serde_json::json!({
                "ok": res.ok,
                "version": res.version,
                "agentSource": res.agent_source,
                "restarted": res.restarted,
            }))
        }
        other => Err(BridgeError::invalid_arg(format!("未知菜单动作：{other}"))),
    }
}

struct AgentMutateResult {
    ok: bool,
    version: String,
    agent_source: String,
    restarted: bool,
    patch_warning: Option<String>,
    error: Option<String>,
}

const AGENT_PROGRESS_PREFIX: &str = "[agent-progress] ";

/// 从 sidecar stderr 行提取结构化进度载荷（纯函数，便于单测）。
fn parse_agent_progress_line(line: &str) -> Option<serde_json::Value> {
    let raw = line.trim().strip_prefix(AGENT_PROGRESS_PREFIX)?;
    let val: serde_json::Value = serde_json::from_str(raw).ok()?;
    val.is_object().then_some(val)
}

fn emit_agent_progress(app: &AppHandle, stage: &str, message: &str) {
    let _ = app.emit(
        "agent-update-progress",
        serde_json::json!({ "stage": stage, "message": message }),
    );
}

/// 经 sidecar 执行 agent 更新类子命令（透传 WinInet 系统代理 + NODE_USE_ENV_PROXY=1），
/// 若传入 `on_progress` 则实时流式消费 stderr 中的 `[agent-progress]` 行推送阶段进度。
fn run_agent_sidecar_with_progress<F>(
    sv: &crate::supervisor::Supervisor,
    args: &[String],
    mut on_progress: Option<F>,
) -> Result<serde_json::Value, BridgeError>
where
    F: FnMut(serde_json::Value),
{
    use std::io::{BufRead, Read};
    let mut cmd = kernel_process::sanitized_node_command(&sv.node_exe);
    cmd.arg(&sv.sidecar_cli)
        .args(args)
        .arg("--app-dir")
        .arg(&sv.app_dir)
        .env("DSH_TAURI_VERSION", env!("CARGO_PKG_VERSION"))
        .env("NODE_USE_ENV_PROXY", "1");
    for (k, v) in crate::supervisor::system_proxy_env_injection() {
        cmd.env(k, v);
    }
    let mut child = cmd
        .creation_flags_no_window()
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(BridgeError::from)?;
    let mut stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();
    let stdout_handle = std::thread::spawn(move || {
        let mut buf = String::new();
        if let Some(ref mut out) = stdout_pipe {
            let _ = out.read_to_string(&mut buf);
        }
        buf
    });
    let mut last_err_line = String::new();
    if let Some(err_pipe) = stderr_pipe {
        for line in std::io::BufReader::new(err_pipe).lines().map_while(Result::ok) {
            if let Some(payload) = parse_agent_progress_line(&line) {
                if let Some(ref mut cb) = on_progress {
                    cb(payload);
                }
            } else if !line.trim().is_empty() {
                last_err_line = line;
            }
        }
    }
    let _ = child.wait();
    let stdout = stdout_handle.join().unwrap_or_default();
    let line = stdout.trim_end().lines().last().unwrap_or("");
    serde_json::from_str(line).map_err(|e| {
        BridgeError::internal(format!("agent 更新 sidecar 输出解析失败: {e} ({last_err_line})"))
    })
}

fn run_agent_sidecar(sv: &crate::supervisor::Supervisor, args: &[String]) -> Result<serde_json::Value, BridgeError> {
    run_agent_sidecar_with_progress::<fn(serde_json::Value)>(sv, args, None)
}

fn run_agent_check(app: &AppHandle) -> Result<serde_json::Value, BridgeError> {
    let state = app.state::<AppState>();
    let sv = state
        .supervisor
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone()
        .ok_or_else(|| BridgeError::internal("supervisor 未初始化"))?;
    let active = sv.active_agent();
    let args = vec![
        "agent-check-update".to_string(),
        "--current".to_string(),
        active.version,
        "--source".to_string(),
        active.source,
    ];
    run_agent_sidecar(&sv, &args)
}

/// 原子切换 `<user_data_dir>/agent` 为已校验的暂存目录（失败时自动回滚原目录）。
fn swap_agent_overlay(user_data_dir: &std::path::Path, staging_dir: &std::path::Path) -> Result<(), String> {
    let name = staging_dir
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    if !name.starts_with("agent-staging-") || staging_dir.parent() != Some(user_data_dir) {
        return Err("非法暂存目录路径".into());
    }
    if !staging_dir.exists() {
        return Err("暂存目录不存在".into());
    }
    let target = user_data_dir.join("agent");
    let prev = user_data_dir.join("agent-prev");
    if prev.exists() {
        let _ = std::fs::remove_dir_all(&prev);
    }
    let had_prev = if target.exists() {
        std::fs::rename(&target, &prev).map_err(|e| format!("备份旧 agent 覆盖层失败：{e}"))?;
        true
    } else {
        false
    };
    if let Err(e) = std::fs::rename(staging_dir, &target) {
        if had_prev {
            let _ = std::fs::rename(&prev, &target);
        }
        return Err(format!("激活新 agent 覆盖层失败：{e}"));
    }
    if had_prev {
        let _ = std::fs::remove_dir_all(&prev);
    }
    Ok(())
}

/// 刷新现役内核并重启 supervisor（150ms 后切主窗回 loading 页，保证当前 invoke 响应先送达）。
fn restart_agent_kernel(app: &AppHandle, sv: &std::sync::Arc<crate::supervisor::Supervisor>) {
    let state = app.state::<AppState>();
    let preferred = state.last_port.load(std::sync::atomic::Ordering::Relaxed);
    let tx = state
        .supervisor_tx
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone();
    match tx {
        Some(tx) => sv.restart(tx, u16::try_from(preferred).ok()),
        None => {
            let (tx, _rx) = std::sync::mpsc::channel();
            sv.restart(tx, u16::try_from(preferred).ok());
        }
    }
    let loading = state.loading_url.lock().unwrap_or_else(|p| p.into_inner()).clone();
    let app_nav = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(150));
        if !loading.is_empty() {
            let _ = navigate_main(&app_nav, &loading);
        }
    });
}

fn run_agent_install(
    app: &AppHandle,
    req_ver: Option<String>,
    req_reg: Option<String>,
) -> Result<AgentMutateResult, BridgeError> {
    let Ok(_guard) = AGENT_MUTATE_LOCK.try_lock() else {
        return Err(BridgeError::invalid_arg("已有 agent 更新任务正在进行中"));
    };
    let state = app.state::<AppState>();
    let sv = state
        .supervisor
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone()
        .ok_or_else(|| BridgeError::internal("supervisor 未初始化"))?;
    let before = sv.active_agent();

    let (target_ver, target_reg) = match req_ver {
        Some(v) => (v, req_reg),
        None => {
            emit_agent_progress(app, "checking", "正在查询最新稳定版…");
            let check = run_agent_check(app)?;
            if !check.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
                let err = check
                    .get("error")
                    .and_then(|v| v.as_str())
                    .unwrap_or("检查最新稳定版失败")
                    .to_string();
                return Ok(AgentMutateResult {
                    ok: false,
                    version: before.version,
                    agent_source: before.source,
                    restarted: false,
                    patch_warning: None,
                    error: Some(err),
                });
            }
            let latest = check
                .get("latestVersion")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let reg = req_reg.or_else(|| {
                check
                    .get("registry")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(String::from)
            });
            (latest, reg)
        }
    };

    if target_ver.is_empty() {
        return Ok(AgentMutateResult {
            ok: false,
            version: before.version,
            agent_source: before.source,
            restarted: false,
            patch_warning: None,
            error: Some("未获取到有效目标版本号".into()),
        });
    }

    let mut stage_args = vec![
        "agent-stage-update".to_string(),
        "--version".to_string(),
        target_ver,
    ];
    if let Some(reg) = target_reg {
        stage_args.push("--registry".to_string());
        stage_args.push(reg);
    }
    let app_ev = app.clone();
    let staged = run_agent_sidecar_with_progress(
        &sv,
        &stage_args,
        Some(move |payload: serde_json::Value| {
            let _ = app_ev.emit("agent-update-progress", payload);
        }),
    )?;
    if !staged.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
        let err = staged
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("下载或安装失败")
            .to_string();
        return Ok(AgentMutateResult {
            ok: false,
            version: before.version,
            agent_source: before.source,
            restarted: false,
            patch_warning: None,
            error: Some(err),
        });
    }
    let patch_warning = staged
        .get("patchWarning")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    if let Some(summary) = staged.get("patchSummary") {
        crate::supervisor::file_log(&format!("[agent-update] 补丁适配摘要：{summary}"));
    }
    let staging_dir = staged
        .get("stagingDir")
        .and_then(|v| v.as_str())
        .map(std::path::PathBuf::from)
        .ok_or_else(|| BridgeError::internal("暂存结果缺 stagingDir"))?;

    // 停内核释放文件锁 → 原子切换覆盖层 → 刷新现役内核/CLI shim 并重启。
    emit_agent_progress(app, "switching", "正在停止旧内核并切换版本…");
    sv.shutdown();
    std::thread::sleep(std::time::Duration::from_millis(250));
    if let Err(e) = swap_agent_overlay(&state.paths.app_data, &staging_dir) {
        let _ = std::fs::remove_dir_all(&staging_dir);
        restart_agent_kernel(app, &sv);
        return Ok(AgentMutateResult {
            ok: false,
            version: before.version,
            agent_source: before.source,
            restarted: true,
            patch_warning,
            error: Some(e),
        });
    }
    let active = sv.refresh_active_agent();
    super::dsh_cli::ensure_dsh_cli_shim();
    emit_agent_progress(app, "restarting", "切换完成，正在重启内核…");
    restart_agent_kernel(app, &sv);
    Ok(AgentMutateResult {
        ok: true,
        version: active.version,
        agent_source: active.source,
        restarted: true,
        patch_warning,
        error: None,
    })
}

fn run_agent_reset_bundled(app: &AppHandle) -> Result<AgentMutateResult, BridgeError> {
    let Ok(_guard) = AGENT_MUTATE_LOCK.try_lock() else {
        return Err(BridgeError::invalid_arg("已有 agent 更新任务正在进行中"));
    };
    let state = app.state::<AppState>();
    let sv = state
        .supervisor
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone()
        .ok_or_else(|| BridgeError::internal("supervisor 未初始化"))?;
    sv.shutdown();
    std::thread::sleep(std::time::Duration::from_millis(250));
    let overlay = state.paths.app_data.join("agent");
    if overlay.exists() {
        std::fs::remove_dir_all(&overlay).map_err(|e| BridgeError::internal(format!("删除覆盖层失败：{e}")))?;
    }
    let active = sv.refresh_active_agent();
    super::dsh_cli::ensure_dsh_cli_shim();
    restart_agent_kernel(app, &sv);
    Ok(AgentMutateResult {
        ok: true,
        version: active.version,
        agent_source: active.source,
        restarted: true,
        patch_warning: None,
        error: None,
    })
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

    /// 原子切换 agent 覆盖层：合法 staging 生效并清旧备；越界/非法前缀被拒。
    #[test]
    fn swap_agent_overlay_atomic_and_rejects_escape() {
        let base = std::env::temp_dir().join(format!("dsh-swap-agent-{}-{}", std::process::id(), line!()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();

        // 非法目录名被拒
        let bad = base.join("not-staging");
        std::fs::create_dir_all(&bad).unwrap();
        assert!(swap_agent_overlay(&base, &bad).is_err());

        // 首次激活
        let st1 = base.join("agent-staging-1");
        std::fs::create_dir_all(&st1).unwrap();
        std::fs::write(st1.join("marker.txt"), "v1").unwrap();
        swap_agent_overlay(&base, &st1).unwrap();
        assert_eq!(std::fs::read_to_string(base.join("agent").join("marker.txt")).unwrap(), "v1");
        assert!(!st1.exists());

        // 二次覆盖（替换旧 agent 并清 agent-prev）
        let st2 = base.join("agent-staging-2");
        std::fs::create_dir_all(&st2).unwrap();
        std::fs::write(st2.join("marker.txt"), "v2").unwrap();
        swap_agent_overlay(&base, &st2).unwrap();
        assert_eq!(std::fs::read_to_string(base.join("agent").join("marker.txt")).unwrap(), "v2");
        assert!(!base.join("agent-prev").exists());

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn parse_agent_progress_line_extracts_valid_json_only() {
        let p = parse_agent_progress_line(r#"[agent-progress] {"stage":"downloading","message":"正在下载"}"#).unwrap();
        assert_eq!(p["stage"], "downloading");
        assert_eq!(p["message"], "正在下载");
        assert!(parse_agent_progress_line("[sidecar] ordinary log").is_none());
        assert!(parse_agent_progress_line("[agent-progress] not-json").is_none());
        assert!(parse_agent_progress_line("[agent-progress] 123").is_none());
    }
}
