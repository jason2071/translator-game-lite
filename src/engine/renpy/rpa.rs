//! Read-only reader for Ren'Py `.rpa` archives (RPA-3.0, with graceful
//! handling of the common obfuscation variants).
//!
//! The index is a zlib-compressed Python pickle (protocol 1) mapping file
//! names to `(offset, length, prefix)` records. We parse the pickle subset
//! Ren'Py produces instead of pulling in a full pickle implementation.
//!
//! Member data: seek to `offset`, read `length` bytes, zlib-decompress,
//! prepend `prefix`. Archives may be huge, so everything is seek-based.

use std::collections::BTreeMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};

#[derive(Debug, Clone)]
pub struct RpaMember {
    pub offset: u64,
    pub length: u64,
    pub prefix: Vec<u8>,
}

/// A parsed archive index.
#[derive(Debug, Default)]
pub struct RpaIndex {
    pub members: BTreeMap<String, RpaMember>,
}

impl RpaIndex {
    /// Open an archive and parse its index. Only the header and index are
    /// read; member data is extracted lazily via [`Self::read_member`].
    pub fn open(path: &Path) -> Result<Self> {
        let mut file = std::fs::File::open(path)
            .with_context(|| format!("opening archive {}", path.display()))?;

        let mut header = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            match file.read(&mut byte)? {
                0 => bail!("{}: unexpected end of header", path.display()),
                _ => {
                    if byte[0] == b'\n' {
                        break;
                    }
                    header.push(byte[0]);
                    if header.len() > 128 {
                        bail!("{}: not an RPA archive (header too long)", path.display());
                    }
                }
            }
        }
        let header = String::from_utf8_lossy(&header).to_string();
        let parts: Vec<&str> = header.split_whitespace().collect();
        if parts.first() != Some(&"RPA-3.0") {
            bail!("{}: unsupported archive format (only RPA-3.0)", path.display());
        }
        let index_offset = u64::from_str_radix(
            parts.get(1).ok_or_else(|| anyhow!("missing index offset"))?,
            16,
        )?;
        let key = parts
            .get(2)
            .map(|k| u64::from_str_radix(k, 16))
            .transpose()
            .context("bad index key")?
            .unwrap_or(0);

        file.seek(SeekFrom::Start(index_offset))?;
        let mut blob = Vec::new();
        file.read_to_end(&mut blob)?;
        let plain = decompress_variants(&blob, key)
            .ok_or_else(|| anyhow!("{}: could not decode the archive index", path.display()))?;

        let value = pickle_from_bytes(&plain).context("parsing the archive index pickle")?;
        let dict = as_dict(&value).ok_or_else(|| anyhow!("index pickle is not a mapping"))?;

        let mut members = BTreeMap::new();
        for (name, records) in dict {
            let Some(name) = as_str(&name) else { continue };
            let Some(records) = as_list(&records) else { continue };
            for record in records {
                let Some(record) = as_list(&record) else { continue };
                if record.len() < 2 {
                    continue;
                }
                let (Some(offset), Some(length)) = (as_int(&record[0]), as_int(&record[1])) else {
                    continue;
                };
                let prefix = record
                    .get(2)
                    .and_then(as_bytes)
                    .unwrap_or_default();
                // RPA-3.0 stores offsets and lengths XORed with the key.
                let offset = (offset as u64) ^ key;
                let length = (length as u64) ^ key;
                if length == 0 {
                    continue;
                }
                members.insert(
                    name.clone(),
                    RpaMember {
                        offset,
                        length,
                        prefix,
                    },
                );
                break; // first record wins (archives may repeat names)
            }
        }
        Ok(Self { members })
    }

    /// Names of members with the given extension (dot included), lowercase.
    pub fn names_with_extension(&self, extension: &str) -> Vec<String> {
        self.members
            .keys()
            .filter(|n| n.to_lowercase().ends_with(extension))
            .cloned()
            .collect()
    }

    /// Decompress one member into a string.
    ///
    /// Members are usually zlib-compressed, but some archivers store them
    /// raw; a UTF-8 BOM is stripped either way.
    pub fn read_member(&self, path: &Path, name: &str) -> Result<String> {
        let member = self
            .members
            .get(name)
            .ok_or_else(|| anyhow!("{}: no member named {name}", path.display()))?;
        let mut file = std::fs::File::open(path)?;
        file.seek(SeekFrom::Start(member.offset))?;
        let mut block = vec![0u8; member.length as usize];
        file.read_exact(&mut block)?;

        let mut content = member.prefix.clone();
        let zlib_ok = flate2::read::ZlibDecoder::new(&block[..])
            .read_to_end(&mut content)
            .is_ok()
            && content.len() > member.prefix.len();
        if !zlib_ok {
            content = member.prefix.clone();
            content.extend_from_slice(&block);
        }
        if content.starts_with(&[0xEF, 0xBB, 0xBF]) {
            content.drain(0..3);
        }
        Ok(String::from_utf8_lossy(&content).into_owned())
    }
}

/// The index blob may be plain zlib or lightly obfuscated; try the known
/// variants and return the first that decompresses.
fn decompress_variants(blob: &[u8], key: u64) -> Option<Vec<u8>> {
    // Variant 0: not obfuscated at all (common for archives made by tools).
    if let Ok(out) = try_zlib(blob) {
        return Some(out);
    }
    // Variant 1 (Ren'Py standard): first byte untouched, then a repeating
    // big-endian 4-byte key.
    let key_bytes = key.to_be_bytes();
    let mut out = Vec::with_capacity(blob.len());
    for (i, b) in blob.iter().enumerate() {
        out.push(if i == 0 {
            *b
        } else {
            b ^ key_bytes[(i - 1) % key_bytes.len()]
        });
    }
    if let Ok(decoded) = try_zlib(&out) {
        return Some(decoded);
    }
    // Variant 2: every byte XORed with the cycling key.
    let mut out = Vec::with_capacity(blob.len());
    for (i, b) in blob.iter().enumerate() {
        out.push(b ^ key_bytes[i % key_bytes.len()]);
    }
    try_zlib(&out).ok()
}

fn try_zlib(data: &[u8]) -> Result<Vec<u8>, std::io::Error> {
    let mut out = Vec::new();
    flate2::read::ZlibDecoder::new(data).read_to_end(&mut out)?;
    Ok(out)
}

// ------------------------------------------------------------- pickle subset

/// Values the index pickle can contain.
#[derive(Debug, Clone, PartialEq)]
pub enum Pickle {
    Int(i64),
    Str(String),
    Bytes(Vec<u8>),
    List(Vec<Pickle>),
    Tuple(Vec<Pickle>),
    Dict(Vec<(Pickle, Pickle)>),
    None,
}

/// Parse the pickle dialect Ren'Py archives use (protocol 1 with the
/// `_codecs.encode` REDUCE trick for byte strings).
pub fn pickle_from_bytes(data: &[u8]) -> Result<Pickle> {
    let mut p = PickleParser { data, pos: 0, stack: Vec::new(), memo: Vec::new() };
    p.run()
}

struct PickleParser<'a> {
    data: &'a [u8],
    pos: usize,
    stack: Vec<Pickle>,
    memo: Vec<Pickle>,
}

impl<'a> PickleParser<'a> {
    fn peek(&self) -> Option<u8> {
        self.data.get(self.pos).copied()
    }

    fn next_u8(&mut self) -> Result<u8> {
        let b = self.peek().ok_or_else(|| anyhow!("pickle truncated"))?;
        self.pos += 1;
        Ok(b)
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.pos + n > self.data.len() {
            bail!("pickle truncated");
        }
        let slice = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(slice)
    }

    fn read_line(&mut self) -> Result<Vec<u8>> {
        let start = self.pos;
        while let Some(b) = self.peek() {
            self.pos += 1;
            if b == b'\n' {
                break;
            }
        }
        Ok(self.data[start..self.pos - 1].to_vec())
    }

    fn run(&mut self) -> Result<Pickle> {
        loop {
            match self.next_u8()? {
                b'(' => self.stack.push(Pickle::Tuple(Vec::new())), // MARK placeholder
                b't' => {
                    // TUPLE: everything above the MARK; drop the placeholder.
                    let mark = self.last_mark_pos()?;
                    let items: Vec<Pickle> = self.stack.drain(mark + 1..).collect();
                    self.stack.truncate(mark);
                    self.stack.push(Pickle::Tuple(items));
                }
                b'e' => {
                    // APPENDS: extend the list beneath the MARK.
                    let mark = self.last_mark_pos()?;
                    let items: Vec<Pickle> = self.stack.drain(mark + 1..).collect();
                    self.stack.truncate(mark);
                    match self.stack.last_mut() {
                        Some(Pickle::List(list)) => list.extend(items),
                        _ => bail!("APPENDS without list"),
                    }
                }
                b'u' => {
                    // SETITEMS: fill the dict beneath the MARK.
                    let mark = self.last_mark_pos()?;
                    let items: Vec<Pickle> = self.stack.drain(mark + 1..).collect();
                    self.stack.truncate(mark);
                    let pairs = items.chunks(2).filter(|c| c.len() == 2).map(|c| {
                        (c[0].clone(), c[1].clone())
                    });
                    match self.stack.last_mut() {
                        Some(Pickle::Dict(entries)) => entries.extend(pairs),
                        _ => bail!("SETITEMS without dict"),
                    }
                }
                b's' => {
                    // SETITEM (value, key, dict)
                    let value = self.stack.pop().ok_or_else(|| anyhow!("pickle truncated"))?;
                    let key = self.stack.pop().ok_or_else(|| anyhow!("pickle truncated"))?;
                    match self.stack.last_mut() {
                        Some(Pickle::Dict(entries)) => entries.push((key, value)),
                        _ => bail!("SETITEM without dict"),
                    }
                }
                b'a' => {
                    // APPEND: push one item onto the list below it.
                    let item = self.stack.pop().ok_or_else(|| anyhow!("pickle truncated"))?;
                    match self.stack.last_mut() {
                        Some(Pickle::List(list)) => list.push(item),
                        _ => bail!("APPEND without list"),
                    }
                }
                b']' => self.stack.push(Pickle::List(Vec::new())),
                b'}' => self.stack.push(Pickle::Dict(Vec::new())),
                b'K' => {
                    let n = self.take(1)?[0] as i64;
                    self.stack.push(Pickle::Int(n));
                }
                b'M' => {
                    let n = u16::from_le_bytes(self.take(2)?.try_into().unwrap()) as i64;
                    self.stack.push(Pickle::Int(n));
                }
                b'J' => {
                    let n = i32::from_le_bytes(self.take(4)?.try_into().unwrap()) as i64;
                    self.stack.push(Pickle::Int(n));
                }
                b'I' | b'i' => {
                    let line = self.read_line()?;
                    let text = String::from_utf8_lossy(&line);
                    self.stack.push(Pickle::Int(text.trim().parse().context("bad INT")?));
                }
                b'X' => {
                    let len = u32::from_le_bytes(self.take(4)?.try_into().unwrap()) as usize;
                    let bytes = self.take(len)?.to_vec();
                    self.stack
                        .push(Pickle::Str(String::from_utf8_lossy(&bytes).into_owned()));
                }
                b'\x8c' => {
                    // SHORT_BINUNICODE
                    let len = self.next_u8()? as usize;
                    let bytes = self.take(len)?.to_vec();
                    self.stack
                        .push(Pickle::Str(String::from_utf8_lossy(&bytes).into_owned()));
                }
                b'\x8d' => {
                    // BINUNICODE8
                    let len = u64::from_le_bytes(self.take(8)?.try_into().unwrap()) as usize;
                    let bytes = self.take(len)?.to_vec();
                    self.stack
                        .push(Pickle::Str(String::from_utf8_lossy(&bytes).into_owned()));
                }
                b'C' => {
                    // SHORT_BINBYTES
                    let len = self.next_u8()? as usize;
                    let bytes = self.take(len)?.to_vec();
                    self.stack.push(Pickle::Bytes(bytes));
                }
                b'B' => {
                    // BINBYTES
                    let len = u32::from_le_bytes(self.take(4)?.try_into().unwrap()) as usize;
                    let bytes = self.take(len)?.to_vec();
                    self.stack.push(Pickle::Bytes(bytes));
                }
                b'\x8e' => {
                    // BINBYTES8
                    let len = u64::from_le_bytes(self.take(8)?.try_into().unwrap()) as usize;
                    let bytes = self.take(len)?.to_vec();
                    self.stack.push(Pickle::Bytes(bytes));
                }
                b'\x8a' => {
                    // LONG1: byte count followed by little-endian bytes.
                    let n = self.next_u8()? as usize;
                    let bytes = self.take(n)?;
                    let mut value: i64 = 0;
                    for (i, b) in bytes.iter().enumerate().take(n.min(8)) {
                        value |= (*b as i64) << (8 * i);
                    }
                    // Sign-extend for negative numbers shorter than 8 bytes.
                    if n < 8 && bytes[n - 1] & 0x80 != 0 {
                        value |= -1i64 << (8 * n);
                    }
                    self.stack.push(Pickle::Int(value));
                }
                b'\x94' => {
                    // MEMOIZE
                    if let Some(top) = self.stack.last().cloned() {
                        self.memo.push(top);
                    }
                }
                b'\x85' | b'\x86' | b'\x87' => {
                    // TUPLE1 / TUPLE2 / TUPLE3
                    let count = self.data[self.pos - 1] - b'\x85' + 1;
                    let n = count as usize;
                    if self.stack.len() < n {
                        bail!("pickle truncated");
                    }
                    let items: Vec<Pickle> = self.stack.drain(self.stack.len() - n..).collect();
                    self.stack.push(Pickle::Tuple(items));
                }
                b'\x88' => self.stack.push(Pickle::Int(1)),
                b'\x89' => self.stack.push(Pickle::Int(0)),
                b'\x95' => {
                    self.take(8)?; // FRAME — length only, content follows
                }
                b'S' => {
                    let line = self.read_line()?;
                    self.stack.push(Pickle::Str(unescape_python_string(&line)));
                }
                b'V' => {
                    let line = self.read_line()?;
                    self.stack.push(Pickle::Str(String::from_utf8_lossy(&line).into_owned()));
                }
                b'c' => {
                    // GLOBAL: "module\nname\n" — only _codecs.encode is expected.
                    let _module = self.read_line()?;
                    let name = self.read_line()?;
                    if name != b"encode" {
                        bail!("unsupported pickle global");
                    }
                    // Represent the callable as a marker string.
                    self.stack.push(Pickle::Str("\u{0}codecs-encode".into()));
                }
                b'R' => {
                    // REDUCE: callable(args)
                    let args = self.stack.pop().ok_or_else(|| anyhow!("pickle truncated"))?;
                    let callable = self.stack.pop().ok_or_else(|| anyhow!("pickle truncated"))?;
                    if callable == Pickle::Str("\u{0}codecs-encode".into()) {
                        // codecs.encode(s) — the single tuple element for
                        // pickled call syntax.
                        let arg = match args {
                            Pickle::Tuple(mut items) if items.len() == 1 => {
                                items.remove(0)
                            }
                            other => other,
                        };
                        let text =
                            as_str(&arg).ok_or_else(|| anyhow!("codecs.encode needs a string"))?;
                        self.stack.push(Pickle::Bytes(text.as_bytes().to_vec()));
                    } else {
                        bail!("unsupported pickle reduce");
                    }
                }
                b'N' => self.stack.push(Pickle::None),
                b'p' | b'q' | b'r' => {
                    // PUT / BINPUT / LONG_BINPUT — memoize the top of stack.
                    let opcode = self.data[self.pos - 1];
                    match opcode {
                        b'p' => {
                            self.read_line()?;
                        }
                        b'q' => {
                            self.take(1)?;
                        }
                        _ => {
                            self.take(4)?;
                        }
                    }
                    let top = self.stack.last().cloned().ok_or_else(|| anyhow!("pickle truncated"))?;
                    self.memo.push(top);
                }
                b'g' | b'h' => {
                    // GET / BINGET — the index pickles do not rely on these;
                    // consume the id and push a placeholder.
                    match self.data[self.pos - 1] {
                        b'g' => {
                            self.read_line()?;
                        }
                        _ => {
                            self.take(1)?;
                        }
                    }
                    self.stack.push(Pickle::None);
                }
                0x80 => {
                    self.take(1)?; // PROTO
                }
                b'.' => {
                    return self.stack.pop().ok_or_else(|| anyhow!("empty pickle"));
                }
                other => bail!("unsupported pickle opcode {:#04x}", other),
            }
        }
    }

    /// Index of the most recent MARK (`(` placeholder) on the stack.
    fn last_mark_pos(&self) -> Result<usize> {
        self.stack
            .iter()
            .rposition(|v| matches!(v, Pickle::Tuple(items) if items.is_empty()))
            .ok_or_else(|| anyhow!("missing MARK"))
    }
}

fn unescape_python_string(raw: &[u8]) -> String {
    let text = String::from_utf8_lossy(raw);
    let trimmed = text
        .strip_prefix('\'')
        .or_else(|| text.strip_prefix('"'))
        .unwrap_or(&text);
    let trimmed = trimmed
        .strip_suffix('\'')
        .or_else(|| trimmed.strip_suffix('"'))
        .unwrap_or(trimmed);

    let mut out = String::with_capacity(trimmed.len());
    let mut chars = trimmed.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('\\') => out.push('\\'),
            Some('\'') => out.push('\''),
            Some('"') => out.push('"'),
            Some('x') => {
                let hex: String = chars.by_ref().take(2).collect();
                if let Ok(v) = u8::from_str_radix(&hex, 16) {
                    // Latin-1 round trip (how python encoded the bytes).
                    out.push(v as char);
                }
            }
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

fn as_dict(value: &Pickle) -> Option<Vec<(Pickle, Pickle)>> {
    match value {
        Pickle::Dict(entries) => Some(entries.clone()),
        _ => None,
    }
}

fn as_list(value: &Pickle) -> Option<Vec<Pickle>> {
    match value {
        Pickle::List(items) => Some(items.clone()),
        Pickle::Tuple(items) => Some(items.clone()),
        _ => None,
    }
}

fn as_str(value: &Pickle) -> Option<String> {
    match value {
        Pickle::Str(s) => Some(s.clone()),
        _ => None,
    }
}

fn as_bytes(value: &Pickle) -> Option<Vec<u8>> {
    match value {
        Pickle::Bytes(b) => Some(b.clone()),
        Pickle::Str(s) => Some(s.as_bytes().to_vec()), // empty prefixes pickle as strings
        _ => None,
    }
}

fn as_int(value: &Pickle) -> Option<i64> {
    match value {
        Pickle::Int(n) => Some(*n),
        _ => None,
    }
}

#[cfg(test)]
pub(crate) mod testutil {
    use std::io::Write;
    use std::path::{Path, PathBuf};

    pub fn temp_archive(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "gtl-rpa-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("archive.rpa")
    }

    pub fn zlib_compress(data: &[u8]) -> Vec<u8> {
        let mut encoder =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap()
    }

    /// Build a minimal RPA-3.0 archive with the given (name, content)
    /// members. Used by extractor tests too.
    pub fn build_test_archive(path: &Path, members: &[(&str, &str)]) {
        // Fixed-width header: "RPA-3.0 " + 16 + ' ' + 16 + '\n' — so the
        // base offset is known before the real index offset is computed.
        let base = "RPA-3.0 0000000000000000 0000000000000000\n".len() as u64;

        let compressed: Vec<(String, Vec<u8>)> = members
            .iter()
            .map(|(name, content)| ((*name).to_string(), zlib_compress(content.as_bytes())))
            .collect();

        let mut body: Vec<u8> = Vec::new();
        body.extend(std::iter::repeat(b'\n').take(64));
        let mut offsets: Vec<(String, u64, u64)> = Vec::new();
        for (name, blob) in &compressed {
            let offset = base + body.len() as u64;
            body.extend_from_slice(blob);
            offsets.push((name.clone(), offset, blob.len() as u64));
        }

        let mut pickle: Vec<u8> = Vec::new();
        pickle.push(b'}');
        for (i, (name, offset, length)) in offsets.iter().enumerate() {
            // key: name (memoized)
            pickle.extend_from_slice(b"X");
            pickle.extend_from_slice(&(name.len() as u32).to_le_bytes());
            pickle.extend_from_slice(name.as_bytes());
            pickle.extend_from_slice(format!("p{}\n", 10 + i).as_bytes());
            // value: a list containing one (offset, length, prefix) tuple —
            // the exact shape pickle.dumps(protocol 1) produces. RPA-3.0
            // XORs offset and length with the header key.
            pickle.push(b']'); // EMPTY_LIST
            pickle.push(b'('); // MARK for the batch
            pickle.push(b'('); // MARK for the tuple
            pickle.extend_from_slice(b"J");
            pickle.extend_from_slice(&(((*offset as i64) ^ 0x42424242) as i32).to_le_bytes());
            pickle.extend_from_slice(b"J");
            pickle.extend_from_slice(&(((*length as i64) ^ 0x42424242) as i32).to_le_bytes());
            pickle.extend_from_slice(b"X\x00\x00\x00\x00");
            pickle.push(b't'); // TUPLE
            pickle.push(b'e'); // APPENDS
            pickle.push(b's'); // SETITEM
        }
        pickle.push(b'.');

        let index_offset = base + body.len() as u64;
        body.extend_from_slice(&zlib_compress(&pickle));

        let mut f = std::fs::File::create(path).unwrap();
        f.write_all(format!("RPA-3.0 {index_offset:016x} {:016x}\n", 0x42424242u64).as_bytes())
            .unwrap();
        f.write_all(&body).unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::testutil::{build_test_archive, temp_archive};

    #[test]
    fn parses_index_and_extracts_members() {
        let path = temp_archive("build");
        build_test_archive(
            &path,
            &[
                ("a.rpy", "e \"Hello from archive.\"\n"),
                ("b.rpy", "label start:\n    \"World\"\n"),
            ],
        );
        let index = RpaIndex::open(&path).unwrap();
        assert_eq!(index.members.len(), 2);
        assert!(index.members.contains_key("a.rpy"));

        assert!(index.names_with_extension(".rpy").contains(&"a.rpy".to_string()));
        assert!(index.names_with_extension(".rpyc").is_empty());

        let hello = index.read_member(&path, "a.rpy").unwrap();
        assert_eq!(hello, "e \"Hello from archive.\"\n");
        let world = index.read_member(&path, "b.rpy").unwrap();
        assert_eq!(world, "label start:\n    \"World\"\n");
        assert!(index.read_member(&path, "missing.rpy").is_err());
    }

    #[test]
    fn rejects_non_rpa_files() {
        let path = temp_archive("bad");
        std::fs::write(&path, b"PK\x03\x04 not an archive\n").unwrap();
        assert!(RpaIndex::open(&path).is_err());
    }

    #[test]
    fn pickle_parser_handles_text_opcodes() {
        // {"k": (1, 2)} with text-style opcodes: EMPTY_DICT, then
        // SETITEM with a key and a tuple value.
        let data = b"}S'k'\n(I1\nI2\ntp1\ns.";
        let parsed = pickle_from_bytes(data).unwrap();
        let entries = as_dict(&parsed).unwrap();
        assert_eq!(entries.len(), 1);
        let (key, value) = &entries[0];
        assert_eq!(as_str(key).as_deref(), Some("k"));
        let tuple = as_list(value).unwrap();
        assert_eq!(as_int(&tuple[0]), Some(1));
        assert_eq!(as_int(&tuple[1]), Some(2));
    }

    #[test]
    fn pickle_parser_handles_codecs_bytes() {
        // b'' via _codecs.encode('') — how Ren'Py pickles byte-string prefixes.
        let mut d: Vec<u8> = Vec::new();
        d.extend_from_slice(b"c_codecs\nencode\n");
        d.extend_from_slice(b"(X\x00\x00\x00\x00");
        d.extend_from_slice(b"t");
        d.extend_from_slice(b"R");
        d.push(b'.');
        let parsed = pickle_from_bytes(&d).unwrap();
        assert_eq!(as_bytes(&parsed), Some(Vec::new()));
    }
}
