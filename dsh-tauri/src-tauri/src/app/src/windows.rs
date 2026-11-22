//! 多窗管理：主窗（loading→内核页）、浮窗（分屏）。
//!
//! 参数对齐 Electron 版（main.js createFloatWindow）：
//! - 浮窗 900×640（min 480×360），同会话复用、上限 4 个；
//!
//! 宠物窗（160×160 透明置顶 + `__DSH_PET__` 模式注入 + 最小化自动弹窗 +
//! 看门狗）已于 2026-10 随 harness-pet 插件退役整体裁撤；赞助窗与客户端更新
//! 进度弹窗已于 2026-11-22 整体裁撤。
//!
//! 所有窗都注入 bridge 垫片（initialization_script 对每次导航生效）；
//! 浮窗追加模式注入脚本（`__DSH_FLOAT__`，契约 bridge-api.md §5）。

use bridge::{BridgeError, BRIDGE_SHIM_JS};
use tauri::{Emitter, Manager, WebviewUrl};

pub const FLOAT_MAX: usize = 4;

/// 浮窗会话注册表（label 前缀 float-）。
pub fn float_label(session_id: &str) -> String {
    format!("float-{}", sanitize_label(session_id))
}

/// label 只允许安全字符（防 label 注入）。
fn sanitize_label(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).take(64).collect()
}

/// 工作区逻辑尺寸 → 首次打开默认几何（纯计算，可单测）：
/// 90% 宽/高，clamp 到 [min 980×600, 舒适上限 1152×720]，并居中于工作区。
/// 小屏（< min）会顶到 min 下限（与 min_inner_size 同口径，绝不小于最小窗）。
fn compute_default_geometry(work_w: f64, work_h: f64, work_x: f64, work_y: f64) -> (f64, f64, f64, f64) {
    const DEFAULT_W: f64 = 1152.0;
    const DEFAULT_H: f64 = 720.0;
    const MIN_W: f64 = 980.0;
    const MIN_H: f64 = 600.0;
    const RATIO: f64 = 0.9;
    let w = (work_w * RATIO).clamp(MIN_W, DEFAULT_W);
    let h = (work_h * RATIO).clamp(MIN_H, DEFAULT_H);
    let x = work_x + (work_w - w) / 2.0;
    let y = work_y + (work_h - h) / 2.0;
    (w, h, x, y)
}

/// 首次打开（无已存状态）默认尺寸：相对主显示器**工作区**取合理比例，
/// 拿不到 monitor / scale_factor 非法 → 回退 1152×720（系统默认居中），
/// 绝不 panic。返回 (w, h, Some((x, y))) 逻辑像素（inner_size/position 同口径）。
fn default_main_window_geometry(app: &tauri::AppHandle) -> (f64, f64, Option<(f64, f64)>) {
    let Some(mon) = app.primary_monitor().ok().flatten() else {
        return (1152.0, 720.0, None);
    };
    let scale = mon.scale_factor();
    if scale <= 0.0 {
        return (1152.0, 720.0, None);
    }
    // work_area 是物理像素，除 scale_factor 转逻辑像素（inner_size/position 口径）。
    let wa = mon.work_area();
    let work_w = wa.size.width as f64 / scale;
    let work_h = wa.size.height as f64 / scale;
    let work_x = wa.position.x as f64 / scale;
    let work_y = wa.position.y as f64 / scale;
    let (w, h, x, y) = compute_default_geometry(work_w, work_h, work_x, work_y);
    (w, h, Some((x, y)))
}

/// 主窗：decorations 平台门 + 导航围栏 + 垫片。初始加载 loading 页。
///
/// Linux 例外：WebKitGTK 下 undecorated 窗口存在首帧不渲染/白屏的已知问题
/// （tauri/wry），故 Linux 退回原生标题栏。
/// macOS 例外：自绘控制条是 Windows 专属视觉（右上 min/max/close），mac
/// 用户只认左上红绿灯（含绿钮全屏）；实测报告「找不到关闭和全屏按钮」。
/// mac 退回原生标题栏（交通灯），垫片在该平台降级为仅注入 ⋯ 菜单悬浮钮
/// （菜单里的更新/退出/通知开关等功能不丢），不注入全宽控制条。
/// 仅 Windows 维持自绘标题栏（decorations:false）。
///
/// 高 DPI（2.8K）缩放：WebView2 已按系统 DPI 自动缩放 web 内容——wry 建窗时
/// 不覆盖 `ICoreWebView2Controller3::RasterizationScale`（默认 = 显示器 DPI ×
/// 系统文字缩放，且 `ShouldDetectMonitorScaleChanges` 默认 true，跨屏拖动到
/// 不同 scale 的显示器时 WebView2 会自动更新它）。因此此处**刻意不调用**
/// `set_zoom(scale_factor)`：`SetZoomFactor` 是乘在 `RasterizationScale` 之上的
/// 额外倍数，设成 `scale_factor` 会让页面 `devicePixelRatio ≈ scale²`，造成
/// 双重缩放。窗口几何的 scale_factor 换算（`default_main_window_geometry` /
/// `save_main_window_state`）是独立口径，已正确按逻辑/物理像素处理。
#[allow(clippy::too_many_arguments)]
pub fn create_main_window(
    app: &tauri::AppHandle,
    loading_url: &str,
    saved: Option<(i32, i32, f64, f64, bool)>,
) -> Result<tauri::WebviewWindow, tauri::Error> {
    let mut b = tauri::webview::WebviewWindowBuilder::new(
        app,
        "main",
        // url 解析失败走既有 Result 通道（上层 setup `?`），不在主窗创建
        // 路径留 panic——「客户端必须能打开」原则。
        WebviewUrl::External(
            loading_url
                .parse::<tauri::Url>()
                .map_err(tauri::Error::InvalidUrl)?,
        ),
    )
    .title("DSH Pack")
    .min_inner_size(980.0, 600.0)
    // 平台门：Linux（WebKitGTK undecorated 白屏）与 macOS（用户只认原生
    // 红绿灯，自绘控制条是 Windows 专属视觉）退回原生标题栏；仅 Windows
    // 自绘（decorations:false）。垫片在原生标题栏平台同步降级（见
    // bridge-shim.js 平台门），防双份控制条。
    .decorations(cfg!(any(target_os = "linux", target_os = "macos")))
    // 显式声明（用户实测「不能调整窗口大小」）：undecorated 窗口默认应可
    // 拖边缩放，显式置 true 防构建配置漂移；与 Electron frame:false +
    // resizable:true 行为对齐。
    .resizable(true)
    // 外部文件拖入落到实处（2026-10-14 真机实测）：Tauri 默认开启 drag-drop
    // handler，wry 在 Windows 随之设 SetAllowExternalDrop(false)——WebView2 不再
    // 处理外部拖放，页面收不到任何 HTML5 drop 事件；F1 裁撤后 tauri://drag-*
    // 已无消费方（内核全仓 0 处监听），默认态即「拖入文件」全链哑火（实测
    // HTML5 事件 0 条）。显式关闭该 handler 放行页面原生拖放——tauri 文档
    // 原话「required to use HTML5 drag and drop APIs on the frontend on
    // Windows」；内核 composer 附件链与 dsh-better-sidebar 上传都吃 HTML5
    // dataTransfer，此开关是它们的前置。
    .disable_drag_drop_handler()
    .initialization_script(BRIDGE_SHIM_JS)
    .on_navigation(|url| {
        // 导航围栏：仅 127.0.0.1（内核/内嵌页）与 tauri 内部协议。
        let s = url.as_str();
        s.starts_with("http://127.0.0.1") || s.starts_with("tauri://") || s.starts_with("http://tauri.localhost")
    });
    if let Some((x, y, w, h, maxed)) = saved {
        b = b.position(x as f64, y as f64).inner_size(w, h);
        if maxed {
            b = b.maximized(true);
        }
    } else {
        // 首次打开（无已存状态）：按当前显示器工作区取 90% 比例并居中，
        // 拿不到 monitor 回退 1152×720（系统默认居中），绝不 panic。
        let (w, h, pos) = default_main_window_geometry(app);
        b = b.inner_size(w, h);
        if let Some((x, y)) = pos {
            b = b.position(x, y);
        }
    }
    let win = b.build()?;
    let handle = app.clone();
    win.on_window_event(move |e| {
        if matches!(e, tauri::WindowEvent::Resized(_)) {
            if let Some(w) = handle.get_webview_window("main") {
                if let Ok(max) = w.is_maximized() {
                    let _ = handle.emit("window-maximized", max);
                }
                // G3 宠物窗分支（最小化自动弹宠物窗：Resized 内轮询 is_minimized
                // 抓上升沿）已于 2026-10 随 harness-pet 插件退役整体移除。
            }
        }
        if let tauri::WindowEvent::CloseRequested { api, .. } = e {
            // #160：关窗读 closeToTray（缺省 true，Electron `s.closeToTray !== false`
            // 同口径）——true 才 prevent_close + hide 到托盘（后台常驻、内核继续
            // 跑）；false 放行默认销毁：主窗为最后窗口时 tauri-runtime 触发
            // ExitRequested → 真退出（杀树仍走托盘「退出」/ExitRequested 的
            // supervisor.shutdown 路径，此处不再直接退进程）。
            let close_to_tray = handle
                .try_state::<crate::AppState>()
                .map(|state| {
                    let store = shell_core::SettingsStore::new(state.paths.settings.clone());
                    close_to_tray_from_store(&store)
                })
                // 拿不到 AppState（异常启动态）保守按 true：宁隐藏留托盘，不误退。
                .unwrap_or(true);
            if close_to_tray {
                // prevent_close 必须显式调：不拦则窗口走默认销毁。
                api.prevent_close();
                if let Some(w) = handle.get_webview_window("main") {
                    hide_main_to_tray(&w);
                }
            } else {
                // 关窗即退：从窗口销毁到 RunEvent::ExitRequested 之间有窗口期，
                // 后台线程（内核就绪/心跳/更新检查）的在飞事件不得再触窗——
                // EXITING 是唯一能挡住 Destroyed 态 panic 的闸门（见 lib.rs
                // guarded_window_op 文档），此处置位先于销毁。
                crate::mark_exiting("关窗即退（closeToTray=false）");
            }
        }
    });
    Ok(win)
}

/// 读主窗当前 outer_position + inner_size + is_maximized 并存到
/// window-state.json。隐藏到托盘（hide_main_to_tray）与真退出
/// （RunEvent::ExitRequested/Exit）两条路径共用——用户调整过窗口尺寸/位置后，
/// 无论走「关窗→托盘→退出」还是「托盘退出 / Cmd+Q」都能记住。任何读取失败
/// （窗口已销毁 / 异常退出）都 if let 静默跳过，绝不 panic。
///
/// 口径对齐：outer_position/inner_size 返回**物理像素**，而恢复侧
/// create_main_window 的 position/inner_size 吃**逻辑像素**（与
/// default_main_window_geometry 同口径）。必须除 scale_factor 转逻辑像素，
/// 否则高 DPI（scale≠1）下每次保存/恢复都会把窗口放大/偏移，越存越大。
pub fn save_main_window_state(app: &tauri::AppHandle) {
    if let (Some(w), Some(state)) = (app.get_webview_window("main"), app.try_state::<crate::AppState>()) {
        if let (Ok(pos), Ok(size), Ok(scale)) = (w.outer_position(), w.inner_size(), w.scale_factor()) {
            if scale <= 0.0 {
                return;
            }
            let maxed = w.is_maximized().unwrap_or(false);
            let x = pos.x as f64 / scale;
            let y = pos.y as f64 / scale;
            let w = size.width as f64 / scale;
            let h = size.height as f64 / scale;
            let _ = crate::save_window_state(&state, (x.round() as i32, y.round() as i32, w, h, maxed));
        }
    }
}

/// 关窗→托盘（0.5.0）：保存窗口状态后隐藏主窗。进程与内核继续运行，
/// 经托盘「显示主窗口」/ 双击图标（第二实例聚焦）唤回。
/// 唯一真退出入口 = 托盘「退出」（supervisor.shutdown + exit，Job Object 杀树）。
pub fn hide_main_to_tray(win: &tauri::WebviewWindow) {
    let app = win.app_handle();
    // 隐藏前保存窗口状态（settings.json windowState）——此后可能经强杀路径
    // 退出，CloseRequested 不再有触发机会。
    save_main_window_state(&app);
    let _ = win.hide();
}

/// 浮窗（分屏）：同会话复用 + 上限 FLOAT_MAX。
///
/// K23 卡死根治：WebView2 建窗必须在独立线程——同步 command（float_window）
/// 里 `build()` 会在 Windows 上死锁（Tauri 2 官方 Known issues；赞助窗 v0.5.0
/// 三症同源已修，浮窗/宠物窗此前漏改）。复用/FLOAT_MAX/URL 校验仍同步返回，
/// 只有建窗 + show 移入独立线程（与赞助窗/更新进度弹窗同款模式）。
pub fn open_float_window<R: tauri::Runtime>(app: &tauri::AppHandle<R>, kernel_url: &str, session_id: &str) -> Result<serde_json::Value, BridgeError> {
    let label = float_label(session_id);
    if let Some(existing) = app.get_webview_window(&label) {
        let _ = existing.show();
        let _ = existing.set_focus();
        return Ok(serde_json::json!({ "ok": true, "reused": true }));
    }
    let floats = app.webview_windows().keys().filter(|k| k.starts_with("float-")).count();
    if floats >= FLOAT_MAX {
        return Err(BridgeError::not_found(format!("浮窗已达上限 {FLOAT_MAX}")));
    }
    // 同步校验 URL：非法立即报错，不落入线程静默失败（白屏）。
    let url = kernel_url.trim_end_matches('/').to_string();
    let parsed = parse_url(&url)?;
    let mode_script = format!(
        r#"(function(){{ try{{ window.__DSH_FLOAT__ = Object.freeze({{ sessionId: {session_id_json} }}); }}catch(e){{}} }})();"#,
        session_id_json = serde_json::to_string(session_id).unwrap_or_else(|_| "\"\"".into())
    );
    let preset_script = float_session_preset(session_id);
    let handle = app.clone();
    std::thread::Builder::new()
        .name(format!("float-window-{label}"))
        .spawn(move || {
            // 双击竞态复检：两个线程同时过了外层检查时，后来者只聚焦。
            if let Some(existing) = handle.get_webview_window(&label) {
                let _ = existing.show();
                let _ = existing.set_focus();
                return;
            }
            match build_float_window(&handle, &label, parsed, &mode_script, &preset_script) {
                Ok(win) => {
                    let _ = win.show();
                }
                Err(e) => eprintln!("[float] 浮窗创建失败（不影响主窗）: {e}"),
            }
        })
        .map_err(|e| BridgeError::internal(format!("浮窗线程启动: {e}")))?;
    Ok(serde_json::json!({ "ok": true, "async": true }))
}

/// 浮窗构造（独立函数供集成测试复用——mock runtime 下走与生产完全
/// 同款的 builder 路径，验证窗口属性与销毁）。泛型 R 兼容 Wry/MockRuntime。
pub fn build_float_window<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    label: &str,
    url: tauri::Url,
    mode_script: &str,
    preset_script: &str,
) -> Result<tauri::WebviewWindow<R>, tauri::Error> {
    tauri::webview::WebviewWindowBuilder::new(app, label, WebviewUrl::External(url))
        .title("DSH 会话")
        .inner_size(900.0, 640.0)
        .min_inner_size(480.0, 360.0)
        .decorations(false)
        // 同主窗：放行 HTML5 外部拖放（会话浮窗同样是内核 UI，composer
        // 附件链同款；默认 handler 会把它一并拦死，见 create_main_window）。
        .disable_drag_drop_handler()
        .initialization_script(BRIDGE_SHIM_JS)
        .initialization_script(mode_script)
        .initialization_script(preset_script)
        .initialization_script(FLOAT_BAR_SCRIPT)
        .initialization_script(FLOAT_WATCHDOG_SCRIPT)
        .on_navigation(|url| url.as_str().starts_with("http://127.0.0.1"))
        .build()
}

/// URL 解析 helper。
fn parse_url(s: &str) -> Result<tauri::Url, BridgeError> {
    s.parse::<tauri::Url>().map_err(|e| BridgeError::internal(format!("url: {e}")))
}

/// 浮窗 localStorage 预置（Electron preload 语义：比 sessions.open() 可靠）。
fn float_session_preset(session_id: &str) -> String {
    format!(
        r#"(function(){{
  try {{
    var sid = {sid};
    var key = 'dsh.sessions.current';
    var raw = localStorage.getItem(key);
    var parsed = raw ? JSON.parse(raw) : {{}};
    if (parsed && typeof parsed === 'object') {{
      parsed.sessionId = String(sid);
      delete parsed.subagentAddress;
      localStorage.setItem(key, JSON.stringify(parsed));
    }}
  }} catch (e) {{}}
}})();"#,
        sid = serde_json::to_string(session_id).unwrap_or_else(|_| "\"\"".into())
    )
}

/// 浮窗 24px 纯拖拽条 + 关闭按钮（DOMContentLoaded 注入，避免 head 未就绪）。
const FLOAT_BAR_SCRIPT: &str = r#"
(function(){
  function inject(){
    if (document.getElementById('__dsh_desktop_floatbar__')) return;
    var bar = document.createElement('div');
    bar.id = '__dsh_desktop_floatbar__';
    bar.setAttribute('data-tauri-drag-region', '');
    bar.style.cssText = 'position:fixed;top:0;left:0;right:0;height:24px;z-index:2147483647;' +
      'display:flex;align-items:center;background:rgba(15,20,28,.92);border-bottom:1px solid #232b36;' +
      'font:12px "Segoe UI","Microsoft YaHei",sans-serif;color:#9fb0c0;user-select:none;pointer-events:auto';
    var t = document.createElement('span');
    t.textContent = 'DSH 会话'; t.style.cssText = 'padding:0 10px;pointer-events:none';
    var sp = document.createElement('span'); sp.style.flex = '1';
    var btn = document.createElement('button');
    btn.textContent = '\u2715';
    btn.style.cssText = 'width:36px;height:24px;border:0;background:transparent;color:#d7dde4;cursor:pointer;font-size:12px';
    btn.onmouseenter = function(){ btn.style.background = '#c0392b'; };
    btn.onmouseleave = function(){ btn.style.background = 'transparent'; };
    btn.onclick = function(){ try { window.dshDesktop.floatWindow.close(); } catch (e) {} };
    bar.appendChild(t); bar.appendChild(sp); bar.appendChild(btn);
    document.body.style.paddingTop = '24px';
    (document.body || document.documentElement).appendChild(bar);
  }
  if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', inject);
  else inject();
})();
"#;

/// 浮窗活性看门狗（FW1 白屏双保险的壳层一半）：
/// - initialization_script 通道（AddScriptToExecuteOnDocumentCreated），每次
///   导航/reload 必执行，不依赖 eval 时序；
/// - 3s 后 body 仍无任何子元素（内核未监听/重启窗口期导航失败、SPA 挂掉）
///   → 自动 reload 一次（sessionStorage 记次数，每窗最多一次）；
/// - reload 后 3s 仍死 → 可见错误卡（重试/关闭），绝不留纯白屏；
/// - about:blank 预导航文档直接跳过（protocol 守卫）。
const FLOAT_WATCHDOG_SCRIPT: &str = r#"
(function(){
  if (location.protocol !== 'http:' && location.protocol !== 'https:') return;
  var FLAG = '__dsh_float_watchdog_reloaded__';
  function flag(){
    try { return sessionStorage.getItem(FLAG) === '1'; } catch (e) { return false; }
  }
  function setFlag(v){
    try { if (v) sessionStorage.setItem(FLAG, '1'); else sessionStorage.removeItem(FLAG); } catch (e) {}
  }
  function alive(){
    try { return !!(document.body && document.body.childElementCount > 0); } catch (e) { return false; }
  }
  function showError(){
    if (document.getElementById('__dsh_float_load_error__')) return;
    var d = document.createElement('div');
    d.id = '__dsh_float_load_error__';
    d.style.cssText = 'position:fixed;inset:0;z-index:2147483647;display:flex;flex-direction:column;' +
      'align-items:center;justify-content:center;gap:12px;background:#0b1220;color:#e6ecff;' +
      'font:14px "Segoe UI","Microsoft YaHei",sans-serif;text-align:center;padding:24px';
    var t = document.createElement('div'); t.textContent = '浮窗页面加载失败';
    var sub = document.createElement('div');
    sub.style.cssText = 'color:#8b9ac4;font-size:12px;line-height:18px';
    sub.textContent = '页面长时间无内容（内核可能正在启动或重启），自动重试一次仍未恢复。';
    var btns = document.createElement('div'); btns.style.cssText = 'display:flex;gap:10px';
    var retry = document.createElement('button');
    retry.textContent = '重试';
    retry.style.cssText = 'min-width:88px;padding:6px 14px;border:1px solid #3a4656;border-radius:8px;background:#1a2332;color:#e6ecff;cursor:pointer;font-size:13px';
    retry.onclick = function(){ setFlag(false); location.reload(); };
    var close = document.createElement('button');
    close.textContent = '关闭浮窗';
    close.style.cssText = retry.style.cssText;
    close.onclick = function(){
      try {
        if (window.dshDesktop && window.dshDesktop.floatWindow && window.dshDesktop.floatWindow.close) {
          window.dshDesktop.floatWindow.close();
        } else if (window.close) { window.close(); }
      } catch (e) {}
    };
    btns.appendChild(retry); btns.appendChild(close);
    d.appendChild(t); d.appendChild(sub); d.appendChild(btns);
    (document.body || document.documentElement).appendChild(d);
    document.title = '浮窗加载失败';
  }
  setTimeout(function(){
    if (alive()) { setFlag(false); return; }
    if (!flag()) { setFlag(true); location.reload(); return; }
    showError();
  }, 3000);
})();
"#;

// 宠物窗全链（PET_SEQ / pet_auto_open_from_store / should_open_pet_on_minimize /
// open_pet_window / build_pet_window / PET_MODE_SCRIPT / PET_WATCHDOG_SCRIPT 与
// PET_W·PET_H 尺寸常量）已于 2026-10 随 harness-pet 插件退役整体裁撤。
// 下方 close_to_tray_from_store 保留——它服务托盘关窗（commands/menu.rs），与宠物无关。

/// #160：读 settings.json 的 closeToTray（写侧 commands/menu.rs `toggle_setting`
/// 经 `toggle-close-to-tray`，扁平键）。缺省 **true**——对齐 Electron
/// `closeToTrayEnabled()` 的 `s.closeToTray !== false`（仅显式 false 才直接退出，
/// 未设置/损坏/非布尔一律关到托盘，绝不因坏配置误退丢会话）。
pub fn close_to_tray_from_store(store: &shell_core::SettingsStore) -> bool {
    store.get("closeToTray").ok().flatten().and_then(|v| v.as_bool()).unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 问题 1：首次打开默认几何——90% 工作区、clamp 到 [980×600, 1152×720]、
    /// 居中于工作区（左上角 + 剩余/2）。纯计算决策表。
    #[test]
    fn default_geometry_clamps_and_centers() {
        // 大屏（1920×1080 逻辑，无任务栏）→ 顶到舒适上限 1152×720，居中。
        assert_eq!(compute_default_geometry(1920.0, 1080.0, 0.0, 0.0), (1152.0, 720.0, 384.0, 180.0));
        // 笔记本 1366×768（任务栏占高，工作区 1366×728）→ 宽顶到 1152 上限、
        // 高 90%（655.2），都留边距不顶屏。
        let (w, h, x, y) = compute_default_geometry(1366.0, 728.0, 0.0, 0.0);
        assert_eq!(w, 1152.0, "宽应顶到新上限 1152（1366×0.9=1229.4 > 1152）");
        assert!((h - 655.2).abs() < 1e-9, "h={h}");
        assert!((x - (1366.0 - w) / 2.0).abs() < 1e-9, "x={x}");
        assert!((y - (728.0 - h) / 2.0).abs() < 1e-9, "y={y}");
        // 小屏 1024×640 → 宽顶到 min 980、高顶到 min 600（与 min_inner_size 同口径）。
        let (w, h, x, y) = compute_default_geometry(1024.0, 640.0, 0.0, 0.0);
        assert_eq!((w, h), (980.0, 600.0));
        assert!((x - (1024.0 - 980.0) / 2.0).abs() < 1e-9 && (y - (640.0 - 600.0) / 2.0).abs() < 1e-9);
        // 工作区左上角偏移（副屏坐标非 0,0）→ 居中基准随工作区平移。
        let (_, _, x2, y2) = compute_default_geometry(1920.0, 1080.0, 2000.0, 100.0);
        assert_eq!((x2, y2), (2000.0 + 384.0, 100.0 + 180.0));
    }

    #[test]
    fn float_label_sanitizes_hostile_input() {
        assert_eq!(float_label("abc123"), "float-abc123");
        // 注入字符被白名单替换。
        assert_eq!(float_label("a\" onclick=x"), "float-a__onclick_x");
        // 超长截断到 64。
        let long = "x".repeat(200);
        assert_eq!(float_label(&long).len(), "float-".len() + 64);
        // 中文 → 下划线（label 安全字符集）。
        assert_eq!(float_label("会话"), "float-__");
    }

    #[test]
    fn float_preset_embeds_session_and_clears_subagent() {
        let js = float_session_preset("sess-42");
        assert!(js.contains("\"sess-42\""), "sessionId 应以 JSON 字符串嵌入: {js}");
        assert!(js.contains("dsh.sessions.current"));
        assert!(js.contains("delete parsed.subagentAddress"), "对齐 Electron 语义（清 subagentAddress）");
        // 引号安全：恶意 id 不逃逸字符串（serde_json 会转义双引号）。
        let evil = float_session_preset("a\";alert(1);//");
        assert!(evil.contains("a\\\";alert(1);//"), "应 JSON 转义: {evil}");
    }

    /// 浮窗模式注入形态（宠物窗注入脚本随 harness-pet 退役一并删除）。
    #[test]
    fn float_mode_scripts_present() {
        assert!(FLOAT_BAR_SCRIPT.contains("__dsh_desktop_floatbar__"));
        assert!(FLOAT_BAR_SCRIPT.contains("floatWindow.close"));
    }

    #[test]
    fn parse_url_accepts_local_rejects_junk() {
        assert!(parse_url("http://127.0.0.1:51731/").is_ok());
        assert!(parse_url("not a url").is_err());
        // scheme 不设限（围栏在 on_navigation 层）；只测形态拒绝。
    }

    /// 原生标题栏平台门：主窗 decorations 必须平台门控——Linux 退回原生
    /// 标题栏（undecorated 首帧不渲染是 tauri/wry 已知问题）；macOS 同样退
    /// 回原生标题栏（用户只认红绿灯/全屏钮，自绘控制条是 Windows 专属
    /// 视觉，实测「找不到关闭和全屏按钮」）。仅 Windows 维持自绘。
    #[test]
    fn main_window_decorations_platform_gated_shape() {
        let src = include_str!("windows.rs");
        let seg = src
            .split("pub fn create_main_window")
            .nth(1)
            .and_then(|s| s.split("pub fn hide_main_to_tray").next())
            .expect("create_main_window 函数体");
        assert!(
            seg.contains(".decorations(cfg!(any(target_os = \"linux\", target_os = \"macos\")))"),
            "主窗 decorations 必须平台门控（Linux 防白屏 + mac 原生红绿灯，仅 Win 自绘）: {seg}"
        );
        assert!(
            !seg.contains(".decorations(false)"),
            "主窗不得硬编码 decorations(false)（Linux 白屏回归面）: {seg}"
        );
    }

    /// FW1 白屏双保险——壳层看门狗形态锚点：
    /// - 3s 活性探测 + reload 恰好一次（sessionStorage 防抖）+ 二次失败错误卡；
    /// - about:blank 预导航守卫（init script 在导航前文档也会执行一次）；
    /// - 错误卡必须可关闭（优先桥 floatWindow.close，退化 window.close）。
    #[test]
    fn float_watchdog_script_shape() {
        assert!(FLOAT_WATCHDOG_SCRIPT.contains("3000"), "3s 活性探测: {FLOAT_WATCHDOG_SCRIPT}");
        assert!(FLOAT_WATCHDOG_SCRIPT.contains("location.reload()"), "死后必须自动 reload: {FLOAT_WATCHDOG_SCRIPT}");
        assert!(FLOAT_WATCHDOG_SCRIPT.contains("__dsh_float_watchdog_reloaded__"), "reload 只做一次（标记）: {FLOAT_WATCHDOG_SCRIPT}");
        assert!(FLOAT_WATCHDOG_SCRIPT.contains("__dsh_float_load_error__"), "二次失败可见错误卡: {FLOAT_WATCHDOG_SCRIPT}");
        assert!(FLOAT_WATCHDOG_SCRIPT.contains("floatWindow.close"), "错误卡可关窗（桥优先）: {FLOAT_WATCHDOG_SCRIPT}");
        assert!(FLOAT_WATCHDOG_SCRIPT.contains("location.protocol"), "预导航 about:blank 守卫: {FLOAT_WATCHDOG_SCRIPT}");
    }

    /// 看门狗必须接进浮窗 builder（initialization_script 通道，reload 后仍生效）。
    #[test]
    fn float_window_builder_wires_watchdog_shape() {
        let src = include_str!("windows.rs");
        let seg = src
            .split("pub fn build_float_window")
            .nth(1)
            .and_then(|s| s.split("/// URL 解析 helper").next())
            .expect("build_float_window 函数体");
        assert!(seg.contains(".initialization_script(FLOAT_WATCHDOG_SCRIPT)"), "浮窗必须注入看门狗: {seg}");
        assert!(seg.contains(".initialization_script(BRIDGE_SHIM_JS)"), "桥垫片不得回退丢失: {seg}");
    }

    /// K23：浮窗建窗必须移入独立线程（同步 command 里 build() 在 Windows 死锁——
    /// Tauri 2 官方 Known issues）。open_float_window 只做复用/上限/URL 同步校验，
    /// 建窗 + show 全在 std::thread 线程内。
    #[test]
    fn float_window_creation_threaded_shape() {
        let src = include_str!("windows.rs");
        let seg = src
            .split("pub fn open_float_window")
            .nth(1)
            .and_then(|s| s.split("pub fn build_float_window").next())
            .expect("open_float_window 函数体");
        assert!(seg.contains("std::thread::Builder"), "浮窗建窗必须移出同步 command 线程: {seg}");
        assert!(seg.contains("build_float_window(&handle"), "必须经 build_float_window 建窗: {seg}");
        assert!(seg.contains("parse_url(&url)?"), "URL 必须同步校验（非法立即报错不白屏）: {seg}");
    }

    /// 主窗 CloseRequested 语义（#160）：读 closeToTray——true（缺省）拦截默认
    /// 销毁 → 隐藏留托盘；false 放行默认销毁 → 真退出。源码形态断言（WebviewWindow
    /// 无法在单测构造），防回退到无条件 prevent_close（#160 假开关）或 exit(0)
    /// （0.1.0 语义：关窗即退）。
    #[test]
    fn close_requested_hides_instead_of_exit_shape() {
        let src = include_str!("windows.rs");
        let seg = src
            .split("CloseRequested")
            .nth(1)
            .and_then(|s| s.split("/// 浮窗").next())
            .expect("CloseRequested 处理段");
        assert!(seg.contains("close_to_tray_from_store"), "必须读 closeToTray 设置: {seg}");
        assert!(seg.contains("if close_to_tray"), "必须以 closeToTray 门控: {seg}");
        assert!(seg.contains("prevent_close"), "true 分支必须拦截默认窗口销毁: {seg}");
        assert!(seg.contains("hide_main_to_tray"), "true 分支关窗 = 隐藏留托盘（非退出）: {seg}");
        assert!(!seg.contains("exit(0)"), "关窗不得直接退出进程（真退出走托盘/ExitRequested）: {seg}");
    }

    /// 关窗即退（closeToTray=false）必须在放行销毁前**先**置位退出闸门：
    /// 窗口销毁到 RunEvent::ExitRequested 之间仍有后台线程在飞事件（内核就绪/
    /// 心跳/更新检查），此时触已销毁的事件循环 = tao Destroyed 态 panic
    /// （见 lib.rs guarded_window_op 文档），而该 panic 会被 latch 到任意时刻。
    #[test]
    fn close_to_tray_false_arms_exiting_gate_before_destroy_shape() {
        let src = include_str!("windows.rs");
        let seg = src
            .split("CloseRequested")
            .nth(1)
            .and_then(|s| s.split("/// 浮窗").next())
            .expect("CloseRequested 处理段");
        let else_branch = seg.split("} else {").nth(1).expect("closeToTray=false 分支");
        assert!(
            else_branch.contains("crate::mark_exiting("),
            "false 分支必须在放行销毁前置位退出闸门: {else_branch}"
        );
        assert!(!else_branch.contains("exit(0)"), "不得直接退进程: {else_branch}");
    }

    /// #160：closeToTray 读取口径——缺省 true（关到托盘）；仅显式 false 才放行
    /// 直接退出；显式 true / 非布尔 / 损坏一律回落 true（Electron
    /// `s.closeToTray !== false` 同口径，绝不因坏配置误退丢会话）。
    #[test]
    fn close_to_tray_reads_flat_key_defaults_true() {
        let mut path = std::env::temp_dir();
        path.push(format!("dsh-close-to-tray-{}-{}.json", std::process::id(), line!()));
        let _ = std::fs::remove_file(&path);
        let store = shell_core::SettingsStore::new(&path);
        // 缺省：未写入 → true（关到托盘）。
        assert!(close_to_tray_from_store(&store));
        // 显式 false → 放行直接退出。
        store.set("closeToTray", serde_json::json!(false)).unwrap();
        assert!(!close_to_tray_from_store(&store));
        // 显式 true → 关到托盘。
        store.set("closeToTray", serde_json::json!(true)).unwrap();
        assert!(close_to_tray_from_store(&store));
        // 非布尔（字符串）→ 回落缺省 true。
        store.set("closeToTray", serde_json::json!("yes")).unwrap();
        assert!(close_to_tray_from_store(&store));
        let _ = std::fs::remove_file(&path);

        // 损坏文件 → load 自愈为空 → 回落 true（关到托盘，不误退）。
        let mut bad = std::env::temp_dir();
        bad.push(format!("dsh-close-to-tray-bad-{}-{}.json", std::process::id(), line!()));
        let _ = std::fs::remove_file(&bad);
        std::fs::write(&bad, "{not json").unwrap();
        let broken = shell_core::SettingsStore::new(&bad);
        assert!(close_to_tray_from_store(&broken));
        let _ = std::fs::remove_file(&bad);
        let _ = std::fs::remove_file(bad.with_extension("json.broken"));
    }

    /// hide_main_to_tray 先存状态再隐藏（隐藏后可能经强杀路径退出，
    /// CloseRequested 不再有触发机会）。
    #[test]
    fn hide_main_to_tray_saves_state_before_hide_shape() {
        let src = include_str!("windows.rs");
        let seg = src
            .split("pub fn hide_main_to_tray")
            .nth(1)
            .and_then(|s| s.split("// 浮窗").next())
            .expect("hide_main_to_tray 函数体");
        let save_pos = seg.find("save_main_window_state").expect("必须保存窗口状态");
        let hide_pos = seg.find("win.hide()").expect("必须隐藏窗口");
        assert!(save_pos < hide_pos, "先存状态后隐藏（强杀路径兜底）: {seg}");
    }

    /// 外部文件拖入落到实处（2026-10-14 真机实测根治）：Tauri 默认开启 drag-drop
    /// handler，Windows 上 wry 随之设 SetAllowExternalDrop(false)——页面收不到
    /// HTML5 drop；F1 裁撤后 tauri://drag-* 已无消费方，默认态即「拖入文件」全链
    /// 哑火（实测 HTML5 事件 0 条、DropOverlay 不出现）。主窗/浮窗（内核 UI，
    /// composer 附件链所在）必须显式 disable_drag_drop_handler；赞助/更新窗是
    /// 静态自绘页，不涉此面，保持默认（默认拦截同时挡住误拖引发的 file:// 导航面）。
    #[test]
    fn main_and_float_windows_reenable_html5_external_drop_shape() {
        let src = include_str!("windows.rs");
        let main_seg = src
            .split("pub fn create_main_window")
            .nth(1)
            .and_then(|s| s.split("if let Some((x, y, w, h, maxed))").next())
            .expect("create_main_window builder 链段");
        assert!(
            main_seg.contains(".disable_drag_drop_handler()"),
            "主窗必须显式关 Tauri drag-drop handler（否则 Windows HTML5 外部拖放全死）: {main_seg}"
        );
        let float_seg = src
            .split("pub fn build_float_window")
            .nth(1)
            .and_then(|s| s.split("/// URL 解析 helper").next())
            .expect("build_float_window 段");
        assert!(
            float_seg.contains(".disable_drag_drop_handler()"),
            "浮窗同为内核 UI（composer 同链），必须同款放行: {float_seg}"
        );
    }
}
