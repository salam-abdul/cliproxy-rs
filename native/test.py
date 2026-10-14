#!/usr/bin/python3
import os,pathlib,subprocess,tempfile,sys
root=pathlib.Path(__file__).parent
with tempfile.TemporaryDirectory(prefix='native-tests-') as temp:
 d=pathlib.Path(temp)
 commands=[['openssl','req','-x509','-newkey','rsa:2048','-nodes','-days','1','-subj','/CN=Native test CA','-keyout',str(d/'ca.key'),'-out',str(d/'ca.crt')],['openssl','req','-new','-newkey','rsa:2048','-nodes','-subj','/CN=api.anthropic.com','-keyout',str(d/'leaf.key'),'-out',str(d/'leaf.csr')]]
 for c in commands:subprocess.run(c,check=True,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
 (d/'ext').write_text('subjectAltName=DNS:localhost,DNS:api.anthropic.com,IP:127.0.0.1\nextendedKeyUsage=serverAuth\n')
 subprocess.run(['openssl','x509','-req','-in',str(d/'leaf.csr'),'-CA',str(d/'ca.crt'),'-CAkey',str(d/'ca.key'),'-set_serial','1','-days','1','-extfile',str(d/'ext'),'-out',str(d/'leaf.crt')],check=True,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
 env=dict(os.environ,TRANSPARENT_TEST_CERT_DIR=str(d),TRANSPARENT_TEST_CA=str(d/'ca.crt'));env.setdefault('NATIVE_ROUTER_TEST_BINARY',str(root.parent/'target/debug/examples/native_router_fixture'));env.setdefault('NATIVE_MANAGER_TEST_BINARY',str(root.parent/'target/debug/cliproxy'))
 subprocess.run(['node','--test',*[str(p) for p in sorted(root.glob('*.test.cjs'))]],env=env,check=True)
