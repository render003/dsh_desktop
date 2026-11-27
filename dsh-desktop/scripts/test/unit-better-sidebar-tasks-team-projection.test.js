'use strict';

// ---------------------------------------------------------------------------
// unit-better-sidebar-tasks-team-projection.test.js
//
// 回归锁：修复打开「任务管理」标签页时 RenderBoundary 报错
//   `dsh-better-sidebar: Cannot read properties of undefined (reading 'map')`
//
// 根因：DSH 0.2.1-rc.2 / 0.2.2 起，`@deepseek-ai/dsh-agent-team` 移除了
// `agentTeam` 投影里的 `members` 字段，并对每个会话都下发
// `{ enabled: boolean, tasks: [...], failure?: string }`（未开启时
// `enabled: false`）。旧版 `dsh-better-sidebar` 0.24.2 仍按 0.1.7 契约
// 直接执行 `team.members.map(...)`，导致每次打开任务管理页在渲染期抛
// TypeError。同时宿主写接口 `ctx.agentTeams.tryMembership` 在 0.2.2 改为
// `isEnabled`，且任务归属使用 `ownerId`。
//
// 验证方式：
//   1. 直接从随包交付的 `lib/client.js` 与 `lib/client-registry.js` 切片实跑
//      `team-projection.ts` / `subagent-catalog.ts` / `tasks-model.ts` 区段；
//   2. 从 `lib/index.js` 切片实跑 `team-routes.ts` 路由鉴权（0.2.2 `isEnabled`
//      与旧版 `tryMembership` 双形态）；
//   3. src ↔ 产物顶层声明同源护栏。
// ---------------------------------------------------------------------------

const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

const {
  PLUGIN_DIR,
  extractRegion,
  loadRegionSymbols,
  regionTopLevelNames,
  srcTopLevelNames,
} = require('./fixtures/better-sidebar-region.js');

const CHANNEL_BUNDLES = ['client.js', 'client-registry.js'];
const TEAM_PROJECTION = 'src/client/team-projection.ts';
const SUBAGENT_CATALOG = 'src/client/subagent-catalog.ts';
const SUBAGENT_LINEAGE = 'src/client/subagent-lineage.ts';
const TASKS_MODEL = 'src/client/tasks-model.ts';

function loadTeamProjection(bundleRel) {
  return loadRegionSymbols(bundleRel, TEAM_PROJECTION, ['teamProjectionOf', 'teamMembersOf']);
}

function loadSubagentCatalog(bundleRel) {
  return loadRegionSymbols(bundleRel, SUBAGENT_CATALOG, ['subagentCatalogs', 'isKnownLeaf', 'childLive']);
}

function loadTasksModel(bundleRel) {
  return loadRegionSymbols(
    bundleRel,
    [SUBAGENT_LINEAGE, SUBAGENT_CATALOG, TASKS_MODEL],
    ['buildTasksModel', 'subagentCatalogs'],
    { SIDE_LABEL_PREFIX: 'Side: ' },
  );
}

// ===========================================================================
// 1. DSH 0.2.2 agentTeam 投影形状：无 members 字段、未开启时 enabled: false
// ===========================================================================

for (const bundle of CHANNEL_BUNDLES) {
  test(`${bundle}：0.2.2 未开启 Agent Teams（{ enabled: false, tasks: [] }）隐藏团队面板且不抛 .map 错`, () => {
    const { teamProjectionOf, teamMembersOf, plain } = loadTeamProjection(bundle);
    const projections = {
      root1: {
        state: 'ready',
        error: null,
        values: {
          agentTeam: { enabled: false, tasks: [] },
        },
      },
    };
    const team = teamProjectionOf(projections, 'root1');
    assert.equal(team, undefined, 'enabled === false 时应视为无活跃团队，避免每个普通会话都渲染空 TeamBoard');
    assert.deepEqual(plain(teamMembersOf(team, {}, {})), []);
    // 即使调用方直接传入 0.2.2 原始对象，也不得抛 Cannot read properties of undefined (reading 'map')。
    assert.deepEqual(plain(teamMembersOf({ enabled: false, tasks: [] }, {}, {})), []);
  });

  test(`${bundle}：0.2.2 开启 Agent Teams（{ enabled: true, tasks: [...] } 无 members）不崩溃且保留任务板`, () => {
    const { teamProjectionOf, teamMembersOf, plain } = loadTeamProjection(bundle);
    const task = {
      id: 'task-1',
      revision: 1,
      subject: '实现修复',
      description: 'desc',
      status: 'in_progress',
      ownerId: 'child-1',
      ownerName: 'worker',
      blockedBy: [],
      writeScopes: [],
      ready: false,
      writeScopeWarnings: [],
    };
    const projections = {
      root1: {
        state: 'ready',
        error: null,
        values: {
          agentTeam: { enabled: true, tasks: [task] },
        },
      },
    };
    const team = plain(teamProjectionOf(projections, 'root1'));
    assert.equal(team.enabled, true);
    assert.equal(team.tasks.length, 1);
    assert.deepEqual(plain(teamMembersOf(team, {}, {})), [], '无 members 字段时应安全降级为空数组');
  });

  test(`${bundle}：向后兼容 0.1.7 旧投影（无 enabled、有 members）`, () => {
    const { teamProjectionOf, teamMembersOf, plain } = loadTeamProjection(bundle);
    const projections = {
      root1: {
        state: 'ready',
        error: null,
        values: {
          agentTeam: {
            members: [
              { id: 'root1', name: 'lead', role: 'lead', phase: 'active' },
              { id: 'c1', name: 'alpha', role: 'teammate', phase: 'active' },
              { id: 'c2', name: 'beta', role: 'teammate', phase: 'failed', error: 'boom' },
            ],
            tasks: [],
          },
        },
      },
    };
    const team = teamProjectionOf(projections, 'root1');
    assert.ok(team !== undefined);
    const members = plain(teamMembersOf(
      team,
      { c1: { running: true } },
      { root1: { id: 'root1', displayTitle: 'Main', running: false } },
    ));
    assert.deepEqual(
      members.map((m) => [m.id, m.status, m.diagnostics]),
      [
        ['root1', 'idle', []],
        ['c1', 'running', []],
        ['c2', 'failed', ['boom']],
      ],
    );
  });

  test(`${bundle}：buildTasksModel 支持 0.2.2 ownerId 挂载任务、跳过 external 的 childAddress`, () => {
    const { buildTasksModel, subagentCatalogs, plain } = loadTasksModel(bundle);
    const catalogs = subagentCatalogs({
      root1: {
        state: 'ready',
        error: null,
        values: {
          subagentCatalog: [
            { id: 'child-1', createdAt: 1, mode: 'continuable', label: 'Worker', name: 'worker' },
            { id: 'ext-1', createdAt: 2, mode: 'external', label: 'ExternalJob', name: 'ext' },
          ],
        },
      },
      'child-1': { state: 'ready', error: null, values: { subagentCatalog: [] } },
      'ext-1': { state: 'ready', error: null, values: { subagentCatalog: [] } },
    });
    const nodes = plain(buildTasksModel({
      byId: {
        root1: { id: 'root1', displayTitle: 'Main', running: true },
        'child-1': { id: 'child-1', displayTitle: 'Worker', origin: 'subagent', parentId: 'root1' },
      },
      catalogs,
      rootId: 'root1',
      currentSessionId: 'root1',
      live: { 'child-1': { running: true } },
      runs: [],
      teamMembers: [],
      teamTasks: [
        {
          id: 'task-1',
          revision: 1,
          subject: '按 ownerId 绑定',
          description: '',
          status: 'in_progress',
          ownerId: 'child-1',
          ownerName: 'worker',
          blockedBy: [],
          writeScopes: [],
          ready: false,
          writeScopeWarnings: [],
        },
      ],
      folded: false,
    }));
    const child = nodes.find((n) => n.id === 'child-1');
    const ext = nodes.find((n) => n.id === 'ext-1');
    assert.ok(child !== undefined, 'continuable 子代理应入模型');
    assert.deepEqual(child.tasks.map((t) => t.id), ['task-1'], '即使 teamMembers 为空，0.2.2 任务仍按 ownerId 挂到对应节点');
    assert.ok(child.childAddress !== undefined, 'continuable 子代理保留跳转地址');
    assert.ok(ext !== undefined, 'external 子代理应入模型');
    assert.equal(ext.childAddress, undefined, 'external 子代理无本地会话，不应附带 childAddress');
  });

  test(`${bundle}：subagentCatalogs 对缺失 values / 非数组 subagentCatalog 判空不崩`, () => {
    const { subagentCatalogs, plain } = loadSubagentCatalog(bundle);
    const views = plain(subagentCatalogs({
      s1: { state: 'idle', error: null },
      s2: { state: 'ready', error: null, values: { subagentCatalog: null } },
    }));
    assert.deepEqual(views.s1.entries, []);
    assert.equal(views.s1.state, 'loading');
    assert.deepEqual(views.s2.entries, []);
  });
}

// ===========================================================================
// 2. 宿主 team-routes.ts（lib/index.js）：0.2.2 isEnabled 与旧版 tryMembership 兼容
// ===========================================================================

test('lib/index.js team-routes：优先调用 0.2.2 isEnabled(agent)，缺失 tryMembership 不报 TypeError', async () => {
  const region = extractRegion(path.join(PLUGIN_DIR, 'lib', 'index.js'), 'src/team-routes.ts');
  class SidebarError extends Error {
    constructor(code, message, status = 400) {
      super(message);
      this.code = code;
      this.status = status;
    }
  }
  const requireString = (payload, key) => {
    if (!payload || typeof payload[key] !== 'string' || payload[key] === '') {
      throw new SidebarError('bad-request', `missing ${key}`, 400);
    }
    return payload[key];
  };
  const { buildTeamsApi } = vm.runInNewContext(
    `${region}\n({ buildTeamsApi })`,
    vm.createContext({ SidebarError, requireString, Set, Error }),
    { filename: 'lib/index.js#src/team-routes.ts' },
  );

  // 0.2.2 形态：只有 isEnabled，没有 tryMembership。
  let enabled = false;
  const created = [];
  const fakeAgent = { id: 'root-1' };
  const api = buildTeamsApi({
    get(name) {
      if (name === 'agents') return { get: (id) => (id === 'root-1' ? fakeAgent : undefined) };
      if (name === 'agentTeams') {
        return {
          isEnabled: () => enabled,
          async createTask(_agent, req) {
            created.push(req);
            return { id: 'task-1', revision: 1, ...req, status: 'pending', blockedBy: [], writeScopes: [], ready: true, writeScopeWarnings: [] };
          },
          async updateTask() {
            throw new Error('unused');
          },
        };
      }
      return undefined;
    },
  });

  await assert.rejects(
    () => api.taskCreate({ rootSessionId: 'root-1', subject: 's1' }),
    (err) => err instanceof SidebarError && err.status === 404 && /leads no team/.test(err.message),
    'isEnabled 为 false 时应返回 404',
  );

  enabled = true;
  const res = await api.taskCreate({ rootSessionId: 'root-1', subject: 's1', description: 'd1' });
  assert.equal(res.ok, true);
  assert.equal(created.length, 1);
});

// ===========================================================================
// 3. src ↔ 产物同源护栏
// ===========================================================================

test('同源护栏：team-projection.ts 与 subagent-catalog.ts 顶层符号与两份频道产物一致', () => {
  for (const bundle of CHANNEL_BUNDLES) {
    for (const rel of [TEAM_PROJECTION, SUBAGENT_CATALOG]) {
      assert.deepEqual(
        regionTopLevelNames(bundle, rel),
        srcTopLevelNames(rel),
        `${bundle} 的 ${rel} 顶层声明应与 src 一致`,
      );
    }
  }
});
