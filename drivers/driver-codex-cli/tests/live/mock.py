# Minimal OpenAI Responses API mock (SSE) for recording codex.
#
# Copied from the grok/codex recordings tarball's tools/mock.py and extended:
# - every POST body is written whole to <bodies-dir>/<n>.json, numbered from 000;
# - RATELIMIT in the task adds Codex's rate-limit headers (x-codex-primary-*, x-codex-secondary-*)
#   to the 200 answer and sends a `codex.rate_limits` SSE event first;
# - USAGELIMIT answers 429 with the `usage_limit_reached` body and the same headers, the shape
#   Codex reads as a spent plan;
# - CALL_TOOL=<name> for a tool Codex has deferred behind its client-side `tool_search` first
#   answers with a `tool_search_call` for <name>, then calls the tool the search returned;
# - ARGS=<json> (no spaces) beside CALL_TOOL makes the call's arguments that JSON instead of
#   {"text": "hi"}.
# Usage: python3 -I mock.py <port> <log> <bodies-dir>
import http.server, json, sys, time, itertools, re, os
PORT=int(sys.argv[1]); LOG=open(sys.argv[2],'a'); BODIES=sys.argv[3]
os.makedirs(BODIES, exist_ok=True)
seq=itertools.count(); bodyseq=itertools.count()
RESET_AT=2000000000
RATE_HEADERS=[("x-codex-primary-used-percent","42.5"),("x-codex-primary-window-minutes","300"),("x-codex-primary-reset-at",str(RESET_AT)),
              ("x-codex-secondary-used-percent","7.0"),("x-codex-secondary-window-minutes","10080"),("x-codex-secondary-reset-at",str(RESET_AT+86400)),
              ("x-codex-active-limit","codex")]
RATE_EVENT={"type":"codex.rate_limits","plan_type":"plus","rate_limits":{"primary":{"used_percent":42.5,"window_minutes":300,"reset_at":RESET_AT},
            "secondary":{"used_percent":7.0,"window_minutes":10080,"reset_at":RESET_AT+86400}}}
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
        if self.headers.get('content-encoding','').lower()=='zstd':
            log("BODY zstd-compressed, not decoded")
        with open(os.path.join(BODIES,'%03d.json'%next(bodyseq)),'wb') as f: f.write(raw)
        try: body=json.loads(raw)
        except Exception: body={}
        inp=body.get('input',[]); tools=body.get('tools',[])
        tx=texts(inp); users=[t for r,t in tx if r=='user']; last=' '.join(u for u in users if '<agent_reply>' not in u)
        has_out=any(isinstance(i,dict) and i.get('type') in('function_call_output','custom_tool_call_output','mcp_call') for i in (inp if isinstance(inp,list) else []))
        side = isinstance(body.get('tool_choice'),dict)
        log("POST",self.path,json.dumps({"model":body.get('model'),"side":side,"last_user":last[-200:],"tools":tool_names(tools),"has_output":has_out,
            "instructions":(body.get('instructions') or '')[:300],"roles":[ (i.get('role') or i.get('type')) for i in inp if isinstance(i,dict)]}))
        if not side: log("FULLBODY",raw.decode(errors='replace')[:800000])
        if 'USAGELIMIT' in last:
            b=json.dumps({"error":{"type":"usage_limit_reached","message":"mock usage limit","plan_type":"plus","resets_at":RESET_AT}}).encode()
            self.send_response(429)
            for k,v in RATE_HEADERS: self.send_header(k,v)
            self.send_header('x-codex-rate-limit-reached-type','primary')
            self.send_header('content-type','application/json'); self.send_header('content-length',str(len(b))); self.end_headers(); self.wfile.write(b); return
        if 'AUTH401' in last or 'QUOTA429' in last:
            code=401 if 'AUTH401' in last else 429
            b=json.dumps({"error":{"message":"mock "+str(code),"type":"invalid_request_error" if code==401 else "rate_limit_exceeded","code":None}}).encode()
            self.send_response(code); self.send_header('content-type','application/json'); self.send_header('content-length',str(len(b))); self.end_headers(); self.wfile.write(b); return
        self.send_response(200); self.send_header('content-type','text/event-stream'); self.send_header('transfer-encoding','chunked')
        if 'RATELIMIT' in last:
            for k,v in RATE_HEADERS: self.send_header(k,v)
        self.end_headers()
        if 'RATELIMIT' in last: self.ev(dict(RATE_EVENT))
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
            found=[]
            for it in (inp if isinstance(inp,list) else []):
                if isinstance(it,dict) and it.get('type')=='tool_search_output': found+=tool_names(it.get('tools'))
            searchable=any(isinstance(t,dict) and t.get('type')=='tool_search' for t in tools or [])
            names=names+found
            target=None
            m=re.search(r'CALL_TOOL(?:=(\S+))?',last); utname=None
            if m and m.group(1) and m.group(1).startswith('use_tool:'): utname=m.group(1).split(':',1)[1]
            if m and not has_out:
                want=('use_tool' if utname else m.group(1)) or 'echo_tool'
                cands=[x for x in names if want in (x or '')]
                target=cands[0] if cands else None
                if not target and searchable and not found:
                    ts={"type":"tool_search_call","id":"tsc_1","call_id":"call_s","status":"completed","execution":"client","arguments":{"query":want,"limit":5}}
                    self.ev({"type":"response.output_item.added","output_index":oi,"item":ts})
                    self.ev({"type":"response.output_item.done","output_index":oi,"item":ts}); out.append(ts); m=None
            if 'CALL_PATCH' in last and not has_out:
                ct={"type":"custom_tool_call","id":"ctc_1","call_id":"call_p","name":"apply_patch","input":"*** Begin Patch\n*** Add File: /tmp/rec/patched_outside.txt\n+written by apply_patch\n*** End Patch\n","status":"completed"}
                self.ev({"type":"response.output_item.added","output_index":oi,"item":dict(ct,input="")})
                self.ev({"type":"response.output_item.done","output_index":oi,"item":ct}); out.append(ct); target=None; m=None
            if 'CALL_ASK' in last and not has_out:
                fa={"type":"function_call","id":"fc_a","call_id":"call_a","name":"request_user_input","arguments":json.dumps({"questions":[{"id":"q1","header":"Pick","question":"Which?","options":[{"label":"A","description":"a"},{"label":"B","description":"b"}]}]}),"status":"completed"}
                self.ev({"type":"response.output_item.added","output_index":oi,"item":fa}); self.ev({"type":"response.output_item.done","output_index":oi,"item":fa}); out.append(fa); target=None; m=None
            if target:
                name=target; args={"text":"hi"}
                am=re.search(r'ARGS=(\{\S*\})',last)
                if am: args=json.loads(am.group(1))
                if target in ('use_tool',):
                    args={"tool_name":utname or "echo_tool","tool_input":{"text":"hi"}}
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
