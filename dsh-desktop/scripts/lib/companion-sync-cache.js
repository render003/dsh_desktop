'use strict';

// ---------------------------------------------------------------------------
// 配套插件同步快速指纹缓存（纯函数 + 轻量 fs 读写）。
//
// 背景：syncCompanionFiles 每次冷启动要对 32 个插件目录 + VENDOR_DEPS（含
// dsh-pocket 近 400 个依赖文件）逐个递归 statSync，无变更热启动也要 ~650ms。
// 这里按「安装包元数据 + 每个伴随插件源/目标关键路径（lstat 判链接）+ profile
// manifest / patch / 状态存储 + 退役目录存在态」算一次轻量 SHA-256 指纹：
//   - 约 100 次单层 stat/lstat（实测 < 15ms），命中即安全跳过全树递归同步；
//   - 任何源目录变更、目标目录被删/被替换成 symlink、用户增删改 patch/manifest、
//     触发安全模式（safe-mode.flag）或外部强制重同步（DSH_SYNC_FORCE=1）均立刻
//     失效，回落完整同步路径。
// ---------------------------------------------------------------------------

const fs = require('node:fs');
const path = require('node:path');
const crypto = require('node:crypto');

const { companionDirName } = require('./companion-plugins');
const { VENDOR_DEPS } = require('./companion-profile');
const { safeModeRequested } = require('./safe-mode');

const CACHE_VERSION = 1;

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

/**
 * 计算本次伴随插件同步的输入指纹。
 * 返回 null 表示禁止跳过（如 safe-mode.flag 在位或强制重同步）。
 */
function computeSyncFingerprint(opts) {
  const {
    appDir,
    home,
    profileDir,
    plugins = [],
    retiredPlugins = [],
    force = false,
  } = opts || {};
  if (force) return null;
  if (!appDir || !home || !profileDir) return null;
  if (safeModeRequested(home).enabled) return null;

  const h = crypto.createHash('sha256');
  const feed = (s) => h.update(String(s) + '\n');

  feed('v=' + CACHE_VERSION);
  feed('app=' + appDir);
  feed('prof=' + profileDir);
  feed('appPkg=' + lstatSig(path.join(appDir, 'package.json')));
  feed('profPkg=' + lstatSig(path.join(profileDir, 'package.json')));
  feed('profPatch=' + lstatSig(path.join(profileDir, 'cordis.patch.yml')));
  feed('state=' + lstatSig(path.join(home, 'desktop-plugin-state.json')));
  feed('hub=' + lstatSig(path.join(home, 'hotplug-hub', 'packs', 'dsh-desktop', 'hotpack.json')));

  const assetsRoot = path.join(appDir, 'assets', 'plugins');
  const vendorRoot = path.join(appDir, 'node_modules');
  const profNm = path.join(profileDir, 'node_modules');

  for (const name of VENDOR_DEPS) {
    feed('vd:' + name + ':' + lstatSig(path.join(vendorRoot, name, 'package.json'))
      + ':' + lstatSig(path.join(profNm, name))
      + ':' + lstatSig(path.join(profNm, name, 'package.json')));
  }

  for (const p of plugins) {
    const dir = companionDirName(p);
    const src = path.join(assetsRoot, dir);
    const dst = path.join(profNm, ...p.name.split('/'));
    feed([
      'p', p.id, p.name, p.shipsNodeModules ? '1' : '0',
      lstatSig(path.join(src, 'package.json')),
      lstatSig(path.join(src, 'lib')),
      lstatSig(path.join(src, 'dist')),
      lstatSig(path.join(src, 'cordis.patch.yml')),
      lstatSig(dst),
      lstatSig(path.join(dst, 'package.json')),
      lstatSig(path.join(dst, 'lib')),
    ].join('|'));
  }

  for (const r of retiredPlugins) {
    const dst = path.join(profNm, ...r.name.split('/'));
    feed('r:' + r.id + ':' + lstatSig(dst));
  }

  return h.digest('hex');
}

function readSyncCache(cacheFile) {
  try {
    const data = JSON.parse(fs.readFileSync(cacheFile, 'utf8'));
    if (data && data.v === CACHE_VERSION && typeof data.fingerprint === 'string') {
      return data.fingerprint;
    }
  } catch {}
  return null;
}

function writeSyncCache(cacheFile, fingerprint) {
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
  lstatSig,
  computeSyncFingerprint,
  readSyncCache,
  writeSyncCache,
};
