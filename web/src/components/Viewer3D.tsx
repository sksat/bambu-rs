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
//
// `ModelView` is the embeddable engine (mode toggle + canvas + playback) with no
// modal chrome, so it can be dropped into the file-detail screen and the
// pre-print dialog as well as a standalone modal.
export function ModelView({ path }: { path: string }) {
  const is3mf = path.toLowerCase().endsWith(".3mf");
  const mount = useRef<HTMLDivElement>(null);
  const [mode, setMode] = useState<Mode>(is3mf ? "mesh" : "toolpath");
  const [status, setStatus] = useState("loading…");
  // `noMesh` = this .3mf embeds no mesh, so the mesh toggle is disabled (rather
  // than silently bouncing back to toolpath). `dims` is the model's size in mm.
  const [noMesh, setNoMesh] = useState(false);
  const [dims, setDims] = useState<{ x: number; y: number; z: number } | null>(null);

  // Playback state. The render loop owns `head` (a ref); React state mirrors it
  // for the slider. `seek` lets the slider jump the loop; `playing` gates it.
  const [playing, setPlaying] = useState(true);
  const [progress, setProgress] = useState(0); // 0..1
  const [scrubbable, setScrubbable] = useState(false);
  const headRef = useRef(0);
  const totalRef = useRef(0);
  const playingRef = useRef(true);
  const seekRef = useRef<number | null>(null);

  // Reset mesh-availability only when the file changes — not on a mode switch
  // (which would clear the "no mesh" flag the moment we act on it).
  useEffect(() => {
    setNoMesh(false);
  }, [path]);

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
            // This .3mf embeds no mesh (e.g. a plate-only slice) — mark mesh
            // unavailable so its toggle is disabled, and show the toolpath.
            if (!disposed) {
              setNoMesh(true);
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
        const camera = new THREE.PerspectiveCamera(45, w / h, 0.1, 100000);
        // Transparent clear so the CSS white→grey gradient backdrop shows through.
        const renderer = new THREE.WebGLRenderer({ antialias: true, alpha: true });
        renderer.setClearColor(0x000000, 0);
        renderer.setPixelRatio(window.devicePixelRatio);
        renderer.setSize(w, h);
        el.appendChild(renderer.domElement);
        scene.add(new THREE.HemisphereLight(0xffffff, 0xb6bac2, 1.1));
        const key = new THREE.DirectionalLight(0xffffff, 0.85);
        key.position.set(120, -160, 220);
        scene.add(key);
        const fill = new THREE.DirectionalLight(0xffffff, 0.35);
        fill.position.set(-150, 130, 90);
        scene.add(fill);

        // Lay the model on a build plate centred at the origin (so the plate
        // centre is the view centre), Z up like the printer.
        const BED = 180; // A1 mini build plate (mm)
        const CELL = 20; // grid cell size (mm) — the scale reference
        const box = new THREE.Box3().setFromObject(object);
        const size = box.getSize(new THREE.Vector3());
        const ctr = box.getCenter(new THREE.Vector3());
        setDims({ x: size.x, y: size.y, z: size.z });
        if (pb) {
          // gcode is in absolute bed coordinates — shift so the bed centre is 0.
          object.position.set(-BED / 2, -BED / 2, 0);
        } else {
          // mesh is in local coordinates — centre its footprint, rest base on bed.
          object.position.set(-ctr.x, -ctr.y, -box.min.z);
        }
        scene.add(object);

        // The build plate: a soft surface + a grid that conveys scale.
        const plate = new THREE.Mesh(
          new THREE.PlaneGeometry(BED, BED),
          new THREE.MeshBasicMaterial({ color: 0xffffff, transparent: true, opacity: 0.5 }),
        );
        plate.position.z = -0.05;
        scene.add(plate);
        const grid = new THREE.GridHelper(BED, BED / CELL, 0x8b93a0, 0xccd1d8);
        grid.rotation.x = Math.PI / 2; // GridHelper sits in XZ; lay it flat in XY (Z up)
        scene.add(grid);

        // Frame the whole plate, looking at its centre.
        camera.up.set(0, 0, 1);
        const R = BED * 0.62;
        const vFov = (camera.fov * Math.PI) / 180;
        let dist = R / Math.sin(vFov / 2);
        if (camera.aspect < 1) dist /= camera.aspect;
        dist *= 1.05;
        camera.position.copy(new THREE.Vector3(0.9, -0.9, 0.72).normalize().multiplyScalar(dist));
        camera.near = Math.max(dist / 100, 0.1);
        camera.far = dist + BED * 4;
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
    <div className="modelview">
      {is3mf && (
        <div className="viewer__modes" role="group" aria-label="view mode">
          <button
            className={`btn btn--sm${mode === "mesh" ? " is-active" : ""}`}
            aria-pressed={mode === "mesh"}
            disabled={noMesh}
            title={noMesh ? "this file has no embedded mesh (toolpath only)" : undefined}
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
        </div>
      )}
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
      {dims && !status && (
        <div className="dim viewer__dims" data-testid="viewer-dims">
          {fmtMm(dims.x)} × {fmtMm(dims.y)} × {fmtMm(dims.z)} mm · 180 mm plate, 20 mm grid
        </div>
      )}
      {status && (
        <div className="dim viewer__status" data-testid="viewer-status">
          {status}
        </div>
      )}
    </div>
  );
}

// Format a millimetre extent: whole numbers for big parts, one decimal for small.
function fmtMm(v: number): string {
  return v >= 10 ? String(Math.round(v)) : v.toFixed(1);
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
  const lineMat = new THREE.LineBasicMaterial({ color: 0xb4801f }); // deep amber — reads on light
  const line = new THREE.LineSegments(geo, lineMat);
  geo.setDrawRange(0, 0);
  group.add(line);

  const sphereR = Math.max(0.6, span(pts) / 120);
  const marker = new THREE.Mesh(
    new THREE.SphereGeometry(sphereR, 16, 12),
    new THREE.MeshBasicMaterial({ color: 0x7c2d12 }), // dark — the head, visible on light
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
    color: 0x8f98a8, // medium slate — reads as a 3D print on the light backdrop
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
