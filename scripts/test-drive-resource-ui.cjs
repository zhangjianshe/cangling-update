const fs = require('node:fs'), vm = require('node:vm'), assert = require('node:assert/strict');
const source = fs.readFileSync('src/assets/index.html', 'utf8');
const functions = source.slice(source.indexOf('function renderInstallResourcesUpdateState()'), source.indexOf('async function loadInstallResources()'));
assert(source.includes('stopInstallResourcesPoll();\n      stopComposePoll();'));
assert(source.includes('Promise.all([loadInstallResources(), pollInstallResourcesUpdate()])'));
function setup(responses) {
  const elements = {};
  const element = () => ({ children: [], textContent: '', hidden: true, style: {}, setAttribute() {}, appendChild(child) { this.children.push(child); } });
  elements['#install-resources-update'] = element();
  elements['#install-resources-update-status'] = element();
  let refreshed = 0, requests = [], timers = [];
  const context = vm.createContext({ location: { hash: '#/install-resources' }, $: id => elements[id], document: { createElement: element },
    installResources: { path: '', updating: false, updateMessage: '', generation: 0 }, fmtBytes: value => value + ' B',
    clearTimeout() {}, setTimeout(fn) { timers.push(fn); return timers.length; }, confirmDialog: async () => true,
    api: async (path, options) => { requests.push([path, options]); const next = responses.shift(); if (next instanceof Error) throw next; return next; },
    resetInstallResourcePreview() {}, loadInstallResources: async () => refreshed++,
  });
  vm.runInContext(functions, context);
  return { context, elements, timers, requests, refreshed: () => refreshed };
}
const running = { running: true, phase: 'download', path: 'packages/example.iso', current: 2, total: 10, bytesDone: 30, bytesTotal: 100, overallDone: 250, overallTotal: 1000, speed: 50 };
const complete = { running: false, phase: 'complete', result: { totalFiles: 10, downloaded: 2, skipped: 8, resumed: 1 } };
(async () => {
  // A fresh browser has no local job state, but restores the server task on its first GET.
  const t = setup([running, complete]);
  await t.context.pollInstallResourcesUpdate();
  assert.equal(t.requests[0][1], undefined);
  assert.equal(t.elements['#install-resources-update'].disabled, true);
  const children = t.elements['#install-resources-update-status'].children;
  assert.equal(children[0].value, 25);
  assert.match(children[1].textContent, /2\/10 文件.*25.0%.*下载速度 50 B\/s/);
  assert.match(children[2].textContent, /packages\/example.iso.*30 B \/ 100 B/);
  await t.context.updateInstallResources(); assert.equal(t.requests.length, 1);
  await t.context.pollInstallResourcesUpdate();
  assert.equal(t.elements['#install-resources-update'].disabled, false);
  assert.match(t.elements['#install-resources-update-status'].textContent, /10 文件.*续传 1/);
  assert.equal(t.refreshed(), 1);
  const restored = setup([complete]); await restored.context.pollInstallResourcesUpdate();
  assert.match(restored.elements['#install-resources-update-status'].textContent, /10 文件/);
  const failed = setup([{ running: false, error: 'checksum failed' }]); await failed.context.pollInstallResourcesUpdate();
  assert.match(failed.elements['#install-resources-update-status'].textContent, /资源更新失败：checksum failed/);
  const start = setup([running, running]); await start.context.updateInstallResources();
  assert.equal(start.requests[0][1].method, 'POST'); assert.equal(start.context.installResources.updating, true);
  const uncertain = setup([new Error('connection lost'), running]); await uncertain.context.updateInstallResources();
  assert.equal(uncertain.context.installResources.updating, true);
  const retry = setup([new Error('offline'), running]); await retry.context.pollInstallResourcesUpdate();
  assert.match(retry.elements['#install-resources-update-status'].textContent, /正在重试/);
  assert.equal(retry.timers.length, 1); await retry.context.pollInstallResourcesUpdate();
  assert.equal(retry.context.installResources.updating, true);
  const away = setup([]); away.context.location.hash = '#/other'; await away.context.pollInstallResourcesUpdate();
  assert.equal(away.requests.length, 0);
  console.log('Server task restoration, progress, speed, result, request failure recovery and navigation passed');
})().catch(error => { console.error(error); process.exitCode = 1; });
