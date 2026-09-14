"""MCP wire tests against the real copied router and explicit software fixtures."""
import argparse,json,pathlib,queue,subprocess,sys,threading,unittest
from http.server import BaseHTTPRequestHandler,ThreadingHTTPServer
import naa_router_lab as lab
BINARY=None
class Client:
 def __init__(self,port):
  self.p=subprocess.Popen([sys.executable,str(pathlib.Path(__file__).with_name('router_mcp.py')),'--router-url',f'http://127.0.0.1:{port}'],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True)
  self.replies=queue.Queue();self.seq=0
  self.reader=threading.Thread(target=lambda:[self.replies.put(json.loads(line)) for line in self.p.stdout],daemon=True);self.reader.start()
  self.send('initialize',{'protocolVersion':'2025-06-18','capabilities':{},'clientInfo':{'name':'test','version':'1'}});self.receive()
  self.p.stdin.write(json.dumps({'jsonrpc':'2.0','method':'notifications/initialized'})+'\n');self.p.stdin.flush()
 def send(self,method,params):
  self.seq+=1;self.p.stdin.write(json.dumps({'jsonrpc':'2.0','id':self.seq,'method':method,'params':params})+'\n');self.p.stdin.flush();return self.seq
 def receive(self):return self.replies.get(timeout=8)
 def call(self,tool_name,**arguments):
  ident=self.send('tools/call',{'name':tool_name,'arguments':arguments});value=self.receive();assert value['id']==ident,value;return value['result']
 def close(self):
  self.p.stdin.close();self.p.wait(timeout=20);self.reader.join(1);self.p.stdout.close();self.p.stderr.close()
class Integration(unittest.TestCase):
 def test_all_operations_through_stdio_and_real_router(self):
  lab.BINARY=BINARY;r=lab.Router(discovery=True);f=lab.FakeNaa('MCP fixture','usb:test',44100);c=Client(r.http_port)
  try:
   c.send('tools/list',{});self.assertEqual(len(c.receive()['result']['tools']),10)
   self.assertEqual(c.call('naa_status')['structuredContent']['data']['selected_route_id'],None)
   cli=subprocess.run([sys.executable,str(pathlib.Path(__file__).with_name('router_mcp.py')),'--router-url',f'http://127.0.0.1:{r.http_port}','--call','naa_status'],capture_output=True,text=True,timeout=5)
   self.assertEqual(cli.returncode,0,cli.stderr);self.assertIsNone(json.loads(cli.stdout)['structuredContent']['data']['selected_route_id'])
   self.assertTrue(c.call('naa_discovery_status')['structuredContent']['data']['enabled'])
   self.assertEqual(c.call('naa_discover')['structuredContent']['data'],[])
   route=c.call('naa_route_add',name=f.name,host='127.0.0.1',port=f.port,device_id=f.device_id)['structuredContent']['data'];rid=route['id']
   self.assertEqual(c.call('naa_routes')['structuredContent']['data'][0]['id'],rid)
   self.assertFalse(c.call('naa_route_update',route_id=rid,name='Renamed',host='127.0.0.1',port=f.port,device_id=f.device_id)['isError'])
   self.assertFalse(c.call('naa_select',route_id=rid)['isError'])
   conn,_,_=r.connect('mcp-auth');conn.sendall(lab.control('getdevices',direction='output'));lab.line(conn)
   self.assertEqual(c.call('naa_dacs')['structuredContent']['data'][0]['devices'][0]['id'],f.device_id)
   self.assertFalse(c.call('naa_stop')['isError']);conn.close()
   self.assertIsNone(c.call('naa_status')['structuredContent']['data']['selected_route_id'])
   self.assertFalse(c.call('naa_route_remove',route_id=rid)['isError']);self.assertEqual(c.call('naa_routes')['structuredContent']['data'],[])
  finally:c.close();r.close();f.close()
 def test_stop_overtakes_stalled_selection_on_same_stdio_connection(self):
  entered=threading.Event();released=threading.Event()
  class Handler(BaseHTTPRequestHandler):
   def log_message(self,*args):pass
   def do_POST(self):
    self.rfile.read(int(self.headers['Content-Length']))
    if self.path=='/api/select':entered.set();released.wait(5)
    if self.path=='/api/stop':released.set()
    body=b'{}';self.send_response(200);self.send_header('Content-Length',len(body));self.end_headers();self.wfile.write(body)
  server=ThreadingHTTPServer(('127.0.0.1',0),Handler);thread=threading.Thread(target=server.serve_forever,daemon=True);thread.start();c=Client(server.server_port)
  try:
   select_id=c.send('tools/call',{'name':'naa_select','arguments':{'route_id':'r'}});self.assertTrue(entered.wait(2))
   stop_id=c.send('tools/call',{'name':'naa_stop','arguments':{}})
   self.assertTrue(released.wait(1), 'Stop was queued behind the stalled selection')
   responses=[c.receive(),c.receive()];self.assertEqual({r['id'] for r in responses},{select_id,stop_id});self.assertTrue(released.is_set())
  finally:released.set();c.close();server.shutdown();server.server_close();thread.join(1)
if __name__=='__main__':
 p=argparse.ArgumentParser();p.add_argument('--binary',required=True);a=p.parse_args();BINARY=a.binary;unittest.main(argv=[sys.argv[0]])
