/**
 * Client-side line discipline for the `scpi` terminal source (SPECIFICATION.md
 * §11.2, §9): local echo, backspace, left/right cursor movement, and
 * up/down history — everything a serial port's own device would normally
 * echo back, but which AWIC's SCPI engine never does (it only ever answers
 * a complete line, SPEC §9). Pure and DOM-free so it can be unit tested
 * without xterm.js; `index.ts` feeds it xterm's raw `onData` chunks and
 * writes the returned `echo` straight back to the terminal.
 *
 * Every mutating key redraws the *whole* user-typed line from the cursor's
 * pre-edit column onward (`redrawFrom`) rather than computing a minimal
 * diff: `\x1b[<n>D` back to the start of the user's text, `\x1b[K` to erase
 * whatever was there, then the new buffer, then `\x1b[<n>D` back to the
 * intended cursor column. Simple to get right and to test byte-for-byte;
 * the lines this editor ever redraws are a handful of SCPI keywords long,
 * so the extra bytes are irrelevant.
 */

/** What one `feed()` call produces: raw bytes to write to the terminal now
 * (already includes any cursor-movement escapes needed), and zero or more
 * completed lines — normally one (Enter), more only if `data` contained a
 * pasted multi-line chunk. Lines never include their `\r`/`\n`. */
export interface LineEditorResult {
  readonly echo: string;
  readonly lines: readonly string[];
}

/** Recognized escape sequences this editor acts on; anything else starting
 * with ESC is consumed and dropped rather than inserted as literal bytes
 * (SPEC's console is a single-line editor — Home/End/PageUp/... have
 * nothing sensible to do here, and a bare, dangling ESC would otherwise
 * pollute the visible buffer). */
const UP = "\x1b[A";
const DOWN = "\x1b[B";
const RIGHT = "\x1b[C";
const LEFT = "\x1b[D";

export class LineEditor {
  private buffer = "";
  private cursor = 0;
  private history: string[] = [];
  /** `null` when not currently browsing history; otherwise an index into
   * `history`, and `pendingLine` holds what the user had typed before the
   * first press of Up, restored when Down navigates past the newest entry. */
  private historyIndex: number | null = null;
  private pendingLine = "";

  /** Current buffer contents — for tests and for a caller that wants to
   * inspect state without parsing escape sequences itself. */
  get line(): string {
    return this.buffer;
  }

  get cursorPosition(): number {
    return this.cursor;
  }

  get historyEntries(): readonly string[] {
    return this.history;
  }

  /** The bytes that put `output` on screen around a half-typed line. An
   * answer used to land wherever the cursor was, so typing the next command
   * before the last one was answered gave `SYST:EERROR -113,"FOO"` — the
   * answer spliced into the line being edited. With text in the buffer this
   * clears that screen line, writes `output` (which ends with the server's
   * next prompt), then redraws the buffer with the cursor where it was. */
  wrapOutput(output: string): string {
    if (this.buffer.length === 0) return output;
    const back = this.buffer.length - this.cursor;
    return `\r\x1b[2K${output}${this.buffer}${back > 0 ? `\x1b[${back}D` : ""}`;
  }

  /** Feeds one chunk of raw input as xterm.js's `onData` delivers it —
   * one keystroke in the common case, but possibly several (a paste, or a
   * multi-byte escape sequence arriving as one chunk). */
  feed(data: string): LineEditorResult {
    let echo = "";
    const lines: string[] = [];
    let i = 0;
    while (i < data.length) {
      if (data[i] === "\r" || data[i] === "\n") {
        lines.push(this.buffer);
        if (this.buffer.length > 0) this.history.push(this.buffer);
        this.buffer = "";
        this.cursor = 0;
        this.historyIndex = null;
        echo += "\r\n";
        i += 1;
        continue;
      }
      if (data[i] === "\x7f" || data[i] === "\b") {
        echo += this.backspace();
        i += 1;
        continue;
      }
      if (data.startsWith(UP, i)) {
        echo += this.recallHistory(-1);
        i += UP.length;
        continue;
      }
      if (data.startsWith(DOWN, i)) {
        echo += this.recallHistory(1);
        i += DOWN.length;
        continue;
      }
      if (data.startsWith(RIGHT, i)) {
        if (this.cursor < this.buffer.length) {
          this.cursor += 1;
          echo += RIGHT;
        }
        i += RIGHT.length;
        continue;
      }
      if (data.startsWith(LEFT, i)) {
        if (this.cursor > 0) {
          this.cursor -= 1;
          echo += LEFT;
        }
        i += LEFT.length;
        continue;
      }
      if (data[i] === "\x1b") {
        i = this.skipUnrecognizedEscape(data, i);
        continue;
      }
      echo += this.insert(data[i]!);
      i += 1;
    }
    return { echo, lines };
  }

  private insert(ch: string): string {
    const oldCursor = this.cursor;
    this.buffer = this.buffer.slice(0, this.cursor) + ch + this.buffer.slice(this.cursor);
    this.cursor += 1;
    return this.redrawFrom(oldCursor);
  }

  private backspace(): string {
    if (this.cursor === 0) return "";
    const oldCursor = this.cursor;
    this.buffer = this.buffer.slice(0, this.cursor - 1) + this.buffer.slice(this.cursor);
    this.cursor -= 1;
    return this.redrawFrom(oldCursor);
  }

  private recallHistory(direction: -1 | 1): string {
    const oldCursor = this.cursor;
    if (direction === -1) {
      if (this.history.length === 0 || this.historyIndex === 0) return "";
      if (this.historyIndex === null) {
        // First Up press: remember what was being typed so Down can
        // restore it once the user navigates past the newest entry.
        this.pendingLine = this.buffer;
        this.historyIndex = this.history.length - 1;
      } else {
        this.historyIndex -= 1;
      }
      this.buffer = this.history[this.historyIndex]!;
    } else {
      if (this.historyIndex === null) return "";
      if (this.historyIndex < this.history.length - 1) {
        this.historyIndex += 1;
        this.buffer = this.history[this.historyIndex]!;
      } else {
        this.historyIndex = null;
        this.buffer = this.pendingLine;
      }
    }
    this.cursor = this.buffer.length;
    return this.redrawFrom(oldCursor);
  }

  /** Consumes a `\x1b`-led sequence this editor doesn't act on: a bracketed
   * CSI sequence (`ESC [ params final-byte`) is skipped whole; a bare ESC
   * (or a CSI with no final byte in this chunk) is skipped alone. */
  private skipUnrecognizedEscape(data: string, start: number): number {
    if (data[start + 1] !== "[") return start + 1;
    let j = start + 2;
    while (j < data.length && /[0-9;]/.test(data[j]!)) j += 1;
    return Math.min(j + 1, data.length);
  }

  /** Redraws the buffer from `fromCursor` (the on-screen column, relative to
   * the start of the user's text, the cursor was at *before* this edit) —
   * see the module doc for why a full redraw rather than a minimal diff. */
  private redrawFrom(fromCursor: number): string {
    let out = "";
    if (fromCursor > 0) out += `\x1b[${fromCursor}D`;
    out += "\x1b[K";
    out += this.buffer;
    const back = this.buffer.length - this.cursor;
    if (back > 0) out += `\x1b[${back}D`;
    return out;
  }
}
