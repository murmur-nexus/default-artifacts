# Mirrors murmur's claude_bridge.rs: one request per connection, connection: close,
# GET -> 405, missing/wrong bearer -> 401, notifications -> 202, plain JSON responses.
# Copied from the grok/codex recordings' tools/bridge.py. Added for driver-grok-build:
# `image_tool`, which answers the way murmur's bridge answers a call whose tool returned one
# attachment (claude_bridge.rs `tool_result()`): a text item carrying the tool's output and the
# `[image attached: ...]` line, then an MCP `image` item with the PNG as standard padded base64.
# The PNG is drawn here, deterministically, so its sha256 is fixed (printed to the log at start).
# Usage: python3 -I bridge.py <port> <bearer token> <log>
import socket, threading, json, sys, uuid, base64, binascii, hashlib, struct, zlib
PORT=int(sys.argv[1]); TOKEN=sys.argv[2]; LOG=open(sys.argv[3],'a')
SID=str(uuid.uuid4())
TOOLS=[{"name":"echo_tool","description":"Echo the given text back.","inputSchema":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}},
       {"name":"session-status","description":"Report this session's status.","inputSchema":{"type":"object","properties":{}}},
       {"name":"image_tool","description":"Return a picture.","inputSchema":{"type":"object","properties":{"text":{"type":"string"}}}}]
def png(width=24, height=16):
    # A real PNG: a red left half and a blue right half, 8-bit RGB, no filter.
    def chunk(kind, data):
        body = kind + data
        return struct.pack(">I", len(data)) + body + struct.pack(">I", binascii.crc32(body) & 0xFFFFFFFF)
    rows = b"".join(b"\x00" + b"".join(bytes((200,30,30)) if x < width//2 else bytes((30,60,200)) for x in range(width)) for _ in range(height))
    return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress(rows, 9)) + chunk(b"IEND", b""))
PNG=png()
def image_result():
    text=f"IMAGE-TOOL-RAN\n[image attached: picture.png, image/png, {len(PNG)} bytes]"
    return {"content":[{"type":"text","text":text},{"type":"image","data":base64.b64encode(PNG).decode("ascii"),"mimeType":"image/png"}],"isError":False}
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
        p=req.get('params') or {}; a=p.get('arguments') or {}
        if p.get('name')=='image_tool': ok(image_result())
        else: ok({"content":[{"type":"text","text":f"echoed: {a.get('text','')}"}],"isError":False})
    else: resp(c,"200 OK",json.dumps({"jsonrpc":"2.0","id":i,"error":{"code":-32601,"message":"method not found"}}),"application/json")
log("PNG",len(PNG),"bytes sha256",hashlib.sha256(PNG).hexdigest(),"base64",len(base64.b64encode(PNG)),"chars")
s=socket.socket(); s.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1); s.bind(('127.0.0.1',PORT)); s.listen(64)
while True:
    c,_=s.accept(); threading.Thread(target=lambda c=c:(handle(c),c.close()),daemon=True).start()
