import { useStatus } from "./useStatus";
import { useControl } from "./useControl";
import { Header } from "./components/Header";
import { OverviewSection } from "./components/OverviewSection";
import { TempSection } from "./components/TempSection";
import { AmsSection } from "./components/AmsSection";
import { Controls, ConfirmDialog } from "./components/Controls";
import { MachineSection } from "./components/MachineSection";
import { CamerasSection } from "./components/CameraSection";
import { ErrorBoundary } from "./components/ErrorBoundary";
import { FilesSection } from "./components/FilesSection";
import { HealthSection } from "./components/HealthSection";
import { FooterSection } from "./components/FooterSection";
import "./app.css";

// Instrument / telemetry dashboard: dense readouts, one accent, panels that
// reflow fluidly from one phone column to a multi-column desktop grid. Live over
// /api/ws; control via the password-optional write API.
export function App() {
  const { status, conn, history } = useStatus();
  const control = useControl();
  return (
    <div className="term">
      <Header conn={conn} />
      {status ? (
        <ErrorBoundary>
          {/* Monitoring-first: a hero row pairs the camera(s) with the vital
              readouts; controls + machine sit below; files and health span full
              width. On a phone it all collapses to one column (vitals first). */}
          <main className="dash">
            {/* Big status band across the top. */}
            <section className="dash__status">
              <OverviewSection s={status} control={control} />
            </section>
            {/* Left: camera(s) with the temperature card (readouts + its own
                set/cool) beneath. Right: one controls section with the
                machine/motion and AMS folded in. Stacks to a single column on a
                phone. */}
            <section className="dash__hero">
              <div className="dash__left">
                <CamerasSection password={control.password || null} />
                <TempSection s={status} history={history} control={control} />
              </div>
              <div className="dash__control">
                <Controls control={control} status={status}>
                  <MachineSection s={status} control={control} />
                  {status.ams && <AmsSection ams={status.ams} />}
                </Controls>
              </div>
            </section>
            <FilesSection sdcard={status.sdcard} />
            <HealthSection s={status} />
            <FooterSection s={status} />
          </main>
        </ErrorBoundary>
      ) : (
        <p className="waiting" data-testid="waiting">
          awaiting telemetry…
        </p>
      )}
      {control.toast && (
        <div className={`toast toast--${control.toast.kind}`} data-testid="toast">
          {/* CUD: kind is shown by a shape (✓ / ⚠ / ✕), not just the border
              colour, so ok and warn don't blur together for colour-blind users. */}
          <span className="toast__icon" aria-hidden="true">
            {control.toast.kind === "ok" ? "✓" : control.toast.kind === "warn" ? "⚠" : "✕"}
          </span>
          {control.toast.msg}
        </div>
      )}
      {control.confirmStop && (
        <ConfirmDialog
          message="Stop the running print? This can't be undone."
          onConfirm={() => control.confirmAndStop()}
          onCancel={() => control.cancelStop()}
        />
      )}
      {control.confirm && (
        <ConfirmDialog
          message={control.confirm.message}
          confirmLabel={control.confirm.label ?? "confirm"}
          onConfirm={() => control.runConfirm()}
          onCancel={() => control.cancelConfirm()}
        />
      )}
    </div>
  );
}
