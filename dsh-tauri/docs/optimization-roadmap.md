# DSH Pack 后续优化方案与分期推进计划

> 编写日期：2026-11-24  
> 适用分支：`custom`（基于 `bd7fcf02`）  
> 状态：**推进中**（第 1、2、3 期已完成并同步实机验证；第 4 期按确认推进）

---

## 零、现状基线与实测数据

基于当前本机最新一次冷启动日志（`%APPDATA%\dsh-desktop\logs\desktop.log`，2026-11-24）与代码审计：

| 阶段 / 指标 | 当前现状 | 备注 |
| --- | --- | --- |
| 壳侧 boot 五步合计 | **~0.68s** | `repair 153ms` + `sync 368ms` + `patches 59ms` + `compat-pin 1ms` + `preflight 97ms` |
| 内核进程冷启动至就绪 | **~16s**（`02:40:08 → 02:40:24`） | 当前启动最大瓶颈，全量加载 30+ 伴随插件 |
| 内置插件体积 Top 3 | `dsh-better-sidebar` 14.96MB / `billion-context` 8.99MB / `dsh-pocket` 2.65MB | `dsh-better-sidebar` 内含 2.55MB `src/` 源码目录 |
| Rust 构建告警 | **3 处 `dead_code`** | `commands/common.rs` ×2、`logging.rs` ×1 |
| 超大单文件 | `supervisor.rs`（3831 行）、`bridge-shim.js`（1035 行） | 高频修改落点集中，单文件阅读与维护成本高 |

以下将 4 个方向的优化拆解为可独立评审、独立提交、独立回滚的工程子项。

---

## 一、稳定性与容错闭环（P0，建议优先）

### 1.1 Agent 覆盖层崩溃环自动回退 `bundled`

- **问题**：已上线 `%APPDATA%\dsh-desktop\agent` 覆盖层热更新与菜单手动「恢复内置」，但如果未来某个上游 `@deepseek-ai/dsh` 稳定版存在破坏性变更，导致新覆盖层启动即崩并触发 `CrashLoop`，用户只能进入恢复页手动排障或删除目录。
- **方案**：
  1. 在 `dsh-tauri/src-tauri/src/app/src/supervisor.rs` 的崩溃环入口（`enter_recovery_tx` / `Verdict::Tripped`）之前增加自动兜底：
     - 若 `self.active_agent().source == "updated"` 且本轮尚未执行过自动回退：
       - 将 `%APPDATA%\dsh-desktop\agent` 原子挪为 `%APPDATA%\dsh-desktop\agent-broken-<ts>`（保留现场便于事后排障）；
       - 记一条 `guard_incident("agent-overlay-rollback", ...)` 事故摘要 + 写 `desktop.log`；
       - 调用 `refresh_active_agent()` 切回 `bundled`，重置崩溃环计数并自动重启一次内核。
  2. 在恢复页（`dsh-tauri/ui/recovery.html` + `commands/recovery.rs`）读取 `agentSource`，当仍处于 `updated` 时暴露「回退内置 Agent」按钮作为人工兜底。
- **涉及文件**：`supervisor.rs`、`commands/recovery.rs`、`dsh-tauri/ui/recovery.html`、`dsh-tauri/contracts/ipc-commands.md`（若新增恢复动作则同步契约；若直接复用 `menu_action('agent-reset-bundled')` 则**零新增 IPC 命令**）。
- **风险与验收**：风险低；新增 Rust 单测验证「updated 覆盖层崩溃触发自动隔离并回落 bundled；bundled 崩溃不触发无限循环」。

### 1.2 Agent 更新透出补丁适配报告

- **问题**：仓库维护 66 项运行时补丁；当通过「检查更新」升级到新版 `@deepseek-ai/dsh`（如 `0.2.2` / `0.2.3`）时，`stageAgentUpdate` 已在暂存目录执行 `applyAll`，但菜单侧尚未展示哪些补丁命中、哪些因上游变化出现 `anchor-missing` / `degraded`。
- **方案**：
  1. 在 `dsh-desktop/scripts/lib/agent-update.js` 的 `stageAgentUpdate` 返回体中规范化 `patchSummary`：`{ total, changed, anchorMissing, degraded, failed, missingIds: string[] }`。
  2. 在 `commands/menu.rs`（`agent-install-update`）透传 `patchSummary`（或精简的 `patchWarning` 文案），同时落盘 `desktop.log`。
  3. 在 `bridge-shim.js` 的菜单状态行和系统通知中：若 `anchorMissing > 0` 或 `degraded > 0`，明确提示（例如：`已更新到 v0.2.2（3 项旧补丁失配，详情见日志）`），便于判断是否需要升级桌面壳或重锚补丁。
- **涉及文件**：`dsh-desktop/scripts/lib/agent-update.js`、`dsh-tauri/src-tauri/src/app/src/commands/menu.rs`、`dsh-tauri/contracts/bridge-api.md`、`dsh-tauri/src-tauri/crates/bridge/dist/bridge-shim.js`、`unit-agent-update.test.js`。
- **风险与验收**：风险极低（仅增量字段）；`ta7-contract-audit` 与 `unit-agent-update` 全绿。

### 1.3 终端 `dsh` CLI shim 跟随现役内核

- **问题**：`dsh-tauri/src-tauri/src/app/src/commands/dsh_cli.rs` 生成的 `dsh.cmd` / `dsh` shim 硬编码指向内置 `app_dir/node_modules/@deepseek-ai/dsh/lib/bin.js`。用户通过菜单升级覆盖层后，桌面端运行新版而终端 `dsh` 仍运行旧内置版。
- **方案**：
  1. `ensure_dsh_cli_shim()` 改调 `supervisor::resolve_active_agent(&app_dir, &paths.app_data)` 获取现役 `bin_js` 写入 shim（内容不变时零写入幂等）。
  2. 在 `menu.rs` 的 `agent-install-update` 与 `agent-reset-bundled` 切换完成并 `refresh_active_agent()` 后，异步调用一次 `dsh_cli::ensure_dsh_cli_shim()` 自动同步刷新 shim。
- **涉及文件**：`dsh-tauri/src-tauri/src/app/src/commands/dsh_cli.rs`、`dsh-tauri/src-tauri/src/app/src/commands/menu.rs`。
- **风险与验收**：风险极低；补齐 `dsh_cli.rs` 单测验证覆盖层存在时 shim 指向覆盖层，删除后回到内置。

---

## 二、启动与运行期性能（P1）

### 2.1 内核冷启动瘦身：内置插件「精简预设」与包体清理

- **问题**：
  - 壳侧 boot 已降至 `0.68s`，但内核启动仍占 **16s**，主要消耗在 30+ 伴随插件的冷加载与初始化（如 `openclaw-bridge`、`dsh-offpeak`、`dsh-purge-community`、`dsh-pocket`、`dsh-basics-panel`、`dsh-synapse` 等并非人人日常使用）。
  - `dsh-desktop/assets/plugins/dsh-better-sidebar` 当前 14.96MB，其中 `src/` 占 2.55MB（运行时只加载 `lib/`），同步与打包均做无用拷贝。
- **方案**：
  1. **内置插件管理页（`dsh-builtin-manager`）新增预设快捷切换**：提供「仅保留常用核心」（如保留侧边栏、压缩引擎、上下文洞察、管理页、免费搜索等，批量停用低频/桥接类插件）与「全部恢复启用」按钮，不删文件、随时可逆。
  2. **同步过滤研发源码目录**：在 `dsh-desktop/scripts/sync-companion-plugins.js` 复制伴随插件到 `profiles/web/node_modules` 时跳过非运行必需的 `src/` / `*.map` / `tsconfig*.json`（不影响仓内保留源码便于二次构建），减少磁盘 I/O 与同步体积。
- **涉及文件**：`dsh-desktop/assets/plugins/dsh-builtin-manager/lib/client.js`、`dsh-desktop/scripts/sync-companion-plugins.js`、对应单测。
- **风险与验收**：中等（需确认 `dsh-better-sidebar` 及全部在册插件运行时不读取 `src/`，跑 `unit-plugin-inject-services` 与实机启动验证）；预期减少数秒内核冷启动并降低常驻内存。

### 2.2 boot 链 `patches` 步骤指纹快路径

- **问题**：`patches` 步骤每次冷启动都对 66 项补丁逐一读盘扫描并打印 66 行「已应用，跳过」日志（耗 ~59ms + 大量日志噪声）。
- **方案**：
  1. 复用 `companion-sync-cache.js` 同款设计，在 `%APPDATA%\dsh-desktop\patch-cache.json` 记录指纹：`{ kernelVersion, agentSource, registryHash, targetMtimes }`。
  2. `sidecar/cli.js` 的 `boot` 步骤 `patches` 前校验指纹（核对 `patch-registry.js` 哈希 + 现役内核版本 + 抽样/关键靶文件 `mtimeMs/size`）；全命中时直接快路径跳过（`~5ms`），仅打印一行汇总日志；任何不匹配或显式传 `--force-patches` / `patches-apply` 命令时走全量补丁逻辑。
- **涉及文件**：新增 `dsh-desktop/scripts/lib/patch-cache.js`、接入 `dsh-tauri/sidecar/cli.js`、新增 `unit-patch-cache.test.js`。
- **风险与验收**：低风险（fail-open 回落全量扫描，且 `patches-apply` 回滚链强制绕过缓存）；验证缓存命中 / 文件被篡改失效 / 换核失效三种场景。

### 2.3 启动期探活宽限与日志噪音修复

- **问题**：
  1. **首轮假死采样文案 Bug**：实机日志 `02:40:55` 出现 `内核假死可疑（端口通、HTTP 无响应）×1（取证不可用：非 Windows/WSL 模式/采样失败）`。核查 `supervisor.rs:1559-1580` 发现：首次触发（`zombie == 1`）时 `last_cpu` 为 `None`，即使 Windows CPU 采样成功（`cur.is_some()`），也会落入 `None` 分支误报「取证不可用」。
  2. **冷启动高负载单次超时误告警**：内核刚就绪后的前 60s 内前端密集拉会话/插件资源，事件循环偶发短暂抖动导致单次 HTTP 探活超时。
  3. **自定义网关用户的官方 401 日志噪音**：未配置官方 DeepSeek Key 时，内核启动会尝试拉取 `https://api.deepseek.com/models` 并输出 `HTTP 401`（属上游行为，可先评估是否仅降噪或保持不动，避免侵入上游认证逻辑）。
- **方案**：
  1. 修复 `supervisor.rs` 的 `cpu_note` 分支：区分「首次基线采样（`cur.is_some() && last_cpu.is_none()`）→ 下轮起计算 CPU 增量」与真正的「采样失败（`cur.is_none()`）」。
  2. 在 `probe_loop` 启动后的前 60s 宽限期内将单次 HTTP 探活超时适当放宽（如 2s → 4s），不改变 `ZOMBIE_THRESHOLD = 20` 的判死口径。
  3. 官方 401 噪音：默认**不打补丁**，仅在排查文档备注原因（避免误伤官方 Key 用户）。
- **涉及文件**：`dsh-tauri/src-tauri/src/app/src/supervisor.rs`（含单测）。
- **风险与验收**：风险极低；Rust 单测覆盖 `cpu_note` 三态分支。

---

## 三、日常交互体验（P2）

### 3.1 Agent 更新分阶段实时进度

- **问题**：`npm install @deepseek-ai/dsh` 需下载安装 300+ 包，耗时 30s~2 分钟；目前菜单按钮只停留在「更新中…」，用户无法判断是在下载、打补丁还是卡住。
- **方案**：
  1. `agent-update.js` 在关键节点向 `stderr` 输出结构化进度行（如 `[agent-progress] {"stage":"downloading","message":"正在下载 @deepseek-ai/dsh@0.2.2…"}` → `verifying` → `patching` → `switching` → `restarting`）。
  2. Rust `commands/menu.rs` 的 `run_agent_sidecar` 在 `agent-stage-update` 时逐行读取 `stderr`，解析 `[agent-progress]` 后通过 `app.emit("agent-update-progress", payload)` 推送事件。
  3. `bridge-shim.js` 监听 `agent-update-progress` 实时更新 `.dch-mh-note` 状态文案。
- **涉及文件**：`agent-update.js`、`commands/menu.rs`、`bridge-shim.js`、`dsh-tauri/contracts/bridge-api.md`、`dsh-tauri/contracts/ipc-commands.md`、`crates/bridge/src/shim.rs`（事件白名单测试）。
- **风险与验收**：低风险（需同步契约事件表与 `shim.rs` 静态测试）。

### 3.2 右上角 `…` 菜单新增「重启内核」

- **问题**：用户修改需要重启生效的插件配置、或遇到内核状态异常时，目前没有直接的重启入口，只能点「退出」再重新打开应用。
- **方案**：
  1. `commands/menu.rs` 新增 `menu_action("restart-kernel")`，复用 `restart_agent_kernel(&app, &sv)`（刷新现役内核 → 重跑 boot 链 → 切 `loading.html` → 就绪后回到主界面）。
  2. `bridge-shim.js` 在「重新加载」下方新增「重启内核」菜单项。
  3. 同步更新 `bridge-api.md` §2.3、`ta7-contract-audit.test.js`、`shim.rs`。
- **涉及文件**：`commands/menu.rs`、`bridge-shim.js`、`bridge-api.md`、`ta7-contract-audit.test.js`、`shim.rs`。
- **风险与验收**：极低风险；实机验证点击后平滑回到 loading 页并恢复就绪。

### 3.3 内置插件管理页「一键更新全部」

- **问题**：内置插件管理页（`dsh-builtin-manager`）支持检查全部更新和单件逐个更新，但遇到 5~8 个插件同时有新版时需逐一点击等待。
- **方案**：
  1. 在 `dsh-builtin-manager` 顶部工具栏，当检查更新发现可更新插件数 `>= 2` 时，显示「全部更新（N）」按钮。
  2. 前端按顺序串行调用现有的 `dshDesktop.pluginManager.update(id)`（天然复用 sidecar 串行锁，零后端新增通道），逐项实时刷新进度（`正在更新 2/5：dsh-pocket…`），全部完成后统一提示重启生效。
- **涉及文件**：`dsh-desktop/assets/plugins/dsh-builtin-manager/lib/client.js`（及对应测试）。
- **风险与验收**：极低风险（纯前端编排既有桥方法，零契约改动）。

---

## 四、工程卫生与可维护性（P3）

### 4.1 清理 3 处 Rust `dead_code` 编译告警

- **问题**：每次 `cargo build` / `cargo test` 都出现 3 条固定 warning：
  - `dsh-tauri/src-tauri/src/app/src/commands/common.rs`: `atomic_write`、`b64`
  - `dsh-tauri/src-tauri/src/app/src/logging.rs`: `ENV_LOCK`（仅测试使用，应标 `#[cfg(test)]`）
- **方案**：
  - 核查全仓引用后，对仅测试使用的 `ENV_LOCK` 加 `#[cfg(test)]`，对确无消费方的 `atomic_write` / `b64` 直接移除（若测试用则移入 `#[cfg(test)]`），实现 `dsh-tauri-app` 零告警编译。
- **涉及文件**：`commands/common.rs`、`logging.rs`。
- **风险与验收**：极低风险；`cargo test --workspace` + `cargo build --release` 零新增/零残留 warning。

### 4.2 超大单文件渐进式拆分

- **问题**：
  - `dsh-tauri/src-tauri/src/app/src/supervisor.rs` 已达 **3831 行**（混杂 boot 瀑布、探活/假死取证、WinInet 代理桥接、WSL 适配、单测）。
  - `dsh-tauri/src-tauri/crates/bridge/dist/bridge-shim.js` 已达 **1035 行**（桥方法、标题栏、菜单、自定义图标、Agent 更新、CSS 全部堆在单个 IIFE 内）。
- **方案（纯重构，零行为变更）**：
  1. 将 `supervisor.rs` 拆为目录模块 `supervisor/`：
     - `supervisor/mod.rs`（结构体、状态机、公共 API 与 `ActiveAgent`）
     - `supervisor/boot.rs`（boot 瀑布、守护快照、回滚链）
     - `supervisor/probe.rs`（TCP/HTTP 探活、假死 CPU 取证、崩溃环）
     - `supervisor/proxy.rs`（Windows WinInet 系统代理桥接与 env 注入）
     - `supervisor/tests.rs`（单测收口）
  2. `bridge-shim.js` 保持单文件发布产物（`include_str!` 不变），内部按清晰分区注释整理，若后续继续增长再考虑小脚本拼接构建（避免为单纯拆分引入复杂前端打包链，遵循仓库「无多余构建工具链」约定）。
- **涉及文件**：`dsh-tauri/src-tauri/src/app/src/supervisor/`。
- **风险与验收**：注意 `lib.rs` / `ta7` 中任何 `include_str!("supervisor.rs")` 源码形态测试需先全仓 grep 核对后再拆分；`cargo test --workspace` 与 `ta7-contract-audit` 全绿。

---

## 五、建议分期推进顺序

每期均独立可测、独立提交、可按需裁剪：

| 期次 | 包含项 | 核心收益 | 预估改动面 |
| --- | --- | --- | --- |
| **第 1 期（稳定性 + 高频体验）** | `1.1` 覆盖层崩溃自动回退 + `1.2` 补丁适配报告 + `1.3` CLI shim 同步 + `3.2` 菜单「重启内核」 + `4.1` 零告警清理 | 把刚上线的 Agent 热更新打磨成完整防呆闭环，附带高频「重启内核」和零告警编译 | Rust 壳 + `agent-update.js` + `bridge-shim.js` + 契约 |
| **第 2 期（更新体验增强）** | `3.1` Agent 更新分阶段进度 + `3.3` 内置插件「一键更新全部」 | 长耗时更新过程全透明，插件维护一键完成 | `agent-update.js` + `menu.rs` + `bridge-shim.js` + `dsh-builtin-manager` |
| **第 3 期（启动与运行期提速）** | `2.1` 插件精简预设 & 同步去源码 + `2.2` patches 指纹快路径 + `2.3` 探活宽限 & 假死采样文案修复 | 压缩内核 16s 冷启动时间，消除假死误报与多余读盘 | `dsh-builtin-manager` + `sync/patch-cache` + `supervisor.rs` |
| **第 4 期（架构重构）** | `4.2` `supervisor.rs` 拆分子模块 | 降低后续迭代维护成本（零功能变更，独立重构 PR/提交） | `dsh-tauri/src-tauri/src/app/src/supervisor/` |

---

## 六、每期通用验收门禁

1. **Node 门禁**：`node scripts/check-syntax.js`、`node scripts/compat/validate-pin.js`、`ta7-contract-audit.test.js` 及新增 `unit-*.test.js` 全绿；若触及补丁层则同步重取并校验 `patch-surface.snapshot.json`。
2. **Rust 门禁**：`cargo test --workspace` 全绿；构建遵循本机共享缓存 `E:/build-cache/cargo-target`。
3. **本机实测**：经 `dsh-tauri/scripts/sync-installed.ps1 -Launch` 增量同步到安装版并核查 `desktop.log` 与界面交互。
