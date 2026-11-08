# Deploying from CI

This guide is for an app repository that wants its GitHub Actions release workflow to deploy to
hoster. The workflow builds and pushes the image, takes the digest from the push step, asks
GitHub for an OIDC token meant for hoster, and posts the digest to hoster's deploy webhook.
No deploy secret is stored in CI: the token proves which repository, ref, workflow and
environment the request comes from, and the stack file says which of those may deploy
(ADR-0003, ADR-0042, ADR-0047).

A complete workflow to copy is in
[`examples/github-actions/release.yml`](../examples/github-actions/release.yml).

## 1. One-time setup

### 1.1 The stack file names the image repository

The service's `image` is the repository the workflow pushes to, **without a tag**
(stack-format.md §2.2):

```toml
[services.api]
image = "ghcr.io/acme/notes-api"
```

hoster checks every digest it receives against this repository in the registry before it
deploys, so a webhook can never start an image from somewhere else. Image names on ghcr are
lowercase; if the GitHub owner or repository name has capitals, lowercase them in both places.

If the package is private, enter a classic PAT with `read:packages` under
**Settings → Registries** (ADR-0025). The OIDC token does not authorize pulls.

### 1.2 Find the repository id

hoster matches the repository by its numeric id, which stays the same when the repository is
renamed or transferred. Any of these gives it:

- `gh api repos/acme/notes --jq .id`
- `${{ github.repository_id }}` inside any workflow of the repository
- the **Refused webhook deploys** table on the stack page: send one deploy before adding the
  rule (the attempt is refused with `403`) and copy the `repository_id` column.

### 1.3 Allow the repository in the stack's `[deploy]` rule

Without a `[deploy]` section, a stack can only be deployed from the UI. Add one and apply the
new revision in the UI (stack-format.md §2.5):

```toml
[deploy]
repository_id = 123456789          # required
refs = ["refs/tags/v*"]            # optional: globs on the token's `ref`
environment = "production"         # optional: exact match on the token's `environment`
```

- `refs` uses `*` within one path segment and `**` across segments. `refs/tags/v*` allows
  release tags; `refs/heads/main` allows pushes to `main`. At least one pattern must match.
- `environment` is only present in the token when the job declares `environment:` (§1.4).
- `job_workflow_ref` (exact match) is for pinning a **reusable** workflow, for example
  `acme/workflows/.github/workflows/deploy.yml@refs/heads/main`. For a workflow that lives in the
  app repository, the claim ends in the ref that triggered it (`…/release.yml@refs/tags/v1.4.2`),
  so it changes with every release; use `refs` and `environment` instead.

### 1.4 Protect the deploy with a GitHub environment (recommended)

The `[deploy]` rule decides which tokens hoster accepts. A GitHub environment decides which jobs
can get such a token in the first place. Together they keep a pushed branch or an edited workflow
from deploying production (ADR-0047).

In the app repository, under **Settings → Environments → New environment**, create
`production` and set:

- **Deployment branches and tags:** *Selected branches and tags*, with the tag pattern `v*`
  (and nothing else). Jobs from other refs cannot use the environment.
- **Required reviewers** (optional): someone approves each release before the job runs. Tick
  **Prevent self-review** if the team is big enough.
- **Allow administrators to bypass** off, if the bypass should not be available.

Then let the release job use it (`environment: production`, see the example) and require it in
hoster with `environment = "production"`. Also protect who can create `v*` tags, with a tag
ruleset under **Settings → Rules → Rulesets**.

## 2. The release workflow

### 2.1 Permissions

```yaml
permissions:
  contents: read
  packages: write   # push to ghcr with GITHUB_TOKEN
  id-token: write   # request an OIDC token for hoster
```

Without `id-token: write`, the `ACTIONS_ID_TOKEN_REQUEST_*` variables are not set and the token
request fails.

### 2.2 Build, push and capture the digest

`docker/build-push-action` pushes the image and outputs its digest as `steps.<id>.outputs.digest`
(`sha256:…`). That digest, not a tag, is what hoster deploys, so a tag moved later changes
nothing:

```yaml
- id: push
  uses: docker/build-push-action@v7
  with:
    context: .
    push: true
    platforms: linux/amd64   # the VPS's architecture
    tags: ${{ steps.meta.outputs.tags }}
```

### 2.3 Request the token with hoster's audience

The token's `aud` must equal hoster's configured audience: `https://<admin_host>` by default, or
`[webhook] audience` in `hoster.toml` (`HOSTER_WEBHOOK__AUDIENCE`). Request it from the runner's
token endpoint with that audience, URL-encoded, and mask it in the log:

```sh
audience=$(jq -rn --arg a "$HOSTER_AUDIENCE" '$a|@uri')
token=$(curl -fsS -H "Authorization: bearer $ACTIONS_ID_TOKEN_REQUEST_TOKEN" \
  "$ACTIONS_ID_TOKEN_REQUEST_URL&audience=$audience" | jq -r .value)
echo "::add-mask::$token"
```

`core.getIDToken(audience)` from `actions/github-script` does the same. A token is valid for a
few minutes and hoster accepts each one **once**: request a fresh one for every POST, including
retries.

### 2.4 Send the webhook

```sh
body=$(jq -nc --arg d "$DIGEST" --arg t "$GITHUB_REF_NAME" \
  '{stack:"notes", images:{api:$d}, tag:$t}')
curl -sS --fail-with-body -X POST "$HOSTER_URL" \
  -H 'Content-Type: application/json' \
  -H "Authorization: Bearer $token" \
  --data "$body"
```

`HOSTER_URL` is `https://<admin_host>/hooks/deploy`.

The body (at most 64 KiB):

| Field | Required | Meaning |
|---|---|---|
| `stack` | yes | The stack's `name`. |
| `images` | yes | Service name → digest (`sha256:` and 64 hex digits). Several services deploy together as **one** deployment; services left out keep what they run. |
| `tag` | no | Informational, shown in the deploy history. Any text up to 256 characters. |

Unknown fields are refused, and `stack_file` is reserved: stack files are applied in the UI
(ADR-0027). Repository, commit, ref, run id and actor in the deploy history come from the
verified token, not from the body.

### 2.5 Answers

| Status | Body | Meaning |
|---|---|---|
| `202` | `{"result":"accepted","deployment_id":"8","state":"Received"}` | A deployment started, or was queued behind the running one (`"state":"Queued"`, ADR-0040). Follow it at `/stacks/<stack>/deployments/<id>`. |
| `200` | `{"result":"noop","deployment_id":"7"}` | The stack already runs these digests with this configuration (ADR-0029). |
| `400` | `{"error":…}` | The body is not valid JSON of the shape above. |
| `401` | `{"error":"invalid token"}` | Missing, malformed, expired, wrong audience, wrong signature, or already used. The reason is only in hoster's log. |
| `403` | `{"error":"not allowed to deploy this stack"}` | The stack has no `[deploy]` rule, does not exist, or the rule does not match the token. For a stack that exists, the attempt and its claims are listed on its page. |
| `409` | `{"error":"the stack cannot be deployed right now"}` | The stack is being removed. |
| `413` | `{"error":…}` | The body is larger than 64 KiB. |
| `422` | `{"error":…}` | Invalid stack name, empty `images`, bad digest, invalid `tag`, a `stack_file` field, unknown service, a service that is not CI-built, or a digest that is not in the service's repository. |
| `502` | `{"error":…}` | hoster could not reach the registry to check a digest. |
| `503` | `{"error":"cannot verify tokens right now"}` | GitHub's key set is unreachable; retry with a new token. |
| `500` | `{"error":"internal error"}` | hoster failed internally; the cause is in its log. |

The `202` means the deployment was accepted, not that it went live: a failed health check aborts
it and leaves the old containers serving (ADR-0017). Watch the deployment page or the stack's
history.

## 3. What hoster checks in the token

hoster verifies the RS256 signature against GitHub's published keys, then `iss`
(`https://token.actions.githubusercontent.com`), `aud`, `exp`/`nbf`/`iat` with 60 s of clock
skew, and that the `jti` has not been used before. Only then is the body read and the
`[deploy]` rule applied. A token from a tag push of `acme/notes` whose release job runs in the
`production` environment carries claims like these (hoster's webhook tests sign exactly this
example):

<!-- oidc-claims-example: src/web/webhook/tests.rs reads the next code block -->
```json
{
  "jti": "example-0b3f1c2e",
  "sub": "repo:acme/notes:environment:production",
  "environment": "production",
  "aud": "https://hoster.example.com",
  "ref": "refs/tags/v1.4.2",
  "sha": "0f3c5e1a9d2b7c4e8f6a1b3d5c7e9f0a2b4c6d8e",
  "repository": "acme/notes",
  "repository_owner": "acme",
  "actor_id": "12",
  "repository_visibility": "private",
  "repository_id": "123456789",
  "repository_owner_id": "65",
  "run_id": "123456",
  "run_number": "10",
  "run_attempt": "1",
  "runner_environment": "github-hosted",
  "actor": "octocat",
  "workflow": "Release",
  "head_ref": "",
  "base_ref": "",
  "event_name": "push",
  "ref_type": "tag",
  "workflow_ref": "acme/notes/.github/workflows/release.yml@refs/tags/v1.4.2",
  "job_workflow_ref": "acme/notes/.github/workflows/release.yml@refs/tags/v1.4.2",
  "iss": "https://token.actions.githubusercontent.com",
  "nbf": 1762520000,
  "iat": 1762520000,
  "exp": 1762520300
}
```

| `[deploy]` field | Claim |
|---|---|
| `repository_id` | `repository_id` |
| `refs` | `ref` |
| `environment` | `environment` |
| `job_workflow_ref` | `job_workflow_ref` |

The stack page's refused-attempts table shows `repository`, `repository_id`, `ref`,
`environment` and `workflow_ref` of each refused token, never the token itself.

## 4. Troubleshooting

- **The token request fails:** the job lacks `permissions: id-token: write`, or the workflow runs
  from a fork's pull request, which never gets an OIDC token.
- **`401` every time:** compare the audience the job requests with hoster's
  (`https://<admin_host>` unless `[webhook] audience` is set), and check the server clock
  (`hoster doctor`). A retried POST with the same token is a replay and is refused.
- **`403`:** open the stack page and compare the refused attempt's claims with the `[deploy]`
  rule. A job without `environment:` has no `environment` claim; a branch push has
  `refs/heads/…`, not `refs/tags/…`.
- **`422` "not in its repository":** the image was pushed to a different repository than the
  service's `image`, or the name differs in case.
- **Accepted, then failed:** the deployment page shows the step and the container's logs; a pull
  failure on a private package means the registry PAT is missing or lacks `read:packages`.
