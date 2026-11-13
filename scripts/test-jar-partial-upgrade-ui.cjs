const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const html = fs.readFileSync(require('node:path').join(__dirname, '../src/assets/index.html'), 'utf8');
for (const match of html.matchAll(/<script(?:\s[^>]*)?>([\s\S]*?)<\/script>/g)) new vm.Script(match[1]);
const elements = new Map();
const $ = key => { if (!elements.has(key)) elements.set(key, { innerHTML: '', disabled: false }); return elements.get(key); };
let status, confirmed = true, posts = 0, watched = null, confirmation = '';
const context = vm.createContext({
  $, Number, state: {}, fmtBytes: String, fmtDateTime: String,
  esc: value => String(value == null ? '' : value).replace(/[&<>"']/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c])),
  renderJarUpgrade() {}, checkJarEnvironment() {}, uploadJarFiles() {}, openJarStorageRepair() {}, flash() {},
  api: async (url, options) => { if (options && options.method === 'POST') { posts++; return { id: 'job' }; } return status; },
  confirmDialog: async message => { confirmation = message; return confirmed; },
  watchJarUpgrade: id => { watched = id; }
});
let start = html.indexOf('    function drawJarUpgrade(');
vm.runInContext(html.slice(start, html.indexOf('    async function checkJarEnvironment(', start)), context);
start = html.indexOf('    function jarUpgradeConfirmation(');
vm.runInContext(html.slice(start, html.indexOf('    function watchJarUpgrade(', start)), context);
const partial = { ready: true, upgrading: false, eligible_targets: 1, incoming_dir: '/test', script: '/test/upgrade.sh', files: [{ name: 'cis-server-1.0.0.jar', present: true }], nodes: [{ name: 'business', ok: true, targets: [{ jar: 'cis-server-1.0.0.jar', container: 'app' }] }, { name: 'GPU <script>', ok: false, message: 'SSH failed <img>', targets: [] }] };
context.drawJarUpgrade(partial);
let rendered = $('#main').innerHTML;
assert.match(rendered, /id="btn-jar-run" >执行升级/);
assert.match(rendered, /跳过（检查失败）/);
assert.match(rendered, /GPU &lt;script&gt;/);
assert.doesNotMatch(rendered, /GPU <script>/);
assert.match(rendered, /本批次可升级目标：1/);
context.drawJarUpgrade({ ...partial, ready: false, eligible_targets: 0 });
assert.match($('#main').innerHTML, /id="btn-jar-run" disabled/);
context.drawJarUpgrade({ ...partial, upgrading: true });
assert.match($('#main').innerHTML, /id="btn-jar-run" disabled/);
(async () => {
  status = partial; await context.runJarUpgrade();
  assert.equal(posts, 1); assert.equal(watched, 'job'); assert.match(confirmation, /1 个匹配目标/); assert.match(confirmation, /GPU <script>/); assert.match(confirmation, /将跳过/);
  status = { ...partial, ready: false }; await context.runJarUpgrade(); assert.equal(posts, 1);
  status = { ...partial, upgrading: true }; await context.runJarUpgrade(); assert.equal(posts, 1);
  status = partial; confirmed = false; await context.runJarUpgrade(); assert.equal(posts, 1);
  console.log('JAR upgrade UI: failed GPU excluded visibly, matching targets remain enabled, names escaped, confirmation lists exclusions, stale/unready/busy/cancelled requests do not execute.');
})().catch(e => { console.error(e); process.exitCode = 1; });
