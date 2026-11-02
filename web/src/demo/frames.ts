import { FRAME_HEADER_LEN } from "../ws/frame";
import { TERM_FRAME_HEADER_LEN, TERM_FRAME_MAGIC } from "../ws/term-frame";
import { VIEW_FRAME_HEADER_LEN, VIEW_FRAME_MAGIC } from "../ws/view-frame";

/** The server's side of the binary frames in SPECIFICATION.md §3.2, §3.4 and §3.5. */

export function sampleFrame(opts: {
  stream: number;
  sequence: number;
  encoding: 0 | 2;
  channelCount: number;
  sampleCount: number;
  t0Ns: bigint;
  payload: Uint8Array;
}): ArrayBuffer {
  const buf = new ArrayBuffer(FRAME_HEADER_LEN + opts.payload.length);
  const view = new DataView(buf);
  view.setUint8(0, 0xa5);
  view.setUint8(1, 1);
  view.setUint16(2, opts.stream, true);
  view.setUint32(4, opts.sequence, true);
  view.setUint8(8, opts.encoding);
  view.setUint8(9, opts.channelCount);
  view.setUint32(12, opts.sampleCount, true);
  view.setBigUint64(16, opts.t0Ns, true);
  new Uint8Array(buf, FRAME_HEADER_LEN).set(opts.payload);
  return buf;
}

export function viewFrame(opts: {
  view: number;
  sequence: number;
  kind: 1 | 2 | 3 | 4;
  channelCount: number;
  follow: boolean;
  itemCount: number;
  firstSample: number;
  samplesPerItem: number;
  payload: Uint8Array;
}): ArrayBuffer {
  const buf = new ArrayBuffer(VIEW_FRAME_HEADER_LEN + opts.payload.length);
  const view = new DataView(buf);
  view.setUint8(0, VIEW_FRAME_MAGIC);
  view.setUint8(1, 1);
  view.setUint16(2, opts.view, true);
  view.setUint32(4, opts.sequence, true);
  view.setUint8(8, opts.kind);
  view.setUint8(9, opts.channelCount);
  view.setUint8(10, 8);
  view.setUint8(11, opts.follow ? 1 : 0);
  view.setUint32(12, opts.itemCount, true);
  view.setBigUint64(16, BigInt(opts.firstSample), true);
  view.setBigUint64(24, BigInt(opts.samplesPerItem), true);
  new Uint8Array(buf, VIEW_FRAME_HEADER_LEN).set(opts.payload);
  return buf;
}

export function termFrame(session: number, sequence: number, text: string): ArrayBuffer {
  const payload = new TextEncoder().encode(text);
  const buf = new ArrayBuffer(TERM_FRAME_HEADER_LEN + payload.length);
  const view = new DataView(buf);
  view.setUint8(0, TERM_FRAME_MAGIC);
  view.setUint8(1, 1);
  view.setUint16(2, session, true);
  view.setUint32(4, sequence, true);
  new Uint8Array(buf, TERM_FRAME_HEADER_LEN).set(payload);
  return buf;
}

/** Appends `value` as unsigned LEB128, the run-length form of a digital-runs view frame. */
export function pushLeb128(out: number[], value: number): void {
  let rest = value;
  while (rest >= 0x80) {
    out.push((rest % 0x80) | 0x80);
    rest = Math.floor(rest / 0x80);
  }
  out.push(rest);
}
