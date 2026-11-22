'use strict';

// 单测：scripts/lib/pocket-update-compat.js
// 验证 dsh-pocket 客户端更新/重启接入 window.dshDesktop 桥，以及宿主端非 workspace 不再传 -w。

const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');

const {
  POCKET_CLIENT_BRIDGE_MARKER,
  POCKET_HOST_UPDATE_MARKER,
  patchPocketClientSource,
  patchPocketHostSource,
  patchPocketDir,
} = require('../lib/pocket-update-compat');

const POCKET_DIR = path.resolve(__dirname, '..', '..', 'assets', 'plugins', 'dsh-pocket');

test('dsh-pocket 随包产物已含桌面更新桥与非 workspace 适配，且二次补丁幂等', () => {
  const clientSrc = fs.readFileSync(path.join(POCKET_DIR, 'client', 'client.js'), 'utf8');
  const hostSrc = fs.readFileSync(path.join(POCKET_DIR, 'lib', 'index.js'), 'utf8');

  assert.ok(clientSrc.includes(POCKET_CLIENT_BRIDGE_MARKER), 'client.js 应已接入桌面更新桥');
  assert.ok(clientSrc.includes('window.dshDesktop.pluginManager.update("dsh-pocket")'), 'runUpdate 应走 pluginManager.update');
  assert.ok(clientSrc.includes('window.dshDesktop.restartService()'), 'restartPocket 应走 restartService');
  assert.ok(!clientSrc.includes('update dsh-pocket --latest -w'), '手动提示不应再写死 -w');
  assert.equal(patchPocketClientSource(clientSrc).status, 'already');

  assert.ok(hostSrc.includes(POCKET_HOST_UPDATE_MARKER), 'lib/index.js 应已适配非 workspace 参数');
  assert.ok(hostSrc.includes('pnpm-workspace.yaml'), '宿主端仅在 workspace 下才追加 -w');
  assert.equal(patchPocketHostSource(hostSrc).status, 'already');
});

test('patchPocketDir：对模拟上游 tarball 目录首次改写、二次幂等；未知文件返回 anchor-missing', () => {
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'dsh-pocket-compat-'));
  try {
    fs.mkdirSync(path.join(tmp, 'client'), { recursive: true });
    fs.mkdirSync(path.join(tmp, 'lib'), { recursive: true });
    fs.writeFileSync(path.join(tmp, 'client', 'client.js'), [
      'const restartPocket = async () => {',
      '    setUpdateInfo((u) => ({ ...u, restarting: true, startedAt: Date.now() }));',
      '    try {',
      '      await call(POCKET_ENDPOINTS.restart, {});',
      '    } catch {}',
      '  };',
      '  const runUpdate = async () => {',
      '    setUpdateInfo((u) => ({ ...u, updating: true, result: null, startedAt: Date.now() }));',
      '    try {',
      '      const r = await call(POCKET_ENDPOINTS.update, {});',
      '    } catch {}',
      '  };',
      '  const msg = "dsh plugin --profile web update dsh-pocket --latest -w";',
    ].join('\n'));
    fs.writeFileSync(path.join(tmp, 'lib', 'index.js'), [
      'function performUpdate(profile) {',
      "    const child = spawn('dsh', ['plugin', '--profile', profile, 'update', 'dsh-pocket', '--latest', '-w'], {",
      '      stdio: ["ignore", "pipe", "pipe"],',
      '    });',
      '}',
    ].join('\n'));

    const r1 = patchPocketDir(tmp);
    assert.deepEqual(r1, { client: 'changed', host: 'changed' });
    const r2 = patchPocketDir(tmp);
    assert.deepEqual(r2, { client: 'already', host: 'already' });

    const patchedClient = fs.readFileSync(path.join(tmp, 'client', 'client.js'), 'utf8');
    assert.ok(!patchedClient.includes('-w'));
    assert.ok(patchedClient.includes('window.dshDesktop.pluginManager.update("dsh-pocket")'));
    assert.equal(patchPocketClientSource('/* unrelated */').status, 'anchor-missing');
    assert.equal(patchPocketHostSource('/* unrelated */').status, 'anchor-missing');
  } finally {
    fs.rmSync(tmp, { recursive: true, force: true });
  }
});
