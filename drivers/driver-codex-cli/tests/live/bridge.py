# Mirrors murmur's claude_bridge.rs: one request per connection, connection: close,
# GET -> 405, DELETE -> 400, missing/wrong bearer -> 401, notifications -> 202, plain JSON responses.
#
# Copied from the grok/codex recordings tarball's tools/bridge.py and extended with two tools:
# - picture_tool answers the way murmur's bridge does when a tool returns an image: a text item,
#   then an MCP `image` item with the PNG at <png> as standard padded base64;
# - fail_tool answers with `isError: true`.
# Usage: python3 -I bridge.py <port> <token> <log> <png>
import socket, threading, json, sys, uuid, base64
PORT=int(sys.argv[1]); TOKEN=sys.argv[2]; LOG=open(sys.argv[3],'a'); PNG=sys.argv[4]
SID=str(uuid.uuid4())
TOOLS=[{"name":"echo_tool","description":"Echo the given text back.","inputSchema":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}},
       {"name":"session-status","description":"Report this session's status.","inputSchema":{"type":"object","properties":{}}},
       {"name":"picture_tool","description":"Returns a picture.","inputSchema":{"type":"object","properties":{"text":{"type":"string"}}}},
       {"name":"fail_tool","description":"Always fails.","inputSchema":{"type":"object","properties":{"text":{"type":"string"}}}}]
def picture_result():
    with open(PNG,'rb') as f: data=f.read()
    text=f"PICTURE-TOOL-RAN\n[image attached: picture.png, image/png, {len(data)} bytes]"
    return {"content":[{"type":"text","text":text},{"type":"image","data":base64.b64encode(data).decode('ascii'),"mimeType":"image/png"}],"isError":False}
def log(*a):
    LOG.write(" ".join(str(x) for x in a)+"\n"); LOG.flush()
def resp(c,status,body=None,ct=None,extra=None):
    body=body or ""
    r=f"HTTP/1.1 {status}\r\n"
    if ct: r+=f"content-type: {ct}\r\n"
    if extra: r+=extra+"\r\n"
    r+=f"content-length: {len(body.encode())}\r\nconnection: close\r\n\r\n"+body
    c.sendall(r.encode()); 
    try: c.shutdown(socket.SHUT_WR)
    except: pass
def handle(c):
    f=c.makefile('rb')
    line=f.readline().decode(errors='replace'); method=line.split(' ')[0] if line else ''
    path=line.split(' ')[1] if len(line.split(' '))>1 else ''
    cl=0; auth=False; hdrs={}
    while True:
        h=f.readline().decode(errors='replace')
        if not h or not h.strip(): break
        k,_,v=h.partition(':'); hdrs[k.strip().lower()]=v.strip()
        lo=h.lower().rstrip()
        if lo.startswith('content-length:'): cl=int(lo.split(':',1)[1].strip() or 0)
        elif lo.startswith('authorization:'): auth = lo.split(':',1)[1].strip()==f"bearer {TOKEN}"
    log("HTTP",method,path,"auth" if auth else "NOAUTH", json.dumps({k:v for k,v in hdrs.items() if k!='authorization'}))
    if method=='GET': return resp(c,"405 Method Not Allowed")
    if method=='DELETE': return resp(c,"400 Bad Request")
    if not auth: return resp(c,"401 Unauthorized")
    body=f.read(cl) if cl else b''
    try: req=json.loads(body)
    except Exception: return resp(c,"400 Bad Request")
    log("RPC",json.dumps(req)[:600])
    if 'id' not in req: return resp(c,"202 Accepted")
    m=req.get('method'); i=req['id']
    def ok(result,extra=None): resp(c,"200 OK",json.dumps({"jsonrpc":"2.0","id":i,"result":result}),"application/json",extra)
    if m=='initialize':
        pv=(req.get('params') or {}).get('protocolVersion','2024-11-05')
        ok({"protocolVersion":pv,"capabilities":{"tools":{}},"serverInfo":{"name":"murmur-bridge","version":"0.10.0"}},f"Mcp-Session-Id: {SID}")
    elif m=='tools/list': ok({"tools":TOOLS})
    elif m=='tools/call':
        p=req.get('params') or {}; a=p.get('arguments') or {}; name=p.get('name')
        if name=='picture_tool': ok(picture_result())
        elif name=='fail_tool': ok({"content":[{"type":"text","text":f"fail_tool refused: {a.get('text','')}"}],"isError":True})
        else: ok({"content":[{"type":"text","text":f"echoed: {a.get('text','')}"}],"isError":False})
    else: resp(c,"200 OK",json.dumps({"jsonrpc":"2.0","id":i,"error":{"code":-32601,"message":"method not found"}}),"application/json")
s=socket.socket(); s.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1); s.bind(('127.0.0.1',PORT)); s.listen(64)
while True:
    c,_=s.accept(); threading.Thread(target=lambda c=c:(handle(c),c.close()),daemon=True).start()
