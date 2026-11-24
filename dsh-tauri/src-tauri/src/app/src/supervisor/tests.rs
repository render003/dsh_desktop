//! supervisor 单元测试与源码形态锁。

use super::*;

/// 合并 supervisor 目录全部运行期源码（CRLF 归一），供源码形态锚点测试使用。
fn supervisor_all_src() -> String {
    concat!(
        include_str!("mod.rs"),
        "\n",
        include_str!("boot.rs"),
        "\n",
        include_str!("probe.rs"),
        "\n",
        include_str!("proxy.rs"),
    )
    .replace("\r\n", "\n")
}

use std::sync::atomic::Ordering;

#[test]
fn has_materialized_farm_entry_detects_real_dir_and_skips_missing_or_unrelated() {
    let root = std::env::temp_dir().join(format!("dsh-farm-precheck-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let farm = root.join("farm");
    let payload = root.join("payload");
    std::fs::create_dir_all(farm.join("@deepseek-ai")).unwrap();
    std::fs::create_dir_all(payload.join("@deepseek-ai").join("dsh")).unwrap();
    std::fs::create_dir_all(payload.join("koffi")).unwrap();

    // 空 farm / 仅有 payload 不拥有的用户包 → false（跳过 node spawn）
    std::fs::create_dir_all(farm.join("user-only-pkg")).unwrap();
    assert!(!has_materialized_farm_entry(&farm, &payload));

    // 顶层实体化目录撞 payload 包 → true
    std::fs::create_dir_all(farm.join("koffi")).unwrap();
    assert!(has_materialized_farm_entry(&farm, &payload));
    std::fs::remove_dir_all(farm.join("koffi")).unwrap();
    assert!(!has_materialized_farm_entry(&farm, &payload));

    // scoped 实体化目录撞 payload 包 → true
    std::fs::create_dir_all(farm.join("@deepseek-ai").join("dsh")).unwrap();
    assert!(has_materialized_farm_entry(&farm, &payload));

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn node_compile_cache_wired_for_kernel_and_sidecar() {
    let src = supervisor_all_src();
    let spawn = src
        .split("fn spawn_kernel")
        .nth(1)
        .and_then(|s| s.split("/// 内核退出处理").next())
        .expect("spawn_kernel 段");
    assert!(spawn.contains("\"NODE_COMPILE_CACHE\""), "内核 spawn 必须注入 NODE_COMPILE_CACHE");
    let boot = src
        .split("fn run_sidecar_boot_inner")
        .nth(1)
        .and_then(|s| s.split("fn run_koffi_preflight").next())
        .expect("run_sidecar_boot_inner 段");
    assert!(boot.contains("\"NODE_COMPILE_CACHE\""), "sidecar boot 必须注入 NODE_COMPILE_CACHE");
    assert!(boot.contains("\"--guard-snapshot\""), "sidecar boot 必须支持合并启动快照");
}

/// 内核报错提取（真机形态回放，2026-08 QQ 群 ERR_INVALID_ARG_TYPE 案例）：
/// 未处理 rejection 的原始转储——message 行 + 栈帧 + `{ code: … }` 壳。
/// 此前壳侧 reason 只有「启动失败且无可回滚快照」概括，用户截图常把
/// message 行裁掉，排查被迫盲猜；恢复页/事故报告必须自带这一行。
#[test]
fn summarize_kernel_error_extracts_real_message() {
    let tail = concat!(
        "web-err| node:internal/process/task_queues:104:5\n",
        "web-err|     triggerUncaughtException(\n",
        "web-err|     ^\n",
        "web-err| TypeError [ERR_INVALID_ARG_TYPE]: The \"paths[0]\" argument must be of type string. Received type undefined\n",
        "web-err|     at ZoneAwarePromise (D:\\app\\zone.js:1:1)\n",
        "web-err|     at file:///D:/DSH%20Desktop/dsh-desktop/node_modules/@deepseek-ai/dsh/lib/bin.js:130:9\n",
        "web-err|     at process.processTicksAndRejections (node:internal/process/task_queues:104:5) {\n",
        "web-err|   code: 'ERR_INVALID_ARG_TYPE'\n",
        "web-err| }\n",
        "web-err|\n",
        "web-err| Node.js v24.18.1\n",
    );
    assert_eq!(
        summarize_kernel_error(tail).as_deref(),
        Some("TypeError [ERR_INVALID_ARG_TYPE]: The \"paths[0]\" argument must be of type string. Received type undefined")
    );
}

/// 前缀只出现在 4KB 管道块首：块内多行裸行同样要被扫描到（stderr 线程
/// 整块 relay 的真实形态），且多轮报错取**最后**一条（最贴近退出现场）。
#[test]
fn summarize_kernel_error_handles_chunk_prefix_and_prefers_latest() {
    let tail = concat!(
        "web-err| Error: dsh: first attempt failed\n",
        "web-err|     at somewhere (a.js:1:1)\n",
        "web| [loader-isolation] entry webserver failed: Error: listen EACCES: permission denied 127.0.0.1:50391\n",
        "    at updateError (file:///C:/x/cordis-plugin-loader/lib/index.js:326:9)\n",
        "dsh: fatal load failure: TypeError [ERR_INVALID_ARG_TYPE]: The \"cwd\" argument must be of type string\n",
    );
    assert_eq!(
        summarize_kernel_error(tail).as_deref(),
        Some("dsh: fatal load failure: TypeError [ERR_INVALID_ARG_TYPE]: The \"cwd\" argument must be of type string")
    );
}

/// 纯噪声（只有栈帧/版本尾/壳侧行）→ None：恢复页 reason 保持壳侧原样，
/// 绝不把栈帧或无关行当根因透出。
#[test]
fn summarize_kernel_error_returns_none_on_noise_only() {
    assert_eq!(summarize_kernel_error(""), None);
    assert_eq!(summarize_kernel_error("web-err|     at a.js:1:1\nweb-err| Node.js v24.18.1\n"), None);
    assert_eq!(summarize_kernel_error("内核退出 code=Some(1) 第 2 次\n"), None);
    // throw er 头部行与空壳行不算消息。
    assert_eq!(summarize_kernel_error("web-err| throw er; // Unhandled 'error' event\nweb-err| {\nweb-err| }\n"), None);
}

/// 提取结果截断到 300 字符（错误对象 toString 可能携带超长 URL/参数回显，
/// 恢复页 #why 与事故详情都不该被撑爆）。
#[test]
fn summarize_kernel_error_caps_length() {
    let long = format!("Error: {}\n", "x".repeat(1000));
    let got = summarize_kernel_error(&long).expect("有消息");
    assert_eq!(got.chars().count(), 300);
}

/// sidecar 崩溃死因在 stderr **末尾**（V8 fatal/abort 报告在 sidecar 自身
/// 进度行之后）：必须留尾部而非头部——真机 0xC0000409（logs.zip
/// 2026-08-31）此前 take(6) 只留头部进度行，死因行整段丢失。
#[test]
fn tail_lines_keeps_tail_not_head() {
    let s = concat!(
        "[sidecar] boot 步骤 repair → OK (25ms)\n",
        "[sidecar] boot 步骤 sync → OK (12ms)\n",
        "#\n",
        "# Fatal error in V8: Check failed\n",
        "#\n",
    );
    assert_eq!(
        tail_lines(s, 3),
        "# | # Fatal error in V8: Check failed | #"
    );
}

/// 行数不足全量保留（不 panic 不补齐）；空/纯空白输入 → 空串
/// （调用方据此走 stdout 退路，而不是把「无输出」当内容透出）。
#[test]
fn tail_lines_edges() {
    assert_eq!(tail_lines("a\nb\nc", 10), "a | b | c");
    assert_eq!(tail_lines("", 5), "");
    assert_eq!(tail_lines("\n \n\t\n", 5), "");
}

/// boot stderr 转出：保序、trim、去空行，未超上限就全量——「补丁应用汇总」
/// 与各类自愈行都在中段，按头/尾切片会把它们切掉（这次修的就是这个）。
#[test]
fn sidecar_boot_log_lines_keeps_all_in_order() {
    let got = sidecar_boot_log_lines(
        "[sidecar] boot 步骤 repair → OK (25ms)\n\n   \n[sidecar] 补丁应用汇总: 写入 0 处 / 失配 0 项 / 共 60 项\n  [sidecar] 尾行 \n",
        400,
    );
    assert_eq!(
        got,
        vec![
            "[sidecar] boot 步骤 repair → OK (25ms)".to_string(),
            "[sidecar] 补丁应用汇总: 写入 0 处 / 失配 0 项 / 共 60 项".to_string(),
            "[sidecar] 尾行".to_string(),
        ]
    );
}

/// 超上限：留**末尾**（死因在末尾，与 tail_lines 同判据），且第一行显式注明
/// 截断量——绝不静默丢行（本次修的正是「静默丢整段」）。
#[test]
fn sidecar_boot_log_lines_over_cap_keeps_tail_and_says_so() {
    let s = "L1\nL2\nL3\nL4\n";
    let got = sidecar_boot_log_lines(s, 2);
    assert_eq!(got.len(), 3, "notice + 末尾 2 行");
    assert_eq!(got[1], "L3");
    assert_eq!(got[2], "L4");
    assert!(got[0].contains("共 4 行"), "notice 应注明总行数: {}", got[0]);
    assert!(got[0].contains("末尾 2 行"), "notice 应注明转出量: {}", got[0]);
    assert!(got[0].contains("丢弃前 2 行"), "notice 应注明丢弃量: {}", got[0]);
}

/// 空/纯空白 → 空集：不给 desktop.log 刷空行（也不产生 notice）。
#[test]
fn sidecar_boot_log_lines_empty_is_quiet() {
    assert!(sidecar_boot_log_lines("", 400).is_empty());
    assert!(sidecar_boot_log_lines("\n \n\t\n", 400).is_empty());
}

/// IO 作用域：只认 `mark` 之后追加的本次内核输出——mark 之后无新内容
/// （Node 缺失等未 spawn 即失败）不得引用上一次运行的残留报错（旧根因
/// 安到新失败上）。显式路径 + 纯函数组合，不重定向全局环境（并行套件
/// 下 env 重定向会与 sidecar 集成测试的后台日志中继线程交叉污染）。
#[test]
fn kernel_error_note_scopes_to_log_mark() {
    let home = std::env::temp_dir().join(format!("dsh-sup-kerr-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).expect("建临时目录");
    let log = home.join("dsh-web.log");
    std::fs::write(&log, "web-err| TypeError [ERR_STALE]: previous run residue\n").expect("预写旧内容");
    let mark = std::fs::metadata(&log).unwrap().len();
    // 未记账 / mark 之后无输出 → None。
    assert_eq!(kernel_error_note(None), None, "未记账 → None");
    assert_eq!(read_log_since(&log, mark), None, "不得引用上次运行残留");
    // mark 之后追加本次报错 → 提取本次消息。
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new().append(true).open(&log).unwrap();
    writeln!(f, "web-err| TypeError [ERR_INVALID_ARG_TYPE]: The \"paths[0]\" argument must be of type string").unwrap();
    let tail = read_log_since(&log, mark).expect("应读到本次输出");
    let msg = summarize_kernel_error(&tail).expect("应提取本次报错");
    assert!(msg.contains("ERR_INVALID_ARG_TYPE"), "含根因: {msg}");
    assert!(!msg.contains("ERR_STALE"), "不得混入旧内容: {msg}");
    let note = format!("内核报错：{msg}");
    assert!(note.starts_with("内核报错：TypeError"), "组装形态: {note}");
    let _ = std::fs::remove_dir_all(&home);
}

/// issue #159 假死判定「回合感知」：无进行中回合时达阈值判死（真死兜底），
/// 存在进行中回合时豁免（内核正工作不得误杀）。
#[test]
fn should_restart_zombie_respects_active_turns() {
    // 真死内核（无回合）：达阈值即重启。
    assert!(!Supervisor::should_restart_zombie(19, 0), "未达阈值不重启");
    assert!(Supervisor::should_restart_zombie(20, 0), "达阈值且无回合 → 重启（真死兜底）");
    assert!(Supervisor::should_restart_zombie(25, 0));
    // 进行中回合：即便达阈值也不重启（内核正思考/压缩，HTTP 无响应是预期）。
    assert!(!Supervisor::should_restart_zombie(20, 1), "有进行中回合不得重启");
    assert!(!Supervisor::should_restart_zombie(100, 2), "回合数 >0 恒豁免");
    // 回合结束（0）后恢复判死。
    assert!(Supervisor::should_restart_zombie(20, 0));
}

/// 假死取证采样：自身进程 CPU 时间可采且单调递增（spin 忙等 ≥50ms
/// 后必有增量）；已死 pid → None（探活环不教条重试）。Windows 实测，
/// 其他平台采样原语缺席（取证行自然缺席，不构成失败）。
#[test]
#[cfg(windows)]
fn zombie_cpu_millis_samples_own_process() {
    let a = Supervisor::zombie_cpu_millis(std::process::id()).expect("自身进程 CPU 时间应可采样");
    let start = std::time::Instant::now();
    while start.elapsed() < std::time::Duration::from_millis(60) {
        std::hint::spin_loop();
    }
    let b = Supervisor::zombie_cpu_millis(std::process::id()).expect("二次采样");
    assert!(b > a, "spin 忙等后 CPU 时间必须增加: {a} -> {b}");
    // 几乎不可能存活的 pid（Windows pid 区间外大值）→ 采样失败 None。
    assert_eq!(Supervisor::zombie_cpu_millis(3_999_999), None, "死 pid 不得采出 CPU 时间");
}

/// FILETIME → ms 换算：100ns 单位、高低位拼接。
#[test]
#[cfg(windows)]
fn filetime_ms_conversion() {
    use windows_api::Win32::Foundation::FILETIME;
    let ft = FILETIME { dwLowDateTime: 10_000, dwHighDateTime: 0 }; // 10000×100ns = 1ms
    assert_eq!(filetime_ms(&ft), 1);
    let ft_hi = FILETIME { dwLowDateTime: 0, dwHighDateTime: 1 }; // 2^32×100ns ≈ 429.5s
    assert_eq!(filetime_ms(&ft_hi), 429_496);
}

/// #122/#129 假死形态的确定性验证：TCP 握手被 OS 协议栈代答、应用层
/// 永不响应——http_alive 必须判死（纯 TCP 探测恒活的正是这种形态）。
/// 三态对照：挂死服务器（accept 后不读写）/ 正常 HTTP / 无监听端口。
#[test]
fn http_alive_detects_zombie_vs_live_vs_dead() {
    // ① 假死：accept 但永不响应（事件循环卡死的协议栈镜像）
    let zombie = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let zport = zombie.local_addr().unwrap().port();
    let zthread = std::thread::spawn(move || {
        for stream in zombie.incoming() {
            let _stream: std::net::TcpStream = stream.unwrap();
            std::thread::sleep(std::time::Duration::from_secs(30)); // 持住连接不响应
        }
    });
    // ② 正常：最小 HTTP 响应（404 也是活）
    let live = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let lport = live.local_addr().unwrap().port();
    let lthread = std::thread::spawn(move || {
        for stream in live.incoming() {
            let mut s = stream.unwrap();
            use std::io::{Read, Write};
            let mut buf = [0u8; 128];
            let _ = s.read(&mut buf);
            let _ = s.write_all(b"HTTP/1.1 404 Not Found
Content-Length: 0

");
        }
    });
    std::thread::sleep(std::time::Duration::from_millis(150));
    assert!(!Supervisor::http_alive(zport), "假死（TCP 通、HTTP 永不响应）必须判死——signal timed out 的根形态");
    assert!(Supervisor::http_alive(lport), "正常 HTTP（含 404）必须判活");
    // ③ 端口不存在
    let dead = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let dport = dead.local_addr().unwrap().port();
    drop(dead);
    assert!(!Supervisor::http_alive(dport), "无监听端口必须判死");
    drop(zthread);
    drop(lthread);
}

#[test]
fn probe_http_timeout_applies_60s_startup_grace() {
    assert_eq!(Supervisor::probe_http_timeout(Duration::from_secs(0)), Duration::from_secs(5));
    assert_eq!(Supervisor::probe_http_timeout(Duration::from_secs(59)), Duration::from_secs(5));
    assert_eq!(Supervisor::probe_http_timeout(Duration::from_secs(60)), Duration::from_secs(3));
    assert_eq!(Supervisor::probe_http_timeout(Duration::from_secs(120)), Duration::from_secs(3));
}

#[test]
fn format_zombie_cpu_note_distinguishes_baseline_delta_and_unavailable() {
    let now = std::time::Instant::now();
    // ① 首次成功采样：不再误报「取证不可用」
    let (note1, tag1) = Supervisor::format_zombie_cpu_note(1, None, Some(1200), now);
    assert!(note1.contains("首次基线采样"), "首轮成功采样应明确提示基线采样: {note1}");
    assert!(!note1.contains("取证不可用"), "首轮成功采样不得误报取证不可用: {note1}");
    assert_eq!(tag1.as_deref(), Some("×1 基线采样"));

    // ② 第二轮：有基线且 Δ > 25% → 忙环
    let prev_t = now.checked_sub(Duration::from_millis(3000)).unwrap();
    let (note2, tag2) = Supervisor::format_zombie_cpu_note(2, Some((prev_t, 1200)), Some(3000), now);
    assert!(note2.contains("忙环"), "高 CPU 增量应判忙环: {note2}");
    assert!(tag2.unwrap_or_default().contains("Δ=1800ms/3000ms"));

    // ③ 第二轮：有基线且 Δ 极小 → 阻塞/挂起
    let (note3, _) = Supervisor::format_zombie_cpu_note(3, Some((prev_t, 1200)), Some(1250), now);
    assert!(note3.contains("阻塞/挂起"), "极低 CPU 增量应判阻塞/挂起: {note3}");

    // ④ 采样失败（cur == None）→ 取证不可用
    let (note4, tag4) = Supervisor::format_zombie_cpu_note(1, None, None, now);
    assert!(note4.contains("取证不可用"), "采样失败才报取证不可用: {note4}");
    assert_eq!(tag4.as_deref(), Some("×1 采样缺席"));
}

/// 仓库根定位（与装配层 find_repo_root 同规则）。
fn repo_root() -> Option<std::path::PathBuf> {
    let mut dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for _ in 0..6 {
        if dir.join("dsh-desktop").join("vendor").join("node").exists() {
            return Some(dir);
        }
        if !dir.pop() {
            break;
        }
    }
    None
}

/// 干净临时 home + userData（测试沙箱）。
fn sandbox(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("dsh-tauri-test-{}-{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// 生产 wsl.exe 原语（Node 解析测试构造用：构造期零 wsl.exe 调用，
/// 真实调用仅发生在 WSL boot 分支——本组测试不进入）。
fn real_wsl_invoker() -> Arc<dyn wsl_backend::WslInvoker> {
    Arc::new(wsl_backend::RealWslInvoker) as Arc<dyn wsl_backend::WslInvoker>
}

#[test]
fn kernel_version_from_package_json() {
    let dir = sandbox("ver");
    let pkg_dir = dir.join("node_modules").join("@deepseek-ai").join("dsh");
    std::fs::create_dir_all(&pkg_dir).unwrap();
    std::fs::write(pkg_dir.join("package.json"), r#"{"name":"x","version":"0.1.0-rc.8"}"#).unwrap();
    assert_eq!(read_kernel_version(&dir), "0.1.0-rc.8");
    // 缺文件 / 坏 JSON → unknown（不 panic）。
    assert_eq!(read_kernel_version(&sandbox("ver2")), "unknown");
    let bad = sandbox("ver3");
    std::fs::write(bad.join("package.json"), "not json at all").unwrap();
    assert_eq!(read_kernel_version(&bad), "unknown");
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&bad);
}

#[test]
fn resolve_active_agent_prefers_newer_overlay() {
    let base = sandbox("active-agent");
    let app = base.join("app");
    let ud = base.join("ud");
    let write_pkg = |root: &std::path::Path, ver: &str, with_bin: bool| {
        let pkg = root.join("node_modules").join("@deepseek-ai").join("dsh");
        std::fs::create_dir_all(pkg.join("lib")).unwrap();
        std::fs::write(pkg.join("package.json"), format!(r#"{{"version":"{ver}"}}"#)).unwrap();
        if with_bin {
            std::fs::write(pkg.join("lib").join("bin.js"), "// stub").unwrap();
        }
    };
    write_pkg(&app, "0.2.2-rc.5", true);
    let a0 = resolve_active_agent(&app, &ud);
    assert_eq!(a0.source, "bundled");
    assert_eq!(a0.version, "0.2.2-rc.5");
    write_pkg(&ud.join("agent"), "0.2.2-rc.5", true);
    assert_eq!(resolve_active_agent(&app, &ud).source, "bundled");
    let _ = std::fs::remove_dir_all(ud.join("agent"));
    write_pkg(&ud.join("agent"), "0.2.2", false);
    assert_eq!(resolve_active_agent(&app, &ud).source, "bundled");
    write_pkg(&ud.join("agent"), "0.2.2", true);
    let a1 = resolve_active_agent(&app, &ud);
    assert_eq!(a1.source, "updated");
    assert_eq!(a1.version, "0.2.2");
    assert!(a1.bin_js.starts_with(ud.join("agent")));
    // 崩溃环隔离：agent 目录挪为 agent-broken-* 后自动回落 bundled
    let broken = quarantine_broken_agent_overlay(&ud).unwrap().expect("应隔离 agent 目录");
    assert!(broken.exists() && broken.file_name().unwrap().to_str().unwrap().starts_with("agent-broken-"));
    assert!(!ud.join("agent").exists());
    assert_eq!(resolve_active_agent(&app, &ud).source, "bundled");
    // 二次调用无残留 → Ok(None)
    assert_eq!(quarantine_broken_agent_overlay(&ud).unwrap(), None);
    let _ = std::fs::remove_dir_all(&base);
}

/// Node 三级解析链优先级（v0.5.4 便携版修复 + node 版本适配：装机版
/// vendor 优先口径；vendor 平台主名/备名选择的底层单测在
/// kernel-process::node_resolve，此处验证 supervisor 构造接线）：
/// 健康 vendor 优先 → vendor 坏/缺回落系统 ≥22 → 全缺 None。
#[test]
fn node_resolution_priority_chain() {
    struct StubNodeProbe {
        path_hit: Option<PathBuf>,
        version: Option<String>,
        vendor_version: Option<String>,
    }
    impl kernel_process::NodeProbe for StubNodeProbe {
        fn find_node_in_path(&self) -> Option<PathBuf> {
            self.path_hit.clone()
        }
        fn node_version(&self, exe: &std::path::Path) -> Option<String> {
            // 系统命中路径返回 version；其余（vendor 绝对路径）返回 vendor_version。
            if self.path_hit.as_deref() == Some(exe) {
                self.version.clone()
            } else {
                self.vendor_version.clone()
            }
        }
    }
    let probe = |sys_ver: Option<&str>, vendor_ver: Option<&str>| {
        Arc::new(StubNodeProbe {
            path_hit: Some(PathBuf::from(if cfg!(windows) { r"C:\fake\node.exe" } else { "/fake/node" })),
            version: sys_ver.map(String::from),
            vendor_version: vendor_ver.map(String::from),
        }) as Arc<dyn kernel_process::NodeProbe>
    };
    let mk = |root: &std::path::Path, with_vendor: bool| -> std::path::PathBuf {
        let d = root.to_path_buf();
        if with_vendor {
            std::fs::create_dir_all(d.join("dsh-desktop").join("vendor").join("node")).unwrap();
            let name = if cfg!(windows) { "node.exe" } else { "node" };
            std::fs::write(d.join("dsh-desktop").join("vendor").join("node").join(name), b"").unwrap();
        }
        d
    };

    // ① 装机版主路径：vendor 在位且健康 → 优先 vendor（即便系统也有达标
    //    node），杜绝系统 node 大版本差异（如 node24）击穿内核 ESM loader。
    let root = sandbox("nr1");
    let sv = Supervisor::new_with_probes(&mk(&root, true), real_wsl_invoker(), probe(Some("v24.15.0"), Some("v24.15.0")));
    assert!(matches!(&sv.node_resolved, Some(kernel_process::node_resolve::ResolvedNode::Vendor(_))), "健康 vendor 优先于系统 node");
    assert_eq!(sv.node_exe, sv.node_resolved.as_ref().unwrap().exe(), "node_exe 必须指向命中者");
    // ② vendor 探测不通（被杀软拦到超时）+ 系统达标 → 回落系统 node。
    let root2 = sandbox("nr2");
    let sv2 = Supervisor::new_with_probes(&mk(&root2, true), real_wsl_invoker(), probe(Some("v22.1.0"), None));
    assert!(matches!(&sv2.node_resolved, Some(kernel_process::node_resolve::ResolvedNode::System { major: 22, .. })), "vendor 探测不通须回落系统 node");
    // ③ 便携版缺 vendor：无内置 node → 用系统 node（起死回生主路径）。
    let root3 = sandbox("nr3");
    let sv3 = Supervisor::new_with_probes(&mk(&root3, false), real_wsl_invoker(), probe(Some("v22.1.0"), None));
    assert!(matches!(&sv3.node_resolved, Some(kernel_process::node_resolve::ResolvedNode::System { .. })), "便携版无 vendor 须用系统 node");
    // ④ vendor 在位但探测不通 + 系统过旧：第 1、2 级皆不命中 → 终底返回 vendor。
    let root4b = sandbox("nr4b");
    let sv4b = Supervisor::new_with_probes(&mk(&root4b, true), real_wsl_invoker(), probe(Some("v18.20.4"), None));
    assert!(matches!(&sv4b.node_resolved, Some(kernel_process::node_resolve::ResolvedNode::Vendor(_))), "vendor 坏且系统过旧 → 终底 vendor 保底");
    // ⑤ 便携版缺文件形态：无系统 node 且无 vendor → None + 占位 node_exe
    //    （spawn 失败旧路径终防线在位）。
    let root4 = sandbox("nr4");
    let sv4 = Supervisor::new_with_probes(&mk(&root4, false), real_wsl_invoker(), probe(None, None));
    assert!(sv4.node_resolved.is_none(), "三级链全空 → None");
    let primary = if cfg!(windows) { "node.exe" } else { "node" };
    assert_eq!(sv4.node_exe, root4.join("dsh-desktop").join("vendor").join("node").join(primary), "全缺时占位主名路径");
    for d in [&root, &root2, &root3, &root4, &root4b] {
        let _ = std::fs::remove_dir_all(d);
    }
}

/// Node 全缺的 boot 瀑布首步行为：清晰 BootStep 错误（name=node-resolve）
/// + last_error + 恢复页态——替代旧「sidecar spawn os error 2」。
#[test]
fn node_missing_boot_step_clear_error() {
    struct NoNodeProbe;
    impl kernel_process::NodeProbe for NoNodeProbe {
        fn find_node_in_path(&self) -> Option<PathBuf> { None }
        fn node_version(&self, _exe: &std::path::Path) -> Option<String> { None }
    }
    // 伪仓库根：无 vendor node → 三级链全空（不依赖本机是否装有 node）。
    let root = sandbox("nr-miss");
    std::fs::create_dir_all(root.join("dsh-desktop")).unwrap();
    let sv: Arc<Supervisor> = Arc::new(Supervisor::new_with_probes(
        &root,
        real_wsl_invoker(),
        Arc::new(NoNodeProbe) as Arc<dyn kernel_process::NodeProbe>,
    ));
    assert!(sv.node_resolved.is_none());
    let (tx, rx) = std::sync::mpsc::channel();
    Supervisor::boot_waterfall(Arc::clone(&sv), tx, None);
    // 首个事件即 node-resolve 失败步，错误文案含可操作指引。
    match rx.recv_timeout(Duration::from_secs(10)) {
        Ok(SupervisorEvent::BootStep { name, ok, error, .. }) => {
            assert_eq!(name, "node-resolve");
            assert!(!ok);
            let e = error.expect("缺失错误必须带 error 文案");
            assert!(e.contains("Node"), "文案须指明 Node: {e}");
            assert!(e.contains("vendor/node"), "文案须指出内置路径: {e}");
        }
        other => panic!("首个事件应为 node-resolve 失败步: {other:?}"),
    }
    assert_eq!(sv.state(), RunState::CrashLoop, "全缺必须立即转恢复页态");
    assert!(sv.last_error().is_some_and(|e| e.contains("Node")), "last_error 须记录缺失原因");
    // 随后的事件只能是恢复页信号（CrashLoop），不得再有任何 boot 步
    // （sidecar 五步等——全缺短路）。
    match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(SupervisorEvent::CrashLoop { .. }) => {}
        other => panic!("node-resolve 后应只跟恢复页信号: {other:?}"),
    }
    assert!(rx.try_recv().is_err(), "CrashLoop 后不得再有事件");
    let _ = std::fs::remove_dir_all(&root);
}

/// 功能集成：真机 boot 链（sidecar 五步）在沙箱 home 上执行。
/// 覆盖：Supervisor::run_sidecar_boot（步骤解析 + ok 判定 + 事件转发）。
#[test]
fn sidecar_boot_sandbox_integration() {
    let Some(root) = repo_root() else { eprintln!("[skip] 仓库检出不含 dsh-desktop（CI 无依赖环境）"); return; };
    let _env = crate::logging::ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let home = sandbox("boot");
    std::env::set_var("DSH_HOME", &home);
    std::env::set_var("DSH_TAURI_USERDATA", home.join("ud"));
    let sv = Supervisor::new(&root);
    let (tx, rx) = std::sync::mpsc::channel();
    let result = sv.run_sidecar_boot(&tx, 0);
    std::env::remove_var("DSH_HOME");
    std::env::remove_var("DSH_TAURI_USERDATA");
    assert!(result.is_ok(), "sidecar boot 应成功: {result:?}");
    // 步骤事件按固定顺序全部转发（data-flow.md §3）。
    // 五步契约：compat-pin 为 143fa9e7（compat-layer M1）加入的 fail-closed 步骤；
    // presets 步随 v1.0.0 纯净线（随包 Agent 预设子系统整体移除）退役。
    let names: Vec<String> = rx.iter().map(|e| match e { SupervisorEvent::BootStep { name, .. } => name, _ => String::new() }).take(5).collect();
    assert_eq!(names, vec!["repair", "sync", "patches", "compat-pin", "preflight"], "boot 步骤顺序契约");
    // 沙箱 home 上 profile 结构确已建立（同步器落盘）。
    assert!(home.join("profiles").join("web").join("cordis.patch.yml").exists(), "profile patch 应已建立");
    let _ = std::fs::remove_dir_all(&home);
}

/// 功能集成（真机全链）：boot → 内核 spawn → 就绪行 → TCP 可达 → 关停。
/// 覆盖：spawn_boot / spawn_kernel / ReadyLineParser 接线 / kill_tree / Job Object。
#[test]
fn full_boot_to_kernel_ready_integration() {
    let Some(root) = repo_root() else { eprintln!("[skip] 仓库检出不含 dsh-desktop"); return; };
    let _env = crate::logging::ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let home = sandbox("full");
    std::env::set_var("DSH_HOME", &home);
    std::env::set_var("DSH_TAURI_USERDATA", home.join("ud"));
    let sv: Arc<Supervisor> = Arc::new(Supervisor::new(&root));
    // 版本断言以 pin 为唯一事实源（dsh-desktop/scripts/compat/kernel-pin.json 的
    // kernel.packageVersion）。曾写死前缀 "0.1."（当时为覆盖 rc.8→rc.1 平移），
    // 内核跨到 0.2.0-rc.2 后它以「错误的理由」变红：这里要验的是「payload 版本
    // == pin」这条契约，不是某个具体大版本号。
    let pin_path = root.join("dsh-desktop").join("scripts").join("compat").join("kernel-pin.json");
    let pin_raw = std::fs::read_to_string(&pin_path)
        .unwrap_or_else(|e| panic!("kernel-pin.json 应可读（{}）: {e}", pin_path.display()));
    let pin: serde_json::Value = serde_json::from_str(&pin_raw).expect("kernel-pin.json 应为合法 JSON");
    let want = pin["kernel"]["packageVersion"].as_str().unwrap_or("").to_string();
    assert!(!want.is_empty(), "pin 读不到 kernel.packageVersion: {pin_raw}");
    assert_eq!(sv.kernel_version, want, "payload 内核版本应等于 pin");
    let (tx, rx) = std::sync::mpsc::channel();
    sv.spawn_boot(tx, None);
    // boot（~4s）+ 内核就绪（~6s），150s 兜底；先到的 BootStep 逐条核对。
    let deadline = Instant::now() + Duration::from_secs(150);
    let mut boot_steps: Vec<String> = Vec::new();
    let url = loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left.max(Duration::from_millis(1))) {
            Ok(SupervisorEvent::BootStep { name, ok, .. }) => {
                assert!(ok, "boot 步骤 {name} 不应失败");
                boot_steps.push(name);
            }
            Ok(SupervisorEvent::KernelReady { url, port }) => {
                let ok = std::net::TcpStream::connect_timeout(&format!("127.0.0.1:{port}").parse().unwrap(), Duration::from_secs(3)).is_ok();
                assert!(ok, "就绪端口应可连: {port}");
                break url;
            }
            Ok(other) => panic!("非预期事件: {other:?}"),
            Err(_) => panic!("150s 内未就绪（boot_steps={boot_steps:?}）"),
        }
    };
    assert_eq!(boot_steps, vec!["repair", "sync", "patches", "compat-pin", "preflight"]);
    assert!(url.starts_with("http://127.0.0.1:"), "就绪 URL 形态: {url}");
    assert_eq!(sv.state(), RunState::Ready);
    assert!(sv.kernel_url().is_some());
    // 关停（杀树；Job Object 兜强杀场景由专测覆盖）。
    sv.shutdown();
    std::env::remove_var("DSH_HOME");
    std::env::remove_var("DSH_TAURI_USERDATA");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn generation_increments_on_restart_and_state_transitions() {
    let Some(root) = repo_root() else { eprintln!("[skip]"); return; };
    let sv: Arc<Supervisor> = Arc::new(Supervisor::new(&root));
    let g0 = sv.inner.lock().unwrap_or_else(|p| p.into_inner()).generation;
    sv.set_state_for_test(RunState::Ready);
    assert_eq!(sv.state(), RunState::Ready);
    let (tx, _rx) = std::sync::mpsc::channel();
    sv.restart(tx, None);
    assert_eq!(sv.inner.lock().unwrap_or_else(|p| p.into_inner()).generation, g0 + 1, "restart 应递增代际号");
    sv.shutdown();
    assert!(sv.inner.lock().unwrap_or_else(|p| p.into_inner()).stopping);
    let _ = Ordering::Relaxed;
}

/// #155 EADDRINUSE 崩溃环回归锚点：reuse_or_new_port 不得把忙端口交给内核。
/// 行为测试（真监听占用）：期望端口被占用时返回 OS 随机安全端口（≠占用端口）；
/// 空闲时优先复用期望端口（origin 稳定）。
#[test]
fn reuse_or_new_port_never_returns_busy_port() {
    let Some(root) = repo_root() else { eprintln!("[skip] 仓库检出不含 dsh-desktop"); return; };
    let sv: Arc<Supervisor> = Arc::new(Supervisor::new(&root));
    // ① 空闲端口：优先复用（origin 稳定，localStorage 偏好不丢）。
    let free_port = kernel_process::port::probe_bind(0).expect("OS 分配空闲端口");
    assert_eq!(sv.reuse_or_new_port(free_port), free_port, "空闲端口应被复用");
    // ② 占用端口：必须换新端口（≠ 占用端口），绝不把忙端口交给内核。
    let held = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let busy = held.local_addr().unwrap().port();
    let chosen = sv.reuse_or_new_port(busy);
    assert_ne!(chosen, busy, "忙端口绝不能被选择（EADDRINUSE 秒退 → 崩溃环入口）: busy={busy} chosen={chosen}");
    assert!(kernel_process::port::is_safe_port(chosen), "换新端口必须安全");
    // ③ 不安全端口：不选择。
    let chosen2 = sv.reuse_or_new_port(6666);
    assert_ne!(chosen2, 6666);
    drop(held);
}

/// #155 稳定化等待锚点（形态）：kill_kernel 的 local 分支必须在杀树后等待
/// 端口释放（wait_port_free）；on_kernel_exit 自动重启的 spawn 前必须等待
/// 同一端口释放或换新端口。防回退（revert 到「杀完即 spawn」的竞态形态）。
#[test]
fn port_release_wait_anchors_present() {
    let src = supervisor_all_src();
    let kill_seg = src
        .split("pub fn kill_kernel")
        .nth(1)
        .and_then(|s| s.split("pub fn shutdown").next())
        .expect("kill_kernel 段");
    assert!(kill_seg.contains("wait_port_free"), "kill_kernel local 分支必须等待端口释放");
    assert!(kill_seg.contains("Duration::from_secs(5)"), "杀树后端口释放等待上限 5s");
    let exit_seg = src
        .split("fn on_kernel_exit")
        .nth(1)
        .and_then(|s| s.split("/// 探活循环").next())
        .expect("on_kernel_exit 段");
    let ok_seg = exit_seg
        .split("Verdict::Ok =>")
        .nth(1)
        .and_then(|s| s.split("/// 探活循环").next())
        .expect("自动重启（Verdict::Ok）段");
    assert!(ok_seg.contains("wait_port_free"), "自动重启 spawn 前必须等端口释放");
    assert!(ok_seg.contains("choose_stable_port(None)"), "等不到释放必须换新端口（绝不把忙端口交给内核）");
    let reuse_seg = src
        .split("fn reuse_or_new_port")
        .nth(1)
        .and_then(|s| s.split("/// 拉起内核并同步等待就绪").next())
        .expect("reuse_or_new_port 段");
    assert!(reuse_seg.contains("choose_stable_port(None)"), "复用失败必须回落 OS 随机端口");
    assert!(!reuse_seg.contains("unwrap_or(preferred)"), "旧实现「忙端口兜底」必须移除");
}

/// #155 崩溃环防止的行为测试：内核秒退（EADDRINUSE 形态）时，kill_kernel
/// 的端口释放等待不持锁（wait_port_free 期间 inner 锁已释放）——用
/// 已死端口验证 kill_kernel 幂等安全 + 不阻塞（真机依赖 wait_port_free
/// 轮询，此处验证 kill_kernel 在无内核时零副作用）。
#[test]
fn kill_kernel_no_kernel_is_safe() {
    let Some(root) = repo_root() else { eprintln!("[skip]"); return; };
    let sv: Arc<Supervisor> = Arc::new(Supervisor::new(&root));
    sv.set_state_for_test(RunState::Ready);
    sv.kill_kernel(); // kernel=None：不应 panic / 不应无限阻塞
    assert_eq!(sv.state(), RunState::Ready);
}

/// K3 双 web 实例排查（K2「端口 7388 复用/孤儿内核」）：netstat 监听 PID
/// 提取纯函数——Windows `netstat -ano -p TCP` 行形态
/// `TCP 127.0.0.1:7388 0.0.0.0:0 LISTENING 12345` 正确提取；IPv6 地址、
/// 非 LISTENING、表头、字段不足全部跳过。
#[test]
fn parse_listening_pids_from_netstat_lines() {
    let out = "\n  TCP    127.0.0.1:7388    0.0.0.0:0              LISTENING       12345\n\
               \x20 TCP    [::1]:51731        [::]:0               LISTENING       3344\n\
               \x20 TCP    127.0.0.1:6666    0.0.0.0:0              TIME_WAIT       9876\n\
               \x20 TCP    127.0.0.1:9999    0.0.0.0:0              LISTENING       12ab\n\
               \x20 Proto  Local Address     Foreign Address       State           PID\n";
    assert_eq!(parse_listening_pids(out, 7388), vec![12345], "监听 7388 的 PID 必须提取");
    assert_eq!(parse_listening_pids(out, 51731), vec![3344], "IPv6 [::1]:51731 形态提取");
    assert_eq!(parse_listening_pids(out, 6666), Vec::<u32>::new(), "TIME_WAIT 非 LISTENING 不提取");
    assert_eq!(parse_listening_pids(out, 9999), Vec::<u32>::new(), "PID 非法（12ab）跳过");
    assert_eq!(parse_listening_pids(out, 4444), Vec::<u32>::new(), "无监听行 → 空");
}

/// K3 双 web 实例漏杀点回归锚点（形态）：stdout-EOF 且进程仍存活时
/// spawn_kernel 的收尾必须收割该进程（kill + wait）——此前「Ok(None) →
/// 按退出处理」不杀进程，on_kernel_exit 自动重启的新内核与孤儿并存 =
/// 双 web 实例（端口 7388 复用形态）。防回退（revert 到不收割）。
#[test]
fn stdout_eof_orphan_kernel_is_reaped_shape() {
    let src = supervisor_all_src();
    let seg = src
        .split("// stdout EOF = 进程退出。")
        .nth(1)
        .and_then(|s| s.split("// stderr 收尾线程").next())
        .expect("stdout-EOF 收尾段");
    assert!(
        seg.contains("c.kill()") && seg.contains("c.wait()"),
        "stdout EOF 但进程仍在必须 kill + wait 收割（防孤儿内核 = 双 web 实例）: {seg}"
    );
    assert!(
        seg.contains("已收割防双 web 实例"),
        "收割动作必须留日志（排障取证）: {seg}"
    );
}

/// K3 终态兜底出口接线：restart_kernel_after_renderer_escape 必须 =
/// kill_kernel + on_kernel_exit（崩溃环判定天然限次，与恢复页互斥协同）。
#[test]
fn restart_kernel_after_renderer_escape_anchor() {
    let src = supervisor_all_src();
    let seg = src
        .split("pub fn restart_kernel_after_renderer_escape")
        .nth(1)
        .and_then(|s| s.split("/// 杀内核整树").next())
        .expect("restart_kernel_after_renderer_escape 函数体");
    assert!(seg.contains("self.kill_kernel();"), "必须杀内核整树: {seg}");
    assert!(seg.contains("self.on_kernel_exit(None, tx);"), "必须走 on_kernel_exit 自动重启链: {seg}");
}

/// 看门狗判定表（纯函数全态枚举）：boot 进行态（Boot/Repair/Sync/Patch/
/// Spawn）触发转恢复页；Ready（迟到的正常就绪）与 Recovery（瀑布已自愈）
/// 不触发；stopping（退出路径）压制一切——防退出时误发恢复页事件。
#[test]
fn watchdog_decision_table_all_states() {
    use RunState::*;
    for s in [Boot, Repair, Sync, Patch, Spawn, CrashLoop] {
        assert!(Supervisor::watchdog_should_fire(false, s), "{s:?} 仍卡 boot 链应触发看门狗");
    }
    assert!(!Supervisor::watchdog_should_fire(false, Ready), "迟到的正常就绪不得被看门狗误杀");
    assert!(!Supervisor::watchdog_should_fire(false, Recovery), "已进恢复页不得重复触发");
    for s in [Boot, Spawn, Ready, CrashLoop] {
        assert!(!Supervisor::watchdog_should_fire(true, s), "stopping={s:?} 退出路径压制看门狗");
    }
}

/// v0.5.1「频繁重启 + 白屏」回归锚点组（真机复现定案的四个断流/复活面）。
/// 形态断言法（include_str!），防回退：
///   ① KernelReady 单源 + 热探：全文件只允许一处 send(KernelReady)——
///      在就绪行线程内且前置 http_alive 热探（自动重启路径换页不再竞速
///      HTTP bind → 不再产 chrome-error 白页）；on_boot_success 不得再发。
///   ② 崩溃环 Cooldown 不得自动重启（此前 `_` 兜底臂把恢复页背后的内核
///      反复拉起，页面在恢复页/内核页横跳 = 用户侧「频繁重启」）。
///   ③ 探活环令牌 + CrashLoop/Recovery 退出 + 内核不在不补刀（防旧环
///      复活/误杀自动重启的新内核）。
///   ④ 看门狗代际感知（restart 叠犬不得打断新 boot）。
#[test]
fn regression_v051_restart_whitescreen_anchors() {
    let src = supervisor_all_src();
    // ① KernelReady 单源（拼接构串避免测试字面量自匹配 include_str! 文本）。
    let send_tx2 = ["tx2", ".send(", "SupervisorEvent::KernelReady"].concat();
    let send_tx = ["tx", ".send(", "SupervisorEvent::KernelReady"].concat();
    let sends = src.matches(&send_tx2).count() + src.matches(&send_tx).count();
    assert_eq!(sends, 1, "KernelReady 只允许就绪行线程单点发送（双发=双 navigate/双心跳监测/双诊断探针）: {sends}");
    let ready_seg = src
        .split("if url.is_none() {")
        .nth(1)
        .and_then(|s| s.split("// stdout EOF").next())
        .expect("就绪行线程段");
    let probe_pos = ready_seg.find("Self::http_alive(port)").expect("就绪换页前必须 HTTP 热探");
    let send_pos = ready_seg.find(&send_tx2).expect("就绪换页发送");
    assert!(probe_pos < send_pos, "热探必须先于 KernelReady（防就绪行早于 HTTP bind 的白页竞速）");
    let boot_seg = src
        .split("fn on_boot_success")
        .nth(1)
        .and_then(|s| s.split("/// 拉起内核并同步等待就绪").next())
        .expect("on_boot_success 段");
    assert!(!boot_seg.contains("KernelReady"), "on_boot_success 不得再发 KernelReady（单源在就绪行线程）");
    assert!(!boot_seg.contains("probe_loop"), "探活布防不在此处（就绪行线程统一布防，覆盖自动重启路径）");
    // ② Cooldown 不自动重启。
    let exit_seg = src
        .split("fn on_kernel_exit")
        .nth(1)
        .and_then(|s| s.split("//! 内核探活").next())
        .expect("on_kernel_exit 段");
    assert!(
        exit_seg.contains("Verdict::Tripped | Verdict::Cooldown =>"),
        "Cooldown 必须与 Tripped 同路进恢复页（不得落自动重启）"
    );
    assert!(exit_seg.contains("Verdict::Ok =>"), "仅 Ok 判定可自动重启");
    assert!(!exit_seg.contains("_ =>"), "不得有兜底臂吞掉 Cooldown");
    // ③ 探活环守卫。
    let probe_seg = src
        .split("fn probe_loop")
        .nth(1)
        .and_then(|s| s.split("/// FILETIME").next())
        .expect("probe_loop 段");
    assert!(probe_seg.contains("g.probe_gen != probe_gen"), "探活环必须校验令牌（同代际换内核不得双环）");
    assert!(probe_seg.contains("RunState::CrashLoop"), "崩溃环态必须退出探活（防恢复页期间复活内核）");
    assert!(probe_seg.contains("kernel_present"), "内核已不在时不得补刀记崩溃（防连带杀掉自动重启的新内核）");
    // ④ 看门狗代际。
    let wd_seg = src
        .split("fn spawn_boot_watchdog")
        .nth(1)
        .and_then(|s| s.split("/// 看门狗触发判定").next())
        .expect("spawn_boot_watchdog 段");
    assert!(wd_seg.contains("g.generation != gen"), "看门狗必须代际感知（restart 叠犬不得打断新 boot）");
    // ⑤ 自动重启状态门（VB1 残留缺口收口）：boot 期退出不得走 2s 自动
    //    重启（与瀑布二层 spawn 竞速 = 双内核/端口互踩）；唤醒时必须
    //    复查 state == Ready，Ready 态运行期崩溃的自动重启语义保持不变。
    let ok_seg = exit_seg
        .split("Verdict::Ok =>")
        .nth(1)
        .and_then(|s| s.split("/// 探活循环").next())
        .expect("自动重启（Verdict::Ok）段");
    let wake_seg = ok_seg
        .split("std::thread::sleep(Duration::from_secs(2))")
        .nth(1)
        .and_then(|s| s.split("this.spawn_kernel").next())
        .expect("自动重启线程唤醒段");
    assert!(
        wake_seg.contains("g.state != RunState::Ready"),
        "自动重启线程唤醒时必须复查 Ready 态（boot 期退出归瀑布 ready_tx 路径）: {wake_seg}"
    );
}

/// enter_recovery_tx 幂等锚点：冷却期内后续崩溃不得重发 CrashLoop 事件
/// （事件会再导航恢复页 + 弹系统通知，连环崩溃下发会刷屏）。
#[test]
fn enter_recovery_is_idempotent_while_in_crashloop() {
    let src = supervisor_all_src();
    let seg = src
        .split("fn enter_recovery_tx")
        .nth(1)
        .and_then(|s| s.split("/// 恢复页「重启」").next())
        .expect("enter_recovery_tx 段");
    assert!(seg.contains("already"), "必须判定已在崩溃环态");
    let already_pos = seg.find("if already").expect("already 分支");
    let send_pos = seg.find("tx.send(SupervisorEvent::CrashLoop").expect("CrashLoop 事件发送");
    assert!(already_pos < send_pos, "already 分支必须先于事件发送 return");
}

/// P1-1（V13 审查）回归锚点：on_kernel_exit 置 kernel=None 时必须同步
/// 递增 probe_gen——旧探活环令牌立即失效，不再在「自动重启新内核就绪行
/// 布防前」误杀健康新内核（端口漂移/冷启动 >7s 形态）。
#[test]
fn kernel_exit_revokes_old_probe_gen() {
    let src = supervisor_all_src();
    let exit_seg = src
        .split("fn on_kernel_exit")
        .nth(1)
        .and_then(|s| s.split("/// 探活循环").next())
        .expect("on_kernel_exit 段");
    assert!(exit_seg.contains("g.probe_gen += 1;"), "内核退出必须递增 probe_gen（吊销旧探活环）");
    let kill_seg = src
        .split("fn on_kernel_exit")
        .nth(1)
        .and_then(|s| s.split("/// 探活循环").next())
        .expect("on_kernel_exit 段");
    let none_pos = kill_seg.find("g.kernel = None;").expect("kernel 置空");
    let probe_pos = kill_seg.find("g.probe_gen += 1;").expect("probe_gen 递增");
    assert!(probe_pos > none_pos, "probe_gen 递增必须位于 kernel 置空之后（同锁块内）");
}

/// P2-2（V13 审查）回归锚点：kill_kernel 的 WSL 分支必须先把 child take
/// 到局部并释放 inner 锁再 b.stop()（最长 30s 阻塞不持锁）。
#[test]
fn wsl_kill_releases_lock_before_stop() {
    let src = supervisor_all_src();
    let kill_seg = src
        .split("pub fn kill_kernel")
        .nth(1)
        .and_then(|s| s.split("pub fn shutdown").next())
        .expect("kill_kernel 段");
    // WSL 分支：drop(g) 必须先于 backend.stop()。
    let drop_pos = kill_seg.find("drop(g);").expect("锁释放");
    let stop_pos = kill_seg.find("backend.stop();").expect("WSL stop 调用");
    assert!(drop_pos < stop_pos, "WSL 分支必须先释放锁再 b.stop()（30s 阻塞不持 inner 锁）");
}

/// P2-3（V13 审查）回归锚点：enter_recovery_tx 必须递增 generation——
/// 看门狗/崩溃环开火后，慢 boot 步（AV 拖 300s）返回时 cancelled(gen)
/// 命中代际不符即中止，不再把状态从 CrashLoop 拉回 Ready（页面横跳）。
#[test]
fn enter_recovery_bumps_generation_to_revoke_inflight_waterfall() {
    let src = supervisor_all_src();
    let seg = src
        .split("fn enter_recovery_tx")
        .nth(1)
        .and_then(|s| s.split("/// 恢复页「重启」").next())
        .expect("enter_recovery_tx 段");
    assert!(seg.contains("g.generation += 1;"), "进入恢复页必须递增代际（吊销在途瀑布）");
}

/// 看门狗端到端（短超时注入，生产 300s 参数化）：boot 永挂（卡 Boot 态）
/// → 超时 → CrashLoop 事件 + last_error 带「看门狗」+ 状态落 CrashLoop
/// （恢复页路径）；对照组：已 Ready 的 supervisor 超时后零事件。
#[test]
fn watchdog_short_timeout_routes_stuck_boot_to_recovery() {
    let Some(root) = repo_root() else { eprintln!("[skip] 仓库检出不含 dsh-desktop"); return; };
    // ① 卡 Boot：不跑瀑布，仅让 supervisor 停在初始态。
    let sv: Arc<Supervisor> = Arc::new(Supervisor::new(&root));
    assert_eq!(sv.state(), RunState::Boot);
    let (tx, rx) = std::sync::mpsc::channel();
    Supervisor::spawn_boot_watchdog(&sv, tx, Duration::from_millis(150));
    match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(SupervisorEvent::CrashLoop { .. }) => {}
        other => panic!("boot 永挂应收到 CrashLoop（恢复页路径），得到 {other:?}"),
    }
    assert!(sv.last_error().as_deref().unwrap_or("").contains("看门狗"), "last_error 应指明看门狗超时: {:?}", sv.last_error());
    assert_eq!(sv.state(), RunState::CrashLoop, "状态应落 CrashLoop（换恢复页的壳侧锚点）");
    // ② 已 Ready：超时不打扰（迟到的正常就绪）。
    let sv2: Arc<Supervisor> = Arc::new(Supervisor::new(&root));
    sv2.set_state_for_test(RunState::Ready);
    let (tx2, rx2) = std::sync::mpsc::channel();
    Supervisor::spawn_boot_watchdog(&sv2, tx2, Duration::from_millis(150));
    assert!(rx2.recv_timeout(Duration::from_secs(1)).is_err(), "Ready 态超时后不得有任何事件");
    assert_eq!(sv2.state(), RunState::Ready);
}

// ------------------------------------------------------------------
// WSL 托管模式（契约 wsl-backend.md；形态锁 + 注桩端到端）
// ------------------------------------------------------------------

/// 收割禁令（不变量 §7.2）：supervisor 全文件不得出现发行版级终结命令
/// （wsl 的 terminate / shutdown 全局形态——那会终结整个发行版内用户的
/// 其他进程）。kill_kernel 的 WSL 分支必须是 stop（pid 文件）→ 杀包装
/// child 的次序。（拼接构串避免测试字面量自匹配 include_str! 文本。）
#[test]
fn wsl_harvest_never_terminates_shape() {
    let src = supervisor_all_src();
    let terminate = ["wsl", " --term", "inate"].concat();
    let shutdown = ["wsl", " --shut", "down"].concat();
    assert!(!src.contains(&terminate), "绝不调用发行版级终结命令（契约 §4.6 红线）");
    assert!(!src.contains(&shutdown), "绝不调用发行版级停机命令");
    let seg = src
        .split("pub fn kill_kernel")
        .nth(1)
        .and_then(|s| s.split("pub fn shutdown").next())
        .expect("kill_kernel 段");
    let stop_pos = seg.find("backend.stop();").expect("WSL 分支必须先 stop（WSL 内 pid 文件收割）");
    let kill_pos = seg.find("let _ = c.kill();").expect("再杀 wsl.exe 包装 child");
    assert!(stop_pos < kill_pos, "收割次序：WSL 内 stop 先于杀包装进程");
    // shutdown 的 WSL 分支：stop 必须 fire-and-forget（退出不等 30s 上限）。
    let sd = src.split("pub fn shutdown").nth(1).and_then(|s| s.split("\n    }\n}").next()).expect("shutdown 段");
    assert!(sd.contains("std::thread::spawn(move || {"), "WSL stop 须后台线程 fire-and-forget");
}

/// boot 瀑布 WSL 步序形态（契约 §4.2 顺序红线）：configure → 回落分支 →
/// ensure_installed **先于** sidecar boot（补丁目标含 <UNC>/agent/
/// node_modules）；farm/koffi 跳过分支在场；端口 --port 0；看门狗 35 分钟。
#[test]
fn wsl_boot_waterfall_order_shape() {
    let src = supervisor_all_src();
    let seg = src
        .split("fn boot_waterfall")
        .nth(1)
        .and_then(|s| s.split("fn on_boot_success").next())
        .expect("boot_waterfall 段");
    let ensure_pos = seg.find("run_wsl_ensure_installed(&tx)").expect("ensure_installed 步骤");
    let sidecar_pos = seg.find("run_sidecar_boot(&tx, gen)").expect("sidecar boot 步骤");
    assert!(ensure_pos < sidecar_pos, "ensure_installed 必须先于插件/补丁链（Electron main.js 4957 同序）");
    // farm-repair / koffi 的 WSL 跳过分支。
    assert!(seg.contains("if wsl.is_none() {\n                this.run_farm_repair();"), "farm 修复仅 local 跑");
    assert!(seg.contains("if wsl.is_none() {\n                this.run_koffi_preflight();"), "koffi 预检仅 local 跑");
    // 端口：WSL --port 0。
    assert!(seg.contains("let port = if wsl.is_some() {\n                0u16"), "WSL 端口占位 0（实际端口从就绪行解析）");
    // 看门狗放宽常量存在且被选用。
    assert!(src.contains("BOOT_WATCHDOG_TIMEOUT_WSL: Duration = Duration::from_secs(35 * 60)"));
    assert!(src.contains("BOOT_WATCHDOG_TIMEOUT_WSL\n            } else"), "watchdog 按 WSL 意图放宽");
    // sidecar --home 接线（UNC 写穿）。
    let home_seg = src.split("fn sidecar_home_args").nth(1).and_then(|s| s.split("\n    }").next()).expect("sidecar_home_args 段");
    assert!(home_seg.contains("\"--home\""), "WSL 模式 sidecar 子命令须传 --home <UNC>");
}

/// spawn 形态：WSL 分支走 wsl_spawn_args（严格 argv）、不设 PGID/current_dir/
/// env；就绪行线程内 actual port 提取 + 受限端口收链；local 分支 spec 链不变。
#[test]
fn wsl_spawn_and_actual_port_shape() {
    let src = supervisor_all_src();
    let seg = src
        .split("fn spawn_kernel")
        .nth(1)
        .and_then(|s| s.split("/// 内核退出处理").next())
        .expect("spawn_kernel 段");
    assert!(seg.contains("backend.server_cmd(no_open)"), "WSL 命令串经 wsl-backend spec 构造");
    // 就绪行线程：WSL actual port 提取。
    assert!(seg.contains("url_port(&u)"), "就绪行 URL 提取 actual port（契约 §4.3）");
    assert!(seg.contains("kernel_process::port::is_safe_port(p)"), "受限端口表检查");
    assert!(seg.contains("if wsl_mode {\n                                g.port = Some(port);"), "actual port 写回 Inner.port");
    // local 分支的既有锚点仍在。
    assert!(seg.contains("SpawnSpec::new(&self.node_exe"), "local 分支 SpawnSpec 链不变");
    assert!(seg.contains("set_process_group_leader(&mut cmd)"), "local 分支 PGID 设置不变");
}

/// 孤儿内核记账接线（壳闪退根治）：瀑布最前收割（先于端口选择，孤儿
/// 可能仍占旧端口）；local spawn 后写 pid 记账、WSL 不记；内核收尸与
/// 显式杀树都撤账；收割判定必须带映像复核（pid 复用防护）。
#[test]
fn stale_kernel_accounting_wired_shape() {
    let src = supervisor_all_src();
    let wf = src
        .split("fn boot_waterfall")
        .nth(1)
        .and_then(|s| s.split("fn on_boot_success").next())
        .expect("boot_waterfall 段");
    let harvest = wf.find("this.harvest_stale_kernel()").expect("瀑布起点必须收割孤儿内核");
    let port = wf.find("let port = if wsl.is_some()").expect("端口选择步");
    assert!(harvest < port, "孤儿收割必须先于端口选择（孤儿可能仍占旧端口）");
    let spawn = src
        .split("fn spawn_kernel")
        .nth(1)
        .and_then(|s| s.split("/// 内核退出处理").next())
        .expect("spawn_kernel 段");
    let gate = spawn.find("if !wsl_mode {").expect("记账必须 WSL 门控");
    let write = spawn.find("stale_kernel::write(").expect("spawn 后必须写 pid 记账");
    assert!(gate < write, "WSL 模式不得记账（wsl.exe 不是内核本体）: {spawn}");
    assert!(spawn.contains("pid: child.id()"), "记账必须用真实子进程 pid");
    let exit = src
        .split("fn on_kernel_exit")
        .nth(1)
        .and_then(|s| s.split("pub fn kernel_pid").next())
        .expect("on_kernel_exit 段");
    assert!(
        exit.contains("stale_kernel::remove(&self.pid_file)"),
        "内核收尸后必须撤账: {exit}"
    );
    let kill = src
        .split("pub fn kill_kernel")
        .nth(1)
        .and_then(|s| s.split("pub fn shutdown").next())
        .expect("kill_kernel 段");
    assert!(
        kill.contains("stale_kernel::remove(&self.pid_file)"),
        "显式杀树必须撤账（防下次启动把已死 pid 当孤儿）: {kill}"
    );
    let harvest_fn = src
        .split("fn harvest_stale_kernel")
        .nth(1)
        .and_then(|s| s.split("/// 守护瀑布主体").next())
        .expect("harvest_stale_kernel 段");
    assert!(
        harvest_fn.contains("process_image_matches("),
        "收割必须做进程映像复核（防 pid 复用误杀）: {harvest_fn}"
    );
    assert!(
        harvest_fn.contains("harvest_decision("),
        "收割必须经纯函数判定表: {harvest_fn}"
    );
    assert!(
        harvest_fn.contains("stale_kernel::remove(&self.pid_file)"),
        "无论判定结果都必须清账（坏记账不得跨运行累积）: {harvest_fn}"
    );
}

/// WSL TCP 失联去留判定（探活防误杀核心）：WSL 内进程存活 → 不重启（继续
/// 等待转发恢复，前端重连同一内核）；已退出 → 按退出处理重启。
#[test]
fn decide_wsl_tcp_lost_gate() {
    assert!(Supervisor::decide_wsl_tcp_lost(true), "WSL 内进程存活 → 继续等待，不重启");
    assert!(!Supervisor::decide_wsl_tcp_lost(false), "WSL 内进程已退出 → 按退出处理");
}

/// 崩溃环根治锚点（macOS NODE_OPTIONS 泄漏）：spawn_kernel local 分支必须
/// 经 sanitized_node_command（env_clear + 白名单），内核环境不含
/// NODE_OPTIONS / NODE_REQUIRE / ELECTRON_RUN_AS_NODE / NODE_PATH，只含
/// 白名单（PATH 等）+ 监管标识（DSH_DESKTOP_SUPERVISED / NO_COLOR）。
#[test]
fn spawn_kernel_env_is_sanitized_shape_and_behavior() {
    // ① 形态锚点：防回退到「只挂白名单不 env_clear」的旧内联循环形态。
    let src = supervisor_all_src();
    let seg = src
        .split("fn spawn_kernel")
        .nth(1)
        .and_then(|s| s.split("/// 内核退出处理").next())
        .expect("spawn_kernel 段");
    assert!(seg.contains("sanitized_node_command(&spec.node_exe)"), "spawn_kernel 必须经净化构造: {seg}");
    assert!(!seg.contains("for (k, v) in std::env::vars()"), "不得残留内联白名单循环（旧形态 env 泄漏）: {seg}");
    // ② 行为断言：净化命令 = env_clear + 白名单 + 监管标识，禁漏变量绝不出现。
    let _env = crate::logging::ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    std::env::set_var("NODE_OPTIONS", "--require=/bad/genie-safe-delete.cjs");
    let mut cmd = sanitized_node_command("node");
    cmd.env("DSH_DESKTOP_SUPERVISED", "1").env("NO_COLOR", "1");
    let envs: std::collections::BTreeMap<String, Option<String>> = cmd
        .get_envs()
        .map(|(k, v)| (k.to_string_lossy().into_owned(), v.map(|x| x.to_string_lossy().into_owned())))
        .collect();
    assert!(!envs.contains_key("NODE_OPTIONS"), "NODE_OPTIONS 不得泄漏进内核: {envs:?}");
    assert!(!envs.contains_key("NODE_REQUIRE"), "NODE_REQUIRE 不得泄漏进内核: {envs:?}");
    assert!(!envs.contains_key("ELECTRON_RUN_AS_NODE"));
    assert!(!envs.contains_key("NODE_PATH"));
    assert_eq!(envs.get("DSH_DESKTOP_SUPERVISED").and_then(|v| v.as_deref()), Some("1"), "监管标识必须在场: {envs:?}");
    assert_eq!(envs.get("NO_COLOR").and_then(|v| v.as_deref()), Some("1"), "NO_COLOR 必须在场: {envs:?}");
    // Windows 环境键大小写不敏感且实际键名随来源漂移（系统 "Path" vs
    // 显式注入 "PATH"），白名单透传断言必须按 ASCII 大小写折叠核对。
    assert!(envs.keys().any(|k| k.eq_ignore_ascii_case("PATH")), "白名单 PATH 必须透传: {envs:?}");
    std::env::remove_var("NODE_OPTIONS");
}

/// WinInet 系统代理桥接（beta.3）：ProxyServer 三形态解析 + 用户显式
/// 环境变量优先 + spawn_kernel 注入在位锚点。
#[test]
fn wininet_proxy_bridge_parses_and_respects_user_env() {
    // 解析：全协议单地址 / 分协议 https 优先（空值跳过 http 兑底）/ 已带 scheme / 空。
    assert_eq!(parse_wininet_proxy_url("127.0.0.1:7892").as_deref(), Some("http://127.0.0.1:7892"));
    assert_eq!(
        parse_wininet_proxy_url("ftp=1.1.1.1:21;https=10.0.0.2:8443;http=10.0.0.2:8080").as_deref(),
        Some("http://10.0.0.2:8443")
    );
    assert_eq!(parse_wininet_proxy_url("https=;http=10.0.0.2:8080").as_deref(), Some("http://10.0.0.2:8080"));
    assert_eq!(parse_wininet_proxy_url("http://127.0.0.1:7892").as_deref(), Some("http://127.0.0.1:7892"));
    assert_eq!(parse_wininet_proxy_url(""), None);
    assert_eq!(parse_wininet_proxy_url("   "), None);
    // 注入：无用户代理变量 → 代理对 + NO_PROXY + NODE_USE_ENV_PROXY。
    let none = |_: &str| false;
    let inj = proxy_env_injection("http://127.0.0.1:7892", &none);
    assert!(inj.contains(&("HTTPS_PROXY".to_string(), "http://127.0.0.1:7892".to_string())));
    assert!(inj.contains(&("HTTP_PROXY".to_string(), "http://127.0.0.1:7892".to_string())));
    assert!(inj.contains(&("NO_PROXY".to_string(), "localhost,127.0.0.1,::1".to_string())));
    assert!(inj.contains(&("NODE_USE_ENV_PROXY".to_string(), "1".to_string())));
    // 用户已有 HTTPS_PROXY → 不注入代理对与 NO_PROXY，仍补 NODE_USE_ENV_PROXY。
    let has_https = |k: &str| k.eq_ignore_ascii_case("HTTPS_PROXY");
    let inj2 = proxy_env_injection("http://127.0.0.1:7892", &has_https);
    assert!(inj2.iter().all(|(k, _)| !matches!(k.as_str(), "HTTPS_PROXY" | "HTTP_PROXY" | "NO_PROXY")));
    assert!(inj2.contains(&("NODE_USE_ENV_PROXY".to_string(), "1".to_string())));
    // 显式 NO_PROXY / NODE_USE_ENV_PROXY（含 =0）尊重不覆盖。
    let has_no_proxy = |k: &str| k.eq_ignore_ascii_case("NO_PROXY");
    assert!(proxy_env_injection("http://x:1", &has_no_proxy).iter().all(|(k, _)| k != "NO_PROXY"));
    let has_flag = |k: &str| k.eq_ignore_ascii_case("NODE_USE_ENV_PROXY");
    assert!(proxy_env_injection("http://x:1", &has_flag).iter().all(|(k, _)| k != "NODE_USE_ENV_PROXY"));
    // 形态锚点：spawn_kernel local 分支必须含桥接注入调用（防回退）。
    let src = supervisor_all_src();
    let seg = src
        .split("fn spawn_kernel")
        .nth(1)
        .and_then(|s| s.split("/// 内核退出处理").next())
        .expect("spawn_kernel 段");
    assert!(seg.contains("system_proxy_env_injection()"), "spawn_kernel 必须含 WinInet 桥接注入");
}

/// 形态（v0.5.4 Node 三级解析接线）：构造经 resolve_node_with +
/// existing_vendor_node（非硬编码 vendor 路径）；缺失步 name=node-resolve
/// 且先于 sidecar boot；spawn 仍经 self.node_exe（命中者注入，spawn_kernel
/// local 分支零改动）。
#[test]
fn node_resolution_wiring_shape() {
    let src = supervisor_all_src();
    let ctor = src
        .split("fn new_with_probes")
        .nth(1)
        .and_then(|s| s.split("pub fn state(").next())
        .expect("new_with_probes 段");
    assert!(ctor.contains("kernel_process::node_resolve::resolve_node_with"), "构造必须经三级解析链");
    assert!(ctor.contains("kernel_process::node_resolve::existing_vendor_node"), "vendor 保底经在位判定");
    let wf = src
        .split("fn boot_waterfall")
        .nth(1)
        .and_then(|s| s.split("fn on_boot_success").next())
        .expect("boot_waterfall 段");
    let node_pos = wf.find("\"node-resolve\"").expect("缺失步 name=node-resolve");
    let wsl_pos = wf.find("wsl_configure_or_fallback(&this)").expect("WSL configure 步");
    let sidecar_pos = wf.find("run_sidecar_boot(&tx, gen)").expect("sidecar boot 步");
    assert!(node_pos < wsl_pos && node_pos < sidecar_pos, "node 预检必须先于 WSL configure 与 sidecar boot（sidecar 也依赖 Windows node）");
    assert!(wf[node_pos..].contains("enter_recovery"), "全缺须转恢复页");
}

/// koffi-preflight 通过判定契约（X2 指出的形态修正）：末行 JSON `ok==true`
/// 语义——逐字 `{"ok":true}`（WSL 跳过形态）与 `{"ok":true,"skipped":
/// "no-script"}`（脚本缺失形态）都过；`{"ok":false}`/非 JSON 不过。
#[test]
fn koffi_preflight_passed_json_contract() {
    assert!(koffi_preflight_passed("{\"ok\":true}\n"));
    assert!(koffi_preflight_passed("日志行\n{\"ok\":true,\"skipped\":\"no-script\"}\n"), "脚本缺失跳过形态必须判过（旧 ends_with 契约漏判）");
    assert!(!koffi_preflight_passed("{\"ok\":false}\n"));
    assert!(!koffi_preflight_passed("not json\n"));
    assert!(!koffi_preflight_passed(""));
}

/// C2b 形态锚点：假死重启同计——probe_loop 的三条受控重启路径（TCP 失联
/// ×3 / 假死 ×20 / 假死有回合豁免超限强制重启）必须经 on_kernel_exit(None)
/// 走同一崩溃环判定（含 C2a 慢环计数），不得绕开计数直接杀进程拉起。
#[test]
fn zombie_restart_shares_crash_loop_counter_shape() {
    let src = supervisor_all_src();
    let seg = src
        .split("fn probe_loop")
        .nth(1)
        .and_then(|s| s.split("/// 原地重启").next())
        .expect("probe_loop 段");
    let calls = seg.matches("this.on_kernel_exit(None, &tx);").count();
    assert_eq!(calls, 3, "假死（zombie≥20）与端口失联（consecutive≥3）与假死豁免超限三条路径都必须经 on_kernel_exit（C2b 同计）: {calls}");
    // on_kernel_exit 内部经 record_crash（计数判据入口）。
    let exit_seg = src
        .split("fn on_kernel_exit")
        .nth(1)
        .and_then(|s| s.split("/// 探活循环").next())
        .expect("on_kernel_exit 段");
    assert!(exit_seg.contains("g.crash.record_crash(now)"), "退出判定必须经 CrashLoopDetector（快环窗口 + C2a 慢环计数同源）");
}

/// 就绪行 URL 端口提取形态。
#[test]
fn url_port_forms() {
    assert_eq!(url_port("http://127.0.0.1:51731"), Some(51731));
    assert_eq!(url_port("http://127.0.0.1:51731/token"), Some(51731));
    assert_eq!(url_port("https://host:80"), Some(80));
    assert_eq!(url_port("http://host"), None, "无端口 → None（按拉起失败收链）");
    assert_eq!(url_port("http://host:0"), Some(0), "0 可解析（受限表外，由 is_safe 放行——OS 已分配不可能为 0）");
}

/// WSL 注桩端到端（Windows + 仓库检出；本机 WSL VM 损坏——wsl.exe 全链
/// 经 StubInvoker 桩替身，UNC 用本地目录模拟形态，与 cli.test.js 的
/// DSH_TAURI_WSL_UNC_HOME 手法一致）：settings backend=wsl → configure →
/// ensure_installed（npm 桩）→ sidecar boot（--home，真 node 五步）→
/// spawn（桩 echo 就绪行）→ actual port 换页事件 → Ready → 收割。
#[test]
#[cfg(windows)]
fn wsl_stub_boot_to_kernel_ready_e2e() {
    use std::sync::Mutex as StdMutex;
    let Some(root) = repo_root() else { eprintln!("[skip] 仓库检出不含 dsh-desktop"); return; };
    let _env = crate::logging::ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let home = sandbox("wsl-e2e");
    let unc_home = home.join("unc-home"); // 模拟 \\wsl.localhost\<distro>\... 形态的落点目录
    std::fs::create_dir_all(&unc_home).unwrap();
    // settings：backend=wsl（distro/installDir 留空——configure 走探测链）。
    std::env::set_var("DSH_TEST_HOME", &home);
    std::env::set_var("DSH_TAURI_USERDATA", home.join("ud"));
    let store = shell_core::SettingsStore::new(shell_core::DshPaths::resolve().settings);
    store.set("backend", serde_json::json!("wsl")).unwrap();
    // 覆盖缝：distro/home/UNC 全注入（supervisor 与 sidecar 两半边同键）。
    std::env::set_var("DSH_TAURI_WSL_DISTRO", "Ubuntu-22.04");
    std::env::set_var("DSH_TAURI_WSL_HOME", "/home/tester");
    std::env::set_var("DSH_TAURI_WSL_UNC_HOME", &unc_home);

    /// wsl.exe 桩：探测/安装全脚本化；spawn 用 cmd echo 就绪行 + ping 保活。
    struct WslStub {
        calls: StdMutex<Vec<String>>,
    }
    impl wsl_backend::WslInvoker for WslStub {
        fn run_with_lines(
            &self,
            _distro: &str,
            cmd: &str,
            _timeout: Duration,
            _on_line: &mut (dyn FnMut(&str) + Send),
        ) -> wsl_backend::WslRunResult {
            self.calls.lock().unwrap_or_else(|p| p.into_inner()).push(cmd.to_string());
            let (stdout, code) = if cmd.contains("printf %s \"$HOME\"") {
                ("/home/tester\n".to_string(), 0)
            } else if cmd.contains("node --version") {
                ("v20.11.0\n".to_string(), 0)
            } else if cmd.contains("npm --version") {
                ("10.2.4\n".to_string(), 0)
            } else if cmd.contains("npm install") {
                ("...npm 进度行...\nWSL_INSTALL_OK\n".to_string(), 0)
            } else if cmd.contains("mkdir -p") {
                (String::new(), 0)
            } else if cmd.contains("test -f") {
                (String::new(), 1) // agent 未就绪 → 走安装分支
            } else if cmd.starts_with("p=") {
                ("ok\n".to_string(), 0) // stop_cmd（收割）
            } else {
                (String::new(), 0)
            };
            wsl_backend::WslRunResult { ok: code == 0, code, timed_out: false, stdout, stderr: String::new() }
        }
        fn list_distros(&self) -> Vec<String> {
            vec!["Ubuntu-22.04".into()]
        }
        fn spawn_server(&self, _distro: &str, _cmd: &str) -> std::io::Result<Child> {
            // ping 寿命 300s：必须显著大于热探最坏耗时（50×2.1s≈105s，
            // 防火墙把回环拒绝慢速化时 connect_timeout 每次等满）+ 测试
            // 断言窗——此前 90s 在慢拒绝环境下被热探吃光，kill_kernel 时
            // stub 已自然退出，收割断言必败（环境态竞态，非产品回归）。
            let mut c = Command::new("cmd");
            c.args(["/d", "/c", "echo dsh web: http://127.0.0.1:39517 & ping -n 300 127.0.0.1 > nul"]);
            c.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
            c.creation_flags_win(); // WinFlags trait 经 use super::* 可见
            c.spawn()
        }
    }

    let stub = Arc::new(WslStub { calls: StdMutex::new(Vec::new()) });
    let sv: Arc<Supervisor> = Arc::new(Supervisor::new_with_wsl_invoker(
        &root,
        stub.clone() as Arc<dyn wsl_backend::WslInvoker>,
    ));
    assert!(sv.wsl_cfg.is_some(), "settings backend=wsl 应解析为 WSL 意图");
    let (tx, rx) = std::sync::mpsc::channel();
    sv.spawn_boot(tx, None);
    // 兜底窗口 900s：本用例在 workspace 全量并行下实测偶发超 150s
    // （image/logging 等 env 关键区测试同进程抢 CPU），非产品缺陷。
    // 2026-11-07 内核换代 0.2.1-rc.4 → 0.2.2-rc.1 后放宽：随包家族 340 → 364 个包，
    // 真机 WSL（Ubuntu-24.04，profile 在 \\wsl.localhost UNC 上）实测 repair 54.7s +
    // sync 182.5s 就把 300s 吃掉，五步虽全过但 KernelReady 未到点即判红——
    // 这是 UNC 同步体量，不是契约或逻辑变化。
    let deadline = Instant::now() + Duration::from_secs(900);
    let mut saw_install = false;
    let mut boot_steps: Vec<String> = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(Instant::now()).max(Duration::from_millis(1));
        match rx.recv_timeout(left) {
            Ok(SupervisorEvent::BootStep { name, ok, .. }) => {
                assert!(ok, "boot 步骤 {name} 不应失败");
                if name == "wsl-install" { saw_install = true; }
                if !name.starts_with("wsl-") { boot_steps.push(name); }
            }
            Ok(SupervisorEvent::KernelReady { url, port }) => {
                assert!(url.starts_with("http://127.0.0.1:"), "就绪 URL 形态: {url}");
                assert_eq!(port, 39517, "actual port 必须取就绪行解析值（spawn 传入 0）: {port}");
                break;
            }
            Ok(SupervisorEvent::CrashLoop { .. }) => panic!("WSL 注桩链不应进恢复页"),
            Ok(other) => { let _ = other; }
            Err(_) => panic!("900s 内未就绪（boot_steps={boot_steps:?} saw_install={saw_install}）"),
        }
    }
    // 链路断言：五步全过（compat-pin 为 143fa9e7 加入）+ 安装步在场 + 运行态就绪 + actual port 落 Inner。
    assert_eq!(boot_steps, vec!["repair", "sync", "patches", "compat-pin", "preflight"], "sidecar 五步契约（经 --home UNC）");
    assert!(saw_install, "agent 未就绪应触发 wsl-install 步（BootStep 进度上报）");
    assert!(sv.wsl_active().is_some(), "configure 成功后 WSL 运行态生效");
    assert_eq!(sv.backend_effective(), "wsl");
    assert_eq!(sv.fallback_reason(), "", "无回落");
    assert_eq!(sv.inner.lock().unwrap_or_else(|p| p.into_inner()).port, Some(39517), "Inner.port = actual port");
    assert_eq!(sv.effective_home(), unc_home, "effective_home = UNC（模拟目录形态）");
    // 等状态落 Ready（on_boot_success 在 KernelReady 同线程近旁）。
    for _ in 0..50 {
        if sv.state() == RunState::Ready { break; }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(sv.state(), RunState::Ready);
    // 收割：kill_kernel 应发 stop_cmd（pid 文件）+ 杀包装 child。
    sv.kill_kernel();
    let calls = stub.calls.lock().unwrap_or_else(|p| p.into_inner()).clone();
    assert!(
        calls.iter().any(|c| c.starts_with("p=") && c.contains("kill $(cat")),
        "kill_kernel 的 WSL 分支必须发 pid 文件收割命令: {calls:?}"
    );
    let terminate = ["--term", "inate"].concat();
    assert!(!calls.iter().any(|c| c.contains(&terminate)), "绝不调用发行版级终结命令");
    sv.shutdown();
    // 清环境。
    for k in ["DSH_TAURI_WSL_DISTRO", "DSH_TAURI_WSL_HOME", "DSH_TAURI_WSL_UNC_HOME", "DSH_TEST_HOME", "DSH_TAURI_USERDATA"] {
        std::env::remove_var(k);
    }
    let _ = std::fs::remove_dir_all(&home);
}
