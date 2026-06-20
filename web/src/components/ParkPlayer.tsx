import { useEffect, useRef, useState } from "react";
import { listParks, type ParkIndex } from "../cameras";

// How often to re-poll the filmstrip index (picks up new frames while a run is live),
// and the playback rate when "play" is pressed.
const POLL_MS = 1500;
const PLAY_FPS = 6;

// A video-player-style review of one camera's park filmstrip: scrub past frames, step
// frame-by-frame, play through them, or follow the live tip as the run captures more.
// Frames are the indexed `/park/{n}` JPEGs; the index (`/parks`) is polled so it works
// both during a run and after it stops (until the next run replaces it).
export function ParkPlayer({ id }: { id: string }) {
  const [idx, setIdx] = useState<ParkIndex>({ running: false, count: 0, parks: [] });
  const [cur, setCur] = useState(0);
  const [playing, setPlaying] = useState(false);
  // Follow the live tip: stay pinned to the newest frame as new ones arrive. Set false
  // the moment the user scrubs/steps back, true again when they return to the end.
  const [atLive, setAtLive] = useState(true);
  // Bumped each poll; used only to bust the cache on the LIVE tip so a freshly-written
  // (or replaced) frame refreshes, while past frames keep a stable, cacheable src.
  const [tick, setTick] = useState(0);

  const count = idx.count;
  const last = Math.max(0, count - 1);

  // Mirror `cur` into a ref so the playback interval reads the latest without re-arming.
  const curRef = useRef(0);
  useEffect(() => {
    curRef.current = cur;
  }, [cur]);

  // Poll the filmstrip index (live during a run, frozen after it stops).
  useEffect(() => {
    let live = true;
    const poll = async () => {
      const s = await listParks(id);
      if (!live) return;
      setIdx(s);
      setTick((t) => t + 1);
    };
    void poll();
    const h = setInterval(() => void poll(), POLL_MS);
    return () => {
      live = false;
      clearInterval(h);
    };
  }, [id]);

  // Keep `cur` valid as the strip grows/shrinks: pinned to the tip while following,
  // otherwise clamped into range.
  useEffect(() => {
    if (count === 0) {
      setCur(0);
      return;
    }
    setCur((c) => (atLive ? last : Math.min(c, last)));
  }, [count, atLive, last]);

  // Playback: advance ~PLAY_FPS. At the end, tail the live tip if the run is still going
  // (sit at the newest, jump forward as frames arrive); else stop — a finished strip
  // doesn't loop. setState stays out of the updater (curRef drives it) to keep it clean.
  useEffect(() => {
    if (!playing || count === 0) return;
    const h = setInterval(() => {
      const c = curRef.current;
      if (c >= last) {
        if (idx.running) setAtLive(true);
        else setPlaying(false);
        return;
      }
      const nx = c + 1;
      setCur(nx);
      if (nx >= last) setAtLive(true);
    }, 1000 / PLAY_FPS);
    return () => clearInterval(h);
  }, [playing, count, last, idx.running]);

  const goto = (n: number) => {
    const v = Math.min(last, Math.max(0, n));
    setCur(v);
    setAtLive(v >= last);
  };
  const first = () => {
    setPlaying(false);
    goto(0);
  };
  const prev = () => {
    setPlaying(false);
    goto(cur - 1);
  };
  const next = () => goto(cur + 1);
  const latest = () => {
    setAtLive(true);
    setCur(last);
  };

  if (count === 0) {
    return (
      <div className="pp pp--empty" data-testid="park-player">
        <div className="cam__msg" data-testid="park-empty">
          {idx.running ? "waiting for the first park…" : "no park frames — start a park run"}
        </div>
      </div>
    );
  }

  const liveTip = atLive && idx.running;
  const meta = idx.parks[cur];
  // `cur` is the scrubber POSITION; the frame URL needs the entry's real index `n`, which
  // can be sparse (the index parser skips malformed lines), so don't assume n === position.
  const frameN = meta?.n ?? cur;
  // Frames are the indexed `/park/{n}` JPEGs (the index lives at `/park`).
  const src = `/api/camera/${id}/park/${frameN}${liveTip ? `?t=${tick}` : ""}`;

  return (
    <div className="pp" data-testid="park-player">
      <img
        className="pp__frame"
        src={src}
        alt={`park frame ${cur + 1} of ${count}`}
        data-testid="park-frame"
        data-n={frameN}
      />
      <div className="pp__bar" data-testid="park-transport">
        <button
          className="pp__btn"
          data-testid="park-first"
          title="first frame"
          aria-label="first frame"
          disabled={cur === 0}
          onClick={first}
        >
          ⏮
        </button>
        <button
          className="pp__btn"
          data-testid="park-prev"
          title="previous frame"
          aria-label="previous frame"
          disabled={cur === 0}
          onClick={prev}
        >
          ◀
        </button>
        <button
          className="pp__btn pp__btn--play"
          data-testid="park-play"
          title={playing ? "pause" : "play"}
          aria-label={playing ? "pause" : "play"}
          onClick={() => setPlaying((p) => !p)}
        >
          {playing ? "⏸" : "▶"}
        </button>
        <button
          className="pp__btn"
          data-testid="park-next"
          title="next frame"
          aria-label="next frame"
          disabled={cur >= last}
          onClick={next}
        >
          ▶
        </button>
        <button
          className="pp__btn"
          data-testid="park-latest"
          title="latest frame"
          aria-label="latest frame"
          disabled={cur >= last && !idx.running}
          onClick={latest}
        >
          ⏭
        </button>
        <input
          className="pp__scrub"
          type="range"
          min={0}
          max={last}
          value={cur}
          data-testid="park-scrub"
          aria-label="scrub park frames"
          onChange={(e) => goto(Number(e.target.value))}
        />
        <span className="pp__count" data-testid="park-count">
          {cur + 1} / {count}
        </span>
        {liveTip && (
          <span className="pp__live" data-testid="park-live">
            ● live
          </span>
        )}
      </div>
      {meta?.t != null && (
        <div className="pp__meta dim" data-testid="park-meta">
          t={meta.t.toFixed(1)}s
          {meta.confidence != null ? ` · conf ${meta.confidence.toFixed(2)}` : ""}
        </div>
      )}
    </div>
  );
}
