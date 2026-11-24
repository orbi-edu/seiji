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
    stopRtc();
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

  // `false` when the socket is not open: the message was not sent.
  function send(msg) {
    if (!ws || ws.readyState !== WebSocket.OPEN) return false;
    ws.send(JSON.stringify(msg));
    return true;
  }

  // Input goes on the WebRTC input channel once it is open, else on the socket. A gesture keeps
  // the path its `down` took, so its points never overtake each other on two paths.
  let gesturePath = null;
  function sendInput(msg) {
    let path = dc && dc.readyState === "open" ? "rtc" : "ws";
    if (msg.t === "touch") {
      if (msg.phase === "down") gesturePath = path;
      else if (gesturePath) path = gesturePath;
      if (msg.phase === "up") gesturePath = null;
    }
    if (path === "rtc" && dc && dc.readyState === "open") {
      dc.send(JSON.stringify(msg));
      return true;
    }
    return send(msg);
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
      // A new socket starts a new sample: the old one's round trips say nothing about it.
      rtts.length = 0;
      lastPong = performance.now();
      ping();
      startRtc();
    };
    ws.onmessage = (ev) => (typeof ev.data === "string" ? onText(JSON.parse(ev.data)) : onFrame(ev.data));
    ws.onclose = (ev) => {
      ws = null;
      resetVideo();
      // The session's WebRTC end closed with it; the next socket tries again.
      stopRtc();
      rtcTried = false;
      if (ended) return;
      // An input sent before the drop may or may not have reached the device: say so, never replay.
      if (pending.size) {
        flash(`The connection dropped: ${pending.size === 1 ? "an input" : `${pending.size} inputs`} may not have been applied.`, "error");
        pending.clear();
      }
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

  // Over the input channel when it is open: the round trip of the path input takes.
  function ping() {
    const msg = { t: "ping", at: performance.now() };
    if (dc && dc.readyState === "open") dc.send(JSON.stringify(msg));
    else send(msg);
  }
  setInterval(ping, 1000);

  // ── link quality ───────────────────────────────────────────────────────

  // The last round trips, for the jitter: how much one differs from the one before.
  const rtts = [];
  let lastPong = 0;
  let poorSince = 0;
  let poorNotedAt = -Infinity;
  const linkEl = $("link");

  function jitter() {
    if (rtts.length < 2) return 0;
    let sum = 0;
    for (let i = 1; i < rtts.length; i++) sum += Math.abs(rtts[i] - rtts[i - 1]);
    return sum / (rtts.length - 1);
  }

  function onPong(sample) {
    rtts.push(sample);
    if (rtts.length > 10) rtts.shift();
    lastPong = performance.now();
  }

  function renderLink() {
    if (ended) {
      linkEl.hidden = true;
      return;
    }
    const now = performance.now();
    let text;
    let quality;
    if (!ws || ws.readyState !== WebSocket.OPEN) {
      text = "offline";
      quality = "poor";
    } else if (lastPong && now - lastPong > 3000) {
      // Nothing came back for a while: the link is stalled, whatever the last round trip was.
      text = `no answer ${Math.round((now - lastPong) / 1000)} s`;
      quality = "poor";
    } else if (rtt !== null) {
      const j = jitter();
      text = `${Math.round(rtt)} ms ±${Math.round(j)}`;
      quality = rtt < 150 && j < 30 ? "good" : rtt < 350 && j < 100 ? "fair" : "poor";
    } else {
      linkEl.hidden = true;
      return;
    }
    linkEl.textContent = text;
    linkEl.className = `link ${quality}`;
    linkEl.title =
      "Round trip to the Seiji host, and how much it varies (jitter). " +
      "Over a poor link the picture and your input lag; Smooth needs less bandwidth.";
    linkEl.hidden = false;
    // Say so once a minute at most, after the link stayed poor for a few seconds.
    if (quality !== "poor") {
      poorSince = 0;
    } else if (!poorSince) {
      poorSince = now;
    } else if (now - poorSince > 3000 && now - poorNotedAt > 60000) {
      poorNotedAt = now;
      flash("The link to the device is slow or unstable: the picture and your input lag. Smooth needs less bandwidth.", "warning");
    }
  }
  setInterval(renderLink, 500);

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
        onPong(rtt);
        // The server's clock at the middle of the round trip.
        clockOffset = msg.server_ms - (Date.now() - rtt / 2);
        break;
      }
      case "preset":
        flash(`Preset: ${msg.preset}`);
        break;
      case "rtc_answer":
        onRtcAnswer(msg);
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
        if (msg.code === "rtc_unavailable") {
          // No WebRTC on this host or no port free: the WebSocket stream goes on, nothing to say.
          stopRtc();
          break;
        }
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
  let decodeErrors = [];
  const stats = { frames: [], bytes: [], age: null, drawn: 0 };
  // Frames sent to the decoder and not yet shown: capture time → [frame number, when sent]. A
  // frame is acknowledged once it is shown, so a slow decoder holds the stream back like a slow
  // link.
  const decoding = new Map();
  // At most this many frames wait in the decoder; past it, frames are dropped up to a keyframe.
  const MAX_DECODE_QUEUE = 3;

  function base64Bytes(s) {
    const bin = atob(s);
    const out = new Uint8Array(bin.length);
    for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
    return out;
  }

  // Acknowledges every frame still in the decoder (it was reset, or stalled) and starts over at a
  // keyframe.
  function flushDecoding() {
    let last = null;
    for (const [seq] of decoding.values()) last = seq;
    decoding.clear();
    if (last !== null) send({ t: "ack", seq: last });
    waitingKey = true;
    send({ t: "keyframe" });
  }

  function resetVideo() {
    decoding.clear();
    waitingKey = true;
  }

  function newDecoder() {
    return new VideoDecoder({
      output: (frame) => {
        const entry = decoding.get(frame.timestamp);
        if (entry !== undefined) {
          decoding.delete(frame.timestamp);
          send({ t: "ack", seq: entry[0] });
        }
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
      error: (e) => {
        decoder = null;
        // A browser that cannot decode this stream fails the same way every time: stop there.
        const now = Date.now();
        decodeErrors = decodeErrors.filter((t) => t > now - 10000).concat(now);
        if (e && e.name === "NotSupportedError") {
          end(`This browser cannot decode the stream (${format ? format.codec : "H.264"}).`);
        } else if (decodeErrors.length > 3) {
          end("The stream could not be decoded. Open the device again.");
        } else if (format) {
          configure(format);
          flushDecoding();
        }
      },
    });
  }

  async function configure(f) {
    format = f;
    const config = {
      codec: f.codec,
      codedWidth: f.width,
      codedHeight: f.height,
      description: base64Bytes(f.description),
      optimizeForLatency: true,
    };
    waitingKey = true;
    try {
      const support = await VideoDecoder.isConfigSupported(config);
      if (!support.supported) {
        end(`This browser cannot decode the stream (${f.codec}, ${f.width}×${f.height}).`);
        return;
      }
    } catch {
      end(`This browser cannot decode the stream (${f.codec}).`);
      return;
    }
    if (format !== f || ended) return;
    if (!decoder || decoder.state === "closed") decoder = newDecoder();
    decoder.configure(config);
    decoding.clear();
  }

  function onFrame(buf) {
    const view = new DataView(buf);
    const key = (view.getUint8(0) & 1) === 1;
    const seq = view.getUint32(1);
    const capturedUs = Number(view.getBigInt64(5));
    stats.bytes.push([Date.now(), buf.byteLength]);
    const ready = decoder && decoder.state === "configured";
    if (ready && !key && decoder.decodeQueueSize >= MAX_DECODE_QUEUE) {
      // The decoder is behind: drop up to the next keyframe rather than fall further behind.
      waitingKey = true;
      send({ t: "keyframe" });
    }
    if (!ready || (waitingKey && !key)) {
      send({ t: "ack", seq });
      return;
    }
    waitingKey = false;
    decoding.set(capturedUs, [seq, performance.now()]);
    decoder.decode(new EncodedVideoChunk({ type: key ? "key" : "delta", timestamp: capturedUs, data: new Uint8Array(buf, 13) }));
  }

  // A decoder that holds a frame for a second has stalled: let the stream go on.
  setInterval(() => {
    const oldest = decoding.values().next().value;
    if (oldest && performance.now() - oldest[1] > 1000) flushDecoding();
  }, 500);

  // ── WebRTC ─────────────────────────────────────────────────────────────

  // With `info.webrtc`, the page also offers a WebRTC session over the socket: the stream as
  // H.264 over UDP into a <video> laid over the canvas, and input on the `input` data channel.
  // A lost packet then holds back only its own frame, not the input or the frames after it.
  // The socket keeps the replies, and the video goes back to it when WebRTC fails or never
  // connects.
  const video = $("rtc");
  let pc = null;
  let dc = null;
  let rtcOn = false; // the page shows the WebRTC video
  let rtcTried = false; // one try per socket
  let rtcTimer = 0;
  let rtcBytes = null;

  function rtcUsable() {
    return !!(info && info.webrtc && "RTCPeerConnection" in window && "requestVideoFrameCallback" in HTMLVideoElement.prototype);
  }

  async function startRtc() {
    if (pc || rtcTried || ended || !rtcUsable()) return;
    rtcTried = true;
    const peer = new RTCPeerConnection({ bundlePolicy: "max-bundle" });
    pc = peer;
    const tr = peer.addTransceiver("video", { direction: "recvonly" });
    // Show each frame as soon as it is decoded: no jitter buffer beyond what decoding needs.
    if ("jitterBufferTarget" in tr.receiver) tr.receiver.jitterBufferTarget = 0;
    if ("playoutDelayHint" in tr.receiver) tr.receiver.playoutDelayHint = 0;
    dc = peer.createDataChannel("input", { ordered: true });
    dc.onmessage = (ev) => {
      try {
        onText(JSON.parse(ev.data));
      } catch { /* not ours */ }
    };
    peer.ontrack = (ev) => {
      video.srcObject = new MediaStream([ev.track]);
      placeVideo();
      video.play().catch(() => {});
      video.requestVideoFrameCallback(onRtcFrame);
    };
    peer.onconnectionstatechange = () => {
      if (pc === peer && ["failed", "closed"].includes(peer.connectionState)) {
        stopRtc("The WebRTC link failed: the video is back on the WebSocket.");
      }
    };
    try {
      await peer.setLocalDescription(await peer.createOffer());
    } catch {
      stopRtc();
      return;
    }
    if (pc !== peer || !send({ t: "rtc_offer", sdp: peer.localDescription.sdp })) {
      stopRtc();
      return;
    }
    // A page that cannot reach the host over UDP stays on the WebSocket.
    rtcTimer = setTimeout(() => {
      if (!rtcOn) stopRtc();
    }, 10000);
  }

  async function onRtcAnswer(msg) {
    if (!pc) return;
    try {
      await pc.setRemoteDescription({ type: "answer", sdp: msg.sdp });
    } catch {
      stopRtc();
    }
  }

  function stopRtc(why) {
    clearTimeout(rtcTimer);
    if (pc) {
      pc.ontrack = null;
      pc.onconnectionstatechange = null;
      pc.close();
    }
    pc = null;
    dc = null;
    rtcBytes = null;
    video.srcObject = null;
    video.classList.remove("on");
    if (rtcOn) {
      rtcOn = false;
      send({ t: "video_via", via: "ws" });
      if (why) flash(why, "warning");
    }
  }

  // The video covers the canvas exactly; the canvas below keeps the pointer.
  function placeVideo() {
    const r = canvas.getBoundingClientRect();
    const s = screen.getBoundingClientRect();
    video.style.left = `${r.left - s.left}px`;
    video.style.top = `${r.top - s.top}px`;
    video.style.width = `${r.width}px`;
    video.style.height = `${r.height}px`;
  }
  window.addEventListener("resize", () => {
    if (rtcOn) placeVideo();
  });

  function onRtcFrame(_now, meta) {
    if (!pc) return;
    // The canvas keeps the picture's size, so the layout and the pointer map stay right.
    if (canvas.width !== meta.width || canvas.height !== meta.height) {
      canvas.width = meta.width;
      canvas.height = meta.height;
    }
    if (!rtcOn) {
      rtcOn = true;
      clearTimeout(rtcTimer);
      send({ t: "video_via", via: "rtc" });
      video.classList.add("on");
      veil.hidden = true;
      resetVideo();
    }
    placeVideo();
    stats.frames.push(Date.now());
    // The RTP time is the capture time at 90 kHz, modulo 2^32: the frame's age on the server's
    // clock.
    if (meta.rtpTimestamp !== undefined) {
      const wrap = 2 ** 32 / 90;
      const age = (((Date.now() + clockOffset - meta.rtpTimestamp / 90) % wrap) + wrap) % wrap;
      if (age < 10000) stats.age = age;
    }
    video.requestVideoFrameCallback(onRtcFrame);
  }

  // The received bytes, for the bitrate in Stats.
  setInterval(async () => {
    if (!pc || !rtcOn) return;
    let bytes = 0;
    (await pc.getStats()).forEach((r) => {
      if (r.type === "inbound-rtp" && r.kind === "video") bytes += r.bytesReceived;
    });
    if (rtcBytes !== null) stats.bytes.push([Date.now(), bytes - rtcBytes]);
    rtcBytes = bytes;
  }, 1000);

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
      `Transport: ${rtcOn ? "WebRTC (UDP)" : "WebSocket"}`,
      `Round trip: ${rtt === null ? "—" : `${Math.round(rtt)} ms`}`,
      `Jitter: ${rtts.length < 2 ? "—" : `${Math.round(jitter())} ms`}`,
      `Frame age: ${stats.age === null ? "—" : `${Math.round(stats.age)} ms`}`,
      `Decode queue: ${rtcOn ? "—" : decoder ? decoder.decodeQueueSize : 0}`,
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
  // Where a 0-1 point of the picture is, in CSS pixels of the screen element.
  function toScreen(p) {
    const r = canvas.getBoundingClientRect();
    const s = screen.getBoundingClientRect();
    return { x: r.left - s.left + p.x * r.width, y: r.top - s.top + p.y * r.height };
  }
  function showFinger(p) {
    if (!p) {
      fingerDot.hidden = true;
      return;
    }
    const q = toScreen(p);
    fingerDot.style.left = `${q.x}px`;
    fingerDot.style.top = `${q.y}px`;
    fingerDot.hidden = false;
  }

  // The finger's path and a ripple for each tap, drawn at once: input feels immediate though
  // the device's answer takes a round trip.
  const trail = $("trail");
  const t2d = trail.getContext("2d");
  const TRAIL_MS = 400;
  const RIPPLE_MS = 350;
  const accent = getComputedStyle(document.documentElement).getPropertyValue("--accent").trim() || "#2563eb";
  const points = []; // {x, y, t, start}: screen CSS pixels; `start` begins a new stroke
  const ripples = [];
  let drawing = false;
  function trailAdd(p, start) {
    points.push({ ...toScreen(p), t: performance.now(), start });
    kickTrail();
  }
  function ripple(p) {
    ripples.push({ ...toScreen(p), t: performance.now() });
    kickTrail();
  }
  function kickTrail() {
    if (drawing) return;
    drawing = true;
    requestAnimationFrame(drawTrail);
  }
  function drawTrail() {
    const s = screen.getBoundingClientRect();
    const dpr = window.devicePixelRatio || 1;
    const w = Math.round(s.width * dpr);
    const h = Math.round(s.height * dpr);
    if (trail.width !== w || trail.height !== h) {
      trail.width = w;
      trail.height = h;
    }
    t2d.setTransform(dpr, 0, 0, dpr, 0, 0);
    t2d.clearRect(0, 0, s.width, s.height);
    const now = performance.now();
    while (points.length && now - points[0].t > TRAIL_MS) points.shift();
    while (ripples.length && now - ripples[0].t > RIPPLE_MS) ripples.shift();
    t2d.strokeStyle = accent;
    t2d.lineCap = "round";
    t2d.lineWidth = 6;
    for (let i = 1; i < points.length; i++) {
      const a = points[i - 1];
      const b = points[i];
      if (b.start) continue;
      t2d.globalAlpha = 0.55 * (1 - (now - b.t) / TRAIL_MS);
      t2d.beginPath();
      t2d.moveTo(a.x, a.y);
      t2d.lineTo(b.x, b.y);
      t2d.stroke();
    }
    t2d.lineWidth = 2;
    for (const r of ripples) {
      const k = (now - r.t) / RIPPLE_MS;
      t2d.globalAlpha = 0.7 * (1 - k);
      t2d.beginPath();
      t2d.arc(r.x, r.y, 12 + 18 * k, 0, 2 * Math.PI);
      t2d.stroke();
    }
    t2d.globalAlpha = 1;
    if (points.length || ripples.length) requestAnimationFrame(drawTrail);
    else drawing = false;
  }

  // Each touch carries when the page saw it (`ts`), so the server can hand a gesture that the
  // link delayed or bunched to the device with its own spacing.
  function touch(phase, p, ts, edge) {
    sendInput({ t: "touch", phase, x: p.x, y: p.y, edge, ts });
  }

  // Moves are sent at most once per animation frame, always the latest point.
  let moveAt = null;
  let moveTs = 0;
  let moveScheduled = false;
  function queueMove(p, ts) {
    moveAt = p;
    moveTs = ts;
    if (moveScheduled) return;
    moveScheduled = true;
    requestAnimationFrame(flushMove);
  }
  function flushMove() {
    moveScheduled = false;
    if (moveAt && (finger || wheel.active)) touch("move", moveAt, moveTs);
    moveAt = null;
  }

  let finger = null;
  let downAt = null;
  canvas.addEventListener("pointerdown", (ev) => {
    if (!control || ev.button !== 0 || finger) return;
    ev.preventDefault();
    screen.focus();
    canvas.setPointerCapture(ev.pointerId);
    endWheel();
    const p = norm(ev);
    finger = ev.pointerId;
    downAt = p;
    touch("down", p, ev.timeStamp, edgeOf(p));
    showFinger(p);
    trailAdd(p, true);
  });
  canvas.addEventListener("pointermove", (ev) => {
    if (ev.pointerId !== finger) return;
    const p = norm(ev);
    queueMove(p, ev.timeStamp);
    showFinger(p);
    trailAdd(p, false);
  });
  function lift(ev) {
    if (ev.pointerId !== finger) return;
    finger = null;
    flushMove();
    const p = norm(ev);
    touch("up", p, ev.timeStamp);
    showFinger(null);
    // A tap, not a drag: show where it landed.
    if (downAt && Math.hypot(p.x - downAt.x, p.y - downAt.y) < 0.01) ripple(p);
    downAt = null;
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
    touch("up", wheel.at, performance.now());
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
        touch("down", wheel.at, ev.timeStamp);
        trailAdd(wheel.at, true);
      }
      const scale = ev.deltaMode === 1 ? 16 : ev.deltaMode === 2 ? r.height : 1;
      const x = wheel.at.x - (ev.deltaX * scale) / r.width;
      const y = wheel.at.y - (ev.deltaY * scale) / r.height;
      if (x < 0.02 || x > 0.98 || y < 0.05 || y > 0.95) {
        // Out of room: lift and start the next drag where this one began.
        endWheel();
      } else {
        wheel.at = { x, y };
        queueMove(wheel.at, ev.timeStamp);
        trailAdd(wheel.at, false);
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
  // Sends a text input and remembers it for its answer; says so when it could not be sent.
  function request(kind, t) {
    const id = nextId++;
    if (!sendInput({ t: kind, id, text: t })) {
      flash("Not connected: the text was not sent.", "error");
      return false;
    }
    pending.set(id, kind);
    return true;
  }
  function paste(t) {
    request("paste", t);
  }
  function key(usage, mods) {
    flushText();
    sendInput({ t: "key", usage, mods: mods || [] });
  }

  screen.addEventListener("keydown", (ev) => {
    if (!control) return;
    const k = ev.key.toLowerCase();
    if (ev.metaKey && ev.shiftKey && k === "h") {
      ev.preventDefault();
      flushText();
      sendInput({ t: "button", name: "home" });
    } else if (ev.metaKey && k === "l") {
      ev.preventDefault();
      flushText();
      sendInput({ t: "button", name: "lock" });
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
    if (t && request("paste_and_type", t)) {
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
  $("home").addEventListener("click", () => sendInput({ t: "button", name: "home" }));
  $("lock").addEventListener("click", () => sendInput({ t: "button", name: "lock" }));
  $("shot").addEventListener("click", save);

  refresh().then(connect);
  setInterval(refresh, 5000);
})();
