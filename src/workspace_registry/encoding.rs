use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

/// A previously observed directory identity. Use it to compare fresh observations
/// with a registration, never as proof that two path spellings still refer to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum DirectoryIdentity {
    #[cfg(unix)]
    Unix {
        #[serde(with = "hex_u64")]
        device: u64,
        #[serde(with = "hex_u64")]
        inode: u64,
    },
    #[cfg(windows)]
    Windows {
        #[serde(with = "hex_u64")]
        volume_serial_number: u64,
        #[serde(with = "hex_file_id")]
        file_id: [u8; 16],
    },
}

#[derive(Debug, Clone, Eq, Serialize, Deserialize)]
#[serde(try_from = "EncodedRoot", into = "EncodedRoot")]
pub(super) struct WorkspaceRoot(PathBuf);

impl WorkspaceRoot {
    pub(super) fn new(path: PathBuf) -> Result<Self, &'static str> {
        if !path.is_absolute() {
            return Err("root must be an absolute path on this platform");
        }
        if path.as_os_str().as_encoded_bytes().contains(&0) {
            return Err("root contains a NUL character");
        }
        Ok(Self(path))
    }

    pub(super) fn as_path(&self) -> &Path {
        &self.0
    }
}

impl PartialEq for WorkspaceRoot {
    fn eq(&self, other: &Self) -> bool {
        // Path equality skips some separators and interior `.` components.
        self.0.as_os_str() == other.0.as_os_str()
    }
}

#[derive(Serialize, Deserialize)]
#[serde(
    tag = "encoding",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
enum EncodedRoot {
    Utf8(String),
    UnixBytes(String),
    WindowsUtf16(String),
}

impl From<WorkspaceRoot> for EncodedRoot {
    fn from(root: WorkspaceRoot) -> Self {
        match root.0.into_os_string().into_string() {
            Ok(utf8) => Self::Utf8(utf8),
            #[cfg(unix)]
            Err(native) => {
                use std::os::unix::ffi::OsStrExt;
                Self::UnixBytes(encode_hex(native.as_bytes().iter().copied()))
            }
            #[cfg(windows)]
            Err(native) => {
                use std::os::windows::ffi::OsStrExt;
                Self::WindowsUtf16(encode_hex(native.encode_wide().flat_map(u16::to_be_bytes)))
            }
            #[cfg(not(any(unix, windows)))]
            Err(_) => unreachable!("unsupported non-UTF-8 Workspace root"),
        }
    }
}

impl TryFrom<EncodedRoot> for WorkspaceRoot {
    type Error = &'static str;

    fn try_from(encoded: EncodedRoot) -> Result<Self, Self::Error> {
        let native_path = match encoded {
            EncodedRoot::Utf8(value) => return Self::new(value.into()),
            #[cfg(unix)]
            EncodedRoot::UnixBytes(value) => {
                use std::os::unix::ffi::OsStringExt;
                std::ffi::OsString::from_vec(decode_hex(&value)?)
            }
            #[cfg(windows)]
            EncodedRoot::WindowsUtf16(value) => {
                use std::os::windows::ffi::OsStringExt;
                if value.len() % 4 != 0 {
                    return Err("Windows path must use four hex digits per code unit");
                }
                let wide: Vec<_> = decode_hex(&value)?
                    .chunks_exact(2)
                    .map(|pair| u16::from_be_bytes([pair[0], pair[1]]))
                    .collect();
                std::ffi::OsString::from_wide(&wide)
            }
            _ => return Err("Workspace root encoding does not match this platform"),
        };
        if native_path.to_str().is_some() {
            return Err("a Unicode Workspace root must use the utf8 encoding");
        }
        Self::new(native_path.into())
    }
}

fn encode_hex(bytes: impl IntoIterator<Item = u8>) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut hex = String::new();
    for byte in bytes {
        hex.push(char::from(DIGITS[usize::from(byte >> 4)]));
        hex.push(char::from(DIGITS[usize::from(byte & 0xf)]));
    }
    hex
}

fn decode_hex(value: &str) -> Result<Vec<u8>, &'static str> {
    fn nibble(digit: u8) -> Result<u8, &'static str> {
        match digit {
            b'0'..=b'9' => Ok(digit - b'0'),
            b'a'..=b'f' => Ok(digit - b'a' + 10),
            _ => Err("expected lowercase hexadecimal digits"),
        }
    }

    let pairs = value.as_bytes().chunks_exact(2);
    if !pairs.remainder().is_empty() {
        return Err("expected an even number of hexadecimal digits");
    }
    pairs
        .map(|pair| Ok(nibble(pair[0])? << 4 | nibble(pair[1])?))
        .collect()
}

fn deserialize_hex<'de, const N: usize, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<[u8; N], D::Error> {
    let hex = String::deserialize(deserializer)?;
    if hex.len() != 2 * N {
        return Err(de::Error::invalid_length(
            hex.len(),
            &"a fixed-width hexadecimal identity",
        ));
    }
    decode_hex(&hex)
        .map_err(de::Error::custom)?
        .try_into()
        .map_err(|_| de::Error::custom("invalid hexadecimal identity width"))
}

mod hex_u64 {
    use super::*;

    pub(super) fn serialize<S: Serializer>(value: &u64, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&encode_hex(value.to_be_bytes()))
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
        deserialize_hex(deserializer).map(u64::from_be_bytes)
    }
}

#[cfg(windows)]
mod hex_file_id {
    use super::*;

    pub(super) fn serialize<S: Serializer>(
        value: &[u8; 16],
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&encode_hex(*value))
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<[u8; 16], D::Error> {
        deserialize_hex(deserializer)
    }
}
