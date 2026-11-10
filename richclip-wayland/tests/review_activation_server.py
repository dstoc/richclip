"""Minimal wire fixture for review_activation_dispatch.rs; no compositor."""
import array
import os
import selectors
import socket
import struct
import sys


def word(n):
    return struct.pack("=I", n)


def string(s):
    data = s.encode() + b"\0"
    return word(len(data)) + data + b"\0" * (-len(data) % 4)


def event(conn, obj, opcode, body=b"", fd=None):
    data = word(obj) + word(((len(body) + 8) << 16) | opcode) + body
    if fd is None:
        conn.sendall(data)
    else:
        conn.sendmsg([data], [(socket.SOL_SOCKET, socket.SCM_RIGHTS, array.array("i", [fd]))])


listener = socket.socket(socket.AF_UNIX)
listener.bind(sys.argv[1])
listener.listen()
print("ready", flush=True)
sel = selectors.DefaultSelector()
sel.register(listener, selectors.EVENT_READ)
clients = []
pending = None
post_barrier = False
source = None


def forward():
    global pending
    if pending is not None and post_barrier:
        event(clients[1]["conn"], source, 0, string("text/plain"), pending)
        os.close(pending)
        pending = None
        print("send forwarded during activation barrier", flush=True)


while True:
    for key, _ in sel.select():
        if key.fileobj is listener:
            conn, _ = listener.accept()
            state = {"conn": conn, "buf": b"", "fds": [], "syncs": 0,
                     "role": len(clients), "objects": {1: "display"}}
            clients.append(state)
            sel.register(conn, selectors.EVENT_READ, state)
            continue
        c = key.data
        data, ancillary, _, _ = c["conn"].recvmsg(65536, socket.CMSG_SPACE(16))
        if not data:
            sys.exit(0)
        c["buf"] += data
        for level, kind, data in ancillary:
            if level == socket.SOL_SOCKET and kind == socket.SCM_RIGHTS:
                fds = array.array("i")
                fds.frombytes(data)
                c["fds"].extend(fds)
        while len(c["buf"]) >= 8:
            obj, size_op = struct.unpack("=II", c["buf"][:8])
            size, op = size_op >> 16, size_op & 65535
            if len(c["buf"]) < size:
                break
            body, c["buf"] = c["buf"][8:size], c["buf"][size:]
            value = lambda offset=0: struct.unpack("=I", body[offset:offset+4])[0]
            interface = c["objects"].get(obj)
            if interface == "display" and op == 1:
                registry = value()
                c["objects"][registry] = "registry"
                for name, iface, version in [(1, "zwlr_data_control_manager_v1", 2), (2, "wl_seat", 8)]:
                    event(c["conn"], registry, 0, word(name) + string(iface) + word(version))
            elif interface == "registry":
                length = value(4)
                iface = body[8:8+length-1].decode()
                new_id = value(8 + ((length + 3) // 4) * 4 + 4)
                c["objects"][new_id] = iface
            elif interface == "display" and op == 0:
                c["syncs"] += 1
                # Initial enumeration, pre-selection barrier, activation barrier.
                if c["role"] == 0 and c["syncs"] == 3:
                    post_barrier = True
                    forward()
                callback = value()
                event(c["conn"], callback, 0, word(0))
                event(c["conn"], 1, 1, word(callback))
            elif interface == "zwlr_data_control_manager_v1":
                new_id = value()
                c["objects"][new_id] = "source" if op == 0 else "device"
                if op == 0:
                    source = new_id
                else:
                    c["device"] = new_id
            elif interface == "device" and op == 0 and c["role"] == 1:
                cap = clients[0]
                offer = 0xff000000
                cap["objects"][offer] = "offer"
                event(cap["conn"], cap["device"], 0, word(offer))
                event(cap["conn"], offer, 0, string("text/plain"))
                event(cap["conn"], cap["device"], 1, word(offer))
            elif interface == "offer" and op == 0:
                pending = c["fds"].pop(0)
                forward()
