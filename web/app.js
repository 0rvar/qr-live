"use strict";

(() => {
  const BODY_STEPS = 24;
  const LOG_ROWS = 12;
  const SRC_LIMIT = 20000;
  const HEX_LIMIT = 2048;
  const RIPPLE_MS = 1600;
  const PNG_PAN_PX_PER_S = 90;
  const MONO = getComputedStyle(document.documentElement).getPropertyValue("--mono").trim() || "monospace";
  const TAU = Math.PI * 2;

  const $ = (id) => document.getElementById(id);
  const el = {
    stage: $("stage"),
    canvas: $("ring"),
    center: $("center"),
    code: $("code"),
    codeText: $("code-text"),
    codeImg: $("code-img"),
    stepN: $("step-n"),
    stepTotal: $("step-total"),
    lap: $("lap"),
    lapWrap: $("lap-wrap"),
    lang: $("lang"),
    files: $("files"),
    phase: $("phase"),
    elapsed: $("elapsed"),
    size: $("size"),
    note: $("note"),
    conn: $("conn"),
    mode: $("mode"),
    log: $("log"),
    tSteps: $("t-steps"),
    totalsScope: $("totals-scope"),
    announce: $("announce"),
    tDone: $("t-done"),
    tLive: $("t-live"),
    tReplay: $("t-replay"),
    tFail: $("t-fail"),
    tLast: $("t-last"),
  };
  const ctx = el.canvas.getContext("2d");
  const base = document.createElement("canvas");
  const bctx = base.getContext("2d");
  const codeNode = document.createTextNode("");
  el.codeText.appendChild(codeNode);

  const reducedMQ = matchMedia("(prefers-reduced-motion: reduce)");
  let reduced = reducedMQ.matches;
  reducedMQ.addEventListener("change", (e) => {
    reduced = e.matches;
    ripple = 0;
  });

  // ---- relay state -------------------------------------------------------

  let steps = [];
  let n = 0;
  let mode = "";
  let lap = 0;
  let results = []; // latest StepDone per ring index, each with its absolute key
  let visited = new Uint8Array(0);
  let visitedLap = 0;
  let cur = null; // { index, key, phase, startedAt }
  let clockOffset = 0; // server clock minus local clock
  let connected = false;
  let avgStepMs = 1500;

  // Server-wide totals: taken from snapshot.totals, then advanced by events.
  // Without snapshot.totals they are counted from history and events instead.
  const totals = { live: 0, replay: 0, fail: 0, lapsDone: 0, lastLap: null, fromServer: false };
  let lastKey = -1; // newest step_done already counted and logged
  let runId = null;
  let cacheGen = 0; // bumped whenever the source cache is invalidated
  const log = []; // newest last, at most LOG_ROWS entries

  const keyOf = (l, i) => (l - 1) * n + i;
  const mod = (k) => ((k % n) + n) % n;
  const kind = (r) => (!r.ok ? "fail" : r.live ? "live" : "replay");

  // ---- geometry and colours ---------------------------------------------

  let W = 0, H = 0, dpr = 1, cx = 0, cy = 0, R = 100, half = 100;
  let font = 0, labelR = 0, labelsOn = false, hr = 4, nodeR = 1.5;
  let codeCapacity = 4000;
  let colors = {};
  let baseDirty = true;

  // head animation along the ring, in absolute step keys
  let headPos = 0;
  let anim = null; // { from, to, t0, dur }
  let ripple = 0; // performance.now() of the last lap flash, 0 when none
  let rippleFailed = false; // the flashing lap had failed steps

  const ang = (p) => -Math.PI / 2 + (p / n) * TAU;

  function readColors() {
    const cs = getComputedStyle(document.documentElement);
    const v = (name) => cs.getPropertyValue(name).trim();
    colors = {
      fg: v("--fg"), dim: v("--ring-dim"), faint: v("--faint"), line: v("--line"),
      head: v("--head"), live: v("--live"), replay: v("--replay"), fail: v("--fail"), build: v("--build"),
    };
  }

  function layout() {
    const rect = el.stage.getBoundingClientRect();
    W = Math.max(1, rect.width);
    H = Math.max(1, rect.height);
    dpr = window.devicePixelRatio || 1;
    el.canvas.width = base.width = Math.round(W * dpr);
    el.canvas.height = base.height = Math.round(H * dpr);
    cx = W / 2;
    cy = H / 2;
    half = Math.min(W, H) / 2;
    const margin = Math.max(6, half * 0.035);
    const count = n || 128;

    // Labels sit radially outside the ring. Solve for the ring radius so the
    // longest name just fits: R + font * (gap + longest) = half - margin,
    // with the font size proportional to the node spacing.
    let longest = 10;
    if (steps.length) {
      bctx.font = `100px ${MONO}`;
      longest = 0;
      for (const s of steps) longest = Math.max(longest, bctx.measureText(s.name).width / 100);
    }
    const fill = 0.84;
    const gap = 1.1;
    R = (half - margin) / (1 + ((fill * TAU) / count) * (gap + longest));
    font = (fill * TAU * R) / count;
    labelsOn = font >= 6.5;
    if (!labelsOn) {
      R = half - margin - 4;
      font = 0;
    }
    const spacing = (TAU * R) / count;
    hr = Math.min(24, Math.max(3, spacing * 0.42));
    nodeR = Math.max(1, spacing * 0.13);
    labelR = R + hr * 1.5 + font * 0.3;

    const rc = R - hr * 3.2;
    Object.assign(el.center.style, {
      left: `${cx - rc}px`,
      top: `${cy - rc}px`,
      width: `${2 * rc}px`,
      height: `${2 * rc}px`,
    });
    el.stage.style.setProperty("--r", `${R}px`);

    const codeFont = Math.max(7, R * 0.03);
    const cols = (2 * rc) / (codeFont * 0.6);
    const rows = (2 * rc) / (codeFont * 1.22);
    codeCapacity = Math.ceil(cols * rows * 1.05);

    readColors();
    baseDirty = true;
    fitLang();
    if (png) sizePng();
  }

  // ---- static layer: ring, nodes, labels ---------------------------------

  function drawBase() {
    baseDirty = false;
    const b = bctx;
    b.setTransform(1, 0, 0, 1, 0, 0);
    b.clearRect(0, 0, base.width, base.height);
    if (!n) return;
    b.setTransform(dpr, 0, 0, dpr, 0, 0);

    b.globalAlpha = 1;
    b.strokeStyle = colors.line;
    b.lineWidth = 1;
    b.beginPath();
    b.arc(cx, cy, R, 0, TAU);
    b.stroke();

    // origin mark at ruby: where the head meets the tail
    const a0 = ang(0);
    b.strokeStyle = colors.dim;
    b.beginPath();
    b.moveTo(cx + Math.cos(a0) * (R - hr * 2.6), cy + Math.sin(a0) * (R - hr * 2.6));
    b.lineTo(cx + Math.cos(a0) * (R - hr * 1.4), cy + Math.sin(a0) * (R - hr * 1.4));
    b.stroke();

    for (let i = 0; i < n; i++) {
      const a = ang(i);
      const x = cx + Math.cos(a) * R;
      const y = cy + Math.sin(a) * R;
      const r = results[i];
      b.beginPath();
      if (visited[i] && r) {
        b.globalAlpha = 0.6;
        b.fillStyle = colors[kind(r)];
        b.arc(x, y, nodeR * 1.6, 0, TAU);
      } else {
        b.globalAlpha = 0.9;
        b.fillStyle = colors.faint;
        b.arc(x, y, nodeR, 0, TAU);
      }
      b.fill();
    }

    if (labelsOn) {
      b.font = `${font}px ${MONO}`;
      b.textBaseline = "middle";
      for (let i = 0; i < n; i++) {
        b.globalAlpha = visited[i] ? 0.72 : 0.5;
        b.fillStyle = visited[i] ? colors.fg : colors.dim;
        drawLabel(b, i);
      }
    }
    b.globalAlpha = 1;
  }

  function drawLabel(c, i) {
    const a = ang(i);
    c.save();
    c.translate(cx, cy);
    if (Math.cos(a) >= -1e-9) {
      c.rotate(a);
      c.textAlign = "left";
      c.fillText(steps[i].name, labelR, 0);
    } else {
      c.rotate(a + Math.PI);
      c.textAlign = "right";
      c.fillText(steps[i].name, -labelR, 0);
    }
    c.restore();
  }

  // ---- per-frame drawing --------------------------------------------------

  const easeInOut = (t) => (t < 0.5 ? 4 * t * t * t : 1 - Math.pow(-2 * t + 2, 3) / 2);
  const easeOut = (t) => 1 - Math.pow(1 - t, 3);

  function arcStroke(c, p0, p1, width, color, alpha) {
    // Skip sub-pixel arcs: canvas sweeps the long way round when the end angle
    // precedes the start, and Chrome renders near-zero sweeps on the far side
    // of the circle.
    if ((ang(p1) - ang(p0)) * R < 0.5) return;
    c.globalAlpha = alpha;
    c.strokeStyle = color;
    c.lineWidth = width;
    c.beginPath();
    c.arc(cx, cy, R, ang(p0), ang(p1));
    c.stroke();
  }

  function drawBody(now) {
    if (!cur) return;
    const headKey = cur.key;
    let oldest = headKey;
    for (let j = 1; j <= BODY_STEPS + 1; j++) {
      const k = headKey - j;
      const r = results[mod(k)];
      if (!r || r.key !== k) break;
      oldest = k;
    }
    if (oldest === headKey) return;
    const tail = Math.max(headPos - BODY_STEPS, oldest);
    const wMax = hr * 1.5;
    const PIECES = 6;
    ctx.lineCap = "butt";
    for (let k = headKey - 1; k >= oldest; k--) {
      const r = results[mod(k)];
      const color = colors[kind(r)];
      const s0 = Math.max(k, tail);
      const s1 = Math.min(k + 1, headPos);
      if (s1 - s0 < 1e-3) continue;
      const step = (s1 - s0) / PIECES;
      for (let p = 0; p < PIECES; p++) {
        const p0 = s0 + p * step;
        const p1 = p0 + step;
        const d = Math.min(1, Math.max(0, (headPos - (p0 + p1) / 2) / BODY_STEPS));
        const w = wMax * Math.pow(1 - d, 0.8) + 0.8;
        const alpha = 0.9 * Math.pow(1 - d, 1.4) + 0.06;
        // overlap pieces slightly to hide anti-aliasing seams
        arcStroke(ctx, p0, Math.min(s1, p1 + step * 0.15), w, color, alpha);
      }
    }
    ctx.globalAlpha = 1;
  }

  function drawHead(now) {
    if (!cur) return;
    const a = ang(headPos);
    const x = cx + Math.cos(a) * R;
    const y = cy + Math.sin(a) * R;
    const building = cur.phase === "build";
    const color = !connected ? colors.dim : building ? colors.build : colors.head;
    const t = reduced ? 0 : now / 1000;

    if (connected) {
      if (building) {
        const breath = reduced ? 0.7 : 0.55 + 0.35 * Math.sin(t * TAU * 0.6);
        ctx.globalAlpha = breath;
        ctx.strokeStyle = colors.build;
        ctx.lineWidth = Math.max(1, hr * 0.28);
        ctx.setLineDash([hr * 0.7, hr * 0.55]);
        ctx.lineDashOffset = -t * hr * 3;
        ctx.beginPath();
        ctx.arc(x, y, hr * 2.4, 0, TAU);
        ctx.stroke();
        ctx.setLineDash([]);
      } else {
        const s = reduced ? 0.5 : 0.5 + 0.5 * Math.sin(t * TAU * 1.4);
        ctx.globalAlpha = 0.14 + 0.14 * s;
        ctx.fillStyle = colors.head;
        ctx.beginPath();
        ctx.arc(x, y, hr * (1.9 + 0.7 * s), 0, TAU);
        ctx.fill();
      }
    }

    ctx.globalAlpha = 1;
    ctx.fillStyle = color;
    ctx.beginPath();
    ctx.ellipse(x, y, hr * 1.35, hr, a + Math.PI / 2, 0, TAU);
    ctx.fill();
    // eye, facing the direction of travel
    const ea = a + 0.6 * (hr / R);
    ctx.fillStyle = "#0a0c0f";
    ctx.beginPath();
    ctx.arc(cx + Math.cos(ea) * (R - hr * 0.35), cy + Math.sin(ea) * (R - hr * 0.35), Math.max(0.8, hr * 0.22), 0, TAU);
    ctx.fill();

    if (labelsOn) {
      ctx.font = `${font}px ${MONO}`;
      ctx.textBaseline = "middle";
      ctx.fillStyle = color;
      drawLabel(ctx, cur.index);
    }
  }

  function drawRipple(now) {
    if (!ripple) return;
    const t = (now - ripple) / RIPPLE_MS;
    if (t >= 1 || reduced) {
      ripple = 0;
      return;
    }
    // a lap with failed steps flashes dimmer, in the failure colour
    const e = (1 - t) * (rippleFailed ? 0.6 : 1);
    const g = easeOut(t);
    const color = rippleFailed ? colors.fail : colors.head;
    ctx.strokeStyle = color;
    ctx.globalAlpha = 0.5 * e;
    ctx.lineWidth = hr * 2 * e + 0.5;
    ctx.beginPath();
    ctx.arc(cx, cy, R, 0, TAU);
    ctx.stroke();
    ctx.lineWidth = 1.5;
    ctx.globalAlpha = 0.45 * e;
    ctx.beginPath();
    ctx.arc(cx, cy, R + (half - R) * g, 0, TAU);
    ctx.stroke();
    ctx.globalAlpha = 0.3 * e;
    ctx.beginPath();
    ctx.arc(cx, cy, R * (1 - 0.45 * g), 0, TAU);
    ctx.stroke();
    ctx.fillStyle = color;
    ctx.globalAlpha = 0.8 * e;
    for (let i = 0; i < n; i++) {
      const a = ang(i + g * 0.5);
      ctx.beginPath();
      ctx.arc(cx + Math.cos(a) * R, cy + Math.sin(a) * R, nodeR * 1.8, 0, TAU);
      ctx.fill();
    }
    ctx.globalAlpha = 1;
  }

  function frame(now) {
    requestAnimationFrame(frame);
    if ((window.devicePixelRatio || 1) !== dpr) layout();
    if (!n) return;
    if (baseDirty) drawBase();

    if (anim) {
      const p = (now - anim.t0) / anim.dur;
      if (p >= 1 || reduced) {
        headPos = anim.to;
        anim = null;
      } else {
        headPos = anim.from + (anim.to - anim.from) * easeInOut(p);
      }
    }

    ctx.setTransform(1, 0, 0, 1, 0, 0);
    ctx.globalAlpha = 1;
    ctx.clearRect(0, 0, el.canvas.width, el.canvas.height);
    ctx.drawImage(base, 0, 0);
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    drawRipple(now);
    drawBody(now);
    drawHead(now);

    tickClock();
    tickTypewriter();
    tickPng(now);
  }

  // ---- centre readout -----------------------------------------------------

  let lastElapsed = "";
  function tickClock() {
    let s = "–";
    if (cur && cur.startedAt) s = fmtElapsed(Date.now() + clockOffset - cur.startedAt);
    if (s !== lastElapsed) {
      lastElapsed = s;
      el.elapsed.textContent = s;
    }
  }

  function fitLang() {
    const name = el.lang.textContent || "";
    const len = Math.max(4, name.length);
    const px = Math.min(R * 0.15, (R * 1.35) / (len * 0.6));
    el.lang.style.fontSize = `${Math.max(14, px)}px`;
  }

  function renderNow() {
    el.stepTotal.textContent = n ? String(n) : "–";
    el.lap.textContent = lap ? String(lap) : "–";
    if (!cur || !n) {
      el.lang.textContent = n ? "idle" : "connecting";
      el.files.textContent = " ";
      el.stepN.textContent = "–";
      el.size.textContent = "–";
      setPhase("idle");
      fitLang();
      return;
    }
    const st = steps[cur.index];
    el.stepN.textContent = String(cur.index + 1);
    el.lang.textContent = st.name;
    el.files.textContent = `${st.input} → ${st.output}`;
    el.size.textContent = fmtBytes(st.size);
    setPhase(cur.phase);
    fitLang();
  }

  function setPhase(p) {
    el.phase.dataset.phase = p;
    el.phase.textContent = p;
  }

  // ---- source panel -------------------------------------------------------

  let srcCtl = null;
  let tw = { text: "", shown: 0 };
  let png = null; // { w, h, t0 }
  let srcIndex = -1; // step whose source is shown or loading; -1 when none

  // Program sources never change while a server runs, so each step's source
  // is fetched at most once per run. One entry per ring index:
  // { kind: "text" | "hex", text, note } or { kind: "png", url, w, h, note }.
  // Only one PNG blob URL is held at a time.
  const srcCache = new Map();
  let pngCacheIndex = -1;

  function clearSourceCache() {
    for (const e of srcCache.values()) if (e.url) URL.revokeObjectURL(e.url);
    srcCache.clear();
    pngCacheIndex = -1;
  }

  function cachePut(index, entry) {
    if (entry.kind === "png") {
      if (pngCacheIndex >= 0 && pngCacheIndex !== index) {
        const old = srcCache.get(pngCacheIndex);
        if (old && old.url) URL.revokeObjectURL(old.url);
        srcCache.delete(pngCacheIndex);
      }
      pngCacheIndex = index;
    }
    srcCache.set(index, entry);
  }

  function clearSource() {
    if (srcCtl) srcCtl.abort();
    srcCtl = null;
    srcIndex = -1;
    tw = { text: "", shown: 0 };
    codeNode.data = "";
    png = null;
    el.codeImg.style.backgroundImage = "";
    el.code.className = "";
    el.note.textContent = "";
  }

  function showSource(entry) {
    el.note.textContent = entry.note || "";
    if (entry.kind === "png") {
      png = { w: entry.w, h: entry.h, t0: performance.now(), boxW: 0, drawW: 0 };
      el.codeImg.style.backgroundImage = `url("${entry.url}")`;
      el.code.className = "png";
      sizePng();
      return;
    }
    el.code.className = entry.kind === "hex" ? "hex" : "";
    startTypewriter(entry.text);
  }

  function loadSource(index) {
    clearSource();
    const st = steps[index];
    if (!st) return;
    const cached = srcCache.get(index);
    if (cached) {
      srcIndex = index;
      showSource(cached);
      return;
    }
    // A hidden tab fetches nothing; visibilitychange loads the current step.
    if (document.hidden) return;
    srcIndex = index;
    const ctl = new AbortController();
    srcCtl = ctl;
    const isPng = st.binary && /\.png$/i.test(st.input);
    const url = isPng ? `/api/source/${index}` : `/api/source/${index}?limit=${st.binary ? HEX_LIMIT : SRC_LIMIT}`;
    const gen = cacheGen;
    fetch(url, { signal: ctl.signal })
      .then((res) => {
        if (!res.ok) throw new Error(`HTTP ${res.status}`);
        return isPng ? res.blob() : res.arrayBuffer();
      })
      .then((body) => (isPng ? decodePng(body) : decodeBytes(st, body)))
      .then((entry) => {
        if (gen !== cacheGen) {
          if (entry.url) URL.revokeObjectURL(entry.url);
          return;
        }
        cachePut(index, entry);
        if (!ctl.signal.aborted && srcIndex === index) showSource(entry);
      })
      .catch((err) => {
        if (ctl.signal.aborted || err.name === "AbortError") return;
        el.note.textContent = `source unavailable (${err.message})`;
      });
  }

  function decodeBytes(st, body) {
    const bytes = new Uint8Array(body);
    if (st.binary) {
      return { kind: "hex", text: hexdump(bytes), note: `binary · first ${fmtBytes(bytes.length)} as hex` };
    }
    return { kind: "text", text: prepText(new TextDecoder("utf-8").decode(bytes.subarray(0, SRC_LIMIT))) };
  }

  function decodePng(blob) {
    return new Promise((resolve, reject) => {
      const url = URL.createObjectURL(blob);
      const img = new Image();
      img.onload = () => {
        resolve({ kind: "png", url, w: img.naturalWidth, h: img.naturalHeight,
          note: `image/png · ${img.naturalWidth} × ${img.naturalHeight} px` });
      };
      img.onerror = () => {
        URL.revokeObjectURL(url);
        reject(new Error("bad image"));
      };
      img.src = url;
    });
  }

  document.addEventListener("visibilitychange", () => {
    if (document.hidden || !cur || cur.phase === "idle") return;
    if (srcIndex !== cur.index) loadSource(cur.index);
  });

  function sizePng() {
    const box = el.code.getBoundingClientRect();
    const scale = Math.max(1, Math.floor((box.height * 0.34) / png.h));
    const h = png.h * scale;
    el.codeImg.style.height = `${h}px`;
    png.boxW = box.width;
    png.drawW = png.w * scale;
  }

  function tickPng(now) {
    if (!png || !png.boxW) return;
    const travel = Math.max(0, png.drawW - png.boxW);
    let x = 0;
    if (!reduced && travel > 0) {
      const d = (((now - png.t0) / 1000) * PNG_PAN_PX_PER_S) % (2 * travel);
      x = d <= travel ? d : 2 * travel - d;
    }
    el.codeImg.style.backgroundPosition = `${-Math.round(x)}px 0`;
  }

  function startTypewriter(text) {
    tw = { text, shown: 0 };
    codeNode.data = "";
  }

  function tickTypewriter() {
    const limit = Math.min(tw.text.length, codeCapacity);
    if (tw.shown >= limit) return;
    const rate = reduced ? limit : Math.max(40, Math.ceil(codeCapacity / 50));
    tw.shown = Math.min(limit, tw.shown + rate);
    codeNode.data = tw.text.slice(0, tw.shown);
  }

  function prepText(s) {
    s = s.replace(/\r\n?/g, "\n").replace(/[\x00-\x08\x0b-\x1f\x7f]/g, "·");
    const sample = s.slice(0, 4000);
    const visible = sample.replace(/\s/g, "").length;
    if (sample.length > 200 && visible / sample.length < 0.05) {
      // whitespace-only languages: make spaces and tabs visible
      s = s.replace(/ /g, "·").replace(/\t/g, "→");
    }
    return s;
  }

  function hexdump(bytes) {
    const lines = [];
    for (let off = 0; off < bytes.length; off += 16) {
      let hex = "";
      let asc = "";
      for (let i = 0; i < 16; i++) {
        const b = bytes[off + i];
        if (off + i < bytes.length) {
          hex += b.toString(16).padStart(2, "0") + " ";
          asc += b >= 0x20 && b < 0x7f ? String.fromCharCode(b) : ".";
        } else {
          hex += "   ";
        }
        if (i === 7) hex += " ";
      }
      lines.push(`${off.toString(16).padStart(8, "0")}  ${hex} |${asc}|`);
    }
    return lines.join("\n");
  }

  // ---- side panel ---------------------------------------------------------

  const logRows = [];
  for (let i = 0; i < LOG_ROWS; i++) {
    const li = document.createElement("li");
    el.log.appendChild(li);
    logRows.push(li);
  }

  function cell(cls, text) {
    const s = document.createElement("span");
    s.className = cls;
    s.textContent = text;
    return s;
  }

  const GLYPH = { live: "●", replay: "○", fail: "✕" };

  function renderLog(fresh) {
    for (let i = 0; i < LOG_ROWS; i++) {
      const li = logRows[i];
      const r = log[log.length - 1 - i];
      li.replaceChildren();
      li.className = "";
      li.removeAttribute("title");
      if (!r) continue;
      const k = kind(r);
      const skipped = r.ok && !r.live && r.skipped ? String(r.skipped) : "";
      li.className = skipped ? `${k} skipped` : k;
      li.append(
        cell("g", GLYPH[k]),
        cell("n", r.name),
        cell("d", fmtDuration(r.duration_ms)),
        cell("o", fmtBytes(r.output_bytes)),
        cell("h", !r.ok ? r.error || "failed" : skipped ? "skipped" : (r.output_sha1 || "").slice(0, 7)),
      );
      li.title = !r.ok
        ? `lap ${r.lap} · ${r.error || "failed"} · got ${r.output_sha1 || "-"} expected ${r.expected_sha1}`
        : skipped
          ? `lap ${r.lap} · replayed, skipped: ${skipped} · sha1 ${r.output_sha1}`
          : `lap ${r.lap} · ${r.live ? "live" : "replayed"} · sha1 ${r.output_sha1}`;
    }
    if (fresh && !reduced && logRows[0].animate) {
      logRows[0].animate([{ opacity: 0.25 }, { opacity: 1 }], { duration: 450, easing: "ease-out" });
    }
  }

  function renderTotals() {
    el.tSteps.textContent = String(totals.live + totals.replay + totals.fail);
    el.totalsScope.textContent = totals.fromServer ? "since server start" : "since page load";
    el.tDone.textContent = String(totals.lapsDone);
    el.tLive.textContent = String(totals.live);
    el.tReplay.textContent = String(totals.replay);
    el.tFail.textContent = String(totals.fail);
    const L = totals.lastLap;
    el.tLast.textContent = L ? fmtDuration(L.duration_ms) : "–";
    el.tLast.title = L ? `lap ${L.lap}: ${L.live_steps} live, ${L.failed_steps} failed` : "";
    el.mode.textContent = mode || "–";
    el.mode.dataset.mode = mode;
  }

  // Logs a completed step once; counts it too unless the totals for it
  // already came from the server.
  function recordDone(r, count) {
    if (r.key <= lastKey) return false;
    lastKey = r.key;
    if (count) totals[kind(r)]++;
    log.push(r);
    if (log.length > LOG_ROWS) log.splice(0, log.length - LOG_ROWS);
    return true;
  }

  function resetRun() {
    Object.assign(totals, { live: 0, replay: 0, fail: 0, lapsDone: 0, lastLap: null, fromServer: false });
    lastKey = -1;
    log.length = 0;
    cacheGen++;
    clearSource();
    clearSourceCache();
  }

  // ---- event handling -----------------------------------------------------

  function moveHead(key) {
    const now = performance.now();
    const delta = key - headPos;
    if (reduced || delta <= 0 || delta > 4) {
      headPos = key;
      anim = null;
      return;
    }
    const dur = Math.min(700, Math.max(140, avgStepMs * 0.45));
    anim = { from: headPos, to: key, t0: now, dur };
  }

  function onSnapshot(d) {
    steps = Array.isArray(d.steps) ? d.steps : [];
    const resized = steps.length !== n;
    n = steps.length;
    mode = d.mode || "";
    lap = d.lap || 1;
    clockOffset = (d.server_time_ms || Date.now()) - Date.now();
    results = new Array(n).fill(null);
    visited = new Uint8Array(n);
    visitedLap = lap;

    const history = Array.isArray(d.history) ? d.history.slice(-256) : [];
    let newest = -1;
    for (const h of history) {
      const r = Object.assign({}, h, { key: keyOf(h.lap, h.index) });
      results[h.index] = r;
      if (h.lap === lap) visited[h.index] = 1;
      newest = Math.max(newest, r.key);
    }

    if (d.current && steps[d.current.index]) {
      const i = d.current.index;
      cur = { index: i, key: keyOf(lap, i), phase: d.current.phase || "run", startedAt: d.current.started_at_ms || 0 };
    } else {
      const next = newest + 1;
      cur = n ? { index: mod(next), key: Math.max(0, next), phase: "idle", startedAt: 0 } : null;
    }

    // A new server process (new run_id) starts afresh. Without a run_id, a
    // relay position that went backwards is taken as a restart.
    const rid = typeof d.run_id === "string" ? d.run_id : null;
    const restarted = rid !== null ? runId !== null && rid !== runId : !!cur && cur.key <= lastKey;
    if (restarted || resized) resetRun();
    runId = rid;

    const st = d.totals && typeof d.totals === "object" ? d.totals : null;
    if (st) {
      Object.assign(totals, {
        live: st.steps_live | 0,
        replay: st.steps_replayed | 0,
        fail: st.steps_failed | 0,
        lapsDone: st.laps_done | 0,
        lastLap: st.last_lap || null,
        fromServer: true,
      });
    }
    for (const h of history) recordDone(Object.assign({}, h, { key: keyOf(h.lap, h.index) }), !st);

    headPos = cur ? cur.key : 0;
    anim = null;
    ripple = 0;
    if (resized) layout();
    baseDirty = true;
    renderNow();
    renderLog(false);
    renderTotals();
    if (cur && cur.phase !== "idle") {
      loadSource(cur.index);
      announce();
    } else {
      clearSource();
    }
  }

  function announce() {
    if (!cur || !n) return;
    el.announce.textContent = `${steps[cur.index].name}, step ${cur.index + 1} of ${n}`;
  }

  function onStepStart(d) {
    if (!n || !steps[d.index]) return;
    const key = keyOf(d.lap, d.index);
    if (d.lap !== visitedLap) {
      visited.fill(0);
      visitedLap = d.lap;
      baseDirty = true;
    }
    lap = d.lap;
    cur = { index: d.index, key, phase: steps[d.index].has_build ? "build" : "run", startedAt: d.at_ms };
    moveHead(key);
    renderNow();
    renderTotals();
    loadSource(d.index);
    announce();
  }

  function onPhase(d) {
    if (!cur || cur.key !== keyOf(d.lap, d.index)) return;
    cur.phase = d.phase;
    setPhase(d.phase);
  }

  function onStepDone(d) {
    if (!n || !steps[d.index]) return;
    const r = Object.assign({}, d, { key: keyOf(d.lap, d.index) });
    results[d.index] = r;
    if (d.lap === visitedLap) visited[d.index] = 1;
    baseDirty = true;
    if (typeof d.duration_ms === "number" && d.duration_ms > 0) {
      avgStepMs = avgStepMs * 0.8 + d.duration_ms * 0.2;
    }
    if (recordDone(r, true)) {
      renderLog(true);
      renderTotals();
    }
  }

  function onLapDone(d) {
    totals.lapsDone++;
    totals.lastLap = d;
    renderTotals();
    if (!reduced) {
      ripple = performance.now();
      rippleFailed = (d.failed_steps | 0) > 0;
      if (el.lapWrap.animate) {
        el.lapWrap.animate(
          [{ color: rippleFailed ? colors.fail : colors.head, transform: "scale(1.25)" }, { transform: "scale(1)" }],
          { duration: 900, easing: "ease-out" },
        );
      }
    }
  }

  // ---- connection ---------------------------------------------------------

  let es = null;
  let retryTimer = 0;
  let everConnected = false;

  function setConnected(ok) {
    connected = ok;
    el.stage.classList.toggle("offline", !ok);
    el.conn.hidden = ok;
    el.conn.textContent = ok ? "" : everConnected ? "reconnecting…" : "connecting…";
  }

  function handler(fn) {
    return (ev) => {
      let data;
      try {
        data = JSON.parse(ev.data);
      } catch {
        return;
      }
      fn(data);
    };
  }

  function connect() {
    retryTimer = 0;
    if (es) es.close();
    es = new EventSource("/api/events");
    es.addEventListener("snapshot", handler((d) => {
      everConnected = true;
      setConnected(true);
      onSnapshot(d);
    }));
    es.addEventListener("step_start", handler(onStepStart));
    es.addEventListener("phase", handler(onPhase));
    es.addEventListener("step_done", handler(onStepDone));
    es.addEventListener("lap_done", handler(onLapDone));
    es.addEventListener("error", () => {
      setConnected(false);
      // EventSource retries network errors itself; a closed source needs a new one.
      if (es.readyState === EventSource.CLOSED && !retryTimer) {
        retryTimer = setTimeout(connect, 3000);
      }
    });
  }

  // ---- formatting ---------------------------------------------------------

  function fmtBytes(b) {
    if (typeof b !== "number" || !isFinite(b)) return "–";
    if (b < 1024) return `${b} B`;
    if (b < 1024 * 1024) return `${(b / 1024).toFixed(1)} KiB`;
    return `${(b / 1024 / 1024).toFixed(2)} MiB`;
  }

  function fmtDuration(ms) {
    if (typeof ms !== "number" || !isFinite(ms)) return "–";
    if (ms < 1000) return `${Math.round(ms)} ms`;
    if (ms < 60000) return `${(ms / 1000).toFixed(2)} s`;
    const s = Math.round(ms / 1000);
    if (s < 3600) return `${Math.floor(s / 60)}m ${String(s % 60).padStart(2, "0")}s`;
    return `${Math.floor(s / 3600)}h ${String(Math.floor(s / 60) % 60).padStart(2, "0")}m`;
  }

  function fmtElapsed(ms) {
    if (ms < 0) ms = 0;
    if (ms < 60000) return `${(ms / 1000).toFixed(1)} s`;
    return fmtDuration(ms);
  }

  // ---- boot ---------------------------------------------------------------

  new ResizeObserver(layout).observe(el.stage);
  layout();
  setConnected(false);
  renderNow();
  renderLog(false);
  renderTotals();
  connect();
  requestAnimationFrame(frame);
})();
