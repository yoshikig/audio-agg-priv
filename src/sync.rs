// Lightweight control messages for time synchronization.
// Shared over the same UDP socket as audio.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncMessage {
    Ping { t0_ms: u64 },
    Pong { t0_ms: u64, t1_ms: u64, t2_ms: u64 },
}

const SYNC_MAGIC: u8 = b'T';
const SYNC_VERSION: u8 = 1;
const TYPE_PING: u8 = 1;
const TYPE_PONG: u8 = 2;

pub fn encode(msg: SyncMessage) -> Vec<u8> {
    match msg {
        SyncMessage::Ping { t0_ms } => {
            let mut v = Vec::with_capacity(1 + 1 + 1 + 8);
            v.push(SYNC_MAGIC);
            v.push(SYNC_VERSION);
            v.push(TYPE_PING);
            v.extend_from_slice(&t0_ms.to_be_bytes());
            v
        }
        SyncMessage::Pong { t0_ms, t1_ms, t2_ms } => {
            let mut v = Vec::with_capacity(1 + 1 + 1 + 8 + 8 + 8);
            v.push(SYNC_MAGIC);
            v.push(SYNC_VERSION);
            v.push(TYPE_PONG);
            v.extend_from_slice(&t0_ms.to_be_bytes());
            v.extend_from_slice(&t1_ms.to_be_bytes());
            v.extend_from_slice(&t2_ms.to_be_bytes());
            v
        }
    }
}

pub fn decode(data: &[u8]) -> Option<SyncMessage> {
    if data.len() < 3 { return None; }
    if data[0] != SYNC_MAGIC || data[1] != SYNC_VERSION { return None; }
    match data[2] {
        TYPE_PING => {
            if data.len() < 3 + 8 { return None; }
            let mut b = [0u8; 8];
            b.copy_from_slice(&data[3..11]);
            Some(SyncMessage::Ping { t0_ms: u64::from_be_bytes(b) })
        }
        TYPE_PONG => {
            if data.len() < 3 + 8 + 8 + 8 { return None; }
            let mut b0 = [0u8; 8];
            let mut b1 = [0u8; 8];
            let mut b2 = [0u8; 8];
            b0.copy_from_slice(&data[3..11]);
            b1.copy_from_slice(&data[11..19]);
            b2.copy_from_slice(&data[19..27]);
            Some(SyncMessage::Pong {
                t0_ms: u64::from_be_bytes(b0),
                t1_ms: u64::from_be_bytes(b1),
                t2_ms: u64::from_be_bytes(b2),
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_ping() {
        let m = SyncMessage::Ping { t0_ms: 123 };
        let v = encode(m);
        let d = decode(&v).unwrap();
        assert_eq!(m, d);
    }

    #[test]
    fn roundtrip_pong() {
        let m = SyncMessage::Pong { t0_ms: 1, t1_ms: 2, t2_ms: 3 };
        let v = encode(m);
        let d = decode(&v).unwrap();
        assert_eq!(m, d);
    }
}

