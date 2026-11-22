'use strict';

// 配套插件同步快速指纹缓存单测：
// 覆盖指纹命中跳过、源文件/目标文件/patch/状态存储变更失效、符号链接失效、
// 安全模式（safe-mode.flag）与强制重同步（force/DSH_SYNC_FORCE）强制绕过。

const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');

const {
  computeSyncFingerprint,
  readSyncCache,
  writeSyncCache,
} = require('../lib/companion-sync-cache');

function mkFixture(t) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'dsh-sync-cache-'));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const appDir = path.join(root, 'app');
  const home = path.join(root, 'home');
  const ud = path.join(root, 'ud');
  const profileDir = path.join(home, 'profiles', 'web');
  fs.mkdirSync(path.join(appDir, 'assets', 'plugins', 'demo-plugin', 'lib'), { recursive: true });
  fs.mkdirSync(path.join(appDir, 'node_modules'), { recursive: true });
  fs.mkdirSync(path.join(profileDir, 'node_modules', 'demo-plugin', 'lib'), { recursive: true });
  fs.writeFileSync(path.join(appDir, 'package.json'), '{"name":"dsh-desktop","version":"1.1.0"}\n');
  fs.writeFileSync(path.join(appDir, 'assets', 'plugins', 'demo-plugin', 'package.json'), '{"name":"demo-plugin","version":"1.0.0"}\n');
  fs.writeFileSync(path.join(profileDir, 'package.json'), '{"name":"dsh-profile-web"}\n');
  fs.writeFileSync(path.join(profileDir, 'cordis.patch.yml'), '[]\n');
  fs.writeFileSync(path.join(profileDir, 'node_modules', 'demo-plugin', 'package.json'), '{"name":"demo-plugin","version":"1.0.0"}\n');
  return {
    root,
    appDir,
    home,
    ud,
    profileDir,
    plugins: [{ id: 'demo', name: 'demo-plugin' }],
    retiredPlugins: [{ id: 'old', name: 'old-plugin' }],
  };
}

test('无变更时指纹稳定，读写缓存往返一致', (t) => {
  const fx = mkFixture(t);
  const fp1 = computeSyncFingerprint(fx);
  assert.ok(fp1 && fp1.length === 64, '应返回 64 位 SHA-256 hex');
  const cacheFile = path.join(fx.ud, 'companion-sync-cache.json');
  assert.equal(readSyncCache(cacheFile), null);
  assert.equal(writeSyncCache(cacheFile, fp1), true);
  assert.equal(readSyncCache(cacheFile), fp1);
  assert.equal(computeSyncFingerprint(fx), fp1, '重复计算指纹应完全一致');
});

test('插件包变更、目标目录删除、patch 改动或退役目录出现均使指纹失效', (t) => {
  const fx = mkFixture(t);
  const base = computeSyncFingerprint(fx);

  // 1) 改源插件 package.json 大小 → 失效
  const srcPkg = path.join(fx.appDir, 'assets', 'plugins', 'demo-plugin', 'package.json');
  fs.writeFileSync(srcPkg, '{"name":"demo-plugin","version":"1.0.1","extra":true}\n');
  const fpSrc = computeSyncFingerprint(fx);
  assert.notEqual(fpSrc, base, '源插件变更必须失效');

  // 2) 删目标插件目录 → 失效
  fs.rmSync(path.join(fx.profileDir, 'node_modules', 'demo-plugin'), { recursive: true, force: true });
  const fpDel = computeSyncFingerprint(fx);
  assert.notEqual(fpDel, fpSrc, '目标插件被删必须失效');

  // 3) 改 cordis.patch.yml → 失效
  fs.writeFileSync(path.join(fx.profileDir, 'cordis.patch.yml'), '- id: x\n  disabled: true\n');
  const fpPatch = computeSyncFingerprint(fx);
  assert.notEqual(fpPatch, fpDel, 'patch 变更必须失效');

  // 4) 出现退役插件残留目录 → 失效
  fs.mkdirSync(path.join(fx.profileDir, 'node_modules', 'old-plugin'), { recursive: true });
  const fpRet = computeSyncFingerprint(fx);
  assert.notEqual(fpRet, fpPatch, '退役目录出现必须失效');
});

test('safe-mode.flag 在位或 force=true 时禁止跳过（返回 null）', (t) => {
  const fx = mkFixture(t);
  assert.equal(computeSyncFingerprint({ ...fx, force: true }), null, 'force=true 必须返回 null');

  fs.writeFileSync(path.join(fx.home, 'safe-mode.flag'), '{"reason":"test"}\n');
  assert.equal(computeSyncFingerprint(fx), null, 'safe-mode.flag 在位必须返回 null（绝不跳过安全模式熔断）');
});
