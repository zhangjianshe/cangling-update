const assert=require('node:assert/strict'),fs=require('node:fs'),vm=require('node:vm');
const html=fs.readFileSync(require('node:path').join(__dirname,'../src/assets/index.html'),'utf8');
const elements=new Map();
for(const id of ['jar-run-progress','jar-run-bar','jar-run-percent','jar-run-message','jar-upgrade-log','btn-jar-run'])elements.set('#'+id,{hidden:true,textContent:'',disabled:false,removeAttribute(name){delete this[name];}});
let request,tick,flashes=[];
const context=vm.createContext({$:id=>elements.get(id),Number,Math,encodeURIComponent,jarUpgradePoll:{timer:null,jobId:null},api:()=>new Promise((resolve,reject)=>request={resolve,reject}),setTimeout(fn){tick=fn;return 1;},clearInterval(){tick=null;},flash:(kind,text)=>flashes.push([kind,text])});
let a=html.indexOf('    function setJarRunProgress('),b=html.indexOf('    async function runJarUpgrade()',a);vm.runInContext(html.slice(a,b),context);
a=html.indexOf('    function watchJarUpgrade(');b=html.indexOf('    function imageDirectoryQuery()',a);vm.runInContext(html.slice(a,b),context);
const get=id=>elements.get('#'+id),flush=()=>new Promise(resolve=>setImmediate(resolve));
(async()=>{
 context.watchJarUpgrade('first');assert.equal(get('jar-run-progress').hidden,false);assert.equal(get('jar-run-bar').value,undefined);
 request.resolve({current:1,total:2,percent:50,message:'node/app',done:false});await flush();assert.equal(get('jar-run-bar').value,50);assert.match(get('jar-run-percent').textContent,/1\/2/);assert.equal(get('jar-run-message').textContent,'node/app');
 tick();request.resolve({done:true,error:null,message:'success',current:2,total:2,percent:100});await flush();assert.equal(get('jar-run-progress').hidden,true);assert.equal(context.jarUpgradePoll.jobId,null);
 context.watchJarUpgrade('failed');request.resolve({done:true,error:'restart failed',total:2,current:1,percent:50});await flush();assert.equal(get('jar-run-progress').hidden,true);assert.deepEqual(flashes.at(-1),['err','restart failed']);
 context.watchJarUpgrade('network');request.reject(Error('offline'));await flush();assert.equal(get('jar-run-progress').hidden,true);assert.equal(get('btn-jar-run').disabled,false);
 context.watchJarUpgrade('old');const old=request;context.stopJarUpgradePoll();context.watchJarUpgrade('new');old.resolve({done:true,message:'stale'});await flush();assert.equal(get('jar-run-progress').hidden,false);assert.equal(context.jarUpgradePoll.jobId,'new');
 context.stopJarUpgradePoll();request.resolve({done:false,percent:75,total:4,current:3,message:'late'});await flush();assert.equal(get('jar-run-progress').hidden,true);
 console.log('JAR progress UI: immediate pending indicator, actual counts, completion/failure/network cleanup and stale response guards passed.');
})().catch(e=>{console.error(e);process.exitCode=1;});
