// Seiji's screen viewer: shows the simulator's MJPEG stream and turns pointer, wheel and keyboard
// input into taps, swipes and typing. Every request is same-origin under the session path, with
// the session cookie; input also carries `X-Seiji-View: 1`.
"use strict";

(() => {
  const $ = (id) => document.getElementById(id);
  const frame = $("frame");
  const screen = $("screen");
  const veil = $("veil");
  const veilText = $("veil-text");
  const note = $("note");
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
    frame.removeAttribute("src");
    veil.hidden = false;
    veilText.textContent = message;
    $("dot").className = "dot off";
    for (const b of document.querySelectorAll("button")) b.disabled = true;
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
  }

  // ── stream ─────────────────────────────────────────────────────────────

  let retry = 0;
  function connect() {
    if (ended) return;
    frame.src = `stream?t=${Date.now()}`;
  }
  frame.addEventListener("load", () => {
    veil.hidden = true;
    retry = 0;
  });
  frame.addEventListener("error", () => {
    if (ended) return;
    veil.hidden = false;
    veilText.textContent = "Reconnecting…";
    refresh();
    retry = Math.min(retry + 1, 5);
    setTimeout(connect, 600 * retry);
  });

  // ── input ──────────────────────────────────────────────────────────────

  // Inputs go one at a time, in order: a tap must land before the text typed after it.
  let queue = Promise.resolve();
  function send(input) {
    if (!control || ended) return;
    queue = queue.then(() => post(input));
  }

  async function post(input) {
    if (ended) return;
    const aspect = frame.naturalWidth && frame.naturalHeight ? frame.naturalWidth / frame.naturalHeight : null;
    let resp;
    try {
      resp = await fetch("input", {
        method: "POST",
        headers: { "Content-Type": "application/json", "X-Seiji-View": "1" },
        body: JSON.stringify({ input, aspect }),
      });
    } catch {
      flash("Could not reach Seiji.", "error");
      return;
    }
    if (resp.ok) return;
    const err = await errorOf(resp);
    if (resp.status === 401 || resp.status === 410) {
      end(err.message || "The viewer closed.");
    } else {
      flash(err.message || `Input failed (HTTP ${resp.status}).`, resp.status === 409 ? "warning" : "error");
      if (resp.status === 409) refresh();
    }
  }

  // Pointer position as 0-1 of the picture.
  function norm(ev) {
    const r = frame.getBoundingClientRect();
    return {
      x: Math.min(1, Math.max(0, (ev.clientX - r.left) / r.width)),
      y: Math.min(1, Math.max(0, (ev.clientY - r.top) / r.height)),
      px: ev.clientX - r.left,
      py: ev.clientY - r.top,
    };
  }

  function ripple(p) {
    const dot = document.createElement("div");
    dot.className = "touch";
    const r = frame.getBoundingClientRect();
    const s = screen.getBoundingClientRect();
    dot.style.left = `${p.px + r.left - s.left}px`;
    dot.style.top = `${p.py + r.top - s.top}px`;
    screen.appendChild(dot);
    setTimeout(() => dot.remove(), 450);
  }

  let down = null;
  frame.addEventListener("pointerdown", (ev) => {
    if (!control || ev.button !== 0) return;
    screen.focus();
    frame.setPointerCapture(ev.pointerId);
    down = { p: norm(ev), t: performance.now() };
  });
  frame.addEventListener("pointerup", (ev) => {
    if (!down) return;
    const start = down;
    down = null;
    const p = norm(ev);
    const ms = performance.now() - start.t;
    const moved = Math.hypot(p.px - start.p.px, p.py - start.p.py);
    if (moved < 8) {
      ripple(p);
      send({ kind: "tap", x: start.p.x, y: start.p.y, hold: ms >= 400 ? ms / 1000 : null });
    } else {
      ripple(p);
      send({ kind: "swipe", x1: start.p.x, y1: start.p.y, x2: p.x, y2: p.y, duration: Math.min(2, Math.max(0.08, ms / 1000)) });
    }
  });
  frame.addEventListener("pointercancel", () => {
    down = null;
  });

  // Wheel: one swipe per gesture, in the content's direction.
  let wheel = 0;
  let wheelTimer = 0;
  let wheelAt = null;
  frame.addEventListener(
    "wheel",
    (ev) => {
      if (!control) return;
      ev.preventDefault();
      wheel += ev.deltaY;
      wheelAt = wheelAt || norm(ev);
      clearTimeout(wheelTimer);
      wheelTimer = setTimeout(() => {
        const r = frame.getBoundingClientRect();
        const dy = Math.max(-0.45, Math.min(0.45, -wheel / r.height));
        const from = { x: wheelAt.x, y: Math.min(0.9, Math.max(0.1, wheelAt.y)) };
        const to = { x: from.x, y: Math.min(0.95, Math.max(0.05, from.y + dy)) };
        wheel = 0;
        wheelAt = null;
        if (Math.abs(to.y - from.y) > 0.02) send({ kind: "swipe", x1: from.x, y1: from.y, x2: to.x, y2: to.y, duration: 0.25 });
      }, 120);
    },
    { passive: false },
  );

  // Keyboard: printable keys are batched into one `type`; a few keys map to HID codes.
  const KEYS = { Enter: 40, Escape: 41, Backspace: 42, Tab: 43, ArrowRight: 79, ArrowLeft: 80, ArrowDown: 81, ArrowUp: 82 };
  let typed = "";
  let typeTimer = 0;
  function flushTyped() {
    clearTimeout(typeTimer);
    if (typed) send({ kind: "type", text: typed });
    typed = "";
  }
  screen.addEventListener("keydown", (ev) => {
    if (!control) return;
    if (ev.metaKey && ev.shiftKey && ev.key.toLowerCase() === "h") {
      ev.preventDefault();
      flushTyped();
      send({ kind: "button", name: "home" });
    } else if (ev.metaKey && ev.key.toLowerCase() === "l") {
      ev.preventDefault();
      flushTyped();
      send({ kind: "button", name: "lock" });
    } else if (ev.metaKey && ev.key.toLowerCase() === "s") {
      ev.preventDefault();
      save();
    } else if (ev.metaKey || ev.ctrlKey || ev.altKey) {
      // Leave the browser's own shortcuts alone.
    } else if (KEYS[ev.key]) {
      ev.preventDefault();
      flushTyped();
      send({ kind: "key", code: KEYS[ev.key] });
    } else if (ev.key.length === 1) {
      ev.preventDefault();
      typed += ev.key;
      clearTimeout(typeTimer);
      typeTimer = setTimeout(flushTyped, 180);
    }
  });
  screen.addEventListener("paste", (ev) => {
    if (!control) return;
    const text = ev.clipboardData && ev.clipboardData.getData("text/plain");
    if (text) {
      ev.preventDefault();
      flushTyped();
      send({ kind: "type", text });
    }
  });

  // ── toolbar ────────────────────────────────────────────────────────────

  function save() {
    const a = document.createElement("a");
    a.href = "screenshot.jpg";
    a.download = "";
    document.body.appendChild(a);
    a.click();
    a.remove();
  }
  $("home").addEventListener("click", () => send({ kind: "button", name: "home" }));
  $("lock").addEventListener("click", () => send({ kind: "button", name: "lock" }));
  $("shot").addEventListener("click", save);

  refresh().then(connect);
  setInterval(refresh, 5000);
})();
