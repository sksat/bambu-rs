import { Component } from "react";
import type { ErrorInfo, ReactNode } from "react";

// A render error in any panel shows an inline message instead of a blank page.
export class ErrorBoundary extends Component<{ children: ReactNode }, { error: string | null }> {
  constructor(props: { children: ReactNode }) {
    super(props);
    this.state = { error: null };
  }

  static getDerivedStateFromError(err: unknown) {
    return { error: err instanceof Error ? err.message : String(err) };
  }

  componentDidCatch(err: unknown, info: ErrorInfo) {
    console.error("dashboard render error", err, info);
  }

  render() {
    if (this.state.error) {
      return (
        <p className="waiting auth-err" data-testid="render-error">
          dashboard error: {this.state.error} — try reloading.
        </p>
      );
    }
    return this.props.children;
  }
}
