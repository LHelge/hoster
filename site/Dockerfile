# The documentation site, served by aphid-server: built and pushed by .github/workflows/release.yml
# after aphid has rendered the site into site/dist/. The same setup as examples/static-site/.
# aphid-server listens on $PORT (8080), answers /healthz for hoster's default probe and serves
# 404.html for missing pages. Keep the tag equal to APHID_VERSION in release.yml (its `site` job
# checks it), so the server matches the aphid that built the site.
FROM ghcr.io/lhelge/aphid-server:0.5.1

COPY dist /site
# The stack file JSON Schema at the URL stack files name in `#:schema` (aphid copies static/ to
# dist/static/; `hoster stack schema` regenerates it). The glob takes the precompressed .br/.gz
# siblings along.
COPY dist/static/stack.schema.json* /site/
