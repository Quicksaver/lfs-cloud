# Server Configuration

`lfs-cloud.yml` is private server-owned configuration. Do not commit it to the
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
  metadata_path: ./.lfs-cloud/metadata.sqlite3

repository_providers:
  github-main:
    type: github
    api_url: https://api.github.com
    oauth_client_id: ${LFS_CLOUD_GITHUB_CLIENT_ID}
    oauth_client_secret: ${LFS_CLOUD_GITHUB_CLIENT_SECRET}

storage_providers:
  drive-personal:
    type: google_drive
    credentials_ref: google-drive-personal
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
lfs-cloud serve --config ./lfs-cloud.yml --host 0.0.0.0 --port 8080
```

`public_url` is the URL embedded in Git LFS batch action responses. Set it to
the address clients can actually reach. Plaintext LAN mode exposes OAuth codes,
LFS credentials, and object bytes to the network, so it requires the explicit
`allow_insecure_http: true` development opt-in. Prefer HTTPS through trusted TLS
termination. Client commands using this LAN URL also require
`--allow-insecure-http`.

## Google Drive Credentials

`credentials_ref` is not the credential itself. It tells the server which
environment variable contains a flat OAuth JSON value:

```json
{
  "client_id": "google-oauth-client-id",
  "client_secret": "google-oauth-client-secret",
  "refresh_token": "google-refresh-token"
}
```

For a bare reference such as `google-drive-personal`, the current loader reads:

```text
LFS_CLOUD_GOOGLE_DRIVE_CREDENTIAL_GOOGLE_DRIVE_PERSONAL
```

Bare references are converted by prefixing
`LFS_CLOUD_GOOGLE_DRIVE_CREDENTIAL_`, uppercasing ASCII letters, and replacing
`-` with `_`. They may contain only ASCII letters, digits, `_`, or `-`.

You can name the environment variable explicitly:

```yaml
storage_providers:
  drive-personal:
    type: google_drive
    credentials_ref: env:LFS_CLOUD_DRIVE_PERSONAL_JSON
    root_folder_id: 012345abcdef
```

The MVP Drive scope is:

```text
https://www.googleapis.com/auth/drive.file
```

The configured `root_folder_id` must be a folder that the app credential can
access and create children in. Git users never receive Drive tokens or direct
Drive access.

## Durable LFS Sessions

Production `serve` processes persist unexpired local LFS sessions in the
configured metadata database so Git credentials continue to work across server
restarts. The database contains only the local token's SHA-256 digest. The
private GitHub OAuth token and the session identity, scopes, and timestamps are
authenticated together with AES-256-GCM before persistence.

The dedicated encryption key is derived from the configured GitHub
`oauth_client_secret`; the secret itself is never stored in SQLite. Keep that
secret stable while sessions are active. Rotating it intentionally makes
existing rows unreadable, so allow current sessions to expire or intentionally
remove them before changing it.

## Metadata Path

If `server.metadata_path` is omitted, the server stores SQLite metadata at:

```text
<config directory>/.lfs-cloud/metadata.sqlite3
```

Relative `metadata_path` values resolve against the directory containing the
config file.

## Validation Rules

- `server.public_url` and GitHub `api_url` must be HTTP(S) URLs without
  credentials, query strings, or fragments. They must use HTTPS unless the
  host is an exact IPv4/IPv6 loopback address. Non-loopback HTTP requires the
  explicit development-only `server.allow_insecure_http: true` setting.
- Google credential JSON `token_uri` values, and custom Google Drive API base
  URLs used by embedded runtimes or tests, follow the same URL rules and must
  use HTTPS except for loopback HTTP endpoints.
- Provider IDs and storage IDs must start with an ASCII letter or digit and use
  only ASCII letters, digits, `_`, or `-`.
- Repository route components must be safe path segments. Repository names must
  not include `.git`.
- GitHub repository mappings must include a positive numeric
  `provider_repository_id` matching GitHub's stable repository ID.
- Every repository mapping must reference configured repository and storage
  providers.
- Duplicate repository IDs and duplicate generated route paths are rejected.
