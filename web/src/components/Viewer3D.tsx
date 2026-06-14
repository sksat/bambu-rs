import { useEffect, useRef, useState } from "react";
import type { Object3D } from "three";

// Lazy 3D viewer: three.js + its loaders are dynamically imported (a separate
// chunk) only when opened, so they don't bloat the main bundle.
//
// Sliced Bambu files are `*.gcode.3mf`, whose mesh lives in external components
// (`3D/Objects/*.model`) that three's 3MFLoader doesn't follow — it renders
// empty. So for a `.3mf` we render the embedded *gcode toolpath* (via the
// dedicated /api/files/gcode endpoint) and only fall back to 3MFLoader for a
// non-sliced `.3mf` that has no plate gcode. A raw `.gcode` renders directly.
export function Viewer3D({ path, onClose }: { path: string; onClose: () => void }) {
  const mount = useRef<HTMLDivElement>(null);
  const [status, setStatus] = useState("loading…");

  useEffect(() => {
    let disposed = false;
    let cleanup = () => {};
    void (async () => {
      try {
        const THREE = await import("three");
        const { OrbitControls } = await import("three/addons/controls/OrbitControls.js");
        const object = await loadObject(path, (m) => setStatus(m));
        if (object === null) return; // status already set by loader
        if (disposed || !mount.current) return;

        const el = mount.current;
        const w = el.clientWidth || 600;
        const h = el.clientHeight || 420;
        const scene = new THREE.Scene();
        scene.background = new THREE.Color(0x111317);
        const camera = new THREE.PerspectiveCamera(45, w / h, 0.1, 100000);
        const renderer = new THREE.WebGLRenderer({ antialias: true });
        renderer.setPixelRatio(window.devicePixelRatio);
        renderer.setSize(w, h);
        el.appendChild(renderer.domElement);
        scene.add(new THREE.HemisphereLight(0xffffff, 0x333333, 1.3));
        const key = new THREE.DirectionalLight(0xffffff, 1.1);
        key.position.set(1, 1, 1);
        scene.add(key);

        // Center the object at the origin and frame its bounding sphere so the
        // part fills the view (a snug fit, accounting for the canvas aspect).
        const box = new THREE.Box3().setFromObject(object);
        const sphere = box.getBoundingSphere(new THREE.Sphere());
        object.position.sub(sphere.center);
        scene.add(object);
        const r = sphere.radius || 50;
        const vFov = (camera.fov * Math.PI) / 180;
        let dist = r / Math.sin(vFov / 2);
        if (camera.aspect < 1) dist /= camera.aspect; // portrait: fit width instead
        dist *= 1.2; // a little breathing room
        const dir = new THREE.Vector3(1, 0.85, 1).normalize();
        camera.position.copy(dir.multiplyScalar(dist));
        camera.near = Math.max(r / 100, 0.01);
        camera.far = dist + r * 4;
        camera.lookAt(0, 0, 0);
        camera.updateProjectionMatrix();
        const controls = new OrbitControls(camera, renderer.domElement);
        controls.target.set(0, 0, 0);
        controls.enableDamping = true;
        setStatus("");

        let raf = 0;
        const tick = () => {
          raf = requestAnimationFrame(tick);
          controls.update();
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
  }, [path]);

  return (
    <div className="modal" role="dialog" aria-modal="true" data-testid="viewer">
      <div className="modal__box modal__box--viewer">
        <div className="viewer__head">
          <span className="lbl">3D view</span>
          <span className="dim viewer__name">{path.split("/").pop()}</span>
          <button className="btn btn--sm viewer__close" onClick={onClose}>
            close
          </button>
        </div>
        <div className="viewer__canvas" ref={mount} data-testid="viewer-canvas" />
        {status && (
          <div className="dim viewer__status" data-testid="viewer-status">
            {status}
          </div>
        )}
      </div>
    </div>
  );
}

// Fetch + parse the model into a three Object3D, choosing the right source:
// `.gcode` → raw toolpath; `.gcode.3mf`/sliced `.3mf` → embedded plate gcode;
// non-sliced `.3mf` → 3MFLoader fallback. Returns null after calling `fail`.
async function loadObject(path: string, fail: (msg: string) => void): Promise<Object3D | null> {
  const lower = path.toLowerCase();
  const parseGcode = async (text: string) => {
    const obj = new (await import("three/addons/loaders/GCodeLoader.js")).GCodeLoader().parse(text);
    // GCodeLoader emits extrusion + travel as separate line objects sharing two
    // materials: extrusion uses the material named "extruded" (green); travel
    // (rapid) moves use the one confusingly named "path" (red). Remove travel
    // entirely (not just hide) so the preview shows just the printed part AND the
    // camera frames it — Box3.setFromObject would otherwise include the travel
    // extents and shrink the part to a dot.
    const travel: Object3D[] = [];
    obj.traverse((c: Object3D) => {
      if ((c as { material?: { name?: string } }).material?.name === "path") travel.push(c);
    });
    for (const c of travel) c.parent?.remove(c);
    return obj;
  };

  if (lower.endsWith(".gcode")) {
    const res = await fetch(`/api/files/raw?name=${encodeURIComponent(path)}`);
    if (!res.ok) return fail(`couldn't load model (HTTP ${res.status})`), null;
    return parseGcode(await res.text());
  }

  if (lower.endsWith(".3mf")) {
    // Prefer the sliced plate's gcode toolpath (what actually renders).
    const g = await fetch(`/api/files/gcode?name=${encodeURIComponent(path)}&plate=1`);
    if (g.ok) return parseGcode(await g.text());
    // No plate gcode → a non-sliced model 3mf; try the mesh loader (best effort).
    if (g.status === 404) {
      const res = await fetch(`/api/files/raw?name=${encodeURIComponent(path)}`);
      if (!res.ok) return fail(`couldn't load model (HTTP ${res.status})`), null;
      return new (await import("three/addons/loaders/3MFLoader.js")).ThreeMFLoader().parse(
        await res.arrayBuffer(),
      );
    }
    return fail(`couldn't load model (HTTP ${g.status})`), null;
  }

  return fail("unsupported file type"), null;
}
