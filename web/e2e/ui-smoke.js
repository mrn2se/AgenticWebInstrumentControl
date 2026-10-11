// UI smoke test against a running AWIC server: `make e2e` (URL=... to point
// elsewhere). Exits non-zero on the first failed check. Each check is a bug
// that shipped once and that no unit test could see.
const { chromium } = require('playwright');

const URL = process.env.URL || 'http://127.0.0.1:8080/';
let failures = 0;
const check = (ok, what) => { console.log(`${ok ? 'PASS' : 'FAIL'}  ${what}`); if (!ok) failures++; };

(async () => {
  const browser = await chromium.launch();
  const page = await browser.newPage({ viewport: { width: 1280, height: 900 } });
  const errors = [];
  page.on('pageerror', (e) => errors.push(e.message));
  page.on('console', (m) => { if (m.type() === 'error') errors.push(m.text()); });
  await page.goto(URL, { waitUntil: 'networkidle' });
  await page.waitForTimeout(1000);

  // Light DOM throughout (SPEC §11: a global Tailwind stylesheet cannot reach
  // into a shadow root) — the shell and every module are plain descendants
  // of <awic-app>, no shadowRoot hop needed.
  const visibleModules = () => page.evaluate(() => {
    const main = document.querySelector('awic-app').querySelector('#content');
    return [...main.children].filter((el) => el.getBoundingClientRect().height > 0).map((el) => el.tagName.toLowerCase());
  });

  // 1. Switching tabs shows exactly the chosen module. (Hidden modules once
  //    stayed on screen: a shadow-DOM module's own `:host { display }` rule
  //    outranked `[hidden]`. Light DOM plus Tailwind's `!important`
  //    `[hidden]` preflight rule is what keeps this fixed.)
  const tabs = await page.locator('nav button').allTextContents();
  check(tabs.length >= 2, `tabs present: ${JSON.stringify(tabs)}`);
  for (const tab of [...tabs, tabs[0]]) {
    await page.locator('nav button', { hasText: tab }).click();
    await page.waitForTimeout(500);
    const visible = await visibleModules();
    check(visible.length === 1, `after "${tab}": exactly one module visible ${JSON.stringify(visible)}`);
  }

  // 2. The plot keeps its size. (It once grew without bound: its container
  //    had no definite height, so every responsive resize enlarged it.)
  await page.locator('nav button', { hasText: 'Graph' }).click();
  const plotHeight = () => page.evaluate(() => {
    const graph = document.querySelector('awic-app').querySelector('awic-module-graph');
    const plot = graph && graph.querySelector('#plot, .plot');
    return plot ? Math.round(plot.getBoundingClientRect().height) : -1;
  });
  await page.waitForTimeout(1500);
  const h1 = await plotHeight();
  await page.waitForTimeout(3000);
  const h2 = await plotHeight();
  check(h1 > 0 && h1 === h2, `plot height stable over 3 s (${h1}px -> ${h2}px)`);

  // 3b. Starting an acquisition (fake backend, T15) actually draws into the
  //     default WebGL2 canvas — the renderer "draws only on change", so a
  //     blank canvas after a real capture would mean nothing landed.
  await page.waitForFunction(() => {
    const graph = document.querySelector('awic-app').querySelector('awic-module-graph');
    const select = graph && graph.querySelector('#device');
    return !!(select && select.options.length > 0);
  }, { timeout: 5000 }).catch(() => {});
  const rendererChoice = await page.evaluate(() => {
    const graph = document.querySelector('awic-app').querySelector('awic-module-graph');
    return graph && graph.querySelector('#renderer') ? graph.querySelector('#renderer').value : null;
  });
  check(rendererChoice === 'webgl2', `graph module defaults to the webgl2 renderer (got "${rendererChoice}")`);

  await page.locator('awic-module-graph #start').click();
  await page.waitForTimeout(2000); // let at least one sample frame stream in and get drawn

  const glCanvasStats = () => page.evaluate(() => {
    const graph = document.querySelector('awic-app').querySelector('awic-module-graph');
    const canvas = graph && graph.querySelector('canvas[data-role="webgl2-gl"]');
    if (!canvas) return { found: false };
    const gl = canvas.getContext('webgl2');
    if (!gl) return { found: true, hasGl: false };
    const w = canvas.width, h = canvas.height;
    if (w === 0 || h === 0) return { found: true, hasGl: true, empty: true };
    // One full-framebuffer read (a thin 1px digital level line can sit
    // between sparsely sampled points) rather than many small reads, which
    // also avoids the GPU-stall cost of dozens of separate `readPixels`
    // round-trips — this renderer sets `preserveDrawingBuffer: true`
    // specifically so a readback like this sees the last drawn frame.
    const buf = new Uint8Array(w * h * 4);
    gl.readPixels(0, 0, w, h, gl.RGBA, gl.UNSIGNED_BYTE, buf);
    const bg = [buf[0], buf[1], buf[2]];
    let diffCount = 0;
    for (let i = 0; i < buf.length; i += 4) {
      if (buf[i] !== bg[0] || buf[i + 1] !== bg[1] || buf[i + 2] !== bg[2]) diffCount++;
    }
    return { found: true, hasGl: true, empty: false, diffCount };
  });
  const glStats = await glCanvasStats();
  check(glStats.found && glStats.hasGl, `webgl2 canvas has a live GL context: ${JSON.stringify(glStats)}`);
  check(
    !!glStats.diffCount && glStats.diffCount > 0,
    `webgl2 canvas shows drawn lanes, not a blank background (${glStats.diffCount} differing pixels)`,
  );

  // 3c. Wheel-zoom on the canvas changes the reported viewport (and so the
  //     time axis labels, which are drawn to the same canvas from the same
  //     viewport — SPEC §5.3's onViewportChange). `.hover()` (not a bare
  //     `mouse.move`) scrolls the canvas into view first — the plot sits
  //     below the fold at this viewport size, and a wheel dispatched at a
  //     coordinate outside the visible viewport lands on nothing.
  const viewRange = () => page.evaluate(() => {
    const graph = document.querySelector('awic-app').querySelector('awic-module-graph');
    return {
      start: graph.querySelector('#view-start').value,
      end: graph.querySelector('#view-end').value,
    };
  });
  const before = await viewRange();
  const glCanvasLocator = page.locator('awic-module-graph canvas[data-role="webgl2-gl"]');
  const canvasVisible = await glCanvasLocator.isVisible().catch(() => false);
  if (canvasVisible) {
    await glCanvasLocator.hover();
    await page.mouse.wheel(0, -400); // zoom in
    await page.waitForTimeout(300);
  }
  const after = await viewRange();
  check(
    canvasVisible && (before.start !== after.start || before.end !== after.end),
    `wheel-zoom over the plot changes the reported time range (${JSON.stringify(before)} -> ${JSON.stringify(after)})`,
  );

  // 3d. A popover opens on click and closes on Escape (SPEC §11.3) without
  //     ever pushing the plot down — checked by the plot-height check above
  //     already covering the whole session; this checks the open/close
  //     behaviour itself.
  const displayPopoverHidden = () => page.evaluate(() => {
    const graph = document.querySelector('awic-app').querySelector('awic-module-graph');
    return graph.querySelector('#popover-display-panel').hidden;
  });
  await page.locator('awic-module-graph #popover-display-btn').click();
  await page.waitForTimeout(150);
  check((await displayPopoverHidden()) === false, 'Display popover opens on click');
  await page.keyboard.press('Escape');
  await page.waitForTimeout(150);
  check((await displayPopoverHidden()) === true, 'Display popover closes on Escape');

  // 3e. Live mode (SPEC §5.5): starts (or attaches to) a recording, opens a
  //     follow view, and the WebGL2 canvas keeps changing as new view frames
  //     stream in — a canvas that stops changing would mean the follow view
  //     silently stalled. The fake digital device is picked explicitly (by
  //     the picker's own `data-capability`, SPEC §5.1: never by model name
  //     or label — the fake backend's devices carry no label at all) so its
  //     `D0`..`D7` channel names are available for the pattern search check
  //     below.
  const digitalDeviceSelected = await page.evaluate(() => {
    const graph = document.querySelector('awic-app').querySelector('awic-module-graph');
    const select = graph.querySelector('#device');
    const digital = [...select.options].find((o) => o.dataset.capability === 'digital');
    if (!digital) return false;
    select.value = digital.value;
    select.dispatchEvent(new Event('change', { bubbles: true }));
    return true;
  });
  check(digitalDeviceSelected, 'Live mode: a digital-capability fake device is available to select');

  // 3e-bis. The Digital capability now carries a rate ladder, not just a
  // ceiling ("the UI cannot pick 1 MHz for the logic analyzer"), so the
  // Acquisition popover's rate select must offer more than the one option
  // it used to be stuck with for a digital device.
  await page.waitForTimeout(200);
  const digitalRateOptionCount = await page.evaluate(() => {
    const graph = document.querySelector('awic-app').querySelector('awic-module-graph');
    const select = graph.querySelector('#rate');
    return select ? select.options.length : 0;
  });
  check(
    digitalRateOptionCount > 1,
    `Acquisition popover offers more than one rate for a digital device (${digitalRateOptionCount} options)`,
  );

  await page.locator('awic-module-graph #mode-live').click();
  await page.waitForTimeout(200);
  // Record at 1 MHz, as on the real bench: at the 24 MHz ceiling a debug
  // server snapshots tens of millions of samples and the checks below time
  // out on speed, not on correctness.
  await page.evaluate(() => {
    const select = document.querySelector('awic-module-graph #rate');
    if ([...select.options].some((o) => o.value === '1000000')) {
      select.value = '1000000';
      select.dispatchEvent(new Event('change', { bubbles: true }));
    }
  });
  await page.locator('awic-module-graph #start').click();
  await page.waitForTimeout(500);

  const canvasFingerprint = () => page.evaluate(() => {
    const graph = document.querySelector('awic-app').querySelector('awic-module-graph');
    const canvas = graph.querySelector('canvas[data-role="webgl2-gl"]');
    const gl = canvas && canvas.getContext('webgl2');
    if (!gl || canvas.width === 0 || canvas.height === 0) return null;
    const w = canvas.width, h = canvas.height;
    const buf = new Uint8Array(w * h * 4);
    gl.readPixels(0, 0, w, h, gl.RGBA, gl.UNSIGNED_BYTE, buf);
    // A cheap content fingerprint (sampled, not the whole buffer) is enough
    // to tell "still updating" from "stalled" between two readbacks.
    let sum = 0;
    for (let i = 0; i < buf.length; i += 977) sum += buf[i];
    return sum;
  });
  await page.locator('awic-module-graph canvas[data-role="webgl2-gl"]').scrollIntoViewIfNeeded().catch(() => {});
  const liveFingerprint1 = await canvasFingerprint();
  await page.waitForTimeout(2000);
  const liveFingerprint2 = await canvasFingerprint();
  check(
    liveFingerprint1 !== null && liveFingerprint2 !== null,
    `Live mode: webgl2 canvas is readable (${liveFingerprint1}, ${liveFingerprint2})`,
  );
  // Streaming is judged by frames arriving, not by pixels changing: a fast
  // signal renders as solid toggled bands that can be identical frame to frame.
  const liveFrames = () => page.evaluate(() => Number(document.querySelector('awic-module-graph').dataset.liveFrames || 0));
  // Start-up (rec.start, first view) takes a variable while on a debug
  // server; measure the rate only once frames have begun.
  await page
    .waitForFunction(() => Number(document.querySelector('awic-module-graph').dataset.liveFrames || 0) > 0, { timeout: 5000 })
    .catch(() => {});
  const frames1 = await liveFrames();
  await page.waitForTimeout(1000);
  const frames2 = await liveFrames();
  // A liveness check, not a speed test: a debug server computes ~5 views/s
  // of the fake's dense signal; a release server reaches the full 30 fps.
  check(
    frames1 > 0 && frames2 - frames1 >= 3,
    `Live mode: view frames keep arriving (${frames1} -> ${frames2} in 1 s)`,
  );

  // 3f. No page scroll at 1280x720 on the Graph tab, in both modes (SPEC §11.3).
  await page.setViewportSize({ width: 1280, height: 720 });
  await page.waitForTimeout(300);
  const noScroll = (label) => page.evaluate((label) => {
    const info = { scrollHeight: document.scrollingElement.scrollHeight, innerHeight: window.innerHeight };
    return { label, ...info };
  }, label);
  let scroll = await noScroll('Live');
  check(
    scroll.scrollHeight <= scroll.innerHeight + 1,
    `Graph tab (${scroll.label} mode): no page scroll at 1280x720 (scrollHeight ${scroll.scrollHeight} <= innerHeight ${scroll.innerHeight})`,
  );
  await page.locator('awic-module-graph #mode-single').click();
  await page.waitForTimeout(200);
  scroll = await noScroll('Single');
  check(
    scroll.scrollHeight <= scroll.innerHeight + 1,
    `Graph tab (${scroll.label} mode): no page scroll at 1280x720 (scrollHeight ${scroll.scrollHeight} <= innerHeight ${scroll.innerHeight})`,
  );
  await page.setViewportSize({ width: 1280, height: 900 });
  await page.locator('awic-module-graph #mode-live').click();
  await page.waitForTimeout(200);

  // 3g. Snapshot (SPEC §5.5, §2.8.5) turns the visible Live range into a
  //     capture and shows a measurement via the existing capture.measure UI.
  await page.evaluate(() => {
    const graph = document.querySelector('awic-app').querySelector('awic-module-graph');
    const panel = graph.querySelector('#side-panel');
    if (panel.hidden) graph.querySelector('#panel-toggle').click();
  });
  await page.locator('awic-module-graph #snapshot').click();
  await page.waitForTimeout(500);
  await page.evaluate(() => {
    const graph = document.querySelector('awic-app').querySelector('awic-module-graph');
    const button = graph.querySelector('#snapshot-channels button');
    if (button) button.click();
  });
  const measurementResolved = await page
    .waitForFunction(() => {
      const graph = document.querySelector('awic-app').querySelector('awic-module-graph');
      const table = graph.querySelector('#measurements table');
      if (!table) return false;
      const rows = [...table.querySelectorAll('tr')].slice(1);
      return rows.length > 0 && rows.every((r) => !r.textContent.includes('measuring'));
    }, { timeout: 5000 })
    .then(() => true)
    .catch(() => false);
  check(measurementResolved, 'Snapshot: Measure resolves every requested measurement row (none stuck "measuring…")');
  const measurementHasRealValue = await page.evaluate(() => {
    const graph = document.querySelector('awic-app').querySelector('awic-module-graph');
    const table = graph.querySelector('#measurements table');
    const rows = table ? [...table.querySelectorAll('tr')].slice(1) : [];
    return rows.some((r) => !r.textContent.includes('unavailable') && !r.textContent.includes('error'));
  });
  check(measurementHasRealValue, 'Snapshot: at least one measurement shows a real value, not "unavailable"');

  // 3h. Pattern search (SPEC §2.9, §5.5): the fake digital device's D0
  //     toggles every sample, so it always has rising edges — "D0 rising"
  //     must return at least one match with no parse error.
  await page.evaluate(() => {
    const graph = document.querySelector('awic-app').querySelector('awic-module-graph');
    const input = graph.querySelector('#search-pattern');
    input.value = 'D0 rising';
    input.dispatchEvent(new Event('input', { bubbles: true }));
  });
  await page.locator('awic-module-graph #search-btn').click();
  const searchMatched = await page
    .waitForFunction(() => {
      const graph = document.querySelector('awic-app').querySelector('awic-module-graph');
      const results = graph.querySelector('#search-results');
      return !!results && results.querySelectorAll('button').length > 0;
    }, { timeout: 5000 })
    .then(() => true)
    .catch(() => false);
  // Only the fake backend (bus 253) is sure to toggle D0; on the real bench
  // D0 is whatever is wired there, often an idle line with no edge at all.
  const fakeDevicePresent = await page.evaluate(() =>
    [...document.querySelectorAll('awic-module-graph #device option')].some((o) => o.value.startsWith('253-')));
  if (fakeDevicePresent) {
    check(searchMatched, 'Pattern search: "D0 rising" returns at least one clickable match');
  } else {
    console.log('SKIP  Pattern search matches: no fake device on this bench (D0 may never rise)');
  }
  const searchError = await page.evaluate(() => {
    const graph = document.querySelector('awic-app').querySelector('awic-module-graph');
    return graph.querySelector('#search-error').textContent.trim();
  });
  check(searchError === '', `Pattern search: no parse error shown for a valid pattern (got "${searchError}")`);

  // 3i. Decode (SPEC §6, T21): the fake digital device's D5 carries a
  //     genuinely decodable UART byte stream (`awic-fake`'s own doc comment,
  //     `crates/awic-fake/src/digital.rs`) — bind UART's "rx" role to D5,
  //     apply, and expect at least one decoded frame in the side panel.
  await page.waitForFunction(() => {
    const graph = document.querySelector('awic-app').querySelector('awic-module-graph');
    const select = graph && graph.querySelector('#decode-decoder');
    return !!(select && select.options.length > 0);
  }, { timeout: 5000 }).catch(() => {});
  const decodeFormReady = await page.evaluate(() => {
    const graph = document.querySelector('awic-app').querySelector('awic-module-graph');
    const decoderSelect = graph.querySelector('#decode-decoder');
    if (!decoderSelect) return false;
    decoderSelect.value = 'uart';
    decoderSelect.dispatchEvent(new Event('change', { bubbles: true }));
    const roleSelect = graph.querySelector('#decode-roles select[data-role="rx"]');
    const d5 = roleSelect && [...roleSelect.options].find((o) => o.textContent === 'D5');
    if (!roleSelect || !d5) return false;
    roleSelect.value = d5.value;
    roleSelect.dispatchEvent(new Event('change', { bubbles: true }));
    return true;
  });
  check(decodeFormReady, 'Decode: the UART decoder and D5 (its role "rx") are selectable in the side panel');
  await page.locator('awic-module-graph #decode-apply').click();
  const decodeFramesShown = await page
    .waitForFunction(() => {
      const graph = document.querySelector('awic-app').querySelector('awic-module-graph');
      const frames = graph.querySelector('#decode-frames');
      return !!frames && frames.querySelectorAll('button').length > 0;
    }, { timeout: 5000 })
    .then(() => true)
    .catch(() => false);
  // Only the fake backend (bus 253) carries a UART on D5; on the real bench
  // D5 is unwired, so an empty frame list is the right answer there.
  const fakeBench = await page.evaluate(() =>
    [...document.querySelectorAll('awic-module-graph #device option')].some((o) => o.value.startsWith('253-')));
  if (fakeBench) {
    check(decodeFramesShown, 'Decode: applying UART on D5 lists at least one decoded frame');
  } else {
    console.log('SKIP  Decode frames: no fake device on this bench (D5 carries no UART)');
  }
  const decodeError = await page.evaluate(() => {
    const graph = document.querySelector('awic-app').querySelector('awic-module-graph');
    return graph.querySelector('#decode-error').textContent.trim();
  });
  check(decodeError === '', `Decode: no error shown for a valid decode (got "${decodeError}")`);
  // Clicking a decoded frame must not throw (it centers the view, SPEC:
  // "clicking one centers the view on it") — a real console error would
  // already have failed the page's own error listener at the end of this
  // script, but clicking explicitly here is what actually exercises it.
  if (fakeBench) {
    await page.evaluate(() => {
      const graph = document.querySelector('awic-app').querySelector('awic-module-graph');
      graph.querySelector('#decode-frames button').click();
    });
  }

  await page.locator('awic-module-graph #stop').click();
  await page.waitForTimeout(300);

  // 3. The theme switch applies and the three states are distinct.
  for (const [label, dark] of [['Dark', true], ['Light', false]]) {
    await page.locator('#theme-switch button', { hasText: label }).click();
    await page.waitForTimeout(200);
    const bg = await page.evaluate(() => getComputedStyle(document.body).backgroundColor);
    const lum = bg.match(/\d+/g).slice(0, 3).reduce((a, v) => a + Number(v), 0) / 3;
    check(dark ? lum < 80 : lum > 180, `theme "${label}" renders a ${dark ? 'dark' : 'light'} page (${bg})`);
  }

  // 4. The Terminal tab (SPEC §11.2/§11.2.1): opening the always-present
  //    `log` source actually shows real server log text (xterm's
  //    accessibility tree mirrors its rendered buffer regardless of which
  //    visual renderer — dom/canvas/webgl — is in play, so it's a reliable
  //    place to check text content from outside), and the terminal fills
  //    the viewport without pushing the page itself into scrolling — the
  //    same class of bug check 2 above guards against for the plot.
  await page.locator('nav button', { hasText: 'Terminal' }).click();
  await page.waitForFunction(() => {
    const term = document.querySelector('awic-app').querySelector('awic-module-terminal');
    const select = term && term.querySelector('#source');
    return !!(select && [...select.options].some((o) => o.value === 'log'));
  }, { timeout: 5000 }).catch(() => {});
  await page.evaluate(() => {
    const term = document.querySelector('awic-app').querySelector('awic-module-terminal');
    const select = term.querySelector('#source');
    select.value = 'log';
    select.dispatchEvent(new Event('change', { bubbles: true }));
    term.querySelector('#open').click();
  });
  const logTextOk = await page
    .waitForFunction(() => {
      const term = document.querySelector('awic-app').querySelector('awic-module-terminal');
      const tree = term && term.querySelector('.xterm-accessibility-tree');
      return !!tree && tree.textContent.trim().length > 0;
    }, { timeout: 10000 })
    .then(() => true)
    .catch(() => false);
  check(logTextOk, 'Terminal tab: opening the log source shows real log text');

  // 4a-bis. The `decode` source picker entry (SPEC §11.2, T21): "the
  //         Terminal tab's source picker offers 'Decode…'" — grouped and
  //         labelled like every other source kind, always available (no
  //         recording required to *see* the option, only to open it).
  const decodeOptionPresent = await page.evaluate(() => {
    const term = document.querySelector('awic-app').querySelector('awic-module-terminal');
    const select = term.querySelector('#source');
    return [...select.options].some((o) => o.value === 'decode');
  });
  check(decodeOptionPresent, 'Terminal tab: the source picker offers a "Decode" entry');

  // 4b. The `scpi` terminal source (SPEC §9, §11.2): opening it and typing
  //     `*IDN?` (via the client-side LineEditor's local echo, then a real
  //     term.write round trip) shows AWIC's own identity line — the same
  //     engine TCP port 5025 serves, reached here through the browser
  //     console instead of a socket.
  await page.evaluate(() => {
    const term = document.querySelector('awic-app').querySelector('awic-module-terminal');
    const select = term.querySelector('#source');
    select.value = 'scpi';
    select.dispatchEvent(new Event('change', { bubbles: true }));
    term.querySelector('#open').click();
  });
  await page.waitForTimeout(500);
  // The newly opened session becomes the active (non-hidden) pane; clicking
  // its container focuses xterm's own input capture, the same way a user
  // would click into the terminal before typing.
  await page.locator('awic-module-terminal #panes > div:not([hidden])').click();
  await page.keyboard.type('*IDN?');
  await page.keyboard.press('Enter');
  const scpiIdnShown = await page
    .waitForFunction(() => {
      const term = document.querySelector('awic-app').querySelector('awic-module-terminal');
      // Scoped to the visible pane specifically: the `log` session opened
      // just above is still in the DOM (hidden, not closed), with its own
      // `.xterm-accessibility-tree` earlier in document order — an
      // unscoped querySelector would silently check the wrong terminal.
      const tree = term && term.querySelector('#panes > div:not([hidden]) .xterm-accessibility-tree');
      return !!tree && tree.textContent.includes('AWIC,awic,0,');
    }, { timeout: 5000 })
    .then(() => true)
    .catch(() => false);
  check(scpiIdnShown, 'Terminal tab: SCPI console answers *IDN? with the AWIC identity line');

  // 1280x720 specifically (SPEC §11.3), not this script's own 1280x900 —
  // the smallest viewport the layout is required to fit without scrolling.
  await page.setViewportSize({ width: 1280, height: 720 });
  await page.waitForTimeout(300);
  const scrollInfo = await page.evaluate(() => ({
    scrollHeight: document.scrollingElement.scrollHeight,
    innerHeight: window.innerHeight,
  }));
  check(
    scrollInfo.scrollHeight <= scrollInfo.innerHeight + 1,
    `Terminal tab: no page scroll at 1280x720 (scrollHeight ${scrollInfo.scrollHeight} <= innerHeight ${scrollInfo.innerHeight})`,
  );
  await page.setViewportSize({ width: 1280, height: 900 });

  // The active tab's label is readable in both themes. (It once rendered
  // muted text on the accent background: two classes set `color`, and the
  // compiled stylesheet's order, not the class attribute, picked the wrong one.)
  for (const theme of ['Light', 'Dark']) {
    await page.locator('#theme-switch button', { hasText: theme }).click();
    await page.waitForTimeout(200);
    const contrast = await page.evaluate(() => {
      const active = [...document.querySelectorAll('nav button')].find((b) => b.getAttribute('class').includes('bg-awic-accent'));
      if (!active) return null;
      const style = getComputedStyle(active);
      // WCAG relative luminance and contrast ratio of text over background.
      const lum = (css) => {
        const [r, g, b] = css.match(/[\d.]+/g).slice(0, 3).map((v) => {
          const c = Number(v) / 255;
          return c <= 0.03928 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4;
        });
        return 0.2126 * r + 0.7152 * g + 0.0722 * b;
      };
      const [hi, lo] = [lum(style.color), lum(style.backgroundColor)].sort((a, b) => b - a);
      return { color: style.color, background: style.backgroundColor, ratio: (hi + 0.05) / (lo + 0.05) };
    });
    check(
      contrast !== null && contrast.ratio >= 3,
      `active tab label is readable in ${theme}: contrast ${contrast ? contrast.ratio.toFixed(2) : '?'} >= 3 (${JSON.stringify(contrast)})`,
    );
  }

  // 5. The MCP tab (SPEC §8.5): shows this server's endpoint, and a tool
  //    call made by an MCP client appears in its log and sessions list.
  {
    await page.locator('nav button', { hasText: 'MCP' }).click();
    await page.waitForTimeout(500);
    const endpoint = await page.locator('awic-module-mcp #mcp-endpoint').textContent();
    check(endpoint === new globalThis.URL('/mcp', URL).href, `MCP tab shows the endpoint (${endpoint})`);
    const post = (body, session) => fetch(new globalThis.URL('/mcp', URL), {
      method: 'POST',
      headers: { 'content-type': 'application/json', ...(session ? { 'Mcp-Session-Id': session } : {}) },
      body: JSON.stringify(body),
    });
    const init = await post({ jsonrpc: '2.0', id: 1, method: 'initialize', params: { protocolVersion: '2025-06-18', clientInfo: { name: 'ui-smoke' } } });
    const session = init.headers.get('mcp-session-id');
    await post({ jsonrpc: '2.0', id: 2, method: 'tools/call', params: { name: 'list_instruments', arguments: {} } }, session);
    await page.waitForFunction(() => [...document.querySelectorAll('awic-module-mcp #mcp-log summary')]
      .some((s) => s.textContent.includes('list_instruments')), null, { timeout: 5000 }).catch(() => {});
    const logged = await page.locator('awic-module-mcp #mcp-log summary', { hasText: 'list_instruments' }).count();
    check(logged >= 1, `an MCP tool call shows up in the MCP tab's log (${logged})`);
    // The session list arrives in its own fetch, which can land after the
    // log's: wait for it as for the log entry, rather than counting at once.
    await page.waitForFunction(() => [...document.querySelectorAll('awic-module-mcp #sessions li')]
      .some((li) => li.textContent.includes('ui-smoke')), null, { timeout: 5000 }).catch(() => {});
    const sessions = await page.locator('awic-module-mcp #sessions li', { hasText: 'ui-smoke' }).count();
    check(sessions >= 1, `the MCP client shows up as a session (${sessions})`);
    await fetch(new globalThis.URL('/mcp', URL), { method: 'DELETE', headers: { 'Mcp-Session-Id': session } });
  }

  check(errors.length === 0, `no page errors${errors.length ? ': ' + errors.join(' | ') : ''}`);
  await browser.close();
  console.log(failures ? `\n${failures} check(s) failed` : '\nall checks passed');
  process.exit(failures ? 1 : 0);
})();
