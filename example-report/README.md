# Example reports

What an agent and a person learn from AWIC on a real bench. Every picture is a link to the report
it comes from. Waveform pictures are drawn by AWIC's own renderer (`render_view` over MCP,
`GET /api/render` in a browser); the screenshots show the web UI.

## CAN with two nodes — [can-two-nodes.md](can-two-nodes.md)

An RP2350 sends CAN frames through an MCP2515 controller; the scope watches the bus, the logic
analyzer the SPI lines to the controller. One frame, seen from both sides.

[![One CAN frame, acknowledged](img/can2-frame.svg)](can-two-nodes.md)

[![The SPI burst for one frame](img/can2-spi-burst.svg)](can-two-nodes.md)

[![Graph tab, CAN decoded on the scope](img/web-graph-scope-can.png)](can-two-nodes.md)

[![Graph tab, SPI decoded on the logic analyzer](img/web-graph-logic-spi.png)](can-two-nodes.md)

## A bit-banged UART next to a hardware one — [uart-bit-banged.md](uart-bit-banged.md)

Why one serial line decodes cleanly and the other drops a character now and then, told by the
histogram of bit lengths.

[![Low pulse widths of the bit-banged line](img/uart-d0-hist.svg)](uart-bit-banged.md)

[![Graph tab, UART decoded](img/web-graph-logic-uart.png)](uart-bit-banged.md)

[![Terminal tab on the debug probe's serial port](img/web-terminal-serial.png)](uart-bit-banged.md)

## An OLED display, read back from its I²C wires — [esp32-display.md](esp32-display.md)

An ESP32-C6 draws a screen; AWIC records the bus and rebuilds the picture from the bytes on the wire.

[![The board and its display](img/esp32-oled-photo.jpg)](esp32-display.md)

[![The screen rebuilt from the captured I²C traffic](img/esp32-oled-rebuilt.svg)](esp32-display.md)

[![One refresh, then the UART line](img/esp32-i2c-refresh.svg)](esp32-display.md)

## CAN bus on the oscilloscope alone — [can-on-the-scope.md](can-on-the-scope.md)

A 500 kbit/s bus found and decoded from the scope's two probes, differential, with no transceiver.

[![Seven CAN frames on CANL and CANH](img/can-full.svg)](can-on-the-scope.md)

## I²C display on the logic analyzer — [i2c-display.md](i2c-display.md)

One survey call names the I²C pair, one decode call reads the framebuffer an OLED is sent.

[![One second of I²C: two display refreshes](img/i2c-full.svg)](i2c-display.md)

[![The SCL period histogram](img/i2c-hist.svg)](i2c-display.md)

## A sine generator and the calibration output — [sine-and-calibration.md](sine-and-calibration.md)

An uncompensated 1:10 probe, and the oscilloscope dropping samples at 16 MS/s, shown with a sine.

[![Sine folded at 8 MS/s](img/sine-eye-8msps.svg)](sine-and-calibration.md)

[![Calibration square with probe overshoot](img/cal-square.svg)](sine-and-calibration.md)

## The USB tab — [usb-tab.md](usb-tab.md)

The bench as the server sees it, and which ports AWIC may switch.

[![USB tab](img/web-usb.png)](usb-tab.md)

## The MCP tab — [mcp-tab.md](mcp-tab.md)

Who is connected, and every request an agent made, with the pictures it was sent.

[![MCP tab](img/web-mcp.png)](mcp-tab.md)
