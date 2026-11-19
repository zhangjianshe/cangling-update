// Exercise the real tunnel public-key UI handlers against a fake API, never live authorizations.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const html = fs.readFileSync(require('node:path').join(__dirname, '../src/assets/index.html'), 'utf8');
for (const m of html.matchAll(/<script(?:\s[^>]*)?>([\s\S]*?)<\/script>/g)) new vm.Script(m[1]);
const source = html.slice(html.indexOf('    const tunnelKeyState ='), html.indexOf('    async function loadCertificates()'));
const nodes = new Map(), calls = [];
function element(id) {
  const value = {value:'', disabled:false, hidden:false, classList:{add() {}}, dataset:{}, reportValidity() {return true;}, focus() {},
    querySelectorAll(selector) {
      const attr = selector.slice(1,-1);
      return [...this.innerHTML.matchAll(new RegExp(attr+'="(\\d+)"','g'))].map(m => ({disabled:false,dataset:{[attr === 'data-tunnel-grant' ? 'tunnelGrant' : 'tunnelRevoke']:m[1]}}));
    }
  };
  let text = '';
  Object.defineProperty(value, 'innerHTML', { get() {return text;}, set(v) {
    text = v;
    for (const match of v.matchAll(/id="([^"]+)"/g)) nodes.set('#'+match[1], element(match[1]));
  }});
  return value;
}
['tunnel-key-content','dialog','dialog-modal','dialog-title','dialog-body','dialog-extra'].forEach(id => nodes.set('#'+id,element(id)));
let authorized = [], fail = false, confirm = true;
const key = {name:'Windows Keeper',algorithm:'ssh-ed25519',fingerprint:'SHA256:test-key',publicKey:'ssh-ed25519 TEST Windows'};
const context = vm.createContext({
  $(selector) { return nodes.get(selector); },
  esc(value) { return String(value).replaceAll('<','&lt;'); },
  closeDialog() { nodes.get('#dialog').hidden = true; nodes.delete('#tunnel-account-form'); nodes.delete('#tunnel-key-form'); },
  setDialogAction(label, handler) { context.action = handler; },
  async confirmDialog() { return confirm; },
  async api(url, options={}) {
    calls.push({url,method:options.method || 'GET',body:options.body && JSON.parse(options.body)});
    if (fail) throw new Error('test network error');
    if (url.endsWith('/account')) return {keys:[key]};
    if (options.method === 'POST') { authorized = [key]; return key; }
    if (options.method === 'DELETE') { authorized=[]; return {message:'已撤销公钥授权'}; }
    return {keys:authorized,enabled:true,path:'/config/tunnel_authorized_keys'};
  }
});
vm.runInContext(source,context);
(async () => {
  await context.loadTunnelKeys();
  assert.match(nodes.get('#tunnel-key-content').innerHTML,/尚未授权公钥/);
  assert.match(nodes.get('#tunnel-key-content').innerHTML,/主机 root SSH 授权文件/);
  assert.match(nodes.get('#tunnel-key-content').innerHTML,/添加和撤销会同步维护/);
  assert.doesNotMatch(nodes.get('#tunnel-key-content').innerHTML,/tunnel-sync-ssh|同步 SSH 授权/);
  nodes.get('#tunnel-account-open').onclick();
  nodes.get('#tunnel-account-url').value = 'https://account.example';
  nodes.get('#tunnel-account-user').value = 'test';
  const password = nodes.get('#tunnel-account-password'); password.value = 'do-not-store';
  await context.action();
  assert.equal(password.value,'');
  const content = nodes.get('#tunnel-key-content');
  assert.match(content.innerHTML,/Windows Keeper/);
  assert.match(content.innerHTML,/授权访问/);
  assert.doesNotMatch(content.innerHTML,/do-not-store/);
  // Fetch the actual bound button objects on the next render.
  let grantButtons, revokeButtons;
  const original = content.querySelectorAll;
  content.querySelectorAll = function(selector) {
    const result = original.call(this,selector);
    if (selector.includes('grant')) grantButtons=result; else revokeButtons=result;
    return result;
  };
  await context.loadTunnelKeys();
  await grantButtons[0].onclick();
  assert.equal(authorized.length,1);
  assert.match(content.innerHTML,/已授权<\/button>/);
  confirm=false; const before=calls.length; await revokeButtons[0].onclick();
  assert.equal(calls.length,before);
  confirm=true; await revokeButtons[0].onclick();
  assert.equal(authorized.length,0);
  assert.match(content.innerHTML,/已撤销公钥授权/);
  context.openTunnelAccount();
  nodes.get('#tunnel-account-url').value='https://account.example';
  const failedPassword=nodes.get('#tunnel-account-password'); failedPassword.value='secret';
  fail=true; await context.action();
  assert.equal(failedPassword.value,'');
  assert.equal(nodes.get('#tunnel-account-error').textContent,'test network error');
  assert.equal(nodes.get('#dialog-extra').disabled,false);
  console.log('Tunnel key UI checks passed: account selection, grant, duplicate state, automatic SSH synchronization, revoke confirmation, inline errors, password clearing; all inline scripts parse.');
})().catch(error => {console.error(error);process.exitCode=1;});
