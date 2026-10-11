// Start/stop verification for the B-65..B-68 fixes, against a running AWIC
// server (a bench recipe, as checks rather than a random walk).
const { chromium } = require('playwright');

const URL = process.env.URL || 'http://127.0.0.1:8080/';
let failures = 0;
const check = (ok, what) => { console.log(`${ok ? 'PASS' : 'FAIL'}  ${what}`); if (!ok) failures++; };

(async () => {
  const browser = await chromium.launch();
  const page = await browser.newPage({ viewport: { width: 1400, height: 950 } });
  const errors = [];
  page.on('pageerror', (e) => errors.push(e.message));
  await page.goto(URL, { waitUntil: 'networkidle' });
  const graph = () => page.locator('awic-module-graph');
  await page.locator('nav button', { hasText: 'Graph' }).click();
  await page.waitForFunction(() => {
    const g = document.querySelector('awic-app').querySelector('awic-module-graph');
    const s = g && g.querySelector('#device');
    return !!(s && s.options.length > 0);
  });

  // Pick the analyzer (digital, where B-65 silently went to 24 MHz).
  const devices = await graph().locator('#device option').evaluateAll((os) => os.map((o) => ({ v: o.value, cap: o.dataset.capability })));
  const digital = devices.find((d) => d.cap === 'digital') || devices[0];
  await graph().locator('#device').selectOption(digital.v);
  await page.waitForTimeout(300);

  const settings = () => page.evaluate(() => {
    const g = document.querySelector('awic-app').querySelector('awic-module-graph');
    const q = (s) => g.querySelector(s);
    return {
      rate: q('#rate') ? q('#rate').value : null,
      samples: q('#samples') ? q('#samples').value : null,
      trigEnabled: q('#trigger-enabled') ? q('#trigger-enabled').checked : null,
      trigChannel: q('#trigger-channel') ? q('#trigger-channel').value : null,
      trigLevel: q('#trigger-level') ? q('#trigger-level').value : null,
      offsets: [...g.querySelectorAll('input[type=number]')].slice(0, 4).map((i) => i.value),
    };
  });

  // The rate/samples/trigger controls live in popovers (SPEC §11.3). Open the
  // ones we touch, so the clicks are the real ones a person makes.
  const openPopover = async (id) => {
    await graph().locator(`#popover-${id}-btn`).click();
    await page.waitForTimeout(150);
  };

  // Choose settings that are NOT the defaults: the slowest rate on the ladder.
  await openPopover('acq');
  const rates = await graph().locator('#rate option').evaluateAll((os) => os.map((o) => o.value));
  const slow = rates.reduce((a, b) => (Number(b) < Number(a) ? b : a));
  await graph().locator('#rate').selectOption(slow);
  // Slow and long, so the window between the click and the reply is real and
  // the acquisition is still running when the checks look at it.
  await graph().locator('#samples').fill('200000');
  await openPopover('acq'); // close it again
  const before = await settings();
  check(before.rate === slow, `rate set to ${slow} (options: ${rates.length})`);

  // --- B-65: a Start must not rewrite the settings -------------------------
  await graph().locator('#start').click();
  await page.waitForTimeout(2500);
  const after = await settings();
  check(after.rate === before.rate, `B-65 rate kept across a Start (${before.rate} -> ${after.rate})`);
  check(after.samples === before.samples, `B-65 samples kept (${before.samples} -> ${after.samples})`);
  check(
    JSON.stringify(after.offsets) === JSON.stringify(before.offsets),
    `B-65 offsets kept (${JSON.stringify(before.offsets)} -> ${JSON.stringify(after.offsets)})`,
  );

  // A second Start, as a stress run would: still the user's settings.
  await graph().locator('#start').click();
  await page.waitForTimeout(2500);
  const third = await settings();
  check(third.rate === before.rate, `B-65 rate kept across a second Start (${third.rate})`);

  // --- B-67: the window between the click and the reply --------------------
  const stateNow = () => graph().locator('#acq-status').textContent();
  const startDisabled = () => graph().locator('#start').isDisabled();
  await graph().locator('#start').click();
  await page.waitForTimeout(60); // inside the old blind window
  const duringStatus = await stateNow();
  const duringDisabled = await startDisabled();
  check(duringDisabled, `B-67 Start disabled while the start is unanswered (status "${duringStatus}")`);
  check(!/^idle$/.test(duringStatus.trim()), `B-67 status is not "idle" during a start ("${duringStatus}")`);
  await page.waitForTimeout(3000);

  // --- B-66: a single capture holds the device in Live too -----------------
  // (The capture above is still running: 200 k samples at 20 kHz.)
  await graph().locator('#mode-live').click();
  await page.waitForTimeout(400);
  const liveOverCapture = (await stateNow()).trim();
  check(!/^idle$/.test(liveOverCapture), `B-66 Live reports the running capture ("${liveOverCapture}")`);
  check(await startDisabled(), 'B-66 Live does not offer a Start over a running capture');
  await graph().locator('#stop').click();
  await page.waitForTimeout(1500);

  // --- B-66/B-68: Live mode, and what it leaves behind ---------------------
  const recIds = async () => {
    const r = await page.evaluate(async () => {
      const res = await fetch('/mcp', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json', Accept: 'application/json, text/event-stream' },
        body: JSON.stringify({ jsonrpc: '2.0', id: 1, method: 'initialize', params: { protocolVersion: '2025-06-18', capabilities: {}, clientInfo: { name: 'e2e', version: '0' } } }),
      });
      const sid = res.headers.get('Mcp-Session-Id');
      const list = await fetch('/mcp', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json', Accept: 'application/json, text/event-stream', 'Mcp-Session-Id': sid },
        body: JSON.stringify({ jsonrpc: '2.0', id: 2, method: 'tools/call', params: { name: 'list_recordings', arguments: {} } }),
      });
      const body = await list.json();
      return JSON.parse(body.result.content[0].text).recordings.map((r) => r.rec);
    });
    return r;
  };

  const before68 = await recIds();
  await page.waitForTimeout(300);
  await graph().locator('#start').click();
  await page.waitForTimeout(3000);
  const liveStatus = (await stateNow()).trim();
  check(/recording/.test(liveStatus), `B-66 Live reports the recording ("${liveStatus}")`);

  // Switching to Single while it records must show the same thing, with Stop
  // offered rather than a Start the server will refuse.
  await graph().locator('#mode-single').click();
  await page.waitForTimeout(500);
  const singleStatus = (await stateNow()).trim();
  const singleStartDisabled = await startDisabled();
  const singleStopDisabled = await graph().locator('#stop').isDisabled();
  check(/recording/.test(singleStatus), `B-66 Single reports the same recording ("${singleStatus}")`);
  check(singleStartDisabled, 'B-66 Single does not offer a Start the server would refuse');
  check(!singleStopDisabled, 'B-66 Single offers Stop while a recording holds the device');

  await graph().locator('#stop').click();
  await page.waitForTimeout(2500);
  const after68 = await recIds();
  const added = after68.filter((r) => !before68.includes(r));
  check(
    added.length === 0,
    `B-68 the Live run left nothing behind (before ${JSON.stringify(before68)}, after ${JSON.stringify(after68)})`,
  );

  check(errors.length === 0, `no page errors (${JSON.stringify(errors.slice(0, 3))})`);
  await browser.close();
  console.log(failures === 0 ? '\nall checks passed' : `\n${failures} check(s) failed`);
  process.exit(failures === 0 ? 0 : 1);
})();
