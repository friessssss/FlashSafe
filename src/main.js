import { invoke } from "@tauri-apps/api/core";
import "./style.css";

const app = document.getElementById("app");

async function refreshWindows() {
  const rows = await invoke("list_windows");
  const sel = document.getElementById("win-select");
  if (!sel) return;
  const v = sel.value;
  sel.innerHTML = '<option value="">Select window…</option>';
  for (const r of rows) {
    const o = document.createElement("option");
    o.value = String(r.hwnd);
    o.textContent = `${r.title} (PID ${r.pid})`;
    sel.appendChild(o);
  }
  sel.value = v;
}

async function refreshStats() {
  try {
    const s = await invoke("get_engine_stats");
    document.getElementById("stat-fps").textContent =
      s.fps != null ? Number(s.fps).toFixed(1) : "—";
    document.getElementById("stat-frames").textContent = String(s.frames ?? 0);
    document.getElementById("stat-flash").textContent = String(s.flashes ?? 0);
    document.getElementById("stat-mit").textContent = `${Math.round((s.mitigation ?? 0) * 100)}%`;
    const err = s.lastError ?? s.last_error ?? "";
    document.getElementById("stat-err").textContent = err;
  } catch (_) {}
}

function applyPipelineToSliders(p) {
  const g = (x, d) => x ?? d;
  document.getElementById("grid").value = String(g(p.gridSize, p.grid_size));
  document.getElementById("spike").value = String(g(p.spikeDeltaThreshold, p.spike_delta_threshold));
  document.getElementById("peak").value = String(g(p.peakClipCellFraction, p.peak_clip_cell_fraction));
  document.getElementById("pat").value = String(g(p.patternSensitivity, p.pattern_sensitivity));
  document.getElementById("maxm").value = String(g(p.maxMitigation, p.max_mitigation));
  document.getElementById("atk").value = String(g(p.attackMs, p.attack_ms));
  document.getElementById("rel").value = String(g(p.releaseMs, p.release_ms));
  document.getElementById("temp").value = String(g(p.temporalBlend, p.temporal_blend));
  document.getElementById("knee").value = String(g(p.highlightKnee, p.highlight_knee));
  document.getElementById("exp").value = String(g(p.exposureScale, p.exposure_scale));
  document.getElementById("desat").value = String(g(p.desaturateOnThreat, p.desaturate_on_threat));
}

function setPresetButtons(active) {
  document.querySelectorAll(".preset-btn").forEach((b) => {
    b.classList.toggle("active", b.dataset.preset === active);
  });
}

async function main() {
  const presets = await invoke("get_sensitivity_presets");
  let cfg = await invoke("load_config");

  app.innerHTML = `
    <header class="hero">
      <h1>FlashSafe</h1>
      <p class="sub">Compositor capture · mirror window · borderless games work best</p>
    </header>
    <section class="card">
      <h2>Setup</h2>
      <p class="hint">Use <strong>borderless windowed</strong> fullscreen. True exclusive mode cannot be captured.</p>
      <label>Target window</label>
      <select id="win-select"></select>
      <button type="button" id="btn-refresh">Refresh list</button>
      <label class="row"><input type="checkbox" id="en" checked /> <span>Apply dimming on the mirror when a flash is detected</span></label>
      <p class="hint subtle">If this is off, you still get capture + “Reactive frames”, but mitigation stays at 0%.</p>
      <label for="delay">Present delay (ms)</label>
      <p class="field-help">Extra wait before showing each mirrored frame. Leave at <strong>0</strong> for best responsiveness; raise only if you want a tiny safety buffer.</p>
      <input type="number" id="delay" min="0" max="50" />
    </section>
    <section class="card">
      <h2>Sensitivity</h2>
      <p class="hint subtle">Mirror is <strong>click-through</strong> — you play on the game window; FlashSafe only draws on top.</p>
      <div class="preset-row" role="group" aria-label="Sensitivity preset">
        <span class="preset-label">Preset</span>
        <div class="preset-btns">
          <button type="button" class="preset-btn" data-preset="low">Low</button>
          <button type="button" class="preset-btn" data-preset="medium">Medium</button>
          <button type="button" class="preset-btn" data-preset="high">High</button>
          <button type="button" class="preset-btn preset-custom" data-preset="custom" title="Shown when you adjust sliders">Custom</button>
        </div>
      </div>
      <p class="preset-hint">Low = fewer false dimming triggers · High = catches subtler flashes</p>

      <div class="field">
        <label for="grid">Grid (downsample)</label>
        <p class="field-help">How many cells FlashSafe uses to measure brightness (grid × grid). Higher = finer spatial detail for detection, slightly more work. Stats run on a small GPU downscale (320×180), not your full resolution.</p>
        <input type="range" id="grid" min="4" max="64" step="1" />
      </div>
      <div class="field">
        <label for="spike">Spike delta</label>
        <p class="field-help">Minimum jump in average brightness between frames to count as a spike. <strong>Lower</strong> = react to smaller flashes (more sensitive).</p>
        <input type="range" id="spike" min="0.02" max="0.5" step="0.01" />
      </div>
      <div class="field">
        <label for="peak">Peak clip fraction</label>
        <p class="field-help">How much of the grid must be near-white to flag a “whiteout” flash. <strong>Lower</strong> = trigger on smaller bright regions.</p>
        <input type="range" id="peak" min="0.05" max="1" step="0.01" />
      </div>
      <div class="field">
        <label for="pat">Pattern mix</label>
        <p class="field-help">How much a repeating-brightness pattern (roughly 3–30 Hz on the global signal) adds to the threat score. Higher = weight patterned flicker more.</p>
        <input type="range" id="pat" min="0" max="1" step="0.01" />
      </div>
      <div class="field">
        <label for="maxm">Max mitigation</label>
        <p class="field-help">Ceiling for how strongly dimming is applied when threat is maxed (0 = none, 1 = strongest blend toward the toned-down image).</p>
        <input type="range" id="maxm" min="0" max="1" step="0.01" />
      </div>
      <div class="field">
        <label for="atk">Attack (ms)</label>
        <p class="field-help">How quickly dimming ramps <strong>up</strong> when a flash is detected. Lower = snappier darkening.</p>
        <input type="range" id="atk" min="1" max="200" step="1" />
      </div>
      <div class="field">
        <label for="rel">Release (ms)</label>
        <p class="field-help">How quickly dimming fades <strong>down</strong> after the threat passes. Higher = stays dimmed longer (less flicker).</p>
        <input type="range" id="rel" min="20" max="800" step="5" />
      </div>
      <div class="field">
        <label for="temp">Temporal blend</label>
        <p class="field-help">Blends the current mitigated frame with the previous one. Higher = smoother / less single-frame pop, but more motion smear.</p>
        <input type="range" id="temp" min="0" max="0.95" step="0.01" />
      </div>
      <div class="field">
        <label for="knee">Highlight knee</label>
        <p class="field-help">Rolls off very bright highlights before they hit the screen. <strong>Lower</strong> = compress bright highlights more aggressively when mitigating.</p>
        <input type="range" id="knee" min="0.5" max="0.99" step="0.01" />
      </div>
      <div class="field">
        <label for="exp">Exposure scale</label>
        <p class="field-help">Multiplies linear RGB while mitigating. <strong>Lower</strong> = darker image during dimming (stronger effect).</p>
        <input type="range" id="exp" min="0.2" max="1" step="0.01" />
      </div>
      <div class="field">
        <label for="desat">Desaturate</label>
        <p class="field-help">How much color is pulled toward grey during mitigation. Higher can make harsh flashes feel less intense.</p>
        <input type="range" id="desat" min="0" max="1" step="0.01" />
      </div>
    </section>
    <section class="card">
      <h2>Status</h2>
      <p>FPS: <span id="stat-fps">—</span> · Frames: <span id="stat-frames">0</span> · Reactive frames: <span id="stat-flash">0</span> · Mitigation: <span id="stat-mit">0%</span></p>
      <p class="err" id="stat-err"></p>
      <button type="button" id="btn-start">Start / apply</button>
      <button type="button" id="btn-stop" class="secondary">Stop</button>
      <button type="button" id="btn-save" class="secondary">Save settings</button>
    </section>
  `;

  const applyUIFromConfig = () => {
    document.getElementById("en").checked = !!cfg.enabled;
    document.getElementById("delay").value = String(cfg.presentDelayMs ?? cfg.present_delay_ms ?? 0);
    const p = cfg.pipeline || {};
    document.getElementById("grid").value = String(p.gridSize ?? p.grid_size ?? 16);
    document.getElementById("spike").value = String(p.spikeDeltaThreshold ?? p.spike_delta_threshold ?? 0.12);
    document.getElementById("peak").value = String(p.peakClipCellFraction ?? p.peak_clip_cell_fraction ?? 0.35);
    document.getElementById("pat").value = String(p.patternSensitivity ?? p.pattern_sensitivity ?? 0.4);
    document.getElementById("maxm").value = String(p.maxMitigation ?? p.max_mitigation ?? 0.9);
    document.getElementById("atk").value = String(p.attackMs ?? p.attack_ms ?? 8);
    document.getElementById("rel").value = String(p.releaseMs ?? p.release_ms ?? 200);
    document.getElementById("temp").value = String(p.temporalBlend ?? p.temporal_blend ?? 0.28);
    document.getElementById("knee").value = String(p.highlightKnee ?? p.highlight_knee ?? 0.74);
    document.getElementById("exp").value = String(p.exposureScale ?? p.exposure_scale ?? 0.34);
    document.getElementById("desat").value = String(p.desaturateOnThreat ?? p.desaturate_on_threat ?? 0.38);
  };

  applyUIFromConfig();
  const sp = (cfg.sensitivityPreset ?? cfg.sensitivity_preset ?? "medium").toLowerCase();
  if (sp === "low" || sp === "medium" || sp === "high") {
    setPresetButtons(sp);
  } else {
    setPresetButtons("custom");
  }
  await refreshWindows();
  const th = cfg.targetHwnd ?? cfg.target_hwnd;
  if (th) {
    document.getElementById("win-select").value = String(th);
  }

  document.getElementById("btn-refresh").onclick = refreshWindows;

  const onSliderChange = () => {
    setPresetButtons("custom");
    cfg = { ...cfg, sensitivityPreset: "custom" };
  };
  for (const id of [
    "grid",
    "spike",
    "peak",
    "pat",
    "maxm",
    "atk",
    "rel",
    "temp",
    "knee",
    "exp",
    "desat",
  ]) {
    document.getElementById(id)?.addEventListener("input", onSliderChange);
  }

  document.querySelectorAll(".preset-btn[data-preset]").forEach((btn) => {
    btn.addEventListener("click", () => {
      const name = btn.dataset.preset;
      if (name === "custom") return;
      if (!presets[name]) return;
      applyPipelineToSliders(presets[name]);
      setPresetButtons(name);
      cfg = { ...cfg, sensitivityPreset: name };
    });
  });

  const readCfg = () => ({
    enabled: document.getElementById("en").checked,
    targetHwnd: parseInt(document.getElementById("win-select").value, 10) || 0,
    targetTitle:
      document.getElementById("win-select").selectedOptions[0]?.text?.split(" (PID")[0] || "",
    monitorAllScreens: false,
    presentDelayMs: parseInt(document.getElementById("delay").value, 10) || 0,
    sensitivityPreset: document.querySelector(".preset-btn.active")?.dataset.preset || "custom",
    pipeline: {
      gridSize: parseInt(document.getElementById("grid").value, 10),
      spikeDeltaThreshold: parseFloat(document.getElementById("spike").value),
      peakClipCellFraction: parseFloat(document.getElementById("peak").value),
      patternSensitivity: parseFloat(document.getElementById("pat").value),
      maxMitigation: parseFloat(document.getElementById("maxm").value),
      attackMs: parseFloat(document.getElementById("atk").value),
      releaseMs: parseFloat(document.getElementById("rel").value),
      temporalBlend: parseFloat(document.getElementById("temp").value),
      highlightKnee: parseFloat(document.getElementById("knee").value),
      exposureScale: parseFloat(document.getElementById("exp").value),
      desaturateOnThreat: parseFloat(document.getElementById("desat").value),
    },
  });

  document.getElementById("btn-save").onclick = async () => {
    cfg = readCfg();
    await invoke("save_config", { cfg });
  };

  document.getElementById("btn-start").onclick = async () => {
    document.getElementById("en").checked = true;
    cfg = readCfg();
    await invoke("save_config", { cfg });
    const hwnd = cfg.targetHwnd ?? cfg.target_hwnd;
    if (!hwnd) {
      alert("Select a window first.");
      return;
    }
    await invoke("start_engine", { hwnd, cfg });
    await invoke("update_engine_config", { cfg });
  };

  document.getElementById("btn-stop").onclick = async () => {
    await invoke("stop_engine");
  };

  setInterval(refreshStats, 400);
}

main().catch(console.error);
