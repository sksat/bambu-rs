import type { Conn } from "../useStatus";
import { useTheme } from "../useTheme";
import type { Theme } from "../useTheme";

const ICON: Record<Theme, string> = { auto: "◐", dark: "●", light: "○" };

export function Header({ conn }: { conn: Conn }) {
  const { theme, cycle } = useTheme();
  return (
    <header className="hdr">
      <span className="hdr__brand">
        bambu<span className="dim"> / dashboard</span>
      </span>
      <div className="hdr__right">
        <button
          className="theme"
          onClick={cycle}
          title={`theme: ${theme} (click to change)`}
          aria-label={`theme: ${theme}`}
          data-testid="theme"
        >
          {ICON[theme]} {theme}
        </button>
        <span className={`conn conn--${conn}`} data-testid="conn">
          <i className="dot" />
          {conn}
        </span>
      </div>
    </header>
  );
}
