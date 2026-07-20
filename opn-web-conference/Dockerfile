# Build the SPA, then ship it with the tiny Node prod server (deploy/server.mjs).
# Two stages so the runtime image carries no toolchain and no node_modules — the
# servers are stdlib-only.

FROM node:22-alpine AS build
WORKDIR /src
# Manifests first for layer caching, then sources needed to build the app workspace.
COPY package.json tsconfig.base.json ./
COPY packages ./packages
COPY app ./app
RUN npm install
# The app is same-origin (no build-time Core URL), so no build args are needed.
RUN npm run build --workspace app

FROM node:22-alpine AS runtime
WORKDIR /app
ENV NODE_ENV=production PORT=8080 WEB_ROOT=/app/app/dist
# Only the built assets + the stdlib-only servers. No node_modules.
COPY --from=build /src/app/dist ./app/dist
COPY deploy/server.mjs ./deploy/server.mjs
COPY dev-auth/join.mjs ./dev-auth/join.mjs
USER node
EXPOSE 8080
# Rollout gate — Coolify/Traefik hold traffic until this passes. Node 22 has fetch.
HEALTHCHECK --interval=10s --timeout=3s --start-period=10s --retries=5 \
  CMD node -e "fetch('http://127.0.0.1:8080/healthz').then(r=>process.exit(r.ok?0:1)).catch(()=>process.exit(1))"
CMD ["node", "deploy/server.mjs"]
