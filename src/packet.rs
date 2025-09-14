// Packet multiplexer: expose data and sync APIs and provide unified decode.

pub use crate::packet_data::{
    encode_packet, decode_packet, Meta, Decoded, SampleRateCode,
    DataPacketError,
};
pub use crate::packet_sync::{
    encode_sync, decode_sync, SyncDecodeError, SyncMessage,
};
// Re-export data and sync constants/types via this facade.

// Packet magic bytes (first byte) shared across modules
pub(crate) const DATA_PACKET_MAGIC: u8 = b'S';
pub(crate) const SYNC_PACKET_MAGIC: u8 = b'T';

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Message<'a> {
    // Time-sync control message wrapper
    Sync(SyncMessage),
    // Audio/data message (borrowed from input buffer)
    Data(Decoded<'a>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleFormat {
    F32,
    I16,
    U16,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SampleRate(pub u32);

# [cfg(feature="cpal")]
impl Into<cpal::SampleRate> for SampleRate {
    fn into(self) -> cpal::SampleRate {
        cpal::SampleRate(self.0)
    }
}

# [cfg(feature="cpal")]
impl From<cpal::SampleRate> for SampleRate {
    fn from(sr: cpal::SampleRate) -> Self {
        SampleRate(sr.0)
    }
}

/// Try to decode either a sync control message or an audio data message
/// by checking the first magic byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    UnknownMagic,
    Sync(SyncDecodeError),
    Data(DataPacketError),
}

impl core::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            DecodeError::UnknownMagic => write!(f, "unknown packet magic"),
            DecodeError::Sync(e) => write!(f, "sync decode error: {e}"),
            DecodeError::Data(e) => write!(f, "data decode error: {e}"),
        }
    }
}

pub fn decode_message(data: &[u8]) -> Result<Message<'_>, DecodeError> {
    if data.is_empty() { return Err(DecodeError::UnknownMagic); }
    match data[0] {
        SYNC_PACKET_MAGIC => crate::packet_sync::decode_sync(data)
            .map(Message::Sync)
            .map_err(DecodeError::Sync),
        DATA_PACKET_MAGIC => crate::packet_data::decode_packet(data)
            .map(Message::Data)
            .map_err(DecodeError::Data),
        _ => Err(DecodeError::UnknownMagic),
    }
}

pub fn respond_to_ping(socket: &std::net::UdpSocket, src_addr: std::net::SocketAddr, t0_ms: u64) {
    let t1 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_else(|_| std::time::Duration::from_millis(0))
        .as_millis() as u64;
    let t2 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_else(|_| std::time::Duration::from_millis(0))
        .as_millis() as u64;
    let pong = SyncMessage::Pong { t0_ms, t1_ms: t1, t2_ms: t2 };
    let v = encode_sync(&pong);
    let _ = socket.send_to(&v, src_addr);
}
