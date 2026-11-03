# The API

Everything the dashboard and the CLI do goes through a JSON API under `/api/v1`, which you
can use too, for scripts and integrations.

## The reference

Each instance serves its API's OpenAPI 3.1 document at `/api/v1/openapi.json`, without
signing in:

```sh
curl https://spinikus.example.com/api/v1/openapi.json
```

Load it into any OpenAPI tool (Swagger UI, Redocly, Postman, Insomnia) to browse every
operation and its fields, or to generate a client. The document for the newest code is also
in the repository, as
[`docs/openapi.json`](https://github.com/cfowlerdev/spinikus/blob/main/docs/openapi.json).

## Authenticating

Send an API token as a bearer token:

```sh
curl -H "Authorization: Bearer spk_…" https://spinikus.example.com/api/v1/projects
```

Create tokens under **Settings → API tokens** or with `spinikus token create` (see
[API tokens](operating/access.md#api-tokens)). `POST /api/v1/login` with an email and
password also returns one, as `spinikus login` does.

## Conventions

- **Resources** are JSON objects with an `id`, such as `svc_01J9ZX4Q8V6T3N2M1K0H9G8F7E`. The
  prefix says what it is (`prj_`, `env_`, `svc_`, `dep_`, …).
- **Errors** are [problem details](https://www.rfc-editor.org/rfc/rfc9457) with a stable
  `code` to match on, such as `not_found`, `forbidden`, `invalid` (with the `field` at fault),
  `conflict` or `revision_mismatch`.
- **Updates need the revision** they change. Single resources come with an `ETag` header
  holding their revision; send it back as `If-Match` with a `PATCH` or `PUT`. If someone
  changed the resource since you read it, the update is refused with `412`, so you never
  overwrite a change you haven't seen.
- **Lists are pages**: `?limit=` (50 by default, at most 100), and the answer's
  `next_cursor`, passed as `?after=` for the next page, is `null` on the last one.
- **Secrets are write-only**: secret variables come back without a value.

## Example: deploy and wait

```sh
TOKEN=spk_…
API=https://spinikus.example.com/api/v1

# Deploy a service.
deployment=$(curl -fsS -X POST -H "Authorization: Bearer $TOKEN" \
    "$API/services/svc_01J…/deployments" | jq -r .id)

# Follow it as server-sent events until it ends.
curl -fsSN -H "Authorization: Bearer $TOKEN" "$API/deployments/$deployment/stream"
```

## Terminals

`GET /api/v1/services/{id}/terminal` opens a terminal in the service's running container as a
WebSocket, as `spinikus shell` does: binary messages are the terminal's bytes both ways, and
the server ends with a JSON text message, `{"type":"exit","code":0}` or
`{"type":"error","message":"…"}`. Send `{"type":"resize","rows":40,"columns":120}` when the
window changes. The query takes `rows`, `columns`, `deployment_id`, and `command` as a JSON
array, such as `["psql","-U","app"]`.

## Webhooks

Git hosts deliver pushes to `/hooks/git/<provider>/<webhook ID>`, outside `/api/v1`, signed
with the webhook's secret instead of a token. See [Deploy on push](apps/deploy-on-push.md).
