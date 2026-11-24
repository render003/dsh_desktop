//! WinInet 系统代理桥接：从 Windows 注册表读取用户级代理并注入内核/sidecar 子进程环境。

// ---- WinInet 系统代理桥接（0.6.3 beta.3，见 spawn_kernel 注入点注释）----

/// 纯函数：ProxyServer 原始值 → 代理 URL。`127.0.0.1:7892`（全协议单地址）；
/// `ftp=…;https=…;http=…`（分协议：https 优先、空值跳过、http 兑底）；
/// 已带 scheme 原样；空/无法解析 → None。
pub(super) fn parse_wininet_proxy_url(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let picked = if raw.contains(';') {
        let mut https_pick: Option<String> = None;
        let mut http_pick: Option<String> = None;
        for part in raw.split(';') {
            let p = part.trim().to_ascii_lowercase();
            if let Some(v) = p.strip_prefix("https=") {
                if !v.is_empty() {
                    https_pick = Some(v.to_string());
                    break;
                }
                continue;
            }
            if http_pick.is_none() {
                if let Some(v) = p.strip_prefix("http=") {
                    if !v.is_empty() {
                        http_pick = Some(v.to_string());
                    }
                }
            }
        }
        https_pick.or(http_pick)?
    } else {
        raw.to_string()
    };
    let picked = picked.trim();
    if picked.is_empty() {
        return None;
    }
    if picked.contains("://") {
        Some(picked.to_string())
    } else {
        Some(format!("http://{picked}"))
    }
}

/// 纯函数：代理 URL + 父环境探测 → 注入清单。用户显式配置优先：
/// HTTPS_PROXY/HTTP_PROXY/ALL_PROXY（任一形态）在场则不注入代理对；
/// NO_PROXY / NODE_USE_ENV_PROXY 按各自键独立判断（显式值含 =0 也尊重）。
pub(super) fn proxy_env_injection(proxy_url: &str, has_env: &dyn Fn(&str) -> bool) -> Vec<(String, String)> {
    let user_proxy_set = ["HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy"]
        .iter()
        .any(|k| has_env(k));
    let mut out: Vec<(String, String)> = Vec::new();
    if !user_proxy_set {
        out.push(("HTTPS_PROXY".into(), proxy_url.into()));
        out.push(("HTTP_PROXY".into(), proxy_url.into()));
        if !has_env("NO_PROXY") && !has_env("no_proxy") {
            out.push(("NO_PROXY".into(), "localhost,127.0.0.1,::1".into()));
        }
    }
    if !has_env("NODE_USE_ENV_PROXY") {
        out.push(("NODE_USE_ENV_PROXY".into(), "1".into()));
    }
    out
}

/// 注册表薄壳（Windows）：HKCU Internet Settings 的 ProxyEnable==1 + ProxyServer。
#[cfg(windows)]
fn wininet_system_proxy_url() -> Option<String> {
    // windows-api 是 windows crate 的别名（避让本 crate 的 pub mod windows）。
    use windows_api::core::PCWSTR;
    use windows_api::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RRF_RT_REG_SZ};
    const SUBKEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Internet Settings";
    let subkey: Vec<u16> = SUBKEY.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let enable_name: Vec<u16> = "ProxyEnable".encode_utf16().chain(std::iter::once(0)).collect();
        let mut enable: u32 = 0;
        let mut enable_cb = std::mem::size_of::<u32>() as u32;
        let r = RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(subkey.as_ptr()),
            PCWSTR(enable_name.as_ptr()),
            RRF_RT_REG_DWORD,
            None,
            Some(std::ptr::addr_of_mut!(enable).cast::<core::ffi::c_void>()),
            Some(&mut enable_cb),
        );
        if r.is_err() || enable != 1 {
            return None;
        }
        let server_name: Vec<u16> = "ProxyServer".encode_utf16().chain(std::iter::once(0)).collect();
        let mut buf = [0u16; 2048];
        let mut cb = (buf.len() * std::mem::size_of::<u16>()) as u32;
        let r = RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(subkey.as_ptr()),
            PCWSTR(server_name.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast::<core::ffi::c_void>()),
            Some(&mut cb),
        );
        if r.is_err() {
            return None;
        }
        let len = (cb as usize / 2).min(buf.len());
        let raw = String::from_utf16_lossy(&buf[..len]);
        parse_wininet_proxy_url(raw.trim_end_matches('\0'))
    }
}

/// 非 Windows：恒 None（桥接只在 Windows 生效）。
#[cfg(not(windows))]
fn wininet_system_proxy_url() -> Option<String> {
    None
}

/// 组合入口：spawn_kernel 与 agent 更新 sidecar 注入点调用；未启用/读不到/用户已配置 → 空清单。
pub(crate) fn system_proxy_env_injection() -> Vec<(String, String)> {
    match wininet_system_proxy_url() {
        Some(url) => proxy_env_injection(&url, &|k| std::env::var_os(k).is_some()),
        None => Vec::new(),
    }
}
