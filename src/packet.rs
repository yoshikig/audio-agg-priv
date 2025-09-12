#[cfg(feature = "cpal")]
pub use cpal::{SampleFormat, SampleRate};

#[cfg(not(feature = "cpal"))]
mod compat {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum SampleFormat {
        F32,
        I16,
        U16,
        Unknown,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct SampleRate(pub u32);
}

#[cfg(not(feature = "cpal"))]
pub use compat::{SampleFormat, SampleRate};

/// Simple packet format utilities.
///
/// Packet layout (big-endian):
/// - 2 bytes: magic (unique header), fixed to b"SS"
/// - 2 bytes: payload length (u16)
/// - 1 byte : channels
/// - 1 byte : sample rate in kHz (rounded)
/// - 1 byte : sample format code (1=F32, 2=I16, 3=U16, 0=unknown)
/// - 1 byte : reserved (dummy)
/// - 8 bytes: sequence number (u64)
/// - N bytes: payload
///
/// These helpers are used by both sender and receiver.

pub const MAGIC: [u8; 2] = *b"SS";
pub const HEADER_LEN: usize = 2 + 2 + 1 + 1 + 1 + 1 + 8; // 16 bytes

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacketError {
    TooShort,
    BadMagic,
    LengthMismatch,
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
    pub meta: Meta,
    pub payload: &'a [u8],
}

/// Encodes a sequence number, metadata and payload into a packet buffer.
pub fn encode_packet(seq: u64, payload: &[u8], meta: Meta) -> Vec<u8> {
    let len: u16 = payload.len().min(u16::MAX as usize) as u16;
    let mut buf = Vec::with_capacity(HEADER_LEN + payload.len());
    buf.extend_from_slice(&MAGIC);
    buf.extend_from_slice(&len.to_be_bytes());
    buf.push(meta.channels);
    // sample rate encoded as kHz (rounded), 1 byte
    let sr_khz: u8 = (((meta.sample_rate.0 + 500) / 1000).min(255)) as u8;
    buf.push(sr_khz);
    // sample format encoded as 1 byte
    let sf_code: u8 = match meta.sample_format {
        SampleFormat::F32 => 1,
        SampleFormat::I16 => 2,
        SampleFormat::U16 => 3,
        _ => 0,
    };
    buf.push(sf_code);
    buf.push(0); // reserved/dummy
    buf.extend_from_slice(&seq.to_be_bytes());
    buf.extend_from_slice(payload);
    buf
}

/// Decodes a packet into `Decoded { seq, meta, payload }`.
/// Returns a slice into the original buffer for the payload to avoid allocation.
pub fn decode_packet<'a>(data: &'a [u8]) -> Result<Decoded<'a>, PacketError> {
    if data.len() < HEADER_LEN {
        return Err(PacketError::TooShort);
    }
    if data[0..2] != MAGIC {
        return Err(PacketError::BadMagic);
    }
    let mut len_buf = [0u8; 2];
    len_buf.copy_from_slice(&data[2..4]);
    let payload_len = u16::from_be_bytes(len_buf) as usize;

    let channels = data[4];
    let sample_rate_khz = data[5];
    let sample_format_code = data[6];
    // data[7] is reserved/dummy

    let mut seq_buf = [0u8; 8];
    seq_buf.copy_from_slice(&data[8..16]);
    let seq = u64::from_be_bytes(seq_buf);

    if data.len() < HEADER_LEN + payload_len {
        return Err(PacketError::LengthMismatch);
    }
    let payload = &data[HEADER_LEN..HEADER_LEN + payload_len];
    let sample_rate = SampleRate((sample_rate_khz as u32) * 1000);
    let sample_format = match sample_format_code {
        1 => SampleFormat::F32,
        2 => SampleFormat::I16,
        3 => SampleFormat::U16,
        _ => SampleFormat::F32, // default if unknown
    };
    Ok(Decoded {
        seq,
        meta: Meta { channels, sample_rate, sample_format },
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
        let pkt = encode_packet(seq, payload, meta);
        let d = decode_packet(&pkt).expect("decode ok");
        assert_eq!(d.seq, seq);
        assert_eq!(d.meta, meta);
        assert_eq!(d.payload, payload);
    }

    #[test]
    fn decode_rejects_short_packets() {
        assert_eq!(decode_packet(&[]), Err(PacketError::TooShort));
        let mut v = vec![0u8; HEADER_LEN - 1];
        assert_eq!(decode_packet(&v), Err(PacketError::TooShort));
        v.resize(HEADER_LEN, 0);
        assert_eq!(decode_packet(&v), Err(PacketError::BadMagic));
    }

    #[test]
    fn enforces_length_and_magic() {
        let meta = Meta {
            channels: 1,
            sample_rate: SampleRate(44_000),
            sample_format: SampleFormat::I16,
        };
        let pkt = encode_packet(1, b"abc", meta);
        let mut bad = pkt.clone();
        bad[0] = 0; // break magic
        assert_eq!(decode_packet(&bad), Err(PacketError::BadMagic));

        let mut short = pkt.clone();
        short.truncate(HEADER_LEN + 1); // says 3 but only 1 provided
        assert_eq!(decode_packet(&short), Err(PacketError::LengthMismatch));
    }
}
