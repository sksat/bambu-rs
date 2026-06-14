import { useEffect, useRef, useState } from "react";

// Lazy 3D viewer: three.js + its loaders are dynamically imported (a separate
// chunk) only when opened, so they don't bloat the main bundle. Renders a .3mf
// mesh (or .gcode toolpath) fetched from /api/files/raw.
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
        const res = await fetch(`/api/files/raw?name=${encodeURIComponent(path)}`);
        if (!res.ok) {
          setStatus(`couldn't load model (HTTP ${res.status})`);
          return;
        }
        const buf = await res.arrayBuffer();
        if (disposed || !mount.current) return;

        const isGcode = path.toLowerCase().endsWith(".gcode");
        const object = isGcode
          ? new (await import("three/addons/loaders/GCodeLoader.js")).GCodeLoader().parse(
              new TextDecoder().decode(buf),
            )
          : new (await import("three/addons/loaders/3MFLoader.js")).ThreeMFLoader().parse(buf);

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

        // Center the object and frame it.
        const box = new THREE.Box3().setFromObject(object);
        const size = box.getSize(new THREE.Vector3());
        const center = box.getCenter(new THREE.Vector3());
        object.position.sub(center);
        scene.add(object);
        const maxDim = Math.max(size.x, size.y, size.z) || 100;
        camera.position.set(maxDim * 1.4, maxDim * 1.1, maxDim * 1.4);
        camera.near = maxDim / 100;
        camera.far = maxDim * 100;
        camera.updateProjectionMatrix();
        const controls = new OrbitControls(camera, renderer.domElement);
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
