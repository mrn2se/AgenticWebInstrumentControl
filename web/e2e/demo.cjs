// End-to-end check of the GitHub Pages build against its simulated bench, in
// a real browser. Serve web/dist-pages, then: URL=http://127.0.0.1:4173/ node demo.cjs
// SHOTS=dir also saves screenshots.
const { chromium } = require('playwright');

const URL = process.env.URL || 'http://127.0.0.1:4173/';
const SHOTS = process.env.SHOTS;
let failures = 0;
const check = (ok, what) => { console.log(`${ok ? 'PASS' : 'FAIL'}  ${what}`); if (!ok) failures++; };

(async () => {
  const browser = await chromium.launch();
  const context = await browser.newContext({ viewport: { width: 1280, height: 820 } });
  const page = await context.newPage();
  const errors = [];
  page.on('pageerror', (e) => errors.push(e.message));
  page.on('console', (m) => { if (m.type() === 'error') errors.push(m.text()); });
  const shot = async (name) => { if (SHOTS) await page.screenshot({ path: `${SHOTS}/${name}.png` }); };
  // Lets the checks below read the plot's own pixels.
  await page.addInitScript(() => {
    const getContext = HTMLCanvasElement.prototype.getContext;
    HTMLCanvasElement.prototype.getContext = function (type, options) {
      return getContext.call(this, type, type === 'webgl2' ? { ...options, preserveDrawingBuffer: true } : options);
    };
  });
  // Columns of the two scope lanes holding a second stroke: a clean trace is one contiguous run per column.
  const strayColumns = () => page.evaluate(() => {
    const canvas = [...document.querySelectorAll('awic-module-graph canvas')].find((c) => c.getContext('webgl2'));
    const gl = canvas.getContext('webgl2');
    const w = canvas.width, h = canvas.height, px = new Uint8Array(w * h * 4);
    gl.readPixels(0, 0, w, h, gl.RGBA, gl.UNSIGNED_BYTE, px);
    const lit = (x, y) => {
      const i = (y * w + x) * 4;
      return Math.abs(px[i] - px[0]) + Math.abs(px[i + 1] - px[1]) + Math.abs(px[i + 2] - px[2]) > 40;
    };
    let columns = 0;
    for (const [y0, y1] of [[0, h >> 1], [h >> 1, h]]) {
      for (let x = 0; x < w; x++) {
        let runs = 0, inRun = false, gap = 99;
        for (let y = y0 + 2; y < y1 - 2; y++) {
          if (lit(x, y)) { if (!inRun) { if (gap > 3) runs++; inRun = true; } gap = 0; } else { inRun = false; gap++; }
        }
        if (runs > 1) columns++;
      }
    }
    return columns;
  });

  await page.goto(URL, { waitUntil: 'load' });
  await page.waitForSelector('dialog#welcome[open]');
  await page.waitForTimeout(1100);
  check(true, 'the page greets with the welcome dialog');
  await shot('01-welcome');

  // The install assistant answers its choices.
  await page.click('#welcome-tabs [data-tab=install]');
  await page.check('input[name=run][value=systemd]');
  await page.focus('input[name=attestation]');
  await page.keyboard.press('Space');
  const focused = await page.evaluate(() => document.activeElement?.getAttribute('name'));
  check(focused === 'attestation', `changing an option keeps keyboard focus on it (${focused})`);
  const script = await page.textContent('#welcome-script');
  check(script.includes('awic.service') && script.includes('gh attestation verify'), 'install assistant: systemd and attestation show in the commands');
  await shot('02-install');

  const links = await page.locator('#welcome a[href*="#install"], #welcome a[href*="example-report"], #welcome a[href*="DEVELOPMENT"], #welcome a[href*="discussions"]').count();
  check((await page.locator('#welcome a[href*="legal.html"]').count()) === 3, 'the dialog links the legal notice, disclaimer and privacy');
  check(links === 4, `the dialog links installation, development, example reports and discussions (${links})`);
  check((await page.locator('#welcome-tabs button').count()) === 2, 'the dialog offers the demo and the install assistant');

  await page.click('#welcome-tabs [data-tab=demo]');
  await page.click('#welcome-demo');
  await page.waitForFunction(() => document.querySelector('awic-module-graph #device')?.options.length >= 2);
  check(!(await page.isVisible('dialog#welcome')), 'starting the demo closes the dialog');
  check((await page.textContent('#demo-notice')).includes('Simulated'), 'the demo says it is simulated');

  // A logic capture and a decode.
  const graph = page.locator('awic-module-graph');
  const deviceOptions = await graph.locator('#device option').evaluateAll((o) => o.map((x) => ({ v: x.value, t: x.textContent })));
  const logic = deviceOptions.find((o) => /logic/i.test(o.t));
  const scope = deviceOptions.find((o) => /oscillo/i.test(o.t));
  await graph.locator('#device').selectOption(logic.v);
  await page.waitForTimeout(200);
  await graph.locator('#popover-acq-btn').click();
  await graph.locator('#rate').selectOption('8000000');
  await graph.locator('#samples').fill('160000');
  await graph.locator('#samples').dispatchEvent('change');
  await graph.locator('#popover-acq-btn').click();
  await graph.locator('#start').click();
  await page.waitForTimeout(1200);
  await shot('04-logic');
  await graph.locator('#decode-decoder').selectOption('uart').catch(() => {});
  await page.waitForTimeout(100);
  const roles = graph.locator('#decode-roles select');
  if (await roles.count()) await roles.first().selectOption({ label: 'D0' }).catch(async () => { await roles.first().selectOption('0'); });
  await roles.first().selectOption({ label: 'D7' });
  await graph.locator('#decode-apply').click();
  await page.waitForTimeout(300);
  const refusal = await graph.locator('#decode-error').textContent();
  check(/D0/.test(refusal), `a decode on the wrong wire says where the traffic is (${refusal})`);
  await roles.first().selectOption({ label: 'D0' });
  await graph.locator('#decode-apply').click();
  await page.waitForTimeout(500);
  const frames = await graph.locator('#decode-frames').textContent();
  check(/0x74/.test(frames) || /temp/.test(frames), `UART decode lists the line's bytes (${frames.slice(0, 60)}...)`);
  await shot('05-decode');

  // The scope.
  await graph.locator('#device').selectOption(scope.v);
  await page.waitForTimeout(200);
  await graph.locator('#start').click();
  await page.waitForTimeout(1000);
  await shot('06-scope');
  const stray = await strayColumns();
  check(stray === 0, `the scope capture draws no stray strokes (${stray} columns)`);
  check(await graph.locator('#image').isDisabled(), 'the Image button is off where no server can draw it');

  // Leaving for Live and coming back keeps the capture's status.
  await graph.locator('#mode-live').click();
  await page.waitForTimeout(150);
  await graph.locator('#mode-single').click();
  await page.waitForTimeout(150);
  const backStatus = await graph.locator('#acq-status').textContent();
  check(/stopped/.test(backStatus), `back from Live, the status still says the capture ended (${backStatus})`);

  // Live.
  await graph.locator('#mode-live').click();
  await page.waitForTimeout(150);
  await graph.locator('#start').click();
  await page.waitForTimeout(1500);
  const liveFrames = await graph.evaluate((el) => Number(el.dataset.liveFrames || 0));
  check(liveFrames > 5, `live mode receives view frames (${liveFrames})`);
  await shot('07-live');
  await graph.locator('#stop').click();
  await page.waitForTimeout(300);
  await graph.locator('#mode-single').click();

  // USB.
  await page.locator('nav button', { hasText: 'USB' }).click();
  await page.waitForTimeout(500);
  const usbText = await page.locator('awic-module-usb').textContent();
  check(/logic analyzer/i.test(usbText) && /ESP32/i.test(usbText), 'USB tree shows instruments and serial devices');
  await shot('08-usb');

  // Terminal.
  await page.locator('nav button', { hasText: 'Terminal' }).click();
  await page.waitForTimeout(300);
  const term = page.locator('awic-module-terminal');
  await term.locator('#source').selectOption('serial:/dev/ttyACM0');
  await term.locator('#open').click();
  await page.waitForTimeout(600);
  await term.locator('.xterm:visible').first().click();
  await page.keyboard.type('status');
  await page.keyboard.press('Enter');
  await page.waitForTimeout(500);
  await shot('09-terminal');
  const termText = await term.evaluate((el) => [...el.querySelectorAll('.xterm-rows')].map((r) => r.textContent).join(' '));
  check(/awic-demo/.test(termText), 'the serial terminal shows the boot log');
  check(/uptime/.test(termText), 'the serial terminal answers a command');

  // MCP.
  await page.locator('nav button', { hasText: 'MCP' }).click();
  await page.waitForTimeout(800);
  const mcpText = await page.locator('awic-module-mcp').textContent();
  check(/survey_channels/.test(mcpText) && /Claude Code/.test(mcpText), 'MCP tab shows the agent session');
  await shot('10-mcp');

  check((await page.locator('#demo-notice a[href*="legal.html"]').count()) === 1, 'the demo keeps a link to the legal page');
  await page.locator('#demo-notice a[href*="legal.html"]').click();
  await page.waitForSelector('#privacy');
  check(/GitHub\s+Pages/.test(await page.textContent('main')), 'the legal page opens and names the host');
  check(await page.evaluate(() => getComputedStyle(document.body).backgroundColor !== 'rgba(0, 0, 0, 0)'), 'the legal page is styled');
  // CI adds these two files when it deploys; a local build does not have them.
  for (const file of ['THIRD-PARTY-LICENSES.txt', 'awic-web.cdx.json']) {
    check((await context.request.get(new globalThis.URL(file, page.url()).href)).status() === 200 || process.env.NO_NOTICES === '1', `the page's ${file} exists`);
  }

  check(errors.length === 0, `no page errors ${JSON.stringify(errors.slice(0, 5))}`);
  await browser.close();
  console.log(failures ? `${failures} failed` : 'all passed');
  process.exit(failures ? 1 : 0);
})();
