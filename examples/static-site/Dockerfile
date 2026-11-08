# Serves a prebuilt static site with nginx. The site is built before `docker build` runs (by the
# aphid action in release.yml, or any other generator) into `dist/`.
#
# nginx-unprivileged runs as a non-root user and listens on 8080, which suits rootless podman.
# Pin a specific tag (e.g. `1.x.y-alpine`) if you want the base image to change only when you
# change it.
FROM docker.io/nginxinc/nginx-unprivileged:stable-alpine

COPY default.conf /etc/nginx/conf.d/default.conf
COPY dist/ /usr/share/nginx/html/
