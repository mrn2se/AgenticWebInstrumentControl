# VISA check

`make visa` drives the running server's SCPI port (TCP 5025, SPECIFICATION.md
§9) through **pyvisa** — the standard VISA client stack — over
`TCPIP::127.0.0.1::5025::SOCKET`, and prints one `PASS`/`FAIL` line per check.

```
make run          # in one terminal
make visa         # in another
make visa VISA_ADDR=other-host:5025
```

It exists because "AWIC speaks SCPI" is only worth something if a client that
knows nothing about AWIC can drive it. Two things are checked:

- **Conformance** — the IEEE 488.2 mandatory common commands (`*CLS`, `*ESE`,
  `*ESE?`, `*ESR?`, `*IDN?`, `*OPC`, `*OPC?`, `*RST`, `*SRE`, `*SRE?`, `*STB?`,
  `*TST?`, `*WAI`), the SCPI-99 required subsystems (`SYST:ERR?`,
  `SYST:VERS?`, `STATus`), the status-reporting model (ESR bits by error
  class, destructive `*ESR?` read, ESB/MSS summaries through `*ESE`/`*SRE`),
  and compound `;` messages.
- **Control** — selecting an instrument, configuring an acquisition, starting
  it and taking measurements, using nothing but VISA calls.

Two things it pins down on purpose rather than papering over:

- pyvisa's default `write_termination` for a socket resource is `\r\n`, and
  the test leaves it there. A server that accepted only `\n` would fail every
  command from an out-of-the-box VISA client.
- A failed *query* answers nothing at all, because that is what IEEE 488.2
  says a query error does — so a VISA client sees its own timeout. The test
  asserts both that behaviour and that `SYST:COMM:ERR:INL ON` (AWIC's own
  cure, live test B-10) turns it into a readable `ERR <code>,"<msg>"`.

`read_stb()` and `clear()` are reported as `NOTE`, not `FAIL`: a raw TCP
socket has no SRQ line and no VISA device-clear, so pyvisa-py cannot offer
them for any instrument. `*STB?` and `*CLS` do the same jobs in-band.
