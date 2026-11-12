# Serves a prebuilt static site with aphid-server (https://aphid.lhelge.se/wiki/aphid-server/).
# The site is built into `dist/` before `docker build` runs (by the aphid action in release.yml,
# or any other generator). The image runs as non-root, listens on $PORT (8080 by default, which
# hoster injects), answers /healthz and serves 404.html for missing pages.
FROM ghcr.io/lhelge/aphid-server:0.4.1
COPY dist /site
