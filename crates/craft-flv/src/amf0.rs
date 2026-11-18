//! AMF0 values: the encoding of FLV script data and of RTMP command and data messages.
//!
//! The `write_*` functions append one value to a `Vec<u8>` without building a [`Value`], so a
//! caller that sends the same kind of message again can reuse one buffer. [`Value::write`] writes a
//! whole value, and [`read`] reads one back.
//!
//! Reading refuses references, movie clips, typed objects, XML documents, record sets, the
//! "unsupported" marker and the AMF3 switch marker ([`Error::Unsupported`]), values nested deeper
//! than [`MAX_DEPTH`], more than [`MAX_VALUES`] values in one call, strings that are not UTF-8,
//! and a strict array whose count is larger than the bytes left could hold.

use crate::Error;

/// The type markers (AMF0 specification §2.1).
pub mod marker {
    /// A number: an IEEE 754 double, big-endian.
    pub const NUMBER: u8 = 0x00;
    /// A Boolean: one byte, 0 for false.
    pub const BOOLEAN: u8 = 0x01;
    /// A string of up to 65 535 bytes with a 16-bit length.
    pub const STRING: u8 = 0x02;
    /// An anonymous object: properties ending with [`OBJECT_END`].
    pub const OBJECT: u8 = 0x03;
    /// A movie clip (reserved).
    pub const MOVIECLIP: u8 = 0x04;
    /// Null.
    pub const NULL: u8 = 0x05;
    /// Undefined.
    pub const UNDEFINED: u8 = 0x06;
    /// A reference to an earlier object.
    pub const REFERENCE: u8 = 0x07;
    /// An ECMA array: a 32-bit count, then properties ending with [`OBJECT_END`].
    pub const ECMA_ARRAY: u8 = 0x08;
    /// The end of an object's or an ECMA array's properties, after an empty name.
    pub const OBJECT_END: u8 = 0x09;
    /// A strict array: a 32-bit count, then that many values.
    pub const STRICT_ARRAY: u8 = 0x0A;
    /// A date: a double of milliseconds since 1970-01-01 UTC and a 16-bit time zone.
    pub const DATE: u8 = 0x0B;
    /// A string with a 32-bit length.
    pub const LONG_STRING: u8 = 0x0C;
}

/// The deepest nesting of objects and arrays [`read`] accepts.
pub const MAX_DEPTH: usize = 64;

/// The most values one [`read`] or [`read_all`] call decodes, nested ones included. At about 32
/// bytes per [`Value`] this caps the values of one call at about 32 MiB, and it leaves room for the
/// keyframe index a long recording's `onMetaData` may hold.
pub const MAX_VALUES: usize = 1 << 20;

/// The most items [`read`] reserves room for at the start of a strict array.
const STRICT_ARRAY_RESERVE: usize = 64;

/// One AMF0 value.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// A number.
    Number(f64),
    /// A Boolean.
    Boolean(bool),
    /// A string. [`Value::write`] uses the long string marker for strings over 65 535 bytes, and
    /// [`read`] returns both kinds as this variant.
    String(String),
    /// An anonymous object's properties, in order.
    Object(Vec<(String, Value)>),
    /// Null.
    Null,
    /// Undefined.
    Undefined,
    /// An ECMA array's properties, in order.
    EcmaArray(Vec<(String, Value)>),
    /// A strict array's values.
    StrictArray(Vec<Value>),
    /// A date.
    Date {
        /// Milliseconds since 1970-01-01 UTC.
        millis: f64,
        /// The time zone's offset from UTC in minutes.
        offset_minutes: i16,
    },
}

impl Value {
    /// The property `name` of an object or ECMA array, or `None`.
    pub fn get(&self, name: &str) -> Option<&Value> {
        match self {
            Value::Object(p) | Value::EcmaArray(p) => p.iter().find(|(k, _)| k == name).map(|(_, v)| v),
            _ => None,
        }
    }

    /// The number, or `None` for another type.
    pub fn as_number(&self) -> Option<f64> {
        if let Value::Number(n) = self { Some(*n) } else { None }
    }

    /// The Boolean, or `None` for another type.
    pub fn as_bool(&self) -> Option<bool> {
        if let Value::Boolean(b) = self { Some(*b) } else { None }
    }

    /// The string, or `None` for another type.
    pub fn as_str(&self) -> Option<&str> {
        if let Value::String(s) = self { Some(s) } else { None }
    }

    /// Appends the value to `out`. A property name longer than 65 535 bytes, a string longer than
    /// 4 GiB, more than 2^32 - 1 array items, nesting deeper than [`MAX_DEPTH`] or more than
    /// [`MAX_VALUES`] values (both of which [`read`] would refuse) is an error, and `out` then holds
    /// part of the value.
    pub fn write(&self, out: &mut Vec<u8>) -> Result<(), Error> {
        let mut values = 0;
        self.write_at(out, 0, &mut values)
    }

    fn write_at(&self, out: &mut Vec<u8>, depth: usize, values: &mut usize) -> Result<(), Error> {
        if depth > MAX_DEPTH {
            return Err(Error::BadInput("AMF0 values are nested more than 64 deep"));
        }
        *values += 1;
        if *values > MAX_VALUES {
            return Err(Error::BadInput("the AMF0 value holds more than 1 048 576 values"));
        }
        match self {
            Value::Number(n) => write_number(out, *n),
            Value::Boolean(b) => write_bool(out, *b),
            Value::String(s) => write_string(out, s)?,
            Value::Object(props) => {
                write_object_start(out);
                write_properties(out, props, depth, values)?;
                write_object_end(out);
            }
            Value::Null => write_null(out),
            Value::Undefined => out.push(marker::UNDEFINED),
            Value::EcmaArray(props) => {
                let count = u32::try_from(props.len()).map_err(|_| Error::BadInput("an ECMA array holds more than 2^32 - 1 items"))?;
                write_ecma_array_start(out, count);
                write_properties(out, props, depth, values)?;
                write_object_end(out);
            }
            Value::StrictArray(items) => {
                let count = u32::try_from(items.len()).map_err(|_| Error::BadInput("a strict array holds more than 2^32 - 1 items"))?;
                out.push(marker::STRICT_ARRAY);
                out.extend_from_slice(&count.to_be_bytes());
                for v in items {
                    v.write_at(out, depth + 1, values)?;
                }
            }
            Value::Date { millis, offset_minutes } => {
                out.push(marker::DATE);
                out.extend_from_slice(&millis.to_be_bytes());
                out.extend_from_slice(&offset_minutes.to_be_bytes());
            }
        }
        Ok(())
    }
}

fn write_properties(out: &mut Vec<u8>, props: &[(String, Value)], depth: usize, values: &mut usize) -> Result<(), Error> {
    for (k, v) in props {
        write_property_name(out, k)?;
        v.write_at(out, depth + 1, values)?;
    }
    Ok(())
}

/// Appends a number.
pub fn write_number(out: &mut Vec<u8>, n: f64) {
    out.push(marker::NUMBER);
    out.extend_from_slice(&n.to_be_bytes());
}

/// Appends a Boolean.
pub fn write_bool(out: &mut Vec<u8>, b: bool) {
    out.push(marker::BOOLEAN);
    out.push(u8::from(b));
}

/// Appends a string: with the string marker up to 65 535 bytes, with the long string marker
/// above that. A string longer than `u32::MAX` bytes is an error.
pub fn write_string(out: &mut Vec<u8>, s: &str) -> Result<(), Error> {
    if let Ok(len) = u16::try_from(s.len()) {
        out.push(marker::STRING);
        out.extend_from_slice(&len.to_be_bytes());
    } else {
        let len = u32::try_from(s.len()).map_err(|_| Error::BadInput("an AMF0 string is longer than 4 GiB"))?;
        out.push(marker::LONG_STRING);
        out.extend_from_slice(&len.to_be_bytes());
    }
    out.extend_from_slice(s.as_bytes());
    Ok(())
}

/// Appends null.
pub fn write_null(out: &mut Vec<u8>) {
    out.push(marker::NULL);
}

/// Appends the start of an anonymous object. Follow it with [`write_property_name`] and a value
/// for each property, then [`write_object_end`].
pub fn write_object_start(out: &mut Vec<u8>) {
    out.push(marker::OBJECT);
}

/// Appends the start of an ECMA array with `count` items. Follow it with [`write_property_name`]
/// and a value for each item, then [`write_object_end`].
pub fn write_ecma_array_start(out: &mut Vec<u8>, count: u32) {
    out.push(marker::ECMA_ARRAY);
    out.extend_from_slice(&count.to_be_bytes());
}

/// Appends a property name: a 16-bit length and the UTF-8 bytes, with no marker. A name over
/// 65 535 bytes is an error. An empty name is allowed: only an empty name followed by the object end
/// marker ends the properties, and no value starts with that marker.
pub fn write_property_name(out: &mut Vec<u8>, name: &str) -> Result<(), Error> {
    let len = u16::try_from(name.len()).map_err(|_| Error::BadInput("an AMF0 property name is longer than 65 535 bytes"))?;
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(name.as_bytes());
    Ok(())
}

/// Appends the end of an object's or an ECMA array's properties: an empty name and the object end
/// marker.
pub fn write_object_end(out: &mut Vec<u8>) {
    out.extend_from_slice(&[0, 0, marker::OBJECT_END]);
}

/// Reads one value from the start of `data` and returns it with the number of bytes it took.
pub fn read(data: &[u8]) -> Result<(Value, usize), Error> {
    let mut c = Cursor { data, pos: 0, values: 0 };
    let v = c.value(0)?;
    Ok((v, c.pos))
}

/// Reads values from `data` until it ends, as an RTMP command message holds them.
pub fn read_all(data: &[u8]) -> Result<Vec<Value>, Error> {
    let mut c = Cursor { data, pos: 0, values: 0 };
    let mut out = Vec::new();
    while c.pos < data.len() {
        out.push(c.value(0)?);
    }
    Ok(out)
}

struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
    /// Values decoded so far, against [`MAX_VALUES`].
    values: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], Error> {
        let end = self.pos.checked_add(n).filter(|&e| e <= self.data.len()).ok_or(Error::Truncated("an AMF0 value"))?;
        let s = &self.data[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    fn u8(&mut self) -> Result<u8, Error> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, Error> {
        let b = self.take(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }

    fn u32(&mut self) -> Result<u32, Error> {
        let b = self.take(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn f64(&mut self) -> Result<f64, Error> {
        let b = self.take(8)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(b);
        Ok(f64::from_be_bytes(a))
    }

    fn utf8(&mut self, len: usize) -> Result<String, Error> {
        let b = self.take(len)?;
        std::str::from_utf8(b).map(str::to_owned).map_err(|_| Error::Invalid("an AMF0 string is not UTF-8"))
    }

    fn value(&mut self, depth: usize) -> Result<Value, Error> {
        if depth > MAX_DEPTH {
            return Err(Error::Invalid("AMF0 values are nested more than 64 deep"));
        }
        self.values += 1;
        if self.values > MAX_VALUES {
            return Err(Error::Invalid("the AMF0 data holds more than 1 048 576 values"));
        }
        let m = self.u8()?;
        Ok(match m {
            marker::NUMBER => Value::Number(self.f64()?),
            marker::BOOLEAN => Value::Boolean(self.u8()? != 0),
            marker::STRING => {
                let len = self.u16()? as usize;
                Value::String(self.utf8(len)?)
            }
            marker::LONG_STRING => {
                let len = self.u32()? as usize;
                Value::String(self.utf8(len)?)
            }
            marker::OBJECT => Value::Object(self.properties(depth)?),
            marker::NULL => Value::Null,
            marker::UNDEFINED => Value::Undefined,
            marker::ECMA_ARRAY => {
                // The count is approximate (FLV specification E.4.4.4); the end marker ends the list.
                self.u32()?;
                Value::EcmaArray(self.properties(depth)?)
            }
            marker::STRICT_ARRAY => {
                let count = self.u32()? as usize;
                if count > self.data.len() - self.pos {
                    return Err(Error::Truncated("an AMF0 strict array"));
                }
                // Every value takes at least one byte, so `count` is bounded by the input, but a
                // Value is larger than a byte: reserve a small start and grow as values parse.
                let mut items = Vec::with_capacity(count.min(STRICT_ARRAY_RESERVE));
                for _ in 0..count {
                    items.push(self.value(depth + 1)?);
                }
                Value::StrictArray(items)
            }
            marker::DATE => {
                let millis = self.f64()?;
                let b = self.take(2)?;
                Value::Date { millis, offset_minutes: i16::from_be_bytes([b[0], b[1]]) }
            }
            marker::OBJECT_END => return Err(Error::Invalid("an AMF0 object end marker outside an object")),
            marker::MOVIECLIP | marker::REFERENCE => return Err(Error::Unsupported("AMF0 movie clips and references")),
            _ => return Err(Error::Unsupported("this AMF0 type marker")),
        })
    }

    fn properties(&mut self, depth: usize) -> Result<Vec<(String, Value)>, Error> {
        let mut props = Vec::new();
        loop {
            let len = self.u16()? as usize;
            if len == 0 && self.data.get(self.pos) == Some(&marker::OBJECT_END) {
                self.pos += 1;
                return Ok(props);
            }
            let name = self.utf8(len)?;
            let v = self.value(depth + 1)?;
            props.push((name, v));
        }
    }
}
