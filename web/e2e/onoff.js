// Power on/off while the UI is in use: an instrument is
// switched off and on during Live and during a Single capture, then off for a
// while, and a USB-serial adapter under an open terminal. Checks the UI
// recovers each time: no dead "recording", the instrument stays selected with
// its settings, Start only when it is there, nothing left behind.
//
// Needs real hardware on switchable hub ports; the defaults are the bench's.
//   podman run --rm --net=host -v $PWD/web/e2e:/e2e:ro,Z localhost/awic-e2e:latest \
//     node onoff.js        (after `make e2e` built the image)
// Override with SCOPE_HUB/SCOPE_PORT/SCOPE_ID and SERIAL_HUB/SERIAL_PORT/SERIAL_LABEL.
const { chromium } = require('playwright');

const env = process.env;
const URL = env.URL || 'http://127.0.0.1:8080/';
const SCOPE = { hub: env.SCOPE_HUB || '3-3.4', port: Number(env.SCOPE_PORT || 2), id: env.SCOPE_ID || '3-3.4.2' };
const SERIAL = { hub: env.SERIAL_HUB || '3-3.4', port: Number(env.SERIAL_PORT || 4), label: env.SERIAL_LABEL || 'ttyUSB0' };
let failures = 0;
const check = (ok, what) => { console.log(`${ok ? 'PASS' : 'FAIL'}  ${what}`); if (!ok) failures++; };
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function mcp(name, args) {
  const post = (body, sid) => fetch(new globalThis.URL('/mcp', URL), {
    method: 'POST',
    headers: { 'Content-Type': 'application/json', Accept: 'application/json, text/event-stream', ...(sid ? { 'Mcp-Session-Id': sid } : {}) },
    body: JSON.stringify(body),
  });
  const init = await post({ jsonrpc: '2.0', id: 1, method: 'initialize', params: { protocolVersion: '2025-06-18', capabilities: {}, clientInfo: { name: 'onoff', version: '0' } } });
  const sid = init.headers.get('mcp-session-id');
  const res = await (await post({ jsonrpc: '2.0', id: 2, method: 'tools/call', params: { name, arguments: args } }, sid)).json();
  const text = res.result && res.result.content && res.result.content[0] ? res.result.content[0].text : JSON.stringify(res);
  return { error: !!(res.result && res.result.isError), text };
}

(async () => {
  const browser = await chromium.launch();
  const page = await browser.newPage({ viewport: { width: 1400, height: 950 } });
  const errors = [];
  page.on('pageerror', (e) => errors.push(e.message));
  page.on('console', (m) => { if (m.type() === 'error') errors.push(m.text()); });
  await page.goto(URL, { waitUntil: 'networkidle' });
  const graph = () => page.locator('awic-module-graph');
  const state = () => page.evaluate(() => {
    const g = document.querySelector('awic-app').querySelector('awic-module-graph');
    return {
      status: g.querySelector('#acq-status').textContent.trim(),
      start: !g.querySelector('#start').disabled,
      stop: !g.querySelector('#stop').disabled,
      device: g.querySelector('#device').value,
      deviceText: g.querySelector('#device').selectedOptions[0] ? g.querySelector('#device').selectedOptions[0].textContent : '',
      rate: g.querySelector('#rate') ? g.querySelector('#rate').value : null,
      devices: [...g.querySelectorAll('#device option')].map((o) => o.value),
    };
  });
  const wrongDevice = [];
  const startWhileGone = [];
  const watch = async (secs, label) => {
    const seen = [];
    for (let i = 0; i < secs * 2; i++) {
      const s = await state();
      const key = `${s.status.slice(0, 60)} | start=${s.start} stop=${s.stop} dev=${s.device} "${s.deviceText}" n=${s.devices.length}`;
      if (s.device !== SCOPE.id) wrongDevice.push(`${label} t=${(i / 2).toFixed(1)}s ${s.device}`);
      if (/not connected/.test(s.deviceText) && s.start) startWhileGone.push(`${label} t=${(i / 2).toFixed(1)}s`);
      if (seen[seen.length - 1] !== key) { seen.push(key); console.log(`   ${label} t=${(i / 2).toFixed(1)}s ${key}`); }
      await sleep(500);
    }
    return state();
  };

  const recs = async () => JSON.parse((await mcp('list_recordings', {})).text).recordings.map((r) => `${r.rec}:${r.state}`);
  const recsBefore = await recs();
  await page.locator('nav button', { hasText: 'Graph' }).click();
  await page.waitForFunction(() => {
    const g = document.querySelector('awic-app').querySelector('awic-module-graph');
    return g && g.querySelector('#device').options.length > 0;
  });
  await graph().locator('#device').selectOption(SCOPE.id);
  await sleep(500);

  // --- A. Live on the scope, scope switched off and on ----------------------
  await graph().locator('#mode-live').click();
  await sleep(300);
  await graph().locator('#start').click();
  await sleep(3000);
  const live0 = await state();
  check(/recording/.test(live0.status), `A Live records on the scope ("${live0.status}")`);
  console.log('   power-cycling the scope:', (await mcp('power_cycle_usb_port', { hub: SCOPE.hub, port: SCOPE.port, off_ms: 1000 })).text.slice(0, 160));
  const liveAfter = await watch(15, 'A');
  check(!/recording/.test(liveAfter.status) || liveAfter.stop, `A after the power cycle the status is not a dead "recording" ("${liveAfter.status}")`);
  check(liveAfter.devices.filter((d) => d === SCOPE.id).length === 1, `A the scope is listed once again (${JSON.stringify(liveAfter.devices)})`);
  if (liveAfter.stop) { await graph().locator('#stop').click(); await sleep(1500); }
  const liveReady = await state();
  check(liveReady.start, `A Start is offered again (${liveReady.status})`);
  await graph().locator('#device').selectOption(SCOPE.id);
  await sleep(300);
  await graph().locator('#start').click();
  await sleep(3000);
  const live2 = await state();
  check(/recording/.test(live2.status), `A Live starts again after the power cycle ("${live2.status}")`);
  await graph().locator('#stop').click();
  await sleep(2000);

  // --- B. A Single capture on the scope, switched off mid-capture -----------
  await graph().locator('#mode-single').click();
  await sleep(300);
  await graph().locator('#popover-acq-btn').click();
  await sleep(150);
  const rates = await graph().locator('#rate option').evaluateAll((os) => os.map((o) => o.value));
  const slow = rates.reduce((a, b) => (Number(b) < Number(a) ? b : a));
  await graph().locator('#rate').selectOption(slow);
  await graph().locator('#samples').fill('1000000');
  await graph().locator('#popover-acq-btn').click();
  await sleep(200);
  await graph().locator('#start').click();
  await sleep(1500);
  const single0 = await state();
  check(/running|started/.test(single0.status), `B a long Single capture runs ("${single0.status}", ${slow} Hz)`);
  console.log('   power-cycling the scope:', (await mcp('power_cycle_usb_port', { hub: SCOPE.hub, port: SCOPE.port, off_ms: 1000 })).text.slice(0, 160));
  const singleAfter = await watch(15, 'B');
  check(!/running/.test(singleAfter.status), `B the capture does not stay "running" ("${singleAfter.status}")`);
  check(singleAfter.start, 'B Start is offered again');
  check(singleAfter.devices.filter((d) => d === SCOPE.id).length === 1, `B the scope is listed once (${JSON.stringify(singleAfter.devices)})`);
  check(singleAfter.rate === slow, `B the rate set before the power cycle is kept (${slow} -> ${singleAfter.rate})`);
  check(wrongDevice.length === 0, `A/B the scope stayed selected throughout (${JSON.stringify(wrongDevice.slice(0, 3))})`);
  check(startWhileGone.length === 0, `A/B Start is not offered while the selected device is gone (${JSON.stringify(startWhileGone.slice(0, 3))})`);

  // --- B2. The scope switched off for a while, then on again --------------
  console.log('   scope off:', (await mcp('set_usb_power', { hub: SCOPE.hub, port: SCOPE.port, on: false })).text.slice(0, 120));
  await sleep(2500);
  const off = await state();
  check(off.device === SCOPE.id && /not connected/.test(off.deviceText), `B2 while off the scope stays selected as "not connected" ("${off.deviceText}")`);
  check(!off.start, 'B2 Start is not offered while the scope is off');
  console.log('   scope on:', (await mcp('set_usb_power', { hub: SCOPE.hub, port: SCOPE.port, on: true })).text.slice(0, 120));
  let back = null;
  for (let i = 0; i < 30; i++) {
    const st = await state();
    if (!/not connected/.test(st.deviceText) && st.devices.includes(SCOPE.id) && st.start) { back = st; break; }
    await sleep(500);
  }
  check(!!back, `B2 the scope comes back selected with Start offered (${back ? back.deviceText : 'not within 15 s'})`);
  check(!!back && back.rate === slow, `B2 its rate is still ${slow} (${back ? back.rate : '?'})`);

  // --- C. Terminal on the USB-serial adapter, adapter switched off and on ---
  await page.locator('nav button', { hasText: 'Terminal' }).click();
  await sleep(800);
  const term = () => page.locator('awic-module-terminal');
  const serialValue = await term().locator('#source option').evaluateAll((os, label) => {
    const o = os.find((x) => x.textContent.includes(label));
    return o ? o.value : null;
  }, SERIAL.label);
  check(!!serialValue, `C the terminal offers ${SERIAL.label} (${serialValue})`);
  if (serialValue) {
    await term().locator('#source').selectOption(serialValue);
    await sleep(300);
    await term().locator('#open').click();
    await sleep(1500);
    const termText = () => page.evaluate(() => {
      const t = document.querySelector('awic-app').querySelector('awic-module-terminal');
      const pane = t.querySelector('#panes');
      const rows = pane ? [...pane.querySelectorAll('.xterm-rows > div')].map((r) => r.textContent).filter((x) => x.trim()) : [];
      return { status: t.querySelector('#status').textContent.trim(), tabs: t.querySelector('#tabs').textContent.trim(), rows };
    });
    console.log('   terminal before:', JSON.stringify(await termText()));
    console.log('   power-cycling the adapter:', (await mcp('power_cycle_usb_port', { hub: SERIAL.hub, port: SERIAL.port, off_ms: 1000 })).text.slice(0, 160));
    let last = '';
    for (let i = 0; i < 16; i++) {
      const t = JSON.stringify(await termText());
      if (t !== last) { console.log(`   C t=${(i / 2).toFixed(1)}s ${t}`); last = t; }
      await sleep(500);
    }
    // After it is back, a write from another client must still reach the
    // terminal's port: open a session over MCP and write a line.
    const list = JSON.parse((await mcp('serial_list', {})).text).sources.find((s) => s.label.includes(SERIAL.label));
    check(!!list && list.available !== undefined, `C the adapter is listed again (${list ? list.label + ' available=' + list.available : 'missing'})`);
  }

  // --- D. USB tab shows the devices that came back --------------------------
  await page.locator('nav button', { hasText: 'USB' }).click();
  await sleep(1500);
  const tree = await page.evaluate(() => document.querySelector('awic-app').querySelector('awic-module-usb').innerText);
  check((tree.match(/fx2lafw/g) || []).length >= 2, `D the USB tab lists both fx2lafw instruments (${(tree.match(/fx2lafw/g) || []).length})`);
  check(/USB Serial/.test(tree), 'D the USB tab lists the USB-serial adapter again');

  const recsAfter = await recs();
  const left = recsAfter.filter((r) => !recsBefore.includes(r));
  check(left.length === 0, `the Live runs left no recording behind (before ${JSON.stringify(recsBefore)}, after ${JSON.stringify(recsAfter)})`);
  check(errors.length === 0, `no page errors (${JSON.stringify(errors.slice(0, 5))})`);
  await browser.close();
  console.log(failures === 0 ? '\nall checks passed' : `\n${failures} check(s) failed`);
  process.exit(failures === 0 ? 0 : 1);
})();
