//! 内核启动瀑布（守护快照 / WSL 探测安装 / sidecar boot / spawn_kernel / 退出崩溃环）。

use super::*;

impl Supervisor {
    /// 完整启动链（后台线程跑；事件经 tx 推送）。
    ///
    /// **守护瀑布**（对齐 Electron plugin-guard guardedBoot——「坏插件也永远能打开 dsh」）：
    /// ```text
    /// guard-snapshot → 首次拉起(120s) ─成功→ 换页 + 稳定落定
    ///        └失败→ 体检修复(repair) + safe-overlay 禁用坏插件 → 二次拉起(90s)
    ///                └失败→ 回滚最后良好快照(restore) → 三次拉起(90s)
    ///                        └失败→ 事故报告 + 恢复页（restart_service/恢复页重启全链重走瀑布）
    /// ```
    pub fn spawn_boot(self: &Arc<Self>, tx: Sender<SupervisorEvent>, preferred_port: Option<u16>) {
        let this = Arc::clone(self);
        let tx2 = tx.clone();
        std::thread::spawn(move || {
            // 看门狗（D2 永挂形态根治）：boot 全链有界，超时进恢复页——防
            // vendor node 被 AV 拦到半死导致 loading 永挂。WSL 分支放宽到 35
            // 分钟（npm 首装 30 分钟上限 + boot 链余量，契约 §4.2）。
            let wd_timeout = if this.wsl_cfg.as_ref().is_some_and(|c| c.backend == "wsl") {
                BOOT_WATCHDOG_TIMEOUT_WSL
            } else {
                BOOT_WATCHDOG_TIMEOUT
            };
            Self::spawn_boot_watchdog(&this, tx.clone(), wd_timeout);
            // panic 隔离：瀑布任何一环意外 panic（兼容性场景的兜底）→ 落恢复页，
            // 客户端继续运行（全局 panic hook 已另行落盘 panics.log）。
            let this2 = Arc::clone(&this);
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                Self::boot_waterfall(this, tx, preferred_port)
            }));
            if let Err(p) = r {
                let msg = panic_payload_str(&*p);
                this2.enter_recovery_tx(&tx2, &format!("boot 线程异常（已捕获，客户端继续运行）: {msg}"));
            }
        });
    }

    /// boot 看门狗线程：`timeout` 后仍在「未完成」态 → 转恢复页。
    /// 超时参数化：测试注入短超时 + WSL 分支 35 分钟（生产两值），对外行为
    /// 零变更。代际感知（v0.5.1 频繁重启回归修复）：restart 每次
    /// 都会再挂一个看门狗，旧看门狗若不校验代际，会在新一次 boot 进行中
    /// 超时开火，把正在启动的内核打入恢复页（慢机上表现为反复重启）。
    pub(super) fn spawn_boot_watchdog(this: &Arc<Self>, tx: Sender<SupervisorEvent>, timeout: Duration) {
        let wd_tx = tx;
        let this_wd = Arc::clone(this);
        let gen = this.inner.lock().unwrap_or_else(|p| p.into_inner()).generation;
        std::thread::spawn(move || {
            std::thread::sleep(timeout);
            let g = this_wd.inner.lock().unwrap_or_else(|p| p.into_inner());
            if g.generation != gen {
                return; // 旧世代的看门狗：新 boot 已接手，本犬退休。
            }
            if Self::watchdog_should_fire(g.stopping, g.state) {
                drop(g);
                eprintln!("[supervisor] 看门狗：boot 链超时（{:?}），转恢复页", timeout);
                this_wd.enter_recovery_tx(&wd_tx, &format!("boot 链超时（{timeout:?} 看门狗）"));
            }
        });
    }

    /// 看门狗触发判定（纯函数，可单测）：超时时刻未在退出、且尚未到达
    /// Ready（正常就绪，哪怕迟到）或 Recovery（瀑布已自愈转恢复页）才触发。
    pub(super) fn watchdog_should_fire(stopping: bool, state: RunState) -> bool {
        !stopping && state != RunState::Ready && state != RunState::Recovery
    }

    /// 启动期孤儿内核收割（壳闪退兜底，见 `kernel_process::stale_kernel`）：
    /// 上次运行若留下记账且**记账 exe 与本次 node 一致 + 进程映像复核一致**
    /// → 整树收割并等端口释放；任一不符一律不动（防 pid 复用误杀）。
    /// 无论判定结果都清账——坏记账不得跨运行累积。
    pub(super) fn harvest_stale_kernel(&self) {
        let Some(rec) = kernel_process::stale_kernel::read(&self.pid_file) else {
            return;
        };
        let expected_exe = self.node_exe.to_string_lossy().into_owned();
        let image_matches = kernel_process::stale_kernel::process_image_matches(rec.pid, &expected_exe);
        match kernel_process::stale_kernel::harvest_decision(Some(&rec), &expected_exe, image_matches) {
            kernel_process::stale_kernel::HarvestDecision::Kill => {
                let killed = kernel_process::stale_kernel::kill_pid_tree(rec.pid);
                log_line(&format!(
                    "孤儿内核收割：pid={}（exe={}）杀树{}",
                    rec.pid,
                    rec.exe,
                    if killed { "完成" } else { "失败（可能已自行退出）" }
                ));
                if let Some(p) = rec.port {
                    if kernel_process::port::wait_port_free(p, Duration::from_secs(5)) {
                        log_line(&format!("孤儿内核收割后端口 {p} 已确认释放"));
                    } else {
                        log_line(&format!("孤儿内核收割后端口 {p} 5s 内未释放，后续换新端口"));
                    }
                }
            }
            kernel_process::stale_kernel::HarvestDecision::ExeMismatch => log_line(&format!(
                "孤儿内核记账 exe 与本次 node 不一致，跳过收割（记账 {} / 本次 {expected_exe}）",
                rec.exe
            )),
            kernel_process::stale_kernel::HarvestDecision::NotRunning => log_line(&format!(
                "孤儿内核记账 pid={} 已不在或映像不符（pid 复用防护），仅清账",
                rec.pid
            )),
            kernel_process::stale_kernel::HarvestDecision::NoRecord => {}
        }
        kernel_process::stale_kernel::remove(&self.pid_file);
    }

    /// 守护瀑布主体（boot 线程内执行；panic 由 spawn_boot 捕获兜底）。
    pub(super) fn boot_waterfall(this: Arc<Self>, tx: Sender<SupervisorEvent>, preferred_port: Option<u16>) {
        {
            // ---- [-3] 孤儿内核收割（壳闪退兜底）：上次进程若死在退出路径外，
            //      内核无 Child 句柄可收、Job Object 也随父进程句柄释放才生效；
            //      先收割再选端口，避免双 web 实例抢同一 workspace。----
            this.harvest_stale_kernel();
        }
        {
            // 记账本次瀑布的 dsh-web.log 起始偏移：恢复页/事故报告只引用
            // 本次运行的内核输出（io 失败按 None 处理，绝不阻断 boot）。
            let web_log = shell_core::DshPaths::resolve().logs.join("dsh-web.log");
            let mark = std::fs::metadata(&web_log).ok().map(|m| m.len());
            let mut g = this.inner.lock().unwrap_or_else(|p| p.into_inner());
            g.log_mark = mark;
        }
        {
            let gen = this.inner.lock().unwrap_or_else(|p| p.into_inner()).generation;
            // ---- [-2] Node 三级解析预检（v0.5.4）：系统 PATH ≥22 → 内置
            //      vendor → 全缺。全缺时清晰报错直接恢复页——旧形态是后续
            //      sidecar spawn 报「os error 2」，用户无从知道缺的是 Node。
            //      WSL 模式同样需要 Windows 侧 node 跑 sidecar boot，故本步
            //      先于 WSL configure。----
            match &this.node_resolved {
                Some(r) => log_line(&format!("Node 解析命中：{}", r.label())),
                None => {
                    let msg = "未找到可用的 Node.js：既未检测到系统 Node（版本 ≥ 22），安装目录也缺少内置 node（vendor/node/node.exe）。请安装 Node.js 22 或更高版本（https://nodejs.org），或重新下载完整的安装包/便携版。".to_string();
                    eprintln!("[boot] {msg}");
                    // last_error 写入必须独立作用域：enter_recovery 内部的
                    // kill_kernel/state/set_state 都会再取 inner 锁（同下方
                    // sidecar 失败分支的自死锁教训）。
                    {
                        let mut g = this.inner.lock().unwrap_or_else(|p| p.into_inner());
                        g.last_error = Some(msg.clone());
                    }
                    let _ = tx.send(SupervisorEvent::BootStep { name: "node-resolve".into(), ok: false, ms: 0, error: Some(msg) });
                    this.enter_recovery(&tx, "Node 缺失（系统与内置均无）");
                    return;
                }
            }
            // ---- [-1] WSL 后端解析（契约 §4.2 顺序红线：loading 窗已开 →
            //      configure 探测）。失败回落 local 继续启动（issue #54：不
            //      阻塞、不恢复页、配置保留），原因进 fallback_reason。----
            let mut wsl = Self::wsl_configure_or_fallback(&this);
            // ---- [0w] ensure_installed（**必须先于插件/补丁链**：补丁目标含
            //      <UNC>/agent/node_modules，agent 未就位则锚点全空——Electron
            //      main.js 4957 ensureInstalled 先于 syncPlugins 同序）。首装/
            //      版本漂移 → WSL 内 npm staging 安装（进度经 BootStep 事件）；
            //      失败同样回落 local（#54）。----
            if wsl.is_some() {
                if let Err(e) = this.run_wsl_ensure_installed(&tx) {
                    log_line(&format!("WSL agent 安装失败，回落本地模式继续启动（issue #54）: {e}"));
                    this.wsl_fallback(&e);
                    wsl = None;
                }
            }
            // ---- [0.5] farm 实体目录去材料化（Electron repairProfileFallback
            // 等价物，H/V2 实测定论的残余风险）：farm 条目被云同步/复制还原成
            // 实体目录时内核 heal 直接放弃（"exists and is not a symlink"），
            // 原生依赖链断裂 → 预设挂载失败。挪开让 heal 重建 junction。
            // 尽力而为：失败仅日志，绝不阻断 boot 链。
            // WSL 模式跳过（契约 §4.2：junction 是 Windows 本地概念，WSL 内
            // profile fallback 由内核自行 heal；sidecar farm-repair.js 亦自跳）。----
            if wsl.is_none() {
                this.run_farm_repair();
            }
            // ---- [1] sidecar boot（WSL 模式 home=UNC，Windows 侧经 UNC 写穿：
            //      sync/patches/preflight 契约 §4.2；顺带合并 guard-snapshot boot，
            //      省去一次独立 node.exe 冷启动）----
            this.set_state(RunState::Repair);
            let t0 = Instant::now();
            let merged_snap = match this.run_sidecar_boot(&tx, gen) {
                Ok(snap_id) => snap_id,
                Err(e) => {
                    // last_error 写入必须独立作用域：enter_recovery_tx 内部的
                    // kill_kernel/state/set_state 都会再取 inner 锁，同线程持锁
                    // 重入 = 自死锁（v0.5.2 便携版 node.exe 缺失实测：boot 线程
                    // 冻死在 kill_kernel 的 lock()，CrashLoop 永不发出，加载页
                    // 永挂、恢复页永不出现——正是「便携版一直在加载」的主链）。
                    {
                        let mut g = this.inner.lock().unwrap_or_else(|p| p.into_inner());
                        g.last_error = Some(e.clone());
                    }
                    let _ = tx.send(SupervisorEvent::BootStep { name: "sidecar-boot".into(), ok: false, ms: t0.elapsed().as_millis() as u64, error: Some(e) });
                    this.enter_recovery(&tx, "boot 链失败");
                    return;
                }
            };
            if this.inner.lock().unwrap_or_else(|p| p.into_inner()).generation != gen || this.inner.lock().unwrap_or_else(|p| p.into_inner()).stopping {
                return;
            }
            // ---- [1.5] koffi 预检 → 目录选择器降级 overlay（Electron 对齐，升级适配）。
            // WSL 模式跳过（契约 §4.2：win32 预编译探测与 Linux 内核无关，
            // 原生模块由 WSL 内 npm 安装的 linux 变体提供；sidecar 同款跳过）。----
            if wsl.is_none() {
                this.run_koffi_preflight();
            } else {
                log_line("koffi 预检：WSL 托管模式跳过（原生模块为 WSL 内 linux 变体）");
            }
            // ---- [1.6] 启动前快照（优先复用 sidecar boot 合并产出的快照，缺失再回落独立调用）----
            let boot_snap = merged_snap.or_else(|| {
                this.guard_cli_json(&["guard-snapshot", "boot"])
                    .and_then(|v| v.get("id").and_then(|i| i.as_str()).map(String::from))
            });
            if let Some(id) = &boot_snap {
                log_line(&format!("守护瀑布：启动快照 {id}"));
            }
            // ---- [2] 端口 ----
            // WSL 模式 `--port 0`（WSL 内 OS 分配；Windows 侧 bind 探测对 WSL
            // 内监听无意义——design D3），实际端口从就绪行解析。
            let port = if wsl.is_some() {
                0u16
            } else {
                match choose_stable_port(preferred_port) {
                    Some(p) => p,
                    None => {
                        this.enter_recovery(&tx, "无可用安全端口");
                        return;
                    }
                }
            };
            this.inner.lock().unwrap_or_else(|p| p.into_inner()).port = Some(port);
            this.set_state(RunState::Spawn);

            // ---- [3] 首次拉起（有界等待 120s，对齐 Electron waitUntilUp）----
            match Arc::clone(&this).spawn_and_wait_ready(port, &tx, Duration::from_secs(120)) {
                Ok(url) => return this.on_boot_success(url, port, gen, boot_snap),
                Err(first) => {
                    log_line(&format!("守护瀑布：首次拉起失败（{first}），进入体检修复"));
                }
            }
            if this.cancelled(gen) { return; }

            // ---- [4] 二层：重跑 boot 链（强制全量 sync 重新同步伴随插件，修复 node_modules
            // 损坏——自愈主力；guard 快照只含 4 个配置文件，坏文件靠 sync 覆盖）+ 体检修复
            // + safe overlay 禁用坏插件 → 二次拉起 ----
            if let Err(e) = this.run_sidecar_boot_forced(&tx, gen) {
                log_line(&format!("守护瀑布：二层重跑 boot 链失败：{e}"));
            }
            let repaired = this.guard_cli_json(&["guard-repair"]);
            let applied: Vec<String> = repaired
                .and_then(|v| v.get("applied").and_then(|a| a.as_array()).map(|arr| arr.iter().filter_map(|x| x.as_str().map(String::from)).collect()))
                .unwrap_or_default();
            let safe_applied = this.refresh_safe_overlay();
            log_line(&format!("守护瀑布：体检修复 applied={applied:?} safeOverlay禁用={safe_applied}"));
            if !safe_applied && applied.is_empty() {
                log_line("守护瀑布：无可修复项也无失败插件名单，直接进入回滚层");
            }
            let port2 = if wsl.is_some() { 0 } else { this.reuse_or_new_port(port) };
            match Arc::clone(&this).spawn_and_wait_ready(port2, &tx, Duration::from_secs(90)) {
                Ok(url) => {
                    this.guard_incident("boot-recovered", &format!("首次启动失败，体检修复后恢复。修复项：{applied:?}"));
                    return this.on_boot_success(url, port2, gen, boot_snap);
                }
                Err(second) => log_line(&format!("守护瀑布：修复后仍失败（{second}），进入回滚")),
            }
            if this.cancelled(gen) { return; }

            // ---- [5] 三层：回滚最后良好快照 → 三次拉起 ----
            let lastgood = this.guard_cli_json(&["guard-lastgood"])
                .and_then(|v| if v.get("ok").and_then(|b| b.as_bool()).unwrap_or(false) {
                    v.get("id").and_then(|i| i.as_str()).map(|id| (id.to_string(), v.get("reason").and_then(|r| r.as_str()).unwrap_or("").to_string()))
                } else { None });
            let rollback_target = lastgood.filter(|(id, _)| boot_snap.as_deref() != Some(id.as_str()));
            match rollback_target {
                Some((id, reason)) => {
                    log_line(&format!("守护瀑布：回滚到最后良好快照 {id}（{reason}）"));
                    let _ = this.guard_cli_json(&["guard-restore", &id]);
                    let _ = this.guard_cli_json(&["guard-repair"]); // 回滚后再清一次遮蔽
                    // 回滚复活的是快照里的旧 node_modules——sync/patches 步写下的
                    // loader 隔离补丁可能被旧拷贝覆盖（v0.6.2 实爆：回滚后
                    // float-window 缺包被旧 loader re-throw 成整树 fatal → 崩溃环
                    // 转恢复页）。三次拉起前重打一遍补丁，缺包容错交还给树级隔离。
                    // 失败容忍（命令内部告警不阻断），拿不到结果也不阻断回滚层。
                    match this.guard_cli_json(&["patches-apply"]) {
                        Some(_) => log_line("守护瀑布：回滚后已重打兼容/隔离补丁"),
                        None => log_line("守护瀑布：回滚后补丁重打缺席（旧 payload 或命令失败，容忍继续）"),
                    }
                    let port3 = if wsl.is_some() { 0 } else { this.reuse_or_new_port(port) };
                    match Arc::clone(&this).spawn_and_wait_ready(port3, &tx, Duration::from_secs(90)) {
                        Ok(url) => {
                            this.guard_incident("rollback-recovered", &format!("回滚到快照 {id} 后恢复启动"));
                            this.on_boot_success(url, port3, gen, None)
                        }
                        Err(final_err) => {
                            if this.try_auto_rollback_overlay(&tx, &format!("启动失败（回滚快照后）：{final_err}")) {
                                return;
                            }
                            let note = this.kernel_error_suffix();
                            this.guard_incident("boot-failed", &format!("回滚到 {id} 后仍无法启动：{final_err}{note}"));
                            this.enter_recovery(&tx, &format!("回滚后仍失败：{final_err}"));
                        }
                    }
                }
                None => {
                    if this.try_auto_rollback_overlay(&tx, "启动失败且无可回滚快照") {
                        return;
                    }
                    let note = this.kernel_error_suffix();
                    this.guard_incident("boot-failed", &format!("启动失败且无可回滚快照（首次运行或快照耗尽）{note}"));
                    this.enter_recovery(&tx, "启动失败且无可回滚快照（可在恢复页重试）");
                }
            }
        }
    }

    /// 就绪成功路径：待落定快照 + 稳定落定线程（45s 后 markGood，
    /// Electron armStabilityWatch 语义：稳定存活即成为「最后良好」回滚锚点）。
    ///
    /// 换页（KernelReady）与探活布防不在此处：就绪行线程是唯一换页源
    ///（含 HTTP 热探），瀑布/自动重启两条路径统一走它——此前这里再发一次
    /// KernelReady 构成双发（双 navigate / 双心跳监测 / 双诊断探针，M1 审计
    /// 遗留；真机日志可见 t0/probe 输出全双份）。
    pub(super) fn on_boot_success(self: &Arc<Self>, url: String, port: u16, gen: u64, snap: Option<String>) {
        let _ = (url, port);
        {
            let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
            g.pending_good = snap;
        }
        self.set_state(RunState::Ready);
        let this = Arc::clone(self);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(SERVICE_STABLE_SECS));
            let g = this.inner.lock().unwrap_or_else(|p| p.into_inner());
            if g.generation != gen || g.stopping { return; }
            if let Some(id) = g.pending_good.clone() {
                drop(g);
                let _ = this.guard_cli_json(&["guard-mark-good", &id]);
                let mut g2 = this.inner.lock().unwrap_or_else(|p| p.into_inner());
                g2.pending_good = None;
                g2.crash_count = 0; // 稳定落地 → 崩溃计数复位（Electron 同款）
                this.inner_crash_reset();
                log_line(&format!("守护瀑布：服务稳定存活，快照 {id} 落定为最后良好"));
            }
        });
    }

    /// 拉起内核并同步等待就绪（瀑布核心原语）。
    pub(super) fn spawn_and_wait_ready(self: Arc<Self>, port: u16, tx: &Sender<SupervisorEvent>, timeout: Duration) -> Result<String, String> {
        let (rtx, rrx) = std::sync::mpsc::channel::<Result<String, String>>();
        self.inner.lock().unwrap_or_else(|p| p.into_inner()).ready_tx = Some(rtx);
        if let Err(e) = self.clone().spawn_kernel(port, tx) {
            self.inner.lock().unwrap_or_else(|p| p.into_inner()).ready_tx = None;
            return Err(e);
        }
        let deadline = Instant::now() + timeout;
        match rrx.recv_timeout(deadline.saturating_duration_since(Instant::now()).max(Duration::from_millis(1))) {
            Ok(Ok(url)) => Ok(url),
            Ok(Err(e)) => Err(e),
            Err(_) => {
                // 超时：杀掉半死进程，按失败处理。
                self.kill_kernel();
                self.inner.lock().unwrap_or_else(|p| p.into_inner()).ready_tx = None;
                Err(format!("{timeout:?} 内未就绪"))
            }
        }
    }

    // ---- WSL 托管模式（契约 wsl-backend.md §4；local 模式零变更）----

    /// boot 线程首步：configure 探测链（wsl -l -q → distro → $HOME →
    /// installDir → node/npm → UNC）。成功设运行态；失败回落 local（#54：
    /// 配置保留、原因进 fallback_reason、绝不恢复页）。
    pub(super) fn wsl_configure_or_fallback(this: &Arc<Self>) -> Option<Arc<wsl_backend::WslBackend>> {
        let cfg = this.wsl_cfg.clone()?;
        if cfg.backend != "wsl" {
            return None;
        }
        let backend = Arc::new(wsl_backend::WslBackend::new(Arc::clone(&this.wsl_invoker)));
        let opts = wsl_backend::ConfigureOpts::from_env(&cfg.distro, &cfg.install_dir);
        match backend.configure(&opts) {
            Ok(()) => {
                *this.wsl.lock().unwrap_or_else(|p| p.into_inner()) = Some(Arc::clone(&backend));
                let st = backend.status_json();
                log_line(&format!(
                    "WSL 托管后端就绪: distro={} installDir={} UNC={} node={} npm={}{}",
                    backend.distro(),
                    backend.install_dir(),
                    backend.unc_home().display(),
                    st["nodeVersion"].as_str().unwrap_or(""),
                    st["npmVersion"].as_str().unwrap_or(""),
                    if cfg.simulated { "（DSH_WSL_MODE 模拟缝）" } else { "" },
                ));
                Self::log_wsl_network_hint();
                Some(backend)
            }
            Err(e) => {
                log_line(&format!("WSL 探测失败，回落本地模式继续启动（issue #54）: {e}"));
                this.wsl_fallback(&e.to_string());
                None
            }
        }
    }

    /// WSL 网络模式提示（#1 可诊断性）：读 `%USERPROFILE%\.wslconfig` 判是否
    /// mirrored，把「偶发断线致输出中断」的正解打进 boot 日志。mirrored（Win11
    /// 22H2+）让 Windows↔WSL 的 localhost 直连、不经 NAT 端口转发，是转发抖动
    /// 的根治手段；无 .wslconfig 视为默认 NAT。仅记日志，探测失败不阻断启动。
    fn log_wsl_network_hint() {
        let mirrored = std::env::var("USERPROFILE")
            .ok()
            .map(|p| std::path::Path::new(&p).join(".wslconfig"))
            .and_then(|f| std::fs::read_to_string(f).ok())
            .map(|s| {
                let compact: String = s.to_lowercase().chars().filter(|c| !c.is_whitespace()).collect();
                compact.contains("networkingmode=mirrored")
            })
            .unwrap_or(false);
        if mirrored {
            log_line("WSL 网络：检测到 .wslconfig networkingMode=mirrored（localhost 直连，抗转发抖动最佳，无需额外处理）");
        } else {
            log_line("WSL 网络：默认 NAT（未检测到 mirrored）。若偶发断线导致输出中断，建议在 %USERPROFILE%\\.wslconfig 写 [wsl2] networkingMode=mirrored，随后重启 WSL 生效——详见 dsh-desktop/docs/wsl-verification.md");
        }
    }

    /// 回落 local：清运行态 + 记原因（#54；本次运行期有效）。
    fn wsl_fallback(&self, reason: &str) {
        *self.wsl.lock().unwrap_or_else(|p| p.into_inner()) = None;
        *self.fallback_reason.lock().unwrap_or_else(|p| p.into_inner()) = reason.to_string();
    }

    /// ensure_installed（契约 §4.5）：agent 预检（入口存在 + 版本 == payload
    /// 版本——D5 版本锚）→ 首装/漂移重装（WSL 内 npm staging + 原子切换）。
    /// npm 进度行经 BootStep 事件 + 日志透出（首装数分钟必须有用户可见反馈）。
    fn run_wsl_ensure_installed(&self, tx: &Sender<SupervisorEvent>) -> Result<(), String> {
        let Some(backend) = self.wsl_active() else { return Ok(()) };
        let t0 = Instant::now();
        let target = self.kernel_version.clone();
        let tx2 = tx.clone();
        let mut install_started = false;
        let res = backend.ensure_installed(&target, &mut |line| {
            // 首条 npm 行 = 安装实际开始（预检通过前不发事件，loading 页步骤
            // 列表不掺入常态路径）。
            if !std::mem::replace(&mut install_started, true) {
                let _ = tx2.send(SupervisorEvent::BootStep { name: "wsl-install".into(), ok: true, ms: 0, error: None });
            }
            log_line(&format!("wsl-install| {line}"));
        });
        match res {
            Ok(false) => {
                log_line("WSL agent 已就位且版本与 payload 一致（零安装）");
                Ok(())
            }
            Ok(true) => {
                let _ = tx.send(SupervisorEvent::BootStep {
                    name: "wsl-install".into(),
                    ok: true,
                    ms: t0.elapsed().as_millis() as u64,
                    error: None,
                });
                log_line(&format!("WSL agent 安装完成（{}ms）", t0.elapsed().as_millis()));
                Ok(())
            }
            Err(e) => {
                let _ = tx.send(SupervisorEvent::BootStep {
                    name: "wsl-install".into(),
                    ok: false,
                    ms: t0.elapsed().as_millis() as u64,
                    error: Some(e.to_string()),
                });
                Err(e.to_string())
            }
        }
    }

    /// sidecar boot（node cli.js boot），逐步从 stderr 解析 [sidecar] 行转发。
    /// farm 实体目录去材料化（sidecar/farm-repair.js，node 侧 fs 操作）。
    /// 先用 Rust 原生快速扫描（<1ms）：无实体化目录时直接跳过 node.exe 子进程冷启动，
    /// 命中实体化目录再交由 farm-repair.js 完整挪开保留现场。失败仅日志，绝不影响 boot 链。
    fn run_farm_repair(&self) {
        let home = shell_core::DshPaths::resolve().dsh_home;
        let farm = home.join("profiles").join("node_modules");
        let payload_nm = self.app_dir.join("node_modules");
        if !has_materialized_farm_entry(&farm, &payload_nm) {
            return;
        }
        // 双布局：安装形态 app_dir=<install>/resources/dsh-desktop →
        // ../sidecar（resources/sidecar）；repo 检出 → ../dsh-tauri/sidecar。
        let installed = self.app_dir.join("..").join("sidecar").join("farm-repair.js");
        let script = if installed.exists() {
            installed
        } else {
            self.app_dir.join("..").join("dsh-tauri").join("sidecar").join("farm-repair.js")
        };
        if !script.exists() {
            eprintln!("[farm-repair] 脚本缺失（{:?}），跳过", script);
            return;
        }
        let out = sanitized_node_command(&self.node_exe)
            .arg(&script)
            .arg(&self.app_dir)
            .env("NODE_COMPILE_CACHE", self.compile_cache_dir())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .creation_flags_win()
            .output();
        match out {
            Ok(o) => {
                let stderr = String::from_utf8_lossy(&o.stderr);
                for line in stderr.lines().filter(|l| l.contains("[farm-repair]")) {
                    log_line(line);
                }
            }
            Err(e) => eprintln!("[farm-repair] 执行失败（不阻断）：{e}"),
        }
    }

    pub(super) fn run_sidecar_boot(&self, tx: &Sender<SupervisorEvent>, gen: u64) -> Result<Option<String>, String> {
        self.run_sidecar_boot_inner(tx, gen, true, false)
    }

    pub(super) fn run_sidecar_boot_forced(&self, tx: &Sender<SupervisorEvent>, gen: u64) -> Result<Option<String>, String> {
        self.run_sidecar_boot_inner(tx, gen, false, true)
    }

    fn run_sidecar_boot_inner(
        &self,
        tx: &Sender<SupervisorEvent>,
        _gen: u64,
        with_guard_snapshot: bool,
        force_sync: bool,
    ) -> Result<Option<String>, String> {
        let mut cmd = sanitized_node_command(&self.node_exe);
        cmd.arg(&self.sidecar_cli)
            .arg("boot")
            .arg("--app-dir")
            .arg(&self.app_dir)
            .args(self.sidecar_home_args())
            .env("DSH_TAURI_VERSION", env!("CARGO_PKG_VERSION"))
            .env("NODE_COMPILE_CACHE", self.compile_cache_dir());
        if with_guard_snapshot {
            cmd.arg("--guard-snapshot");
        }
        if force_sync {
            cmd.arg("--force-sync");
        }
        let out = cmd
            // GUI 进程起 console 子进程抑制终端窗（boot 是「启动后弹终端」主源，
            // 与本文件其余 node spawn 同口径——0.5.0 实测修复）。
            .creation_flags_win()
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .map_err(|e| {
                let msg = format!("sidecar spawn 失败（node: {} cli: {}）: {e}", self.node_exe.display(), self.sidecar_cli.display());
                eprintln!("[boot] {msg}");
                msg
            })?;
        // boot 通道 stderr 逐行进壳层日志——**成功路径也要**（2026-10-10 补）：
        // repair/sync/patches 三级的自愈与告警行只在这里出现，退出码 0 时整段丢弃
        // 等于把「静默失效」的第二现场也抹掉。实测真形：一次 boot = 115 行 / 15KB
        // （60 补丁全量），其中「补丁应用汇总: …/失配 N 项…」「宿主组合关键服务
        // 自检: 已修复 profile 模块 fallback 链接（@deepseek-ai/dsh-credentials-
        // local）」正是 mac 端 9 插件 failed to load 排查里唯一能证明补丁到底落到
        // 没有的证据。上限只防异常输出（崩溃回环）灌爆 desktop.log，截断显式注明。
        for line in sidecar_boot_log_lines(&String::from_utf8_lossy(&out.stderr), SIDECAR_BOOT_LOG_CAP) {
            log_line(&line);
        }
        if !out.status.success() {
            // 死因在输出**末尾**：sidecar 自身进度行先刷屏，V8 fatal/OOM/abort
            // 报告最后才出（0xC0000409 真机日志此前 take(6) 只留头部，只剩
            // 「repair → OK」，真正的死因行被整段裁掉——排查只能看到「死了」
            // 看不到「为何死」）。改为保留尾部；stderr 空时退回 stdout 尾部。
            let stderr = String::from_utf8_lossy(&out.stderr);
            let mut reason = tail_lines(&stderr, 10);
            if reason.is_empty() {
                reason = tail_lines(&String::from_utf8_lossy(&out.stdout), 3);
            }
            let msg = format!("sidecar boot 退出码 {:?}: {}", out.status.code(), reason);
            eprintln!("[boot] {msg}");
            return Err(msg);
        }
        // stdout：末行 JSON {ok,totalMs,steps[],bootSnapshotId?}
        let stdout = String::from_utf8_lossy(&out.stdout);
        let line = stdout.trim_end().lines().last().unwrap_or("");
        let parsed: serde_json::Value = serde_json::from_str(line).map_err(|e| format!("sidecar 输出解析: {e}"))?;
        for step in parsed.get("steps").and_then(|s| s.as_array()).unwrap_or(&vec![]) {
            let name = step.get("name").and_then(|v| v.as_str()).unwrap_or("?").to_string();
            let ok = step.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
            let ms = step.get("ms").and_then(|v| v.as_u64()).unwrap_or(0);
            let error = step.get("error").and_then(|v| v.as_str()).map(String::from);
            let _ = tx.send(SupervisorEvent::BootStep { name, ok, ms, error });
        }
        if parsed.get("ok").and_then(|v| v.as_bool()) != Some(true) {
            return Err("boot 链存在失败步骤".into());
        }
        let snap_id = parsed
            .get("bootSnapshotId")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from);
        Ok(snap_id)
    }

    /// koffi 预检：失败时启用 picker-browse 降级 overlay（Electron runKoffiPreflight
    /// 与 enablePickerBrowseOverlay 的合并语义；缓存简化为 settings 布尔——每次
    /// 冒烟 ~100ms 级，签名级缓存随出包验证再评估）。
    #[cfg(windows)]
    fn run_koffi_preflight(&self) {
        let settings = shell_core::SettingsStore::new(shell_core::DshPaths::resolve().settings);
        let cached = settings.get("koffiPreflightOk").ok().flatten().and_then(|v| v.as_bool());
            let ok = match cached {
                Some(true) => true,
                _ => {
                    let out = sanitized_node_command(&self.node_exe)
                        .arg(&self.sidecar_cli)
                        .arg("koffi-preflight")
                        .arg("--app-dir")
                        .arg(&self.app_dir)
                        .creation_flags_win()
                        .output();
                    let ok = matches!(out, Ok(o) if o.status.success()
                        && koffi_preflight_passed(&String::from_utf8_lossy(&o.stdout)));
                    if ok {
                        let _ = settings.set("koffiPreflightOk", serde_json::json!(true));
                    }
                    ok
                }
            };
        if !ok {
            let out = sanitized_node_command(&self.node_exe)
                .arg(&self.sidecar_cli)
                .arg("picker-overlay")
                .arg("--app-dir")
                .arg(&self.app_dir)
                .creation_flags_win()
                .output();
            if let Ok(o) = out {
                let stdout = String::from_utf8_lossy(&o.stdout);
                if let Some(line) = stdout.trim_end().lines().last() {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                        if let Some(p) = v.get("path").and_then(|p| p.as_str()) {
                            log_line("koffi 预检未过，启用目录选择器降级 overlay");
                            let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
                            let path = std::path::PathBuf::from(p);
                            if !g.overlays.contains(&path) {
                                g.overlays.push(path);
                            }
                        }
                    }
                }
            }
        } else {
            log_line("koffi 预检通过");
        }
    }

    /// 非 Windows：koffi 预检跳过。预检的故障模式是 win32-x64 预编译二进制
    /// 在 koffi.load() 处访问违例（koffi-preflight.cjs 探测的正是 kernel32.dll）
    /// ——非 Windows 无此故障面；若照跑只会恒失败，导致目录选择器被永久
    /// 降级到 browse 后端（静默功能损失）。
    #[cfg(not(windows))]
    fn run_koffi_preflight(&self) {
        log_line("koffi 预检仅 Windows 需要（本平台跳过）");
    }

    /// 刷新 safe-boot overlay（崩溃自动重启前）：解析 dsh-web.log 失败插件 → 禁用。
    fn refresh_safe_overlay(&self) -> bool {
        let out = sanitized_node_command(&self.node_exe)
            .arg(&self.sidecar_cli)
            .arg("safe-overlay")
            .arg("--app-dir")
            .arg(&self.app_dir)
            .creation_flags_win()
            .output();
        let Ok(o) = out else { return false };
        let stdout = String::from_utf8_lossy(&o.stdout);
        let Some(line) = stdout.trim_end().lines().last() else { return false };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { return false };
        let ids = v.get("ids").and_then(|i| i.as_array()).map(|a| a.len()).unwrap_or(0);
        if ids == 0 {
            return false;
        }
        if let Some(p) = v.get("path").and_then(|p| p.as_str()) {
            log_line(&format!("安全启动 overlay：禁用 {ids} 个失败插件"));
            let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
            let path = std::path::PathBuf::from(p);
            if !g.overlays.contains(&path) {
                g.overlays.push(path);
            }
        }
        true
    }

    /// spawn 内核进程 + 就绪行监视线程。
    ///
    /// WSL 模式（契约 §4.3）：spawn `wsl.exe -d <distro> -e sh -lc <cmd>` 包装
    /// ——不设工作目录（cd 在命令串内）、不设环境（Windows 环境块不传进 WSL，
    /// 净化在命令串 `env -u` 完成）、不设 PGID（WSL 内进程不在 Windows 进程
    /// 树）。Job Object 照常绑 wsl.exe（强杀壳时至少收割包装进程）。local
    /// 路径逐字节不变（不变量 §7.1）。
    pub(super) fn spawn_kernel(self: Arc<Self>, port: u16, tx: &Sender<SupervisorEvent>) -> Result<(), String> {
        let overlays = self.inner.lock().unwrap_or_else(|p| p.into_inner()).overlays.clone();
        let active_agent = self.active_agent();
        let wsl_backend_active = self.wsl_active();
        let wsl_mode = wsl_backend_active.is_some();
        let mut child = if let Some(backend) = &wsl_backend_active {
            let no_open = kernel_process::semver::needs_no_open_flag(&active_agent.version);
            let cmd = backend.server_cmd(no_open);
            log_line(&format!(
                "内核(WSL) spawn: wsl.exe -d {} -e sh -lc {}（--port 0，实际端口待就绪行）",
                backend.distro(),
                cmd
            ));
            backend.spawn_server(no_open).map_err(|e| format!("spawn wsl.exe: {e}"))?
        } else {
            let use_system_ca = self.node_resolved.as_ref().map(|r| r.supports_use_system_ca()).unwrap_or(false);
            let spec = SpawnSpec::new(&self.node_exe, &active_agent.bin_js, &active_agent.version, port, &overlays, use_system_ca);
            // 环境净化：env_clear + 白名单（sanitized_node_command）。macOS 启动
            // 崩溃环根治——`std::process::Command` 默认继承父进程全部环境，此前
            // 只挂白名单不 env_clear，白名单形同虚设，NODE_OPTIONS 等任意父进程
            // 变量泄漏进内核（WorkBuddy 的 genie-safe-delete.cjs 猴补丁被打进内核，
            // boot 时批量删 node_modules.lock 被抛 SAFE_DELETE_BULK_CONFIRM_REQUIRED）。
            let mut cmd = sanitized_node_command(&spec.node_exe);
            cmd.args(&spec.node_args).arg(&spec.bin_js).args(&spec.web_args);
            // 监管标识（main.js childEnv 语义）+ Node 24 原生 V8 字节码编译缓存（二次启动省 3~6s）。
            cmd.env("DSH_DESKTOP_SUPERVISED", "1")
                .env("NO_COLOR", "1")
                .env("NODE_COMPILE_CACHE", self.compile_cache_dir());
            // WinInet 系统代理桥接（0.6.3 beta.3）：国内个人用户最常见形态是
            // 「只开 Windows 系统代理」——WinInet（注册表）对 Node 完全不可见，
            // 父环境无代理变量时 beta2 白名单也无从透传 → 内核直连 googleapis
            // 等被墙域必超时（2026-09-08 内置 node 四组对照实测实锤：仅
            // HTTPS_PROXY 不生效，HTTPS_PROXY+NODE_USE_ENV_PROXY=1 才通）。
            // 此处探测注册表代理，仅在父环境缺失时注入（用户显式配置优先）；
            // NO_PROXY 固定放行本机回环（内核自身 127.0.0.1 请求不进代理）。
            let proxy_injection = system_proxy_env_injection();
            if !proxy_injection.is_empty() {
                for (k, v) in &proxy_injection {
                    cmd.env(k, v);
                }
                log_line(&format!("WinInet 系统代理桥接注入内核: {proxy_injection:?}"));
            }
            cmd.current_dir(&self.app_dir).stdin(Stdio::null())
                .stdout(Stdio::piped()).stderr(Stdio::piped())
                .creation_flags_win();
            // Unix 杀树根基：内核设为进程组长（PGID == pid），后续全部子孙（工具
            // 进程/持久终端会话）天然继承同组——kill_kernel 的 killpg(-pgid) 才能
            // 整组收割（mac 退出后内核残留的根因）。Windows no-op（杀树走 Job
            // Object + taskkill）。
            kernel_process::kill_tree::set_process_group_leader(&mut cmd);
            let child = cmd.spawn().map_err(|e| format!("spawn: {e}"))?;
            log_line(&format!("内核 pid={} spawn: {}", child.id(), spec.display_cmd()));
            child
        };

        // Review#2 根治：Job Object 杀树保护（父进程被强杀时 OS 收割内核树）。
        // WSL 模式绑定的 wsl.exe 包装进程（WSL 内进程收割见 kill_kernel 三层；
        // 强杀壳时 WSL 内可能残留——契约 §4.6 残余风险，spawn 前 rm -f dsh.pid
        // + 下次启动兜底）。
        if let Err(e) = kernel_process::job_object::assign_child_to_kill_on_close_job(&child) {
            log_line(&format!("Job Object 赋值失败（杀树保护降级为显式 taskkill）: {e}"));
        }
        // 孤儿内核记账（壳闪退兜底）：local 模式记 pid/exe/port，内核退出或
        // 显式杀树时删除；下次启动在 spawn 前收割。WSL 不记——wsl.exe 不是
        // 内核本体（收割语义见 kill_kernel 三层与契约 §4.6）。
        if !wsl_mode {
            kernel_process::stale_kernel::write(
                &self.pid_file,
                &kernel_process::stale_kernel::KernelPidRecord {
                    pid: child.id(),
                    exe: self.node_exe.to_string_lossy().into_owned(),
                    port: Some(port),
                },
            );
        }
        let stdout = child.stdout.take().ok_or("stdout piped 失败")?;
        let stderr = child.stderr.take();
        self.inner.lock().unwrap_or_else(|p| p.into_inner()).kernel = Some(child);

        // 就绪行监视（独占读 stdout；读 EOF 时若进程仍在则继续探活兜底）。
        let this = Arc::clone(&self);
        let tx2 = tx.clone();
        std::thread::spawn(move || {
            let mut parser = ReadyLineParser::new();
            let mut url: Option<String> = None;
            for chunk in BufReader::new(stdout).split(b'\n') {
                let chunk = match chunk { Ok(c) => c, Err(_) => break };
                let text = String::from_utf8_lossy(&chunk).into_owned();
                if !text.trim().is_empty() {
                    log_line(&format!("web| {text}"));
                    // C3（B2 配方）：内核 stdout 持久化 dsh-web.log（4MB 轮转）——
                    // 此前 println/log_line 在 GUI 进程全丢弃，safe-overlay 崩溃
                    // 自愈层无日志可解析、诊断无附件。IO 失败静默（不阻断启动）。
                    crate::logging::append_capped(
                        &shell_core::DshPaths::resolve().logs.join("dsh-web.log"),
                        &format!("web| {text}"),
                        crate::logging::LOG_CAP_BYTES,
                    );
                }
                if url.is_none() {
                    if let Some(u) = parser.feed(&format!("{text}\n")) {
                        url = Some(u.clone());
                        // WSL 模式实际端口 = 就绪行 URL 的端口（--port 0 由 WSL 内
                        // OS 分配；spawn 传入值仅日志参考——契约 §4.3/D3）。
                        // 端口落在 Chromium 受限端口表 → 按本次拉起失败收链
                        //（杀掉重试，瀑布二/三层承接；Electron restrictedPortOf
                        // 两模式共用语义）。
                        let port = if wsl_mode {
                            match url_port(&u) {
                                Some(p) if kernel_process::port::is_safe_port(p) => p,
                                bad => {
                                    let reason = match bad {
                                        Some(p) => format!("就绪行端口 {p} 在 Chromium 受限端口表"),
                                        None => format!("就绪行 URL 缺端口: {u}"),
                                    };
                                    log_line(&format!("{reason}，按本次拉起失败收链"));
                                    let mut g = this.inner.lock().unwrap_or_else(|p| p.into_inner());
                                    g.kernel_url = None;
                                    if let Some(rtx) = g.ready_tx.take() {
                                        let _ = rtx.send(Err(reason));
                                    }
                                    drop(g);
                                    this.kill_kernel();
                                    return;
                                }
                            }
                        } else {
                            port
                        };
                        {
                            let mut g = this.inner.lock().unwrap_or_else(|p| p.into_inner());
                            g.kernel_url = Some(u.clone());
                            if wsl_mode {
                                g.port = Some(port);
                            }
                        }
                        let rtx = this.inner.lock().unwrap_or_else(|p| p.into_inner()).ready_tx.take();
                        if let Some(rtx) = rtx { let _ = rtx.send(Ok(u.clone())); }
                        this.set_state(RunState::Ready);
                        // HTTP 热探（用户实测「Failed to fetch 闪现」根治）：ready 行
                        // 只表示内核进程打出就绪日志，HTTP 监听可能有 ~100ms 窗口
                        // 尚未接受请求。本线程是 KernelReady 的唯一发送源（瀑布
                        // on_boot_success 与崩溃自动重启两条路径统一在此换页）——
                        // 此前自动重启路径无热探，就绪行早于 bind 时换页 → WebView2
                        // 错误页（白屏形态之一；且错误页上垫片照常发心跳，渲染层
                        // 心跳监测判定「页面健康」永不自愈）。
                        for i in 0..50 {
                            if Self::http_alive(port) { break; }
                            if i == 49 { log_line("HTTP 热探超时（5s），换页继续（可能首次闪败）"); }
                            std::thread::sleep(Duration::from_millis(100));
                        }
                        let _ = tx2.send(SupervisorEvent::KernelReady { url: u, port });
                        // 探活布防（唯一位置）：就绪行线程对每次内核就绪都布防——
                        // 覆盖瀑布路径与崩溃自动重启路径（此前只在 on_boot_success
                        // 布防，自动重启后的内核处于无探活状态）。probe_gen 令牌
                        // 递增令上一代探活环自行退出（同代际换内核不得双探活）。
                        let (probe_gen, live_gen) = {
                            let mut g = this.inner.lock().unwrap_or_else(|p| p.into_inner());
                            g.probe_gen += 1;
                            (g.probe_gen, g.generation)
                        };
                        this.probe_loop(port, tx2.clone(), live_gen, probe_gen);
                    }
                }
            }
            // stdout EOF = 进程退出。
            let (code, exited) = {
                let mut g = this.inner.lock().unwrap_or_else(|p| p.into_inner());
                match g.kernel.as_mut() {
                    Some(c) => match c.try_wait() {
                        Ok(Some(st)) => (st.code(), true),
                        Ok(None) => {
                            // stdout 关了但进程仍在：罕见（进程主动关闭 stdout /
                            // 句柄提前释放）。**必须收割**——on_kernel_exit 把它当
                            // 退出后自动重启的新内核会与它并存 = 双 web 实例
                            //（端口 7388 复用/孤儿内核形态，synapse「另一个 dsh
                            // web 实例」告警的壳侧来源）。K3 排查看板点。
                            let _ = c.kill();
                            let code = c.wait().ok().and_then(|st| st.code());
                            log_line(&format!("stdout EOF 但进程仍在（pid={}），已收割防双 web 实例", c.id()));
                            (code, true)
                        }
                        Err(_) => (None, true),
                    },
                    None => (None, false),
                }
            };
            if exited {
                this.on_kernel_exit(code, &tx2);
            }
        });
        // stderr 收尾线程（防管道满阻塞内核）。
        if let Some(err) = stderr {
            std::thread::spawn(move || {
                use std::io::Read;
                let mut buf = [0u8; 4096];
                let mut e = err;
                while let Ok(n) = e.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    let line = format!("web-err| {}", String::from_utf8_lossy(&buf[..n]).trim_end());
                    log_line(&line);
                    // C3（B2 配方）：内核 stderr 同落 dsh-web.log（4MB 轮转）。
                    crate::logging::append_capped(
                        &shell_core::DshPaths::resolve().logs.join("dsh-web.log"),
                        &line,
                        crate::logging::LOG_CAP_BYTES,
                    );
                }
            });
        }
        Ok(())
    }

    /// 内核退出处理：崩溃环判定。
    pub(super) fn on_kernel_exit(self: &Arc<Self>, code: Option<i32>, tx: &Sender<SupervisorEvent>) {
        let now = now_ms();
        let (verdict, crashes) = {
            let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
            if g.stopping {
                return;
            }
            // 瀑布等待者唤醒：启动期退出 = 本次拉起失败。
            if let Some(rtx) = g.ready_tx.take() {
                let _ = rtx.send(Err(format!("内核启动期退出 code={code:?}")));
            }
            g.kernel = None;
            // P1-1（V13 审查）：内核死亡即吊销旧探活环令牌。若不递增，旧环在
            // 「自动重启 spawn 新内核（kernel.is_some()==true）→ 新内核就绪行
            // 递增 probe_gen」之间的窗口内令牌仍匹配，且端口可能尚未就绪 →
            // 3 次失联误杀健康新内核（端口漂移/冷启动 >7s 形态）。probe_gen
            // 只由就绪行线程递增（覆盖瀑布与自动重启两路径）；此处吊销的是
            // 上一代内核的环，二者不冲突。
            g.probe_gen += 1;
            g.crash_count += 1;
            let v = g.crash.record_crash(now);
            (v, g.crash_count)
        };
        // 内核已收尸：撤孤儿记账（下次启动不该再去收割这个已死的 pid）。
        kernel_process::stale_kernel::remove(&self.pid_file);
        log_line(&format!("内核退出 code={code:?} 第 {crashes} 次"));
        let _ = tx.send(SupervisorEvent::KernelExit { code, crashed: true });
        match verdict {
            // 崩溃环触发后冷却期内的后续崩溃：维持恢复页，不再自动拉起——
            // 此前 Cooldown 落在 `_` 兜底臂继续自动重启，恢复页背后内核反复
            // 拉起/退出（用户侧「频繁重启」的主机），且任一次走到就绪行还会
            // 经 KernelReady 把页面从恢复页拉回内核页（页面反复横跳）。
            Verdict::Tripped | Verdict::Cooldown => {
                if self.try_auto_rollback_overlay(tx, "崩溃环触发") {
                    return;
                }
                self.enter_recovery_tx(tx, "崩溃环触发");
            }
            Verdict::Ok => {
                // 未成环：自动重启一次（Electron watchServerProc 语义：异常退出自动拉起）。
                // 探活/换页不在此布防：新内核的就绪行线程统一负责（probe_gen 令牌）。
                let port = self.inner.lock().unwrap_or_else(|p| p.into_inner()).port;
                let gen = self.inner.lock().unwrap_or_else(|p| p.into_inner()).generation;
                let this = Arc::clone(self);
                let tx2 = tx.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_secs(2));
                    let g = this.inner.lock().unwrap_or_else(|p| p.into_inner());
                    // 状态门（VB1 残留缺口收口）：自动重启仅覆盖 **Ready 态的运行期
                    // 崩溃**。boot 期（Spawn/Repair 等）的内核退出归瀑布——
                    // ready_tx 已把「内核启动期退出」按本次拉起失败回报给
                    // spawn_and_wait_ready，瀑布二/三层会接管重拉；此前的 2s
                    // 自动重启线程与瀑布二层 spawn 竞速（瀑布先重跑 sidecar
                    // boot，>2s），会产出双内核：自动重启的内核占住端口后被
                    // g.kernel 覆写泄漏，瀑布层内核绑定失败连锁退出（慢机
                    // 「启动期 CPU 尖峰/反复拉起」形态）。唤醒时复查（而非仅
                    // 布防时）：restart() 后到 boot_waterfall 置 Repair 前有
                    // 毫秒级 Ready 空窗，旧线程须在此自灭。
                    if g.stopping || g.generation != gen || g.kernel.is_some() || g.state != RunState::Ready {
                        return;
                    }
                    drop(g);
                    this.refresh_safe_overlay();
                    if let Some(p) = port {
                        // #155 稳定化等待：内核刚死（探活/退出判定）时监听端口
                        // 未必即刻归还（Windows taskkill 子孙收割异步 / TIME_WAIT）。
                        // 直接 spawn 同一端口会 EADDRINUSE 秒退，被当作又一次
                        // 崩溃计入崩溃环（0.5.3「内核反复拉起」形态）。先等端口
                        // 释放；等不到就换 OS 随机端口——绝不把忙端口交给内核。
                        let target = if kernel_process::port::wait_port_free(p, Duration::from_secs(3)) {
                            p
                        } else {
                            log_line(&format!("自动重启：端口 {p} 3s 内未释放，换新端口"));
                            // K3 双 web 实例排查：内核已死但端口仍被占用 =
                            // 旧内核未彻底收割（孤儿/杀不净）或另一实例持有——
                            // 输出持有者诊断供用户报障。
                            log_port_holder_diag(p);
                            match kernel_process::port::choose_stable_port(None) {
                                Some(np) => np,
                                None => {
                                    this.enter_recovery_tx(&tx2, "自动重启无可用端口");
                                    return;
                                }
                            }
                        };
                        if Arc::clone(&this).spawn_kernel(target, &tx2).is_err() {
                            this.enter_recovery_tx(&tx2, "自动重启失败");
                        }
                    }
                });
            }
        }
    }
}
