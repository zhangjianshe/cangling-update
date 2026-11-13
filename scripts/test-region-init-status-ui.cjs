const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const html = fs.readFileSync(path.join(__dirname, '../src/assets/index.html'), 'utf8');
for (const match of html.matchAll(/<script(?:\s[^>]*)?>([\s\S]*?)<\/script>/g)) new vm.Script(match[1]);
const start = html.indexOf('    async function loadCanglingDataInit()');
const end = html.indexOf('    async function startCanglingDataInit()', start);
assert(start >= 0 && end > start);
async function render(region) {
  const nodes = new Map();
  const $ = id => {
    if (!nodes.has(id)) nodes.set(id, {});
    return nodes.get(id);
  };
  const poll = { inflight: false, timer: null };
  const context = vm.createContext({
    $, state: { canglingTab: 'run', canglingRunTab: 'data-init' }, canglingDataInitPoll: poll,
    api: async route => route.endsWith('/region-init') ? region : { available: false, output: [] },
    setInterval: () => 1, clearInterval: () => {}
  });
  vm.runInContext(html.slice(start, end), context);
  await context.loadCanglingDataInit();
  assert.equal(poll.inflight, false);
  return { $, poll };
}
(async () => {
  const failure = { done: true, error: 'geom missing', message: 'old failure' };
  const recovered = await render({ initialized: true, available: false, reason: 'data and indexes ready', job: failure, output: ['old failure'] });
  assert.equal(recovered.$('#cangling-region-init-status').className, 'flash ok');
  assert.equal(recovered.$('#cangling-region-init-status').textContent, 'data and indexes ready');
  assert.equal(recovered.$('#cangling-region-init-output').hidden, true);
  assert.equal(recovered.$('#cangling-region-init-start').disabled, true);
  const failed = await render({ initialized: false, available: true, job: failure, output: ['actual failure'] });
  assert.equal(failed.$('#cangling-region-init-status').className, 'flash err');
  assert.equal(failed.$('#cangling-region-init-status').textContent, 'old failure');
  assert.equal(failed.$('#cangling-region-init-output').textContent, 'actual failure');
  const active = await render({ initialized: true, available: false, job: { done: false, message: 'verifying indexes', percent: 95 }, output: ['verifying'] });
  assert.equal(active.$('#cangling-region-init-status').textContent, 'verifying indexes');
  assert.equal(active.$('#cangling-region-init-progress').hidden, false);
  assert.equal(active.poll.timer, 1);
  const ready = await render({ initialized: true, available: false });
  assert.equal(ready.$('#cangling-region-init-status').className, 'flash ok');
  assert.match(ready.$('#cangling-region-init-status').textContent, /已就绪/);
  const missing = await render({ initialized: false, available: true, geometryRepairs: ['cl-base.public.sys_region'] });
  assert.match(missing.$('#cangling-region-init-status').textContent, /需补齐空间字段或几何数据/);
  console.log('Region status UI: repaired database supersedes old failure; current errors and active jobs remain visible.');
})().catch(error => { console.error(error); process.exitCode = 1; });
