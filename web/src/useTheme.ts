import { useEffect, useState } from "react";

export type Theme = "auto" | "dark" | "light";
const ORDER: Theme[] = ["auto", "dark", "light"];

/** Theme preference, persisted in localStorage and applied via a `data-theme`
 *  attribute on <html> (absent = follow the OS via prefers-color-scheme). */
export function useTheme(): { theme: Theme; cycle: () => void } {
  const [theme, setTheme] = useState<Theme>(
    () => (localStorage.getItem("bambu_theme") as Theme | null) ?? "auto",
  );

  useEffect(() => {
    const root = document.documentElement;
    if (theme === "auto") root.removeAttribute("data-theme");
    else root.setAttribute("data-theme", theme);
    localStorage.setItem("bambu_theme", theme);
  }, [theme]);

  return {
    theme,
    cycle: () => setTheme((t) => ORDER[(ORDER.indexOf(t) + 1) % ORDER.length]),
  };
}
