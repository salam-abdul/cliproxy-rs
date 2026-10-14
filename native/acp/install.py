#!/usr/bin/python3
"""Build the qualified ACP adapter from pinned official packages plus a small patch."""
from pathlib import Path
import hashlib,json,shutil,subprocess,sys
src=Path(__file__).resolve().parent
root=Path(sys.argv[1]).resolve() if len(sys.argv)>1 else Path.home()/'.local/share/cliproxy-native/acp/releases/0.89.1-native1'
if root.exists():raise SystemExit('Refusing to overwrite an ACP release; choose a new directory')
root.mkdir(parents=True)
for name in ['package.json','package-lock.json']:shutil.copy2(src/name,root/name)
subprocess.run(['npm','ci','--ignore-scripts','--no-audit','--no-fund','--prefix',str(root)],check=True)
for relative,expected in json.loads((src/'upstream-hashes.json').read_text()).items():
 if hashlib.sha256((root/relative).read_bytes()).hexdigest()!=expected:raise SystemExit('Unexpected upstream ACP bytes: '+relative)
subprocess.run(['git','apply','--unsafe-paths','--directory='+str(root),str(src/'remote-control.patch')],check=True)
shutil.copy2(src/'routed-remote-control.mjs',root/'node_modules/@agentclientprotocol/claude-agent-acp/dist/routed-remote-control.mjs')
shutil.copy2(src/'launch',root/'launch');(root/'launch').chmod(0o755)
files=['launch','node_modules/@agentclientprotocol/claude-agent-acp/dist/acp-agent.js','node_modules/@agentclientprotocol/claude-agent-acp/dist/routed-remote-control.mjs','node_modules/@anthropic-ai/claude-agent-sdk/sdk.mjs']
manifest={'adapterVersion':'0.89.1','sdkVersion':'0.3.293','qualifiedNativeClaudeVersion':'2.1.296','hashes':{f:hashlib.sha256((root/f).read_bytes()).hexdigest() for f in files}}
(root/'manifest.json').write_text(json.dumps(manifest,indent=2)+'\n')
print('ACP launcher:',root/'launch');print('Not activated; configure Zed after qualification.')
