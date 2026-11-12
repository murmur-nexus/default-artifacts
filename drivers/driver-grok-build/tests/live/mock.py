# Minimal OpenAI Responses API mock (SSE), used by both grok and codex.
# Copied from the grok/codex recordings' tools/mock.py. Additions for driver-grok-build:
#   TOOL_INPUT=<compact JSON object>   the tool_input a `CALL_TOOL=use_tool:<server__tool>` call
#                                      carries, instead of {"text":"hi"}
#   CALL_FILE_FORM=tool_input_file:<server__tool> | CALL_FILE_FORM=file, with FORM_PATH=<path>
#                                      a `use_tool` call in one of grok's file-backed forms
#   every function_call_output in a request is logged as a TOOLOUTPUT line: its shape, and the
#   sha256 of every base64 image it carries: an image block's data, or a data: URL, also one
#   written into text
# Usage: python3 -I mock.py <port> <log>
import base64, hashlib
import http.server, json, sys, time, itertools, re
PORT=int(sys.argv[1]); LOG=open(sys.argv[2],'a')
seq=itertools.count()
def log(*a): LOG.write(" ".join(str(x) for x in a)+"\n"); LOG.flush()
def texts(inp):
    out=[]
    for it in inp if isinstance(inp,list) else [{"role":"user","content":inp}]:
        c=it.get('content')
        if isinstance(c,str): out.append((it.get('role'),c))
        elif isinstance(c,list):
            for p in c:
                if isinstance(p,dict) and 'text' in p: out.append((it.get('role'),p['text']))
    return out
def image_digests(value, found):
    # Every base64 image anywhere in a tool output: data: URLs and {"data": ...} blocks.
    if isinstance(value, dict):
        for k, v in value.items():
            if isinstance(v, str) and v.startswith('data:') and ';base64,' in v:
                found.append((v[5:v.index(';')], hashlib.sha256(base64.b64decode(v.split(',',1)[1])).hexdigest()))
            elif k == 'data' and isinstance(v, str) and len(v) > 64:
                try: found.append((value.get('mimeType') or value.get('media_type') or '?', hashlib.sha256(base64.b64decode(v, validate=True)).hexdigest()))
                except Exception: pass
            else: image_digests(v, found)
    elif isinstance(value, list):
        for v in value: image_digests(v, found)
    elif isinstance(value, str):
        # A data: URL written into text, as grok writes an MCP image item into a tool output.
        for media, data in re.findall(r'data:([A-Za-z0-9.+/-]+);base64,([A-Za-z0-9+/=]+)', value):
            found.append((media + " (as text)", hashlib.sha256(base64.b64decode(data)).hexdigest()))
        if value[:1] in '[{':
            try: image_digests(json.loads(value), found)
            except Exception: pass
    return found
def shape(value):
    if isinstance(value, dict): return {k: shape(v) for k, v in value.items()}
    if isinstance(value, list): return [shape(v) for v in value]
    if isinstance(value, str): return value if len(value) <= 160 else f"<string of {len(value)} chars>"
    return value
def tool_names(tools):
    names=[]
    for t in tools or []:
        if t.get('type')=='function': names.append(t.get('name'))
        elif t.get('type')=='namespace':
            for s in t.get('tools',[]): names.append(t.get('name','')+'/'+str(s.get('name')))
        else: names.append(t.get('type')+':'+str(t.get('name',t.get('server_label',''))))
    return names
class H(http.server.BaseHTTPRequestHandler):
    protocol_version='HTTP/1.1'
    def log_message(self,*a): pass
    def ev(self,obj):
        obj.setdefault('sequence_number',next(seq))
        data=f"event: {obj['type']}\ndata: {json.dumps(obj)}\n\n".encode()
        self.wfile.write(f"{len(data):x}\r\n".encode()+data+b"\r\n"); self.wfile.flush()
    def do_GET(self):
        log("GET",self.path)
        if False:
            b=json.dumps({"object":"list","data":[{"id":"mock-model","object":"model","owned_by":"mock"}],"models":[]}).encode()
            self.send_response(200)
        else:
            b=b'{"error":{"message":"not found"}}'; self.send_response(404)
        self.send_header('content-type','application/json'); self.send_header('content-length',str(len(b))); self.end_headers(); self.wfile.write(b)
    def do_POST(self):
        n=int(self.headers.get('content-length') or 0); raw=self.rfile.read(n)
        try: body=json.loads(raw)
        except Exception: body={}
        inp=body.get('input',[]); tools=body.get('tools',[])
        tx=texts(inp); users=[t for r,t in tx if r=='user']; last=' '.join(u for u in users if '<agent_reply>' not in u)
        has_out=any(isinstance(i,dict) and i.get('type') in('function_call_output','custom_tool_call_output','mcp_call') for i in (inp if isinstance(inp,list) else []))
        side = isinstance(body.get('tool_choice'),dict)
        log("POST",self.path,json.dumps({"model":body.get('model'),"side":side,"last_user":last[-200:],"tools":tool_names(tools),"has_output":has_out,
            "instructions":(body.get('instructions') or '')[:300],"roles":[ (i.get('role') or i.get('type')) for i in inp if isinstance(i,dict)]}))
        if not side: log("FULLBODY",raw.decode(errors='replace')[:800000])
        for it in (inp if isinstance(inp,list) else []):
            if isinstance(it,dict) and it.get('type')=='function_call_output':
                log("TOOLOUTPUT",json.dumps({"call_id":it.get('call_id'),"output_type":type(it.get('output')).__name__,"shape":shape(it.get('output')),"images":image_digests(it.get('output'),[])}))
        if 'AUTH401' in last or 'QUOTA429' in last:
            code=401 if 'AUTH401' in last else 429
            b=json.dumps({"error":{"message":"mock "+str(code),"type":"invalid_request_error" if code==401 else "rate_limit_exceeded","code":None}}).encode()
            self.send_response(code); self.send_header('content-type','application/json'); self.send_header('content-length',str(len(b))); self.end_headers(); self.wfile.write(b); return
        self.send_response(200); self.send_header('content-type','text/event-stream'); self.send_header('transfer-encoding','chunked'); self.end_headers()
        rid=f"resp_{next(seq)}"; base={"id":rid,"object":"response","created_at":int(time.time()),"model":body.get('model'),"status":"in_progress","output":[],"error":None,"incomplete_details":None,"instructions":None,"metadata":{},"parallel_tool_calls":True,"temperature":1.0,"top_p":1.0,"tool_choice":"auto","tools":[],"text":{"format":{"type":"text"}},"usage":None}
        self.ev({"type":"response.created","response":base}); self.ev({"type":"response.in_progress","response":base})
        out=[]; oi=0
        if side:
            fc={"type":"function_call","id":"fc_t","call_id":"call_t","name":body['tool_choice'].get('name','x'),"arguments":json.dumps({"session_title":"Mock session"}),"status":"completed"}
            self.ev({"type":"response.output_item.added","output_index":0,"item":fc}); self.ev({"type":"response.output_item.done","output_index":0,"item":fc}); out=[fc]
        else:
            if 'THINK' in last:
                rs={"type":"reasoning","id":"rs_1","summary":[]}
                self.ev({"type":"response.output_item.added","output_index":oi,"item":rs})
                self.ev({"type":"response.reasoning_summary_part.added","item_id":"rs_1","output_index":oi,"summary_index":0,"part":{"type":"summary_text","text":""}})
                for d in ["Thinking ","about it."]:
                    self.ev({"type":"response.reasoning_summary_text.delta","item_id":"rs_1","output_index":oi,"summary_index":0,"delta":d})
                self.ev({"type":"response.reasoning_summary_text.done","item_id":"rs_1","output_index":oi,"summary_index":0,"text":"Thinking about it."})
                self.ev({"type":"response.reasoning_summary_part.done","item_id":"rs_1","output_index":oi,"summary_index":0,"part":{"type":"summary_text","text":"Thinking about it."}})
                rs={"type":"reasoning","id":"rs_1","summary":[{"type":"summary_text","text":"Thinking about it."}]}
                self.ev({"type":"response.output_item.done","output_index":oi,"item":rs}); out.append(rs); oi+=1
            names=tool_names(tools)
            target=None
            m=re.search(r'CALL_TOOL(?:=(\S+))?',last); utname=None
            if m and m.group(1) and m.group(1).startswith('use_tool:'): utname=m.group(1).split(':',1)[1]
            if m and not has_out:
                want=('use_tool' if utname else m.group(1)) or 'echo_tool'
                cands=[x for x in names if want in (x or '')]
                target=cands[0] if cands else None
            if 'CALL_PATCH' in last and not has_out:
                ct={"type":"custom_tool_call","id":"ctc_1","call_id":"call_p","name":"apply_patch","input":"*** Begin Patch\n*** Add File: /tmp/rec/patched_outside.txt\n+written by apply_patch\n*** End Patch\n","status":"completed"}
                self.ev({"type":"response.output_item.added","output_index":oi,"item":dict(ct,input="")})
                self.ev({"type":"response.output_item.done","output_index":oi,"item":ct}); out.append(ct); target=None; m=None
            ff=re.search(r'CALL_FILE_FORM=(tool_input_file|file)(?::(\S+))?',last)
            if ff and not has_out:
                fp=re.search(r'FORM_PATH=(\S+)',last); fp=fp.group(1) if fp else "/tmp/murmur-probe.json"
                fargs={"file":fp} if ff.group(1)=='file' else {"tool_name":ff.group(2) or "echo_tool","tool_input_file":fp}
                fc={"type":"function_call","id":"fc_f","call_id":"call_f","name":"use_tool","arguments":json.dumps(fargs),"status":"completed"}
                self.ev({"type":"response.output_item.added","output_index":oi,"item":dict(fc,arguments="",status="in_progress")})
                self.ev({"type":"response.function_call_arguments.delta","item_id":"fc_f","output_index":oi,"delta":fc["arguments"]})
                self.ev({"type":"response.function_call_arguments.done","item_id":"fc_f","output_index":oi,"arguments":fc["arguments"]})
                self.ev({"type":"response.output_item.done","output_index":oi,"item":fc}); out.append(fc); target=None; m=None
            if 'CALL_ASK' in last and not has_out:
                fa={"type":"function_call","id":"fc_a","call_id":"call_a","name":"request_user_input","arguments":json.dumps({"questions":[{"id":"q1","header":"Pick","question":"Which?","options":[{"label":"A","description":"a"},{"label":"B","description":"b"}]}]}),"status":"completed"}
                self.ev({"type":"response.output_item.added","output_index":oi,"item":fa}); self.ev({"type":"response.output_item.done","output_index":oi,"item":fa}); out.append(fa); target=None; m=None
            if target:
                name=target; args={"text":"hi"}
                if target in ('use_tool',):
                    ti=re.search(r'TOOL_INPUT=(\{\S*\})',last)
                    args={"tool_name":utname or "echo_tool","tool_input":json.loads(ti.group(1)) if ti else {"text":"hi"}}
                if '/' in name: ns,name=name.split('/',1)
                else: ns=None
                fc={"type":"function_call","id":"fc_1","call_id":"call_1","name":name,"arguments":"","status":"in_progress"}
                if ns: fc["namespace"]=ns
                self.ev({"type":"response.output_item.added","output_index":oi,"item":fc})
                a=json.dumps(args)
                self.ev({"type":"response.function_call_arguments.delta","item_id":"fc_1","output_index":oi,"delta":a[:6]})
                self.ev({"type":"response.function_call_arguments.delta","item_id":"fc_1","output_index":oi,"delta":a[6:]})
                self.ev({"type":"response.function_call_arguments.done","item_id":"fc_1","output_index":oi,"arguments":a})
                fc=dict(fc,arguments=a,status="completed")
                self.ev({"type":"response.output_item.done","output_index":oi,"item":fc}); out.append(fc)
            elif not out or out[-1].get('type')=='reasoning':
                if m and not has_out: reply="NO_MATCHING_TOOL tools="+",".join(map(str,names))
                elif has_out: reply="MOCKREPLY tool result received"
                elif 'RECALL' in last: reply="MOCKREPLY saw_secret="+str(any('SECRET-4242' in t for r,t in tx))
                else: reply="MOCKREPLY ok"
                chunks=[reply[i:i+12] for i in range(0,len(reply),12)]
                if 'SLOW' in last: chunks=[f"tick{i} " for i in range(60)]
                msg={"type":"message","id":"msg_1","role":"assistant","status":"in_progress","content":[]}
                self.ev({"type":"response.output_item.added","output_index":oi,"item":msg})
                self.ev({"type":"response.content_part.added","item_id":"msg_1","output_index":oi,"content_index":0,"part":{"type":"output_text","text":"","annotations":[]}})
                for ch in chunks:
                    self.ev({"type":"response.output_text.delta","item_id":"msg_1","output_index":oi,"content_index":0,"delta":ch,"logprobs":[]})
                    if 'SLOW' in last: time.sleep(0.5)
                full="".join(chunks)
                self.ev({"type":"response.output_text.done","item_id":"msg_1","output_index":oi,"content_index":0,"text":full,"logprobs":[]})
                part={"type":"output_text","text":full,"annotations":[]}
                self.ev({"type":"response.content_part.done","item_id":"msg_1","output_index":oi,"content_index":0,"part":part})
                msg=dict(msg,status="completed",content=[part])
                self.ev({"type":"response.output_item.done","output_index":oi,"item":msg}); out.append(msg)
        usage={"input_tokens":120,"input_tokens_details":{"cached_tokens":20},"output_tokens":15,"output_tokens_details":{"reasoning_tokens":5},"total_tokens":135}
        done=dict(base,status="completed",output=out,usage=usage)
        self.ev({"type":"response.completed","response":done})
        self.wfile.write(b"0\r\n\r\n"); self.wfile.flush()
http.server.ThreadingHTTPServer(('127.0.0.1',PORT),H).serve_forever()
