#!/usr/bin/env python3
# Extract RT_GROUP_ICON groups from a packed PE and write .ico files.
# Verified on GameViewer.exe: counts at +12/+14, ID entries at +16, DataEntry
# offsets are RVAs (image base 0x140000000), .rsrc RawPtr 0x4598e00 / VA 0x45b7000.
import struct, sys, os

path, outdir = sys.argv[1], sys.argv[2]
data = open(path, 'rb').read()
pe = struct.unpack_from('<I', data, 0x3c)[0]
nsec = struct.unpack_from('<H', data, pe + 6)[0]
opt = struct.unpack_from('<H', data, pe + 20)[0]
imgbase = struct.unpack_from('<Q', data, pe + 24 + 24)[0]
secs = []
rsrc_off = None
for i in range(nsec):
    s = pe + 24 + opt + i * 40
    name = data[s:s + 8].rstrip(b'\0').decode()
    vs, va, rs, rp = struct.unpack_from('<IIII', data, s + 8)
    secs.append((va, vs, rs, rp))
    if name == '.rsrc':
        rsrc_off = rp
assert rsrc_off is not None, 'no .rsrc'

def rva2file(rva):
    for va, vs, rs, rp in secs:
        if va <= rva < va + max(vs, rs):
            return rp + (rva - va)
    raise ValueError(f'rva {hex(rva)} not in any section')

def entries(diroff):
    d = diroff & 0x7fffffff
    n_id = struct.unpack_from('<H', data, d + 14)[0]
    return [struct.unpack_from('<II', data, d + 16 + e * 8) for e in range(n_id)]

def leaves(diroff, item_id=None):
    for nid, o in entries(diroff):
        if o & 0x80000000:
            yield from leaves(rsrc_off + (o & 0x7fffffff), nid)
        else:
            rva, size = struct.unpack_from('<II', data, rsrc_off + (o & 0x7fffffff))
            fo = rva2file(rva)  # values are already VA-based (land in .rsrc VA range)
            yield (item_id if item_id is not None else nid), data[fo:fo + size]

RT_ICON, RT_GROUP_ICON = 3, 14
icons, groups = {}, {}
for tid, o in entries(rsrc_off):
    if tid in (RT_ICON, RT_GROUP_ICON):
        for eid, blob in leaves(rsrc_off + (o & 0x7fffffff)):
            (icons if tid == RT_ICON else groups)[eid] = blob
print(f'icons={len(icons)} groups={len(groups)}')

os.makedirs(outdir, exist_ok=True)
for gid, gdir in groups.items():
    _, rtype, count = struct.unpack_from('<HHH', gdir, 0)
    if rtype != 1:
        continue
    ents, parts = [], []
    for i in range(count):
        # ICONDIRENTRY is 14 bytes: w,h,colors,reserved,planes(2),bits(2),bytesInRes(4),id(2)
        w, h, nc, resv, planes, bits, bsize, id_ = struct.unpack_from('<BBBBHHIH', gdir, 6 + i * 14)
        if id_ not in icons:
            print(f'group {gid}: icon {id_} missing'); continue
        ents.append(struct.pack('<BBBBHHIH', w, h, nc, resv, planes, bits, len(icons[id_]), len(ents)))
        parts.append(icons[id_])
    out = struct.pack('<HHH', 0, 1, len(ents)) + b''.join(ents) + b''.join(parts)
    mx = max(ents, key=lambda e: e[0] * e[1])
    fn = os.path.join(outdir, f'app-{gid}-{mx[0]}x{mx[1]}.ico')
    open(fn, 'wb').write(out)
    print('wrote', fn, len(out), 'bytes')
