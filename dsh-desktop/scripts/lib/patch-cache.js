'use strict';

// ---------------------------------------------------------------------------
// boot 补丁阶段快速指纹缓存（纯函数 + 轻量 fs 读写）。
//
// 背景：boot 的 patches 步在热启动时需要对 66 项补丁对应的 ~90 个 JS 文件
// 逐一 readFileSync + 正则/字符串幂等校验（~60ms 且输出大量冗余日志）。
// 本模块仅对以下输入做一次轻量 lstatSync + SHA-256 指纹：
//   1. 安装包元数据 + 现役 Agent 覆盖层指针（agent-active.json）；
//   2. 补丁基础设施脚本（patch-registry / runtime-patches / patch-target-resolver
//      / patch-runner / scripts/patch-*.js）；
//   3. 所有 file 补丁经 resolveFiles 解析出的实际目标文件；
//   4. 所有 root 补丁涉及的核心目标文件（在各 nm-roots 下的 lstat 签名）。
//
// 任何目标文件、补丁脚本、Agent 覆盖层切换、安全模式（safe-mode.flag）、
// WSL 模式或强制标志（--force-patches / DSH_PATCH_CACHE=0）都会使缓存失效，
// 立即回落完整 applyPatches()；patches-apply 回滚修复命令始终绕过本缓存。
// ---------------------------------------------------------------------------

const fs = require('node:fs');
const path = require('node:path');
const crypto = require('node:crypto');

const { getSpecsByGroup } = require('./patch-registry');
const { resolveNmRoots } = require('./patch-target-resolver');
const { resolveFiles } = require('../integration/patch-runner');
const { resolveActiveAgentRoot } = require('./agent-update');
const { safeModeRequested } = require('./safe-mode');

const CACHE_VERSION = 1;

/** 12 项 root 补丁涉及的 @deepseek-ai 包内代表性文件与内核版本标记。 */
const ROOT_PATCH_PROBE_RELS = [
  path.join('dsh', 'package.json'),
  path.join('dsh-web-search-deepseek', 'lib', 'index.js'),
  path.join('dsh-session', 'lib', 'index.js'),
  path.join('dsh-api-workspace-controller', 'lib', 'index.js'),
  path.join('dsh-api-workspace-controller', 'lib', 'client.js'),
  path.join('dsh-api-workspace-controller', 'lib', 'typert.host.js'),
  path.join('dsh-client-ui-workspace', 'lib', 'client.js'),
  path.join('dsh-session-persistence-jsonl', 'lib', 'index.js'),
  path.join('dsh-tools', 'lib', 'pipeline.js'),
  path.join('dsh-llm-pi-ai', 'lib', 'index.js'),
  path.join('dsh-client-ui-settings-models', 'lib', 'client.js'),
  path.join('dsh-client-modules', 'lib', 'client.js'),
];

/** 单路径 lstat 签名：区分缺失、符号链接/junction、目录与普通文件。 */
function lstatSig(p) {
  try {
    const st = fs.lstatSync(p);
    if (st.isSymbolicLink()) return 'L';
    if (st.isDirectory()) return 'D:' + Math.round(st.mtimeMs);
    return 'F:' + st.size + ':' + Math.round(st.mtimeMs);
  } catch {
    return 'X';
  }
}

/** 收集补丁脚本清单（固定核心模块 + scripts/patch-*.js 动态扫）。 */
function collectPatchScriptPaths(appDir) {
  const out = [
    path.join(appDir, 'package.json'),
    path.join(appDir, 'scripts', 'lib', 'patch-registry.js'),
    path.join(appDir, 'scripts', 'lib', 'runtime-patches.js'),
    path.join(appDir, 'scripts', 'lib', 'patch-target-resolver.js'),
    path.join(appDir, 'scripts', 'lib', 'patch-engine.js'),
    path.join(appDir, 'scripts', 'integration', 'patch-runner.js'),
  ];
  const scriptsDir = path.join(appDir, 'scripts');
  try {
    const names = fs.readdirSync(scriptsDir)
      .filter((n) => /^patch-.*\.js$/i.test(n))
      .sort();
    for (const name of names) out.push(path.join(scriptsDir, name));
  } catch {}
  return out;
}

/**
 * 计算本次 boot patches 阶段的输入指纹。
 * 返回 null 表示禁止走快路径（强制、WSL 模式、安全模式或必要路径缺失）。
 */
function computePatchFingerprint(opts) {
  const {
    appDir,
    home,
    userDataDir,
    wslMode = false,
    force = false,
    specs = getSpecsByGroup(),
  } = opts || {};

  if (force || wslMode) return null;
  if (!appDir || !home || !userDataDir) return null;
  if (safeModeRequested(home).enabled) return null;

  const h = crypto.createHash('sha256');
  const feed = (s) => h.update(String(s) + '\n');

  feed('v=' + CACHE_VERSION);
  feed('app=' + appDir);
  feed('home=' + home);
  feed('ud=' + userDataDir);
  feed('specs=' + specs.length);

  // 现役 Agent 覆盖层状态（切换覆盖层或回退内置内核时立刻失效）。
  const active = resolveActiveAgentRoot({ appDir, userDataDir });
  feed('active=' + active.source + '|' + active.root + '|' + (active.version || ''));
  feed('activeFile=' + lstatSig(path.join(userDataDir, 'agent-active.json')));

  // 补丁基础设施脚本：任意补丁源码更新立刻失效。
  for (const p of collectPatchScriptPaths(appDir)) {
    feed('script:' + p + ':' + lstatSig(p));
  }

  const ctx = { home, appDir, userDataDir, wslMode: false };
  const targetFiles = new Set();
  const rootDirs = new Set();

  for (const spec of specs) {
    if (spec.kind === 'root') {
      for (const r of resolveNmRoots(ctx, spec)) {
        if (r) rootDirs.add(r);
      }
      continue;
    }
    const multi = Array.isArray(spec.pkgRels) && spec.pkgRels.length > 0;
    if (multi) {
      for (const pkgRel of spec.pkgRels) {
        for (const f of resolveFiles(ctx, { ...spec, pkgRel })) targetFiles.add(f);
      }
    } else {
      for (const f of resolveFiles(ctx, spec)) targetFiles.add(f);
    }
  }

  // 把每个 nm-root 下的 root 补丁关键目标纳入指纹。
  for (const nmRoot of [...rootDirs].sort()) {
    feed('nmRoot:' + nmRoot + ':' + lstatSig(nmRoot));
    for (const rel of ROOT_PATCH_PROBE_RELS) {
      targetFiles.add(path.join(nmRoot, '@deepseek-ai', rel));
    }
  }

  for (const f of [...targetFiles].sort()) {
    feed('target:' + f + ':' + lstatSig(f));
  }

  return h.digest('hex');
}

function readPatchCache(cacheFile) {
  try {
    const data = JSON.parse(fs.readFileSync(cacheFile, 'utf8'));
    if (data && data.v === CACHE_VERSION && typeof data.fingerprint === 'string') {
      return data.fingerprint;
    }
  } catch {}
  return null;
}

function writePatchCache(cacheFile, fingerprint) {
  if (!cacheFile || !fingerprint) return false;
  try {
    fs.mkdirSync(path.dirname(cacheFile), { recursive: true });
    fs.writeFileSync(cacheFile, JSON.stringify({
      v: CACHE_VERSION,
      fingerprint,
      at: new Date().toISOString(),
    }) + '\n', 'utf8');
    return true;
  } catch {
    return false;
  }
}

module.exports = {
  CACHE_VERSION,
  ROOT_PATCH_PROBE_RELS,
  lstatSig,
  collectPatchScriptPaths,
  computePatchFingerprint,
  readPatchCache,
  writePatchCache,
};
