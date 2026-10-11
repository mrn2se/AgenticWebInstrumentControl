"""VISA conformance and control check against a running AWIC server.

    make visa                  # in a container, against 127.0.0.1:5025
    ADDR=host:5025 make visa

AWIC's SCPI port (SPECIFICATION.md §9) is meant to be usable from any VISA
client, not only from AWIC's own tools — that is the whole point of speaking
SCPI rather than a private line protocol. This drives it through **pyvisa**
over `TCPIP::<host>::<port>::SOCKET`, checking two different things:

1.  **Conformance.** The IEEE 488.2 common commands a generic VISA driver
    issues before it knows anything about the instrument, and the SCPI-99
    required subsystems. A driver's startup sequence is typically
    `*CLS; *ESE 0; *SRE 0; *OPC?` followed by `SYST:ERR?` polling, and every
    part of it has to answer.

2.  **Control.** That an instrument can then actually be selected, configured,
    captured from and measured — over VISA alone, with no AWIC-specific
    client code.

Deliberately left at pyvisa's own socket defaults where it matters: the
default `write_termination` is `\\r\\n`, and a server that only accepted `\\n`
would fail every command from an out-of-the-box VISA client, so that is part
of what is being tested rather than configured away.

Exits non-zero on the first failed check, like `web/e2e/ui-smoke.js`.
"""

import os
import sys

import pyvisa

ADDR = os.environ.get("ADDR", "127.0.0.1:5025")
HOST, _, PORT = ADDR.partition(":")
RESOURCE = f"TCPIP::{HOST}::{PORT or '5025'}::SOCKET"

failures = 0
skipped = 0


def check(ok, what, detail=""):
    global failures
    if not ok:
        failures += 1
    print(f"{'PASS' if ok else 'FAIL'}  {what}" + (f"  [{detail}]" if detail else ""))


def skip(what, why):
    global skipped
    skipped += 1
    print(f"SKIP  {what}  [{why}]")


def section(name):
    print(f"\n--- {name} ---")


rm = pyvisa.ResourceManager("@py")
instr = rm.open_resource(RESOURCE)
# The server answers with '\n'; the write termination stays at pyvisa's
# socket default ('\r\n') on purpose — see the module docstring.
instr.read_termination = "\n"
instr.timeout = 10_000

section("connection")
check(instr.write_termination == "\r\n", "pyvisa's default write termination is CR LF",
      repr(instr.write_termination))

idn = instr.query("*IDN?")
fields = idn.split(",")
check(len(fields) == 4, "*IDN? has the four IEEE 488.2 fields", idn)
check(all(f.strip() for f in fields), "no *IDN? field is blank", idn)

section("IEEE 488.2 mandatory common commands")
# Every one of these is required by IEEE 488.2 §10. A *query* that the engine
# does not know answers nothing at all (which is what the standard says a
# query error does), so a missing one shows up here as a VISA timeout rather
# than as an error string — which is exactly how they were found.
for cmd, expected in [("*IDN?", None), ("*OPC?", "1"), ("*ESR?", None),
                      ("*STB?", None), ("*ESE?", None), ("*SRE?", None),
                      ("*TST?", "0")]:
    try:
        got = instr.query(cmd)
        ok = got == expected if expected is not None else got != ""
        check(ok, f"{cmd} answers", got)
    except Exception as exc:                                  # noqa: BLE001
        check(False, f"{cmd} answers", f"{type(exc).__name__}: {exc}")

# The setters answer nothing, so they are checked by the error queue staying
# empty afterwards — a command the engine rejects would queue -113.
instr.write("*CLS")
for setter in ["*RST", "*WAI", "*OPC", "*ESE 0", "*SRE 0"]:
    instr.write(setter)
check(instr.query("SYST:ERR?").startswith('0,'),
      "a full *CLS/*RST/*WAI/*OPC/*ESE/*SRE sequence queues no error")

section("SCPI-99 required subsystems")
check(instr.query("SYST:VERS?") == "1999.0", "SYST:VERS? names the SCPI version",
      instr.query("SYST:VERS?"))
for q in ["STAT:OPER?", "STAT:OPER:EVEN?", "STAT:OPER:COND?",
          "STAT:QUES?", "STAT:QUES:EVEN?", "STAT:QUES:COND?"]:
    try:
        check(instr.query(q) == "0", f"{q} reads 0")
    except Exception as exc:                                  # noqa: BLE001
        check(False, f"{q} reads 0", f"{type(exc).__name__}: {exc}")
instr.write("STAT:OPER:ENAB 512")
check(instr.query("STAT:OPER:ENAB?") == "512", "STAT:OPER:ENAB round-trips")
instr.write("STAT:QUES:ENAB 256")
check(instr.query("STAT:QUES:ENAB?") == "256", "STAT:QUES:ENAB round-trips")
instr.write("*ESE 48")
instr.write("STAT:PRES")
check(instr.query("STAT:OPER:ENAB?") == "0" and instr.query("STAT:QUES:ENAB?") == "0",
      "STAT:PRES clears the SCPI enables")
check(instr.query("*ESE?") == "48", "STAT:PRES leaves *ESE alone, as SCPI-99 says")

section("status reporting")
instr.write("*CLS")
check(instr.query("*ESR?") == "0" and instr.query("*STB?") == "0",
      "*CLS clears both the ESR and the status byte")
instr.write("BOGUS:HEADER")
check(instr.query("*ESR?") == "32", "a command error sets ESR bit 5 (CME)")
check(instr.query("*ESR?") == "0", "reading the ESR clears it (destructive read)")
check(instr.query("*STB?") == "4", "the queued error shows as status-byte bit 2 (EAV)")
err = instr.query("SYST:ERR?")
check(err.startswith("-113,") and '"' in err,
      'SYST:ERR? uses the standard <code>,"<message>" form', err)
check(instr.query("SYST:ERR?").startswith('0,'), "the error queue drains")

instr.write("*CLS")
instr.write("*ESE 32")
instr.write("*SRE 4")
instr.write("BOGUS:HEADER")
check(instr.query("*STB?") == "100",
      "*ESE/*SRE summarise into ESB and MSS (4 + 32 + 64)")
instr.write("*CLS")
instr.write("*SRE 0")
instr.write("*ESE 0")

section("message syntax")
instr.write("ACQ:RATE 2000000;:ACQ:SAMP 4096")
check(instr.query("ACQ:RATE?;:ACQ:SAMP?") == "2.0E+6;4.096E+3",
      "a compound query answers ;-joined, in order")
check(instr.query_ascii_values("ACQ:RATE?") == [2_000_000.0],
      "query_ascii_values parses the NR3 numbers pyvisa expects")
check(instr.query("acq:rate?") == "2.0E+6", "headers are case-insensitive")
check(instr.query("ACQuire:RATe?") == "2.0E+6", "long form resolves to the same command")
check(instr.query("SYST:ERR?").startswith('0,'), "none of that queued an error")

section("a failed query is silent unless inline errors are on")
# IEEE 488.2: a query error produces no response, so a VISA client waits for
# its own timeout. That is standard behaviour and not a bug — but it is a trap
# worth pinning down, and AWIC's own cure for it is SYST:COMM:ERR:INL.
instr.timeout = 1500
try:
    instr.query("MEAS:VPP? CH1")
    check(False, "a failed query is silent by default", "it answered")
except pyvisa.VisaIOError as exc:
    check(exc.error_code == pyvisa.constants.StatusCode.error_timeout,
          "a failed query is silent by default (VISA timeout)", "VI_ERROR_TMO")
instr.timeout = 10_000
instr.write("*CLS")
instr.write("SYST:COMM:ERR:INL ON")
try:
    reply = instr.query("MEAS:VPP? CH1")
    check(reply.startswith("ERR "), "SYST:COMM:ERR:INL ON answers a failed query", reply)
except Exception as exc:                                      # noqa: BLE001
    check(False, "SYST:COMM:ERR:INL ON answers a failed query",
          f"{type(exc).__name__}: {exc}")
instr.write("SYST:COMM:ERR:INL OFF")
instr.write("*CLS")

section("instrument control over VISA alone")
catalog = instr.query("INST:CAT?")
names = [n.strip('"') for n in catalog.split(",") if n.strip('"')]
check(bool(catalog), "INST:CAT? lists the bench", catalog)
if not names:
    skip("select, configure, capture, measure", "no instrument in INST:CAT?")
for target in names:
    print(f"  ({target})")
    instr.write("*RST")
    instr.write(f'INST:SEL "{target}"')
    check(instr.query("INST:SEL?").strip('"') == target, f"INST:SEL selects {target}")
    instr.write("ACQ:RATE 1000000")
    instr.write("ACQ:CHAN (@1)")
    instr.write("ACQ:SAMP 100000")
    # Analog full-scale range. Digital instruments ignore it, so it is set
    # unconditionally: what is being checked is that a VISA client *can* set
    # it, which before B-30 it could not.
    instr.write("ACQ:RANG 5.0")
    check(instr.query("ACQ:CHAN?") == "(@1)",
          "ACQ:CHAN round-trips as a SCPI channel list", instr.query("ACQ:CHAN?"))
    check(instr.query("ACQ:RANG?") == "5.0E+0",
          "ACQ:RANG round-trips", instr.query("ACQ:RANG?"))
    check(instr.query("SYST:ERR?").startswith('0,'), "configuring queued no error")
    handle = instr.query("ACQ:STAR")
    check(handle.startswith("capture://"), "ACQ:STAR returns a capture handle", handle)
    # Whether a measurement has a *value* depends on what is wired to the
    # probe, so the check is that it answers — a number or a stated reason,
    # never silence. Inline mode is what makes the reason readable here.
    instr.write("SYST:COMM:ERR:INL ON")
    for meas in ["MEAS:FREQ? CH1", "MEAS:PER? CH1", "MEAS:VPP? CH1"]:
        try:
            got = instr.query(meas)
            check(bool(got), f"{meas} answers with a value or a reason", got)
        except Exception as exc:                              # noqa: BLE001
            check(False, f"{meas} answers", f"{type(exc).__name__}: {exc}")
    instr.write("SYST:COMM:ERR:INL OFF")
    instr.write("*CLS")

section("what VISA cannot do over a raw socket")
# Not AWIC limitations: a TCPIP SOCKET resource has no SRQ line and no VISA
# device-clear, so pyvisa-py refuses these. A client that wants the status
# byte asks for it with *STB? instead, which works above.
for name, call in [("read_stb()", instr.read_stb), ("clear()", instr.clear)]:
    try:
        call()
        print(f"NOTE  {name} is supported by this backend after all")
    except Exception as exc:                                  # noqa: BLE001
        print(f"NOTE  {name} unsupported over TCPIP SOCKET ({type(exc).__name__})"
              " — use *STB? / *CLS")

instr.close()
print(f"\n{'FAILED' if failures else 'OK'}: {failures} failed, {skipped} skipped")
sys.exit(1 if failures else 0)
