// Terminal tab against real hardware: a USB-serial adapter
// whose TX is on an analyzer channel (what was typed is checked on the wire
// over MCP) and whose RX something drives when this prints READY_RX — it then
// waits for GO_FILE to appear. Checks typing (keys, control characters,
// non-ASCII, paste) byte for byte, frame formats, receiving, a second browser
// tab joining the port, the SCPI console, the server log, live decode, and
// the serial_open join rules (B-93).
//
//   podman run --rm --net=host -e GO_FILE=/sig/go -v $PWD/web/e2e:/e2e:ro,Z \
//     -v /some/dir:/sig:Z localhost/awic-e2e:latest node terminal.js
// Env: SERIAL_LABEL (ttyUSB0), LA (analyzer id), TX_CH (analyzer channel on TX).
const { chromium } = require('playwright');
const fs = require('fs');

const env = process.env;
const URL = env.URL || 'http://127.0.0.1:8080/';
const SERIAL_LABEL = env.SERIAL_LABEL || 'ttyUSB0';
const LA = env.LA || '3-3.4.1.1';
const TX_CH = Number(env.TX_CH || 2);
const GO = env.GO_FILE || '/tmp/go_rx';
let failures = 0;
const check = (ok, what) => { console.log(`${ok ? 'PASS' : 'FAIL'}  ${what}`); if (!ok) failures++; };
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

let sid = null;
async function mcp(name, args) {
  const post = (body) => fetch(new globalThis.URL('/mcp', URL), {
    method: 'POST',
    headers: { 'Content-Type': 'application/json', Accept: 'application/json, text/event-stream', ...(sid ? { 'Mcp-Session-Id': sid } : {}) },
    body: JSON.stringify(body),
  });
  if (!sid) {
    const init = await post({ jsonrpc: '2.0', id: 1, method: 'initialize', params: { protocolVersion: '2025-06-18', capabilities: {}, clientInfo: { name: 'terminal-e2e', version: '0' } } });
    sid = init.headers.get('mcp-session-id');
  }
  const res = await (await post({ jsonrpc: '2.0', id: 2, method: 'tools/call', params: { name, arguments: args } })).json();
  return { error: !!res.result.isError, text: res.result.content[0].text };
}
const headS = async (rec) => {
  const r = JSON.parse((await mcp('list_recordings', {})).text).recordings.find((x) => x.rec === rec);
  return r.head_sample / r.rate_hz;
};
const wireBytes = async (rec, t0, opts) => {
  const r = await mcp('decode', { rec, decoder: 'uart', channels: [{ role: 'rx', channel: TX_CH }], options: opts, format: 'transactions', t0_s: t0 });
  if (r.error) return { error: r.text };
  const d = JSON.parse(r.text);
  return { bytes: Buffer.concat(d.transactions.map((t) => Buffer.from(t.data.replace(/ /g, ''), 'hex'))), errors: d.summary.errors };
};

async function openTab(browser) {
  const page = await browser.newPage({ viewport: { width: 1400, height: 950 } });
  page.errors = [];
  page.on('pageerror', (e) => page.errors.push(e.message));
  page.on('console', (m) => { if (m.type() === 'error') page.errors.push(m.text()); });
  await page.goto(URL, { waitUntil: 'networkidle' });
  await page.locator('nav button', { hasText: 'Terminal' }).click();
  await page.waitForFunction(() => document.querySelector('awic-module-terminal #source').options.length > 0);
  return page;
}
const t = (page) => page.locator('awic-module-terminal');
async function selectSource(page, text) {
  const value = await t(page).locator('#source option').evaluateAll((os, x) => (os.find((o) => o.textContent.includes(x)) || {}).value, text);
  await t(page).locator('#source').selectOption(value);
  await sleep(200);
  return value;
}
async function setSerial(page, s) {
  await page.evaluate((s) => {
    const m = document.querySelector('awic-module-terminal');
    for (const [id, v] of Object.entries({ '#baud': String(s.baud), '#data-bits': String(s.bits), '#parity': s.parity, '#stop-bits': s.stop })) {
      const el = m.querySelector(id); el.value = v; el.dispatchEvent(new Event('change', { bubbles: true }));
    }
  }, s);
}
const rows = (page) => page.evaluate(() => {
  const m = document.querySelector('awic-module-terminal');
  const pane = [...m.querySelectorAll('#panes > div')].find((p) => !p.hidden);
  return pane ? [...pane.querySelectorAll('.xterm-rows > div')].map((r) => r.textContent.replace(/ /g, ' ').trimEnd()).filter((x) => x) : [];
});
const status = (page) => t(page).locator('#status').textContent();
const focusPane = (page) => page.evaluate(() => {
  const m = document.querySelector('awic-module-terminal');
  const pane = [...m.querySelectorAll('#panes > div')].find((p) => !p.hidden);
  pane.querySelector('.xterm-helper-textarea').focus();
});
async function closeActiveTab(page) {
  await page.evaluate(() => {
    const m = document.querySelector('awic-module-terminal');
    const btns = [...m.querySelectorAll('#tabs button')].filter((b) => b.textContent.trim() === '×');
    const active = btns.find((b) => b.closest('[aria-selected="true"], .bg-awic-accent, [data-active="true"]')) || btns[btns.length - 1];
    if (active) active.click();
  });
  await sleep(500);
}

(async () => {
  const browser = await chromium.launch();
  const page = await openTab(browser);
  const recs = JSON.parse((await mcp('list_recordings', {})).text).recordings;
  for (const r of recs.filter((x) => x.device === LA)) await mcp('delete_recording', { rec: r.rec });
  const startRec = async () => JSON.parse((await mcp('start_recording', { device: LA, rate_hz: 4000000, channels: [0, 1, 2, 3, 4, 5, 6, 7] })).text).rec;
  let rec = await startRec();

  // --- 1. Typing goes out on the wire, byte for byte --------------------------
  const serialValue = await selectSource(page, SERIAL_LABEL);
  check(!!serialValue, `1 ${SERIAL_LABEL} is offered (${serialValue})`);
  await setSerial(page, { baud: 115200, bits: 8, parity: 'none', stop: 'one' });
  await t(page).locator('#open').click();
  await sleep(1200);
  check((await status(page)).trim() === '', `1 opens without an error ("${(await status(page)).trim()}")`);
  const t0 = await headS(rec);
  await focusPane(page);
  await page.keyboard.type('hello web terminal');
  await page.keyboard.press('Enter');
  await page.keyboard.press('Control+C');
  await page.keyboard.press('Backspace');
  await page.keyboard.press('ArrowUp');
  await page.keyboard.press('Tab');
  // Non-ASCII as a real keyboard produces it (a key event carrying text);
  // Playwright's own type() injects it as insertText, which xterm ignores.
  const cdp = await page.context().newCDPSession(page);
  for (const ch of ['ü', 'é', '€']) {
    await cdp.send('Input.dispatchKeyEvent', { type: 'keyDown', key: ch, text: ch, unmodifiedText: ch });
    await cdp.send('Input.dispatchKeyEvent', { type: 'keyUp', key: ch });
  }
  const paste = 'P'.repeat(40) + '0123456789'.repeat(26);
  await page.evaluate((text) => {
    const m = document.querySelector('awic-module-terminal');
    const ta = [...m.querySelectorAll('#panes > div')].find((p) => !p.hidden).querySelector('.xterm-helper-textarea');
    const dt = new DataTransfer(); dt.setData('text/plain', text);
    ta.dispatchEvent(new ClipboardEvent('paste', { clipboardData: dt, bubbles: true, cancelable: true }));
  }, paste);
  await sleep(1500);
  const sent = await wireBytes(rec, t0, [{ id: 'baud', value: 115200 }]);
  const want = Buffer.concat([Buffer.from('hello web terminal\r\x03\x7f\x1b[A\t', 'latin1'), Buffer.from('üé€', 'utf8'), Buffer.from(paste)]);
  check(!sent.error && sent.bytes.equals(want), `1 the wire carries exactly what was typed (${sent.error || `${sent.bytes.length} of ${want.length} bytes, ${sent.errors} errors`})`);
  if (!sent.error && !sent.bytes.equals(want)) {
    console.log('   got ', JSON.stringify(sent.bytes.toString('latin1').slice(0, 80)));
    console.log('   want', JSON.stringify(want.toString('latin1').slice(0, 80)));
  }
  const echoed = (await rows(page)).join('\n');
  check(!/hello web terminal/.test(echoed), '1 a serial pane does not echo locally (the device would)');

  // --- 2. Reopen at 9600 7E1: the settings reach the port ---------------------
  await closeActiveTab(page);
  await selectSource(page, SERIAL_LABEL);
  await setSerial(page, { baud: 9600, bits: 7, parity: 'even', stop: 'one' });
  await t(page).locator('#open').click();
  await sleep(1200);
  const t2 = await headS(rec);
  await focusPane(page);
  await page.keyboard.type('seven E one');
  await sleep(1500);
  const s7 = await wireBytes(rec, t2, [{ id: 'baud', value: 9600 }, { id: 'data_bits', value: 7 }, { id: 'parity', value: 'even' }]);
  check(!s7.error && s7.bytes.toString() === 'seven E one' && s7.errors === 0, `2 9600 7E1 on the wire (${s7.error || JSON.stringify(s7.bytes.toString()) + ', ' + s7.errors + ' errors'})`);

  // --- 3. Receiving, and a second browser tab joining the same port -----------
  await closeActiveTab(page);
  await selectSource(page, SERIAL_LABEL);
  await setSerial(page, { baud: 9600, bits: 8, parity: 'none', stop: 'one' });
  await t(page).locator('#open').click();
  await sleep(1000);
  const page2 = await openTab(browser);
  await selectSource(page2, SERIAL_LABEL);
  await setSerial(page2, { baud: 9600, bits: 8, parity: 'none', stop: 'one' });
  await t(page2).locator('#open').click();
  await sleep(1000);
  check((await status(page2)).trim() === '', `3 a second tab joins the open port ("${(await status(page2)).trim()}")`);
  // On this bench the analyzer stalls (and the adapter reads garbage) when
  // the ESP32 drives its pin while the analyzer records: record around it.
  await mcp('stop_recording', { rec }); await mcp('delete_recording', { rec });
  try { fs.unlinkSync(GO); } catch {}
  console.log('READY_RX');
  for (let i = 0; i < 60 && !fs.existsSync(GO); i++) await sleep(500);
  await sleep(1500);
  const r1 = (await rows(page)).join('\n');
  const r2 = (await rows(page2)).join('\n');
  check(/->TERM 9600 ok/.test(r1), `3 the first tab shows what the device sent (${JSON.stringify(r1.slice(-120))})`);
  check(/->TERM 9600 ok/.test(r2), `3 the joined tab shows it too (${JSON.stringify(r2.slice(-120))})`);
  rec = await startRec();
  await sleep(500);
  const t3 = await headS(rec);
  await focusPane(page2);
  await page2.keyboard.type('from tab two');
  await sleep(1500);
  const s2 = await wireBytes(rec, t3, [{ id: 'baud', value: 9600 }]);
  check(!s2.error && s2.bytes.toString() === 'from tab two', `3 typing in the joined tab goes out (${s2.error || JSON.stringify(s2.bytes.toString())})`);
  await closeActiveTab(page2);
  await sleep(500);
  const t4 = await headS(rec);
  await focusPane(page);
  await page.keyboard.type('still open');
  await sleep(1500);
  const s3 = await wireBytes(rec, t4, [{ id: 'baud', value: 9600 }]);
  check(!s3.error && s3.bytes.toString() === 'still open', `3 closing the joined tab leaves the first one working (${s3.error || JSON.stringify(s3.bytes.toString())})`);
  check(page2.errors.length === 0, `3 no page errors in the second tab (${JSON.stringify(page2.errors.slice(0, 3))})`);
  await page2.close();

  // --- 4. SCPI console: line editing, history, answers and errors -------------
  await selectSource(page, 'SCPI');
  await t(page).locator('#open').click();
  await sleep(800);
  await focusPane(page);
  await page.keyboard.type('*IDX');
  await page.keyboard.press('Backspace');
  await page.keyboard.type('N?');
  await page.keyboard.press('Enter');
  await sleep(800);
  await page.keyboard.press('ArrowUp');
  await page.keyboard.press('Enter');
  await sleep(800);
  await page.keyboard.type('FOO:BAR?');
  await page.keyboard.press('Enter');
  await page.keyboard.type('SYST:ERR?');
  await page.keyboard.press('Enter');
  await sleep(1000);
  const scpi = await rows(page);
  const idn = scpi.filter((l) => /AWIC/i.test(l) && !/\*ID/.test(l));
  check(idn.length >= 2, `4 *IDN? answered, and again from history (${idn.length}: ${JSON.stringify(scpi.slice(0, 8))})`);
  check(scpi.some((l) => /-1\d\d|undefined|header/i.test(l)), `4 an unknown command shows up in SYST:ERR? (${JSON.stringify(scpi.slice(-4))})`);

  // --- 5. Server log ---------------------------------------------------------
  await selectSource(page, 'Server log');
  await t(page).locator('#open').click();
  await sleep(1500);
  const log = (await rows(page)).join('\n');
  check(/term\.open|rec\.start|INFO/.test(log), `5 the server log shows recent lines (${log.length} chars)`);

  // --- 6. Decode a recording live --------------------------------------------
  await selectSource(page, 'Decode');
  await page.evaluate(async (rec) => {
    const m = document.querySelector('awic-module-terminal');
    m.querySelector('#settings').click();
    await new Promise((r) => setTimeout(r, 800));
    const recSel = m.querySelector('#decode-rec');
    recSel.value = String(rec); recSel.dispatchEvent(new Event('change', { bubbles: true }));
    await new Promise((r) => setTimeout(r, 300));
    const dec = m.querySelector('#decode-decoder');
    dec.value = 'uart'; dec.dispatchEvent(new Event('change', { bubbles: true }));
    await new Promise((r) => setTimeout(r, 300));
  }, rec);
  const roleOptions = await page.evaluate(() => [...document.querySelector('awic-module-terminal').querySelectorAll('#decode-roles select[data-role="rx"] option')].map((o) => `${o.value}:${o.textContent}`));
  await page.evaluate((ch) => {
    const sel = document.querySelector('awic-module-terminal').querySelector('#decode-roles select[data-role="rx"]');
    const opt = [...sel.options].find((o) => o.textContent.trim() === `D${ch}`) || [...sel.options].find((o) => o.value === String(ch));
    sel.value = opt ? opt.value : sel.value; sel.dispatchEvent(new Event('change', { bubbles: true }));
  }, TX_CH);
  await t(page).locator('#open').click();
  await sleep(1000);
  const decStatus = (await status(page)).trim() + (await t(page).locator('#decode-form-error').textContent()).trim();
  check(decStatus === '', `6 the decode session opens (roles ${JSON.stringify(roleOptions.slice(0, 4))}) ${decStatus}`);
  // The UI still holds the port at 9600: an agent asking for 115200 must be
  // told, and one asking for nothing joins at 9600.
  const serialSource = JSON.parse((await mcp('serial_list', {})).text).sources.find((s) => s.label.includes(SERIAL_LABEL)).source;
  const conflict = await mcp('serial_open', { source: serialSource, baud: 115200 });
  check(conflict.error && /serial_settings_conflict/.test(conflict.text) && /9600 8N1/.test(conflict.text), `6 joining the tab's 9600 port at 115200 is refused (${conflict.text.slice(0, 140)})`);
  const joined = await mcp('serial_open', { source: serialSource });
  const joinedRes = joined.error ? null : JSON.parse(joined.text);
  check(!!joinedRes && joinedRes.serial && joinedRes.serial.baud === 9600, `6 joining without settings gets the port's 9600 (${joined.text.slice(0, 160)})`);
  if (joinedRes) await mcp('serial_close', { session: joinedRes.session });
  // Close the tab's serial session, then send at 115200 while the decode pane watches.
  await page.evaluate((label) => {
    const m = document.querySelector('awic-module-terminal');
    const tab = [...m.querySelectorAll('#tabs > *')].find((el) => el.textContent.includes(label));
    const x = tab && [...tab.querySelectorAll('button')].find((b) => b.textContent.trim() === '×');
    if (x) x.click();
  }, SERIAL_LABEL);
  await sleep(800);
  const ses = JSON.parse((await mcp('serial_open', { source: serialSource, baud: 115200 })).text);
  await sleep(300);
  await mcp('serial_write', { session: ses.session, data: 'live decode works\r\n' });
  await sleep(2500);
  const dec = (await rows(page)).join('\n');
  check(/live decode works/.test(dec), `6 the decode pane shows the bytes as they cross the wire (${JSON.stringify(dec.slice(-160))})`);
  await mcp('serial_close', { session: ses.session });

  // --- 7. Closing every tab ---------------------------------------------------
  for (let i = 0; i < 6; i++) await closeActiveTab(page);
  const tabsLeft = await page.evaluate(() => [...document.querySelector('awic-module-terminal').querySelectorAll('#tabs button')].filter((b) => b.textContent.trim() === '×').length);
  check(tabsLeft === 0, `7 every session tab closes (${tabsLeft} left)`);

  await mcp('stop_recording', { rec });
  await mcp('delete_recording', { rec });
  check(page.errors.length === 0, `no page errors (${JSON.stringify(page.errors.slice(0, 5))})`);
  await browser.close();
  console.log(failures === 0 ? '\nall checks passed' : `\n${failures} check(s) failed`);
  process.exit(failures === 0 ? 0 : 1);
})();
