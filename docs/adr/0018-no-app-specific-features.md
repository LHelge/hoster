# ADR-0018: No app-specific features

- Status: Accepted; OTLP is decoded by the receiver of ADR-0069
- Date: 2026-11-07

## Context

Tools like Coolify grow large through one-click databases, framework detection, buildpacks and
per-app integrations. Every such feature is code that has to be maintained and secured.

## Decision

hoster knows about images, stacks, services, routes, health checks, volumes and env. It knows
nothing about particular applications, frameworks, databases or protocols beyond HTTP.

## Alternatives considered

- **Templates or one-click apps**: users can share stack files instead. That needs no code in
  hoster.
- **Built-in database management** (backups, migrations): app or ops concern. Volume backups are
  generic (ADR-0030).
- **S3 or other protocol-aware routing**: handled by the generic routing model (ADR-0013).

## Consequences

- hoster stays small, and every feature applies to all apps.
- Users write a bit more stack config for common software.
