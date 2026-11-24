//! 内核探活、假死取证（CPU 增量采样）与启动期宽限策略。

use super::*;

impl Supervisor {
    /// 内核进程 pid（local 模式；WSL 模式/未启动 → None——WSL 内核不在
    /// Windows 进程表，对 wsl.exe 包装进程采样无意义）。供假死取证采样。
    pub fn kernel_pid(&self) -> Option<u32> {
        if self.wsl_active().is_some() {
            return None;
        }
        let g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        g.kernel.as_ref().map(|c| c.id())
    }

    /// 内核进程 CPU 时间（用户+内核态，毫秒）——假死取证采样原语。
    /// Windows：`OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)` +
    /// `GetProcessTimes`；其他平台 v1 不支持（返回 None，取证行自然缺席）。
    /// 进程已退出/权限不足 → None（探活环自会走退出处理，不教条重试）。
    pub(super) fn zombie_cpu_millis(pid: u32) -> Option<u64> {
        #[cfg(windows)]
        {
            use windows_api::Win32::Foundation::{CloseHandle, FILETIME};
            use windows_api::Win32::System::Threading::{GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
            unsafe {
                let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
                let mut creation = FILETIME::default();
                let mut exit = FILETIME::default();
                let mut kernel = FILETIME::default();
                let mut user = FILETIME::default();
                let ok = GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user);
                let _ = CloseHandle(handle);
                // windows 0.61：GetProcessTimes 返回 Result<(), Error>。
                if ok.is_err() {
                    return None;
                }
                Some(filetime_ms(&kernel).saturating_add(filetime_ms(&user)))
            }
        }
        #[cfg(not(windows))]
        {
            let _ = pid;
            None
        }
    }

    /// 探活循环：TCP connect + 就绪超时。
    /// HTTP 应用层探活：读到任何响应字节（含 404/401——内核对 / 至少回
    /// index/错误页）即证明事件循环在转。TCP 握手由 OS 协议栈完成，进程
    /// 按探测启动后经过时长决定单次 HTTP 探活 I/O 超时：前 60s 启动宽限期 5s，之后 3s。
    pub(super) fn probe_http_timeout(elapsed: Duration) -> Duration {
        if elapsed < PROBE_GRACE_WINDOW {
            PROBE_GRACE_HTTP_TIMEOUT
        } else {
            PROBE_NORMAL_HTTP_TIMEOUT
        }
    }

    /// 假死时也恒成功——必须发请求读响应才能区分（issue #122/#129）。
    pub(super) fn http_alive(port: u16) -> bool {
        Self::http_alive_with_timeout(port, PROBE_NORMAL_HTTP_TIMEOUT)
    }

    /// 可配置 I/O 超时的 HTTP 探活（启动宽限期 5s / 常态 3s）。
    pub(super) fn http_alive_with_timeout(port: u16, io_timeout: Duration) -> bool {
        use std::io::{Read, Write};
        let Ok(addr) = format!("127.0.0.1:{port}").parse() else { return false };
        let Ok(mut s) = std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(2)) else {
            return false;
        };
        let _ = s.set_read_timeout(Some(io_timeout));
        let _ = s.set_write_timeout(Some(io_timeout));
        if s.write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n").is_err() {
            return false;
        }
        let mut buf = [0u8; 16];
        matches!(s.read(&mut buf), Ok(n) if n > 0)
    }

    /// 假死取证 CPU 文案与序列标签构造（纯函数，可单测）：
    /// 区分「首次基线采样」「增量定性（忙环/阻塞）」「真正采样失败」三种状态，
    /// 修复此前 ×1 首轮必把成功基线误报为「取证不可用」的日志缺陷。
    pub(super) fn format_zombie_cpu_note(
        zombie: usize,
        last_cpu: Option<(std::time::Instant, u64)>,
        cur: Option<u64>,
        now: std::time::Instant,
    ) -> (String, Option<String>) {
        match (last_cpu, cur) {
            (Some((t, prev)), Some(c)) => {
                let win_ms = now.duration_since(t).as_millis() as u64;
                let d = c.saturating_sub(prev);
                if win_ms > 0 {
                    let verdict = if d * 4 > win_ms {
                        "忙环（事件循环被同步长任务独占）"
                    } else {
                        "阻塞/挂起（CPU 几乎无消耗）"
                    };
                    (
                        format!("（取证：{win_ms}ms 窗口 CPU Δ={d}ms，{verdict}）"),
                        Some(format!("×{zombie} Δ={d}ms/{win_ms}ms {verdict}")),
                    )
                } else {
                    (String::new(), None)
                }
            }
            (None, Some(_)) => (
                "（首次基线采样，下轮起计算 CPU 增量）".to_string(),
                Some(format!("×{zombie} 基线采样")),
            ),
            (_, None) => (
                "（取证不可用：非 Windows/WSL 模式/采样失败）".to_string(),
                Some(format!("×{zombie} 采样缺席")),
            ),
        }
    }

    /// 假死受控重启判定（纯函数，可单测）：HTTP 无响应连续次数达阈值且**无**
    /// 进行中 agent 回合才重启。存在进行中回合时（内核正思考/压缩、事件循环
    /// 被占导致 HTTP 无响应）豁免——issue #159 内核被强杀根因。真死内核（无
    /// 回合）仍按原阈值受控重启，兜底不破坏。
    pub(super) fn should_restart_zombie(zombie: usize, active_turns: u64) -> bool {
        zombie >= ZOMBIE_THRESHOLD && active_turns == 0
    }

    /// WSL 模式 TCP 连续失联达阈值时的处置决策（纯函数，可单测）：入参为「WSL 内
    /// 内核进程是否存活」探测结果。存活 → true（继续等待 localhost 转发恢复、
    /// **不重启**，给前端 reconnect 留住同一内核与它的 durable event）；已退出 →
    /// false（按原退出处理链重启）。非 WSL 模式不调用本函数（TCP 失联 == 进程真死）。
    pub(super) fn decide_wsl_tcp_lost(wsl_kernel_alive: bool) -> bool {
        wsl_kernel_alive
    }

    pub(super) fn probe_loop(self: &Arc<Self>, port: u16, tx: Sender<SupervisorEvent>, gen: u64, probe_gen: u64) {
        let this = Arc::clone(self);
        std::thread::spawn(move || {
            // 就绪后失联分两形态（#122 假死定性）：
            //   a) TCP 连不上（进程死/端口死）→ 连续 3 次按退出处理（原语义）；
            //   b) TCP 通但 HTTP 连续无响应 → 假死，连续 20 次（~60s）受控重启。
            //      阈值从 5(15s) 提到 20(60s)：用户实测内核做上下文压缩/LSTM 推理
            //      时事件循环被占 20-30s——15s 会误杀正在工作的内核（导致
            //      "signal time out" + "中断不了" + 频繁压缩的恶性循环）。
            //      走 on_kernel_exit 的崩溃环窗口限次（天然防死循环）。
            //
            // 退出条件（v0.5.1 频繁重启回归修复）：
            //   - probe_gen 令牌不符（新一代内核已布防，本环属上一代内核）；
            //   - 状态进 CrashLoop/Recovery：崩溃环/恢复页期间不得继续探活——
            //     此前探活环在恢复页期间继续探死端口，3 次失联后 on_kernel_exit
            //     会把崩溃自动重启刚拉起的新内核一并杀掉（复活-再杀循环）。
            let probe_started = std::time::Instant::now();
            let mut consecutive = 0usize;
            let mut zombie = 0usize;
            let mut defer = 0usize; // 连续「有回合但 HTTP 无响应」的阈值窗口数（stale 兜底，见 ZOMBIE_DEFER_MAX）
            // 假死取证基线（v0.6.3）：上一 tick 的 (时刻, 内核进程 CPU 毫秒)。
            // 健康 tick 复位——跨间隙的陈旧基线会把休眠窗口的 Δ 算进来误导定性。
            let mut last_cpu: Option<(std::time::Instant, u64)> = None;
            // 取证序列（最近 6 条短标签）：假死强杀时随 incident 落盘，
            // 用户反馈一份事故报告即携带完整定性证据链。
            let mut cpu_series: std::collections::VecDeque<String> = std::collections::VecDeque::new();
            loop {
                std::thread::sleep(Duration::from_secs(3));
                {
                    let g = this.inner.lock().unwrap_or_else(|p| p.into_inner());
                    if g.stopping || g.generation != gen || g.probe_gen != probe_gen {
                        return;
                    }
                    if g.state == RunState::Recovery || g.state == RunState::CrashLoop {
                        return;
                    }
                }
                // SocketAddr::from(([u8;4], u16)) 是全函数——等价于
                // "127.0.0.1:{port}" 解析成功路径，但不留生产 unwrap。
                let tcp_ok = std::net::TcpStream::connect_timeout(
                    &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
                    Duration::from_secs(2),
                )
                .is_ok();
                let http_timeout = Self::probe_http_timeout(probe_started.elapsed());
                if tcp_ok && Self::http_alive_with_timeout(port, http_timeout) {
                    consecutive = 0;
                    zombie = 0;
                    defer = 0;
                    last_cpu = None;
                    cpu_series.clear();
                    continue;
                }
                if !tcp_ok {
                    zombie = 0;
                    defer = 0;
                    consecutive += 1;
                    let _ = tx.send(SupervisorEvent::ProbeFailed { consecutive });
                    if consecutive >= 3 {
                        // 端口连续失联但进程可能还活着：杀掉按退出处理。
                        // 内核已不在（退出处理链已接管：自动重启或恢复页）时
                        // 不得再记一次崩溃——那会把后续自动重启的新内核当作
                        // 本次失败连带处理。
                        let kernel_present = {
                            let g = this.inner.lock().unwrap_or_else(|p| p.into_inner());
                            g.kernel.is_some()
                        };
                        if !kernel_present {
                            return;
                        }
                        // WSL 模式专属防误杀：连不上 127.0.0.1:port 常常只是 WSL2
                        // localhost 转发抖动（睡眠唤醒 / 网络切换 / 虚拟网卡重置），
                        // WSL 内内核进程其实还活着。若照 local 语义 kill + 重启，会
                        // 换上全新空内核——旧会话的流式输出彻底丢失，且前端 reconnect
                        // 只连到新进程（durable event 随旧进程消失，续不上）。故先探
                        // WSL 内进程：仍在 → 不重启、复位失联计数继续等转发恢复（前端
                        // 自会重连同一内核续流）；确认没了 → 按原退出处理重启。
                        if let Some(backend) = this.wsl_active() {
                            if Self::decide_wsl_tcp_lost(backend.is_server_alive()) {
                                log_line("WSL：TCP 连续失联但 WSL 内内核进程存活（疑 localhost 转发抖动）——不重启，等待转发恢复（前端将重连同一内核）");
                                consecutive = 0;
                                continue;
                            }
                            log_line("WSL：TCP 连续失联且 WSL 内内核进程已退出，按内核退出处理");
                        }
                        this.kill_kernel();
                        this.on_kernel_exit(None, &tx);
                        return;
                    }
                    continue;
                }
                // TCP 通、HTTP 无响应：假死形态。
                zombie += 1;
                let _ = tx.send(SupervisorEvent::ZombieSuspect { consecutive: zombie });
                // 假死取证（v0.6.3）：每 tick 采样内核进程 CPU 时间增量——
                // 忙环形态（同步 JS 长任务/上下文压缩独占事件循环）Δ≈窗口
                // 时长；阻塞/挂起形态 Δ≈0。v0.6.2 用户实爆（×1..×7 反复重置
                // 后 kill -1）只有「无响应」一条线索，取证三件套之 CPU 采样
                // 补齐定性依据（另两件：stdout 停点不可信已由 HTTP 探活覆盖、
                // 文件 dump 归事故报告链）。
                let cpu_note = {
                    let now = std::time::Instant::now();
                    let cur = this.kernel_pid().and_then(Supervisor::zombie_cpu_millis);
                    let (note, tag) = Self::format_zombie_cpu_note(zombie, last_cpu, cur, now);
                    last_cpu = cur.map(|c| (now, c));
                    if let Some(t) = tag {
                        while cpu_series.len() >= 6 {
                            cpu_series.pop_front();
                        }
                        cpu_series.push_back(t);
                    }
                    note
                };
                log_line(&format!("内核假死可疑（端口通、HTTP 无响应）×{zombie}{cpu_note}"));
                if Supervisor::should_restart_zombie(zombie, crate::session_notify::active_turns()) {
                    log_line("内核假死判定成立（连续 60s HTTP 无响应，20×3s 探活），受控重启");
                    // 事故报告：携 CPU 定性证据链（忙环→内核侧同步长任务；阻塞→
                    // 锁/等待），用户反馈一份即够——不再需要现场翻 desktop.log。
                    this.guard_incident(
                        "zombie-restart",
                        &format!(
                            "内核假死受控重启（连续 60s HTTP 无响应；进行中 agent 回合 {} 个）。\nCPU 取证近况：{}\n完整时序见 desktop.log（假死可疑条目），内核输出见 dsh-web.log 尾部。",
                            crate::session_notify::active_turns(),
                            if cpu_series.is_empty() { "（缺席：非 Windows/WSL 模式）".to_string() } else { cpu_series.iter().cloned().collect::<Vec<_>>().join("；") }
                        ),
                    );
                    this.kill_kernel();
                    this.on_kernel_exit(None, &tx);
                    return;
                }
                if zombie >= ZOMBIE_THRESHOLD {
                    // 达阈值但存在进行中回合（内核正思考/压缩，事件循环被占 →
                    // HTTP 无响应是预期）：不判死，复位计数继续观察——回合结束后
                    // 若仍无响应，下一轮 20 次再判真死（issue #159）。defer 计数
                    // 封顶：回合信号滞留（watcher 崩溃/日志损坏）也不让真死内核
                    // 永远逃过假死重启兜底。
                    defer += 1;
                    if defer >= ZOMBIE_DEFER_MAX {
                        log_line("内核假死且持续无响应（回合信号可能失效），强制受控重启");
                        this.guard_incident(
                            "zombie-restart-deferred",
                            &format!(
                                "内核假死强制受控重启（{ZOMBIE_DEFER_MAX} 个阈值窗口无响应且回合信号滞留，进行中回合计数 {}）。\nCPU 取证近况：{}\n若取证呈忙环：回合真实仍在跑（长压缩/推理），可考虑调大 ZOMBIE_DEFER_MAX；若呈阻塞：回合信号已失效，请反馈 dsh-web.log 尾部。",
                                crate::session_notify::active_turns(),
                                if cpu_series.is_empty() { "（缺席：非 Windows/WSL 模式）".to_string() } else { cpu_series.iter().cloned().collect::<Vec<_>>().join("；") }
                            ),
                        );
                        this.kill_kernel();
                        this.on_kernel_exit(None, &tx);
                        return;
                    }
                    log_line("内核假死可疑但存在进行中 agent 回合，延迟重启（复位计数继续观察）");
                    zombie = 0;
                }
            }
        });
    }
}

/// FILETIME（100ns 单位）→ 毫秒（假死取证采样原语的换算半边）。
#[cfg(windows)]
pub(super) fn filetime_ms(ft: &windows_api::Win32::Foundation::FILETIME) -> u64 {
    let raw = ((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64;
    raw / 10_000
}
