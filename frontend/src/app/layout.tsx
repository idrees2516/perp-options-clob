import type { Metadata, Viewport } from 'next';
import type { ReactNode } from 'react';
import './globals.css';
import { ToastProvider } from '@/components/Toast';
import { SessionProvider } from '@/providers/SessionProvider';

export const metadata: Metadata = {
  title: 'ns-venue 0DTE Terminal',
  description:
    'Trading terminal for the ns-venue 0DTE options matching engine (integer ticks, HMAC-signed REST + WebSocket)',
};

export const viewport: Viewport = {
  width: 'device-width',
  initialScale: 1,
  themeColor: '#0a0e12',
};

export default function RootLayout({ children }: { children: ReactNode }) {
  return (
    <html lang="en">
      <body className="bg-bg font-sans text-txt antialiased">
        <ToastProvider>
          <SessionProvider>{children}</SessionProvider>
        </ToastProvider>
      </body>
    </html>
  );
}
