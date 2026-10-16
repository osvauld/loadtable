#!/usr/bin/env python3
"""Real native CLI/daemon/worker acceptance. No outer admission wrapper.
Own cached-disk fixtures only. No device operations, commits or cache cleanup.
"""
import json,os,subprocess,tempfile,time
from pathlib import Path
from wait_budget import WaitBudget

def main():
 import argparse
 ap=argparse.ArgumentParser();ap.add_argument('binary',type=Path);a=ap.parse_args();binary=a.binary.resolve()
 root=Path(tempfile.mkdtemp(prefix='native-commands-',dir=Path.home()/'build-cache/tm-fixtures'))
 state=root/'state';source=root/'source';(source/'src').mkdir(parents=True)
 (source/'Cargo.toml').write_text('[package]\nname="native-acceptance"\nversion="0.1.0"\nedition="2024"\n[workspace]\n')
 (source/'Cargo.lock').write_text('version = 4\n[[package]]\nname = "native-acceptance"\nversion = "0.1.0"\n')
 (source/'src/lib.rs').write_text('#[test] fn synthetic_passes(){assert_eq!(2+2,4);}\n')
 env=dict(os.environ,TASK_MANAGER_STATE_DIR=str(state),PYTHONDONTWRITEBYTECODE='1')
 log=(root/'daemon.log').open('w');daemon=subprocess.Popen([str(binary),'daemon'],env=env,stdout=log,stderr=subprocess.STDOUT)
 jobs=[]
 def cli(*args,expect_error=False):
  argv=list(map(str,args))
  if argv and argv[0]=='logs':argv.append('--json')
  p=subprocess.run([str(binary),*argv],env=env,cwd=source,capture_output=True,text=True,timeout=15)
  data=json.loads(p.stdout)
  if expect_error:assert p.returncode and data.get('error'),data
  else:assert p.returncode==0 and not data.get('error'),(p.stderr,data)
  return data
 def enqueue(*argv,run=True):
  args=['enqueue','--who','synthetic-agent-a']+(['--run'] if run else [])+['--',*argv]
  result=cli(*args);id=result['submitted_id'];assert any(j['id']==id for j in result['jobs']);jobs.append(id);return id
 def wait(id,terminal=True):
  budget=WaitBudget(300,0)
  while True:
   # Each query is an independent Agent-B client, not the submitter's memory.
   j=next(j for j in cli('list')['jobs'] if j['id']==id)
   assert not budget.update(j['phase']),j
   if (terminal and j['phase'] in ('passed','failed','cancelled','interrupted')) or (not terminal and j['phase']=='running'):return j
   time.sleep(.1)
 try:
  deadline=time.monotonic()+10
  while not (state/'taskd.sock').exists():assert daemon.poll() is None and time.monotonic()<deadline;time.sleep(.05)
  first=enqueue('/bin/sh','-c','test "$CARGO_BUILD_JOBS" = 2 || exit 91; printf agent-a-start; sleep 2; printf agent-a-done')
  second=enqueue('/bin/sh','-c','printf second-start; sleep 2; printf second-done')
  cli('approve',first,expect_error=True)
  for id in (first,second):cli('approve-command',id)
  cli('start-worker')
  wait(first,False);wait(second,False)
  queried=cli('list');assert sum(j['phase']=='running' for j in queried['jobs'])==2,queried
  admission=json.loads(subprocess.check_output([str(binary),'admission'],env=env,text=True))
  assert admission['backend']=='rust-flock-v1'
  assert sum(r['busy'] and (r['holder'] or {}).get('who','').startswith('synthetic-agent-a:') for r in admission['resources'])>=2
  for id in (first,second):assert wait(id)['phase']=='passed'
  expected_log=cli('logs',first)['log']['text']
  assert 'agent-a-done' in expected_log
  plain=subprocess.run([str(binary),'logs',str(first)],env=env,capture_output=True,text=True,timeout=15)
  assert plain.returncode==0 and plain.stdout==expected_log,(plain.stdout,plain.stderr)
  rejected=subprocess.run([str(binary),'enqueue','--','env','CARGO_BUILD_JOBS=16','cargo','test'],env=env,cwd=source,capture_output=True,text=True,timeout=15)
  assert rejected.returncode and 'Cargo jobs must be 1 or 2' in rejected.stderr
  print('PASS independent Agent-B queries owners/running jobs/logs/results; two native run slots overlap',flush=True)
  cargo=enqueue('cargo','test','--locked','--offline','--lib',run=False);cli('approve-command',cargo);cli('start-worker')
  result=wait(cargo);assert result['phase']=='passed',result
  assert [s['stage'] for s in result['execution']['stages']]==['build','run'];assert result['execution']['snapshot'] is None
  assert 'test result: ok.' in cli('logs',cargo)['log']['text']
  print('PASS real Cargo compile/run split through Rust admission, durable success without snapshot claim',flush=True)
  failed=enqueue('/bin/sh','-c','printf expected-failure; exit 7');cli('approve-command',failed);cli('start-worker')
  result=wait(failed);assert result['phase']=='failed' and result['execution']['stages'][-1]['exit_code']==7,result
  cancelled=enqueue('/bin/sh','-c','printf cancel-start; sleep 30');cli('approve-command',cancelled);cli('start-worker');wait(cancelled,False);cli('cancel',cancelled)
  assert wait(cancelled)['phase']=='cancelled'
  result=cli('list');assert len(result['jobs'])==5 and all(j['phase'] in ('passed','failed','cancelled') for j in result['jobs'])
  assert all(not any(e['configuration']==j['spec']['configuration'] for e in result['estimates']) for j in result['jobs'] if j['phase']!='passed')
  print('PASS failed/cancelled commands retain logs/status and do not learn success',flush=True)
  run=subprocess.run([str(binary),'run','--who','synthetic-agent-c','--run','--','/bin/sh','-c','printf run-command-proof; exit 9'],env=env,cwd=source,capture_output=True,text=True,timeout=60)
  assert run.returncode==9 and 'run-command-proof' in run.stdout,(run.returncode,run.stdout,run.stderr)
  assert run.stdout.splitlines()[-1]=='job 6 failed (exit 9) owner=synthetic-agent-c',run.stdout
  success=subprocess.run([str(binary),'run','--who','synthetic-agent-c','--run','--','true'],env=env,cwd=source,capture_output=True,text=True,timeout=60)
  assert success.returncode==0 and success.stdout.splitlines()[-1]=='job 7 passed (exit 0) owner=synthetic-agent-c',(success.stdout,success.stderr)
  result=cli('list');assert len(result['jobs'])==7 and result['jobs'][-1]['phase']=='passed'
  print('PASS one-command tasks run explicitly submits/approves/starts and preserves child exit code',flush=True)
  time.sleep(3)
  summary={'status':'passed','fixture':str(root),'state':str(state),'jobs':result['jobs'],'estimates':result['estimates'],'no_device_operations':True}
  (root/'summary.json').write_text(json.dumps(summary,indent=2));print('SUMMARY',root/'summary.json',flush=True)
 finally:
  for id in jobs:
   try:
    j=next(j for j in cli('list')['jobs'] if j['id']==id)
    if j['phase'] not in ('passed','failed','cancelled','interrupted'):cli('cancel',id)
   except Exception:pass
  time.sleep(2);daemon.terminate();daemon.wait(timeout=10);log.close()
if __name__=='__main__':main()
