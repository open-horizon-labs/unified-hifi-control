import importlib.util, pathlib, unittest
SPEC = importlib.util.spec_from_file_location('router_mcp', pathlib.Path(__file__).with_name('router_mcp.py'))
m = importlib.util.module_from_spec(SPEC); SPEC.loader.exec_module(m)
class Contract(unittest.TestCase):
 def test_all_proxy_operations_have_tools(self):
  self.assertEqual(set(m.TOOLS), {'naa_status','naa_routes','naa_dacs','naa_discovery_status','naa_discover','naa_route_add','naa_route_update','naa_route_remove','naa_select','naa_stop'})
 def test_unknown_arguments_never_reach_backend(self):
  calls=[]
  b=m.Bridge('http://127.0.0.1:8787');b.http=lambda *a:calls.append(a)
  self.assertTrue(b.call('naa_stop',{'host':'other'})['isError']);self.assertEqual(calls,[])
 def test_select_accepted_does_not_claim_resumed(self):
  b=m.Bridge('http://127.0.0.1:8787');b.http=lambda *a:{'hqp_control':{'phase':'waiting_for_naa'},'selected_route_id':'r'}
  result=b.call('naa_select',{'route_id':'r'})['structuredContent']
  self.assertEqual(result['outcome'],'pending')
 def test_timeout_mutation_is_indeterminate_and_not_retried(self):
  b=m.Bridge('http://127.0.0.1:8787');calls=[]
  def fail(*a):calls.append(a);raise TimeoutError('timeout')
  b.http=fail
  self.assertEqual(b.call('naa_select',{'route_id':'r'})['structuredContent']['outcome'],'indeterminate');self.assertEqual(len(calls),1)
 def test_unparseable_write_reply_is_indeterminate(self):
  import json
  b=m.Bridge('http://127.0.0.1:8787')
  def fail(*a):raise json.JSONDecodeError('bad reply','',0)
  b.http=fail
  self.assertEqual(b.call('naa_route_add',{'name':'x','host':'x'})['structuredContent']['outcome'],'indeterminate')
 def test_dacs_read_catalog_without_auth_or_scan(self):
  calls=[];b=m.Bridge('http://127.0.0.1:8787')
  def read(*a):calls.append(a);return {'dac_catalog':[{'host':'x','devices':[]}]}
  b.http=read
  self.assertEqual(b.call('naa_dacs',{})['structuredContent']['data'],[{'host':'x','devices':[]}]);self.assertEqual(calls,[('GET','/api/state',None)])
 def test_only_explicit_local_router_url(self):
  for url in ['http://example.com:8787','http://127.0.0.1:8787/evil','http://u:p@127.0.0.1:8787','http://192.168.1.61:8088','https://127.0.0.1:8787']:
   with self.assertRaises(ValueError):m.Bridge(url)
if __name__=='__main__':unittest.main()
