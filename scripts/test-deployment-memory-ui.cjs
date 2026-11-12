const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const html = fs.readFileSync(path.join(__dirname, '../src/assets/index.html'), 'utf8');
for (const match of html.matchAll(/<script(?:\s[^>]*)?>([\s\S]*?)<\/script>/g)) new vm.Script(match[1]);
const applications = [
  { id: 'cangling-map', title: '地图服务', group: 'single', workload: 'Deployment', replicas: 1, node: 'n01', ports: [3000], image: 'cangling-map:0.1.25' },
  { id: 'cis-map', title: '影像服务', group: 'multi', workload: 'Deployment', replicas: 2, node: 'n01', ports: [8079], image: 'gdal-base:test', memoryLimitGiB: 16 },
  { id: 'cangling-nginx', title: '系统入口', group: 'entry', workload: 'DaemonSet', replicas: 0, node: '', ports: [80], image: 'nginx:test', memoryLimitGiB: 2 }
];
const context = vm.createContext({ state: { canglingConfig: { applications } }, esc: String });
const start = html.indexOf('    function canglingAppRows(');
const end = html.indexOf('    function canglingSelectedNodes(', start);
vm.runInContext(html.slice(start, end), context);
for (const group of ['single', 'multi', 'entry']) {
  const row = context.canglingAppRows(group);
  assert.match(row, /data-cangling-app-field="memoryLimitGiB" type="number"/);
  assert.match(row, /aria-label="[^\"]+内存限制（G）"/);
}
assert.match(context.canglingAppRows('single'), /memoryLimitGiB"[^>]+value="8"/);
assert.match(context.canglingAppRows('multi'), /memoryLimitGiB"[^>]+value="16"/);
assert.match(context.canglingAppRows('entry'), /memoryLimitGiB"[^>]+value="2"/);
assert.doesNotMatch(context.canglingAppRows('single'), /data-cangling-app-field="replicas"/);
for (const value of ['1', '8', '16', '1024']) assert.equal(context.readCanglingMemoryLimit(value, '测试应用'), Number(value));
for (const value of ['', '0', '-1', '1.5', '1025', 'NaN', 'Infinity']) {
  assert.throws(() => context.readCanglingMemoryLimit(value, '测试应用'), /测试应用.*1–1024 G/);
}
console.log('Deployment UI checks passed: all workload groups, legacy default 8 G, custom limits, singleton row and invalid input; inline scripts parse.');
