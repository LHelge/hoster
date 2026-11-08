# The documentation site as an nginx image, built and pushed by .github/workflows/release.yml
# after aphid has rendered the site into site/dist/. The same setup as examples/static-site/
# (nginx-unprivileged, port 8080, /healthz for hoster's default probe).
FROM docker.io/nginxinc/nginx-unprivileged:stable-alpine

COPY default.conf /etc/nginx/conf.d/default.conf
COPY dist/ /usr/share/nginx/html/
