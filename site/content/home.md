# Ship by pushing a release

hoster is a self-hosted deployment orchestrator for a single VPS. GitHub Actions builds your image
and pushes it to ghcr; a minute later the new version serves traffic on its hostname with a valid
certificate, and no request was dropped during the switch.

```sh
curl -fsSL https://github.com/LHelge/hoster/releases/latest/download/install.sh | sudo bash
```

---

# One binary, nothing else to run

hoster is one statically linked Rust binary under systemd. It embeds the reverse proxy (Pingora),
ACME certificates, the deploy engine, the webhook receiver, a registry poller and a web admin UI
with GitHub login. Containers run under rootless podman, each stack on its own network.

---

# Deploys by digest, without downtime

A release workflow posts the pushed image's digest to hoster, authenticated with the job's GitHub
OIDC token, so no deploy secret lives in CI. hoster starts the new container next to the old one,
health-checks it, swaps the route atomically and drains the old one.

---

[Read the docs →](/wiki/overview/)
