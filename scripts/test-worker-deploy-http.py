#!/usr/bin/env python3
"""End-to-end worker deployment with fake SSH; no remote service is changed."""
import datetime,json,os,shutil,socket,sqlite3,subprocess,tempfile,time,uuid
from pathlib import Path
from urllib.request import Request,urlopen
from urllib.error import HTTPError
ROOT=Path(__file__).resolve().parents[1]
SSH=r'''#!/usr/bin/env python3
import datetime,json,os,shlex,sqlite3,sys,uuid
args=sys.argv[1:];target=args[-2];command=args[-1];body=sys.stdin.buffer.read()
if command!='sh -s':
 assert body[:4]==b'\x7fELF';sys.exit(0)
script=body.decode()
if 'CK_ARCH=' in script:
 print('CK_ARCH=x86_64\nCK_MACHINE=mock-remote-machine-'+target);sys.exit(0)
if 'CK_NODE_ID=' in script:
 assert 'hostnamectl' not in script and 'k3s-agent' not in script
 token=shlex.split(next(l for l in script.splitlines() if l.startswith('token=')).split('=',1)[1])[0]
 if target.endswith('10.90.0.2'):
  print('injected failure '+token+' '+os.environ.get('CANGLING_SSH_PASSWORD',''),file=sys.stderr);sys.exit(9)
 node=str(uuid.uuid5(uuid.NAMESPACE_DNS,target));now=datetime.datetime.now(datetime.timezone.utc).isoformat()
 with sqlite3.connect(os.environ['WORKER_TEST_DB']) as db:
  db.execute('INSERT INTO cluster_nodes (id,name,addr,role,version,info_json,registered_at,last_seen) VALUES (?,?,?,?,?,?,?,?)',(node,'mock-worker',target.split('@')[1]+':5400','worker',os.environ['WORKER_TEST_VERSION'],'{}',now,now))
 print('CK_NODE_ID='+node)
'''
def main():
 with tempfile.TemporaryDirectory(prefix='worker-deploy-http-') as tmp:
  temp=Path(tmp);exe=temp/'cangling-update';shutil.copy2(ROOT/'target/debug/cangling-update',exe);(temp/'cangling-repo').symlink_to(ROOT.parent/'cangling-repo',target_is_directory=True)
  bindir=temp/'bin';bindir.mkdir();(bindir/'ssh').write_text(SSH);(bindir/'ssh').chmod(0o755)
  with socket.socket() as s:s.bind(('127.0.0.1',0));port=s.getsockname()[1]
  with socket.socket(socket.AF_INET,socket.SOCK_DGRAM) as s:s.bind(('127.0.0.1',0));discovery=s.getsockname()[1]
  version=subprocess.check_output([str(exe),'version'],text=True).strip().lstrip('v');token=uuid.uuid4().hex;password='fixture-host-password'
  env=dict(os.environ,PATH=str(bindir)+':'+os.environ['PATH'],WORKER_TEST_DB=str(temp/'config/cangling.db'),WORKER_TEST_VERSION=version,CANGLING_CLUSTER_TOKEN=token)
  log=(temp/'server.log').open('w');process=subprocess.Popen([str(exe),'--data-dir',str(temp/'config'),'--bind','127.0.0.1','--port',str(port),'--role','master','--discovery-port',str(discovery)],env=env,stdout=log,stderr=log)
  def api(path,body=None,headers=None):
   req=Request('http://127.0.0.1:'+str(port)+path,data=None if body is None else json.dumps(body).encode(),headers={'Content-Type':'application/json',**(headers or {})})
   try:
    with urlopen(req,timeout=30) as r:return r.status,json.load(r)
   except HTTPError as e:return e.code,json.load(e)
  try:
   for _ in range(100):
    try:
     if api('/api/auth/status')[0]==200:break
    except OSError:pass
    time.sleep(.1)
   else:raise AssertionError('server startup failed')
   issued=subprocess.check_output([str(exe),'--data-dir',str(temp/'config'),'issue-session','--username','admin'],env=env,text=True);session=issued.split('token=')[1].strip();auth={'Cookie':'cangling_session='+session}
   assert api('/api/hosts/deploy-workers')[0]==401
   status,options=api('/api/hosts/deploy-workers',headers=auth);assert status==200 and options['worker_port']==5400
   selected=[]
   for i in [2,3]:
    status,host=api('/api/hosts',{'name':'test-'+str(i),'host':'10.90.0.'+str(i),'ssh_port':2200+i,'username':'root','password':password},auth);assert status==200,host;selected.append(host['id'])
   master='http://10.90.0.1:'+str(port)
   assert api('/api/hosts/deploy-workers',{'host_ids':['local'],'master_url':master},auth)[0]==400
   assert api('/api/hosts/deploy-workers',{'host_ids':selected,'master_url':'http://127.0.0.1:5400'},auth)[0]==400
   status,job=api('/api/hosts/deploy-workers',{'host_ids':selected,'master_url':master},auth);assert status==200,job
   for _ in range(80):
    with sqlite3.connect(temp/'config/cangling.db') as db:db.execute("UPDATE cluster_nodes SET last_seen=? WHERE role='worker'",(datetime.datetime.now(datetime.timezone.utc).isoformat(),))
    _,current=api('/api/jobs/'+job['id'],headers=auth)
    if current['done']:break
    time.sleep(.25)
   assert current['done'],current
   assert '成功 1，失败 1' in current['message'],current
   assert token not in json.dumps(current) and password not in json.dumps(current), 'credentials leaked'
   assert '[已隐藏]' in current['error'] and '10.90.0.2' in current['error'],current
   assert not list((temp/'config').glob('.ssh-askpass-*')),'temporary credential helper was left behind'
   print('Worker deployment HTTP: authentication, target validation, SSH transfer EOF, registration, partial failure continuation, secret redaction and askpass cleanup passed (mock SSH).')
  finally:process.terminate();process.wait(timeout=15);log.close()
if __name__=='__main__':main()
