use crate::packet::SYNC_PACKET_MAGIC;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncMessage {
  Ping { t0_ms: u64 },
  Pong { t0_ms: u64, t1_ms: u64, t2_ms: u64 },
}
const SYNC_VERSION: u8 = 1;
const TYPE_PING: u8 = 1;
const TYPE_PONG: u8 = 2;

// Encode a sync message to bytes.
pub fn encode_sync(msg: &SyncMessage) -> Vec<u8> {
  match *msg {
    SyncMessage::Ping { t0_ms } => {
      let mut v = Vec::with_capacity(1 + 1 + 1 + 8);
      v.push(SYNC_PACKET_MAGIC);
      v.push(SYNC_VERSION);
      v.push(TYPE_PING);
      v.extend_from_slice(&t0_ms.to_be_bytes());
      v
    }
    SyncMessage::Pong {
      t0_ms,
      t1_ms,
      t2_ms,
    } => {
      let mut v = Vec::with_capacity(1 + 1 + 1 + 8 + 8 + 8);
      v.push(SYNC_PACKET_MAGIC);
      v.push(SYNC_VERSION);
      v.push(TYPE_PONG);
      v.extend_from_slice(&t0_ms.to_be_bytes());
      v.extend_from_slice(&t1_ms.to_be_bytes());
      v.extend_from_slice(&t2_ms.to_be_bytes());
      v
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncDecodeError {
  TooShort,
  BadMagic,
  BadVersion,
  UnknownType,
}

impl core::fmt::Display for SyncDecodeError {
  fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
    match self {
      SyncDecodeError::TooShort => write!(f, "sync packet too short"),
      SyncDecodeError::BadMagic => write!(f, "bad sync packet magic"),
      SyncDecodeError::BadVersion => {
        write!(f, "unsupported sync packet version")
      }
      SyncDecodeError::UnknownType => write!(f, "unknown sync packet type"),
    }
  }
}

// Decode only sync messages; data messages are handled in packet.rs
pub fn decode_sync(data: &[u8]) -> Result<SyncMessage, SyncDecodeError> {
  if data.is_empty() {
    return Err(SyncDecodeError::TooShort);
  }
  if data[0] != SYNC_PACKET_MAGIC {
    return Err(SyncDecodeError::BadMagic);
  }
  if data.len() < 2 {
    return Err(SyncDecodeError::TooShort);
  }
  if data[1] != SYNC_VERSION {
    return Err(SyncDecodeError::BadVersion);
  }
  if data.len() < 3 {
    return Err(SyncDecodeError::TooShort);
  }
  match data[2] {
    TYPE_PING => {
      if data.len() < 3 + 8 {
        return Err(SyncDecodeError::TooShort);
      }
      let mut b = [0u8; 8];
      b.copy_from_slice(&data[3..11]);
      Ok(SyncMessage::Ping {
        t0_ms: u64::from_be_bytes(b),
      })
    }
    TYPE_PONG => {
      if data.len() < 3 + 8 + 8 + 8 {
        return Err(SyncDecodeError::TooShort);
      }
      let mut b0 = [0u8; 8];
      let mut b1 = [0u8; 8];
      let mut b2 = [0u8; 8];
      b0.copy_from_slice(&data[3..11]);
      b1.copy_from_slice(&data[11..19]);
      b2.copy_from_slice(&data[19..27]);
      Ok(SyncMessage::Pong {
        t0_ms: u64::from_be_bytes(b0),
        t1_ms: u64::from_be_bytes(b1),
        t2_ms: u64::from_be_bytes(b2),
      })
    }
    _ => Err(SyncDecodeError::UnknownType),
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::packet::{
    Message, Meta, SampleFormat, SampleRate, SyncMessage, decode_message,
    encode_packet,
  };

  #[test]
  fn roundtrip_ping() {
    let m = SyncMessage::Ping { t0_ms: 123 };
    let v = encode_sync(&m);
    let d = decode_sync(&v).unwrap();
    assert_eq!(m, d);
  }

  #[test]
  fn roundtrip_pong() {
    let m = SyncMessage::Pong {
      t0_ms: 1,
      t1_ms: 2,
      t2_ms: 3,
    };
    let v = encode_sync(&m);
    let d = decode_sync(&v).unwrap();
    assert_eq!(m, d);
  }

  #[test]
  fn decode_data_message_via_packet() {
    let meta = Meta {
      channels: 2,
      sample_rate: SampleRate(48_000),
      sample_format: SampleFormat::F32,
    };
    let pkt = encode_packet(1, b"xyz", meta, 42);
    let m = decode_message(&pkt).unwrap();
    match m {
      Message::Data(dm) => {
        assert_eq!(dm.seq, 1);
        assert_eq!(dm.timestamp_ms, 42);
        assert_eq!(dm.payload, b"xyz");
      }
      _ => panic!("expected data message"),
    }
  }
}
