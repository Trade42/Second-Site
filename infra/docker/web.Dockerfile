FROM node:22-slim
RUN corepack enable
WORKDIR /repo
COPY package.json pnpm-workspace.yaml pnpm-lock.yaml ./
COPY packages/contracts packages/contracts
COPY apps/web apps/web
RUN pnpm install --frozen-lockfile && pnpm --filter @second-site/web build
WORKDIR /repo/apps/web
EXPOSE 3000
CMD ["pnpm", "start"]
