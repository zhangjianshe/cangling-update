// Exercise the actual inline UI handlers with a fake Kubernetes API; never restarts live workloads.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const path = require('node:path');
const html = fs.readFileSync(path.join(__dirname, '../src/assets/index.html'), 'utf8');
for (const match of html.matchAll(/<script(?:\s[^>]*)?>([\s\S]*?)<\/script>/g)) new vm.Script(match[1]);
function section(start, end) { return html.slice(html.indexOf(start), html.indexOf(end, html.indexOf(start))); }
const source = section('    function canglingJarDeploymentsFor(', '    function renderCanglingJars()') +
  section('    async function restartCanglingDeployment(', '    function renderCanglingDiagnosis()');
const jar = 'np4-base-server-1.0.0.jar';
function deployment(name, file = jar, spec = {}) {
  return { metadata: { name, namespace: 'custom' }, spec: { replicas: 1, ...spec,
    template: { spec: { volumes: [{ name: 'jar', hostPath: { path: '/opt/cangling/jars/' + file } }],
      containers: [{ volumeMounts: [{ name: 'jar', mountPath: '/app/application.jar' }] }] } } } };
}
function harness(items) {
  const calls = [], messages = [], prompts = [];
  const context = vm.createContext({ state: { canglingConfig: { namespace: 'custom' } },
    canglingJarRestarts: new Set(), canglingDeploymentRestarts: new Set(),
    canglingJarDeployments: items, canglingJarDeploymentError: '', canglingJarDeploymentLoad: 0,
    canglingJarJobWatch: '', progress: { hidden: true, classList: { contains() { return false; } } }, confirm: true, fail: '',
    renderCanglingJars() {}, renderCanglingRuntime() {}, async loadCanglingRuntime() {},
    $(selector) { return context.progress; }, esc: String,
    async api(url, options) {
      calls.push({ url, method: options?.method || 'GET' });
      if (!options) { if (context.readError) throw new Error('Kubernetes unavailable'); return { items }; }
      if (url.includes(context.fail) && context.fail) throw new Error('test failure');
      return {};
    },
    async confirmDialog(message) { prompts.push(message); return context.confirm; },
    flash(kind, message) { messages.push({ kind, message }); }
  });
  vm.runInContext(source, context);
  return { context, calls, messages, prompts };
}
(async () => {
  const direct = deployment('renamed-base');
  const subpath = deployment('second-base');
  subpath.spec.template.spec.volumes[0].hostPath.path = '/opt/cangling/jars/';
  subpath.spec.template.spec.containers[0].volumeMounts[0].subPath = jar;
  const unrelated = deployment('cis-map', 'cis-map-1.0.0.jar');
  const wrongRoot = deployment('wrong-root');
  wrongRoot.spec.template.spec.volumes[0].hostPath.path = '/elsewhere/' + jar;
  const unmounted = deployment('unmounted');
  unmounted.spec.template.spec.containers[0].volumeMounts = [];
  const h = harness([direct, subpath, unrelated, wrongRoot, unmounted]);
  assert.deepEqual(Array.from(h.context.canglingJarDeploymentsFor(jar), item => item.metadata.name), ['renamed-base', 'second-base']);
  await h.context.restartCanglingJarDeployments(jar);
  assert.equal(h.prompts.length, 1);
  assert.match(h.prompts[0], /custom\/renamed-base/);
  assert.deepEqual(h.calls.filter(call => call.method === 'POST').map(call => call.url), [
    '/api/k3s/workloads/deployments/custom/renamed-base/restart',
    '/api/k3s/workloads/deployments/custom/second-base/restart'
  ]);
  assert.equal(h.messages.at(-1).kind, 'ok');
  assert.equal(h.context.canglingJarRestarts.size, 0);
  const stale = harness([unrelated]); stale.context.canglingJarDeployments = [direct];
  await stale.context.restartCanglingJarDeployments(jar);
  assert.equal(stale.calls.filter(call => call.method === 'POST').length, 0);
  const unavailable = harness([direct]); unavailable.context.readError = true;
  await unavailable.context.restartCanglingJarDeployments(jar);
  assert.equal(unavailable.calls.filter(call => call.method === 'POST').length, 0);
  assert.match(unavailable.messages.at(-1).message, /Kubernetes unavailable/);
  const cancelled = harness([direct]); cancelled.context.confirm = false;
  await cancelled.context.restartCanglingJarDeployments(jar);
  assert.equal(cancelled.calls.filter(call => call.method === 'POST').length, 0);
  for (const spec of [{ replicas: 0 }, { paused: true }]) {
    const blocked = harness([deployment('blocked', jar, spec)]);
    await blocked.context.restartCanglingJarDeployments(jar);
    assert.equal(blocked.calls.filter(call => call.method === 'POST').length, 0);
    assert.equal(blocked.messages.at(-1).kind, 'err');
  }
  const active = harness([direct]); active.context.progress.hidden = false;
  await active.context.restartCanglingJarDeployments(jar);
  assert.equal(active.calls.filter(call => call.method === 'POST').length, 0);
  const failedProgress = harness([direct]); failedProgress.context.progress.hidden = false;
  failedProgress.context.progress.classList.contains = name => name === 'phase-error';
  await failedProgress.context.restartCanglingJarDeployments(jar);
  assert.equal(failedProgress.calls.filter(call => call.method === 'POST').length, 1);
  const unknown = harness([unrelated]);
  await unknown.context.restartCanglingJarDeployments(jar);
  assert.equal(unknown.prompts.length, 0);
  const partial = harness([direct, subpath]); partial.context.fail = '/second-base/';
  await partial.context.restartCanglingJarDeployments(jar);
  assert.equal(partial.messages.at(-1).kind, 'err');
  assert.match(partial.messages.at(-1).message, /1 个部署.*second-base/);
  const duplicate = harness([direct]);
  await Promise.all([duplicate.context.restartCanglingJarDeployments(jar), duplicate.context.restartCanglingJarDeployments(jar)]);
  assert.equal(duplicate.calls.filter(call => call.method === 'POST').length, 1);
  const original = harness([direct]); original.context.confirm = false;
  await original.context.restartCanglingDeployment(direct);
  assert.equal(original.prompts.length, 1);
  assert.equal(original.calls.length, 0);
  assert.equal(original.context.canglingDeploymentRestarts.size, 0);
  console.log('JAR restart UI checks passed: mount matching, confirmation, guarded states, partial failure, duplicate clicks, existing deployment action; inline scripts parse.');
})().catch(error => { console.error(error); process.exitCode = 1; });
