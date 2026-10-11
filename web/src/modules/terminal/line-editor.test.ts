import { describe, expect, it } from "vitest";
import { LineEditor } from "./line-editor";

describe("LineEditor", () => {
  it("output arriving mid-line is written around the half-typed text, not into it", () => {
    const ed = new LineEditor();
    ed.feed("SYST:E");
    // The answer to the previous command, ending with the server's prompt.
    expect(ed.wrapOutput('-113,"FOO"\r\nSCPI> ')).toBe('\r\x1b[2K-113,"FOO"\r\nSCPI> SYST:E');
    // The cursor goes back to where it was inside the line.
    ed.feed("\x1b[D\x1b[D");
    expect(ed.wrapOutput("x\r\nSCPI> ")).toBe("\r\x1b[2Kx\r\nSCPI> SYST:E\x1b[2D");
  });

  it("output with nothing typed passes through untouched", () => {
    expect(new LineEditor().wrapOutput("AWIC\r\nSCPI> ")).toBe("AWIC\r\nSCPI> ");
  });

  it("echoes typed characters and builds up the line", () => {
    // Every edit redraws the whole line from its pre-edit cursor column
    // (module doc) rather than emitting a minimal per-character diff, so
    // even a plain append re-sends the buffer — this only checks the
    // resulting state; the exact redraw bytes are pinned by the more
    // targeted tests below.
    const editor = new LineEditor();
    editor.feed("*");
    editor.feed("I");
    expect(editor.line).toBe("*I");
    expect(editor.cursorPosition).toBe(2);
  });

  it("Enter completes the line, echoes CRLF, and resets the buffer", () => {
    const editor = new LineEditor();
    editor.feed("*IDN?");
    const result = editor.feed("\r");
    expect(result).toEqual({ echo: "\r\n", lines: ["*IDN?"] });
    expect(editor.line).toBe("");
    expect(editor.cursorPosition).toBe(0);
  });

  it("a single feed() call can contain the whole line plus Enter", () => {
    const editor = new LineEditor();
    const result = editor.feed("*IDN?\r");
    expect(result.lines).toEqual(["*IDN?"]);
    expect(result.echo.endsWith("\r\n")).toBe(true);
    expect(editor.line).toBe("");
  });

  it("an empty line on Enter is not pushed to history but is still reported", () => {
    const editor = new LineEditor();
    const result = editor.feed("\r");
    expect(result.lines).toEqual([""]);
    expect(editor.historyEntries).toEqual([]);
  });

  it("backspace at the end of the line erases the last character", () => {
    const editor = new LineEditor();
    editor.feed("AB");
    const result = editor.feed("\x7f");
    expect(editor.line).toBe("A");
    expect(editor.cursorPosition).toBe(1);
    // Move left from the pre-edit cursor column (2), clear to end of line,
    // redraw "A"; cursor is already at the end (back = 0).
    expect(result.echo).toBe("\x1b[2D\x1b[KA");
  });

  it("backspace on an empty line is a no-op", () => {
    const editor = new LineEditor();
    expect(editor.feed("\x7f").echo).toBe("");
  });

  it("backspace in the middle of the line removes the character before the cursor", () => {
    const editor = new LineEditor();
    editor.feed("ABC");
    editor.feed("\x1b[D"); // cursor now between B and C
    const result = editor.feed("\x7f"); // removes 'B'
    expect(editor.line).toBe("AC");
    expect(editor.cursorPosition).toBe(1);
    expect(result.echo).toBe("\x1b[2D\x1b[KAC\x1b[1D");
  });

  it("left/right arrows move the cursor within bounds and are silent at the edges", () => {
    const editor = new LineEditor();
    editor.feed("AB");
    expect(editor.cursorPosition).toBe(2);
    expect(editor.feed("\x1b[C").echo).toBe(""); // already at the end
    expect(editor.feed("\x1b[D").echo).toBe("\x1b[D");
    expect(editor.cursorPosition).toBe(1);
    editor.feed("\x1b[D");
    expect(editor.cursorPosition).toBe(0);
    expect(editor.feed("\x1b[D").echo).toBe(""); // already at the start
  });

  it("inserting in the middle of the line redraws the tail and repositions the cursor", () => {
    const editor = new LineEditor();
    editor.feed("AC");
    editor.feed("\x1b[D"); // cursor between A and C
    const result = editor.feed("B");
    expect(editor.line).toBe("ABC");
    expect(editor.cursorPosition).toBe(2);
    expect(result.echo).toBe("\x1b[1D\x1b[KABC\x1b[1D");
  });

  it("up recalls the most recent history entry, saving the in-progress line", () => {
    const editor = new LineEditor();
    editor.feed("*IDN?\r");
    editor.feed("SYST");
    const result = editor.feed("\x1b[A");
    expect(editor.line).toBe("*IDN?");
    expect(editor.cursorPosition).toBe(5);
    expect(result.echo).toBe("\x1b[4D\x1b[K*IDN?");
  });

  it("down after up restores the line that was being typed", () => {
    const editor = new LineEditor();
    editor.feed("*IDN?\r");
    editor.feed("SYST");
    editor.feed("\x1b[A");
    const result = editor.feed("\x1b[B");
    expect(editor.line).toBe("SYST");
    expect(result.echo).toBe("\x1b[5D\x1b[KSYST");
  });

  it("up repeatedly walks further back through history, stopping at the oldest entry", () => {
    const editor = new LineEditor();
    editor.feed("A\r");
    editor.feed("B\r");
    editor.feed("C\r");
    editor.feed("\x1b[A"); // C
    editor.feed("\x1b[A"); // B
    editor.feed("\x1b[A"); // A
    expect(editor.line).toBe("A");
    const result = editor.feed("\x1b[A"); // already oldest
    expect(editor.line).toBe("A");
    expect(result.echo).toBe("");
  });

  it("down with no history browsing in progress is a no-op", () => {
    const editor = new LineEditor();
    editor.feed("A\r");
    expect(editor.feed("\x1b[B").echo).toBe("");
  });

  it("up with an empty history is a no-op", () => {
    const editor = new LineEditor();
    expect(editor.feed("\x1b[A").echo).toBe("");
  });

  it("an unrecognized escape sequence is swallowed, not inserted", () => {
    const editor = new LineEditor();
    const result = editor.feed("A\x1b[HB"); // Home key, unrecognized
    expect(editor.line).toBe("AB");
    expect(result.lines).toEqual([]);
  });

  it("a pasted multi-line chunk yields one completed line per embedded newline", () => {
    const editor = new LineEditor();
    const result = editor.feed("ACQ:RATE 1E6\rACQ:STAR\r");
    expect(result.lines).toEqual(["ACQ:RATE 1E6", "ACQ:STAR"]);
    expect(editor.historyEntries).toEqual(["ACQ:RATE 1E6", "ACQ:STAR"]);
  });
});
