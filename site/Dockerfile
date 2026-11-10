# The documentation site as an nginx image, built and pushed by .github/workflows/release.yml
# after aphid has rendered the site into site/dist/. The same setup as examples/static-site/
# (nginx-unprivileged, port 8080, /healthz for hoster's default probe).
FROM docker.io/nginxinc/nginx-unprivileged:stable-alpine

COPY default.conf /etc/nginx/conf.d/default.conf
COPY dist/ /usr/share/nginx/html/
# The stack file JSON Schema at the URL stack files name in `#:schema` (aphid copies static/ to
# dist/static/; `hoster stack schema` regenerates it).
COPY dist/static/stack.schema.json /usr/share/nginx/html/stack.schema.json
