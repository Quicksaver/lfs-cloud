# Server Configuration

`lfscloud.yml` is private server-owned configuration. Do not commit it to the
Git repositories being served: it contains repository-to-storage routing,
credential references, Drive folder IDs, and metadata database paths.

The committed repository-side `.lfsconfig` should contain only the LFS Cloud
endpoint for that repository, for example:

```ini
[lfs]
    url = http://127.0.0.1:8080/github.com/octo-org/assets.git/info/lfs
```

## Minimal Local Config

```yaml
server:
  host: 127.0.0.1
  port: 8080
  public_url: http://127.0.0.1:8080
  max_batch_objects: 100
  max_provider_calls: 16
  max_concurrent_requests: 64
  max_concurrent_uploads: 8
  max_concurrent_uploads_per_user: 2
  metadata_path: ./.lfscloud/metadata.sqlite3

repository_providers:
  github-main:
    type: github
    api_url: https://api.github.com
    personal_access_token: ${LFS_CLOUD_GITHUB_PAT}

storage_providers:
  drive-personal:
    type: google_drive
    credentials:
      type: gcloud
      config_dir: ${HOME}/.config/lfscloud/gcloud-drive
    root_folder_id: 012345abcdef
    display_name: Personal Drive LFS

repositories:
  - id: github-main:octo-org/assets
    repo_provider: github-main
    host: github.com
    owner: octo-org
    name: assets
    provider_repository_id: '123456789'
    storage_provider: drive-personal
```

With that mapping, the repository LFS URL is:

```text
http://127.0.0.1:8080/github.com/octo-org/assets.git/info/lfs
```

Repository `name` omits the `.git` suffix because the route adds it.
`provider_repository_id` is GitHub's immutable numeric repository ID, available
with `gh api repos/OWNER/REPOSITORY --jq .id`. LFS Cloud verifies this value
before every permission check so a renamed, transferred, deleted, or reused
`owner/name` cannot silently switch the mapping to another repository.

`personal_access_token` is the only supported GitHub authentication mode. Prefer a fine-grained PAT
limited to the listed repositories with repository Metadata read access, and
run `lfscloud login --server URL` to exchange it for a short-lived local
LFS token. The PAT is never written to Git's credential helper.

## LAN Config

Use a LAN bind only on a trusted network:

```yaml
server:
  host: 0.0.0.0
  port: 8080
  public_url: http://192.168.1.25:8080
  allow_insecure_http: true
```

The `serve` command can also override the bind address and port:

```bash
lfscloud serve --config ./lfscloud.yml --host 0.0.0.0 --port 8080
```

`public_url` is the URL embedded in Git LFS batch action responses. Set it to
the address clients can actually reach. Plaintext LAN mode exposes the GitHub
PAT, LFS credentials, and object bytes to the network, so it requires the explicit
`allow_insecure_http: true` development opt-in. Prefer HTTPS through trusted TLS
termination. Client commands using this LAN URL also require
`--allow-insecure-http`.

## Provider And Request Work Limits

`server.max_batch_objects` bounds the number of object entries accepted in one
Git LFS batch request. The default is `100`. Duplicate entries still count
toward this request/response limit, while their storage lookups are collapsed
to one call per distinct OID and size.

`server.max_provider_calls` bounds concurrent repository-provider and
storage-provider work across the entire server process. The default is `16`.
Successful repository permission decisions are reused only for the same local
session, repository, and operation for 15 seconds, which lets a batch action
complete without an immediate duplicate GitHub check. Google Drive access
tokens are cached until shortly before their reported expiry, with concurrent
requests to `gcloud` collapsed into one token request.

`server.max_concurrent_requests` bounds active HTTP request handling across all
server routes. The default is `64`. Excess requests receive HTTP 503 with a
one-second `Retry-After` value instead of joining an unbounded queue. An
authenticated Git LFS batch body also has a 15-second idle timeout and a
60-second total read deadline, so a slow-dripping client cannot retain a
request slot indefinitely.

`server.max_concurrent_uploads` bounds uploads that retain local staging
resources across body ingestion and backend storage. The default is `8`.
`server.max_concurrent_uploads_per_user` applies a second limit, defaulting to
`2`, keyed by stable repository-provider user ID when available. Excess upload
staging receives HTTP 503 with `Retry-After: 1`. Each admitted upload also
reserves its declared bytes atomically against aggregate temp-directory
capacity until its temporary file is released. The live filesystem check and
64 MiB free-space headroom remain in effect as secondary guardrails.

## Google Drive Credentials

The recommended local configuration uses the Google Cloud CLI to mint
short-lived tokens from an isolated Application Default Credentials directory:

```yaml
storage_providers:
  drive-personal:
    type: google_drive
    credentials:
      type: gcloud
      config_dir: ${HOME}/.config/lfscloud/gcloud-drive
      # executable: /absolute/path/to/gcloud
    root_folder_id: 012345abcdef
```

Install `gcloud`, create the directory, and generate the credentials once with
the browser flow documented in the README. The directory must contain
`application_default_credentials.json`. LFS Cloud invokes `gcloud` at runtime,
so the executable must remain installed and accessible to the server user.

The MVP Drive scope is:

```text
https://www.googleapis.com/auth/drive.file
```

The configured `root_folder_id` must be a folder that the app credential can
access and create children in. Git users never receive Drive tokens or direct
Drive access.

`lfscloud serve` asks `gcloud` for an ADC access token for every configured
Drive provider, then performs a non-mutating metadata probe that confirms the
root is a live folder with child-write capability. These checks complete before
the HTTP listener binds; a missing/invalid credential or unusable root prevents
the server from reporting readiness.

## Durable LFS Sessions

Production `serve` processes persist unexpired local LFS sessions in the
configured metadata database so Git credentials continue to work across server
restarts. The database contains only the local token's SHA-256 digest. The
private GitHub PAT and the session identity, scopes, and timestamps are
authenticated together with AES-256-GCM before persistence.

The dedicated encryption key is derived from the configured GitHub PAT; the PAT
itself is never stored in SQLite. Keep it
stable while sessions are active. Rotating it intentionally makes existing
rows unreadable, so allow current sessions to expire or remove them first.

## Metadata Path

If `server.metadata_path` is omitted, the server stores SQLite metadata at:

```text
<config directory>/.lfscloud/metadata.sqlite3
```

Relative `metadata_path` values resolve against the directory containing the
config file.

The server creates an `upload-locks` directory beside the metadata database.
All LFS Cloud processes that can write to the same Google Drive root must use
the same metadata location. Those OS-backed object-keyed locks serialize the
final Drive existence check and upload across local processes and are released
automatically if a process exits. Cross-host writers are not supported by the
MVP. Lookup deterministically selects the smallest Drive file ID when an older
race has already left multiple otherwise exact object matches.

## Validation Rules

- `server.public_url`, GitHub `api_url`, and CLI `--server` route bases use the
  same validation policy. They must be HTTP(S) URLs without credentials, query
  strings, fragments, trailing slashes, whitespace, control characters,
  backslashes, or path dot segments. They must use HTTPS unless the host is an
  exact IPv4/IPv6 loopback address. Non-loopback HTTP requires the explicit
  development-only `server.allow_insecure_http: true` setting and the matching
  CLI `--allow-insecure-http` flag.
- `server.max_batch_objects`, `server.max_provider_calls`,
  `server.max_concurrent_requests`, `server.max_concurrent_uploads`, and
  `server.max_concurrent_uploads_per_user` must be greater than zero when
  configured. The per-user upload limit cannot exceed the process-wide upload
  limit.
- Custom Google Drive API base URLs used by embedded runtimes or tests must use
  HTTPS except for loopback HTTP endpoints.
- Provider IDs and storage IDs must start with an ASCII letter or digit and use
  only ASCII letters, digits, `_`, or `-`.
- Repository route components must be safe path segments. Repository names must
  not include `.git`.
- GitHub repository mappings must include a positive numeric
  `provider_repository_id` matching GitHub's stable repository ID.
- Every repository mapping must reference configured repository and storage
  providers.
- Duplicate repository IDs and duplicate generated route paths are rejected.
