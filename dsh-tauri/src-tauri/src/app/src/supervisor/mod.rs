//! 内核 supervisor：boot 链 → spawn → 就绪换页 → 探活 → 崩溃环 → 原地重启。
//!
//! 数据流契约（contracts/data-flow.md §3）：
//! ```text
//! sidecar boot（repair→sync→patches→compat-pin→preflight）
//!   → choose_stable_port（优先上次端口）
//!   → spawn vendor-node（环境白名单 + DSH_DESKTOP_SUPERVISED=1）
//!   → ReadyLineParser → kernel-ready → 主窗换页
//!   → 探活（TCP + 进程 wait）→ 崩溃环 → 恢复页
//! ```
//! 杀树：Windows taskkill /T /F（Electron 版实证：控制台进程优雅 kill 无效）；
//! Unix killpg(-pgid, SIGKILL)——spawn 时设内核为进程组长，整组一次收割
//! （kernel_process::kill_tree）。
//!
//! WSL 托管模式（契约 wsl-backend.md，两模式共用瀑布/崩溃环/看门狗，零变更）：
//! boot 线程首步 configure（失败回落 local——issue #54，不阻塞不恢复页）→
//! ensure_installed（先于插件/补丁链）→ sidecar boot `--home <UNC>` →
//! spawn `wsl.exe -d <distro> -e sh -lc`（--port 0，实际端口从就绪行解析）→
//! 收割三层（WSL 内 pid 文件 kill + 杀 wsl.exe 包装 + 300ms 缓冲；
//! wsl 的发行版级 terminate/shutdown 全局终结命令**绝不调用**——见契约
//! §4.6 红线）。local 模式全链行为零变更（不变量 §7.1）。

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use kernel_process::{choose_stable_port, kill_tree, node_compile_cache_dir, sanitized_node_command, CrashLoopDetector, ReadyLineParser, SpawnSpec};
use kernel_process::crash_loop::Verdict;
/// 稳定落定窗口（Electron SERVICE_STABLE_MS 同语义：就绪后稳定存活此时长，
/// 启动快照才成为「最后良好」回滚锚点）。
const SERVICE_STABLE_SECS: u64 = 45;
/// boot 看门狗上限（D2「永挂形态」根治）：boot 全链有界 5 分钟。
const BOOT_WATCHDOG_TIMEOUT: Duration = Duration::from_secs(300);
/// WSL 分支看门狗上限（契约 §4.2）：npm 首装 30 分钟超时 + boot 链余量。
const BOOT_WATCHDOG_TIMEOUT_WSL: Duration = Duration::from_secs(35 * 60);
/// 假死判定阈值（HTTP 无响应连续探活次数；3s × 20 = 60s）。存在进行中
/// agent 回合时豁免（issue #159），见 [`should_restart_zombie`]。
const ZOMBIE_THRESHOLD: usize = 20;
/// 假死「有回合」连续豁免上限（stale 计数兜底）：active_turns 因 watcher
/// 崩溃/会话日志损坏而滞留 >0 时，最多再豁免 N 个阈值窗口（N×60s），之后
/// 仍强制受控重启——保真死内核仍能被假死重启兜底（issue #159）。
const ZOMBIE_DEFER_MAX: usize = 3;
/// 内核就绪后前 60s 宽限期：冷加载 30+ 插件 + 首屏会话拉取期间事件循环易短时抖动，
/// 将单次 HTTP 探活读/写超时适当放宽到 5s，避免启动首分钟出现无意义假死告警。
const PROBE_GRACE_WINDOW: Duration = Duration::from_secs(60);
const PROBE_GRACE_HTTP_TIMEOUT: Duration = Duration::from_secs(5);
const PROBE_NORMAL_HTTP_TIMEOUT: Duration = Duration::from_secs(3);

use shell_core::RunState;

mod boot;
mod probe;
mod proxy;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod stability_tests;

pub(crate) use proxy::system_proxy_env_injection;
#[cfg(test)]
use proxy::{parse_wininet_proxy_url, proxy_env_injection};
#[cfg(all(test, windows))]
use probe::filetime_ms;

/// supervisor 对外事件（发给装配层，转发给窗口/托盘/日志）。
#[derive(Debug, Clone)]
pub enum SupervisorEvent {
    /// boot 链某步完成。
    BootStep { name: String, ok: bool, ms: u64, error: Option<String> },
    /// 内核就绪，主窗应换页到该 URL。
    KernelReady { url: String, port: u16 },
    /// 内核退出（异常）。
    KernelExit { code: Option<i32>, crashed: bool },
    /// 崩溃环触发 → 切恢复页。
    CrashLoop { crashes: usize },
    /// 探活失败计数变化（诊断用）。
    ProbeFailed { consecutive: usize },
    /// 内核假死可疑：TCP 可连但 HTTP 连续无响应（#122/#129——crash-shield
    /// 吞异常保进程活着，纯 TCP 探测恒过，事件循环卡死不可见）。
    ZombieSuspect { consecutive: usize },
    /// 状态迁移。
    StateChanged(RunState),
}

pub struct Supervisor {
    pub app_dir: PathBuf,
    pub sidecar_cli: PathBuf,
    pub node_exe: PathBuf,
    /// Node 三级解析链结果（v0.5.4：系统 PATH ≥22 → 内置 vendor → None）。
    /// `None` = 两级全缺——boot 瀑布首步发清晰 BootStep 错误转恢复页
    /// （替代旧「sidecar spawn os error 2」不可读形态）；`node_exe` 此时为
    /// vendor 主名占位（终防线，正常流程到不了 spawn）。
    pub node_resolved: Option<kernel_process::node_resolve::ResolvedNode>,
    pub bin_js: PathBuf,
    pub kernel_version: String,
    inner: Arc<Mutex<Inner>>,
    /// WSL 后端运行态（boot 线程 configure 成功后 Some；未配置/回落 local 为
    /// None——所有 WSL 分支以 `wsl_active().is_some()` 守卫，local 零变更）。
    wsl: Arc<Mutex<Option<Arc<wsl_backend::WslBackend>>>>,
    /// 启动期 WSL 探测失败回落 local 的原因（#54；本次运行期有效，设置页展示）。
    fallback_reason: Arc<Mutex<String>>,
    /// 后端模式意图（settings 三键 + env 覆盖，`new` 时定死：watchdog 时长与
    /// boot 线程 configure 的输入；探测本身延迟到 boot 线程——setup 零 wsl.exe）。
    wsl_cfg: Option<wsl_backend::BackendCfg>,
    /// wsl.exe 原语（生产 RealWslInvoker；测试注入桩——design D7）。
    wsl_invoker: Arc<dyn wsl_backend::WslInvoker>,
    /// 孤儿内核记账文件（`<app_data>/kernel.pid.json`）：spawn 后写、退出/杀树
    /// 时删，下次 boot 起点收割（壳闪退后内核残留 → 双 web 实例的兜底）。
    pid_file: PathBuf,
}

/// 现役 agent 内核解析结果（覆盖层 `<app_data>/agent` 优先于内置 `app_dir`，
/// 且仅在覆盖层版本严格高于内置版本时生效；新安装包内置版本 >= 覆盖层时自动切回 bundled）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveAgent {
    pub bin_js: PathBuf,
    pub version: String,
    pub source: String,
}

struct Inner {
    state: RunState,
    active_agent: ActiveAgent,
    kernel: Option<Child>,
    kernel_url: Option<String>,
    port: Option<u16>,
    last_error: Option<String>,
    crash: CrashLoopDetector,
    crash_count: usize,
    /// 注入内核的 --patch overlay 列表（picker 降级 / safe-boot 禁用）。
    overlays: Vec<std::path::PathBuf>,
    /// 守护瀑布的就绪等待通道（spawn_boot 同步段持有 rx；stdout 线程/退出路径发 tx）。
    ready_tx: Option<std::sync::mpsc::Sender<Result<String, String>>>,
    /// 待落定良好快照 id（就绪稳定 SERVICE_STABLE_SECS 后 markGood）。
    pending_good: Option<String>,
    /// restart_service 的代际号：旧世代的异步任务看到代际变了就自杀。
    generation: u64,
    /// 探活环令牌：每次就绪布防递增，旧探活环看到令牌不符自行退出
    /// （同代际下崩溃自动重启换内核后，旧环不得继续探活/误杀新内核）。
    probe_gen: u64,
    stopping: bool,
    /// 本次运行是否已因崩溃环自动回退过 updated 覆盖层（防无限回退循环）。
    overlay_auto_rolled_back: bool,
    /// 本次 boot 瀑布起点的 dsh-web.log 偏移（内核报错提取作用域，None=未
    /// 记账：恢复页不得引用上一次运行的残留输出——那会把旧根因安到新失败上）。
    log_mark: Option<u64>,
}


/// vendor node 路径解析已迁移 `kernel_process::node_resolve`（v0.5.4 三级
/// 解析链：系统 PATH ≥22 → 内置 vendor → None；vendor 平台主名/备名选择、
/// 版本解析的单测见该 crate）。此处仅保留语义注记：
/// - vendor 可执行名 Windows node.exe、其余平台 node（vendor 目录按平台
///   分发双二进制——mac 检出内 node 为 Mach-O）；
/// - vendor 调用在用户机上可能被 AV/SmartScreen 拦到半死——`--version`
///   探测自带 5s 超时（node_resolve::VERSION_PROBE_TIMEOUT），永挂防线
///   另有 boot 看门狗兜底。

impl Supervisor {
    pub fn new(repo_root: &std::path::Path) -> Self {
        Self::new_with_wsl_invoker(
            repo_root,
            Arc::new(wsl_backend::RealWslInvoker) as Arc<dyn wsl_backend::WslInvoker>,
        )
    }

    /// 测试构造（wsl.exe 原语注桩——design D7；生产恒 `new` + RealWslInvoker）。
    fn new_with_wsl_invoker(repo_root: &std::path::Path, invoker: Arc<dyn wsl_backend::WslInvoker>) -> Self {
        Self::new_with_probes(
            repo_root,
            invoker,
            Arc::new(kernel_process::RealNodeProbe) as Arc<dyn kernel_process::NodeProbe>,
        )
    }

    /// 全注桩构造（wsl.exe + node 探测双缝——D7 同手法；Node 解析优先级与
    /// 缺失路径的确定性测试用，生产恒 `new`）。
    fn new_with_probes(
        repo_root: &std::path::Path,
        invoker: Arc<dyn wsl_backend::WslInvoker>,
        node_probe: Arc<dyn kernel_process::NodeProbe>,
    ) -> Self {
        let app_dir = repo_root.join("dsh-desktop");
        // sidecar cli 双布局解析：开发检出在 <repo>/dsh-tauri/sidecar/，
        // 安装产物在 <安装根>/resources/sidecar/（repo_root 即 resources）。
        // 曾实测：只认开发布局时安装包首启 node 秒退「Cannot find module」，
        // 瀑布终态恢复页且全程零 stderr——最难排查的一类静默故障。
        let sidecar_cli = {
            let dev = repo_root.join("dsh-tauri").join("sidecar").join("cli.js");
            if dev.exists() {
                dev
            } else {
                repo_root.join("sidecar").join("cli.js")
            }
        };
        // 后端模式意图（env > settings 三键；与 sidecar wsl-mode.js detectWslBackend
        // 同口径，含 DSH_WSL_MODE 模拟缝）。只解析不探测——wsl.exe 冷启动可达
        // 数十秒，setup 线程零调用（design 5.3），探测延迟到 boot 线程。
        let wsl_cfg = {
            let store = shell_core::SettingsStore::new(shell_core::DshPaths::resolve().settings);
            let map = store.load().unwrap_or_default();
            let cfg = wsl_backend::detect_backend_mode_from_map(&map);
            (cfg.backend == "wsl").then_some(cfg)
        };
        // ---- Node 三级解析链（v0.5.4 便携版修复基础，构造期一次性）：内置
        //      vendor/node（探测健康）优先 → 系统 PATH 中 ≥22 的 node → None。装机
        //      版优先用厂商测过的内置 node，杜绝系统 node 大版本差异（如 node24）
        //      击穿内核 ESM loader；便携版无内置 node 时回落系统 node。----
        let node_resolved = kernel_process::node_resolve::resolve_node_with(
            node_probe.as_ref(),
            kernel_process::node_resolve::existing_vendor_node(&app_dir),
        );
        let node_exe = node_resolved
            .as_ref()
            .map(|r| r.exe().to_path_buf())
            // 全缺占位：保持「spawn 报错 → 恢复页」旧错误路径存活（boot 首步
            // 的清晰错误正常先一步拦截，此处仅终防线）。
            .unwrap_or_else(|| kernel_process::node_resolve::vendor_node_exe(&app_dir));
        let paths = shell_core::DshPaths::resolve();
        let active_agent = resolve_active_agent(&app_dir, &paths.app_data);
        Self {
            sidecar_cli,
            node_exe,
            node_resolved,
            bin_js: app_dir.join("node_modules").join("@deepseek-ai").join("dsh").join("lib").join("bin.js"),
            kernel_version: read_kernel_version(&app_dir),
            app_dir,
            inner: Arc::new(Mutex::new(Inner {
                state: RunState::Boot,
                active_agent,
                kernel: None,
                kernel_url: None,
                port: None,
                last_error: None,
                crash: CrashLoopDetector::new(),
                crash_count: 0,
                overlays: Vec::new(),
                ready_tx: None,
                pending_good: None,
                generation: 0,
                probe_gen: 0,
                stopping: false,
                overlay_auto_rolled_back: false,
                log_mark: None,
            })),
            wsl: Arc::new(Mutex::new(None)),
            fallback_reason: Arc::new(Mutex::new(String::new())),
            wsl_cfg,
            wsl_invoker: invoker,
            pid_file: paths.app_data.join("kernel.pid.json"),
        }
    }

    pub fn active_agent(&self) -> ActiveAgent {
        self.inner.lock().unwrap_or_else(|p| p.into_inner()).active_agent.clone()
    }
    pub fn refresh_active_agent(&self) -> ActiveAgent {
        let next = resolve_active_agent(&self.app_dir, &shell_core::DshPaths::resolve().app_data);
        self.inner.lock().unwrap_or_else(|p| p.into_inner()).active_agent = next.clone();
        next
    }
    pub fn state(&self) -> RunState {
        self.inner.lock().unwrap_or_else(|p| p.into_inner()).state
    }
    pub fn kernel_url(&self) -> Option<String> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner()).kernel_url.clone()
    }
    pub fn crash_count(&self) -> usize {
        self.inner.lock().unwrap_or_else(|p| p.into_inner()).crash_count
    }
    pub fn last_error(&self) -> Option<String> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner()).last_error.clone()
    }

    // ---- WSL 托管模式访问面（commands 层消费；local 模式零开销）----

    /// WSL 后端是否生效（configure 成功且未回落）。
    pub fn wsl_active(&self) -> Option<Arc<wsl_backend::WslBackend>> {
        self.wsl.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
    /// 实际生效后端（运行态；供 `wsl_config_get` 的 backend 字段）。
    pub fn backend_effective(&self) -> &'static str {
        if self.wsl_active().is_some() { "wsl" } else { "local" }
    }
    /// 启动期 WSL 探测失败回落 local 的原因（空 = 无回落）。
    pub fn fallback_reason(&self) -> String {
        self.fallback_reason.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
    /// WSL 状态快照（契约 §2.1 status 对象；未配置/回落 = configured:false 全空）。
    pub fn wsl_status_json(&self) -> serde_json::Value {
        match self.wsl_active() {
            Some(b) => b.status_json(),
            None => serde_json::json!({
                "configured": false, "distro": "", "installDir": "",
                "nodeVersion": "", "npmVersion": "", "agentVersion": "", "lastError": "",
            }),
        }
    }
    /// Windows 侧数据落点统一出口（契约 §6：local → DshPaths::dsh_home；
    /// wsl → UNC 等价路径）。
    pub fn effective_home(&self) -> PathBuf {
        match self.wsl_active() {
            Some(b) => b.unc_home(),
            None => shell_core::DshPaths::resolve().dsh_home,
        }
    }

    fn set_state(&self, next: RunState) {
        let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if g.state != next {
            let _ = g.state.can_transition_to(next);
            g.state = next;
        }
    }


    fn cancelled(&self, gen: u64) -> bool {
        let g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        g.generation != gen || g.stopping
    }

    /// 端口复用（同端口重试保 origin 稳定）；占用则换新端口。
    fn reuse_or_new_port(&self, preferred: u16) -> u16 {
        if let Some(p) = choose_stable_port(Some(preferred)) {
            return p;
        }
        // #155 EADDRINUSE 崩溃环：期望端口仍被占用（残留进程/AV 锁）时，
        // 绝不能把忙端口交给内核（绑定失败秒退 → 被误判崩溃）。换 OS 随机
        // 安全端口；随机分配失败才退回期望端口（终防线，正常流程到不了）。
        // K3：占用者可能是另一个 dsh web 实例/孤儿内核（端口 7388 复用形态）——
        // 输出持有者诊断（pid/进程名/启动时间），便于用户报障。
        log_port_holder_diag(preferred);
        if let Some(p) = choose_stable_port(None) {
            return p;
        }
        log_line(&format!("端口 {preferred} 占用且 OS 随机分配失败，仍尝试期望端口（终防线）"));
        preferred
    }

    /// guard 子命令薄跑（stdout 末行 JSON 解析；失败返回 None——瀑布降级而非崩）。
    fn guard_cli_json(&self, args: &[&str]) -> Option<serde_json::Value> {
        let out = sanitized_node_command(&self.node_exe)
            .arg(&self.sidecar_cli)
            .args(args)
            .arg("--app-dir")
            .arg(&self.app_dir)
            .args(self.sidecar_home_args())
            .creation_flags_win()
            .output()
            .ok()?;
        if !out.status.success() { return None; }
        let stdout = String::from_utf8_lossy(&out.stdout);
        let line = stdout.trim_end().lines().last()?;
        serde_json::from_str(line).ok()
    }

    /// 事故报告落盘（guard/incidents/）。
    fn guard_incident(&self, kind: &str, detail: &str) {
        let _ = self.guard_cli_json(&["guard-incident", kind, detail]);
    }

    // ---- 插件保护中心交互面（guard:action 迁移，V16-G1 收口）----
    // 只读/轻量查询面：读守护瀑布已落盘的状态与报告。写动作（snapshot /
    // restore / repair）仍走守护瀑布自动面（boot_waterfall），此处不暴露，
    // 避免手动回滚与自动瀑布竞态（快照/回滚发生在「无服务进程持锁」窗口期，
    // 交互期调用会与运行中的内核文件锁撞车）。

    /// guard:action status：快照列表 + 未解决事故列表 + 最后良好快照（只读）。
    pub fn guard_status(&self) -> Option<serde_json::Value> {
        self.guard_cli_json(&["guard-status"])
    }

    /// guard:action check：静态体检（healthCheck findings，不执行修复）。
    pub fn guard_check(&self) -> Option<serde_json::Value> {
        self.guard_cli_json(&["guard-health"])
    }

    /// guard:action incident：读单条事故详情（content 截断 30KB）。
    pub fn guard_incident_read(&self, id: &str) -> Option<serde_json::Value> {
        self.guard_cli_json(&["guard-read-incident", id])
    }

    /// guard:action resolve-incident：把事故重命名为 .resolved.md（软解决）。
    pub fn guard_resolve_incident(&self, id: &str) -> Option<serde_json::Value> {
        self.guard_cli_json(&["guard-resolve-incident", id])
    }

    fn inner_crash_reset(&self) { /* 兼容占位：crash_count 复位已直写 */ }

    /// sidecar 子命令的 `--home` 追加参数：WSL 生效时 home = UNC（guard-*/
    /// safe-overlay 等以 ctx.home 为数据根的子命令不自检 WSL，必须显式切；
    /// boot 子命令自检 WSL 后自取 UNC home，--home 仅作其回落 local 时的
    /// 显式值——两侧一致）。local 模式返回空（现行为零变更）。
    fn sidecar_home_args(&self) -> Vec<String> {
        match self.wsl_active() {
            Some(b) => vec!["--home".into(), b.unc_home().to_string_lossy().into_owned()],
            None => Vec::new(),
        }
    }

    /// Node 24 原生 V8 编译缓存目录（挂在 userData 下，自动随 DSH_TAURI_USERDATA 隔离）。
    fn compile_cache_dir(&self) -> PathBuf {
        let dir = node_compile_cache_dir(&shell_core::DshPaths::resolve().app_data);
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    /// 原地重启（restart_service）：杀树 → 刷新现役内核 → 重跑 boot 链 → 换页。
    pub fn restart(self: &Arc<Self>, tx: Sender<SupervisorEvent>, preferred_port: Option<u16>) {
        {
            let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
            g.generation += 1;
            g.stopping = false;
            g.overlay_auto_rolled_back = false;
            g.kernel_url = None;
            g.crash.record_graceful_restart();
        }
        self.kill_kernel();
        self.refresh_active_agent();
        self.spawn_boot(tx, preferred_port);
    }

    /// 覆盖层崩溃环自动回退（1.1）：若现役内核来自热更新覆盖层（`source == "updated"`）
    /// 且本轮尚未自动回退过，先杀树、把 `<app_data>/agent` 隔离到 `agent-broken-<ts>`，
    /// 切回 `bundled` 内核并自动重启一次，杜绝坏版本覆盖层导致打不开客户端。
    fn try_auto_rollback_overlay(self: &Arc<Self>, tx: &Sender<SupervisorEvent>, reason: &str) -> bool {
        let active = self.active_agent();
        if active.source != "updated" {
            return false;
        }
        let port = {
            let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
            if g.overlay_auto_rolled_back {
                return false;
            }
            g.overlay_auto_rolled_back = true;
            g.port
        };
        self.kill_kernel();
        std::thread::sleep(Duration::from_millis(200));
        let app_data = shell_core::DshPaths::resolve().app_data;
        match quarantine_broken_agent_overlay(&app_data) {
            Ok(Some(dest)) => {
                log_line(&format!(
                    "覆盖层自动回退：updated v{} 触发「{reason}」，已隔离至 {} 并切回 bundled",
                    active.version,
                    dest.display()
                ));
                self.guard_incident(
                    "agent-overlay-rollback",
                    &format!(
                        "热更新 Agent 覆盖层 v{} 异常（{reason}），已自动隔离至 {} 并回退内置版本重试。",
                        active.version,
                        dest.display()
                    ),
                );
            }
            Ok(None) => {
                log_line(&format!("覆盖层自动回退：未发现 agent 目录（{reason}），直接切回 bundled"));
            }
            Err(e) => {
                log_line(&format!("覆盖层自动回退失败（{e}），转交恢复页"));
                return false;
            }
        }
        let fallback = self.refresh_active_agent();
        log_line(&format!("已切回 {} v{}，自动重新拉起内核", fallback.source, fallback.version));
        {
            let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
            g.generation += 1;
            g.stopping = false;
            g.kernel_url = None;
            g.crash = CrashLoopDetector::new();
            g.crash_count = 0;
        }
        self.spawn_boot(tx.clone(), port);
        true
    }

    /// 进入恢复页。
    fn enter_recovery(&self, tx: &Sender<SupervisorEvent>, reason: &str) {
        self.enter_recovery_tx(tx, reason);
    }
    /// 本次瀑布内核报错的附加说明（`\n内核报错：…`；无账/无输出时为空串）。
    /// 事故报告（guard/incidents/）与恢复页共用，用户反馈时不再丢最关键的
    /// message 行——此前壳侧 reason 只有概括（「启动失败且无可回滚快照」），
    /// 真实根因（如 `TypeError [ERR_INVALID_ARG_TYPE]: The "paths[0]" …`）
    /// 只埋在 dsh-web.log 深处，QQ 群排查只能靠截图盲猜。
    fn kernel_error_suffix(&self) -> String {
        let mark = self.inner.lock().unwrap_or_else(|p| p.into_inner()).log_mark;
        match kernel_error_note(mark) {
            Some(note) => format!("\n{note}"),
            None => String::new(),
        }
    }
    fn enter_recovery_tx(&self, tx: &Sender<SupervisorEvent>, reason: &str) {
        self.kill_kernel();
        // 幂等：已在崩溃环态（冷却期内后续崩溃）不再重发 CrashLoop 事件——
        // 事件会再导航恢复页 + 弹系统通知，崩溃连环下发会刷屏。
        let already = self.state() == RunState::CrashLoop;
        self.set_state(RunState::CrashLoop);
        // 壳侧 reason 只是概括；附上本次运行的内核报错尾行（真实根因）。
        let suffix = self.kernel_error_suffix();
        let reason_owned = if suffix.is_empty() { reason.to_string() } else { format!("{reason}{suffix}") };
        {
            let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
            g.last_error = Some(reason_owned);
            // P2-3（V13 审查）：吊销在途瀑布。看门狗/崩溃环开火时若不递增
            // 代际，慢 boot 步（AV 拖 300s）返回后 continue 的瀑布会继续
            // spawn 内核（boot_waterfall 只在每步间查 cancelled(gen)），把
            // 状态从 CrashLoop 拉回 Ready → 页面横跳。递增后该瀑布的
            // cancelled(gen) 命中代际不符即中止，恢复页保持。
            g.generation += 1;
            // 内核已死，URL 随之作废——不清则恢复页「重新加载」会按 stale URL
            // 换页到已死端口（真机复现：ERR_CONNECTION_REFUSED 错误页，无任何
            // 可操作按钮，比停在 loading 页更糟）。清空后 reload 走重启分支。
            g.kernel_url = None;
        }
        if already {
            log_line(&format!("崩溃环冷却期内再次崩溃，维持恢复页（{reason}）"));
            return;
        }
        log_line(&format!("崩溃环触发，转恢复页（{reason}）"));
        let crashes = self.inner.lock().unwrap_or_else(|p| p.into_inner()).crash_count;
        let _ = tx.send(SupervisorEvent::CrashLoop { crashes });
    }

    /// 恢复页「重启」：手动复位崩溃环。
    pub fn recovery_restart(self: &Arc<Self>, tx: Sender<SupervisorEvent>) {
        self.recovery_restart_with_port(tx, None);
    }

    /// 恢复页「重启」（带优先端口）：复位崩溃环 + 全链重启。
    /// preferred 传上次内核端口（origin 稳定，SPA localStorage 偏好不丢）。
    pub fn recovery_restart_with_port(self: &Arc<Self>, tx: Sender<SupervisorEvent>, preferred_port: Option<u16>) {
        {
            let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
            g.crash.record_recovery();
            g.crash_count = 0;
            g.last_error = None;
        }
        self.set_state(RunState::Recovery);
        self.restart(tx, preferred_port);
    }

    /// K3 终态兜底（2026-08）：渲染进程反复死亡、浏览器进程级重载
    /// （CoreWebView2.Reload / Navigate）也救不活的形态（v0.5.3 白屏：
    /// WebView2 渲染进程 OOM 崩溃后白屏无限挂，实证「整窗重启」是唯一有效
    /// 恢复）——supervisor 级内核重启。语义 = 强制内核按退出处理：
    /// `kill_kernel()` 收割当前内核树，`on_kernel_exit(None)` 走崩溃环判定
    /// ——未成环走自动重启链拉起新内核（新内核就绪行线程统一换页 + 布防
    /// 新一代心跳监测）；成环进恢复页（与崩溃环窗口限次/恢复页互斥协同，
    /// 不双杀）。调用方为 renderer 心跳监测线程（lib.rs `kernel_restart_escape`）。
    pub fn restart_kernel_after_renderer_escape(self: &Arc<Self>, tx: &Sender<SupervisorEvent>) {
        log_line("[renderer-recovery] 升级到 supervisor 级内核重启（整窗重启是唯一有效恢复）");
        self.kill_kernel();
        self.on_kernel_exit(None, tx);
    }

    /// 杀内核整树（restart / 恢复页 / 探活失败 / 应用退出共用）。
    ///
    /// local：Windows taskkill /T /F；Unix：killpg(-pgid, SIGKILL) 整组收割
    /// ——OS 绑定见 kernel_process::kill_tree（本函数仅持锁取 child + 派发）。
    ///
    /// WSL：三层收割（契约 §4.6；发行版级 terminate/shutdown 全局终结命令
    /// **绝不调用**——那会终结整个发行版内用户的其他进程）：
    /// ① WSL 内按 pid 文件 kill（另一条 wsl.exe 调用，≤30s；taskkill /T 对
    ///   WSL 内进程无效——不在 Windows 进程树，/T 枚举不到）；
    /// ② 杀 wsl.exe 包装 child + 收尸；
    /// ③ 300ms 缓冲后再进端口探测（Electron killTree WSL 分支语义）。
    pub fn kill_kernel(&self) {
        let backend = self.wsl_active();
        let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        // 显式杀树 = 由我们收尸，撤孤儿记账（防下次启动把已死 pid 当孤儿）。
        kernel_process::stale_kernel::remove(&self.pid_file);
        if let Some(mut c) = g.kernel.take() {
            let pid = c.id();
            let port = g.port;
            match &backend {
                Some(b) => {
                    // P2-2（V13 审查）：b.stop()（WSL 内 pid 文件收割）最长可
                    // 阻塞 30s，全程持 inner 锁会堵死所有 supervisor 查询
                    // （state()/kernel_url()/crash_count() 与探活环判活）。
                    // child 已 take 到局部、锁先释放再 stop/kill/wait
                    // （与 local 分支同手法）。
                    let backend = Arc::clone(b);
                    drop(g);
                    backend.stop();
                    let _ = c.kill();
                    let _ = c.wait();
                    std::thread::sleep(Duration::from_millis(300));
                    if let Some(p) = port {
                        if kernel_process::port::wait_port_free(p, Duration::from_secs(5)) {
                            log_line(&format!("WSL 收割完成，端口 {p} 已确认释放"));
                        } else {
                            log_line(&format!("WSL 收割后端口 {p} 5s 内未释放，后续换新端口"));
                        }
                    }
                }
                None => {
                    kill_tree(&mut c, pid);
                    drop(g);
                    // #155 EADDRINUSE 4311 崩溃环第一根因：旧内核被杀到监听
                    // socket 真正归还之间存在窗口期（taskkill /T /F 子孙收割
                    // 异步 / TIME_WAIT 残留），新内核在窗口内 spawn 同一端口
                    // 会绑定失败秒退 → 被误判为崩溃计入崩溃环。这里等端口
                    // 确认空闲再返回（restart / 恢复页 / 探活失败共用本出口），
                    // 等不到就交给 choose_stable_port 换新端口。
                    if let Some(p) = port {
                        if kernel_process::port::wait_port_free(p, Duration::from_secs(5)) {
                            log_line(&format!("杀树完成，端口 {p} 已确认释放"));
                        } else {
                            log_line(&format!("杀树后端口 {p} 5s 内未释放（AV/残留进程占用），后续换新端口"));
                            // K3 双 web 实例排查：杀树后端口仍未归还 = 旧内核子孙
                            // 未死净（孤儿监听）或另一 dsh web 实例持有——输出
                            // 持有者诊断供用户报障（desktop.log 可取证）。
                            log_port_holder_diag(p);
                        }
                    }
                }
            }
        } else {
            drop(g);
        }
    }

    /// 应用退出路径：同步终结（不依赖事件循环）。
    /// WSL 分支：WSL 内 stop fire-and-forget（退出不等 30s 上限；Electron
    /// killTreeSync 同款）+ 同步杀 wsl.exe 包装进程。
    pub fn shutdown(&self) {
        self.inner.lock().unwrap_or_else(|p| p.into_inner()).stopping = true;
        if let Some(b) = self.wsl_active() {
            std::thread::spawn(move || {
                b.stop();
            });
            let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(mut c) = g.kernel.take() {
                let _ = c.kill();
                let _ = c.wait();
            }
        } else {
            self.kill_kernel();
        }
    }
}

fn read_kernel_version(app_dir: &std::path::Path) -> String {
    let pkg = app_dir.join("node_modules").join("@deepseek-ai").join("dsh").join("package.json");
    let Ok(raw) = std::fs::read_to_string(pkg) else { return "unknown".into() };
    if let Some(pos) = raw.find("\"version\"") {
        if let Some(colon) = raw[pos..].find(':') {
            let rest = &raw[pos + colon..];
            if let Some(q1) = rest.find('"') {
                if let Some(len) = rest[q1 + 1..].find('"') {
                    return rest[q1 + 1..q1 + 1 + len].to_string();
                }
            }
        }
    }
    "unknown".into()
}

/// 解析现役 agent 内核：若 `<user_data_dir>/agent` 含完整 `bin.js` 且版本严格高于内置
/// `app_dir` 版本，返回 `source = "updated"`；否则回落 `bundled`（新客户端自带更高
/// 内核时自动接管，无需手动清理）。
pub(crate) fn resolve_active_agent(app_dir: &std::path::Path, user_data_dir: &std::path::Path) -> ActiveAgent {
    let bundled_bin = app_dir.join("node_modules").join("@deepseek-ai").join("dsh").join("lib").join("bin.js");
    let bundled_ver = read_kernel_version(app_dir);
    let overlay_root = user_data_dir.join("agent");
    let overlay_bin = overlay_root.join("node_modules").join("@deepseek-ai").join("dsh").join("lib").join("bin.js");
    let overlay_ver = read_kernel_version(&overlay_root);
    if overlay_bin.exists() && overlay_ver != "unknown" {
        let newer = bundled_ver == "unknown"
            || kernel_process::semver::compare_versions(&overlay_ver, &bundled_ver) == Some(std::cmp::Ordering::Greater);
        if newer {
            return ActiveAgent {
                bin_js: overlay_bin,
                version: overlay_ver,
                source: "updated".into(),
            };
        }
    }
    ActiveAgent {
        bin_js: bundled_bin,
        version: bundled_ver,
        source: "bundled".into(),
    }
}

/// 将损坏的 `<user_data_dir>/agent` 覆盖层隔离为 `agent-broken-<ms>`（保留现场排障）。
pub(crate) fn quarantine_broken_agent_overlay(user_data_dir: &std::path::Path) -> Result<Option<PathBuf>, String> {
    let overlay = user_data_dir.join("agent");
    if !overlay.exists() {
        return Ok(None);
    }
    let dest = user_data_dir.join(format!("agent-broken-{}", now_ms()));
    std::fs::rename(&overlay, &dest).map_err(|e| format!("隔离损坏覆盖层失败：{e}"))?;
    Ok(Some(dest))
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// 从就绪行 URL 提取端口（WSL 模式实际端口来源——契约 §4.3）：
/// 取最后一个 `:` 后的连续数字段（容忍尾随路径）。无端口 → None。
fn url_port(url: &str) -> Option<u16> {
    let idx = url.rfind(':')?;
    let digits: String = url[idx + 1..].chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// koffi-preflight 通过判定：stdout 末行 JSON 的 `ok == true`。
/// 此前用 `ends_with("{\"ok\":true}")` 字符串契约——sidecar 的「脚本缺失
/// 跳过」形态 `{"ok":true,"skipped":"no-script"}` 不满足该匹配（X2 指出），
/// 改按 JSON 语义判定（契约放宽为「末行 JSON ok 字段为 true」，WSL 跳过
/// 分支的逐字 `{"ok":true}` 亦满足）。
fn koffi_preflight_passed(stdout: &str) -> bool {
    stdout
        .trim_end()
        .lines()
        .last()
        .and_then(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .and_then(|v| v.get("ok").and_then(|x| x.as_bool()))
        .unwrap_or(false)
}

/// 从 netstat 文本提取监听指定端口的持有者 PID（纯解析，可单测）。
/// Windows `netstat -ano -p TCP` 行形态：
/// `  TCP    127.0.0.1:7388   0.0.0.0:0   LISTENING   12345`
/// 字段序：Proto / LocalAddr / ForeignAddr / State / PID。任何字段数不足
/// 或 State ≠ LISTENING 的行跳过（含首行表头，其 State 列为 "State"）。
/// 非 Windows 构建下仅测试引用（诊断主体走 lsof），allow(dead_code) 豁免。
#[cfg_attr(not(windows), allow(dead_code))]
fn parse_listening_pids(netstat_out: &str, port: u16) -> Vec<u32> {
    let needle = format!(":{port}");
    netstat_out
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() >= 5 && fields[1].ends_with(&needle) && fields[3] == "LISTENING" {
                fields[4].parse::<u32>().ok()
            } else {
                None
            }
        })
        .collect()
}

/// 双 web 实例诊断（K3）：端口被外部进程占用（另一个 dsh web 实例 / 未杀净
/// 的孤儿内核，K2「端口 7388 复用」形态）时，输出持有者信息
/// （pid / 进程名 / 启动时间）到 desktop.log——补充 synapse 侧「另一个 dsh
/// web 实例」告警的壳侧证据，用户报障可直接贴日志。OS 查询失败静默
///（诊断绝不影响主流程；枚举不到时仅记一句无法枚举）。
fn log_port_holder_diag(port: u16) {
    let diag = port_holder_diag_text(port);
    if diag.is_empty() {
        log_line(&format!("端口 {port} 被外部进程占用，但未能枚举持有者（权限受限/查询失败）"));
    } else {
        log_line(&format!("端口 {port} 被外部进程占用（疑似双 web 实例/孤儿内核），持有者诊断: {diag}"));
    }
}

/// 端口持有者诊断文本（pid/进程名/启动时间）。
/// - Windows：netstat 枚举监听 PID → tasklist 取进程名 → PowerShell 取启动
///   时间（Get-Process.StartTime）。
/// - 非 Windows：lsof -i TCP:<port> 取 pid → ps -o lstart 取启动时间。
fn port_holder_diag_text(port: u16) -> String {
    #[cfg(windows)]
    {
        let mut pids: Vec<u32> = Vec::new();
        if let Ok(o) = std::process::Command::new("netstat")
            .args(["-ano", "-p", "TCP"])
            .creation_flags_win()
            .output()
        {
            pids = parse_listening_pids(&String::from_utf8_lossy(&o.stdout), port);
            pids.dedup();
        }
        let mut parts: Vec<String> = Vec::new();
        for pid in pids {
            let name = std::process::Command::new("tasklist")
                .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
                .creation_flags_win()
                .output()
                .ok()
                .map(|o| {
                    String::from_utf8_lossy(&o.stdout)
                        .lines()
                        .next()
                        .unwrap_or("")
                        .split(',')
                        .next()
                        .unwrap_or("")
                        .trim_matches('"')
                        .to_string()
                })
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "未知".into());
            let start = std::process::Command::new("powershell")
                .args([
                    "-NoProfile",
                    "-Command",
                    &format!(
                        "$p=Get-Process -Id {pid} -ErrorAction SilentlyContinue; if($p){{$p.StartTime.ToString('yyyy-MM-dd HH:mm:ss')}}"
                    ),
                ])
                .creation_flags_win()
                .output()
                .ok()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "未知".into());
            parts.push(format!("pid={pid} 进程={name} 启动={start}"));
        }
        parts.join("；")
    }
    #[cfg(not(windows))]
    {
        let mut parts: Vec<String> = Vec::new();
        if let Ok(o) = std::process::Command::new("lsof").args(["-ti", &format!("TCP:{port}")]).output() {
            for line in String::from_utf8_lossy(&o.stdout).lines() {
                if let Ok(pid) = line.trim().parse::<u32>() {
                    if let Ok(p) = std::process::Command::new("ps")
                        .args(["-p", &pid.to_string(), "-o", "pid=,comm=,lstart="])
                        .output()
                    {
                        let row = String::from_utf8_lossy(&p.stdout).trim().to_string();
                        if !row.is_empty() {
                            parts.push(row);
                        }
                    }
                }
            }
        }
        parts.join("；")
    }
}

fn log_line(msg: &str) {
    // T4 反馈：无时间戳时恢复耗时只能外部计时——补 HH:MM:SS 前缀。
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (h, m, sec) = ((secs / 3600) % 24, (secs / 60) % 60, secs % 60);
    println!("[supervisor {h:02}:{m:02}:{sec:02}] {msg}");
    file_log(&format!("[supervisor {h:02}:{m:02}:{sec:02}] {msg}"));
}

/// 落盘日志（logs/desktop.log）：supervisor/路由事件双写（stdout + 文件）。
/// v0.5.2 真机实测发现：GUI 子系统无控制台，println/eprintln 在安装态无人
/// 接收；而 desktop.log 此前仅被诊断（diag-export）读取、无任何写入方——
/// 崩溃环/看门狗触发后排障时「打开日志」是空目录，恢复页「请导出日志反馈」
/// 无从取证。追加写失败静默（日志绝不影响主流程）。
pub fn file_log(line: &str) {
    // C3（B2 配方）：desktop.log 经 logging::append_capped 落盘（4MB 轮转
    // .old 只留一代；IO 失败静默）——此前 GUI 进程 stdout 丢弃，boot/路由
    // 日志从不落盘。时间戳口径不变（chrono_like_now）。
    crate::logging::append_capped(
        &shell_core::DshPaths::resolve().logs.join("desktop.log"),
        &format!("{} {line}", crate::chrono_like_now()),
        crate::logging::LOG_CAP_BYTES,
    );
}

#[cfg(windows)]
trait WinFlags {
    fn creation_flags_win(&mut self) -> &mut Self;
}
#[cfg(windows)]
impl WinFlags for Command {
    fn creation_flags_win(&mut self) -> &mut Self {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        self.creation_flags(CREATE_NO_WINDOW)
    }
}
#[cfg(not(windows))]
trait WinFlags {
    fn creation_flags_win(&mut self) -> &mut Self;
}
#[cfg(not(windows))]
impl WinFlags for Command {
    fn creation_flags_win(&mut self) -> &mut Self {
        self
    }
}

/// 检测 profile fallback farm（`~/.dsh/profiles/node_modules`）里是否存在被实体化
/// （非 symlink/junction 的真实目录）且 payload 也拥有的包。
///
/// 99.9% 正常启动时全为 junction/symlink，本纯 Rust 预检 <1ms 即返回 false，
/// 直接省掉拉起 `farm-repair.js` 的一次 node.exe 冷启动；命中时仍交由
/// `farm-repair.js` 执行完整挪开保留现场。
fn is_real_materialized_dir(path: &std::path::Path) -> bool {
    // read_link 对 POSIX symlink 与 Windows NTFS junction 均返回 Ok；
    // 真实目录返回 Err 且 symlink_metadata().is_dir() 为 true。
    if std::fs::read_link(path).is_ok() {
        return false;
    }
    std::fs::symlink_metadata(path).map(|m| m.is_dir()).unwrap_or(false)
}

fn has_materialized_farm_entry(farm: &std::path::Path, payload_nm: &std::path::Path) -> bool {
    let Ok(entries) = std::fs::read_dir(farm) else {
        return false;
    };
    if !payload_nm.exists() {
        return false;
    }
    for ent in entries.flatten() {
        let name = ent.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with(".materialized-") {
            continue;
        }
        let full = ent.path();
        if name_str.starts_with('@') {
            if let Ok(subs) = std::fs::read_dir(&full) {
                for sub in subs.flatten() {
                    let sub_name = sub.file_name();
                    if payload_nm.join(&name).join(&sub_name).exists() && is_real_materialized_dir(&sub.path()) {
                        return true;
                    }
                }
            }
        } else if payload_nm.join(&name).exists() && is_real_materialized_dir(&full) {
            return true;
        }
    }
    false
}


/// panic 载荷转字符串（&str / String / 其他兜底）。
pub(crate) fn panic_payload_str(p: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = p.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = p.downcast_ref::<String>() {
        s.clone()
    } else {
        "未知 panic 载荷".to_string()
    }
}

/// 从 dsh-web.log 自 `mark` 偏移起（本次 boot 瀑布的内核输出）提取最后一条
/// 报错行，组装成「内核报错：…」。None = 未记账 / 读不到 / 本次无报错行。
/// 全链不 panic 不阻断：诊断增强失败按无附加说明处理。
fn kernel_error_note(mark: Option<u64>) -> Option<String> {
    let mark = mark?;
    let path = shell_core::DshPaths::resolve().logs.join("dsh-web.log");
    let tail = read_log_since(&path, mark)?;
    let msg = summarize_kernel_error(&tail)?;
    Some(format!("内核报错：{msg}"))
}

/// 读取文件自 `mark` 字节偏移到当前末尾的内容（单次封顶 256KB——4MB 轮转
/// 文件被外部截断时 mark 可能远超末尾，此时 None；正常一次启动输出远小于
/// 封顶值）。全链静默失败。
fn read_log_since(path: &std::path::Path, mark: u64) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    if len <= mark {
        return None;
    }
    let start = mark.max(len.saturating_sub(256 * 1024));
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = String::new();
    f.read_to_string(&mut buf).ok()?;
    Some(buf)
}

/// 从内核输出尾部提取最后一条报错消息行（纯函数，可单测）。
///
/// dsh-web.log 的 `web-err| ` 前缀按 4KB 管道块只出现在块首（stderr 线程
/// 整块读取），块内换行不带前缀——统一按行剥前缀后当内核输出。排除栈帧
/// （`at …`）、`throw er` 头、Node 版本尾、错误对象的 `{`/`}` 壳后，命中
/// 「错误类名 / ERR_ 码 / fatal 自述」特征的行为消息行，取**最后**一条
/// （多轮报错时最后者最贴近退出现场）。
fn summarize_kernel_error(tail: &str) -> Option<String> {
    let mut last_msg: Option<&str> = None;
    for line in tail.lines() {
        let l = line.strip_prefix("web-err| ").unwrap_or(line).trim_end();
        let t = l.trim_start();
        if t.is_empty()
            || t.starts_with("at ")
            || t.starts_with("throw er")
            || t.starts_with("Node.js v")
            || t.starts_with('{')
            || t == "}"
        {
            continue;
        }
        if t.contains("Error") || t.contains("[ERR_") || t.contains("fatal") {
            last_msg = Some(t);
        }
    }
    last_msg.map(|m| m.chars().take(300).collect())
}

/// 取多行文本的尾部 n 行（跳过纯空白行，` | ` 连成单行日志）。
///
/// 与 summarize_kernel_error「取最后一条」同一取向：进程崩溃死因
/// （V8 fatal、OOM、abort 报告）总在输出末尾——头部保留会把死因裁掉。
fn tail_lines(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.is_empty() {
        return String::new();
    }
    let start = lines.len().saturating_sub(n);
    lines[start..].join(" | ")
}

/// sidecar boot stderr 的转出上限：一次 boot 实测 115 行 / 15KB（60 补丁全量、
/// 已收敛态），首启全量注入时更长——400 行足够覆盖，超出即截断且**显式注明**。
const SIDECAR_BOOT_LOG_CAP: usize = 400;

/// 纯函数：sidecar boot 的 stderr → 待转入壳层日志的行（trim、去空行、保序）。
/// 未超上限全量转出；超上限保留**末尾** cap 行，并在最前一行注明截断量——死因
/// 在末尾判据与 [`tail_lines`] 同源（V8 fatal/abort 报告总是最后才出）。
/// 空/纯空白输入 → 空集（不往 desktop.log 刷空行）。
fn sidecar_boot_log_lines(stderr: &str, cap: usize) -> Vec<String> {
    let lines: Vec<&str> = stderr.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    if lines.is_empty() {
        return Vec::new();
    }
    if lines.len() <= cap {
        return lines.iter().map(|l| l.to_string()).collect();
    }
    let skipped = lines.len() - cap;
    let mut out = Vec::with_capacity(cap + 1);
    out.push(format!(
        "[sidecar] boot stderr 共 {} 行，仅转出末尾 {} 行（丢弃前 {} 行）",
        lines.len(),
        cap,
        skipped
    ));
    out.extend(lines[skipped..].iter().map(|l| l.to_string()));
    out
}

#[cfg(test)]
impl Supervisor {
    /// 测试辅助：直接设置状态（绕过迁移表）。
    fn set_state_for_test(&self, s: RunState) {
        self.inner.lock().unwrap_or_else(|p| p.into_inner()).state = s;
    }
}
