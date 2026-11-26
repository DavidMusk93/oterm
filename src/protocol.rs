use bytes::{Buf, BufMut, Bytes, BytesMut};
use serde::{Deserialize, Serialize};

// Client -> Server opcodes
pub const OP_INPUT: u8 = 0x01;
pub const OP_RESIZE: u8 = 0x02;
pub const OP_ACK: u8 = 0x03;
pub const OP_PING: u8 = 0x04;
pub const OP_ATTACH: u8 = 0x05;
pub const OP_ACTIVITY: u8 = 0x06;

// Server -> Client opcodes
pub const OP_OUTPUT: u8 = 0x11;
pub const OP_SNAPSHOT: u8 = 0x12;
pub const OP_PONG: u8 = 0x13;
pub const OP_SESSION: u8 = 0x14;
pub const OP_AUTH_LOCK: u8 = 0x15;
pub const OP_ERROR: u8 = 0x16;

pub const SNAPSHOT_FLAG_FULL_RESET: u8 = 0x01;

#[derive(Debug, Clone, Deserialize)]
pub struct AttachPayload {
    pub session_id: String,
    #[serde(default)]
    pub last_seq: u64,
    #[serde(default = "default_cols")]
    pub cols: u16,
    #[serde(default = "default_rows")]
    pub rows: u16,
}

fn default_cols() -> u16 {
    80
}

fn default_rows() -> u16 {
    24
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    pub name: String,
    pub cols: u16,
    pub rows: u16,
    pub alive: bool,
    pub exit_code: Option<i32>,
    pub created_at_ms: u64,
    pub clients: usize,
    pub end_seq: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct AuthLockPayload {
    pub reason: String,
    pub ttl: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct ErrorPayload {
    pub code: String,
    pub message: String,
}

#[derive(Debug)]
pub enum ClientFrame {
    Input(Bytes),
    Resize { cols: u16, rows: u16 },
    Ack { seq: u64 },
    Ping { client_ts_ms: u64 },
    Attach(AttachPayload),
    Activity,
}

pub fn decode_client_frame(mut data: Bytes) -> Result<ClientFrame, String> {
    if data.is_empty() {
        return Err("empty binary frame".into());
    }
    let op = data.get_u8();
    match op {
        OP_INPUT => Ok(ClientFrame::Input(data)),
        OP_RESIZE => {
            if data.len() < 4 {
                return Err("resize frame too short".into());
            }
            let cols = data.get_u16().clamp(2, 1000);
            let rows = data.get_u16().clamp(1, 500);
            Ok(ClientFrame::Resize { cols, rows })
        }
        OP_ACK => {
            if data.len() < 8 {
                return Err("ack frame too short".into());
            }
            Ok(ClientFrame::Ack {
                seq: data.get_u64(),
            })
        }
        OP_PING => {
            if data.len() < 8 {
                return Err("ping frame too short".into());
            }
            Ok(ClientFrame::Ping {
                client_ts_ms: data.get_u64(),
            })
        }
        OP_ATTACH => {
            let payload: AttachPayload =
                serde_json::from_slice(&data).map_err(|e| format!("invalid attach json: {e}"))?;
            Ok(ClientFrame::Attach(payload))
        }
        OP_ACTIVITY => Ok(ClientFrame::Activity),
        other => Err(format!("unknown client opcode 0x{other:02x}")),
    }
}

pub fn encode_output(end_seq: u64, payload: &[u8]) -> Bytes {
    let mut buf = BytesMut::with_capacity(1 + 8 + payload.len());
    buf.put_u8(OP_OUTPUT);
    buf.put_u64(end_seq);
    buf.extend_from_slice(payload);
    buf.freeze()
}

pub fn encode_snapshot(end_seq: u64, full_reset: bool, payload: &[u8]) -> Bytes {
    let mut buf = BytesMut::with_capacity(1 + 8 + 1 + payload.len());
    buf.put_u8(OP_SNAPSHOT);
    buf.put_u64(end_seq);
    buf.put_u8(if full_reset { SNAPSHOT_FLAG_FULL_RESET } else { 0 });
    buf.extend_from_slice(payload);
    buf.freeze()
}

pub fn encode_pong(client_ts_ms: u64, auth_ttl_secs: u32) -> Bytes {
    let mut buf = BytesMut::with_capacity(1 + 8 + 4);
    buf.put_u8(OP_PONG);
    buf.put_u64(client_ts_ms);
    buf.put_u32(auth_ttl_secs);
    buf.freeze()
}

pub fn encode_json_frame<T: Serialize>(op: u8, payload: &T) -> Bytes {
    let json = serde_json::to_vec(payload).unwrap_or_default();
    let mut buf = BytesMut::with_capacity(1 + json.len());
    buf.put_u8(op);
    buf.extend_from_slice(&json);
    buf.freeze()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_resize_and_ack() {
        let mut r = BytesMut::new();
        r.put_u8(OP_RESIZE);
        r.put_u16(120);
        r.put_u16(40);
        match decode_client_frame(r.freeze()).unwrap() {
            ClientFrame::Resize { cols, rows } => {
                assert_eq!((cols, rows), (120, 40));
            }
            _ => panic!("wrong variant"),
        }

        let mut a = BytesMut::new();
        a.put_u8(OP_ACK);
        a.put_u64(987654321);
        match decode_client_frame(a.freeze()).unwrap() {
            ClientFrame::Ack { seq } => assert_eq!(seq, 987654321),
            _ => panic!("wrong variant"),
        }

        let act = Bytes::from_static(&[OP_ACTIVITY]);
        assert!(matches!(
            decode_client_frame(act).unwrap(),
            ClientFrame::Activity
        ));
    }

    #[test]
    fn encode_output_header() {
        let frame = encode_output(42, b"abc");
        assert_eq!(frame[0], OP_OUTPUT);
        assert_eq!(u64::from_be_bytes(frame[1..9].try_into().unwrap()), 42);
        assert_eq!(&frame[9..], b"abc");
    }
}
