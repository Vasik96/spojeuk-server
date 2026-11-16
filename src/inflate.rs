//! Minimální gzip/deflate dekodér (jen std). DPMD posílá TrafficState.js ~500 KB, s gzipem ~35 KB;
//! ureq je tu bez funkce `gzip`, tak se rozbaluje tady (bez nové závislosti a bez změny Cargo.lock).
//! Postup podle zlib `puff.c`: kanonické Huffmanovy kódy, bez rychlostních triků, pro desítky KB stačí.

const MAXBITS: usize = 15;

const LBASE: [u16; 29] = [3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131, 163, 195, 227, 258];
const LEXT: [u8; 29] = [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0];
const DBASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537, 2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DEXT: [u8; 30] = [0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13, 13];
const CLORDER: [usize; 19] = [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15];

struct Bits<'a> {
    src: &'a [u8],
    pos: usize,
    buf: u32,
    cnt: u32,
}

impl<'a> Bits<'a> {
    fn bits(&mut self, need: u32) -> Result<u32, String> {
        let mut val = self.buf;
        while self.cnt < need {
            let b = *self.src.get(self.pos).ok_or("gzip: nečekaný konec dat")?;
            self.pos += 1;
            val |= (b as u32) << self.cnt;
            self.cnt += 8;
        }
        self.buf = if need >= 32 { 0 } else { val >> need };
        self.cnt -= need;
        Ok(val & ((1u32 << need) - 1))
    }
}

struct Huffman {
    count: [u16; MAXBITS + 1],
    symbol: Vec<u16>,
}

impl Huffman {
    fn new(lengths: &[u8]) -> Result<Huffman, String> {
        let mut count = [0u16; MAXBITS + 1];
        for &l in lengths {
            count[l as usize] += 1;
        }
        let mut left: i32 = 1;
        for len in 1..=MAXBITS {
            left <<= 1;
            left -= count[len] as i32;
            if left < 0 {
                return Err("gzip: přeplněný Huffmanův strom".into());
            }
        }
        let mut offs = [0u16; MAXBITS + 2];
        for len in 1..=MAXBITS {
            offs[len + 1] = offs[len] + count[len];
        }
        let mut symbol = vec![0u16; lengths.len()];
        for (sym, &l) in lengths.iter().enumerate() {
            if l != 0 {
                symbol[offs[l as usize] as usize] = sym as u16;
                offs[l as usize] += 1;
            }
        }
        Ok(Huffman { count, symbol })
    }

    fn decode(&self, s: &mut Bits) -> Result<usize, String> {
        let (mut code, mut first, mut index) = (0i32, 0i32, 0i32);
        for len in 1..=MAXBITS {
            code |= s.bits(1)? as i32;
            let count = self.count[len] as i32;
            if code - count < first {
                return Ok(self.symbol[(index + (code - first)) as usize] as usize);
            }
            index += count;
            first += count;
            first <<= 1;
            code <<= 1;
        }
        Err("gzip: neplatný Huffmanův kód".into())
    }
}

fn codes(s: &mut Bits, out: &mut Vec<u8>, lit: &Huffman, dist: &Huffman, limit: usize) -> Result<(), String> {
    loop {
        let sym = lit.decode(s)?;
        if sym < 256 {
            out.push(sym as u8);
        } else if sym == 256 {
            return Ok(());
        } else {
            let sym = sym - 257;
            if sym >= 29 {
                return Err("gzip: neplatná délka".into());
            }
            let len = LBASE[sym] as usize + s.bits(LEXT[sym] as u32)? as usize;
            let ds = dist.decode(s)?;
            if ds >= 30 {
                return Err("gzip: neplatná vzdálenost".into());
            }
            let d = DBASE[ds] as usize + s.bits(DEXT[ds] as u32)? as usize;
            if d > out.len() {
                return Err("gzip: vzdálenost před začátkem výstupu".into());
            }
            for _ in 0..len {
                let b = out[out.len() - d];
                out.push(b);
            }
        }
        if out.len() > limit {
            return Err("gzip: výstup je větší než limit".into());
        }
    }
}

fn fixed() -> Result<(Huffman, Huffman), String> {
    let mut l = [0u8; 288];
    for (i, v) in l.iter_mut().enumerate() {
        *v = match i {
            0..=143 => 8,
            144..=255 => 9,
            256..=279 => 7,
            _ => 8,
        };
    }
    Ok((Huffman::new(&l)?, Huffman::new(&[5u8; 30])?))
}

fn dynamic(s: &mut Bits) -> Result<(Huffman, Huffman), String> {
    let nlen = s.bits(5)? as usize + 257;
    let ndist = s.bits(5)? as usize + 1;
    let ncode = s.bits(4)? as usize + 4;
    if nlen > 286 || ndist > 30 {
        return Err("gzip: příliš mnoho kódů".into());
    }
    let mut lengths = [0u8; 320];
    for &o in CLORDER.iter().take(ncode) {
        lengths[o] = s.bits(3)? as u8;
    }
    let lencode = Huffman::new(&lengths[..19])?;
    let mut lengths = [0u8; 320];
    let mut idx = 0;
    while idx < nlen + ndist {
        let sym = lencode.decode(s)?;
        if sym < 16 {
            lengths[idx] = sym as u8;
            idx += 1;
        } else {
            let (prev, rep) = match sym {
                16 => {
                    if idx == 0 {
                        return Err("gzip: opakování bez předchozí délky".into());
                    }
                    (lengths[idx - 1], 3 + s.bits(2)? as usize)
                }
                17 => (0, 3 + s.bits(3)? as usize),
                _ => (0, 11 + s.bits(7)? as usize),
            };
            if idx + rep > nlen + ndist {
                return Err("gzip: příliš mnoho délek".into());
            }
            for _ in 0..rep {
                lengths[idx] = prev;
                idx += 1;
            }
        }
    }
    if lengths[256] == 0 {
        return Err("gzip: chybí kód konce bloku".into());
    }
    Ok((Huffman::new(&lengths[..nlen])?, Huffman::new(&lengths[nlen..nlen + ndist])?))
}

/// Surový deflate proud.
pub fn inflate(src: &[u8], limit: usize) -> Result<Vec<u8>, String> {
    let mut s = Bits { src, pos: 0, buf: 0, cnt: 0 };
    let mut out: Vec<u8> = Vec::with_capacity(src.len() * 12);
    loop {
        let last = s.bits(1)?;
        match s.bits(2)? {
            0 => {
                s.buf = 0;
                s.cnt = 0;
                let p = s.pos;
                if p + 4 > src.len() {
                    return Err("gzip: useknutý blok".into());
                }
                let len = src[p] as usize | (src[p + 1] as usize) << 8;
                let nlen = src[p + 2] as usize | (src[p + 3] as usize) << 8;
                if len != (!nlen & 0xffff) {
                    return Err("gzip: poškozená délka bloku".into());
                }
                if p + 4 + len > src.len() {
                    return Err("gzip: useknutý blok".into());
                }
                out.extend_from_slice(&src[p + 4..p + 4 + len]);
                s.pos = p + 4 + len;
                if out.len() > limit {
                    return Err("gzip: výstup je větší než limit".into());
                }
            }
            1 => {
                let (l, d) = fixed()?;
                codes(&mut s, &mut out, &l, &d, limit)?;
            }
            2 => {
                let (l, d) = dynamic(&mut s)?;
                codes(&mut s, &mut out, &l, &d, limit)?;
            }
            _ => return Err("gzip: neplatný typ bloku".into()),
        }
        if last == 1 {
            return Ok(out);
        }
    }
}

/// gzip (RFC 1952): hlavička + deflate; patička (CRC, délka) se nekontroluje, useknutý proud ale selže.
pub fn gunzip(src: &[u8], limit: usize) -> Result<Vec<u8>, String> {
    if src.len() < 18 || src[0] != 0x1f || src[1] != 0x8b || src[2] != 8 {
        return Err("gzip: chybná hlavička".into());
    }
    let flg = src[3];
    let mut p = 10;
    if flg & 4 != 0 {
        let xlen = *src.get(p).ok_or("gzip: useknutá hlavička")? as usize | (*src.get(p + 1).ok_or("gzip: useknutá hlavička")? as usize) << 8;
        p += 2 + xlen;
    }
    for bit in [8u8, 16u8] {
        if flg & bit != 0 {
            while *src.get(p).ok_or("gzip: useknutá hlavička")? != 0 {
                p += 1;
            }
            p += 1;
        }
    }
    if flg & 2 != 0 {
        p += 2;
    }
    if p >= src.len() {
        return Err("gzip: useknutá hlavička".into());
    }
    inflate(&src[p..], limit)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_garbage() {
        assert!(gunzip(b"nejsem gzip nejsem gzip nejsem", 1000).is_err());
        assert!(inflate(&[0xff, 0xff, 0xff], 1000).is_err());
    }
}
