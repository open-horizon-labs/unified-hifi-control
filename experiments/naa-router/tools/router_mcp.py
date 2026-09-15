#!/usr/bin/env python3
"""Headless PoC client: MCP stdio or a single JSON CLI call. No UI automation.

Migration aid only: UHC production tools must use its coordinator/aggregator.
No retries of writes. A lost response may mean a mutation happened.
"""
import argparse
import concurrent.futures
import http.client
import ipaddress
import json
import sys
import threading
import urllib.parse

STRING = {'type':'string','minLength':1}
ROUTE = {'name':STRING,'host':STRING,'port':{'type':'integer','minimum':1,'maximum':65535},'device_id':{'type':'string'}}
# name -> (method, path, description, properties, required)
TOOLS = {
 'naa_status':('GET','/api/state','Read route, session, operation phase, actual audio counters and errors.',{},[]),
 'naa_routes':('GET','/api/routes','List saved routes and exact route/device IDs.',{},[]),
 'naa_dacs':('GET','/api/state','Read last-seen physical DAC lists. Cached observations, not current attachment; no authentication or scan.',{},[]),
 'naa_discovery_status':('GET','/api/discovery','Read whether LAN discovery is configured.',{},[]),
 'naa_discover':('POST','/api/discover','Run a bounded NAA multicast scan. Does not select a route, authenticate or play.',{},[]),
 'naa_route_add':('POST','/api/routes','Save an explicit route; does not select it. Blank device_id resolves a sole DAC on connection.',ROUTE,['name','host']),
 'naa_route_update':('POST','/api/routes/update','Edit a saved route. Editing the selected route disconnects its session.',{'route_id':STRING,**ROUTE},['route_id','name','host']),
 'naa_route_remove':('POST','/api/routes/remove','Remove an explicit saved route; may stop its active session.',{'route_id':STRING},['route_id']),
 'naa_select':('POST','/api/select','Select an exact saved route. May stop and resume playback. A pending result is NOT resumed audio: inspect naa_status until terminal. Do not blindly retry after a lost response.',{'route_id':STRING},['route_id']),
 'naa_stop':('POST','/api/stop','Stop local routing and cancel pending selection. Native HQPlayer Stop can report partial failure; inspect returned state.',{},[]),
}

def listing():
 return [{'name':n,'description':d,'inputSchema':{'type':'object','properties':p,'required':r,'additionalProperties':False},'annotations':{'readOnlyHint':method=='GET','destructiveHint':n in ('naa_select','naa_stop','naa_route_update','naa_route_remove'),'idempotentHint':method=='GET','openWorldHint':True}} for n,(method,_,d,p,r) in TOOLS.items()]

class BackendError(Exception): pass
class Bridge:
 def __init__(self,url):
  u=urllib.parse.urlsplit(url)
  if u.scheme!='http' or u.username or u.password or u.path not in ('','/') or u.query or u.fragment or not u.port:
   raise ValueError('Use an explicit loopback HTTP router URL with a port and no credentials/path')
  if not ipaddress.ip_address(u.hostname).is_loopback:raise ValueError('Router API must be loopback; use a local tunnel for a remote host')
  self.host,self.port=u.hostname,u.port
 def http(self,method,path,body):
  c=http.client.HTTPConnection(self.host,self.port,timeout=16)
  try:
   c.request(method,path,None if body is None else json.dumps(body),{} if body is None else {'Content-Type':'application/json'})
   r=c.getresponse();raw=r.read(2*1024*1024+1)
   if len(raw)>2*1024*1024:raise OSError('Router response exceeded bound')
   value=json.loads(raw)
   if r.status>=300:raise BackendError(value.get('error',f'HTTP {r.status}'))
   return value
  finally:c.close()
 def call(self,name,args):
  method='GET'
  try:
   if name not in TOOLS:raise ValueError('Unknown tool')
   method,path,_,properties,required=TOOLS[name]
   if not isinstance(args,dict) or set(args)-set(properties) or any(k not in args for k in required):raise ValueError('Invalid or missing tool arguments')
   for key,value in args.items():
    rule=properties[key]
    if rule['type']=='string' and (not isinstance(value,str) or len(value)<rule.get('minLength',0)):raise ValueError(f'Invalid {key}')
    if rule['type']=='integer' and (type(value) is not int or not rule['minimum']<=value<=rule['maximum']):raise ValueError(f'Invalid {key}')
   value=self.http(method,path,None if method=='GET' else args)
   if name=='naa_dacs':value=value.get('dac_catalog',[])
   phase=(value.get('hqp_control') or {}).get('phase') if isinstance(value,dict) else None
   outcome='observed' if method=='GET' or name=='naa_discover' else 'accepted'
   if phase in ('checking','stopping','waiting_for_naa','resuming'):outcome='pending'
   if phase=='error':outcome='error'
   return result(outcome,value,outcome=='error')
  except json.JSONDecodeError as e:return result('indeterminate' if method=='POST' else 'unavailable',{'error':'Invalid router response; read naa_status before retrying.'},True)
  except (ValueError,BackendError) as e:return result('error',{'error':str(e)},True)
  except (OSError,http.client.HTTPException) as e:
   return result('indeterminate' if method=='POST' else 'unavailable',{'error':str(e),'next':'Read naa_status before retrying a mutation.'},True)

def result(outcome,data,error=False):
 envelope={'outcome':outcome,'data':data}
 return {'content':[{'type':'text','text':json.dumps(envelope)}],'structuredContent':envelope,'isError':error}

def serve(bridge):
 lock=threading.Lock();slots=threading.BoundedSemaphore(7);stop_slot=threading.BoundedSemaphore(1)
 workers=concurrent.futures.ThreadPoolExecutor(max_workers=7)
 emergency=concurrent.futures.ThreadPoolExecutor(max_workers=1)
 initialized=False
 def emit(message):
  with lock:print(json.dumps(message,separators=(',',':')),flush=True)
 def reply(ident,value):emit({'jsonrpc':'2.0','id':ident,'result':value})
 def fail(ident,code,message):emit({'jsonrpc':'2.0','id':ident,'error':{'code':code,'message':message}})
 def invoke(ident,name,args,slot):
  try:reply(ident,bridge.call(name,args))
  finally:slot.release()
 try:
  while True:
   line=sys.stdin.buffer.readline(1024*1024+1)
   if not line:break
   if len(line)>1024*1024:fail(None,-32700,'Message exceeds bound');break
   ident=None
   try:
    msg=json.loads(line)
    if not isinstance(msg,dict) or msg.get('jsonrpc')!='2.0':raise ValueError('Invalid JSON-RPC request')
    ident=msg.get('id');method=msg.get('method');params=msg.get('params',{})
    if not isinstance(params,dict):raise ValueError('Parameters must be an object')
    if ident is None:continue # Notifications never receive responses or invoke tools.
    if method=='initialize':
     version=params.get('protocolVersion');version=version if version in ('2024-11-05','2025-03-26','2025-06-18') else '2025-06-18'
     initialized=True;reply(ident,{'protocolVersion':version,'capabilities':{'tools':{}},'serverInfo':{'name':'hiphi-naa-router-poc','version':'0.1.0'}})
    elif method=='ping':reply(ident,{})
    elif not initialized:fail(ident,-32000,'Initialize first')
    elif method=='tools/list':reply(ident,{'tools':listing()})
    elif method=='tools/call':
     name=params.get('name');args=params.get('arguments',{})
     if not isinstance(name,str) or name not in TOOLS:fail(ident,-32602,'Unknown tool');continue
     slot,pool=(stop_slot,emergency) if name=='naa_stop' else (slots,workers)
     if not slot.acquire(blocking=False):reply(ident,result('error',{'error':'Tool workers busy; no request dispatched'},True));continue
     pool.submit(invoke,ident,name,args,slot)
    else:fail(ident,-32601,'Method not found')
   except (ValueError,TypeError) as e:fail(ident,-32602,str(e))
 finally:
  workers.shutdown(wait=True);emergency.shutdown(wait=True)

def main():
 p=argparse.ArgumentParser(description=__doc__);p.add_argument('--router-url',required=True);p.add_argument('--call',choices=TOOLS);p.add_argument('--arguments',default='{}');a=p.parse_args()
 try:
  b=Bridge(a.router_url)
  if a.call:
   value=b.call(a.call,json.loads(a.arguments));print(json.dumps(value));return int(value['isError'])
  serve(b);return 0
 except ValueError as e:print(str(e),file=sys.stderr);return 2
if __name__=='__main__':raise SystemExit(main())
