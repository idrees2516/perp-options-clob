# ── build ─────────────────────────────────────────────────────────────────
FROM oven/bun:1 AS build
WORKDIR /repo
COPY package.json bun.lock turbo.json ./
COPY packages/typescript-config/package.json packages/typescript-config/
COPY packages/types/package.json packages/types/
COPY packages/sim-engine/package.json packages/sim-engine/
COPY packages/api-client/package.json packages/api-client/
COPY apps/gateway/package.json apps/gateway/
RUN bun install --frozen-lockfile

COPY packages packages
COPY apps/gateway apps/gateway
RUN bun run --filter=@perp/gateway typecheck

# ── runtime ───────────────────────────────────────────────────────────────
FROM oven/bun:1
WORKDIR /repo
COPY --from=build /repo /repo
ENV GATEWAY_HOST=0.0.0.0
EXPOSE 3031
HEALTHCHECK --interval=15s --timeout=3s CMD bun -e "fetch('http://127.0.0.1:3031/healthz').then(r => process.exit(r.ok ? 0 : 1)).catch(() => process.exit(1))"
CMD ["bun", "apps/gateway/src/index.ts"]
