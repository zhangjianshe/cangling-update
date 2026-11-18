const fs = require('node:fs'), vm = require('node:vm'), assert = require('node:assert/strict');
const source = fs.readFileSync('src/assets/index.html', 'utf8');
const functions = source.slice(source.indexOf('function renderInstallResourcesUpdateState()'), source.indexOf('async function loadInstallResources()'));
const render = source.slice(source.indexOf('async function renderInstallResources()'), source.indexOf('function renderInstallResourcesUpdateState()'));
assert(render.includes('renderInstallResourcesUpdateState();'), 're-entering the page restores update state');
function setup() {
  let elements, resolve, reject, requests = 0, refreshed = 0;
  const location = { hash: '#/install-resources' };
  const state = { path: 'old', updating: false, updateKind: '', updateMessage: '' };
  function mount() {
    elements = { '#install-resources-update': { disabled: false, textContent: '资源更新' }, '#install-resources-update-status': { hidden: true, textContent: '' } };
    return elements;
  }
  mount();
  const context = vm.createContext({ location, $: id => elements[id], installResources: state,
    confirmDialog: async () => true,
    api: (path, options) => { requests++; assert.equal(path, '/api/install-resources/refresh'); assert.equal(options.method, 'POST'); return new Promise((yes, no) => { resolve = yes; reject = no; }); },
    resetInstallResourcePreview: () => {}, loadInstallResources: async () => { refreshed++; },
  });
  vm.runInContext(functions, context);
  return { context, location, state, mount, resolve: value => resolve(value), reject: error => reject(error), requests: () => requests, refreshed: () => refreshed };
}
const report = { totalFiles: 3, downloaded: 1, skipped: 2, resumed: 1 };
(async () => {
  const t = setup();
  const pending = t.context.updateInstallResources();
  await new Promise(setImmediate);
  assert(t.state.updating);
  t.location.hash = '#/other'; t.mount();
  t.location.hash = '#/install-resources'; const returned = t.mount();
  t.context.renderInstallResourcesUpdateState();
  assert.equal(returned['#install-resources-update'].disabled, true);
  assert.equal(returned['#install-resources-update'].textContent, '更新中…');
  assert.match(returned['#install-resources-update-status'].textContent, /正在同步/);
  await t.context.updateInstallResources(); assert.equal(t.requests(), 1);
  t.resolve(report); await pending;
  assert.equal(returned['#install-resources-update'].disabled, false);
  assert.match(returned['#install-resources-update-status'].textContent, /3 文件.*续传 1/);
  assert.equal(t.refreshed(), 1);
  for (const fail of [false, true]) {
    const t = setup(), pending = t.context.updateInstallResources(); await new Promise(setImmediate);
    t.location.hash = '#/other'; const other = t.mount();
    if (fail) t.reject(new Error('offline')); else t.resolve(report);
    await pending;
    assert.equal(t.refreshed(), 0);
    assert.equal(other['#install-resources-update-status'].textContent, '');
    t.location.hash = '#/install-resources'; const returned = t.mount();
    t.context.renderInstallResourcesUpdateState();
    assert.equal(returned['#install-resources-update'].disabled, false);
    assert.equal(returned['#install-resources-update-status'].hidden, false);
    assert.match(returned['#install-resources-update-status'].textContent, fail ? /资源更新失败：offline/ : /3 文件/);
  }
  console.log('Resource update state survives page navigation, completion and failure; duplicate requests are blocked');
})().catch(error => { console.error(error); process.exitCode = 1; });
