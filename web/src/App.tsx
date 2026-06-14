import { useStatus } from "./useStatus";
import { useControl } from "./useControl";
import { Header } from "./components/Header";
import { JobSection } from "./components/JobSection";
import { TempSection } from "./components/TempSection";
import { AmsSection } from "./components/AmsSection";
import { Controls, ConfirmDialog } from "./components/Controls";
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
        <main className="grid">
          <JobSection s={status} />
          <TempSection s={status} history={history} />
          {status.ams && <AmsSection ams={status.ams} />}
          <Controls control={control} />
          <HealthSection s={status} />
          <FooterSection s={status} />
        </main>
      ) : (
        <p className="waiting" data-testid="waiting">
          awaiting telemetry…
        </p>
      )}
      {control.toast && (
        <div className={`toast toast--${control.toast.kind}`} data-testid="toast">
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
    </div>
  );
}
