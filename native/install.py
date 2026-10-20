#!/usr/bin/python3
"""Install a pinned native Rust proxy. Never change Claude defaults implicitly."""
import hashlib,json,os,pathlib,secrets,shutil,socket,sqlite3,subprocess,sys
src=pathlib.Path(__file__).parent
home=pathlib.Path.home();state=home/'.local/state/cliproxy-native';configs=home/'.config/cliproxy-native'
if len(sys.argv)<2:raise SystemExit('Supply the built cliproxy binary')
binary=pathlib.Path(sys.argv[1]).resolve()
if not binary.is_file():raise SystemExit('Supply the built cliproxy binary')
upgrade=(configs/'config.json').exists()
if upgrade:
 if '--upgrade' not in sys.argv:raise SystemExit('Existing installation; stop its launch agent, then use --upgrade')
 with socket.socket() as probe:
  if probe.connect_ex(('127.0.0.1',20148))==0:raise SystemExit('Stop the managed proxy before upgrading')
for p in [state,configs,state/'auth']:p.mkdir(parents=True,exist_ok=True,mode=0o700);p.chmod(0o700)
sources={f:src/f for f in ['credentials.cjs','control.cjs','cliproxy-native','claude','zed-claude']};sources['cliproxy']=binary
hashes={name:hashlib.sha256(p.read_bytes()).hexdigest() for name,p in sources.items()}
build=hashlib.sha256(json.dumps(hashes,sort_keys=True).encode()).hexdigest()[:12]
release=home/'.local/share/cliproxy-native/releases'/build;release.mkdir(parents=True,exist_ok=True,mode=0o700)
for f,p in sources.items():shutil.copy2(p,release/f)
(release/'manifest.json').write_text(json.dumps({'build':build,'hashes':hashes},indent=2))
for f in ['cliproxy-native','claude','cliproxy','zed-claude']:(release/f).chmod(0o755)
if upgrade:
 c=json.loads((configs/'config.json').read_text());database=pathlib.Path(c['database'])
 if database.suffix=='.sqlite':
  target=database.with_suffix('.json')
  if target.exists():raise SystemExit('Refusing ambiguous affinity migration: JSON already exists')
  with sqlite3.connect('file:'+str(database)+'?mode=ro',uri=True) as db:
   pins=dict(db.execute('SELECT session,account FROM pins'));cooldowns=dict(db.execute('SELECT account,until_ms FROM cooldowns'))
  temp=target.with_suffix('.json.tmp');temp.write_text(json.dumps({'pins':pins,'cooldowns':cooldowns}));temp.chmod(0o600);os.replace(temp,target);c['database']=str(target)
 c['managerBinary']=str(release/'cliproxy')
else:
 for f in ['management-key','proxy-secret','client-key']:(state/f).write_text(secrets.token_hex(32));(state/f).chmod(0o600)
 subprocess.run(['openssl','req','-x509','-newkey','rsa:2048','-nodes','-days','3650','-subj','/CN=Native Claude local CA','-keyout',str(state/'ca.key'),'-out',str(state/'ca.crt')],check=True,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
 subprocess.run(['openssl','req','-new','-newkey','rsa:2048','-nodes','-subj','/CN=api.anthropic.com','-keyout',str(state/'leaf.key'),'-out',str(state/'leaf.csr')],check=True,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
 (state/'leaf.ext').write_text('subjectAltName=DNS:api.anthropic.com,DNS:localhost,IP:127.0.0.1\nextendedKeyUsage=serverAuth\n')
 subprocess.run(['openssl','x509','-req','-in',str(state/'leaf.csr'),'-CA',str(state/'ca.crt'),'-CAkey',str(state/'ca.key'),'-set_serial',str(secrets.randbits(120)),'-days','365','-extfile',str(state/'leaf.ext'),'-out',str(state/'leaf.crt')],check=True,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
 for f in ['ca.key','leaf.key']:(state/f).chmod(0o600)
 manager=configs/'manager.yaml';manager.write_text('config-version: 8\nserver:\n  host: "127.0.0.1"\n  port: 20148\naccess:\n  api-keys:\n    - '+json.dumps((state/'client-key').read_text())+'\nmanagement:\n  local-dashboard: true\n  secret-key: '+json.dumps((state/'management-key').read_text())+'\n  allow-remote: false\noauth:\n  auth-dir: '+json.dumps(str(state/'auth'))+'\n  providers:\n    claude:\n      disable-claude-cloak-mode: true\nrouting:\n  session-affinity: true\n  strategy: soonest-reset\n  retry:\n    request-retry: 0\n');manager.chmod(0o600)
 c={'port':20149,'httpProxyPort':20152,'database':str(state/'routing.json'),'events':str(state/'events.jsonl'),'proxySecret':str(state/'proxy-secret'),'key':str(state/'leaf.key'),'cert':str(state/'leaf.crt'),'ca':str(state/'ca.crt'),'authDir':str(state/'auth'),'managementUrl':'http://127.0.0.1:20148','managementKeyFile':str(state/'management-key'),'settingsFile':str(configs/'settings.json'),'managerBinary':str(release/'cliproxy'),'managerConfig':str(manager),'nativeClaude':str(home/'.local/bin/claude'),'qualification':False}
 (configs/'settings.json').write_text(json.dumps({'preferredAccount':None,'names':{}},indent=2));(configs/'settings.json').chmod(0o600)
manager=pathlib.Path(c['managerConfig']);text=manager.read_text()
# These sections are inserted once; subsequent upgrades preserve administrator edits.
if '\nnative-claude:' not in '\n'+text:
 text+='\nnative-claude:\n  config-file: '+json.dumps(str(configs/'config.json'))+'\n  claude-launcher: '+json.dumps(str(home/'.local/share/cliproxy-native/current/claude'))+'\n  acp-launcher: '+json.dumps(str(home/'.local/share/cliproxy-native/acp/releases/0.89.1-r1/launch'))+'\n'
if '\nobservability:' not in '\n'+text:text+='\nobservability:\n  usage:\n    usage-statistics-enabled: true\n'
manager.write_text(text);manager.chmod(0o600)
tmp=configs/'config.json.tmp';tmp.write_text(json.dumps(c,indent=2));tmp.chmod(0o600);os.replace(tmp,configs/'config.json')
link=home/'.local/bin/cliproxy-native';link.parent.mkdir(parents=True,exist_ok=True)
if link.exists() or link.is_symlink():
 if not link.is_symlink() or not str(link.resolve()).startswith(str(home/'.local/share/cliproxy-native/releases')+'/'):raise SystemExit('Refusing to replace unrelated management command')
 temp=link.with_name('cliproxy-native.tmp');temp.unlink(missing_ok=True);temp.symlink_to(release/'cliproxy-native');os.replace(temp,link)
else:link.symlink_to(release/'cliproxy-native')
print('Pinned Rust installation:',release)
print('Start:',str(release/'cliproxy'),'--config',str(manager),'--local-model')
print('Credentials and existing affinity preserved; Claude/Zed launcher pointers unchanged.')
