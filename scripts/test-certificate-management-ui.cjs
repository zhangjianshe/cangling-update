// Render the real certificate UI against distinct root and issuing CA metadata.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const html = fs.readFileSync(require('node:path').join(__dirname, '../src/assets/index.html'), 'utf8');
const subject = html.slice(html.indexOf('    function certificateSubjectValue('), html.indexOf('    function renderClusterLayout('));
const source = html.slice(html.indexOf('    async function loadCertificates()'), html.indexOf('    async function downloadRootCertificate()'));
const content = {innerHTML:''};
let metadata = {
  root_initialized:true, root_subject:'CN = Cangling Root CA, O = Cangling, OU = Root Authority',
  root_expires:'2046-11-14', root_fingerprint:'ROOT-FINGERPRINT', root_certificate_path:'/certs/trust-root.crt.pem',
  issuer_subject:'CN = Cangling Update Ca Root, O = Cangling, OU = Update Website Certificates',
  issuer_expires:'2036-11-16', issuer_fingerprint:'ISSUER-FINGERPRINT', issuer_certificate_path:'/certs/ca.crt.pem',
  embedded_chain_active:true, suggested_ip:'10.0.0.150', loadbalance_issued:false
};
const context = vm.createContext({
  $(selector) {return selector === '#certificate-content' ? content : null;},
  location:{hash:'#/certificates'}, api:async()=>metadata, esc:v=>String(v??''), fmtDateTime:v=>v,
  appPath:v=>v, activateCertificateTab(){},
});
vm.runInContext(subject+source,context);
(async()=>{
  await context.loadCertificates();
  assert.match(content.innerHTML, /class="root-certificate-name">Cangling Update Ca Root<\/div>/);
  assert.match(content.innerHTML, /class="root-certificate-validity">有效期至：2036-11-16/);
  assert.match(content.innerHTML, /class="root-certificate-fingerprint" title="ISSUER-FINGERPRINT">ISSUER-FINGERPRINT/);
  assert.match(content.innerHTML, /客户端信任的根 CA<\/label><div>Cangling Root CA/);
  assert.match(content.innerHTML, /SHA-256：ROOT-FINGERPRINT/);
  assert.match(content.innerHTML, /不是根 CA 私钥密码/);
  metadata = {...metadata, issuer_subject:undefined, issuer_expires:undefined, issuer_fingerprint:undefined, issuer_certificate_path:undefined};
  await context.loadCertificates();
  assert.match(content.innerHTML, /class="root-certificate-name">Cangling Root CA<\/div>/);
  assert.doesNotMatch(content.innerHTML,/undefined/);
  console.log('Certificate UI checks passed: issuing CA card, root trust information, distinct fingerprints, password hint, legacy fallback.');
})().catch(error=>{console.error(error);process.exitCode=1;});
