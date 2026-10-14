'use client';

import { Component } from 'react';
import type { ErrorInfo, ReactNode } from 'react';

interface Props {
  title: string;
  children: ReactNode;
}

interface State {
  error: Error | null;
}

/**
 * Per-panel error boundary: one failing panel shows a compact error card
 * with a retry, and never blanks the whole terminal. componentDidCatch logs
 * only the error message/stack — never component props (no credential data
 * lives in these trees anyway; the secret is never passed as a prop).
 */
export class PanelErrorBoundary extends Component<Props, State> {
  state: State = { error: null };

  static getDerivedStateFromError(error: Error): State {
    return { error };
  }

  componentDidCatch(error: Error, info: ErrorInfo): void {
    console.error(`[panel:${this.props.title}]`, error.message, info.componentStack);
  }

  private reset = (): void => {
    this.setState({ error: null });
  };

  render(): ReactNode {
    if (this.state.error) {
      return (
        <div
          role="alert"
          className="flex h-full flex-col items-center justify-center gap-2 p-4 text-center"
        >
          <p className="text-xs font-semibold tracking-wide text-down-bright uppercase">
            Panel error — {this.props.title}
          </p>
          <p className="max-w-md font-mono text-[11px] break-words text-txt-dim">
            {this.state.error.message}
          </p>
          <button type="button" onClick={this.reset} className="btn btn-ghost">
            RETRY PANEL
          </button>
        </div>
      );
    }
    return this.props.children;
  }
}
