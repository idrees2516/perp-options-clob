"use client";

/**
 * Connection manager — transport selection + gateway credentials.
 *
 * "Sim" runs the deterministic venue in a Web Worker (zero backend);
 * "Gateway" connects to the production socket.io venue with G-25 HMAC
 * credentials, heartbeats and reconnection. The API secret lives in
 * sessionStorage only (dies with the tab) — never localStorage.
 */

import { memo, useCallback, useEffect, useMemo, useState } from "react";
import { toast } from "sonner";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { useVenueStore } from "@/lib/venue-store";
import {
  GatewayRestClient,
  GatewayRestError,
  type GatewayStatsResponse,
  type RemoteEndpoint,
} from "@perp/api-client";
import {
  Cpu,
  Server,
  KeyRound,
  Copy,
  RefreshCw,
  Unplug,
  ShieldCheck,
  Activity,
  Loader2,
  Trash2,
} from "lucide-react";

export const ConnectionDialog = memo(function ConnectionDialog() {
  const open = useVenueStore((s) => s.connectionOpen);
  const setOpen = useVenueStore((s) => s.setConnectionOpen);
  const mode = useVenueStore((s) => s.mode);
  const gateway = useVenueStore((s) => s.gateway);
  const credentials = useVenueStore((s) => s.credentials);
  const connected = useVenueStore((s) => s.connected);
  const connectSim = useVenueStore((s) => s.connectSim);
  const connectRemote = useVenueStore((s) => s.connectRemote);
  const disconnectVenue = useVenueStore((s) => s.disconnectVenue);
  const setCredentials = useVenueStore((s) => s.setCredentials);

  const [origin, setOrigin] = useState("");
  const [port, setPort] = useState("3031");
  const [keyId, setKeyId] = useState("");
  const [secret, setSecret] = useState("");
  const [provisioning, setProvisioning] = useState(false);
  const [stats, setStats] = useState<GatewayStatsResponse | null>(null);
  const [loadingStats, setLoadingStats] = useState(false);

  // Render-phase state reset when the dialog opens/closes (no effects).
  const [wasOpen, setWasOpen] = useState(false);
  if (open && !wasOpen) {
    setWasOpen(true);
    setKeyId(credentials?.key_id ?? "");
    setSecret(credentials?.secret ?? "");
  } else if (!open && wasOpen) {
    setWasOpen(false);
    setStats(null);
  }

  const endpoint: RemoteEndpoint = useMemo(
    () => ({ origin: origin.trim() || undefined, port: Number(port) || 3031 }),
    [origin, port],
  );

  const loadStats = useCallback(async () => {
    setLoadingStats(true);
    try {
      const rest = new GatewayRestClient(endpoint, keyId && secret ? { key_id: keyId, secret } : null);
      setStats(await rest.stats());
    } catch (err) {
      setStats(null);
      if (err instanceof GatewayRestError && err.status !== 401) {
        toast.error("Gateway unreachable", { description: err.message });
      }
    } finally {
      setLoadingStats(false);
    }
  }, [endpoint, keyId, secret]);

  useEffect(() => {
    if (open && mode === "remote") {
      const t = setTimeout(() => void loadStats(), 0);
      return () => clearTimeout(t);
    }
  }, [open, mode, loadStats]);

  const provision = useCallback(async () => {
    setProvisioning(true);
    try {
      const rest = new GatewayRestClient(endpoint);
      const key = await rest.provision("trader");
      setKeyId(key.key_id);
      setSecret(key.secret);
      setCredentials({ key_id: key.key_id, secret: key.secret });
      toast.success("API key provisioned", {
        description: "G-25 HMAC credentials stored for this tab session only",
      });
    } catch (err) {
      toast.error("Provisioning failed", {
        description: err instanceof GatewayRestError ? err.message : String(err),
      });
    } finally {
      setProvisioning(false);
    }
  }, [endpoint, setCredentials]);

  const statusColor =
    gateway.status === "live"
      ? "bg-up"
      : gateway.status === "reconnecting" || gateway.status === "connecting"
        ? "bg-amber-400"
        : gateway.status === "error"
          ? "bg-down"
          : "bg-muted-foreground/40";

  return (
    <Dialog open={open} onOpenChange={setOpen}>
      <DialogContent className="max-w-lg border-hairline bg-panel-2 p-0 gap-0 overflow-hidden">
        <DialogHeader className="px-5 pt-5 pb-3 border-b border-hairline">
          <DialogTitle className="text-sm font-semibold flex items-center gap-2">
            <Activity className="w-4 h-4 text-primary" />
            Venue connection
          </DialogTitle>
          <DialogDescription className="text-xs">
            One contract, two transports — the embedded deterministic sim, or the production
            gateway (socket.io + G-25 signed REST).
          </DialogDescription>
        </DialogHeader>

        <div className="px-5 py-4 space-y-4 max-h-[70vh] overflow-auto scroll-thin">
          {/* transport picker */}
          <div className="grid grid-cols-2 gap-2">
            <button
              type="button"
              onClick={() => connectSim()}
              aria-pressed={mode === "sim"}
              className={`panel p-3 text-left transition-colors ${
                mode === "sim" ? "ring-1 ring-primary/60 bg-primary/5" : "hover:bg-muted/40"
              }`}
            >
              <div className="flex items-center gap-2 mb-1">
                <Cpu className="w-3.5 h-3.5 text-primary" />
                <span className="text-xs font-semibold">Embedded sim</span>
              </div>
              <p className="text-[10.5px] text-muted-foreground leading-relaxed">
                Deterministic venue in a Web Worker — zero backend, works anywhere the app is hosted.
              </p>
            </button>
            <button
              type="button"
              onClick={() => connectRemote({ origin: origin.trim() || undefined, port: Number(port) || 3031 }, keyId && secret ? { key_id: keyId, secret } : null)}
              aria-pressed={mode === "remote"}
              className={`panel p-3 text-left transition-colors ${
                mode === "remote" ? "ring-1 ring-primary/60 bg-primary/5" : "hover:bg-muted/40"
              }`}
            >
              <div className="flex items-center gap-2 mb-1">
                <Server className="w-3.5 h-3.5 text-chart-3" />
                <span className="text-xs font-semibold">Live gateway</span>
              </div>
              <p className="text-[10.5px] text-muted-foreground leading-relaxed">
                Authoritative venue over socket.io — snapshot/delta market data, signed commands.
              </p>
            </button>
          </div>

          {/* live status */}
          <div className="panel p-3 space-y-2">
            <div className="flex items-center justify-between">
              <span className="text-[9px] uppercase tracking-wider text-muted-foreground">Session</span>
              <div className="flex items-center gap-1.5">
                <span className={`w-1.5 h-1.5 rounded-full ${statusColor} ${gateway.status === "live" ? "pulse-dot" : ""}`} />
                <span className="num text-[11px]">
                  {mode === "sim"
                    ? connected
                      ? "SIM · LIVE"
                      : "SIM · BOOTING"
                    : `${gateway.status.toUpperCase()}${connected ? "" : " · WAITING"}`}
                </span>
              </div>
            </div>
            {mode === "remote" && (
              <div className="grid grid-cols-3 gap-2 text-center">
                <Stat label="latency" value={gateway.latencyMs != null ? `${gateway.latencyMs} ms` : "—"} />
                <Stat label="reconnects" value={String(gateway.reconnects)} />
                <Stat label="endpoint" value={gateway.label} className="truncate" />
              </div>
            )}
            {mode === "remote" && gateway.error && (
              <p className="text-[10.5px] text-down/90 leading-relaxed break-words">{gateway.error}</p>
            )}
            {mode === "remote" && (
              <Button
                variant="outline"
                size="sm"
                className="h-7 w-full text-[11px]"
                onClick={() => void loadStats()}
                disabled={loadingStats}
              >
                {loadingStats ? <Loader2 className="w-3 h-3 animate-spin" /> : <RefreshCw className="w-3 h-3" />}
                Gateway stats
              </Button>
            )}
            {stats && (
              <div className="grid grid-cols-4 gap-2 text-center border-t border-hairline/60 pt-2">
                <Stat label="clients" value={String(stats.connections)} />
                <Stat label="keys" value={String(stats.keys)} />
                <Stat label="commands" value={stats.commandsProcessed.toLocaleString()} />
                <Stat label="venue evt" value={stats.venue.events.toLocaleString()} />
              </div>
            )}
          </div>

          {/* endpoint (remote) */}
          <div className="panel p-3 space-y-3">
            <span className="text-[9px] uppercase tracking-wider text-muted-foreground">Gateway endpoint</span>
            <div className="grid grid-cols-3 gap-2">
              <div className="col-span-2 space-y-1">
                <Label htmlFor="gw-origin" className="text-[10px] text-muted-foreground">
                  Origin (empty = same-origin proxy)
                </Label>
                <Input
                  id="gw-origin"
                  placeholder="https://venue.example.com"
                  value={origin}
                  onChange={(e) => setOrigin(e.target.value)}
                  className="h-8 text-[11px] font-mono"
                  autoComplete="off"
                  spellCheck={false}
                />
              </div>
              <div className="space-y-1">
                <Label htmlFor="gw-port" className="text-[10px] text-muted-foreground">
                  Proxy port
                </Label>
                <Input
                  id="gw-port"
                  placeholder="3031"
                  value={port}
                  onChange={(e) => setPort(e.target.value.replace(/[^0-9]/g, ""))}
                  className="h-8 text-[11px] font-mono"
                  autoComplete="off"
                />
              </div>
            </div>
            <p className="text-[10px] text-muted-foreground leading-relaxed">
              Direct origin for dedicated deployments; empty origin routes through the edge proxy
              (<span className="num">XTransformPort</span>) — the browser never sees a cross-origin socket.
            </p>
          </div>

          {/* credentials (remote) */}
          <div className="panel p-3 space-y-3">
            <div className="flex items-center justify-between">
              <span className="text-[9px] uppercase tracking-wider text-muted-foreground flex items-center gap-1.5">
                <KeyRound className="w-3 h-3" />
                API credentials (G-25)
              </span>
              <span className="text-[9px] text-muted-foreground/70">optional — demo gateways run auth-off</span>
            </div>
            <div className="grid grid-cols-2 gap-2">
              <div className="space-y-1">
                <Label htmlFor="gw-key" className="text-[10px] text-muted-foreground">
                  Key ID
                </Label>
                <Input
                  id="gw-key"
                  placeholder="poc-…"
                  value={keyId}
                  onChange={(e) => setKeyId(e.target.value)}
                  className="h-8 text-[11px] font-mono"
                  autoComplete="off"
                  spellCheck={false}
                />
              </div>
              <div className="space-y-1">
                <Label htmlFor="gw-secret" className="text-[10px] text-muted-foreground">
                  Secret
                </Label>
                <Input
                  id="gw-secret"
                  type="password"
                  placeholder="••••••••"
                  value={secret}
                  onChange={(e) => setSecret(e.target.value)}
                  className="h-8 text-[11px] font-mono"
                  autoComplete="off"
                />
              </div>
            </div>
            <div className="flex items-center gap-2">
              <Button
                variant="outline"
                size="sm"
                className="h-7 text-[11px]"
                onClick={() => void provision()}
                disabled={provisioning}
              >
                {provisioning ? <Loader2 className="w-3 h-3 animate-spin" /> : <ShieldCheck className="w-3 h-3" />}
                Provision key
              </Button>
              <Button
                variant="outline"
                size="sm"
                className="h-7 text-[11px]"
                onClick={() => {
                  navigator.clipboard?.writeText(`${keyId}\n${secret}`);
                  toast.success("Credentials copied");
                }}
                disabled={!keyId || !secret}
              >
                <Copy className="w-3 h-3" />
                Copy
              </Button>
              <div className="flex-1" />
              <Button
                variant="ghost"
                size="sm"
                className="h-7 text-[11px] text-muted-foreground"
                onClick={() => {
                  setKeyId("");
                  setSecret("");
                  setCredentials(null);
                }}
                disabled={!keyId && !secret}
              >
                <Trash2 className="w-3 h-3" />
                Clear
              </Button>
            </div>
            <p className="text-[10px] text-muted-foreground leading-relaxed">
              The secret is held in <span className="num">sessionStorage</span> — it dies with the tab and is
              never written to disk. Signing is HMAC-SHA256 over
              <span className="num"> key|nonce|method|path|body_hash</span> with strictly-increasing nonces.
            </p>
          </div>

          {/* actions */}
          <div className="flex items-center gap-2">
            <Button
              size="sm"
              className="h-8 text-[11px] flex-1"
              onClick={() =>
                connectRemote({ origin: origin.trim() || undefined, port: Number(port) || 3031 }, keyId && secret ? { key_id: keyId, secret } : null)
              }
              disabled={mode === "remote" && gateway.status === "connecting"}
            >
              <Server className="w-3.5 h-3.5" />
              Connect gateway
            </Button>
            <Button
              variant="outline"
              size="sm"
              className="h-8 text-[11px]"
              onClick={() => disconnectVenue()}
            >
              <Unplug className="w-3.5 h-3.5" />
              Disconnect
            </Button>
          </div>
        </div>
      </DialogContent>
    </Dialog>
  );
});

function Stat({ label, value, className }: { label: string; value: string; className?: string }) {
  return (
    <div className="min-w-0">
      <p className="text-[8.5px] uppercase tracking-wider text-muted-foreground/70 truncate">{label}</p>
      <p className={`num text-[11px] truncate ${className ?? ""}`}>{value}</p>
    </div>
  );
}
