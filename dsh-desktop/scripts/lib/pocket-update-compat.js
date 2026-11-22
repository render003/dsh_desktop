'use strict';

// ---------------------------------------------------------------------------
// dsh-pocket（手机访问）在 DSH Pack 桌面端的更新与重启适配。
//
// 根因：
//   1. dsh-pocket 上游 client/client.js 点「更新」时走 RPC 调 lib/index.js 的
//      performUpdate → spawn('dsh', ['plugin', '--profile', profile, 'update',
//      'dsh-pocket', '--latest', '-w'])。但 DSH Pack 的 ~/.dsh/profiles/web 既
//      不是 pnpm workspace（无 pnpm-workspace.yaml → 必报 --workspace-root 错误），
//      也不在 package.json dependencies 里（issue #156），且本机未必有全局 dsh CLI。
//   2. dsh-pocket 上游 restartPocket 走独立进程重启（杀旧 dsh web + 拉孤儿进程），
//      在 Tauri 壳托管模式下应直接调用 window.dshDesktop.restartService()。
//
// 方案（双保险，且升到上游新版后由 sidecar 自动续打）：
//   1. 客户端：存在 window.dshDesktop.pluginManager.update 时直接走桌面桥更新
//      ('dsh-pocket')；存在 window.dshDesktop.restartService 时直接走桌面桥重启；
//      错误提示里的手动命令去掉必错的 -w。
//   2. 宿主端（浏览器/独立 CLI 回退）：仅在 profile 目录存在 pnpm-workspace.yaml 时
//      追加 -w；若 package.json dependencies 未声明 dsh-pocket，改用 add dsh-pocket@latest。
// ---------------------------------------------------------------------------

const fs = require('node:fs');
const path = require('node:path');

const POCKET_CLIENT_BRIDGE_MARKER = '/* dsh-pack:pocket-desktop-update */';
const POCKET_HOST_UPDATE_MARKER = '/* dsh-pack:pocket-update-args */';

const CLIENT_RESTART_ANCHOR = 'const restartPocket = async () => {\n    setUpdateInfo((u) => ({ ...u, restarting: true, startedAt: Date.now() }));\n    try {';
const CLIENT_RESTART_REPLACEMENT =
  'const restartPocket = async () => {\n' +
  '    ' + POCKET_CLIENT_BRIDGE_MARKER + '\n' +
  '    setUpdateInfo((u) => ({ ...u, restarting: true, startedAt: Date.now() }));\n' +
  '    try {\n' +
  '      if (typeof window !== "undefined" && window.dshDesktop && typeof window.dshDesktop.restartService === "function") {\n' +
  '        await window.dshDesktop.restartService();\n' +
  '        setUpdateInfo((u) => ({ ...u, restarting: true, result: "ok" }));\n' +
  '        return;\n' +
  '      }';

const CLIENT_UPDATE_ANCHOR = 'const runUpdate = async () => {\n    setUpdateInfo((u) => ({ ...u, updating: true, result: null, startedAt: Date.now() }));\n    try {\n      const r = await call(POCKET_ENDPOINTS.update, {});';
const CLIENT_UPDATE_REPLACEMENT =
  'const runUpdate = async () => {\n' +
  '    setUpdateInfo((u) => ({ ...u, updating: true, result: null, startedAt: Date.now() }));\n' +
  '    try {\n' +
  '      if (typeof window !== "undefined" && window.dshDesktop && window.dshDesktop.pluginManager && typeof window.dshDesktop.pluginManager.update === "function") {\n' +
  '        const r = await window.dshDesktop.pluginManager.update("dsh-pocket");\n' +
  '        setUpdateInfo((u) => ({\n' +
  '          ...u,\n' +
  '          updating: false,\n' +
  '          result: r && r.ok ? "ok" : "fail",\n' +
  '          autoRestart: false,\n' +
  '          output: r && r.ok ? "" : (r && r.error) || "\\u66F4\\u65B0\\u5931\\u8D25"\n' +
  '        }));\n' +
  '        return;\n' +
  '      }\n' +
  '      const r = await call(POCKET_ENDPOINTS.update, {});';

const HOST_SPAWN_ANCHOR = "const child = spawn('dsh', ['plugin', '--profile', profile, 'update', 'dsh-pocket', '--latest', '-w'], {";
const HOST_SPAWN_REPLACEMENT =
  POCKET_HOST_UPDATE_MARKER + '\n' +
  '    const homeDir = process.env.DSH_HOME ?? join(homedir(), \'.dsh\');\n' +
  '    const profileDir = join(homeDir, \'profiles\', profile);\n' +
  '    let hasWorkspace = false;\n' +
  '    try { readFileSync(join(profileDir, \'pnpm-workspace.yaml\')); hasWorkspace = true; } catch { /* 非 workspace */ }\n' +
  '    let hasDep = false;\n' +
  '    try {\n' +
  '      const pkgJson = JSON.parse(readFileSync(join(profileDir, \'package.json\'), \'utf8\'));\n' +
  '      hasDep = Boolean(pkgJson?.dependencies && Object.prototype.hasOwnProperty.call(pkgJson.dependencies, \'dsh-pocket\'));\n' +
  '    } catch { /* 读取失败按未声明处理 */ }\n' +
  '    const updateArgs = hasDep\n' +
  '      ? [\'plugin\', \'--profile\', profile, \'update\', \'dsh-pocket\', \'--latest\']\n' +
  '      : [\'plugin\', \'--profile\', profile, \'add\', \'dsh-pocket@latest\'];\n' +
  '    if (hasWorkspace) updateArgs.push(\'-w\');\n' +
  '    const child = spawn(\'dsh\', updateArgs, {';

/** 补丁客户端 bundle（幂等）：接入 window.dshDesktop.pluginManager.update / restartService，去掉手动提示里的 -w。 */
function patchPocketClientSource(src) {
  if (typeof src !== 'string') return { status: 'anchor-missing', src };
  if (src.includes(POCKET_CLIENT_BRIDGE_MARKER)) return { status: 'already', src };
  if (!src.includes(CLIENT_RESTART_ANCHOR) || !src.includes(CLIENT_UPDATE_ANCHOR)) {
    return { status: 'anchor-missing', src };
  }
  let out = src
    .replace(CLIENT_RESTART_ANCHOR, CLIENT_RESTART_REPLACEMENT)
    .replace(CLIENT_UPDATE_ANCHOR, CLIENT_UPDATE_REPLACEMENT);
  out = out.replace(/update dsh-pocket --latest -w/g, 'add dsh-pocket@latest');
  return { status: 'changed', src: out };
}

/** 补丁宿主入口（幂等）：非 workspace 不再强制传 -w，未在 dependencies 声明时走 add dsh-pocket@latest。 */
function patchPocketHostSource(src) {
  if (typeof src !== 'string') return { status: 'anchor-missing', src };
  if (src.includes(POCKET_HOST_UPDATE_MARKER)) return { status: 'already', src };
  if (!src.includes(HOST_SPAWN_ANCHOR)) return { status: 'anchor-missing', src };
  return {
    status: 'changed',
    src: src.replace(HOST_SPAWN_ANCHOR, HOST_SPAWN_REPLACEMENT),
  };
}

/** 对磁盘上的 dsh-pocket 目录（assets 或更新后的 profile 副本）就地续打两处补丁。 */
function patchPocketDir(dir) {
  const result = { client: 'missing', host: 'missing' };
  const clientPath = path.join(dir, 'client', 'client.js');
  if (fs.existsSync(clientPath)) {
    const src = fs.readFileSync(clientPath, 'utf8');
    const r = patchPocketClientSource(src);
    result.client = r.status;
    if (r.status === 'changed') fs.writeFileSync(clientPath, r.src, 'utf8');
  }
  const hostPath = path.join(dir, 'lib', 'index.js');
  if (fs.existsSync(hostPath)) {
    const src = fs.readFileSync(hostPath, 'utf8');
    const r = patchPocketHostSource(src);
    result.host = r.status;
    if (r.status === 'changed') fs.writeFileSync(hostPath, r.src, 'utf8');
  }
  return result;
}

module.exports = {
  POCKET_CLIENT_BRIDGE_MARKER,
  POCKET_HOST_UPDATE_MARKER,
  patchPocketClientSource,
  patchPocketHostSource,
  patchPocketDir,
};
