const fs=require('node:fs'),vm=require('node:vm'),assert=require('node:assert/strict');
const source=fs.readFileSync('src/assets/index.html','utf8');const fn=source.slice(source.indexOf('async function updateInstallResources()'),source.indexOf('async function loadInstallResources()'));
(async()=>{
 const button={disabled:false,textContent:''},messages=[];let refreshed=false;
 const context=vm.createContext({$:()=>button,confirmDialog:async()=>true,flash:(kind,message)=>messages.push([kind,message]),api:async(path,options)=>{assert.equal(path,'/api/install-resources/refresh');assert.equal(options.method,'POST');return {totalFiles:3,downloaded:1,skipped:2,resumed:1};},installResources:{path:'old'},resetInstallResourcePreview:()=>{},loadInstallResources:async()=>{refreshed=true;}});
 vm.runInContext(fn,context);await context.updateInstallResources();assert.equal(refreshed,true);assert.equal(button.disabled,false);assert(messages.some(([kind,msg])=>kind==='ok' && msg.includes('3 文件') && msg.includes('续传 1')));assert(!messages.some(([,msg])=>msg.includes('undefined')));
 console.log('Resource UI displays camelCase drive synchronization counts');
})().catch(e=>{console.error(e);process.exitCode=1;});
