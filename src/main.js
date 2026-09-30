import { invoke } from "@tauri-apps/api/core";
import "./style.css";

const app = document.getElementById("app");

// Advanced filter controls: [id, config key, label, help, min, max, step].
const ADVANCED = [
  ["rise", "risePerSec", "Brightening speed",
    "How fast the picture may get brighter (fraction of full brightness per second). Lower = flashes are ramped in more slowly.",
    0.05, 3, 0.05],
  ["hold-rise", "holdRisePerSec", "Brightening speed during strobes",
    "The brightening limit used while a strobe or flicker is detected. Keep this low.",
    0.02, 1, 0.01],
  ["fall", "fallPerSec", "Darkening speed",
    "How fast the picture may get darker. Lower = sudden cuts to dark become gentler fades.",
    0.2, 10, 0.1],
  ["trigger", "strobeTrigger", "Strobe trigger",
    "How many quick back-and-forth brightness changes it takes to count as strobing. Lower = strobe mode engages sooner.",
    1, 6, 0.1],
  ["hold", "holdSecs", "Strobe hold (s)",
    "How long strobe mode stays on after the flicker stops.",
    0, 6, 0.1],
  ["floor", "minGain", "Darkest allowed",
    "The picture is never dimmed below this fraction, so it never goes fully black.",
    0, 0.06, 0.005],
  ["radius", "regionRadius", "Area size",
    "Size of the screen area considered together, in tiles. Larger = small bright objects are left alone more.",
    1, 8, 1],
  ["burst", "burstSecs", "Small-change allowance (s)",
    "Lets small, sudden changes (a bright object moving) through without dimming.",
    0, 0.1, 0.005],
];

function el(id) {
  return document.getElementById(id);
}

async function refreshWindows() {
  const rows = await invoke("list_windows");
  const sel = el("win-select");
  const v = sel.value;
  sel.innerHTML = '<option value="">Select the game window…</option>';
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
    el("stat-state").textContent = s.running ? "Protecting" : "Off";
    el("stat-state").classList.toggle("on", !!s.running);
    el("stat-fps").textContent = s.running ? Number(s.fps ?? 0).toFixed(0) : "—";
    el("stat-flash").textContent = String(s.flashes ?? 0);
    el("stat-mit").textContent = `${Math.round((s.mitigation ?? 0) * 100)}%`;
    el("stat-hold").textContent = (s.hold ?? 0) > 0 ? "strobe detected" : "";
    el("stat-err").textContent = s.lastError ?? "";
  } catch (_) {}
}

function setPresetButtons(active) {
  document.querySelectorAll(".preset-btn").forEach((b) => {
    b.classList.toggle("active", b.dataset.preset === active);
  });
}

function showFilter(f) {
  for (const [id, key] of ADVANCED) {
    el(id).value = String(f[key]);
    el(`${id}-val`).textContent = String(f[key]);
  }
}

// Sliders override `base`, so settings without a slider are kept.
function readFilter(base) {
  const f = { ...base };
  for (const [id, key] of ADVANCED) {
    f[key] = key === "regionRadius" ? parseInt(el(id).value, 10) : parseFloat(el(id).value);
  }
  return f;
}

async function main() {
  const presets = await invoke("get_sensitivity_presets");
  let cfg = await invoke("load_config");

  app.innerHTML = `
    <header class="hero">
      <h1>FlashSafe</h1>
      <p class="sub">Softens flashes and strobes in games</p>
    </header>
    <section class="card">
      <h2>Game</h2>
      <p class="hint">Set the game to <strong>borderless windowed</strong> or windowed mode. Exclusive fullscreen can't be captured.</p>
      <select id="win-select"></select>
      <button type="button" id="btn-refresh" class="secondary">Refresh list</button>
    </section>
    <section class="card">
      <h2>Protection strength</h2>
      <div class="preset-row" role="group" aria-label="Protection strength">
        <div class="preset-btns">
          <button type="button" class="preset-btn" data-preset="low">Low</button>
          <button type="button" class="preset-btn" data-preset="medium">Medium</button>
          <button type="button" class="preset-btn" data-preset="high">High</button>
          <button type="button" class="preset-btn preset-custom" data-preset="custom" title="Shown when you adjust advanced settings">Custom</button>
        </div>
      </div>
      <p class="preset-hint">Low = least dimming of normal scenes · High = strongest protection</p>
      <details class="advanced">
        <summary>Advanced settings</summary>
        ${ADVANCED.map(([id, , label, help, min, max, step]) => `
          <div class="field">
            <label for="${id}">${label} <span class="val" id="${id}-val"></span></label>
            <p class="field-help">${help}</p>
            <input type="range" id="${id}" min="${min}" max="${max}" step="${step}" />
          </div>`).join("")}
        <label class="row"><input type="checkbox" id="en" /> <span>Apply the filter (turn off only to compare with the unfiltered picture)</span></label>
      </details>
    </section>
    <section class="card">
      <h2>Status</h2>
      <p><strong id="stat-state">Off</strong> <span class="hint subtle" id="stat-hold"></span></p>
      <p>Flashes softened: <span id="stat-flash">0</span> · Dimming now: <span id="stat-mit">0%</span> · FPS: <span id="stat-fps">—</span></p>
      <p class="err" id="stat-err"></p>
      <button type="button" id="btn-start">Start protection</button>
      <button type="button" id="btn-stop" class="secondary">Stop</button>
    </section>
    <p class="disclaimer">FlashSafe reduces flashing but cannot guarantee every flash is caught. It is not a medical device. If you feel unwell, stop playing.</p>
  `;

  el("en").checked = cfg.enabled !== false;
  showFilter(cfg.filter);
  setPresetButtons(cfg.sensitivityPreset ?? "medium");
  await refreshWindows();
  if (cfg.targetHwnd) {
    el("win-select").value = String(cfg.targetHwnd);
  }

  const readCfg = () => ({
    ...cfg,
    enabled: el("en").checked,
    targetHwnd: parseInt(el("win-select").value, 10) || 0,
    targetTitle: el("win-select").selectedOptions[0]?.text?.split(" (PID")[0] || "",
    sensitivityPreset: document.querySelector(".preset-btn.active")?.dataset.preset || "custom",
    filter: readFilter(cfg.filter),
  });

  // Push every change to the running engine and persist it.
  const apply = async () => {
    cfg = readCfg();
    await invoke("save_config", { cfg });
    await invoke("update_engine_config", { cfg });
  };

  el("btn-refresh").onclick = refreshWindows;
  el("en").addEventListener("change", apply);

  for (const [id] of ADVANCED) {
    el(id).addEventListener("input", () => {
      el(`${id}-val`).textContent = el(id).value;
      setPresetButtons("custom");
      apply();
    });
  }

  document.querySelectorAll(".preset-btn[data-preset]").forEach((btn) => {
    btn.addEventListener("click", () => {
      const name = btn.dataset.preset;
      if (name === "custom" || !presets[name]) return;
      cfg = { ...cfg, filter: presets[name] };
      showFilter(presets[name]);
      setPresetButtons(name);
      apply();
    });
  });

  el("btn-start").onclick = async () => {
    cfg = readCfg();
    if (!cfg.targetHwnd) {
      el("stat-err").textContent = "Select the game window first.";
      return;
    }
    await invoke("save_config", { cfg });
    try {
      await invoke("start_engine", { hwnd: cfg.targetHwnd, cfg });
    } catch (e) {
      el("stat-err").textContent = String(e);
    }
  };

  el("btn-stop").onclick = async () => {
    await invoke("stop_engine");
  };

  setInterval(refreshStats, 400);
}

main().catch(console.error);
