//! Minimal deterministic CBOR — the Rust binding of the frozen wire substrate.
//! Byte-for-byte identical to taut/src/taut/wire/cbor.py and trial/ts/src/cbor.ts:
//! the same tiny subset (int, bytes, text, array, int-keyed map, bool, null) in
//! core deterministic encoding (definite length, shortest-form ints, ascending
//! map keys). Hand-rolled, zero dependencies.

#[derive(Clone, Debug, PartialEq)]
pub enum Cbor {
    Int(i64),
    Bytes(Vec<u8>),
    Text(String),
    Array(Vec<Cbor>),
    Map(Vec<(i64, Cbor)>),
    Bool(bool),
    Null,
}

impl Cbor {
    /// Value for an integer map key (panics if absent / not a map).
    pub fn get(&self, key: i64) -> &Cbor {
        if let Cbor::Map(m) = self {
            for (k, v) in m {
                if *k == key {
                    return v;
                }
            }
        }
        panic!("no map key {}", key);
    }
    pub fn int(&self) -> i64 {
        if let Cbor::Int(n) = self { *n } else { panic!("not an int") }
    }
    pub fn text(&self) -> String {
        if let Cbor::Text(s) = self { s.clone() } else { panic!("not text") }
    }
    pub fn bytes(&self) -> Vec<u8> {
        if let Cbor::Bytes(b) = self { b.clone() } else { panic!("not bytes") }
    }
    pub fn boolean(&self) -> bool {
        if let Cbor::Bool(b) = self { *b } else { panic!("not a bool") }
    }
    pub fn array(&self) -> &[Cbor] {
        if let Cbor::Array(a) = self { a } else { panic!("not an array") }
    }
    pub fn is_null(&self) -> bool {
        matches!(self, Cbor::Null)
    }
    /// All (key, value) pairs of a map (empty if not a map). Used to capture
    /// forward-compat residual: tags the schema doesn't name.
    pub fn map_entries(&self) -> &[(i64, Cbor)] {
        if let Cbor::Map(m) = self { m } else { &[] }
    }
}

fn head(out: &mut Vec<u8>, major: u8, n: u64) {
    let mt = major << 5;
    if n < 24 {
        out.push(mt | n as u8);
    } else if n < 0x100 {
        out.push(mt | 24);
        out.push(n as u8);
    } else if n < 0x1_0000 {
        out.push(mt | 25);
        out.extend_from_slice(&(n as u16).to_be_bytes());
    } else if n < 0x1_0000_0000 {
        out.push(mt | 26);
        out.extend_from_slice(&(n as u32).to_be_bytes());
    } else {
        out.push(mt | 27);
        out.extend_from_slice(&n.to_be_bytes());
    }
}

pub fn encode(v: &Cbor) -> Vec<u8> {
    let mut out = Vec::new();
    enc(v, &mut out);
    out
}

fn enc(v: &Cbor, out: &mut Vec<u8>) {
    match v {
        Cbor::Int(n) => {
            if *n >= 0 {
                head(out, 0, *n as u64);
            } else {
                head(out, 1, (-1 - *n) as u64);
            }
        }
        Cbor::Bytes(b) => {
            head(out, 2, b.len() as u64);
            out.extend_from_slice(b);
        }
        Cbor::Text(s) => {
            let b = s.as_bytes();
            head(out, 3, b.len() as u64);
            out.extend_from_slice(b);
        }
        Cbor::Array(a) => {
            head(out, 4, a.len() as u64);
            for x in a {
                enc(x, out);
            }
        }
        Cbor::Map(m) => {
            let mut entries: Vec<&(i64, Cbor)> = m.iter().collect();
            entries.sort_by_key(|(k, _)| *k); // deterministic: ascending keys
            head(out, 5, m.len() as u64);
            for (k, val) in entries {
                head(out, 0, *k as u64);
                enc(val, out);
            }
        }
        Cbor::Bool(b) => out.push(if *b { 0xf5 } else { 0xf4 }),
        Cbor::Null => out.push(0xf6),
    }
}

pub fn decode(data: &[u8]) -> Cbor {
    let (v, off) = dec(data, 0);
    assert_eq!(off, data.len(), "trailing bytes after top-level CBOR item");
    v
}

fn read_arg(data: &[u8], off: usize, info: u8) -> (u64, usize) {
    match info {
        n if n < 24 => (n as u64, off),
        24 => (data[off] as u64, off + 1),
        25 => (u16::from_be_bytes([data[off], data[off + 1]]) as u64, off + 2),
        26 => (
            u32::from_be_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]]) as u64,
            off + 4,
        ),
        27 => {
            let mut b = [0u8; 8];
            b.copy_from_slice(&data[off..off + 8]);
            (u64::from_be_bytes(b), off + 8)
        }
        _ => panic!("unsupported additional-info {}", info),
    }
}

fn dec(data: &[u8], off: usize) -> (Cbor, usize) {
    let initial = data[off];
    let major = initial >> 5;
    let info = initial & 0x1f;
    let off = off + 1;
    match major {
        0 => {
            let (n, o) = read_arg(data, off, info);
            (Cbor::Int(n as i64), o)
        }
        1 => {
            let (n, o) = read_arg(data, off, info);
            (Cbor::Int(-1 - n as i64), o)
        }
        2 => {
            let (n, o) = read_arg(data, off, info);
            let n = n as usize;
            (Cbor::Bytes(data[o..o + n].to_vec()), o + n)
        }
        3 => {
            let (n, o) = read_arg(data, off, info);
            let n = n as usize;
            (Cbor::Text(String::from_utf8(data[o..o + n].to_vec()).unwrap()), o + n)
        }
        4 => {
            let (n, mut o) = read_arg(data, off, info);
            let mut a = Vec::new();
            for _ in 0..n {
                let (v, o2) = dec(data, o);
                a.push(v);
                o = o2;
            }
            (Cbor::Array(a), o)
        }
        5 => {
            let (n, mut o) = read_arg(data, off, info);
            let mut m = Vec::new();
            for _ in 0..n {
                let (k, o2) = dec(data, o);
                let (v, o3) = dec(data, o2);
                let ki = match k {
                    Cbor::Int(i) => i,
                    _ => panic!("non-integer map key"),
                };
                m.push((ki, v));
                o = o3;
            }
            (Cbor::Map(m), o)
        }
        7 => match info {
            20 => (Cbor::Bool(false), off),
            21 => (Cbor::Bool(true), off),
            22 => (Cbor::Null, off),
            _ => panic!("unsupported simple value {}", info),
        },
        _ => panic!("unsupported major type {}", major),
    }
}
