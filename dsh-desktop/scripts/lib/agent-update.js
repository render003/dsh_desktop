'use strict';

// ---------------------------------------------------------------------------
// DeepSeek Harness（@deepseek-ai/dsh）稳定版检查与增量更新模块（纯 Node）。
//
// 职责：
//   1. checkAgentUpdate：查 npm registry（NPM_CONFIG_REGISTRY 优先，否则
//      npmmirror → npmjs 双源回退）获取 @deepseek-ai/dsh 最新稳定版（跳过 alpha），
//      用 scripts/lib/versions.js 的 compareVersions 判定是否有更新；
//   2. stageAgentUpdate：用内置 vendor node + vendor npm 在独立暂存目录安装
//      @deepseek-ai/dsh@<version>，对暂存副本运行全部桌面运行时补丁（applyAll），
//      校验 bin.js 完整性后返回暂存路径，由 Rust 壳在停内核后原子切换并重启。
// ---------------------------------------------------------------------------

const fs = require('node:fs');
const path = require('node:path');
const https = require('node:https');
const http = require('node:http');
const { spawn } = require('node:child_process');
const { compareVersions } = require('./versions');

const AGENT_PKG = '@deepseek-ai/dsh';
const DEFAULT_REGISTRIES = [
  'https://registry.npmmirror.com',
  'https://registry.npmjs.org',
];
// 允许的稳定/RC 版本号形态（拒绝 alpha 与任意非 semver 字符串，防命令注入）。
const SAFE_STABLE_VERSION_RE = /^\d+\.\d+\.\d+(?:-(?:rc|beta)\.\d+)?$/;

/** 是否为 alpha 预发布版（跳过不推送给稳定版通道）。 */
function isAlphaVersion(v) {
  return /(?:^|[-.])alpha(?:$|[.\d-])/i.test(String(v || ''));
}

/**
 * 从 npm dist-tags / versions 中选出最新稳定版（排除 alpha）。
 * 优先 dist-tags.latest（官方稳定/推荐通道，只要不是 alpha）；
 * 若 latest 缺席或意外指向 alpha，再从 versions 中挑 compareVersions 最大的非 alpha 版。
 */
function selectStableVersion(distTags, versions) {
  const latest = distTags && typeof distTags.latest === 'string' ? distTags.latest.trim() : '';
  if (latest && !isAlphaVersion(latest) && SAFE_STABLE_VERSION_RE.test(latest)) {
    return latest;
  }
  if (!Array.isArray(versions) || versions.length === 0) return '';
  const candidates = versions
    .map((v) => String(v || '').trim())
    .filter((v) => v && !isAlphaVersion(v) && SAFE_STABLE_VERSION_RE.test(v));
  if (candidates.length === 0) return '';
  candidates.sort(compareVersions);
  return candidates[candidates.length - 1];
}

/** 构造要探测的 registry 列表（NPM_CONFIG_REGISTRY 显式覆盖时只走该源）。 */
function resolveRegistries(env = process.env) {
  const custom = env && env.NPM_CONFIG_REGISTRY ? String(env.NPM_CONFIG_REGISTRY).trim() : '';
  if (custom) return [custom.replace(/\/+$/, '')];
  return DEFAULT_REGISTRIES.slice();
}

/** 读取某个根目录下的 @deepseek-ai/dsh 版本（不存在返回空串）。 */
function readAgentVersion(rootDir) {
  if (!rootDir) return '';
  try {
    const pkg = JSON.parse(
      fs.readFileSync(path.join(rootDir, 'node_modules', '@deepseek-ai', 'dsh', 'package.json'), 'utf8'),
    );
    return pkg && typeof pkg.version === 'string' ? pkg.version.trim() : '';
  } catch {
    return '';
  }
}

/** 判断 userData/agent 覆盖层是否有效且版本高于内置 appDir 版本。 */
function resolveActiveAgentRoot({ appDir, userDataDir }) {
  const bundledVersion = readAgentVersion(appDir);
  if (userDataDir) {
    const overlayRoot = path.join(userDataDir, 'agent');
    const overlayBin = path.join(overlayRoot, 'node_modules', '@deepseek-ai', 'dsh', 'lib', 'bin.js');
    const overlayVersion = readAgentVersion(overlayRoot);
    if (
      overlayVersion &&
      fs.existsSync(overlayBin) &&
      (!bundledVersion || compareVersions(overlayVersion, bundledVersion) > 0)
    ) {
      return { root: overlayRoot, version: overlayVersion, source: 'updated' };
    }
  }
  return { root: appDir, version: bundledVersion || 'unknown', source: 'bundled' };
}

/** 默认 HTTPS/HTTP JSON 拉取器（带超时与重定向，Node 24 在 NODE_USE_ENV_PROXY=1 下自动走代理）。 */
function defaultHttpGetJson(url, timeoutMs = 12000, redirects = 0) {
  return new Promise((resolve, reject) => {
    if (redirects > 5) return reject(new Error('重定向次数过多'));
    let client;
    try {
      const u = new URL(url);
      client = u.protocol === 'https:' ? https : http;
    } catch (err) {
      return reject(err);
    }
    const req = client.get(
      url,
      { headers: { 'User-Agent': 'DSH-Pack-Agent-Updater', Accept: 'application/json' }, timeout: timeoutMs },
      (res) => {
        if (res.statusCode >= 300 && res.statusCode < 400 && res.headers.location) {
          res.resume();
          return resolve(defaultHttpGetJson(new URL(res.headers.location, url).toString(), timeoutMs, redirects + 1));
        }
        let body = '';
        res.on('data', (chunk) => { body += chunk; });
        res.on('end', () => {
          if (res.statusCode !== 200) return reject(new Error('HTTP ' + res.statusCode));
          try {
            resolve(JSON.parse(body));
          } catch {
            reject(new Error('响应不是合法 JSON'));
          }
        });
      },
    );
    req.on('timeout', () => req.destroy(new Error('请求超时')));
    req.on('error', (err) => reject(err));
  });
}

/**
 * 检查 @deepseek-ai/dsh 最新稳定版。
 * 先请求 /@deepseek-ai%2Fdsh/latest（体积极小 ~4KB）；仅当 latest 意外指向 alpha 时
 * 再回查精简 packument 挑最高非 alpha 版本。
 */
async function checkAgentUpdate({
  currentVersion,
  agentSource = 'bundled',
  appDir,
  userDataDir,
  httpGetJson = defaultHttpGetJson,
  env = process.env,
} = {}) {
  const active = (!currentVersion && appDir)
    ? resolveActiveAgentRoot({ appDir, userDataDir })
    : { version: currentVersion || 'unknown', source: agentSource || 'bundled' };
  const cur = String(active.version || 'unknown').trim();
  const src = active.source || agentSource || 'bundled';

  const registries = resolveRegistries(env);
  const enc = encodeURIComponent(AGENT_PKG);
  const errors = [];

  for (const reg of registries) {
    try {
      const latestDoc = await httpGetJson(reg + '/' + enc + '/latest', 12000);
      const rawLatest = latestDoc && typeof latestDoc.version === 'string' ? latestDoc.version.trim() : '';
      let target = selectStableVersion({ latest: rawLatest }, []);
      if (!target) {
        // 兜底：若 latest dist-tag 意外指向 alpha，再拉 packument 选最高非 alpha 版。
        const packument = await httpGetJson(reg + '/' + enc, 15000);
        const tags = packument && packument['dist-tags'] ? packument['dist-tags'] : {};
        const vers = packument && packument.versions ? Object.keys(packument.versions) : [];
        target = selectStableVersion(tags, vers);
      }
      if (!target) {
        errors.push(reg + ': 未解析出有效稳定版本号');
        continue;
      }
      const hasUpdate = cur !== 'unknown' && cur !== ''
        ? compareVersions(target, cur) > 0
        : true;
      return {
        ok: true,
        currentVersion: cur,
        latestVersion: target,
        hasUpdate,
        agentSource: src,
        registry: reg,
      };
    } catch (err) {
      errors.push(reg + ': ' + String((err && err.message) || err));
    }
  }

  return {
    ok: false,
    currentVersion: cur,
    latestVersion: '',
    hasUpdate: false,
    agentSource: src,
    registry: '',
    error: '检查更新失败（' + errors.join('；') + '）',
  };
}

/** 查找内置 vendor npm CLI 入口（缺失返回空串）。 */
function findVendorNpmCli(appDir) {
  if (!appDir) return '';
  const cli = path.join(appDir, 'vendor', 'npm', 'bin', 'npm-cli.js');
  return fs.existsSync(cli) ? cli : '';
}

/**
 * 规整 applyAll() 返回的补丁报告，产出前端可直接消费的摘要与失配告警文案（纯函数）。
 */
function summarizePatchReport(report) {
  if (!report || typeof report !== 'object') return null;
  const total = Number(report.total || 0);
  const changed = Number(report.changed || 0);
  const anchorMissing = Number(report.anchorMissing || 0);
  const degradedIds = Array.isArray(report.degraded) ? report.degraded.map(String) : [];
  const errorIds = Array.isArray(report.errors) ? report.errors.map(String) : [];
  const failed = Number(report.failed || 0) + errorIds.length;
  const parts = [];
  if (anchorMissing > 0) parts.push(anchorMissing + ' 项补丁锚点失配');
  if (degradedIds.length > 0) parts.push(degradedIds.length + ' 项降级(' + degradedIds.join(',') + ')');
  if (failed > 0) parts.push(failed + ' 项执行失败');
  return {
    total,
    changed,
    anchorMissing,
    degradedCount: degradedIds.length,
    degradedIds,
    failed,
    warning: parts.length > 0 ? parts.join('，') + '（详情见日志）' : '',
  };
}

/** 运行 npm install 安装目标版本到 staging 目录。 */
function runNpmInstall({ nodeExe, npmCli, stagingDir, targetVersion, registry, env = process.env, log = () => {}, timeoutMs = 10 * 60 * 1000 }) {
  return new Promise((resolve, reject) => {
    const npmArgs = [
      'install',
      '--no-audit',
      '--no-fund',
      '--omit=dev',
      '--prefer-online',
      '--registry=' + registry,
      AGENT_PKG + '@' + targetVersion,
    ];
    const cmd = npmCli ? nodeExe : (process.platform === 'win32' ? 'npm.cmd' : 'npm');
    const args = npmCli ? [npmCli, ...npmArgs] : npmArgs;
    const childEnv = {
      ...env,
      NODE_USE_ENV_PROXY: '1',
      npm_config_update_notifier: 'false',
      npm_config_fund: 'false',
      npm_config_audit: 'false',
    };
    log('执行 npm install: ' + AGENT_PKG + '@' + targetVersion + ' (registry=' + registry + ')');
    const child = spawn(cmd, args, {
      cwd: stagingDir,
      env: childEnv,
      stdio: ['ignore', 'pipe', 'pipe'],
      windowsHide: true,
    });
    let stderr = '';
    let stdout = '';
    const timer = setTimeout(() => {
      try { child.kill('SIGKILL'); } catch {}
      reject(new Error('npm install 超时（>10 分钟）'));
    }, timeoutMs);
    child.stdout.on('data', (c) => { stdout += String(c); });
    child.stderr.on('data', (c) => {
      const s = String(c);
      stderr += s;
      if (stderr.length > 8192) stderr = stderr.slice(-8192);
    });
    child.on('error', (err) => {
      clearTimeout(timer);
      reject(err);
    });
    child.on('close', (code) => {
      clearTimeout(timer);
      if (code === 0) return resolve({ stdout, stderr });
      const tail = (stderr || stdout).trim().split(/\r?\n/).slice(-6).join(' | ');
      reject(new Error('npm install 退出码 ' + code + (tail ? '：' + tail : '')));
    });
  });
}

/**
 * 将目标稳定版 @deepseek-ai/dsh 安装到独立暂存目录并预打全部桌面补丁。
 * 暂存目录位于 <userDataDir>/agent-staging-<pid>，不影响正在运行的内核；
 * 成功后由 Rust 壳停内核并原子切换为 <userDataDir>/agent。
 */
async function stageAgentUpdate({
  targetVersion,
  appDir,
  userDataDir,
  registry,
  nodeExe = process.execPath,
  applyPatches,
  installRunner = runNpmInstall,
  log = () => {},
  env = process.env,
}) {
  const ver = String(targetVersion || '').trim().replace(/^v/, '');
  if (!SAFE_STABLE_VERSION_RE.test(ver) || isAlphaVersion(ver)) {
    return { ok: false, error: '非法或非稳定版本号：' + String(targetVersion || '') };
  }
  if (!userDataDir) {
    return { ok: false, error: '缺少 userDataDir，无法暂存内核更新' };
  }

  // 清理遗留的旧暂存目录（best-effort）。
  try {
    if (fs.existsSync(userDataDir)) {
      for (const name of fs.readdirSync(userDataDir)) {
        if (name.startsWith('agent-staging-')) {
          try { fs.rmSync(path.join(userDataDir, name), { recursive: true, force: true }); } catch {}
        }
      }
    }
  } catch {}

  const stagingDir = path.join(userDataDir, 'agent-staging-' + process.pid + '-' + Date.now());
  fs.mkdirSync(stagingDir, { recursive: true });
  fs.writeFileSync(
    path.join(stagingDir, 'package.json'),
    JSON.stringify({ name: 'dsh-agent-overlay', private: true, version: '0.0.0' }, null, 2) + '\n',
    'utf8',
  );

  const npmCli = findVendorNpmCli(appDir);
  const registries = registry ? [String(registry).replace(/\/+$/, '')] : resolveRegistries(env);
  let lastErr = null;
  let usedRegistry = '';

  for (const reg of registries) {
    try {
      await installRunner({ nodeExe, npmCli, stagingDir, targetVersion: ver, registry: reg, env, log });
      usedRegistry = reg;
      lastErr = null;
      break;
    } catch (err) {
      lastErr = err;
      log('registry ' + reg + ' 安装失败，尝试回退：' + String((err && err.message) || err));
    }
  }

  if (lastErr) {
    try { fs.rmSync(stagingDir, { recursive: true, force: true }); } catch {}
    return { ok: false, error: '下载安装失败：' + String((lastErr && lastErr.message) || lastErr) };
  }

  const binJs = path.join(stagingDir, 'node_modules', '@deepseek-ai', 'dsh', 'lib', 'bin.js');
  const installedVer = readAgentVersion(stagingDir);
  if (!fs.existsSync(binJs) || !installedVer) {
    try { fs.rmSync(stagingDir, { recursive: true, force: true }); } catch {}
    return { ok: false, error: '安装校验失败：未找到 @deepseek-ai/dsh/lib/bin.js' };
  }

  // 对暂存副本预打全部桌面补丁（仅作用于 stagingDir，不触碰 appDir 与真实 ~/.dsh）。
  let patchSummary = null;
  if (typeof applyPatches === 'function') {
    try {
      const rawReport = await applyPatches(stagingDir);
      patchSummary = summarizePatchReport(rawReport);
    } catch (err) {
      log('暂存目录预打补丁告警（容忍继续）: ' + String((err && err.message) || err));
    }
  }
  const patchWarning = patchSummary && patchSummary.warning ? patchSummary.warning : '';

  try {
    fs.writeFileSync(
      path.join(stagingDir, '.dsh-agent-meta.json'),
      JSON.stringify({
        version: installedVer,
        requestedVersion: ver,
        registry: usedRegistry,
        installedAt: new Date().toISOString(),
        source: 'npm-latest',
        patchSummary,
      }, null, 2) + '\n',
      'utf8',
    );
  } catch {}

  return {
    ok: true,
    stagingDir,
    version: installedVer,
    registry: usedRegistry,
    patchSummary,
    patchWarning,
  };
}

module.exports = {
  AGENT_PKG,
  DEFAULT_REGISTRIES,
  SAFE_STABLE_VERSION_RE,
  isAlphaVersion,
  selectStableVersion,
  resolveRegistries,
  readAgentVersion,
  resolveActiveAgentRoot,
  checkAgentUpdate,
  stageAgentUpdate,
  findVendorNpmCli,
  summarizePatchReport,
};
