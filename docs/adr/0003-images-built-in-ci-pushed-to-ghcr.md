# ADR-0003: Apps are images built in GitHub CI and pushed to ghcr

- Status: Accepted
- Date: 2026-11-07

## Context

Building on the VPS couples deploy latency and reliability to the server's CPU, disk and network,
and puts toolchains and build secrets on the production host.

## Decision

Applications are built into container images by GitHub Actions and pushed to the GitHub Container
Registry (`ghcr.io`). hoster only pulls images, always by digest.

## Alternatives considered

- **Build on the server** (Dockerfile/buildpacks/Nixpacks, Coolify-style): rejected — puts build
  load and toolchains on production, slower, non-reproducible across hosts.
- **Other registries** (Docker Hub, self-hosted registry): ghcr is co-located with the source and
  CI, uses the same GitHub identity, and needs no extra infrastructure. Third-party images from
  other registries can still appear in stacks, resolved to a digest at apply (ADR-0025); only the webhook/poller
  automation targets ghcr.
- **Ship tarballs/binaries instead of images**: loses the isolation and packaging guarantees we rely
  on.

## Consequences

- hoster needs pull credentials for private ghcr packages (ADR-0025).
- The registry is a runtime dependency for deploys (not for already-running apps).
- Reproducibility: the digest that CI built is exactly what runs.
