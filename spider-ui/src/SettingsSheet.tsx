/** Settings, as a sheet over the app.
 *
 *  The scan thresholds are laid out as the Chartink scan itself rather than as
 *  thirteen labelled fields, because that is the form the rules were written in
 *  and the one they are reasoned about in. A field called "depth ratio" means
 *  nothing on its own; the same number sitting inside
 *  `Min(20, Close) < Max(60, Close) × 0.85` explains itself. */

import { useEffect, useRef, useState } from "react";
import {
  defaultSettings,
  getSettings,
  saveSettings,
  type CupParams,
  type Settings,
} from "./api";

/** A number input that tolerates half-typed values.
 *
 *  Parsing on every keystroke and writing back makes a field impossible to
 *  clear — delete the last digit of `30` and a naive control snaps it to `0`
 *  and puts the cursor somewhere else. So the text is local while focused and
 *  only well-formed numbers are committed upward. */
function Num({
  value,
  onChange,
  step = 1,
  min,
  max,
  width = 62,
}: {
  value: number;
  onChange: (n: number) => void;
  step?: number;
  min?: number;
  max?: number;
  width?: number;
}) {
  const [raw, setRaw] = useState(String(value));
  const focused = useRef(false);

  useEffect(() => {
    if (!focused.current) setRaw(String(value));
  }, [value]);

  return (
    <input
      className="num-field num"
      type="number"
      step={step}
      min={min}
      max={max}
      style={{ width }}
      value={raw}
      onFocus={() => (focused.current = true)}
      onBlur={() => {
        focused.current = false;
        const n = Number(raw);
        if (raw.trim() === "" || Number.isNaN(n)) {
          setRaw(String(value));
          return;
        }
        // Show what was actually committed. Clamping quietly while leaving the
        // typed text in place meant a field could read 999 in a 0–1 setting and
        // have saved 1 — the form disagreeing with the file it just wrote.
        const committed = clamp(n, min, max);
        setRaw(String(committed));
        onChange(committed);
      }}
      onChange={(e) => {
        setRaw(e.target.value);
        const n = Number(e.target.value);
        if (e.target.value.trim() !== "" && !Number.isNaN(n)) onChange(clamp(n, min, max));
      }}
    />
  );
}

function clamp(n: number, min?: number, max?: number) {
  if (min !== undefined && n < min) return min;
  if (max !== undefined && n > max) return max;
  return n;
}

function Check({
  checked,
  onChange,
  label,
  hint,
}: {
  checked: boolean;
  onChange: (b: boolean) => void;
  label: string;
  hint?: string;
}) {
  return (
    <label className="check">
      <input type="checkbox" checked={checked} onChange={(e) => onChange(e.target.checked)} />
      <span>
        {label}
        {hint && <i>{hint}</i>}
      </span>
    </label>
  );
}

export default function SettingsSheet({
  onClose,
  onSaved,
}: {
  onClose: () => void;
  onSaved: (message: string) => void;
}) {
  const [draft, setDraft] = useState<Settings | null>(null);
  const [saved, setSaved] = useState<Settings | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    getSettings()
      .then((s) => {
        setDraft(s);
        setSaved(s);
      })
      .catch(() => {});
  }, []);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.stopPropagation();
        onClose();
      }
    };
    // Capture phase: the app's own arrow-key handler is on `window` too, and a
    // sheet that lets keystrokes through to the list behind it feels broken.
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, [onClose]);

  if (!draft) return null;

  const set = (patch: Partial<Settings>) => setDraft({ ...draft, ...patch });
  const setCup = (patch: Partial<CupParams>) =>
    setDraft({ ...draft, patterns: { chartink: { ...draft.patterns.chartink, ...patch } } });

  const cup = draft.patterns.chartink;
  const dirty = JSON.stringify(draft) !== JSON.stringify(saved);
  // Only the rules and the score floor change what a rescan would produce;
  // download and universe settings do nothing until the next sync, so offering
  // a rescan for them would just burn thirty seconds.
  const scanChanged =
    !!saved &&
    (JSON.stringify(draft.patterns) !== JSON.stringify(saved.patterns) ||
      draft.min_pattern_score !== saved.min_pattern_score);

  const save = async (rescan: boolean) => {
    setBusy(true);
    try {
      await saveSettings(draft, rescan);
      setSaved(draft);
      onSaved(rescan ? "Settings saved — rescanning" : "Settings saved to data/settings.json");
      if (!rescan) onClose();
      else onClose();
    } catch (e) {
      onSaved(`Could not save settings: ${e}`);
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="sheet-backdrop" onMouseDown={onClose}>
      <div className="sheet" onMouseDown={(e) => e.stopPropagation()}>
        <header className="sheet-head">
          <h2>Settings</h2>
          <span className="path">data/settings.json</span>
          <div className="spacer" />
          <button className="ghost" onClick={onClose} title="Close (Esc)">
            ✕
          </button>
        </header>

        <div className="sheet-body">
          <section>
            <h3>The scan</h3>
            <p className="note">
              Your Chartink cup-and-handle breakout, condition for condition. Edit any number in
              place — these are the only rules the app runs.
            </p>

            <div className="rules">
              <div className="rule">
                <span>Close crossed above Max(</span>
                <Num value={cup.breakout_lookback} min={2} max={400} onChange={(n) => setCup({ breakout_lookback: n })} />
                <span>, 1 day ago Close)</span>
              </div>
              <div className="rule">
                <span>Min(</span>
                <Num value={cup.depth_window} min={2} max={400} onChange={(n) => setCup({ depth_window: n })} />
                <span>, Close) &lt; Max(</span>
                <Num value={cup.base_window} min={2} max={400} onChange={(n) => setCup({ base_window: n })} />
                <span>, Close) ×</span>
                <Num value={cup.depth_ratio} step={0.01} min={0.1} max={1} onChange={(n) => setCup({ depth_ratio: n })} />
              </div>
              <div className="rule">
                <span>Close &gt; Sma(Close,</span>
                <Num value={cup.trend_sma} min={2} max={400} onChange={(n) => setCup({ trend_sma: n })} />
                <span>)</span>
              </div>
              <div className="rule">
                <span>Sma(Volume,</span>
                <Num value={cup.volume_fast_sma} min={1} max={400} onChange={(n) => setCup({ volume_fast_sma: n })} />
                <span>) &gt; Sma(Volume,</span>
                <Num value={cup.volume_slow_sma} min={1} max={400} onChange={(n) => setCup({ volume_slow_sma: n })} />
                <span>)</span>
              </div>
              <div className="rule">
                <span>Rsi(</span>
                <Num value={cup.rsi_period} min={2} max={200} onChange={(n) => setCup({ rsi_period: n })} />
                <span>) &gt;</span>
                <Num value={cup.rsi_min} step={0.5} min={0} max={100} onChange={(n) => setCup({ rsi_min: n })} />
              </div>
              <div className="rule">
                <span>Adx(</span>
                <Num value={cup.adx_period} min={2} max={200} onChange={(n) => setCup({ adx_period: n })} />
                <span>) &gt;</span>
                <Num value={cup.adx_min} step={0.5} min={0} max={100} onChange={(n) => setCup({ adx_min: n })} />
              </div>
              <div className="rule">
                <span>Volume &gt;</span>
                <Num value={cup.volume_multiple} step={0.1} min={0} max={20} onChange={(n) => setCup({ volume_multiple: n })} />
                <span>× Sma(Volume,</span>
                <Num value={cup.volume_sma} min={1} max={400} onChange={(n) => setCup({ volume_sma: n })} />
                <span>)</span>
              </div>
            </div>

            {/* `scan_lookback` is deliberately not offered. It is still in
                settings.json, but nothing in the engine reads it — the scan
                covers every stored bar — so a control for it would look like a
                knob and turn nothing. */}
            <div className="grid">
              <label>
                <span>Minimum score to list</span>
                <Num value={draft.min_pattern_score} step={0.05} min={0} max={1} onChange={(n) => set({ min_pattern_score: n })} />
                <i>0–1. Raise to cut weaker hits out of the scanner.</i>
              </label>
            </div>
          </section>

          <section>
            <h3>Universe</h3>
            <p className="note">
              While <code>data/universe_symbols.csv</code> exists it decides the universe outright
              and the market-cap rules below are only a fallback.
            </p>
            <div className="grid">
              <label>
                <span>Minimum market cap</span>
                <Num value={draft.min_mcap_cr} step={10} min={0} width={80} onChange={(n) => set({ min_mcap_cr: n })} />
                <i>₹ crore. Applied on the next universe refresh.</i>
              </label>
              <label>
                <span>Minimum price</span>
                <Num value={draft.min_price} step={1} min={0} onChange={(n) => set({ min_price: n })} />
                <i>₹. Penny-stock guard.</i>
              </label>
              <label>
                <span>Minimum median turnover</span>
                <Num value={draft.min_median_turnover_cr} step={0.05} min={0} onChange={(n) => set({ min_median_turnover_cr: n })} />
                <i>₹ crore/day. Only used when market cap is unknown.</i>
              </label>
              <label>
                <span>BSE groups to treat as mainboard</span>
                <input
                  className="text-field"
                  value={draft.bse_groups.join(", ")}
                  onChange={(e) =>
                    set({
                      bse_groups: e.target.value
                        .split(",")
                        .map((s) => s.trim().toUpperCase())
                        .filter(Boolean),
                    })
                  }
                />
                <i>A and B are mainboard; T/X/Z are surveillance, M is SME.</i>
              </label>
            </div>
            <Check
              checked={draft.include_unknown_mcap}
              onChange={(b) => set({ include_unknown_mcap: b })}
              label="Keep stocks whose market cap could not be resolved"
              hint="They are judged on traded value instead of being silently dropped."
            />
            <Check
              checked={draft.prefer_nse}
              onChange={(b) => set({ prefer_nse: b })}
              label="Prefer NSE when a company trades on both exchanges"
            />
          </section>

          <section>
            <h3>Downloads</h3>
            <p className="note">
              Upstox enforces per-second, per-minute <em>and</em> per-30-minute quotas. Concurrency
              alone only respects the first, which is why there is a separate rate cap.
            </p>
            <div className="grid">
              <label>
                <span>History to keep</span>
                <Num value={draft.history_days} step={10} min={60} max={3000} width={74} onChange={(n) => set({ history_days: n })} />
                <i>Calendar days. 400 ≈ one trading year plus warm-up.</i>
              </label>
              <label>
                <span>Parallel downloads</span>
                <Num value={draft.max_concurrency} min={1} max={32} onChange={(n) => set({ max_concurrency: n })} />
                <i>Requests in flight at once.</i>
              </label>
              <label>
                <span>Requests per second</span>
                <Num value={draft.requests_per_second} step={0.5} min={0.5} max={20} onChange={(n) => set({ requests_per_second: n })} />
                <i>Global cap across every worker.</i>
              </label>
            </div>
          </section>
        </div>

        <footer className="sheet-foot">
          <button
            className="ghost"
            disabled={busy}
            onClick={async () => setDraft(await defaultSettings())}
            title="Load the defaults into the form — nothing is written until you save"
          >
            Restore defaults
          </button>
          <div className="spacer" />
          {dirty && <span className="dirty">unsaved changes</span>}
          <button onClick={onClose} disabled={busy}>
            Cancel
          </button>
          {scanChanged ? (
            <button className="primary" disabled={busy} onClick={() => save(true)}>
              Save and rescan
            </button>
          ) : (
            <button className="primary" disabled={busy || !dirty} onClick={() => save(false)}>
              Save
            </button>
          )}
        </footer>
      </div>
    </div>
  );
}
