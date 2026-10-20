import struct, sys, bisect
class DSC:
    def __init__(self, path):
        self.f = open(path, 'rb'); d = self.f
        hdr = d.read(0x100)
        self.magic = hdr[:16].rstrip(b'\0').decode()
        mo, mc, io, ic = struct.unpack_from('<IIII', hdr, 16)
        self.mappings = []
        for i in range(mc):
            d.seek(mo + 32*i); a, s, fo, mp, ip = struct.unpack('<QQQII', d.read(32))
            self.mappings.append((a, s, fo, ip))
        self.images = []
        for i in range(ic):
            d.seek(io + 32*i); a, mt, ino, pfo, pad = struct.unpack('<QQQII', d.read(32))
            d.seek(pfo); p = b''
            while True:
                c = d.read(1)
                if c == b'\0' or not c: break
                p += c
            self.images.append((a, p.decode()))
    def a2o(self, a):
        for ma, ms, fo, ip in self.mappings:
            if ma <= a < ma + ms: return fo + (a - ma)
        raise ValueError(hex(a))
    def read(self, a, n):
        self.f.seek(self.a2o(a)); return self.f.read(n)
    def cstr(self, a):
        self.f.seek(self.a2o(a)); out = b''
        while True:
            c = self.f.read(1)
            if c == b'\0' or not c: break
            out += c
        return out.decode('utf-8', 'replace')
    def image(self, name):
        for a, p in self.images:
            if p == name or p.endswith('/' + name): return a, p
        raise KeyError(name)
    def macho(self, base):
        h = self.read(base, 28)
        magic, cpu, sub, ft, ncmds, sizeofcmds, flags = struct.unpack('<7I', h)
        off = base + 28; cmds = []
        for i in range(ncmds):
            cmd, size = struct.unpack('<II', self.read(off, 8))
            cmds.append((cmd, off, size)); off += size
        return cmds
    def segments(self, base):
        segs = []
        for cmd, off, size in self.macho(base):
            if cmd == 1:  # LC_SEGMENT
                b = self.read(off, size)
                name = b[8:24].rstrip(b'\0').decode(); vmaddr, vmsize, fileoff, filesize, maxp, initp, nsects, fl = struct.unpack_from('<8I', b, 24)
                sects = []
                for j in range(nsects):
                    sb = b[56+68*j:56+68*(j+1)]
                    sn = sb[:16].rstrip(b'\0').decode(); sa, ss, so = struct.unpack_from('<III', sb, 32)
                    sects.append((name + ',' + sn, sa, ss))
                segs.append((name, vmaddr, vmsize, sects))
        return segs
    def symbols(self, base):
        for cmd, off, size in self.macho(base):
            if cmd == 2:  # LC_SYMTAB
                symoff, nsyms, stroff, strsize = struct.unpack('<4I', self.read(off + 8, 16))
                self.f.seek(symoff); raw = self.f.read(12 * nsyms)
                self.f.seek(stroff); strs = self.f.read(strsize)
                syms = []
                for i in range(nsyms):
                    strx, ntype, nsect, ndesc, val = struct.unpack_from('<IBBHI', raw, 12*i)
                    if ntype & 0xe0 or not val: continue  # skip stabs/undefined
                    e = strs.find(b'\0', strx); nm = strs[strx:e].decode('utf-8', 'replace')
                    syms.append((val, nm))
                syms.sort(); return syms
        return []
if __name__ == '__main__':
    c = DSC(sys.argv[1]); print(c.magic, 'mappings', [(hex(a), hex(s), hex(fo)) for a, s, fo, ip in c.mappings], 'images', len(c.images))
    for img in sys.argv[2].split(','):
        base, p = c.image(img); print('==', p, hex(base))
        syms = c.symbols(base); print('symbols', len(syms))
        pats = sys.argv[3].split('|')
        addrs = [s[0] for s in syms]
        for i, (a, nm) in enumerate(syms):
            if any(pt in nm for pt in pats):
                nxt = syms[i+1][0] if i+1 < len(syms) else a + 0x400
                print(f'{a:#010x} size={nxt-a:#x} {nm}')

def local_symbols(c, base):
    c.f.seek(72); lso, lss = struct.unpack('<QQ', c.f.read(16))
    c.f.seek(lso); nlo, nlc, sto, sts, eo, ec = struct.unpack('<6I', c.f.read(24))
    want = c.a2o(base); out = []
    for i in range(ec):
        c.f.seek(lso + eo + 12*i); dof, nstart, ncount = struct.unpack('<3I', c.f.read(12))
        if dof != want: continue
        c.f.seek(lso + nlo + 12*nstart); raw = c.f.read(12*ncount)
        c.f.seek(lso + sto); strs = c.f.read(sts)
        for j in range(ncount):
            strx, ntype, nsect, ndesc, val = struct.unpack_from('<IBBHI', raw, 12*j)
            if ntype & 0xe0 or not val: continue
            e = strs.find(b'\0', strx); out.append((val, strs[strx:e].decode('utf-8','replace')))
    out.sort(); return out

def _uleb(buf, p):
    r = 0; s = 0
    while True:
        b = buf[p]; p += 1; r |= (b & 0x7f) << s
        if not (b & 0x80): break
        s += 7
    return r, p

def exports(c, base):
    # read LC_DYLD_INFO(_ONLY) export trie
    eoff = esize = 0
    for cmd, off, size in c.macho(base):
        if cmd in (0x22, 0x80000022):  # LC_DYLD_INFO / _ONLY
            b = c.read(off, size)
            import struct as _s
            eoff, esize = _s.unpack_from('<II', b, 40)
            break
    if not eoff or not esize:
        return []
    # export_off is a file offset into the shared linkedit
    c.f.seek(eoff); trie = c.f.read(esize)
    names = []
    def walk(p, prefix):
        term_size, p = _uleb(trie, p)
        if term_size:
            # skip flags+addr (terminal info)
            pass
        p2 = p + term_size
        nchild = trie[p2]; p2 += 1
        for _ in range(nchild):
            s = b''
            while trie[p2] != 0:
                s += bytes([trie[p2]]); p2 += 1
            p2 += 1
            child, p2 = _uleb(trie, p2)
            walk(child, prefix + s)
        if term_size:
            names.append(prefix.decode('utf-8', 'replace'))
    walk(0, b'')
    return names

def emit_tbd(c, install_name, out_path):
    base, path = c.image(install_name)
    exps = exports(c, base)
    syms = [e for e in exps if not e.startswith('_OBJC')]
    objc = [e[len('_OBJC_CLASS_$_'):] for e in exps if e.startswith('_OBJC_CLASS_$_')]
    with open(out_path, 'w') as f:
        f.write('--- !tapi-tbd-v2\n')
        f.write('archs:           [ armv7 ]\n')
        f.write('platform:        ios\n')
        f.write('install-name:    %s\n' % install_name)
        f.write('current-version: 1.0.0\n')
        f.write('exports:\n')
        f.write('  - archs:       [ armv7 ]\n')
        if syms:
            f.write('    symbols:     [ %s ]\n' % ', '.join(syms))
        if objc:
            f.write('    objc-classes: [ %s ]\n' % ', '.join(objc))
        f.write('...\n')
    return len(exps), path
