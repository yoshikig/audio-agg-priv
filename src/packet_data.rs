// src/packet_data.rs

use crate::packet::{SampleFormat, SampleRate, DATA_PACKET_MAGIC};

// IMPORTANT: Bump PACKET_VERSION whenever the on-wire packet header/layout changes.
const PACKET_VERSION: u8 = 2;

/// Data packet format utilities (audio payloads).
///
/// Packet layout (big-endian):
/// - 1 byte : magic (fixed to b'S')
/// - 1 byte : version (bumped when layout changes)
/// - 2 bytes: payload length (u16)
/// - 1 byte : channels
/// - 1 byte : sample rate code (enum, see `SampleRateCode`)
/// - 1 byte : sample format code (1=F32, 2=I16, 3=U16, 4=U32, 0=unknown)
/// - 1 byte : reserved (dummy)
/// - 8 bytes: sequence number (u64)
/// - 8 bytes: timestamp (u64, ms since UNIX epoch)
/// - N bytes: payload
const HEADER_LEN: usize = 2 + 2 + 1 + 1 + 1 + 1 + 8 + 8; // 24 bytes

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataPacketError {
  TooShort,
  BadMagic,
  BadVersion,
  LengthMismatch,
}

impl core::fmt::Display for DataPacketError {
  fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
    match self {
      DataPacketError::TooShort => write!(f, "packet too short"),
      DataPacketError::BadMagic => write!(f, "bad data packet magic"),
      DataPacketError::BadVersion => {
        write!(f, "unsupported data packet version")
      }
      DataPacketError::LengthMismatch => {
        write!(f, "declared length exceeds buffer")
      }
    }
  }
}

/// Encoded sample-rate choices used on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum SampleRateCode {
  Unknown = 0,
  Hz8000 = 1,
  Hz16000 = 2,
  Hz22050 = 3,
  Hz24000 = 4,
  Hz32000 = 5,
  Hz44100 = 6,
  Hz48000 = 7,
  Hz88200 = 8,
  Hz96000 = 9,
  Hz176400 = 10,
  Hz192000 = 11,
}

impl SampleRateCode {
  pub fn from_hz(hz: u32) -> Self {
    match hz {
      8_000 => Self::Hz8000,
      16_000 => Self::Hz16000,
      22_050 => Self::Hz22050,
      24_000 => Self::Hz24000,
      32_000 => Self::Hz32000,
      44_100 => Self::Hz44100,
      48_000 => Self::Hz48000,
      88_200 => Self::Hz88200,
      96_000 => Self::Hz96000,
      176_400 => Self::Hz176400,
      192_000 => Self::Hz192000,
      _ => Self::Unknown,
    }
  }

  pub fn to_hz(self) -> u32 {
    match self {
      Self::Unknown => 0,
      Self::Hz8000 => 8_000,
      Self::Hz16000 => 16_000,
      Self::Hz22050 => 22_050,
      Self::Hz24000 => 24_000,
      Self::Hz32000 => 32_000,
      Self::Hz44100 => 44_100,
      Self::Hz48000 => 48_000,
      Self::Hz88200 => 88_200,
      Self::Hz96000 => 96_000,
      Self::Hz176400 => 176_400,
      Self::Hz192000 => 192_000,
    }
  }

  pub fn from_code(code: u8) -> Self {
    match code {
      1 => Self::Hz8000,
      2 => Self::Hz16000,
      3 => Self::Hz22050,
      4 => Self::Hz24000,
      5 => Self::Hz32000,
      6 => Self::Hz44100,
      7 => Self::Hz48000,
      8 => Self::Hz88200,
      9 => Self::Hz96000,
      10 => Self::Hz176400,
      11 => Self::Hz192000,
      _ => Self::Unknown,
    }
  }

  pub fn code(self) -> u8 {
    self as u8
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Meta {
  pub channels: u8,
  pub sample_rate: SampleRate,
  pub sample_format: SampleFormat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decoded<'a> {
  pub seq: u64,
  pub timestamp_ms: u64,
  pub meta: Meta,
  pub payload: &'a [u8],
}

/// Encodes a sequence number, metadata and payload into a packet buffer.
pub fn encode_packet(
  seq: u64,
  payload: &[u8],
  meta: Meta,
  timestamp_ms: u64,
) -> Vec<u8> {
  let len: u16 = payload.len().min(u16::MAX as usize) as u16;
  let mut buf = Vec::with_capacity(HEADER_LEN + payload.len());
  buf.push(DATA_PACKET_MAGIC);
  buf.push(PACKET_VERSION);
  buf.extend_from_slice(&len.to_be_bytes());
  buf.push(meta.channels);
  // sample rate encoded as enum code, 1 byte
  let sr_code = SampleRateCode::from_hz(meta.sample_rate.0).code();
  buf.push(sr_code);
  // sample format encoded as 1 byte
  let sf_code: u8 = match meta.sample_format {
    SampleFormat::F32 => 1,
    SampleFormat::I16 => 2,
    SampleFormat::U16 => 3,
    SampleFormat::U32 => 4,
    _ => 0,
  };
  buf.push(sf_code);
  buf.push(0); // reserved/dummy
  buf.extend_from_slice(&seq.to_be_bytes());
  buf.extend_from_slice(&timestamp_ms.to_be_bytes());
  buf.extend_from_slice(payload);
  buf
}

/// Decodes a packet into `Decoded { seq, meta, payload }`.
/// Returns a slice into the original buffer for the payload to avoid allocation.
pub fn decode_packet<'a>(
  data: &'a [u8],
) -> Result<Decoded<'a>, DataPacketError> {
  if data.len() < HEADER_LEN {
    return Err(DataPacketError::TooShort);
  }
  if data[0] != DATA_PACKET_MAGIC {
    return Err(DataPacketError::BadMagic);
  }
  if data[1] != PACKET_VERSION {
    return Err(DataPacketError::BadVersion);
  }

  let mut len_buf = [0u8; 2];
  len_buf.copy_from_slice(&data[2..4]);
  let payload_len = u16::from_be_bytes(len_buf) as usize;

  let channels = data[4];
  let sample_rate_code = data[5];
  let sample_format_code = data[6];
  // data[7] is reserved/dummy

  let mut seq_buf = [0u8; 8];
  seq_buf.copy_from_slice(&data[8..16]);
  let seq = u64::from_be_bytes(seq_buf);

  let mut ts_buf = [0u8; 8];
  ts_buf.copy_from_slice(&data[16..24]);
  let timestamp_ms = u64::from_be_bytes(ts_buf);

  if data.len() < HEADER_LEN + payload_len {
    return Err(DataPacketError::LengthMismatch);
  }
  let payload = &data[HEADER_LEN..HEADER_LEN + payload_len];
  let sample_rate =
    SampleRate(SampleRateCode::from_code(sample_rate_code).to_hz());
  let sample_format = match sample_format_code {
    1 => SampleFormat::F32,
    2 => SampleFormat::I16,
    3 => SampleFormat::U16,
    4 => SampleFormat::U32,
    _ => SampleFormat::F32, // default if unknown
  };
  Ok(Decoded {
    seq,
    timestamp_ms,
    meta: Meta {
      channels,
      sample_rate,
      sample_format,
    },
    payload,
  })
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn encode_then_decode_roundtrip() {
    let seq = 1234567890123456789u64;
    let payload = b"hello world";
    let meta = Meta {
      channels: 2,
      sample_rate: SampleRate(48_000),
      sample_format: SampleFormat::F32,
    };
    let pkt = encode_packet(seq, payload, meta, 42);
    let d = decode_packet(&pkt).expect("decode ok");
    assert_eq!(d.seq, seq);
    assert_eq!(d.timestamp_ms, 42);
    assert_eq!(d.meta, meta);
    assert_eq!(d.payload, payload);
  }

  #[test]
  fn enforces_length_and_magic_version() {
    let meta = Meta {
      channels: 1,
      sample_rate: SampleRate(44_000),
      sample_format: SampleFormat::I16,
    };
    let pkt = encode_packet(1, b"abc", meta, 0);
    let mut bad_magic = pkt.clone();
    bad_magic[0] = 0; // break magic
    assert_eq!(decode_packet(&bad_magic), Err(DataPacketError::BadMagic));

    let mut bad_version = pkt.clone();
    bad_version[1] = PACKET_VERSION.wrapping_add(1); // wrong version
    assert_eq!(
      decode_packet(&bad_version),
      Err(DataPacketError::BadVersion)
    );

    let mut short = pkt.clone();
    short.truncate(HEADER_LEN + 1);
    assert_eq!(decode_packet(&short), Err(DataPacketError::LengthMismatch));
  }
}
