import { useEffect, useRef, useState } from "react";

type Mode = "mesh" | "toolpath";
type THREE = typeof import("three");

// 3D viewer with two modes:
//  • mesh     — the solid model, parsed from the .3mf's embedded object meshes
//               (three's 3MFLoader won't follow Bambu's external-component refs,
//               so the server hands us the mesh XML at /api/files/mesh and we
//               build the geometry ourselves).
//  • toolpath — the sliced gcode path, with playback: a scrubber + play/pause
//               that reveals the extrusion trail as a head marker walks the path,
//               so you can watch how the head moves.
// three.js is dynamically imported (a lazy chunk) only when the viewer opens.
export function Viewer3D({ path, onClose }: { path: string; onClose: () => void }) {
  const is3mf = path.toLowerCase().endsWith(".3mf");
  const mount = useRef<HTMLDivElement>(null);
  const [mode, setMode] = useState<Mode>(is3mf ? "mesh" : "toolpath");
  const [status, setStatus] = useState("loading…");

  // Playback state. The render loop owns `head` (a ref); React state mirrors it
  // for the slider. `seek` lets the slider jump the loop; `playing` gates it.
  const [playing, setPlaying] = useState(true);
  const [progress, setProgress] = useState(0); // 0..1
  const [scrubbable, setScrubbable] = useState(false);
  const headRef = useRef(0);
  const totalRef = useRef(0);
  const playingRef = useRef(true);
  const seekRef = useRef<number | null>(null);

  useEffect(() => {
    let disposed = false;
    let cleanup = () => {};
    void (async () => {
      try {
        setStatus("loading…");
        setScrubbable(false);
        const THREE = await import("three");
        const { OrbitControls } = await import("three/addons/controls/OrbitControls.js");

        // Build the object (+ playback handle) for the active mode.
        let object: import("three").Object3D;
        let pb: Playback | null = null;
        if (mode === "mesh") {
          const r = await fetch(`/api/files/mesh?name=${encodeURIComponent(path)}`);
          if (!r.ok) return setStatus(`couldn't load mesh (HTTP ${r.status})`);
          const models = ((await r.json()) as { models?: string[] }).models ?? [];
          if (!models.length) {
            // Not a meshed .3mf (or extraction failed) — fall back to toolpath.
            if (!disposed) {
              setStatus("no embedded mesh — showing toolpath");
              setMode("toolpath");
            }
            return;
          }
          object = buildMesh(models, THREE);
        } else {
          const url = is3mf
            ? `/api/files/gcode?name=${encodeURIComponent(path)}&plate=1`
            : `/api/files/raw?name=${encodeURIComponent(path)}`;
          const r = await fetch(url);
          if (!r.ok) return setStatus(`couldn't load toolpath (HTTP ${r.status})`);
          const built = buildToolpath(await r.text(), THREE);
          if (!built) return setStatus("no toolpath in this file");
          object = built.object;
          pb = built.playback;
        }
        if (disposed || !mount.current) return;

        const el = mount.current;
        const w = el.clientWidth || 600;
        const h = el.clientHeight || 420;
        const scene = new THREE.Scene();
        scene.background = new THREE.Color(0x1b1e24); // soft slate, not harsh black
        const camera = new THREE.PerspectiveCamera(45, w / h, 0.1, 100000);
        const renderer = new THREE.WebGLRenderer({ antialias: true });
        renderer.setPixelRatio(window.devicePixelRatio);
        renderer.setSize(w, h);
        el.appendChild(renderer.domElement);
        scene.add(new THREE.HemisphereLight(0xffffff, 0x2a2e36, 1.25));
        const key = new THREE.DirectionalLight(0xffffff, 1.0);
        key.position.set(1, 1.4, 1);
        scene.add(key);

        // Printer space is Z-up; three is Y-up. Rotate so models stand correctly.
        object.rotation.x = -Math.PI / 2;
        // Center on the origin and frame the bounding sphere snugly.
        const sphere = new THREE.Box3().setFromObject(object).getBoundingSphere(new THREE.Sphere());
        object.position.sub(sphere.center);
        scene.add(object);
        const r = sphere.radius || 50;
        const vFov = (camera.fov * Math.PI) / 180;
        let dist = r / Math.sin(vFov / 2);
        if (camera.aspect < 1) dist /= camera.aspect;
        dist *= 1.2;
        camera.position.copy(new THREE.Vector3(1, 0.85, 1).normalize().multiplyScalar(dist));
        camera.near = Math.max(r / 100, 0.01);
        camera.far = dist + r * 4;
        camera.lookAt(0, 0, 0);
        camera.updateProjectionMatrix();
        const controls = new OrbitControls(camera, renderer.domElement);
        controls.target.set(0, 0, 0);
        controls.enableDamping = true;

        // Wire up playback (toolpath only).
        if (pb) {
          totalRef.current = pb.lastPoint;
          headRef.current = 0;
          playingRef.current = true;
          setPlaying(true);
          setScrubbable(true);
          pb.apply(0);
        }
        setStatus("");

        let raf = 0;
        let frame = 0;
        const step = pb ? Math.max(1, pb.lastPoint / 720) : 0; // ~12 s full play
        const tick = () => {
          raf = requestAnimationFrame(tick);
          controls.update();
          if (pb) {
            if (seekRef.current != null) {
              headRef.current = seekRef.current;
              seekRef.current = null;
            } else if (playingRef.current) {
              headRef.current = Math.min(pb.lastPoint, headRef.current + step);
              if (headRef.current >= pb.lastPoint) {
                playingRef.current = false;
                setPlaying(false);
              }
            }
            pb.apply(headRef.current);
            if (frame++ % 4 === 0) setProgress(headRef.current / (pb.lastPoint || 1));
          }
          renderer.render(scene, camera);
        };
        tick();

        const onResize = () => {
          const nw = el.clientWidth || 600;
          const nh = el.clientHeight || 420;
          camera.aspect = nw / nh;
          camera.updateProjectionMatrix();
          renderer.setSize(nw, nh);
        };
        window.addEventListener("resize", onResize);
        cleanup = () => {
          cancelAnimationFrame(raf);
          window.removeEventListener("resize", onResize);
          controls.dispose();
          renderer.dispose();
          renderer.domElement.remove();
        };
      } catch {
        setStatus("couldn't render this model");
      }
    })();
    return () => {
      disposed = true;
      cleanup();
    };
  }, [path, mode, is3mf]);

  const togglePlay = () => {
    const np = !playing;
    if (np && headRef.current >= totalRef.current) {
      headRef.current = 0; // restart from the beginning
      seekRef.current = 0;
    }
    playingRef.current = np;
    setPlaying(np);
  };
  const onScrub = (v: number) => {
    seekRef.current = v;
    playingRef.current = false;
    setPlaying(false);
    setProgress(totalRef.current ? v / totalRef.current : 0);
  };

  return (
    <div className="modal" role="dialog" aria-modal="true" data-testid="viewer">
      <div className="modal__box modal__box--viewer">
        <div className="viewer__head">
          <span className="lbl">3D view</span>
          <span className="dim viewer__name">{path.split("/").pop()}</span>
          {is3mf && (
            <span className="viewer__modes" role="group" aria-label="view mode">
              <button
                className={`btn btn--sm${mode === "mesh" ? " is-active" : ""}`}
                aria-pressed={mode === "mesh"}
                onClick={() => setMode("mesh")}
                data-testid="viewer-mode-mesh"
              >
                mesh
              </button>
              <button
                className={`btn btn--sm${mode === "toolpath" ? " is-active" : ""}`}
                aria-pressed={mode === "toolpath"}
                onClick={() => setMode("toolpath")}
                data-testid="viewer-mode-toolpath"
              >
                toolpath
              </button>
            </span>
          )}
          <button className="btn btn--sm viewer__close" onClick={onClose}>
            close
          </button>
        </div>
        <div className="viewer__canvas" ref={mount} data-testid="viewer-canvas" />
        {mode === "toolpath" && scrubbable && (
          <div className="viewer__play" data-testid="viewer-play-bar">
            <button
              className="btn btn--sm"
              onClick={togglePlay}
              data-testid="viewer-play"
              aria-label={playing ? "pause" : "play"}
            >
              {playing ? "⏸" : "▶"}
            </button>
            <input
              className="viewer__scrub"
              type="range"
              min={0}
              max={totalRef.current || 1}
              value={Math.round(progress * (totalRef.current || 1))}
              onChange={(e) => onScrub(Number(e.target.value))}
              data-testid="viewer-scrub"
              aria-label="playback position"
            />
            <span className="viewer__pct mono">{Math.round(progress * 100)}%</span>
          </div>
        )}
        {status && (
          <div className="dim viewer__status" data-testid="viewer-status">
            {status}
          </div>
        )}
      </div>
    </div>
  );
}

// A playback handle for the toolpath: reveal the extrusion trail up to point `k`
// and move the head marker there.
interface Playback {
  lastPoint: number;
  apply: (k: number) => void;
}

// Parse gcode into an ordered path and build a line (extrusion only) + a head
// marker, returning a Playback that reveals the line via drawRange.
function buildToolpath(
  text: string,
  THREE: THREE,
): { object: import("three").Object3D; playback: Playback } | null {
  let x = 0;
  let y = 0;
  let z = 0;
  let e = 0;
  let absXYZ = true;
  let absE = true;
  const pts: number[] = [0, 0, 0]; // point 0 = origin
  const extr: boolean[] = []; // extr[i] = segment point i → i+1 extrudes
  for (const raw of text.split("\n")) {
    const line = raw.split(";")[0].trim();
    if (!line) continue;
    const up = line.toUpperCase();
    const code = /^[GM]\d+/.exec(up)?.[0];
    if (!code) continue;
    if (code === "G90") absXYZ = true;
    else if (code === "G91") absXYZ = false;
    else if (code === "M82") absE = true;
    else if (code === "M83") absE = false;
    else if (code === "G92") {
      const ev = axis(up, "E");
      if (ev != null) e = ev;
    } else if (code === "G0" || code === "G1") {
      const nx = axis(up, "X");
      const ny = axis(up, "Y");
      const nz = axis(up, "Z");
      const ne = axis(up, "E");
      x = nx == null ? x : absXYZ ? nx : x + nx;
      y = ny == null ? y : absXYZ ? ny : y + ny;
      z = nz == null ? z : absXYZ ? nz : z + nz;
      let extruding = false;
      if (ne != null) {
        const de = absE ? ne - e : ne;
        extruding = de > 1e-6;
        e = absE ? ne : e + ne;
      }
      pts.push(x, y, z);
      extr.push(extruding && code === "G1");
    }
  }
  const nPoints = pts.length / 3;
  if (nPoints < 2) return null;

  // Extrusion segments + a per-point prefix of how many line vertices to reveal.
  const segPos: number[] = [];
  const reveal = new Array<number>(nPoints).fill(0);
  for (let i = 0; i < extr.length; i++) {
    if (extr[i]) {
      segPos.push(pts[i * 3], pts[i * 3 + 1], pts[i * 3 + 2]);
      segPos.push(pts[(i + 1) * 3], pts[(i + 1) * 3 + 1], pts[(i + 1) * 3 + 2]);
    }
    reveal[i + 1] = segPos.length / 3;
  }

  const group = new THREE.Group();
  const geo = new THREE.BufferGeometry();
  geo.setAttribute("position", new THREE.BufferAttribute(new Float32Array(segPos), 3));
  const lineMat = new THREE.LineBasicMaterial({ color: 0xd8a657 }); // amber, easy on the eyes
  const line = new THREE.LineSegments(geo, lineMat);
  geo.setDrawRange(0, 0);
  group.add(line);

  const sphereR = Math.max(0.6, span(pts) / 120);
  const marker = new THREE.Mesh(
    new THREE.SphereGeometry(sphereR, 16, 12),
    new THREE.MeshBasicMaterial({ color: 0xf0e0c0 }),
  );
  group.add(marker);

  const lastPoint = nPoints - 1;
  const apply = (k: number) => {
    const i = Math.min(lastPoint, Math.max(0, Math.floor(k)));
    geo.setDrawRange(0, reveal[i]);
    marker.position.set(pts[i * 3], pts[i * 3 + 1], pts[i * 3 + 2]);
  };
  return { object: group, playback: { lastPoint, apply } };
}

// Build a solid mesh from the .3mf's object model XML(s).
function buildMesh(models: string[], THREE: THREE): import("three").Object3D {
  const group = new THREE.Group();
  const mat = new THREE.MeshStandardMaterial({
    color: 0xb9bec8, // warm light grey — a neutral "print" look
    metalness: 0.05,
    roughness: 0.72,
  });
  for (const xml of models) {
    const doc = new DOMParser().parseFromString(xml, "application/xml");
    const verts = doc.getElementsByTagName("vertex");
    const tris = doc.getElementsByTagName("triangle");
    if (!verts.length || !tris.length) continue;
    const pos = new Float32Array(verts.length * 3);
    for (let i = 0; i < verts.length; i++) {
      pos[i * 3] = num(verts[i].getAttribute("x"));
      pos[i * 3 + 1] = num(verts[i].getAttribute("y"));
      pos[i * 3 + 2] = num(verts[i].getAttribute("z"));
    }
    const idx: number[] = [];
    for (let i = 0; i < tris.length; i++) {
      idx.push(num(tris[i].getAttribute("v1")), num(tris[i].getAttribute("v2")), num(tris[i].getAttribute("v3")));
    }
    const geo = new THREE.BufferGeometry();
    geo.setAttribute("position", new THREE.BufferAttribute(pos, 3));
    geo.setIndex(idx);
    geo.computeVertexNormals();
    group.add(new THREE.Mesh(geo, mat));
  }
  return group;
}

function axis(line: string, a: string): number | null {
  const m = new RegExp(`${a}(-?\\d*\\.?\\d+)`).exec(line);
  return m ? parseFloat(m[1]) : null;
}
function num(s: string | null): number {
  const v = s == null ? 0 : parseFloat(s);
  return Number.isFinite(v) ? v : 0;
}
// Rough overall extent of the path, for sizing the head marker.
function span(pts: number[]): number {
  let min = Infinity;
  let max = -Infinity;
  for (const v of pts) {
    if (v < min) min = v;
    if (v > max) max = v;
  }
  return Number.isFinite(max - min) ? max - min : 100;
}
