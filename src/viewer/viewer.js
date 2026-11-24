// Seiji's screen viewer. One WebSocket (`ws`, same origin, with the session cookie) carries the
// simulator's screen as H.264, decoded with WebCodecs onto a canvas, and the input: a finger that
// follows the pointer (down, move, up), wheel scrolls as drags, keys as HID usages, text through
// the simulator clipboard. The page acknowledges every frame so the server never queues more
// than a few frames for it on a slow link.
"use strict";

(() => {
  const $ = (id) => document.getElementById(id);
  const canvas = $("frame");
  const g2d = canvas.getContext("2d", { alpha: false, desynchronized: true });
  const screen = $("screen");
  const veil = $("veil");
  const veilText = $("veil-text");
  const note = $("note");
  const statsOverlay = $("stats-overlay");
  let info = null;
  let ended = false;
  let control = false;

  // ── helpers ────────────────────────────────────────────────────────────

  function showNote(text, kind) {
    if (!text) {
      note.hidden = true;
      return;
    }
    note.textContent = text;
    note.className = "note" + (kind === "error" ? " error" : "");
    note.hidden = false;
  }

  let noteTimer = 0;
  function flash(text, kind) {
    showNote(text, kind);
    clearTimeout(noteTimer);
    noteTimer = setTimeout(() => showNote(info && info.busy && control ? busyText : ""), 3500);
  }

  const busyText = "A run is using the device. Input is paused until it ends.";

  function end(message) {
    ended = true;
    veil.hidden = false;
    veilText.textContent = message;
    $("dot").className = "dot off";
    for (const b of document.querySelectorAll("button, select")) b.disabled = true;
    if (ws) ws.close();
    if (decoder && decoder.state !== "closed") decoder.close();
  }

  async function errorOf(resp) {
    try {
      const doc = await resp.json();
      return (doc && doc.error) || {};
    } catch {
      return {};
    }
  }

  function minutes(sec) {
    if (sec <= 0) return "ending";
    const m = Math.round(sec / 60);
    return m >= 60 ? `${Math.floor(m / 60)} h ${m % 60} min` : `${m} min`;
  }

  // ── info ───────────────────────────────────────────────────────────────

  async function refresh() {
    if (ended) return;
    let resp;
    try {
      resp = await fetch("info", { cache: "no-store" });
    } catch {
      $("dot").className = "dot off";
      return;
    }
    if (resp.status === 401 || resp.status === 410) {
      end((await errorOf(resp)).message || "The viewer closed.");
      return;
    }
    if (!resp.ok) return;
    info = await resp.json();
    control = !!info.control;
    // com.apple.CoreSimulator.SimRuntime.iOS-18-2 → iOS 18.2
    const runtime = (info.runtime || "")
      .replace(/^com\.apple\.CoreSimulator\.SimRuntime\./, "")
      .replace("-", " ")
      .replace(/-/g, ".");
    $("name").textContent = info.model || "Simulator";
    const app = info.app && info.app.bundle_id ? ` · ${info.app.bundle_id}` : "";
    $("meta").textContent = `${runtime}${app}`;
    $("lease").textContent = info.lease_id;
    $("ends").textContent = `ends in ${minutes(info.ends_at_latest - Date.now() / 1000)}`;
    $("viewers").textContent = info.viewers > 1 ? `${info.viewers} watching` : "";
    $("mode").hidden = control;
    for (const b of document.querySelectorAll("[data-control]")) b.hidden = !control;
    $("keys").hidden = !control;
    $("dot").className = "dot " + (info.busy ? "busy" : "live");
    showNote(info.busy && control ? busyText : "");
    if (info.preset) $("preset").value = info.preset;
  }

  // ── socket ─────────────────────────────────────────────────────────────

  let ws = null;
  let retry = 0;
  let clockOffset = 0; // server clock minus browser clock
  let rtt = null;

  function send(msg) {
    if (ws && ws.readyState === WebSocket.OPEN) ws.send(JSON.stringify(msg));
  }

  function connect() {
    if (ended) return;
    if (!("VideoDecoder" in window)) {
      end("This browser cannot show the stream: it needs WebCodecs (a current Chrome, Edge, Safari or Firefox).");
      return;
    }
    veil.hidden = false;
    veilText.textContent = retry ? "Reconnecting…" : "Connecting…";
    const url = new URL("ws", location.href);
    url.protocol = location.protocol === "https:" ? "wss:" : "ws:";
    ws = new WebSocket(url);
    ws.binaryType = "arraybuffer";
    ws.onopen = () => {
      retry = 0;
      ping();
    };
    ws.onmessage = (ev) => (typeof ev.data === "string" ? onText(JSON.parse(ev.data)) : onFrame(ev.data));
    ws.onclose = (ev) => {
      ws = null;
      if (ended) return;
      // 4401: the session expired. 4410: the device was released.
      if (ev.code === 4401 || ev.code === 4410) {
        end(ev.reason || "The viewer closed.");
        return;
      }
      veil.hidden = false;
      veilText.textContent = "Reconnecting…";
      refresh();
      retry = Math.min(retry + 1, 5);
      setTimeout(connect, 600 * retry);
    };
  }

  function ping() {
    send({ t: "ping", at: performance.now() });
  }
  setInterval(ping, 2000);

  const pending = new Map(); // input id → what it was
  let nextId = 1;

  function onText(msg) {
    switch (msg.t) {
      case "format":
        configure(msg);
        break;
      case "pong": {
        const now = performance.now();
        rtt = now - msg.at;
        // The server's clock at the middle of the round trip.
        clockOffset = msg.server_ms - (Date.now() - rtt / 2);
        break;
      }
      case "preset":
        flash(`Preset: ${msg.preset}`);
        break;
      case "done": {
        const kind = pending.get(msg.id);
        pending.delete(msg.id);
        if (kind === "paste") {
          flash(msg.via === "typed"
            ? "Typed into the focused field (the device clipboard is not reachable; US keyboard characters only)."
            : "Pasted into the focused field.");
        }
        break;
      }
      case "error":
        if (msg.id) pending.delete(msg.id);
        flash(msg.message || "Input failed.", msg.code === "busy" ? "warning" : "error");
        if (msg.code === "busy") refresh();
        break;
    }
  }

  // ── video ──────────────────────────────────────────────────────────────

  let decoder = null;
  let waitingKey = true;
  let format = null;
  const stats = { frames: [], bytes: [], age: null, drawn: 0 };

  function base64Bytes(s) {
    const bin = atob(s);
    const out = new Uint8Array(bin.length);
    for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
    return out;
  }

  function newDecoder() {
    return new VideoDecoder({
      output: (frame) => {
        if (canvas.width !== frame.displayWidth || canvas.height !== frame.displayHeight) {
          canvas.width = frame.displayWidth;
          canvas.height = frame.displayHeight;
        }
        g2d.drawImage(frame, 0, 0);
        const now = Date.now();
        stats.age = now + clockOffset - frame.timestamp / 1000;
        stats.frames.push(now);
        frame.close();
        veil.hidden = true;
      },
      error: () => {
        // A broken decoder starts over at the next keyframe.
        decoder = null;
        if (format) configure(format);
      },
    });
  }

  function configure(f) {
    format = f;
    if (!decoder || decoder.state === "closed") decoder = newDecoder();
    decoder.configure({
      codec: f.codec,
      codedWidth: f.width,
      codedHeight: f.height,
      description: base64Bytes(f.description),
      optimizeForLatency: true,
    });
    waitingKey = true;
  }

  function onFrame(buf) {
    const view = new DataView(buf);
    const key = (view.getUint8(0) & 1) === 1;
    const seq = view.getUint32(1);
    const capturedUs = Number(view.getBigInt64(5));
    send({ t: "ack", seq });
    stats.bytes.push([Date.now(), buf.byteLength]);
    if (!decoder || decoder.state !== "configured") return;
    if (waitingKey && !key) return;
    waitingKey = false;
    decoder.decode(new EncodedVideoChunk({ type: key ? "key" : "delta", timestamp: capturedUs, data: new Uint8Array(buf, 13) }));
  }

  // ── stats overlay ──────────────────────────────────────────────────────

  let statsOn = false;
  let statsTimer = 0;

  function renderStats() {
    const now = Date.now();
    stats.frames = stats.frames.filter((t) => t > now - 1000);
    stats.bytes = stats.bytes.filter(([t]) => t > now - 1000);
    const bits = stats.bytes.reduce((s, [, n]) => s + n, 0) * 8;
    const lines = [
      `FPS: ${stats.frames.length}`,
      `Bitrate: ${(bits / 1e6).toFixed(2)} Mbit/s`,
      `Resolution: ${canvas.width}×${canvas.height}`,
      `Codec: ${format ? format.codec : "—"}`,
      `Round trip: ${rtt === null ? "—" : `${Math.round(rtt)} ms`}`,
      `Frame age: ${stats.age === null ? "—" : `${Math.round(stats.age)} ms`}`,
      `Decode queue: ${decoder ? decoder.decodeQueueSize : 0}`,
    ];
    statsOverlay.textContent = lines.join("\n");
  }

  function toggleStats() {
    statsOn = !statsOn;
    statsOverlay.hidden = !statsOn;
    clearInterval(statsTimer);
    if (statsOn) {
      renderStats();
      statsTimer = setInterval(renderStats, 500);
    }
  }

  $("stats-btn").addEventListener("click", toggleStats);

  // ── touch ──────────────────────────────────────────────────────────────

  // Pointer position as 0-1 of the picture, kept inside it.
  function norm(ev) {
    const r = canvas.getBoundingClientRect();
    return {
      x: Math.min(1, Math.max(0, (ev.clientX - r.left) / r.width)),
      y: Math.min(1, Math.max(0, (ev.clientY - r.top) / r.height)),
    };
  }

  // A gesture that starts at a screen edge is a system gesture (home indicator, Notification Centre).
  function edgeOf(p) {
    if (p.y >= 0.97) return "bottom";
    if (p.y <= 0.015) return "top";
    if (p.x <= 0.015) return "left";
    if (p.x >= 0.985) return "right";
    return undefined;
  }

  const fingerDot = $("finger");
  function showFinger(p) {
    if (!p) {
      fingerDot.hidden = true;
      return;
    }
    const r = canvas.getBoundingClientRect();
    const s = screen.getBoundingClientRect();
    fingerDot.style.left = `${r.left - s.left + p.x * r.width}px`;
    fingerDot.style.top = `${r.top - s.top + p.y * r.height}px`;
    fingerDot.hidden = false;
  }

  // Moves are sent at most once per animation frame, always the latest point.
  let moveAt = null;
  let moveScheduled = false;
  function queueMove(p) {
    moveAt = p;
    if (moveScheduled) return;
    moveScheduled = true;
    requestAnimationFrame(flushMove);
  }
  function flushMove() {
    moveScheduled = false;
    if (moveAt && (finger || wheel.active)) send({ t: "touch", phase: "move", x: moveAt.x, y: moveAt.y });
    moveAt = null;
  }

  let finger = null;
  canvas.addEventListener("pointerdown", (ev) => {
    if (!control || ev.button !== 0 || finger) return;
    ev.preventDefault();
    screen.focus();
    canvas.setPointerCapture(ev.pointerId);
    endWheel();
    const p = norm(ev);
    finger = ev.pointerId;
    send({ t: "touch", phase: "down", x: p.x, y: p.y, edge: edgeOf(p) });
    showFinger(p);
  });
  canvas.addEventListener("pointermove", (ev) => {
    if (ev.pointerId !== finger) return;
    const p = norm(ev);
    queueMove(p);
    showFinger(p);
  });
  function lift(ev) {
    if (ev.pointerId !== finger) return;
    finger = null;
    flushMove();
    const p = norm(ev);
    send({ t: "touch", phase: "up", x: p.x, y: p.y });
    showFinger(null);
  }
  canvas.addEventListener("pointerup", lift);
  canvas.addEventListener("pointercancel", lift);

  // Wheel and trackpad scrolls become a drag that follows the deltas, lifted after a pause.
  const wheel = { active: false, at: null, origin: null, timer: 0 };
  function endWheel() {
    clearTimeout(wheel.timer);
    if (!wheel.active) return;
    wheel.active = false;
    flushMove();
    send({ t: "touch", phase: "up", x: wheel.at.x, y: wheel.at.y });
  }
  canvas.addEventListener(
    "wheel",
    (ev) => {
      if (!control || finger) return;
      ev.preventDefault();
      const r = canvas.getBoundingClientRect();
      if (!wheel.active) {
        const p = norm(ev);
        wheel.origin = { x: Math.min(0.9, Math.max(0.1, p.x)), y: Math.min(0.85, Math.max(0.15, p.y)) };
        wheel.at = { ...wheel.origin };
        wheel.active = true;
        send({ t: "touch", phase: "down", x: wheel.at.x, y: wheel.at.y });
      }
      const scale = ev.deltaMode === 1 ? 16 : ev.deltaMode === 2 ? r.height : 1;
      const x = wheel.at.x - (ev.deltaX * scale) / r.width;
      const y = wheel.at.y - (ev.deltaY * scale) / r.height;
      if (x < 0.02 || x > 0.98 || y < 0.05 || y > 0.95) {
        // Out of room: lift and start the next drag where this one began.
        endWheel();
      } else {
        wheel.at = { x, y };
        queueMove(wheel.at);
      }
      clearTimeout(wheel.timer);
      wheel.timer = setTimeout(endWheel, 120);
    },
    { passive: false },
  );

  // ── keyboard ───────────────────────────────────────────────────────────

  // HID usages (page 7) of a US keyboard. A character is [usage, shift].
  const SPECIAL = {
    Enter: 40, Escape: 41, Backspace: 42, Tab: 43, Delete: 76,
    ArrowRight: 79, ArrowLeft: 80, ArrowDown: 81, ArrowUp: 82,
    Home: 74, End: 77, PageUp: 75, PageDown: 78,
  };
  const US = {};
  for (let i = 0; i < 26; i++) {
    US[String.fromCharCode(97 + i)] = [4 + i, false];
    US[String.fromCharCode(65 + i)] = [4 + i, true];
  }
  "1234567890".split("").forEach((c, i) => (US[c] = [30 + i, false]));
  "!@#$%^&*()".split("").forEach((c, i) => (US[c] = [30 + i, true]));
  const PAIRS = [[" ", 44], ["-", 45, "_"], ["=", 46, "+"], ["[", 47, "{"], ["]", 48, "}"], ["\\", 49, "|"],
    [";", 51, ":"], ["'", 52, "\""], ["`", 53, "~"], [",", 54, "<"], [".", 55, ">"], ["/", 56, "?"]];
  for (const [c, u, s] of PAIRS) {
    US[c] = [u, false];
    if (s) US[s] = [u, true];
  }
  const SHIFT = 225;
  const GUI = 227;

  // Characters a US keyboard cannot type (accents, emoji) go through the clipboard. While such
  // text waits, later characters join it, so the order stays.
  let text = "";
  let textTimer = 0;
  function flushText() {
    clearTimeout(textTimer);
    if (text) paste(text);
    text = "";
  }
  function paste(t) {
    const id = nextId++;
    pending.set(id, "paste");
    send({ t: "paste", id, text: t });
  }
  function key(usage, mods) {
    flushText();
    send({ t: "key", usage, mods: mods || [] });
  }

  screen.addEventListener("keydown", (ev) => {
    if (!control) return;
    const k = ev.key.toLowerCase();
    if (ev.metaKey && ev.shiftKey && k === "h") {
      ev.preventDefault();
      flushText();
      send({ t: "button", name: "home" });
    } else if (ev.metaKey && k === "l") {
      ev.preventDefault();
      flushText();
      send({ t: "button", name: "lock" });
    } else if (ev.metaKey && k === "s") {
      ev.preventDefault();
      save();
    } else if (ev.metaKey && ev.shiftKey && k === "v") {
      ev.preventDefault();
      flushText();
      doPaste();
    } else if (ev.altKey && ev.code === "KeyI") {
      ev.preventDefault();
      toggleStats();
    } else if (ev.metaKey && ["a", "c", "x", "z"].includes(k)) {
      // Editing shortcuts go to the simulator; the browser keeps the rest.
      ev.preventDefault();
      key(US[k][0], ev.shiftKey ? [GUI, SHIFT] : [GUI]);
    } else if (ev.metaKey || ev.ctrlKey) {
      // Leave the browser's own shortcuts alone.
    } else if (SPECIAL[ev.key]) {
      ev.preventDefault();
      key(SPECIAL[ev.key], ev.shiftKey ? [SHIFT] : []);
    } else if (ev.key.length === 1 || [...ev.key].length === 1) {
      ev.preventDefault();
      const us = US[ev.key];
      if (us && !text) {
        key(us[0], us[1] ? [SHIFT] : []);
      } else {
        text += ev.key;
        clearTimeout(textTimer);
        textTimer = setTimeout(flushText, 250);
      }
    }
  });
  screen.addEventListener("paste", (ev) => {
    if (!control) return;
    const t = ev.clipboardData && ev.clipboardData.getData("text/plain");
    if (t) {
      ev.preventDefault();
      flushText();
      paste(t);
    }
  });

  // ── paste box ──────────────────────────────────────────────────────────

  const pasteBox = $("paste-box");
  const pasteText = $("paste-text");

  async function doPaste() {
    // Try the Clipboard API first (needs a user gesture and permission).
    try {
      const t = await navigator.clipboard.readText();
      if (t) {
        paste(t);
        return;
      }
    } catch { /* fall through to the text box */ }
    pasteBox.hidden = false;
    pasteText.value = "";
    pasteText.focus();
  }

  $("paste").addEventListener("click", doPaste);

  $("paste-send").addEventListener("click", () => {
    const t = pasteText.value;
    pasteBox.hidden = true;
    if (t) paste(t);
    screen.focus();
  });

  $("paste-type").addEventListener("click", () => {
    const t = pasteText.value;
    pasteBox.hidden = true;
    if (t) {
      const id = nextId++;
      pending.set(id, "type");
      send({ t: "paste_and_type", id, text: t });
      flash("Typing pasted text…");
    }
    screen.focus();
  });

  $("paste-cancel").addEventListener("click", () => {
    pasteBox.hidden = true;
    screen.focus();
  });

  // ── toolbar ────────────────────────────────────────────────────────────

  $("preset").addEventListener("change", (ev) => send({ t: "preset", name: ev.target.value }));

  function save() {
    const a = document.createElement("a");
    a.href = "screenshot.jpg";
    a.download = "";
    document.body.appendChild(a);
    a.click();
    a.remove();
  }
  $("home").addEventListener("click", () => send({ t: "button", name: "home" }));
  $("lock").addEventListener("click", () => send({ t: "button", name: "lock" }));
  $("shot").addEventListener("click", save);

  refresh().then(connect);
  setInterval(refresh, 5000);
})();
