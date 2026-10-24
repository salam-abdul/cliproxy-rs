const test = require("node:test"), assert = require("node:assert/strict"), cp = require("node:child_process"), path = require("node:path");

function launch(args) {
  const script = `
import hashlib,json,os,pathlib,runpy,ssl,sys,tempfile,urllib.request
from unittest.mock import patch
source=pathlib.Path(sys.argv[1])
args=json.loads(sys.argv[2])
with tempfile.TemporaryDirectory() as temp:
 root=pathlib.Path(temp)
 launcher=root/'claude';launcher.write_bytes(source.read_bytes())
 (root/'manifest.json').write_text(json.dumps({'hashes':{'claude':hashlib.sha256(launcher.read_bytes()).hexdigest()}}))
 exe=root/'native';exe.write_text('fixture')
 secret=root/'secret';secret.write_text('synthetic-proxy-secret')
 config=root/'config.json';config.write_text(json.dumps({'nativeClaude':str(exe),'ca':str(root/'ca'),'port':1,'httpProxyPort':2,'proxySecret':str(secret)}))
 class Reply:
  def read(self):return b'{"ready":true}'
 class Opener:
  def open(self,*a,**kw):return Reply()
 def capture(exe,argv,env):
  print(json.dumps({'args':argv[1:],'env':{k:v for k,v in env.items() if k in ['HTTPS_PROXY','NO_PROXY','ANTHROPIC_API_KEY','ANTHROPIC_AUTH_TOKEN','ANTHROPIC_BASE_URL','CLAUDE_CODE_OAUTH_TOKEN','NODE_EXTRA_CA_CERTS']},'homePreserved':env.get('HOME')==os.environ.get('HOME'),'configDirPreserved':env.get('CLAUDE_CONFIG_DIR')==os.environ.get('CLAUDE_CONFIG_DIR')}))
 env={'CLIPROXY_NATIVE_CONFIG':str(config),'ANTHROPIC_API_KEY':'synthetic-api-key','ANTHROPIC_AUTH_TOKEN':'synthetic-auth','CLAUDE_CODE_OAUTH_TOKEN':'synthetic-oauth','ANTHROPIC_BASE_URL':'http://legacy.invalid','HTTPS_PROXY':'http://legacy.invalid','NO_PROXY':'*'}
 with patch.dict(os.environ,env),patch.object(sys,'argv',[str(launcher),*args]),patch.object(ssl,'create_default_context'),patch.object(urllib.request,'build_opener',return_value=Opener()),patch.object(os,'execve',side_effect=capture):
  runpy.run_path(str(launcher),run_name='__main__')
`;
  const r = cp.spawnSync("python3", ["-c", script, path.join(__dirname, "claude"), JSON.stringify(args)], {encoding:"utf8"});
  assert.equal(r.status, 0, r.stderr);
  return JSON.parse(r.stdout);
}

test("Remote Control server inherits native routing without refused root settings", () => {
  for (const command of ["rc", "remote-control"]) {
    const args = [command, "--spawn", "worktree", "--capacity", "2"];
    const r = launch(args);
    assert.deepEqual(r.args, args);
    assert.equal(r.env.HTTPS_PROXY, "http://claude:synthetic-proxy-secret@127.0.0.1:2");
    assert.equal(r.env.NO_PROXY, "localhost,127.0.0.1,::1");
    assert.ok(r.env.NODE_EXTRA_CA_CERTS.endsWith("/ca"));
    for (const key of ["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN", "CLAUDE_CODE_OAUTH_TOKEN", "ANTHROPIC_BASE_URL"]) assert.equal(r.env[key], undefined);
    assert.ok(r.homePreserved && r.configDirPreserved);
  }
});

test("interactive Remote Control keeps the native settings override", () => {
  const args = ["--remote-control", "Fixture session"];
  const r = launch(args);
  assert.equal(r.args[0], "--settings");
  assert.deepEqual(JSON.parse(r.args[1]), {apiKeyHelper:"",env:{ANTHROPIC_BASE_URL:"https://api.anthropic.com"}});
  assert.deepEqual(r.args.slice(2), args);
});
