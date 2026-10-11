// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Read tensors out of a PyTorch `torch.save` checkpoint without Python.
//!
//! Some fine-tunes ship a small trained head as a `.pt` file beside a safetensors adapter. A
//! `.pt` file is a zip archive, stored without compression, holding a pickle (`data.pkl`) that
//! describes the saved object and one raw little-endian file per tensor storage (`data/0`,
//! `data/1`, ...). This module reads that layout and nothing more:
//!
//! - the zip reader walks the central directory and serves stored entries, and refuses
//!   compressed ones;
//! - the pickle reader is a data-only stack machine over the opcodes `torch.save` writes for
//!   dicts, lists, tuples, strings, numbers and tensors. It never imports or calls anything: a
//!   global is recorded by name, and only the two reconstructions a state dict needs,
//!   `torch._utils._rebuild_tensor_v2` and `collections.OrderedDict`, are interpreted. Any
//!   other call stays an inert [`PickleValue::Call`], and an opcode outside the set is an
//!   error.
//!
//! Tensors decode from float32, float16 and bfloat16 storages into `f32`.

use std::collections::HashMap;
use std::path::Path;

use half::{bf16, f16};

use crate::InferError;

/// A value from the checkpoint's pickle.
#[derive(Clone, Debug, PartialEq)]
pub enum PickleValue {
    None,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Bytes(Vec<u8>),
    Tuple(Vec<PickleValue>),
    List(Vec<PickleValue>),
    /// A dict or an `OrderedDict`, in insertion order.
    Dict(Vec<(PickleValue, PickleValue)>),
    /// A reference to a module attribute, such as `torch FloatStorage`. Never resolved.
    Global {
        module: String,
        name: String,
    },
    /// A persistent id: `torch.save` uses it for tensor storages.
    Persistent(Box<PickleValue>),
    /// A call the reader does not interpret, kept as data.
    Call {
        callable: Box<PickleValue>,
        args: Box<PickleValue>,
    },
    /// A tensor `_rebuild_tensor_v2` would have built.
    Tensor(TorchTensor),
}

impl PickleValue {
    /// The value under a string key, when this is a dict that has one.
    pub fn get(&self, key: &str) -> Option<&PickleValue> {
        match self {
            PickleValue::Dict(items) => items.iter().find_map(|(k, v)| match k {
                PickleValue::Str(s) if s == key => Some(v),
                _ => None,
            }),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            PickleValue::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            PickleValue::Float(v) => Some(*v),
            PickleValue::Int(v) => Some(*v as f64),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            PickleValue::Int(v) => Some(*v),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            PickleValue::Bool(v) => Some(*v),
            _ => None,
        }
    }

    pub fn as_tensor(&self) -> Option<&TorchTensor> {
        match self {
            PickleValue::Tensor(t) => Some(t),
            _ => None,
        }
    }
}

/// The element type of a tensor storage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StorageDtype {
    F32,
    F16,
    BF16,
}

impl StorageDtype {
    fn from_storage_class(name: &str) -> Option<Self> {
        Some(match name {
            "FloatStorage" => Self::F32,
            "HalfStorage" => Self::F16,
            "BFloat16Storage" => Self::BF16,
            _ => return None,
        })
    }

    fn width(self) -> usize {
        match self {
            Self::F32 => 4,
            Self::F16 | Self::BF16 => 2,
        }
    }
}

/// Where a tensor's values live: a storage file of the archive and a view into it.
#[derive(Clone, Debug, PartialEq)]
pub struct TorchTensor {
    pub storage: String,
    pub dtype: StorageDtype,
    /// Offset into the storage, in elements.
    pub offset: usize,
    pub shape: Vec<usize>,
    pub stride: Vec<usize>,
}

impl TorchTensor {
    pub fn numel(&self) -> usize {
        self.shape.iter().product()
    }

    fn is_contiguous(&self) -> bool {
        let mut expected = 1;
        for (&dim, &stride) in self.shape.iter().zip(&self.stride).rev() {
            if dim != 1 && stride != expected {
                return false;
            }
            expected *= dim;
        }
        true
    }
}

/// A `torch.save` checkpoint read into memory.
pub struct TorchCheckpoint {
    bytes: Vec<u8>,
    /// Entry name to (data offset, length), for stored entries.
    entries: HashMap<String, (usize, usize)>,
    /// The archive's top directory, such as `head/`.
    prefix: String,
    root: PickleValue,
}

impl TorchCheckpoint {
    /// Reads a checkpoint file.
    ///
    /// # Errors
    ///
    /// Fails when the file is not a zip archive `torch.save` wrote, when an entry it needs is
    /// compressed or missing, or when the pickle uses an opcode this reader does not implement.
    pub fn read(path: &Path) -> Result<Self, InferError> {
        let bytes = std::fs::read(path)
            .map_err(|e| InferError::ModelError(format!("{}: {e}", path.display())))?;
        Self::from_bytes(bytes).map_err(|e| match e {
            InferError::ModelError(m) => InferError::ModelError(format!("{}: {m}", path.display())),
            other => other,
        })
    }

    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self, InferError> {
        let entries = zip_entries(&bytes)?;
        let pickle_name = entries
            .keys()
            .filter(|name| name.ends_with("/data.pkl") || name.as_str() == "data.pkl")
            .min_by_key(|name| name.len())
            .cloned()
            .ok_or_else(|| bad("no data.pkl in the archive"))?;
        let prefix = pickle_name[..pickle_name.len() - "data.pkl".len()].to_string();
        let (start, len) = entries[&pickle_name];
        let root = Unpickler::new(&bytes[start..start + len]).run()?;
        Ok(Self {
            bytes,
            entries,
            prefix,
            root,
        })
    }

    /// The saved object.
    pub fn root(&self) -> &PickleValue {
        &self.root
    }

    /// A tensor's values in `f32`, in row-major order.
    ///
    /// # Errors
    ///
    /// Fails when the tensor is not contiguous or its storage file is missing or too short.
    pub fn tensor_f32(&self, tensor: &TorchTensor) -> Result<Vec<f32>, InferError> {
        if !tensor.is_contiguous() {
            return Err(bad(&format!(
                "tensor of shape {:?} with stride {:?} is not contiguous",
                tensor.shape, tensor.stride
            )));
        }
        let name = format!("{}data/{}", self.prefix, tensor.storage);
        let &(start, len) = self
            .entries
            .get(&name)
            .ok_or_else(|| bad(&format!("storage {name} is missing")))?;
        let width = tensor.dtype.width();
        let first = tensor.offset * width;
        let end = first + tensor.numel() * width;
        if end > len {
            return Err(bad(&format!(
                "storage {name} holds {len} bytes, the tensor needs {end}"
            )));
        }
        let raw = &self.bytes[start + first..start + end];
        Ok(match tensor.dtype {
            StorageDtype::F32 => raw
                .chunks_exact(4)
                .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                .collect(),
            StorageDtype::F16 => raw
                .chunks_exact(2)
                .map(|b| f16::from_le_bytes([b[0], b[1]]).to_f32())
                .collect(),
            StorageDtype::BF16 => raw
                .chunks_exact(2)
                .map(|b| bf16::from_le_bytes([b[0], b[1]]).to_f32())
                .collect(),
        })
    }
}

fn bad(message: &str) -> InferError {
    InferError::ModelError(format!("torch checkpoint: {message}"))
}

// ---------------------------------------------------------------------------
// Zip: stored entries through the central directory
// ---------------------------------------------------------------------------

fn u16_at(b: &[u8], at: usize) -> Result<usize, InferError> {
    b.get(at..at + 2)
        .map(|s| u16::from_le_bytes([s[0], s[1]]) as usize)
        .ok_or_else(|| bad("truncated zip record"))
}

fn u32_at(b: &[u8], at: usize) -> Result<usize, InferError> {
    b.get(at..at + 4)
        .map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]) as usize)
        .ok_or_else(|| bad("truncated zip record"))
}

fn u64_at(b: &[u8], at: usize) -> Result<usize, InferError> {
    b.get(at..at + 8)
        .map(|s| u64::from_le_bytes(s.try_into().expect("eight bytes")) as usize)
        .ok_or_else(|| bad("truncated zip record"))
}

/// Every stored entry's data range, keyed by name.
fn zip_entries(b: &[u8]) -> Result<HashMap<String, (usize, usize)>, InferError> {
    const EOCD: u32 = 0x0605_4b50;
    const CENTRAL: u32 = 0x0201_4b50;
    const LOCAL: u32 = 0x0403_4b50;
    // The end-of-central-directory record sits in the last 64 KiB + 22 bytes.
    let floor = b.len().saturating_sub(22 + 0xFFFF);
    let eocd = (floor..b.len().saturating_sub(21))
        .rev()
        .find(|&i| u32_at(b, i).ok() == Some(EOCD as usize))
        .ok_or_else(|| bad("not a zip archive"))?;
    let count = u16_at(b, eocd + 10)?;
    let mut at = u32_at(b, eocd + 16)?;
    if count == 0xFFFF || at == 0xFFFF_FFFF {
        return Err(bad("zip64 central directories are not supported"));
    }
    let mut entries = HashMap::with_capacity(count);
    for _ in 0..count {
        if u32_at(b, at)? != CENTRAL as usize {
            return Err(bad("malformed central directory"));
        }
        let method = u16_at(b, at + 10)?;
        let mut size = u32_at(b, at + 20)?;
        let name_len = u16_at(b, at + 28)?;
        let extra_len = u16_at(b, at + 30)?;
        let comment_len = u16_at(b, at + 32)?;
        let mut local = u32_at(b, at + 42)?;
        let name = b
            .get(at + 46..at + 46 + name_len)
            .ok_or_else(|| bad("truncated entry name"))?;
        let name = String::from_utf8_lossy(name).into_owned();
        // Zip64: sizes and offsets that do not fit 32 bits move to extra field 0x0001, in the
        // order uncompressed size, compressed size, local header offset.
        let mut extra = at + 46 + name_len;
        let extra_end = extra + extra_len;
        let uncompressed_marker = u32_at(b, at + 24)? == 0xFFFF_FFFF;
        while extra + 4 <= extra_end {
            let id = u16_at(b, extra)?;
            let len = u16_at(b, extra + 2)?;
            if id == 1 {
                let mut field = extra + 4;
                if uncompressed_marker {
                    field += 8;
                }
                if size == 0xFFFF_FFFF {
                    size = u64_at(b, field)?;
                    field += 8;
                }
                if local == 0xFFFF_FFFF {
                    local = u64_at(b, field)?;
                }
            }
            extra += 4 + len;
        }
        at = extra_end + comment_len;
        if method != 0 {
            // Only stored entries are read; skip a compressed one unless something asks for it.
            continue;
        }
        if u32_at(b, local)? != LOCAL as usize {
            return Err(bad(&format!("malformed local header for {name}")));
        }
        let data = local + 30 + u16_at(b, local + 26)? + u16_at(b, local + 28)?;
        if data + size > b.len() {
            return Err(bad(&format!("entry {name} runs past the archive")));
        }
        entries.insert(name, (data, size));
    }
    Ok(entries)
}

// ---------------------------------------------------------------------------
// Pickle: a data-only stack machine
// ---------------------------------------------------------------------------

enum Slot {
    Value(PickleValue),
    Mark,
}

struct Unpickler<'a> {
    data: &'a [u8],
    at: usize,
    stack: Vec<Slot>,
    memo: HashMap<usize, PickleValue>,
}

impl<'a> Unpickler<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            at: 0,
            stack: Vec::new(),
            memo: HashMap::new(),
        }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], InferError> {
        let out = self
            .data
            .get(self.at..self.at + n)
            .ok_or_else(|| bad("truncated pickle"))?;
        self.at += n;
        Ok(out)
    }

    fn byte(&mut self) -> Result<u8, InferError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<usize, InferError> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]) as usize)
    }

    fn u32(&mut self) -> Result<usize, InferError> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize)
    }

    fn line(&mut self) -> Result<String, InferError> {
        let rest = &self.data[self.at..];
        let end = rest
            .iter()
            .position(|&c| c == b'\n')
            .ok_or_else(|| bad("unterminated pickle line"))?;
        self.at += end + 1;
        Ok(String::from_utf8_lossy(&rest[..end]).into_owned())
    }

    fn string(&mut self, n: usize) -> Result<PickleValue, InferError> {
        let raw = self.take(n)?;
        Ok(PickleValue::Str(String::from_utf8_lossy(raw).into_owned()))
    }

    fn push(&mut self, value: PickleValue) {
        self.stack.push(Slot::Value(value));
    }

    fn pop(&mut self) -> Result<PickleValue, InferError> {
        match self.stack.pop() {
            Some(Slot::Value(v)) => Ok(v),
            _ => Err(bad("pickle stack underflow")),
        }
    }

    fn top(&mut self) -> Result<&mut PickleValue, InferError> {
        match self.stack.last_mut() {
            Some(Slot::Value(v)) => Ok(v),
            _ => Err(bad("pickle stack underflow")),
        }
    }

    /// The values pushed since the last mark, which is removed.
    fn pop_mark(&mut self) -> Result<Vec<PickleValue>, InferError> {
        let mark = self
            .stack
            .iter()
            .rposition(|slot| matches!(slot, Slot::Mark))
            .ok_or_else(|| bad("pickle mark missing"))?;
        let items = self.stack.split_off(mark + 1);
        self.stack.pop();
        Ok(items
            .into_iter()
            .map(|slot| match slot {
                Slot::Value(v) => v,
                Slot::Mark => PickleValue::None,
            })
            .collect())
    }

    fn memo_put(&mut self, key: usize) -> Result<(), InferError> {
        let value = self.top()?.clone();
        self.memo.insert(key, value);
        Ok(())
    }

    fn memo_get(&mut self, key: usize) -> Result<(), InferError> {
        let value = self
            .memo
            .get(&key)
            .cloned()
            .ok_or_else(|| bad("pickle memo miss"))?;
        self.push(value);
        Ok(())
    }

    fn set_items(&mut self, items: Vec<PickleValue>) -> Result<(), InferError> {
        if items.len() % 2 != 0 {
            return Err(bad("odd number of dict items"));
        }
        let PickleValue::Dict(dict) = self.top()? else {
            return Err(bad("SETITEMS on a value that is not a dict"));
        };
        let mut items = items.into_iter();
        while let (Some(k), Some(v)) = (items.next(), items.next()) {
            dict.push((k, v));
        }
        Ok(())
    }

    fn append(&mut self, items: Vec<PickleValue>) -> Result<(), InferError> {
        let PickleValue::List(list) = self.top()? else {
            return Err(bad("APPEND on a value that is not a list"));
        };
        list.extend(items);
        Ok(())
    }

    fn run(mut self) -> Result<PickleValue, InferError> {
        loop {
            let op = self.byte()?;
            match op {
                0x80 => {
                    self.byte()?; // PROTO
                }
                0x95 => {
                    self.take(8)?; // FRAME
                }
                b'.' => return self.pop(),
                b'(' => self.stack.push(Slot::Mark),
                b'}' => self.push(PickleValue::Dict(Vec::new())),
                b']' => self.push(PickleValue::List(Vec::new())),
                b')' => self.push(PickleValue::Tuple(Vec::new())),
                b'N' => self.push(PickleValue::None),
                0x88 => self.push(PickleValue::Bool(true)),
                0x89 => self.push(PickleValue::Bool(false)),
                b'K' => {
                    let v = self.byte()?;
                    self.push(PickleValue::Int(v as i64));
                }
                b'M' => {
                    let v = self.u16()?;
                    self.push(PickleValue::Int(v as i64));
                }
                b'J' => {
                    let v = self.u32()? as u32 as i32;
                    self.push(PickleValue::Int(v as i64));
                }
                0x8a => {
                    // LONG1: little-endian two's complement of n bytes.
                    let n = self.byte()? as usize;
                    let raw = self.take(n)?;
                    if n > 8 {
                        return Err(bad("integer wider than 64 bits"));
                    }
                    let mut buf = if raw.last().is_some_and(|b| b & 0x80 != 0) {
                        [0xFFu8; 8]
                    } else {
                        [0u8; 8]
                    };
                    buf[..n].copy_from_slice(raw);
                    self.push(PickleValue::Int(i64::from_le_bytes(buf)));
                }
                b'G' => {
                    let raw = self.take(8)?;
                    let v = f64::from_be_bytes(raw.try_into().expect("eight bytes"));
                    self.push(PickleValue::Float(v));
                }
                b'X' => {
                    let n = self.u32()?;
                    let v = self.string(n)?;
                    self.push(v);
                }
                0x8c => {
                    let n = self.byte()? as usize;
                    let v = self.string(n)?;
                    self.push(v);
                }
                b'B' => {
                    let n = self.u32()?;
                    let raw = self.take(n)?.to_vec();
                    self.push(PickleValue::Bytes(raw));
                }
                b'C' => {
                    let n = self.byte()? as usize;
                    let raw = self.take(n)?.to_vec();
                    self.push(PickleValue::Bytes(raw));
                }
                b'q' => {
                    let key = self.byte()? as usize;
                    self.memo_put(key)?;
                }
                b'r' => {
                    let key = self.u32()?;
                    self.memo_put(key)?;
                }
                0x94 => {
                    let key = self.memo.len();
                    self.memo_put(key)?;
                }
                b'h' => {
                    let key = self.byte()? as usize;
                    self.memo_get(key)?;
                }
                b'j' => {
                    let key = self.u32()?;
                    self.memo_get(key)?;
                }
                b'c' => {
                    let module = self.line()?;
                    let name = self.line()?;
                    self.push(PickleValue::Global { module, name });
                }
                0x93 => {
                    let name = self.pop()?;
                    let module = self.pop()?;
                    match (module, name) {
                        (PickleValue::Str(module), PickleValue::Str(name)) => {
                            self.push(PickleValue::Global { module, name });
                        }
                        _ => return Err(bad("STACK_GLOBAL without two strings")),
                    }
                }
                b't' => {
                    let items = self.pop_mark()?;
                    self.push(PickleValue::Tuple(items));
                }
                0x85 => {
                    let a = self.pop()?;
                    self.push(PickleValue::Tuple(vec![a]));
                }
                0x86 => {
                    let b = self.pop()?;
                    let a = self.pop()?;
                    self.push(PickleValue::Tuple(vec![a, b]));
                }
                0x87 => {
                    let c = self.pop()?;
                    let b = self.pop()?;
                    let a = self.pop()?;
                    self.push(PickleValue::Tuple(vec![a, b, c]));
                }
                b'l' => {
                    let items = self.pop_mark()?;
                    self.push(PickleValue::List(items));
                }
                b'd' => {
                    let items = self.pop_mark()?;
                    self.push(PickleValue::Dict(Vec::new()));
                    self.set_items(items)?;
                }
                b's' => {
                    let v = self.pop()?;
                    let k = self.pop()?;
                    self.set_items(vec![k, v])?;
                }
                b'u' => {
                    let items = self.pop_mark()?;
                    self.set_items(items)?;
                }
                b'a' => {
                    let v = self.pop()?;
                    self.append(vec![v])?;
                }
                b'e' => {
                    let items = self.pop_mark()?;
                    self.append(items)?;
                }
                b'Q' => {
                    let id = self.pop()?;
                    self.push(PickleValue::Persistent(Box::new(id)));
                }
                b'R' => {
                    let args = self.pop()?;
                    let callable = self.pop()?;
                    let value = reduce(callable, args)?;
                    self.push(value);
                }
                0x81 => {
                    // NEWOBJ: cls.__new__(cls, *args), kept as data.
                    let args = self.pop()?;
                    let callable = self.pop()?;
                    self.push(PickleValue::Call {
                        callable: Box::new(callable),
                        args: Box::new(args),
                    });
                }
                b'b' => {
                    // BUILD: state for the object below. A state dict's `_metadata` lands
                    // here; it carries nothing a tensor needs.
                    self.pop()?;
                }
                other => {
                    return Err(bad(&format!(
                        "pickle opcode 0x{other:02x} at byte {} is not supported",
                        self.at - 1
                    )))
                }
            }
        }
    }
}

/// Interpret the two reconstructions a state dict needs; keep every other call as data.
fn reduce(callable: PickleValue, args: PickleValue) -> Result<PickleValue, InferError> {
    if let PickleValue::Global { module, name } = &callable {
        match (module.as_str(), name.as_str()) {
            ("collections", "OrderedDict") => return Ok(PickleValue::Dict(Vec::new())),
            ("torch._utils", "_rebuild_tensor_v2") => return rebuild_tensor(&args),
            _ => {}
        }
    }
    Ok(PickleValue::Call {
        callable: Box::new(callable),
        args: Box::new(args),
    })
}

/// `_rebuild_tensor_v2(storage, storage_offset, size, stride, requires_grad, hooks, ...)`,
/// where `storage` is the persistent id `('storage', StorageClass, key, location, numel)`.
fn rebuild_tensor(args: &PickleValue) -> Result<PickleValue, InferError> {
    let PickleValue::Tuple(args) = args else {
        return Err(bad("_rebuild_tensor_v2 without an argument tuple"));
    };
    let malformed = || bad("malformed _rebuild_tensor_v2 arguments");
    let Some(PickleValue::Persistent(id)) = args.first() else {
        return Err(malformed());
    };
    let PickleValue::Tuple(id) = id.as_ref() else {
        return Err(malformed());
    };
    let (Some(PickleValue::Global { name: class, .. }), Some(PickleValue::Str(key))) =
        (id.get(1), id.get(2))
    else {
        return Err(malformed());
    };
    let dtype = StorageDtype::from_storage_class(class)
        .ok_or_else(|| bad(&format!("storage type {class} is not supported")))?;
    let offset = args
        .get(1)
        .and_then(PickleValue::as_i64)
        .ok_or_else(malformed)?;
    let dims = |value: Option<&PickleValue>| -> Result<Vec<usize>, InferError> {
        match value {
            Some(PickleValue::Tuple(items)) => items
                .iter()
                .map(|v| {
                    v.as_i64()
                        .and_then(|d| usize::try_from(d).ok())
                        .ok_or_else(malformed)
                })
                .collect(),
            _ => Err(malformed()),
        }
    };
    let shape = dims(args.get(2))?;
    let stride = dims(args.get(3))?;
    if shape.len() != stride.len() {
        return Err(malformed());
    }
    Ok(PickleValue::Tensor(TorchTensor {
        storage: key.clone(),
        dtype,
        offset: usize::try_from(offset).map_err(|_| malformed())?,
        shape,
        stride,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stored zip with the given entries, as `torch.save` writes them.
    fn zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut central = Vec::new();
        for (name, data) in entries {
            let offset = out.len() as u32;
            out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
            out.extend_from_slice(&[0u8; 22]);
            out.extend_from_slice(&(name.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(data);
            central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
            central.extend_from_slice(&[0u8; 6]);
            central.extend_from_slice(&0u16.to_le_bytes()); // method: stored
            central.extend_from_slice(&[0u8; 8]);
            central.extend_from_slice(&(data.len() as u32).to_le_bytes());
            central.extend_from_slice(&(data.len() as u32).to_le_bytes());
            central.extend_from_slice(&(name.len() as u16).to_le_bytes());
            central.extend_from_slice(&[0u8; 12]);
            central.extend_from_slice(&offset.to_le_bytes());
            central.extend_from_slice(name.as_bytes());
        }
        let central_at = out.len() as u32;
        out.extend_from_slice(&central);
        out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        out.extend_from_slice(&[0u8; 4]);
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&(central.len() as u32).to_le_bytes());
        out.extend_from_slice(&central_at.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }

    /// The protocol-2 pickle `torch.save({"head": OrderedDict(w=tensor), "t": 1.5, "on": False})`
    /// writes, for a 2x3 float32 tensor `w` in storage "0".
    fn pickle() -> Vec<u8> {
        let mut p = vec![0x80, 2, b'}', b'q', 0, b'('];
        let text = |p: &mut Vec<u8>, s: &str| {
            p.push(b'X');
            p.extend_from_slice(&(s.len() as u32).to_le_bytes());
            p.extend_from_slice(s.as_bytes());
        };
        text(&mut p, "head");
        p.extend_from_slice(b"ccollections\nOrderedDict\nq\x01)R(");
        text(&mut p, "w");
        p.extend_from_slice(b"ctorch._utils\n_rebuild_tensor_v2\n((");
        text(&mut p, "storage");
        p.extend_from_slice(b"ctorch\nFloatStorage\n");
        text(&mut p, "0");
        text(&mut p, "cpu");
        p.extend_from_slice(&[
            b'K', 6, b't', b'Q', b'K', 0, b'K', 2, b'K', 3, 0x86, b'K', 3,
        ]);
        p.extend_from_slice(&[b'K', 1, 0x86, 0x89, b'h', 1, b')', b'R', b't', b'R', b'u']);
        p.extend_from_slice(b"}b");
        text(&mut p, "t");
        p.push(b'G');
        p.extend_from_slice(&1.5f64.to_be_bytes());
        text(&mut p, "on");
        p.extend_from_slice(&[0x89, b'u', b'.']);
        p
    }

    #[test]
    fn reads_a_state_dict_tensor_and_scalars_from_a_stored_archive() {
        let values: Vec<u8> = (0..6)
            .flat_map(|i| (i as f32 * 0.5).to_le_bytes())
            .collect();
        let archive = zip(&[
            ("head/data.pkl", &pickle()),
            ("head/byteorder", b"little"),
            ("head/data/0", &values),
        ]);
        let checkpoint = TorchCheckpoint::from_bytes(archive).unwrap();
        let root = checkpoint.root();
        assert_eq!(root.get("t").and_then(PickleValue::as_f64), Some(1.5));
        assert_eq!(root.get("on").and_then(PickleValue::as_bool), Some(false));
        let tensor = root
            .get("head")
            .and_then(|head| head.get("w"))
            .and_then(PickleValue::as_tensor)
            .unwrap();
        assert_eq!(tensor.shape, vec![2, 3]);
        assert_eq!(tensor.dtype, StorageDtype::F32);
        assert_eq!(
            checkpoint.tensor_f32(tensor).unwrap(),
            vec![0.0, 0.5, 1.0, 1.5, 2.0, 2.5]
        );
    }

    #[test]
    fn an_unknown_opcode_or_a_missing_storage_is_an_error_not_a_guess() {
        let archive = zip(&[("x/data.pkl", &[0x80, 2, b'i', b'.'])]);
        assert!(TorchCheckpoint::from_bytes(archive).is_err());
        let archive = zip(&[("head/data.pkl", &pickle())]);
        let checkpoint = TorchCheckpoint::from_bytes(archive).unwrap();
        let tensor = checkpoint
            .root()
            .get("head")
            .and_then(|head| head.get("w"))
            .and_then(PickleValue::as_tensor)
            .unwrap();
        assert!(checkpoint.tensor_f32(tensor).is_err());
        assert!(TorchCheckpoint::from_bytes(b"not a zip at all".to_vec()).is_err());
    }
}
