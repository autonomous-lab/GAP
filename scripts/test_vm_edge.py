"""Real nginx/Caddy boundary regression: isolated ports, no production state."""
import argparse
import base64
import contextlib
import http.client
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
from pathlib import Path
import socket
import subprocess
import tempfile
import threading
import time
import uuid

SECRET='ab'*32
PROJECT='prj_'+'a'*24
VM='vm_'+'c'*32
BASIC='Basic '+base64.b64encode(b'visitor:isolated password').decode()


def port():
    with socket.socket() as s:
        s.bind(('127.0.0.1',0));return s.getsockname()[1]


class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path=='/internal/vm-http-admission':
            h=self.headers;path=h.get('X-GAP-Original-Path','');host=h.get('X-GAP-Original-Host','')
            assert h.get('X-GAP-Edge-Token')==SECRET
            custom=host=='app.customer.test'
            private=path.startswith('/apps/')
            if private and not custom and h.get('X-GAP-Original-Authorization')!=BASIC:
                self.send_response(401);self.send_header('WWW-Authenticate','Basic realm="GAP microVM"');self.end_headers();return
            self.send_response(200)
            if custom or private:
                from urllib.parse import quote
                raw=h['X-GAP-Original-URI'];query='?'+raw.split('?',1)[1] if '?' in raw else ''
                self.send_header('X-GAP-VM-Identity',VM)
                self.send_header('X-GAP-VM-URI',('/apps/'+PROJECT if custom else '')+quote(path,safe='/-._~')+query)
                if not custom:self.send_header('X-GAP-VM-Strip-Auth','1')
            self.end_headers();return
        if self.headers.get('Upgrade','').lower()=='websocket':
            self.send_response(101);self.send_header('Upgrade','websocket');self.send_header('Connection','Upgrade');self.end_headers();return
        self.send_response(200);self.send_header('Content-Type','application/json');self.end_headers()
        self.wfile.write(json.dumps({'path':self.path,'headers':dict(self.headers),'kind':self.server.kind}).encode())
    do_POST=do_GET
    def log_message(self,*args):pass


def main():
    parser=argparse.ArgumentParser();parser.add_argument('--edge-image',default='gap-http-edge');parser.add_argument('--caddy-image',default='caddy:2.10.2-alpine');args=parser.parse_args()
    repo=Path(__file__).resolve().parent.parent
    import sys
    sys.path.insert(0,str(repo/'runtime/compose'))
    from ingress import Ingress
    from microvm import MicroVMs
    from unittest.mock import patch
    names=[]
    with tempfile.TemporaryDirectory() as tmp, contextlib.ExitStack() as stack:
        tmp=Path(tmp)
        def backend(kind):
            server=ThreadingHTTPServer(('127.0.0.1',0),Handler);server.kind=kind
            threading.Thread(target=server.serve_forever,daemon=True).start();stack.callback(server.server_close);stack.callback(server.shutdown);return server.server_port
        node,guest,realtime=backend('node'),backend('guest'),backend('realtime')
        edge_port,caddy_port=port(),port()
        def run(image,config,binary):
            name='gap-http-test-'+uuid.uuid4().hex[:10];names.append(name)
            command=['docker','run','-d','--rm','--network','host','--name',name,'-v',str(config)+':/tmp/test.conf:ro','--entrypoint',binary,image]
            command+=['-c','/tmp/test.conf','-g','daemon off;'] if binary=='nginx' else ['run','--config','/tmp/test.conf']
            subprocess.run(command,check=True,stdout=subprocess.DEVNULL)
            return name
        def req(path='/',host='client.test',auth=None,extra=None,target=None):
            c=http.client.HTTPConnection('127.0.0.1',target or edge_port,timeout=5)
            h={'Host':host,**(extra or {})}
            if auth:h['Authorization']=auth
            c.request('GET',path,headers=h);r=c.getresponse();data=r.read();headers=dict(r.getheaders());status=r.status;c.close()
            try:data=json.loads(data)
            except ValueError:pass
            return status,data,headers
        def ready(p):
            for _ in range(100):
                try:req(target=p);return
                except OSError:time.sleep(.1)
            raise AssertionError('proxy did not start')
        try:
            token=tmp/'token';token.write_text(SECRET)
            manager=MicroVMs({'state_dir':str(tmp/'state'),'image_dir':str(tmp)},None)
            meta={'vm_id':VM,'project_id':PROJECT,'owner_did':'did:gap:'+'b'*64,'state':'running','vcpus':1,'memory_mib':1024,'disk_gib':4,'ports':[{'guest_port':8000,'worker_port':guest}],'ingress':{'enabled':True,'guest_port':8000}}
            with patch.object(manager,'sync_environment'),patch.object(manager,'public',return_value={'state':'running'}),patch('ingress.AdminConnection') as conn:
                conn.return_value.getresponse.return_value.status=200;manager.save(meta)
                ingress=Ingress({'dedicated_caddy':True,'admission_token_file':str(token),'http_port':caddy_port},manager)
                config,_=ingress.configuration();config['admin']={'disabled':True}
            cp=tmp/'caddy.json';cp.write_text(json.dumps(config));cp.chmod(0o644)
            run(args.caddy_image,cp,'caddy');ready(caddy_port)
            template=(repo/'runtime/edge/nginx.conf').read_text().replace('${GAP_ADMIN_HOST}','admin.test').replace('${GAP_VM_EDGE_TOKEN}',SECRET).replace('gap-node:8080','127.0.0.1:'+str(node)).replace('gap-realtime:8091','127.0.0.1:'+str(realtime)).replace('172.17.0.1:8093','127.0.0.1:'+str(caddy_port)).replace('listen 8080','listen '+str(edge_port))
            nc=tmp/'nginx.conf';nc.write_text(template);nc.chmod(0o644);run(args.edge_image,nc,'nginx');ready(edge_port)
            app='/apps/'+PROJECT+'/'
            for path in [app,app[:-1],'/apps%2f'+PROJECT+'/',app+'ws','/other/../apps/'+PROJECT+'/']:
                status,_,h=req(path);assert status==401,(path,status);assert 'Basic' in h['WWW-Authenticate']
            assert req(app,extra={'X-GAP-VM-Admission':SECRET,'X-GAP-VM-URI':app,'X-GAP-VM-Strip-Auth':'1'})[0]==401
            assert req(app,target=caddy_port)[0]==401
            assert req(app,target=caddy_port,extra={'X-GAP-VM-Admission':'wrong'})[0]==401
            assert req(app,target=caddy_port,extra={'X-GAP-VM-Admission':SECRET,'X-GAP-VM-Identity':'vm_wrong'})[0]==401
            status,data,h=req(app+'hello%20world?q=1',auth=BASIC)
            assert status==200,(status,data);assert data['kind']=='guest';assert data['path']=='/hello%20world?q=1',data
            assert 'Authorization' not in data['headers'];assert 'X-Gap-Vm-Admission' not in data['headers'];assert SECRET not in json.dumps(data);assert h['Cache-Control']=='no-store';assert data['headers'].get('X-Forwarded-Prefix')=='/apps/'+PROJECT
            status,data,_=req('/api/data?q=1',host='app.customer.test',auth='Bearer application-token')
            assert status==200 and data['kind']=='guest',data;assert data['headers'].get('Authorization')=='Bearer application-token';assert data['path']=='/api/data?q=1';assert SECRET not in json.dumps(data);assert not data['headers'].get('X-Forwarded-Prefix')
            status,data,_=req('/v1/example',auth='Bearer owner-token',extra={'X-GAP-VM-Admission':'spoof'})
            assert status==200 and data['kind']=='node';assert data['headers']['Authorization']=='Bearer owner-token';assert SECRET not in json.dumps(data);assert 'spoof' not in json.dumps(data)
            assert req('/v1/realtime')[1]['kind']=='realtime'
            data=req('/_gap/realtime?q=1',host='site.customer.test')[1];assert data['kind']=='realtime' and data['path']=='/v1/realtime?q=1',data
            assert req(app,host='admin.test')[1]['kind']=='node'
            assert req(app+'ws',extra={'Upgrade':'websocket','Connection':'Upgrade'})[0]==401
            assert req(app+'ws',auth=BASIC,extra={'Upgrade':'websocket','Connection':'Upgrade'})[0]==101
            assert req('/ws',host='app.customer.test',extra={'Upgrade':'websocket','Connection':'Upgrade'})[0]==101
            print('PASS real nginx + Caddy: shared Basic, direct proxy rejection, headers, custom domains, URI encoding, realtime, admin isolation, WebSocket handshake')
        except BaseException:
            for name in names:subprocess.run(['docker','logs','--tail','15',name],check=False)
            raise
        finally:
            for name in reversed(names):subprocess.run(['docker','rm','-f',name],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)


if __name__=='__main__':main()
