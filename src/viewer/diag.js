// Seiji's viewer diagnosis: a developer tool, not a feature for users. ⌃⌥D in the viewer, or
// `#diag` at the end of its address, loads it (`#diag=auto` also starts a run). A run opens the
// probe page (`probe.html`) on the device, plays a fixed script of taps, keys and drags, and reads
// the probe's grid back out of every decoded frame. From that it times each input stage by stage
// and finds inputs lost, repeated or wrong, stalls and damaged frames. The report shows in a
// panel, can be copied as JSON, and goes to the server (`GET /v1/admin/viewer/diag`).
"use strict";

(() => {
  const V = window.__seijiViewer;
  if (!V || window.__seijiDiag) return;

  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
  const now = () => performance.now();

  // ── reading the probe ──────────────────────────────────────────────────

  // The grid is found in a small copy of the canvas, by its red and blue corner cells. Then
  // each frame's grid, and every 8th frame's texture, is cut out of the canvas (or the WebRTC
  // video) with `createImageBitmap` and read at about 20 pixels a cell: 1-4 ms a frame, where a
  // readback of the whole canvas took 20 ms a frame in WebKit and slowed the very page it
  // measured.
  const SAMPLE_W = 240;
  const grab = document.createElement("canvas");
  const g = grab.getContext("2d", { willReadFrequently: true });
  const cut = document.createElement("canvas");
  const cg = cut.getContext("2d", { willReadFrequently: true });
  let geo = null; // {x, y, cell} of the grid, in frame pixels
  let misses = 0;
  let locatedAt = -Infinity;
  let reading = false;

  function pixels(src, sw, sh) {
    const h = Math.max(1, Math.round((SAMPLE_W * sh) / sw));
    if (grab.width !== SAMPLE_W || grab.height !== h) {
      grab.width = SAMPLE_W;
      grab.height = h;
    }
    g.drawImage(src, 0, 0, SAMPLE_W, h);
    return g.getImageData(0, 0, SAMPLE_W, h);
  }

  const isRed = (r, gg, b) => r > 170 && gg < 90 && b < 90;
  const isBlue = (r, gg, b) => b > 170 && r < 90 && gg < 90;

  // The bounding box of the pixels that pass `test` inside a box of the sample.
  function blob(img, test, bx1, by1, bx2, by2) {
    let x1 = Infinity, y1 = Infinity, x2 = -1, y2 = -1, n = 0;
    const d = img.data;
    for (let y = Math.max(0, by1); y < Math.min(img.height, by2); y++) {
      for (let x = Math.max(0, bx1); x < Math.min(img.width, bx2); x++) {
        const i = (y * img.width + x) * 4;
        if (test(d[i], d[i + 1], d[i + 2])) {
          n++;
          if (x < x1) x1 = x;
          if (x > x2) x2 = x;
          if (y < y1) y1 = y;
          if (y > y2) y2 = y;
        }
      }
    }
    return n < 4 ? null : { x1, y1, x2: x2 + 1, y2: y2 + 1 };
  }

  // The grid's red and blue corner cells: the highest red pixels on the left, then only the
  // rows of that cell, so the video's colour bleed in the texture below never counts.
  function locate(img) {
    const w = img.width;
    const top = blob(img, isRed, 0, 0, w / 4, img.height / 2);
    if (!top) return null;
    const rows = [top.y1, top.y1 + w / 8];
    const red = blob(img, isRed, 0, rows[0], w / 4, rows[1]);
    const blue = blob(img, isBlue, (3 * w) / 4, rows[0], w, rows[1]);
    if (!red || !blue) return null;
    const cell = (blue.x2 - red.x1) / 11;
    if (cell < 4 || Math.abs(red.x2 - red.x1 - cell) > cell * 0.35 || Math.abs(blue.y1 - red.y1) > cell * 0.3) return null;
    return { x: red.x1, y: red.y1, cell };
  }

  // The luma at a point of the frame, in frame pixels, from a crop of the grid's rows (and the
  // texture's), scaled to about 20 pixels a cell.
  async function reader(src, rows) {
    const sx = Math.max(0, Math.floor(geo.x));
    const sy = Math.max(0, Math.floor(geo.y));
    const sw = Math.ceil(11 * geo.cell);
    const sh = Math.ceil(rows * geo.cell);
    const k = 220 / sw;
    const w = 220;
    const h = Math.max(1, Math.round(sh * k));
    const bitmap = await createImageBitmap(src, sx, sy, sw, sh, { resizeWidth: w, resizeHeight: h });
    if (cut.width !== w || cut.height !== h) {
      cut.width = w;
      cut.height = h;
    }
    cg.drawImage(bitmap, 0, 0);
    bitmap.close();
    const d = cg.getImageData(0, 0, w, h).data;
    return (x, y) => {
      const i = (Math.round((y - sy) * k) * w + Math.round((x - sx) * k)) * 4;
      return 0.299 * d[i] + 0.587 * d[i + 1] + 0.114 * d[i + 2];
    };
  }

  // The mean luma of a cell's middle.
  function cellLuma(at, r, c) {
    const { x, y, cell } = geo;
    let s = 0, n = 0;
    for (let a = 0.3; a <= 0.71; a += 0.1) {
      for (let b = 0.3; b <= 0.71; b += 0.1) {
        s += at(x + (c + a) * cell, y + (r + b) * cell);
        n++;
      }
    }
    return s / n;
  }

  // The still texture, as lumas on a 40 x 24 lattice.
  function textureOf(at) {
    const { x, y, cell } = geo;
    const out = new Float32Array(40 * 24);
    for (let r = 0; r < 24; r++) {
      for (let c = 0; c < 40; c++) {
        const v = at(x + (0.5 + (c + 0.5) / 4) * cell, y + (6.5 + (r + 0.5) / 4) * cell);
        if (!Number.isFinite(v)) return null;
        out[r * 40 + c] = v;
      }
    }
    return out;
  }

  function decodeGrid(at, texture) {
    const cal = [];
    for (let c = 1; c <= 9; c++) cal.push(cellLuma(at, 0, c));
    const white = (cal[0] + cal[2] + cal[4] + cal[6] + cal[8]) / 5;
    const black = (cal[1] + cal[3] + cal[5] + cal[7]) / 4;
    const cut = (white + black) / 2;
    if (!(white - black >= 60) || cal.some((v, i) => (v > cut) !== (i % 2 === 0))) {
      misses++;
      return { bad: "calibration" };
    }
    const bits = [];
    for (let r = 1; r <= 5; r++) {
      if (cellLuma(at, r, 0) > cut || cellLuma(at, r, 10) > cut) {
        misses++;
        return { bad: "border" };
      }
      for (let c = 1; c <= 9; c++) bits.push(cellLuma(at, r, c) > cut ? 1 : 0);
    }
    let i = 0;
    const field = (n) => {
      let v = 0;
      for (let k = 0; k < n; k++) v |= bits[i++] << k;
      return v;
    };
    const p = { taps: field(8), keys: field(8), raf: field(8), last: field(7), px: field(5), py: field(5) };
    const check = field(4);
    if (((p.taps + 3 * p.keys + 5 * p.raf + 7 * p.last + 11 * p.px + 13 * p.py) & 15) !== check) {
      misses++;
      return { bad: "check" };
    }
    misses = 0;
    p.texture = texture ? textureOf(at) : null;
    return p;
  }

  // ── recording ──────────────────────────────────────────────────────────

  let rec = null; // the run being recorded
  const pctl = (a, p) => {
    if (!a.length) return null;
    const s = [...a].sort((x, y) => x - y);
    return Math.round(s[Math.round((s.length - 1) * p)] * 10) / 10;
  };
  const stat = (a) => (a.length ? { n: a.length, p50: pctl(a, 0.5), p95: pctl(a, 0.95), max: pctl(a, 1) } : null);
  function serverMs(pageTime) {
    return Date.now() - (now() - pageTime) + V.state().clockOffset;
  }

  V.hooks.arrive = (a) => {
    if (!rec) return;
    rec.arrivals.set(a.seq, a);
    if (a.key) rec.phase.keyframes++;
    if (a.refresh) rec.phase.refreshes++;
    if (rec.lastSeq !== null && a.seq > rec.lastSeq + 1) rec.phase.skipped += a.seq - rec.lastSeq - 1;
    rec.lastSeq = a.seq;
    rec.phase.arrived++;
  };

  V.hooks.text = (msg) => {
    if (!rec) return;
    if (msg.t === "applied") {
      const input = rec.byTs.get(msg.ts);
      if (input) input.applied = { read_us: msg.read_us, sent_us: msg.sent_us };
    } else if (msg.t === "done" && rec.waiting && msg.id === rec.waiting.id) {
      rec.waiting.resolve(null);
    } else if (msg.t === "error") {
      if (rec.waiting && msg.id === rec.waiting.id) rec.waiting.resolve(msg.message || msg.code);
      rec.errors.push({ at: Math.round(now() - rec.t0), code: msg.code, message: msg.message });
    }
  };

  // A failure here is the diagnosis's own: it goes in the report, never into the viewer.
  function failed(e) {
    if (rec) rec.errors.push({ at: Math.round(now() - rec.t0), code: "diag", message: String((e && e.stack) || e).slice(0, 500) });
  }
  V.hooks.frame = (...args) => {
    try {
      onFrame(...args);
    } catch (e) {
      failed(e);
    }
  };
  for (const name of ["arrive", "text"]) {
    const hook = V.hooks[name];
    V.hooks[name] = (...args) => {
      try {
        hook(...args);
      } catch (e) {
        failed(e);
      }
    };
  }

  function onFrame(view, sw, sh, capturedUs, seq, at) {
    if (!rec) return;
    const ph = rec.phase;
    ph.shown++;
    const q = V.state().decodeQueue;
    if (q > ph.queueMax) ph.queueMax = q;
    const f = { at, cap: capturedUs, seq, phase: ph.name };
    rec.frames.push(f);
    if (misses > 15) {
      geo = null;
      misses = 0;
    }
    if (!geo) {
      // A canvas readback is slow: look for the grid a few times a second at most.
      if (at - locatedAt >= 300) {
        locatedAt = at;
        const s = locate(pixels(view, sw, sh));
        const k = sw / SAMPLE_W;
        geo = s && { x: s.x * k, y: s.y * k, cell: s.cell * k };
      }
      if (!geo) {
        f.bad = "no grid";
        return;
      }
    }
    if (reading) {
      f.unread = true;
      ph.unread++;
      return;
    }
    const texture = rec.frames.length % 8 === 0;
    const start = now();
    reading = true;
    reader(view, texture ? 12.6 : 6)
      .then((at) => {
        if (!rec) return;
        rec.readMs.push(now() - start);
        apply(f, decodeGrid(at, texture));
      })
      .catch(failed)
      .finally(() => (reading = false));
  }

  function apply(f, p) {
    const ph = rec.phases.find((x) => x.name === f.phase);
    if (p.bad) {
      f.bad = p.bad;
      ph.damaged++;
      rec.damage.push({ at: Math.round(f.at - rec.t0), phase: f.phase, why: p.bad, ...context(f.seq) });
      return;
    }
    Object.assign(f, { taps: p.taps, keys: p.keys, raf: p.raf, last: p.last, px: p.px, py: p.py });
    if (p.texture) {
      if (!rec.base && rec.baseFrames) {
        rec.baseFrames.push(p.texture);
        if (rec.baseFrames.length === 5) {
          rec.base = new Float32Array(p.texture.length);
          for (const t of rec.baseFrames) for (let i = 0; i < t.length; i++) rec.base[i] += t[i] / 5;
          rec.baseSharp = sharpness(rec.base);
        }
      } else if (rec.base) {
        let d = 0;
        for (let i = 0; i < p.texture.length; i++) d += Math.abs(p.texture[i] - rec.base[i]);
        f.drift = d / p.texture.length;
        f.sharp = sharpness(p.texture) / rec.baseSharp;
        if (f.drift > 12) {
          ph.damaged++;
          rec.damage.push({ at: Math.round(f.at - rec.t0), phase: f.phase, why: `texture off by ${f.drift.toFixed(1)}`, ...context(f.seq) });
        }
      }
    }
    match(f);
  }

  function sharpness(t) {
    let s = 0;
    for (let r = 0; r < 24; r++) for (let c = 1; c < 40; c++) s += Math.abs(t[r * 40 + c] - t[r * 40 + c - 1]);
    return s || 1;
  }

  // What came just before a damaged frame: was it a keyframe, a refresh, or after skipped frames?
  function context(seq) {
    if (seq === null || seq === undefined) return {};
    const a = rec.arrivals.get(seq);
    let sinceKey = null;
    for (let s = seq; s > seq - 600 && s >= 0; s--) {
      const b = rec.arrivals.get(s);
      if (b && b.key) {
        sinceKey = seq - s;
        break;
      }
    }
    const prev = rec.arrivals.get(seq - 1);
    return { seq, key: a ? a.key : null, refresh: a ? a.refresh : null, after_skip: !prev, frames_since_key: sinceKey };
  }

  // The server-clock time an input went out: when the helper had it, or when the page sent it.
  function sentUs(input) {
    return input.applied ? input.applied.sent_us : serverMs(input.page) * 1000;
  }

  // Inputs waiting for the frame that shows them. A phase counts its touches and keys from the
  // last frame captured before its first input: opening the probe may reload a page that had
  // counted before, and on a slow page the frames shown lag the inputs by seconds. Only a frame
  // captured after an input went out can show it.
  function match(f) {
    for (const p of rec.phases) {
      if (p.firstUs === null || f.cap < p.firstUs) {
        p.baseTaps = f.taps;
        p.baseKeys = f.keys;
      }
    }
    const d = (v, b) => (v - b) & 255;
    for (const input of rec.open) {
      const p = input.ph;
      if (input.seen || p.baseTaps === null || f.cap < sentUs(input)) continue;
      if (input.kind === "tap" && d(f.taps, p.baseTaps) >= input.expect) seen(input, f);
      else if (input.kind === "key" && d(f.keys, p.baseKeys) >= input.expect) {
        seen(input, f);
        if (d(f.keys, p.baseKeys) === input.expect && f.last !== input.char) input.wrong = f.last;
      } else if (input.kind === "move" && Math.abs(f.px - input.px) <= 1) seen(input, f);
    }
    // A later move shown first stands for the earlier ones.
    for (const input of rec.open) {
      if (input.kind !== "move" || input.seen) continue;
      if (rec.open.some((o) => o.kind === "move" && o.seen && o.n > input.n && o.gesture === input.gesture)) input.superseded = true;
    }
    rec.open = rec.open.filter((i) => !i.seen && !i.superseded);
  }

  function seen(input, f) {
    input.seen = { at: f.at, cap: f.cap, seq: f.seq };
  }

  // ── the script ─────────────────────────────────────────────────────────

  function phase(name) {
    // The last frame read was captured before this phase's first input; frames captured later
    // but still before it move the base on as they come (`match`).
    const last = rec.frames.findLast((f) => f.taps !== undefined);
    const p = {
      name, start: now(), end: null, shown: 0, arrived: 0, keyframes: 0, refreshes: 0, skipped: 0, damaged: 0, unread: 0,
      queueMax: 0, firstUs: null, baseTaps: last ? last.taps : null, baseKeys: last ? last.keys : null,
    };
    if (rec.phase) rec.phase.end = p.start;
    rec.phase = p;
    rec.phases.push(p);
    rec.sentTaps = 0;
    rec.sentKeys = 0;
    progress(`${name}…`);
    return p;
  }

  function sendTimed(msg, input) {
    msg.ts = now();
    if (input) {
      const p = rec.phase;
      if (p.firstUs === null) p.firstUs = serverMs(msg.ts) * 1000;
      input.ph = p;
      input.page = msg.ts;
      rec.byTs.set(msg.ts, input);
      rec.inputs.push(input);
      rec.open.push(input);
    }
    return V.sendInput(msg);
  }

  async function tap(x, y) {
    rec.sentTaps++;
    const input = { n: rec.inputs.length, kind: "tap", phase: rec.phase.name, expect: rec.sentTaps, px: Math.round(x * 31) };
    sendTimed({ t: "touch", phase: "down", x, y }, input);
    await sleep(50);
    sendTimed({ t: "touch", phase: "up", x, y });
  }

  // Lowercase letters and spaces only: one HID usage each.
  async function type(text, gap) {
    for (const ch of text) {
      rec.sentKeys++;
      const usage = ch === " " ? 44 : 4 + ch.charCodeAt(0) - 97;
      const input = { n: rec.inputs.length, kind: "key", phase: rec.phase.name, expect: rec.sentKeys, char: ch.charCodeAt(0) };
      sendTimed({ t: "key", usage, mods: [] }, input);
      await sleep(gap);
    }
  }

  async function drag(gesture, y) {
    rec.sentTaps++;
    const down = { n: rec.inputs.length, kind: "tap", phase: rec.phase.name, expect: rec.sentTaps, px: Math.round(0.2 * 31) };
    sendTimed({ t: "touch", phase: "down", x: 0.2, y }, down);
    for (let i = 1; i <= 24; i++) {
      await sleep(16);
      const x = 0.2 + (0.6 * i) / 24;
      const input = { n: rec.inputs.length, kind: "move", phase: rec.phase.name, gesture, px: Math.round(x * 31) };
      sendTimed({ t: "touch", phase: "move", x, y }, input);
    }
    await sleep(16);
    sendTimed({ t: "touch", phase: "up", x: 0.8, y });
  }

  async function waitFor(test, ms) {
    const until = now() + ms;
    while (now() < until) {
      if (test()) return true;
      await sleep(50);
    }
    return false;
  }

  async function run() {
    const st = V.state();
    if (!st.control) throw new Error("this viewer is view-only: the diagnosis needs control");
    rec = {
      t0: now(), phase: null, phases: [], inputs: [], open: [], byTs: new Map(), frames: [], arrivals: new Map(),
      damage: [], errors: [], readMs: [], lastSeq: null, sentTaps: 0, sentKeys: 0,
      base: null, baseFrames: null, baseSharp: 1, waiting: null,
    };
    geo = null;
    phase("open");
    const id = 990000 + Math.floor(Math.random() * 9999);
    const opened = new Promise((resolve) => (rec.waiting = { id, resolve }));
    V.send({ t: "diag_open", id });
    const err = await Promise.race([opened, sleep(30000).then(() => "the device did not open the probe page in 30 s")]);
    rec.waiting = null;
    if (err) throw new Error(err);
    const read = () => rec.frames.filter((f) => f.taps !== undefined);
    if (!(await waitFor(() => read().length >= 20, 20000))) {
      const why = rec.frames.slice(-5).map((f) => f.bad || (f.unread ? "unread" : "pending")).join(", ") || "no frames";
      throw new Error(`the probe page did not show in the video (${why})`);
    }
    rec.baseFrames = [];
    await sleep(1000);

    phase("idle");
    await sleep(5000);
    phase("taps");
    for (let i = 0; i < 20; i++) {
      await tap(0.25 + 0.5 * ((i * 7) % 10) / 10, 0.45 + 0.2 * ((i * 3) % 5) / 5);
      await sleep(350);
    }
    phase("tap_burst");
    for (let i = 0; i < 15; i++) {
      await tap(0.3 + 0.4 * (i % 5) / 5, 0.55);
      await sleep(50);
    }
    await sleep(1500);
    phase("keys");
    await type("the quick brown fox jumps over the lazy dog", 140);
    await sleep(1000);
    phase("key_burst");
    await type("sphinx of black quartz judge my vow", 30);
    await sleep(1500);
    phase("drags");
    for (let i = 0; i < 4; i++) {
      await drag(i, 0.5 + 0.05 * i);
      await sleep(600);
    }
    phase("settle");
    await sleep(3000);
    rec.phase.end = now();
    const report = summarize();
    // The raw recording of the last run, for a developer at the console.
    api.recording = rec;
    rec = null;
    return report;
  }

  // ── the report ─────────────────────────────────────────────────────────

  function summarize() {
    const st = V.state();
    const ws = !st.rtcOn;
    const pageOf = (us) => now() - (Date.now() + st.clockOffset - us / 1000);
    const phases = [];
    for (const p of rec.phases) {
      if (p.name === "open") continue;
      const secs = (p.end - p.start) / 1000;
      const frames = rec.frames.filter((f) => f.phase === p.name && f.taps !== undefined);
      const caps = new Set(frames.map((f) => f.cap)).size;
      let raf = 0;
      for (let i = 1; i < frames.length; i++) raf += (frames[i].raf - frames[i - 1].raf) & 255;
      const sharp = frames.filter((f) => f.sharp).map((f) => f.sharp);
      const inputs = rec.inputs.filter((i) => i.phase === p.name);
      // What the device counted beyond what was sent: the last frame captured before the next
      // phase's first input.
      const next = rec.phases.slice(rec.phases.indexOf(p) + 1).find((q) => q.firstUs !== null);
      const end = rec.frames.findLast((f) => f.taps !== undefined && (!next || f.cap < next.firstUs));
      const extra = (count, base, kind) => {
        const sent = inputs.filter((i) => i.kind === kind).length;
        const n = end && sent && base !== null ? ((count(end) - base) & 255) - sent : 0;
        return n > 0 && n < 100 ? n : 0;
      };
      phases.push({
        name: p.name,
        secs: Math.round(secs * 10) / 10,
        fps_shown: Math.round(p.shown / secs),
        fps_arrived: ws ? Math.round(p.arrived / secs) : null,
        fps_captured: Math.round(caps / secs),
        fps_device: frames.length > 1 ? Math.round(raf / ((frames.at(-1).at - frames[0].at) / 1000)) : null,
        keyframes: p.keyframes,
        refreshes: p.refreshes,
        skipped: p.skipped,
        damaged: p.damaged,
        decode_queue_max: p.queueMax,
        unread: p.unread,
        extra_touches: extra((f) => f.taps, p.baseTaps, "tap"),
        extra_keys: extra((f) => f.keys, p.baseKeys, "key"),
        sharpness_p50: sharp.length ? pctl(sharp, 0.5) : null,
        sharpness_min: sharp.length ? pctl(sharp, 0) : null,
        ...inputStats(inputs, ws, pageOf),
      });
    }
    const gaps = stalls(ws);
    const report = {
      version: 1,
      at: new Date().toISOString(),
      ua: navigator.userAgent,
      lease: st.info ? st.info.lease_id : null,
      model: st.info ? st.info.model : null,
      transport: ws ? "ws" : "rtc",
      codec: st.format ? st.format.codec : null,
      size: st.format ? `${st.format.width}x${st.format.height}` : null,
      rtt_ms: st.rtt === null ? null : Math.round(st.rtt),
      server: st.serverStages,
      phases,
      stalls: gaps.slice(0, 100),
      damage: rec.damage.slice(0, 100),
      errors: rec.errors.slice(0, 50),
      read_ms: stat(rec.readMs),
    };
    report.verdict = verdict(report);
    // A page message holds at most 64 KiB: shorten the longest lists until the report fits.
    const size = () => new TextEncoder().encode(JSON.stringify({ t: "diag_report", report })).length;
    while (size() > 60000 && (report.stalls.length || report.damage.length || report.errors.length)) {
      for (const k of ["stalls", "damage", "errors"]) report[k] = report[k].slice(0, Math.floor(report[k].length / 2));
    }
    return report;
  }

  function inputStats(inputs, ws, pageOf) {
    const out = { sent: inputs.length, lost: 0, lost_after_helper: 0, wrong: 0, superseded: 0 };
    const t = { total: [], upload: [], server: [], device: [], pipeline: [], encode: [], helper: [], down: [], decode: [] };
    for (const i of inputs) {
      if (i.superseded) {
        out.superseded++;
        continue;
      }
      if (!i.seen) {
        out.lost++;
        if (i.applied) out.lost_after_helper++;
        continue;
      }
      if (i.wrong !== undefined) out.wrong++;
      t.total.push(i.seen.at - i.page);
      const shownCap = pageOf(i.seen.cap);
      t.pipeline.push(i.seen.at - shownCap);
      if (i.applied) {
        t.upload.push(i.applied.read_us / 1000 - serverMs(i.page));
        t.server.push((i.applied.sent_us - i.applied.read_us) / 1000);
        t.device.push((i.seen.cap - i.applied.sent_us) / 1000);
      }
      const a = ws && i.seen.seq !== null ? rec.arrivals.get(i.seen.seq) : null;
      if (a) {
        t.encode.push((a.encodedUs - a.capturedUs) / 1000);
        t.helper.push((a.receivedUs - a.encodedUs) / 1000);
        // From the helper to the page: the server's queue for this page, then the network.
        t.down.push(a.at - pageOf(a.receivedUs));
        t.decode.push(i.seen.at - a.at);
      }
    }
    out.ms = {};
    for (const [k, v] of Object.entries(t)) out.ms[k] = stat(v);
    return out;
  }

  // Gaps of more than 150 ms between frames shown, and where the time went.
  function stalls(ws) {
    const out = [];
    const shown = rec.frames.filter((f) => f.phase !== "open");
    for (let i = 1; i < shown.length; i++) {
      const a = shown[i - 1];
      const b = shown[i];
      const gap = b.at - a.at;
      if (gap < 150) continue;
      const capGap = (b.cap - a.cap) / 1000;
      const raf = a.raf !== undefined && b.raf !== undefined ? (b.raf - a.raf) & 255 : null;
      let where;
      if (capGap > gap * 0.6) {
        // The frames were not made or not sent: the device drew few new frames, the server
        // skipped them, or the helper captured none.
        if (raf !== null && raf < (capGap / 1000) * 60 * 0.3) where = "device: the app drew no new frames";
        else if (ws && b.seq !== null && a.seq !== null && b.seq - a.seq > 1) where = "server skipped frames";
        else where = "capture or encode: the device drew, nothing was sent";
      } else if (ws && b.seq !== null && rec.arrivals.get(b.seq) && b.at - rec.arrivals.get(b.seq).at > gap * 0.5) {
        where = "decoder";
      } else {
        where = "network or server queue";
      }
      out.push({ at: Math.round(a.at - rec.t0), phase: b.phase, gap_ms: Math.round(gap), capture_gap_ms: Math.round(capGap), device_frames: raf, where });
    }
    return out;
  }

  function verdict(r) {
    const lines = [];
    const names = { upload: "page → server", server: "in server (lock, pacing)", device: "device: helper → app drew → captured", pipeline: "capture → shown (encode, network, decode)" };
    for (const p of r.phases) {
      if (!p.sent) continue;
      const ms = p.ms;
      const parts = ["upload", "server", "device", "pipeline"].filter((k) => ms[k]).map((k) => [k, ms[k].p50]);
      parts.sort((a, b) => b[1] - a[1]);
      const total = ms.total ? `${ms.total.p50} ms p50, ${ms.total.p95} ms p95` : "never shown";
      const most = parts.length ? `; most of it: ${names[parts[0][0]]} (${parts[0][1]} ms)` : "";
      let line = `${p.name}: ${total}${most}`;
      if (p.lost) line += `; ${p.lost} of ${p.sent} lost (${p.lost_after_helper} after the helper had them)`;
      if (p.wrong) line += `; ${p.wrong} wrong keys`;
      if (p.extra_touches || p.extra_keys) line += `; the device saw ${p.extra_touches} touches and ${p.extra_keys} keys more than were sent`;
      lines.push(line);
    }
    const where = {};
    for (const s of r.stalls) where[s.where] = (where[s.where] || 0) + 1;
    for (const [w, n] of Object.entries(where)) lines.push(`${n} stalls over 150 ms: ${w}`);
    const damaged = r.phases.reduce((s, p) => s + p.damaged, 0);
    if (damaged) {
      const after = r.damage.filter((d) => d.refresh).length;
      const skip = r.damage.filter((d) => d.after_skip).length;
      lines.push(`${damaged} damaged frames (${after} at a refresh frame, ${skip} after skipped frames)`);
    }
    return lines;
  }

  function text(r) {
    const f = (s) => (s ? `${s.p50}/${s.p95}` : "—");
    const read = r.read_ms ? `${r.read_ms.p50}/${r.read_ms.p95} ms a frame` : "—";
    const out = [`Seiji viewer diagnosis · ${r.transport} · ${r.codec} ${r.size} · rtt ${r.rtt_ms} ms · read ${read}`, ""];
    out.push(...r.verdict.map((l) => `• ${l}`), "");
    out.push("phase       fps shown/arrived/captured/device  key ref skip dmg  queue");
    for (const p of r.phases) {
      out.push(`${p.name.padEnd(11)} ${String(p.fps_shown).padStart(3)}/${String(p.fps_arrived ?? "—").padStart(3)}/${String(p.fps_captured).padStart(3)}/${String(p.fps_device ?? "—").padStart(3)}                 ${String(p.keyframes).padStart(3)} ${String(p.refreshes).padStart(3)} ${String(p.skipped).padStart(4)} ${String(p.damaged).padStart(3)}  ${p.decode_queue_max}`);
    }
    out.push("", "input (p50/p95 ms)  total     upload  server  device  pipeline  (encode helper server+net decode)");
    for (const p of r.phases) {
      if (!p.sent) continue;
      const m = p.ms;
      out.push(`${p.name.padEnd(11)} ${String(p.sent - p.lost).padStart(3)}/${String(p.sent).padStart(3)} ${f(m.total).padEnd(9)} ${f(m.upload).padEnd(7)} ${f(m.server).padEnd(7)} ${f(m.device).padEnd(7)} ${f(m.pipeline).padEnd(9)} (${f(m.encode)} ${f(m.helper)} ${f(m.down)} ${f(m.decode)})`);
    }
    return out.join("\n");
  }

  // ── panel ──────────────────────────────────────────────────────────────

  const panel = document.createElement("div");
  panel.className = "diag";
  const head = document.createElement("div");
  head.className = "diag-head";
  const body = document.createElement("pre");
  const btn = (label, fn) => {
    const b = document.createElement("button");
    b.type = "button";
    b.textContent = label;
    b.addEventListener("click", fn);
    head.appendChild(b);
    return b;
  };
  const title = document.createElement("strong");
  title.textContent = "Diagnosis";
  head.appendChild(title);
  let last = null;
  let running = false;
  const runBtn = btn("Run", () => start());
  btn("Copy JSON", () => last && navigator.clipboard.writeText(JSON.stringify(last, null, 2)).catch(() => {}));
  btn("Close", () => (panel.hidden = true));
  panel.append(head, body);
  document.body.appendChild(panel);
  body.textContent = "Runs a fixed script of taps, keys and drags on a probe page it opens on the device (about 40 s). Do not touch the screen meanwhile.";

  function progress(s) {
    body.textContent = s;
  }

  async function start() {
    if (running) return;
    running = true;
    runBtn.disabled = true;
    panel.hidden = false;
    try {
      last = await run();
      body.textContent = text(last);
      V.send({ t: "diag_report", report: last });
    } catch (e) {
      rec = null;
      last = { error: String(e.message || e) };
      body.textContent = `The diagnosis failed: ${last.error}`;
    } finally {
      running = false;
      runBtn.disabled = false;
      api.result = last;
    }
  }

  const api = {
    result: null,
    start,
    show: () => (panel.hidden = false),
  };
  window.__seijiDiag = api;
  if (/^#diag=auto/.test(location.hash)) {
    waitFor(() => V.state().format !== null, 30000).then(() => sleep(1500)).then(start);
  }
})();
