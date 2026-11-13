#!/usr/bin/env python3
"""Authenticated HTTP workflow with a fake K3s command and fake worker. Never alters a real cluster."""
import datetime
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import shutil
import socket
import sqlite3
import subprocess
import sys
import tempfile
import threading
import time
from urllib.request import Request, urlopen
from urllib.error import HTTPError
import uuid

RUNTIME_REPAIR = os.environ.get('ACCELERATOR_HTTP_RUNTIME') == '1'
ROOT = Path(__file__).resolve().parents[1]
BINARY = Path(sys.argv[1] if len(sys.argv)>1 else ROOT/'target/debug/cangling-update').resolve()
REPO = ROOT.parent/'cangling-repo'
FAKE = '''#!/usr/bin/env python3
import os,sys,json
from pathlib import Path
args=sys.argv[1:]
if args and args[0]=='kubectl':args=args[1:]
marker=Path(os.environ['ACCELERATOR_TEST_MARKER'])
scheduling=Path(os.environ['ACCELERATOR_TEST_SCHEDULING'])
state=json.loads(scheduling.read_text()) if scheduling.exists() else {'cordoned':False,'annotations':{}}
node={'metadata':{'name':'fake-gpu','labels':{},'resourceVersion':'123','annotations':state['annotations']},'spec':{'unschedulable':state['cordoned']},'status':{'addresses':[{'type':'InternalIP','address':'10.254.254.1'}],'nodeInfo':{'architecture':'amd64'},'conditions':[{'type':'Ready','status':'True'}],'capacity':{},'allocatable':{}}}
if marker.exists():node['status']['capacity']=node['status']['allocatable']={'nvidia.com/gpu':'1'}
if args[:2]==['get','nodes']:out={'items':[node]}
elif args[:2]==['get','node']:out=node
elif args[:2]==['get','runtimeclasses']:out={'items':[{'metadata':{'name':'nvidia'},'handler':'nvidia'}]}
elif args[:2]==['get','pods']:
 pods=[]
 if marker.exists():
  ds=json.loads(marker.read_text());pods=[{'metadata':{'name':ds['metadata']['name']+'-abc','namespace':'kube-system','uid':'new-pod','ownerReferences':[{'kind':'DaemonSet','name':ds['metadata']['name'],'controller':True}]},'spec':{'nodeName':'fake-gpu','containers':ds['spec']['template']['spec']['containers']},'status':{'phase':'Running','conditions':[{'type':'Ready','status':'True'}]}}]
 out={'items':pods}
elif args[:2]==['get','daemonset']:
 print('');sys.exit(0)
elif args[:2]==['patch','node']:
 ops=json.loads(args[args.index('-p')+1])
 if ops[0]['path']=='/metadata/resourceVersion':
  assert ops[0]['value']=='123' and not state['cordoned']
  assert ops[1]['value']['cangling.cn/accelerator-integration']
  state={'cordoned':True,'annotations':ops[1]['value']}
 else:
  assert state['cordoned'] and ops[0]['value']==state['annotations']['cangling.cn/accelerator-integration']
  state={'cordoned':False,'annotations':{}}
 scheduling.write_text(json.dumps(state));print('patched');sys.exit(0)
elif args and args[0]=='create':
 ds=json.loads(Path(args[args.index('-f')+1]).read_text());assert ds['kind']=='DaemonSet'
 fields=ds['spec']['template']['spec']['affinity']['nodeAffinity']['requiredDuringSchedulingIgnoredDuringExecution']['nodeSelectorTerms'][0]['matchFields']
 assert fields==[{'key':'metadata.name','operator':'In','values':['fake-gpu']}]
 assert ds['spec']['template']['spec']['containers'][0]['imagePullPolicy']=='Never'
 if '--dry-run=server' not in args:marker.write_text(json.dumps(ds))
 print('created');sys.exit(0)
else:
 print('unexpected mock command',args,file=sys.stderr);sys.exit(2)
print(json.dumps(out))
'''

def main():
    token=uuid.uuid4().hex
    worker_job=str(uuid.uuid4())
    submissions=[]
    class Worker(BaseHTTPRequestHandler):
        def log_message(self,*args):pass
        def reply(self,v,status=200):
            data=json.dumps(v).encode();self.send_response(status);self.send_header('Content-Type','application/json');self.end_headers();self.wfile.write(data)
        def do_GET(self):
            if self.headers.get('x-cluster-token')!=token:return self.reply({},401)
            if self.path.endswith('/info'):return self.reply({'apiVersion':1,'ips':['10.254.254.1']})
            if self.path.endswith(worker_job):return self.reply({'done':True,'error':None,'message':'mock prepared'})
            self.reply({},404)
        def do_POST(self):
            if self.headers.get('x-cluster-token')!=token:return self.reply({},401)
            body=json.loads(self.rfile.read(int(self.headers['Content-Length'])))
            assert body['package_id']=='nvidia-amd64' and body['node_ip']=='10.254.254.1'
            assert body['package_stamp']==hashlib.sha256((REPO/'accelerators/packages/nvidia-amd64/SHA256SUMS').read_bytes()).hexdigest()
            assert body['runtime']==RUNTIME_REPAIR and body['import_image']
            submissions.append(body);self.reply({'id':worker_job})
    worker=ThreadingHTTPServer(('127.0.0.1',0),Worker)
    threading.Thread(target=worker.serve_forever,daemon=True).start()
    with tempfile.TemporaryDirectory(prefix='accelerator-http-') as name:
        temp=Path(name);exe=temp/'cangling-update';shutil.copy2(BINARY,exe)
        (temp/'cangling-repo').symlink_to(REPO,target_is_directory=True)
        bindir=temp/'bin';bindir.mkdir();(bindir/'k3s').write_text(FAKE);(bindir/'k3s').chmod(0o755)
        with socket.socket() as sock:sock.bind(('127.0.0.1',0));port=sock.getsockname()[1]
        env=dict(os.environ,PATH=str(bindir)+':'+os.environ['PATH'],CANGLING_CLUSTER_TOKEN=token,ACCELERATOR_TEST_MARKER=str(temp/'created.json'),ACCELERATOR_TEST_SCHEDULING=str(temp/'scheduling.json'))
        data=temp/'config';log=(temp/'server.log').open('w')
        process=subprocess.Popen([str(exe),'--data-dir',str(data),'--bind','127.0.0.1','--port',str(port)],env=env,stdout=log,stderr=log)
        base='http://127.0.0.1:'+str(port)
        def request(path,body=None,headers=None):
            raw=None if body is None else json.dumps(body).encode()
            req=Request(base+path,data=raw,headers={'Content-Type':'application/json',**(headers or {})})
            try:
                with urlopen(req,timeout=45) as res:return res.status,res.read()
            except HTTPError as e:return e.code,e.read()
        try:
            for _ in range(100):
                try:
                    if request('/api/auth/status')[0]==200:break
                except OSError:pass
                time.sleep(.1)
            else:raise AssertionError('test server failed to start')
            issued=subprocess.check_output([str(exe),'--data-dir',str(data),'issue-session','--username','admin'],text=True,env=env)
            session=issued.split('token=')[1].strip();auth={'Cookie':'cangling_session='+session}
            for path in ['/api/k3s/accelerator-integration/catalog','/accelerators/supported']:
                assert request(path)[0]==401,path
                assert request(path,headers=auth)[0]==200,path
            assert request('/api/cluster/accelerators/info')[0]==401
            assert request('/api/cluster/accelerators/info',headers=auth)[0]==401
            machine={'x-cluster-token':token}
            assert request('/api/cluster/accelerators/info',headers=machine)[0]==200
            assert request('/api/cluster/accelerators/jobs/unrelated',headers=machine)[0]==404
            invalid={'package_id':'untrusted','node_ip':'10.254.254.1','runtime':False,'import_image':True,'package_stamp':'0'*64}
            assert request('/api/cluster/accelerators/prepare',invalid,machine)[0]==400
            now=datetime.datetime.now(datetime.timezone.utc).isoformat()
            host={'observedAt':now,'ips':['10.254.254.1'],'platform':'ubuntu-x86-24.04-deb','devices':[{'vendor':'NVIDIA','name':'RTX 5090','driver':'nvidia'}],'tools':[{'vendor':'NVIDIA','result':{'status':'ok'}}],'runtimes':[{'handler':'nvidia','executable':True,'versionProbe':{'status':'ok'}}]}
            if RUNTIME_REPAIR:host['runtimes']=[]
            with sqlite3.connect(data/'cangling.db') as db:
                db.execute('INSERT INTO cluster_nodes (id,name,addr,role,info_json,registered_at,last_seen) VALUES (?,?,?,?,?,?,?)',('test-worker','fake-gpu','127.0.0.1:'+str(worker.server_port),'worker',json.dumps({'accelerator_diagnostics':host}),now,now))
            selection={'node_name':'fake-gpu','profile_id':'nvidia'}
            status,body=request('/api/k3s/accelerator-integration/plan',selection,auth);assert status==200,body
            plan=json.loads(body);assert not plan['blockers'],plan;assert len(plan['actions'])==(3 if RUNTIME_REPAIR else 2)
            repair=dict(selection,fingerprint='wrong',confirmed=True)
            assert request('/api/k3s/accelerator-integration/repair',repair,auth)[0]==409
            assert not submissions
            repair['fingerprint']=plan['fingerprint'];repair['confirmed']=False
            assert request('/api/k3s/accelerator-integration/repair',repair,auth)[0]==400
            repair['confirmed']=True
            status,body=request('/api/k3s/accelerator-integration/repair',repair,auth);assert status==200,body
            job=json.loads(body)
            for _ in range(40):
                status,body=request('/api/jobs/'+job['id'],headers=auth);assert status==200
                progress=json.loads(body)
                if progress['done']:break
                time.sleep(.5)
            assert progress['done'] and progress['error'] is None,progress
            assert len(submissions)==1
            if RUNTIME_REPAIR:assert json.loads((temp/'scheduling.json').read_text())=={'cordoned':False,'annotations':{}}
            status,body=request('/api/k3s/accelerator-integration/plan',selection,auth);assert status==200
            assert not json.loads(body)['actions'],body
            print('HTTP integration: session/token separation, guarded plan, worker dispatch, node-scoped create, registration verification and idempotent recheck passed (mock cluster; runtime repair='+str(RUNTIME_REPAIR)+').')
        finally:
            process.terminate();process.wait(timeout=15);log.close();worker.shutdown();worker.server_close()
if __name__=='__main__':main()
