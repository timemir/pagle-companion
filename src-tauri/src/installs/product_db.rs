//! Battle.net's `product.db`, decoded with the minimal protobuf schema of CMP
//! Contracts (`product.db`); every other field is skipped:
//!
//! ```protobuf
//! message ProductDb { repeated Product products = 1; }
//! message Product   { string uid = 1; string productCode = 2; Client client = 3; }
//! message Client    { string location = 1; string name = 13; }
//! ```
//!
//! Only `location` (the install root) and `name` (the flavor folder's name)
//! are kept: a product with both is a candidate install (CMP-R11).

use std::fmt;

/// One product entry with an install location and a subfolder name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub location: String,
    pub name: String,
}

/// Why a `product.db` couldn't be decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodeError(&'static str);

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for DecodeError {}

/// Decodes a whole `product.db` into its candidates, in file order.
pub fn decode(bytes: &[u8]) -> Result<Vec<Candidate>, DecodeError> {
    let mut candidates = Vec::new();
    for field in Fields::new(bytes) {
        let (number, value) = field?;
        if number == 1
            && let Some(candidate) = decode_product(value.bytes()?)?
        {
            candidates.push(candidate);
        }
    }
    Ok(candidates)
}

fn decode_product(bytes: &[u8]) -> Result<Option<Candidate>, DecodeError> {
    let mut found = None;
    for field in Fields::new(bytes) {
        let (number, value) = field?;
        if number == 3 {
            // A repeated non-repeated field: the last one wins, as in protobuf.
            found = decode_client(value.bytes()?)?;
        }
    }
    Ok(found)
}

fn decode_client(bytes: &[u8]) -> Result<Option<Candidate>, DecodeError> {
    let mut location = None;
    let mut name = None;
    for field in Fields::new(bytes) {
        match field? {
            (1, value) => location = Some(value.string()?),
            (13, value) => name = Some(value.string()?),
            _ => {}
        }
    }
    Ok(match (location, name) {
        (Some(location), Some(name)) if !location.is_empty() && !name.is_empty() => {
            Some(Candidate { location, name })
        }
        _ => None,
    })
}

/// A field's value, by wire type.
enum Value<'a> {
    Scalar,
    Bytes(&'a [u8]),
}

impl<'a> Value<'a> {
    fn bytes(self) -> Result<&'a [u8], DecodeError> {
        match self {
            Value::Bytes(bytes) => Ok(bytes),
            Value::Scalar => Err(DecodeError("a message field isn't length-delimited")),
        }
    }

    fn string(self) -> Result<String, DecodeError> {
        String::from_utf8(self.bytes()?.to_vec())
            .map_err(|_| DecodeError("a string field isn't UTF-8"))
    }
}

/// The fields of one message, in order. Stops after the first error.
struct Fields<'a> {
    rest: &'a [u8],
    failed: bool,
}

impl<'a> Fields<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self {
            rest: bytes,
            failed: false,
        }
    }

    fn next_field(&mut self) -> Result<(u64, Value<'a>), DecodeError> {
        let key = self.varint()?;
        let number = key >> 3;
        if number == 0 {
            return Err(DecodeError("a field number is 0"));
        }
        let value = match key & 7 {
            0 => {
                self.varint()?;
                Value::Scalar
            }
            1 => {
                self.take(8)?;
                Value::Scalar
            }
            2 => {
                let len = usize::try_from(self.varint()?)
                    .map_err(|_| DecodeError("a length is too large"))?;
                Value::Bytes(self.take(len)?)
            }
            5 => {
                self.take(4)?;
                Value::Scalar
            }
            _ => return Err(DecodeError("an unknown wire type")),
        };
        Ok((number, value))
    }

    fn varint(&mut self) -> Result<u64, DecodeError> {
        let mut value = 0u64;
        for (i, &byte) in self.rest.iter().enumerate().take(10) {
            value |= u64::from(byte & 0x7f) << (7 * i);
            if byte & 0x80 == 0 {
                self.rest = &self.rest[i + 1..];
                return Ok(value);
            }
        }
        Err(DecodeError("a varint is cut off or too long"))
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], DecodeError> {
        if len > self.rest.len() {
            return Err(DecodeError("a field runs past the end"));
        }
        let (taken, rest) = self.rest.split_at(len);
        self.rest = rest;
        Ok(taken)
    }
}

impl<'a> Iterator for Fields<'a> {
    type Item = Result<(u64, Value<'a>), DecodeError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed || self.rest.is_empty() {
            return None;
        }
        let field = self.next_field();
        self.failed = field.is_err();
        Some(field)
    }
}

/// A protobuf writer for test fixtures: just enough to build `product.db`s
/// with the fields Battle.net writes around the ones the Companion reads.
#[cfg(test)]
pub mod fixture {
    pub fn varint(out: &mut Vec<u8>, mut value: u64) {
        loop {
            let byte = (value & 0x7f) as u8;
            value >>= 7;
            if value == 0 {
                out.push(byte);
                return;
            }
            out.push(byte | 0x80);
        }
    }

    pub fn bytes_field(out: &mut Vec<u8>, number: u64, bytes: &[u8]) {
        varint(out, number << 3 | 2);
        varint(out, bytes.len() as u64);
        out.extend_from_slice(bytes);
    }

    pub fn varint_field(out: &mut Vec<u8>, number: u64, value: u64) {
        varint(out, number << 3);
        varint(out, value);
    }

    /// One product entry, padded with unknown fields of every wire type.
    pub fn product(uid: &str, location: &str, name: Option<&str>) -> Vec<u8> {
        let mut client = Vec::new();
        bytes_field(&mut client, 1, location.as_bytes());
        bytes_field(&mut client, 2, b"eu");
        varint_field(&mut client, 3, 2);
        bytes_field(&mut client, 8, b"enUS");
        if let Some(name) = name {
            bytes_field(&mut client, 13, name.as_bytes());
        }
        let mut product = Vec::new();
        bytes_field(&mut product, 1, uid.as_bytes());
        bytes_field(&mut product, 2, b"wow");
        bytes_field(&mut product, 3, &client);
        // An unknown nested message, a fixed64 and a fixed32.
        bytes_field(&mut product, 4, &[0x0a, 0x02, b'o', b'k']);
        varint(&mut product, 5 << 3 | 1);
        product.extend_from_slice(&[0; 8]);
        varint(&mut product, 6 << 3 | 5);
        product.extend_from_slice(&[0; 4]);
        product
    }

    /// A whole `product.db` of `products`, with an unknown top-level field.
    pub fn db(products: &[Vec<u8>]) -> Vec<u8> {
        let mut db = Vec::new();
        for product in products {
            bytes_field(&mut db, 1, product);
        }
        bytes_field(&mut db, 2, b"\x0a\x00");
        varint_field(&mut db, 3, 300);
        db
    }
}

#[cfg(test)]
mod tests {
    use super::fixture::*;
    use super::*;

    #[test]
    fn a_product_with_a_location_and_a_name_is_a_candidate() {
        // CMP-R11: every product entry with an install location and a
        // subfolder name gives a candidate install
        let db = db(&[
            product(
                "wow_classic_beta",
                "G:/World of Warcraft",
                Some("_classic_beta_"),
            ),
            product("agent", "C:/ProgramData/Battle.net/Agent", None),
            product("wow", "C:/Games/World of Warcraft", Some("_retail_")),
        ]);
        assert_eq!(
            decode(&db).unwrap(),
            [
                Candidate {
                    location: "G:/World of Warcraft".into(),
                    name: "_classic_beta_".into()
                },
                Candidate {
                    location: "C:/Games/World of Warcraft".into(),
                    name: "_retail_".into()
                },
            ]
        );
    }

    #[test]
    fn unknown_fields_are_skipped() {
        // CMP Contracts, product.db: unknown fields are skipped, at every
        // level and of every wire type
        let mut db = Vec::new();
        varint_field(&mut db, 9, u64::MAX);
        bytes_field(
            &mut db,
            1,
            &product("wow_classic_beta", "D:/WoW", Some("_classic_beta_")),
        );
        varint(&mut db, 7 << 3 | 5);
        db.extend_from_slice(&[1, 2, 3, 4]);
        assert_eq!(decode(&db).unwrap().len(), 1);
    }

    #[test]
    fn an_empty_file_has_no_candidates() {
        // CMP-R11: a file with no product entries gives no candidate
        assert_eq!(decode(&[]).unwrap(), []);
    }

    #[test]
    fn a_cut_off_file_fails_to_decode() {
        // CMP-R11: a product.db that can't be decoded is skipped
        let db = db(&[product(
            "wow_classic_beta",
            "G:/WoW",
            Some("_classic_beta_"),
        )]);
        assert!(decode(&db[..db.len() / 2]).is_err());
        assert!(decode(&[0x0a, 0xff]).is_err());
        assert!(decode(b"not a product database at all").is_err());
    }

    #[test]
    fn a_string_that_is_not_utf8_fails_to_decode() {
        // CMP-R11: a product.db that can't be decoded is skipped
        let mut client = Vec::new();
        bytes_field(&mut client, 1, &[0xff, 0xfe]);
        bytes_field(&mut client, 13, b"_classic_beta_");
        let mut product = Vec::new();
        bytes_field(&mut product, 3, &client);
        let mut db = Vec::new();
        bytes_field(&mut db, 1, &product);
        assert!(decode(&db).is_err());
    }
}
