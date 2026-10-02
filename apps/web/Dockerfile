# ── build ─────────────────────────────────────────────────────────────────
FROM oven/bun:1 AS build
WORKDIR /repo
COPY package.json bun.lock turbo.json ./
COPY packages/typescript-config/package.json packages/typescript-config/
COPY packages/types/package.json packages/types/
COPY packages/sim-engine/package.json packages/sim-engine/
COPY packages/api-client/package.json packages/api-client/
COPY packages/ui/package.json packages/ui/
COPY apps/web/package.json apps/web/
RUN bun install --frozen-lockfile

COPY packages packages
COPY apps/web apps/web
# NEXT_PUBLIC_* vars are inlined at build time — expose as a build arg.
ARG NEXT_PUBLIC_VENUE_GATEWAY_URL=""
ENV NEXT_TELEMETRY_DISABLED=1 NEXT_PUBLIC_VENUE_GATEWAY_URL=${NEXT_PUBLIC_VENUE_GATEWAY_URL}
RUN bun run --filter=@perp/web build

# ── runtime ───────────────────────────────────────────────────────────────
FROM oven/bun:1
WORKDIR /repo
COPY --from=build /repo /repo
EXPOSE 3000
ENV NODE_ENV=production NEXT_TELEMETRY_DISABLED=1
CMD ["bun", "apps/web/node_modules/.bin/next", "start", "-p", "3000"]
