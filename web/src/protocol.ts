// Binary WebSocket wire protocol v1 (mirrors src/protocol.rs)

export const OP_INPUT = 0x01;
export const OP_RESIZE = 0x02;
export const OP_ACK = 0x03;
export const OP_PING = 0x04;
export const OP_ATTACH = 0x05;

export const OP_OUTPUT = 0x11;
export const OP_SNAPSHOT = 0x12;
export const OP_PONG = 0x13;
export const OP_SESSION = 0x14;
export const OP_AUTH_LOCK = 0x15;
export const OP_ERROR = 0x16;

export const SNAPSHOT_FLAG_FULL_RESET = 0x01;

const textEncoder = new TextEncoder();
const textDecoder = new TextDecoder();

export interface SessionInfo {
  id: string;
  name: string;
  cols: number;
  rows: number;
  alive: boolean;
  exit_code: number | null;
  created_at_ms: number;
  clients: number;
  end_seq: number;
}

export type ServerFrame =
  | { type: "output"; endSeq: bigint; data: Uint8Array }
  | { type: "snapshot"; endSeq: bigint; fullReset: boolean; data: Uint8Array }
  | { type: "pong"; clientTsMs: bigint; authTtlSecs: number }
  | { type: "session"; info: SessionInfo }
  | { type: "auth_lock"; reason: string; ttl: number }
  | { type: "error"; code: string; message: string };

export function encodeInput(strOrBytes: string | Uint8Array): Uint8Array {
  const payload = typeof strOrBytes === "string" ? textEncoder.encode(strOrBytes) : strOrBytes;
  const out = new Uint8Array(1 + payload.length);
  out[0] = OP_INPUT;
  out.set(payload, 1);
  return out;
}

export function encodeResize(cols: number, rows: number): Uint8Array {
  const buf = new ArrayBuffer(5);
  const view = new DataView(buf);
  view.setUint8(0, OP_RESIZE);
  view.setUint16(1, cols, false);
  view.setUint16(3, rows, false);
  return new Uint8Array(buf);
}

export function encodeAck(seq: bigint): Uint8Array {
  const buf = new ArrayBuffer(9);
  const view = new DataView(buf);
  view.setUint8(0, OP_ACK);
  view.setBigUint64(1, seq, false);
  return new Uint8Array(buf);
}

export function encodePing(clientTsMs: bigint): Uint8Array {
  const buf = new ArrayBuffer(9);
  const view = new DataView(buf);
  view.setUint8(0, OP_PING);
  view.setBigUint64(1, clientTsMs, false);
  return new Uint8Array(buf);
}

export function encodeAttach(
  sessionId: string,
  lastSeq: bigint,
  cols: number,
  rows: number
): Uint8Array {
  const json = JSON.stringify({
    session_id: sessionId,
    last_seq: Number(lastSeq),
    cols,
    rows,
  });
  const payload = textEncoder.encode(json);
  const out = new Uint8Array(1 + payload.length);
  out[0] = OP_ATTACH;
  out.set(payload, 1);
  return out;
}

export function decodeServerFrame(buffer: ArrayBuffer): ServerFrame | null {
  const bytes = new Uint8Array(buffer);
  if (bytes.length === 0) return null;
  const view = new DataView(buffer);
  const op = view.getUint8(0);

  switch (op) {
    case OP_OUTPUT: {
      if (bytes.length < 9) return null;
      const endSeq = view.getBigUint64(1, false);
      return { type: "output", endSeq, data: bytes.subarray(9) };
    }
    case OP_SNAPSHOT: {
      if (bytes.length < 10) return null;
      const endSeq = view.getBigUint64(1, false);
      const flags = view.getUint8(9);
      return {
        type: "snapshot",
        endSeq,
        fullReset: (flags & SNAPSHOT_FLAG_FULL_RESET) !== 0,
        data: bytes.subarray(10),
      };
    }
    case OP_PONG: {
      if (bytes.length < 13) return null;
      const clientTsMs = view.getBigUint64(1, false);
      const authTtlSecs = view.getUint32(9, false);
      return { type: "pong", clientTsMs, authTtlSecs };
    }
    case OP_SESSION: {
      const info = JSON.parse(textDecoder.decode(bytes.subarray(1))) as SessionInfo;
      return { type: "session", info };
    }
    case OP_AUTH_LOCK: {
      const obj = JSON.parse(textDecoder.decode(bytes.subarray(1))) as {
        reason: string;
        ttl: number;
      };
      return { type: "auth_lock", reason: obj.reason, ttl: obj.ttl };
    }
    case OP_ERROR: {
      const obj = JSON.parse(textDecoder.decode(bytes.subarray(1))) as {
        code: string;
        message: string;
      };
      return { type: "error", code: obj.code, message: obj.message };
    }
    default:
      return null;
  }
}
