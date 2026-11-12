const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const html = fs.readFileSync(path.join(__dirname, '../src/assets/index.html'), 'utf8');
for (const match of html.matchAll(/<script(?:\s[^>]*)?>([\s\S]*?)<\/script>/g)) new vm.Script(match[1]);
const context = vm.createContext({});
vm.runInContext(html.split('\n').find(line => line.includes('const esc =')), context);
const first = html.indexOf('    function trafficUpstreamData(');
const last = html.indexOf('    function trafficDistributionSvg(', first);
vm.runInContext(html.slice(first, last), context);
const targets = [{ group: 'api_pool', address: '10.0.0.150:8078' }, { group: 'api_pool', address: '10.0.0.122:8078' }, { group: 'other_pool', address: '10.0.0.150:8078' }];
const rows = [{ name: '10.0.0.149:8078, 10.0.0.150:8078', requests: 5 }, { name: '10.0.0.150:8078', requests: 2 }, { name: '[fd00::2]:3000', requests: 3 }, { name: '-', requests: 10 }];
const data = context.trafficUpstreamData(rows, targets);
assert.equal(data.length, 3);
assert.equal(data.find(item => item.address === '10.0.0.150:8078').requests, 7);
assert.equal(data.find(item => item.address === '10.0.0.150:8078').group, 'api_pool / other_pool');
assert.equal(data.find(item => item.address === '10.0.0.122:8078').requests, 0);
assert.equal(data.reduce((sum, item) => sum + item.requests, 0), 10);
const svg = context.trafficUpstreamSvg(rows, targets);
assert.match(svg, /10\.0\.0\.150:8078/);
assert.match(svg, /10\.0\.0\.122:8078/);
assert.match(svg, /\[fd00::2\]:3000/);
assert.doesNotMatch(svg, /NP4 Base|GeoServer/);
assert.match(context.trafficUpstreamSvg([], []), /暂无后端请求记录/);
assert.doesNotMatch(context.trafficUpstreamSvg([{ name: '<script>alert(1)</script>', requests: 1 }], []), /<script>/);
assert.equal(context.trafficUpstreamData([{ name: 'node.internal:9000', requests: 4 }], [])[0].address, 'node.internal:9000');
const nav = html.slice(html.indexOf('<nav class="top-nav"'), html.indexOf('</nav>', html.indexOf('<nav class="top-nav"')));
const ids = [...nav.matchAll(/id="(btn-[^"]+)"/g)].map(match => match[1]);
assert.equal(ids[0], 'btn-cluster');
assert.deepEqual(ids.slice(-2), ['btn-projects', 'btn-jar-upgrade']);
const views = [];
const routeContext = { location: { hash: '' }, dbAdmin: {}, setMetaPanelOpen() {} };
for (const name of ['stopComposePoll', 'stopClusterPoll', 'stopK3sPoll', 'stopCanglingRuntimePoll', 'stopImageMonitorPoll', 'stopJarUpgradePoll', 'comboClose', 'closeDialog', 'syncSectionChrome']) routeContext[name] = () => {};
for (const name of ['renderCluster', 'renderHome', 'renderNew', 'renderK3s', 'renderCanglingApps', 'renderImages', 'renderCertificates', 'renderInstallResources', 'renderJarUpgrade', 'renderStorage', 'renderTraffic', 'openProject', 'renderK3sDatabase']) routeContext[name] = (...args) => views.push([name, ...args]);
vm.createContext(routeContext);
for (const [begin, end] of [['    function sectionFromHash(', '    function syncSectionChrome('], ['    function route()', '    function stopClusterPoll(']]) {
 const start = html.indexOf(begin); vm.runInContext(html.slice(start, html.indexOf(end, start)), routeContext);
}
for (const [hash, view, section] of [['', 'renderCluster', 'cluster'], ['#/', 'renderCluster', 'cluster'], ['#/cluster', 'renderCluster', 'cluster'], ['#/projects', 'renderHome', 'projects'], ['#/new', 'renderNew', 'projects'], ['#/p/old-project', 'openProject', 'projects'], ['#/jar-upgrade', 'renderJarUpgrade', 'jar-upgrade']]) {
 routeContext.location.hash = hash; routeContext.route();
 assert.equal(views.at(-1)[0], view, hash);
 assert.equal(routeContext.sectionFromHash(hash), section, hash);
}
console.log('Traffic/navigation UI checks passed: dynamic IPv4/IPv6/hostnames, configured zeroes, retries without double counting, safe labels, host home, menu order and legacy project/JAR routes.');
