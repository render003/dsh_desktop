'use strict';

// 单测：scripts/lib/agent-update.js
// 验证：
//   1. selectStableVersion：优先 latest、跳过 alpha、按 semver 比较正式版 > rc；
//   2. checkAgentUpdate：npmmirror → npmjs 双源回退、NPM_CONFIG_REGISTRY 覆盖、有/无更新判定；
//   3. resolveActiveAgentRoot：仅覆盖层完整且版本严格高于内置时切到 updated，否则回落 bundled；
//   4. stageAgentUpdate：拒绝 alpha 与非法版本号、校验 bin.js 与补丁回调、写 meta。

const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');

const {
  isAlphaVersion,
  selectStableVersion,
  resolveRegistries,
  resolveActiveAgentRoot,
  checkAgentUpdate,
  stageAgentUpdate,
  summarizePatchReport,
} = require('../lib/agent-update');

function mkAgentTree(root, version, withBin = true) {
  const pkgDir = path.join(root, 'node_modules', '@deepseek-ai', 'dsh');
  fs.mkdirSync(path.join(pkgDir, 'lib'), { recursive: true });
  fs.writeFileSync(path.join(pkgDir, 'package.json'), JSON.stringify({ name: '@deepseek-ai/dsh', version }), 'utf8');
  if (withBin) {
    fs.writeFileSync(path.join(pkgDir, 'lib', 'bin.js'), '// stub\n', 'utf8');
  }
}

test('selectStableVersion：优先非 alpha 的 latest，跳过 alpha，正式版大于 rc', () => {
  assert.equal(isAlphaVersion('0.2.3-alpha.2'), true);
  assert.equal(isAlphaVersion('0.2.2-rc.5'), false);
  assert.equal(isAlphaVersion('0.2.2'), false);

  assert.equal(selectStableVersion({ latest: '0.2.2-rc.5', alpha: '0.2.3-alpha.2' }, []), '0.2.2-rc.5');
  assert.equal(selectStableVersion({ latest: '0.2.2', alpha: '0.2.3-alpha.2' }, []), '0.2.2');
  // 若 latest 意外指向 alpha，从 versions 选最高非 alpha 版本
  assert.equal(
    selectStableVersion(
      { latest: '0.2.3-alpha.2' },
      ['0.2.2-rc.4', '0.2.2-rc.5', '0.2.2', '0.2.3-alpha.1', '0.2.3-alpha.2'],
    ),
    '0.2.2',
  );
  // 全是 alpha 时返回空串
  assert.equal(selectStableVersion({ latest: '0.2.3-alpha.2' }, ['0.2.3-alpha.1', '0.2.3-alpha.2']), '');
});

test('checkAgentUpdate：双源回退与版本比较（正式版 > rc，跳过 alpha）', async () => {
  const seen = [];
  const res1 = await checkAgentUpdate({
    currentVersion: '0.2.2-rc.5',
    agentSource: 'bundled',
    env: {},
    httpGetJson: async (url) => {
      seen.push(url);
      if (url.includes('npmmirror')) throw new Error('镜像超时');
      return { version: '0.2.2' };
    },
  });
  assert.equal(res1.ok, true);
  assert.equal(res1.latestVersion, '0.2.2');
  assert.equal(res1.hasUpdate, true, '0.2.2 应大于 0.2.2-rc.5');
  assert.equal(res1.registry, 'https://registry.npmjs.org');
  assert.equal(seen.length, 2, '应先试 npmmirror 再回退 npmjs');

  // 已是最新
  const res2 = await checkAgentUpdate({
    currentVersion: '0.2.2-rc.5',
    env: { NPM_CONFIG_REGISTRY: 'https://custom.npm.local/' },
    httpGetJson: async (url) => {
      assert.equal(url, 'https://custom.npm.local/%40deepseek-ai%2Fdsh/latest');
      return { version: '0.2.2-rc.5' };
    },
  });
  assert.equal(res2.ok, true);
  assert.equal(res2.hasUpdate, false);
  assert.equal(res2.registry, 'https://custom.npm.local');

  // latest 意外为 alpha 时回查 packument 选非 alpha
  const res3 = await checkAgentUpdate({
    currentVersion: '0.2.2-rc.5',
    env: { NPM_CONFIG_REGISTRY: 'https://custom.npm.local' },
    httpGetJson: async (url) => {
      if (url.endsWith('/latest')) return { version: '0.2.3-alpha.2' };
      return {
        'dist-tags': { latest: '0.2.3-alpha.2' },
        versions: { '0.2.2-rc.5': {}, '0.2.2-rc.6': {}, '0.2.3-alpha.2': {} },
      };
    },
  });
  assert.equal(res3.ok, true);
  assert.equal(res3.latestVersion, '0.2.2-rc.6');
  assert.equal(res3.hasUpdate, true);

  // 双源均失败
  const res4 = await checkAgentUpdate({
    currentVersion: '0.2.2-rc.5',
    env: {},
    httpGetJson: async () => { throw new Error('ECONNREFUSED'); },
  });
  assert.equal(res4.ok, false);
  assert.match(res4.error, /检查更新失败/);
  assert.deepEqual(resolveRegistries({}), ['https://registry.npmmirror.com', 'https://registry.npmjs.org']);
});

test('resolveActiveAgentRoot：仅覆盖层完整且严格更新时采用 updated，否则回落 bundled', (t) => {
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'dsh-agent-active-'));
  t.after(() => fs.rmSync(tmp, { recursive: true, force: true }));
  const appDir = path.join(tmp, 'app');
  const ud = path.join(tmp, 'ud');
  mkAgentTree(appDir, '0.2.2-rc.5', true);

  // 无覆盖层 → bundled
  assert.deepEqual(resolveActiveAgentRoot({ appDir, userDataDir: ud }), {
    root: appDir, version: '0.2.2-rc.5', source: 'bundled',
  });

  // 覆盖层同版本或更旧 → 仍 bundled
  mkAgentTree(path.join(ud, 'agent'), '0.2.2-rc.5', true);
  assert.equal(resolveActiveAgentRoot({ appDir, userDataDir: ud }).source, 'bundled');

  // 覆盖层更高但缺 bin.js → 回落 bundled
  fs.rmSync(path.join(ud, 'agent'), { recursive: true, force: true });
  mkAgentTree(path.join(ud, 'agent'), '0.2.2', false);
  assert.equal(resolveActiveAgentRoot({ appDir, userDataDir: ud }).source, 'bundled');

  // 覆盖层更高且完整 → updated
  mkAgentTree(path.join(ud, 'agent'), '0.2.2', true);
  const active = resolveActiveAgentRoot({ appDir, userDataDir: ud });
  assert.equal(active.source, 'updated');
  assert.equal(active.version, '0.2.2');
  assert.equal(active.root, path.join(ud, 'agent'));
});

test('stageAgentUpdate：拒绝 alpha 与非法版本；成功安装后预打补丁并写 meta', async (t) => {
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'dsh-agent-stage-'));
  t.after(() => fs.rmSync(tmp, { recursive: true, force: true }));

  const badAlpha = await stageAgentUpdate({ targetVersion: '0.2.3-alpha.2', appDir: tmp, userDataDir: tmp });
  assert.equal(badAlpha.ok, false);
  assert.match(badAlpha.error, /非法或非稳定版本号/);

  const badInject = await stageAgentUpdate({ targetVersion: '0.2.2 && calc', appDir: tmp, userDataDir: tmp });
  assert.equal(badInject.ok, false);

  let patchedDir = '';
  const stages = [];
  const okRes = await stageAgentUpdate({
    targetVersion: '0.2.2',
    appDir: tmp,
    userDataDir: tmp,
    registry: 'https://registry.npmmirror.com',
    onProgress: (ev) => stages.push(ev.stage),
    installRunner: async ({ stagingDir, targetVersion }) => {
      mkAgentTree(stagingDir, targetVersion, true);
    },
    applyPatches: async (dir) => {
      patchedDir = dir;
      return { total: 66, changed: 50, anchorMissing: 2, degraded: ['slot-error-isolation'], failed: 0, errors: [] };
    },
  });
  assert.equal(okRes.ok, true);
  assert.deepEqual(stages, ['downloading', 'verifying', 'patching']);
  assert.equal(okRes.version, '0.2.2');
  assert.equal(patchedDir, okRes.stagingDir);
  assert.equal(okRes.patchSummary.anchorMissing, 2);
  assert.equal(okRes.patchSummary.degradedCount, 1);
  assert.match(okRes.patchWarning, /2 项补丁锚点失配/);
  assert.match(okRes.patchWarning, /1 项降级/);
  assert.ok(fs.existsSync(path.join(okRes.stagingDir, '.dsh-agent-meta.json')));

  const cleanSummary = summarizePatchReport({ total: 66, changed: 66, anchorMissing: 0, degraded: [], failed: 0, errors: [] });
  assert.equal(cleanSummary.warning, '');
});
