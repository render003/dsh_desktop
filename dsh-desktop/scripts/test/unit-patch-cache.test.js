'use strict';

// unit-patch-cache.test.js — boot 补丁指纹快路径缓存（scripts/lib/patch-cache.js）回归测试。

const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');

const {
  computePatchFingerprint,
  readPatchCache,
  writePatchCache,
} = require('../lib/patch-cache');

function tmpdir(t) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'unit-patch-cache-'));
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
  return dir;
}

function makeFixture(t) {
  const root = tmpdir(t);
  const appDir = path.join(root, 'app');
  const home = path.join(root, 'home');
  const userDataDir = path.join(root, 'userdata');
  fs.mkdirSync(path.join(appDir, 'scripts', 'lib'), { recursive: true });
  fs.mkdirSync(path.join(appDir, 'scripts', 'integration'), { recursive: true });
  fs.mkdirSync(path.join(appDir, 'node_modules', '@deepseek-ai', 'dsh-api-session-controller', 'lib'), { recursive: true });
  fs.mkdirSync(path.join(home, 'profiles'), { recursive: true });
  fs.mkdirSync(userDataDir, { recursive: true });

  fs.writeFileSync(path.join(appDir, 'package.json'), JSON.stringify({ name: 'dsh-desktop', version: '1.1.0' }));
  fs.writeFileSync(path.join(appDir, 'scripts', 'lib', 'patch-registry.js'), '// reg\n');
  fs.writeFileSync(path.join(appDir, 'scripts', 'patch-sample.js'), '// sample\n');
  const targetFile = path.join(appDir, 'node_modules', '@deepseek-ai', 'dsh-api-session-controller', 'lib', 'client.js');
  fs.writeFileSync(targetFile, 'console.log("v1");\n');

  const specs = [
    {
      id: 'sample-file-spec',
      kind: 'file',
      layout: 'runtime-local',
      pkgRel: path.join('dsh-api-session-controller', 'lib', 'client.js'),
    },
    {
      id: 'sample-root-spec',
      kind: 'root',
      layout: 'nm-roots',
    },
  ];
  return { appDir, home, userDataDir, targetFile, specs };
}

test('指纹稳定性 + 缓存读写往返', (t) => {
  const fx = makeFixture(t);
  const fp1 = computePatchFingerprint({ ...fx });
  const fp2 = computePatchFingerprint({ ...fx });
  assert.ok(typeof fp1 === 'string' && fp1.length === 64, '应生成 64 位 SHA-256 指纹');
  assert.equal(fp1, fp2, '无变更时两次指纹必须一致');

  const cacheFile = path.join(fx.userDataDir, 'patch-cache.json');
  assert.equal(readPatchCache(cacheFile), null, '缓存不存在时返回 null');
  assert.equal(writePatchCache(cacheFile, fp1), true);
  assert.equal(readPatchCache(cacheFile), fp1, '写入后读取必须命中');
});

test('目标文件修改 / 补丁脚本变更 / Agent 覆盖层切换均使指纹立刻失效', (t) => {
  const fx = makeFixture(t);
  const base = computePatchFingerprint({ ...fx });

  // 1. 目标文件内容（size/mtime）变化
  fs.writeFileSync(fx.targetFile, 'console.log("v2-modified");\n');
  const afterTarget = computePatchFingerprint({ ...fx });
  assert.notEqual(afterTarget, base, '目标文件变更必须使指纹失效');

  // 2. scripts/patch-*.js 变更
  fs.writeFileSync(path.join(fx.appDir, 'scripts', 'patch-sample.js'), '// sample v2\n');
  const afterScript = computePatchFingerprint({ ...fx });
  assert.notEqual(afterScript, afterTarget, '补丁脚本变更必须使指纹失效');

  // 3. agent-active.json 变化
  fs.writeFileSync(path.join(fx.userDataDir, 'agent-active.json'), JSON.stringify({ version: '0.3.0', root: fx.appDir }));
  const afterOverlay = computePatchFingerprint({ ...fx });
  assert.notEqual(afterOverlay, afterScript, 'agent-active.json 变更必须使指纹失效');
});

test('强制标志 / WSL 模式 / safe-mode.flag 均返回 null（禁止跳过补丁）', (t) => {
  const fx = makeFixture(t);
  assert.equal(computePatchFingerprint({ ...fx, force: true }), null, 'force=true 必须禁止跳过');
  assert.equal(computePatchFingerprint({ ...fx, wslMode: true }), null, 'wslMode=true 必须禁止跳过');

  fs.writeFileSync(path.join(fx.home, 'safe-mode.flag'), '1');
  assert.equal(computePatchFingerprint({ ...fx }), null, 'safe-mode.flag 在位时必须禁止跳过');
});
