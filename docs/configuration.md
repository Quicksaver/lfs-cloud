# Server Configuration

`lfscloud.yml` is private server-owned configuration. Do not commit it to the Git repositories being served: it contains repository-to-storage routing, credential references, Drive folder IDs, and metadata database paths.

The committed repository-side `.lfsconfig` should contain only the LFS Cloud endpoint for that repository, for example:

```ini
[lfs]
    url = http://127.0.0.1:8080/github.com/octo-org/assets.git/info/lfs
```

## Manage Configuration From The CLI

Create `lfscloud.yml` and its `server` section before using the configuration commands. The commands manage the three provider and routing sections:

```text
lfscloud config repository  -> repository_providers
lfscloud config storage     -> storage_providers
lfscloud repository         -> repositories
```

Each resource supports `add`, `list`, and `remove`. With no entry flags, `add` prompts for a complete new entry or all values of an existing entry:

```bash
lfscloud --config ./lfscloud.yml config repository add
lfscloud --config ./lfscloud.yml config storage add
lfscloud --config ./lfscloud.yml repository add
```

Press Enter to accept a displayed default or retain an existing value. If any entry flag is supplied, the command is non-interactive. A new entry then requires every required field, while an existing `--id` accepts only the fields to update. Replaying the same update succeeds without changing the file.

The complete flag-based forms for the currently supported providers are:

```bash
lfscloud --config ./lfscloud.yml config repository add \
  --id github-main \
  --type github \
  --api-url https://api.github.com

lfscloud --config ./lfscloud.yml config storage add \
  --id drive-personal \
  --type google-drive \
  --credentials-type gcloud \
  --config-dir '${HOME}/.config/lfscloud/gcloud-drive' \
  --executable gcloud \
  --root-folder-id YOUR_DRIVE_FOLDER_ID \
  --display-name 'Personal Drive LFS'

lfscloud --config ./lfscloud.yml repository add \
  --id github-main:OWNER/REPOSITORY \
  --repo-provider github-main \
  --host github.com \
  --owner OWNER \
  --name REPOSITORY \
  --provider-repository-id 123456789 \
  --storage-provider drive-personal
```

Single-quote environment references so the shell does not expand them before they are written.

Every environment variable referenced anywhere in the config must be set while a changed document is validated, just as it must be set when the server loads that config.

Partial updates identify the existing entry by its stable ID:

```bash
lfscloud --config ./lfscloud.yml config storage add \
  --id drive-personal \
  --display-name 'Archive Drive'

lfscloud --config ./lfscloud.yml repository add \
  --id github-main:OWNER/REPOSITORY \
  --storage-provider drive-archive
```

List commands print tab-separated, script-friendly summaries. A legacy session-secret fallback is reported only as configured or absent; its value is never printed:

```bash
lfscloud --config ./lfscloud.yml config repository list
lfscloud --config ./lfscloud.yml config storage list
lfscloud --config ./lfscloud.yml repository list
```

Remove commands are idempotent: removing an absent ID succeeds and reports that it was not found. A provider cannot be removed while a repository mapping still references it, because every changed document is validated before it replaces the original:

```bash
lfscloud --config ./lfscloud.yml repository remove \
  --id github-main:OWNER/REPOSITORY
lfscloud --config ./lfscloud.yml config storage remove --id drive-personal
lfscloud --config ./lfscloud.yml config repository remove --id github-main
```

Successful writes use a temporary file beside the config, preserve the original file permissions, and atomically replace it after validation. YAML values and environment references are preserved, but comments and custom formatting are normalized when a change is written.

## Migration Configuration

`lfscloud migrate` is an LFS Cloud client and does not read the private server config or obtain Google Drive credentials. It authenticates to the repository-specific Git LFS route, requests an upload batch to reconcile the inventory, fetches legacy bytes only for objects the server reports missing, and uploads through the returned server actions. The server performs the GitHub write-permission check and owns all storage access, locking, verification, and metadata updates.

The completed migration writes the target URL to both `.lfsconfig` and local Git config. It also preserves the prior source as `remote.<source-remote>.lfsurl` in `.lfsconfig`. This is an allowed Git LFS config key and remains dormant for normal traffic while repository-wide `lfs.url` points to LFS Cloud; a later migration uses it as a command-scoped legacy fetch override. Source URLs with embedded credentials, query strings, or fragments are rejected rather than committed.

This target-first protocol makes follow-up migrations idempotent. A second user can pull the committed `.lfsconfig`, log in to LFS Cloud, and run the same migration: server-present objects are skipped, and only remaining target-missing objects need local or legacy-source bytes.

## Minimal Local Config

```yaml
server:
  host: 127.0.0.1
  port: 8080
  public_url: http://127.0.0.1:8080
  session_encryption_secret: ${LFS_CLOUD_SESSION_SECRET}
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

Repository `name` omits the `.git` suffix because the route adds it. `provider_repository_id` is GitHub's immutable numeric repository ID, available with `gh api repos/OWNER/REPOSITORY --jq .id`. LFS Cloud verifies this value before every permission check so a renamed, transferred, deleted, or reused `owner/name` cannot silently switch the mapping to another repository.

Each user runs `lfscloud login --server URL` with their own GitHub PAT. Login calls GitHub's authenticated-user endpoint to establish identity; it does not grant repository access. For every LFS operation, the server uses that user's retained PAT to check the current GitHub permission on the configured repository. Read or stronger permits downloads; write or admin permits uploads and migration. Token scope, organization SSO policy, expiry, and revocation can still limit otherwise valid repository membership. The PAT is never written to Git's credential helper.

`server.session_encryption_secret` is a server-owned value of at least 32 characters used only to protect durable session credentials. Keep it private and stable across restarts. For transition compatibility, an old repository-provider `personal_access_token` can supply this encryption material when the dedicated setting is absent, but it no longer selects or authenticates the users allowed to log in and should be removed after configuring the dedicated secret.

The metadata upgrade that introduces the dedicated server secret invalidates sessions encrypted with the legacy provider PAT before loading durable session state. Existing users must log in again after that upgrade.

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

`public_url` is the URL embedded in Git LFS batch action responses. Set it to the address clients can actually reach. Plaintext LAN mode exposes users' GitHub PATs during login, LFS credentials, and object bytes to the network, so it requires the explicit `allow_insecure_http: true` development opt-in. Prefer HTTPS through trusted TLS termination. Client commands using this LAN URL also require `--allow-insecure-http`.

## Provider And Request Work Limits

`server.max_batch_objects` bounds the number of object entries accepted in one Git LFS batch request. The default is `100`. Duplicate entries still count toward this request/response limit, while their storage lookups are collapsed to one call per distinct OID and size.

`server.max_provider_calls` bounds concurrent repository-provider and storage-provider work across the entire server process. The default is `16`. Successful repository permission decisions are reused only for the same local session, repository, and operation for 15 seconds, which lets a batch action complete without an immediate duplicate GitHub check. Google Drive access tokens are cached until shortly before their reported expiry, with concurrent requests to `gcloud` collapsed into one token request.

`server.max_concurrent_requests` bounds active HTTP request handling across all server routes. The default is `64`. Excess requests receive HTTP 503 with a one-second `Retry-After` value instead of joining an unbounded queue. An authenticated Git LFS batch body also has a 15-second idle timeout and a 60-second total read deadline, so a slow-dripping client cannot retain a request slot indefinitely.

`server.max_concurrent_uploads` bounds uploads that retain local staging resources across body ingestion and backend storage. The default is `8`. `server.max_concurrent_uploads_per_user` applies a second limit, defaulting to `2`, keyed by stable repository-provider user ID when available. Excess upload staging receives HTTP 503 with `Retry-After: 1`. Each admitted upload also reserves its declared bytes atomically against aggregate temp-directory capacity until its temporary file is released. The live filesystem check and 64 MiB free-space headroom remain in effect as secondary guardrails.

## Google Drive Credentials

The recommended local configuration uses the Google Cloud CLI to mint short-lived tokens from an isolated Application Default Credentials directory:

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

Install `gcloud`, create a Desktop app OAuth client in a Google Cloud project with the Drive API enabled, and download its client JSON. Then generate the isolated ADC state with that project-specific client:

```bash
mkdir -p "$HOME/.config/lfscloud/gcloud-drive"
chmod 700 "$HOME/.config/lfscloud/gcloud-drive"

CLOUDSDK_CONFIG="$HOME/.config/lfscloud/gcloud-drive" \
  gcloud auth application-default login \
  --client-id-file="$HOME/Downloads/client_secret.json" \
  --scopes="https://www.googleapis.com/auth/cloud-platform,https://www.googleapis.com/auth/drive.file"

chmod 600 \
  "$HOME/.config/lfscloud/gcloud-drive/application_default_credentials.json"
```

Keep `--client-id-file` when reauthorizing expired or revoked ADC. Omitting it can select the Google Cloud CLI's shared OAuth client instead of the project where you enabled Drive, causing Drive requests to fail with quota-project or `SERVICE_DISABLED` errors even though token minting succeeds.

The directory must contain `application_default_credentials.json`. LFS Cloud invokes `gcloud` at runtime, so the executable must remain installed and accessible to the server user. When `credentials.executable` is omitted, LFS Cloud defaults to `gcloud.cmd` on Windows and `gcloud` on other platforms.

On Windows, if `gcloud.cmd` cannot find an otherwise available Python installation, point the Google Cloud CLI at the global interpreter and open a new terminal:

```powershell
[Environment]::SetEnvironmentVariable(
    'CLOUDSDK_PYTHON',
    (Get-Command python).Source,
    'User'
)
```

Google Cloud CLI requires `cloud-platform` when explicit ADC scopes are provided; the additional MVP Drive scope is:

```text
https://www.googleapis.com/auth/drive.file
```

The configured `root_folder_id` must be a folder that the app credential can access and create children in. Git users never receive Drive tokens or direct Drive access.

`lfscloud serve` asks `gcloud` for an ADC access token for every configured Drive provider, then performs a non-mutating metadata probe that confirms the root is a live folder with child-write capability. These checks complete before the HTTP listener binds; a missing/invalid credential or unusable root prevents the server from reporting readiness.

## Durable LFS Sessions

Production `serve` processes persist unexpired local LFS sessions in the configured metadata database so Git credentials continue to work across server restarts. The database contains only the local token's SHA-256 digest. The private GitHub PAT and the session identity, scopes, and timestamps are authenticated together with AES-256-GCM before persistence.

The encryption key is derived from `server.session_encryption_secret`; the secret itself is never stored in SQLite. Keep it stable while sessions are active. Rotating it intentionally makes existing rows unreadable, so allow current sessions to expire or remove them first.

## Metadata Path

If `server.metadata_path` is omitted, the server stores SQLite metadata at:

```text
<config directory>/.lfscloud/metadata.sqlite3
```

Relative `metadata_path` values resolve against the directory containing the config file.

The server creates an `upload-locks` directory beside the metadata database. All LFS Cloud processes that can write to the same Google Drive root must use the same metadata location. Those OS-backed object-keyed locks serialize the final Drive existence check and upload across local processes and are released automatically if a process exits. Cross-host writers are not supported by the MVP. Lookup deterministically selects the smallest Drive file ID when an older race has already left multiple otherwise exact object matches.

## Validation Rules

- `server.public_url`, GitHub `api_url`, and CLI `--server` route bases use the same validation policy. They must be HTTP(S) URLs without credentials, query strings, fragments, trailing slashes, whitespace, control characters, backslashes, or path dot segments. They must use HTTPS unless the host is an exact IPv4/IPv6 loopback address. Non-loopback HTTP requires the explicit development-only `server.allow_insecure_http: true` setting and the matching CLI `--allow-insecure-http` flag.
- `server.max_batch_objects`, `server.max_provider_calls`, `server.max_concurrent_requests`, `server.max_concurrent_uploads`, and `server.max_concurrent_uploads_per_user` must be greater than zero when configured. The per-user upload limit cannot exceed the process-wide upload limit.
- Custom Google Drive API base URLs used by embedded runtimes or tests must use HTTPS except for literal loopback IP HTTP endpoints; names such as `localhost` are not accepted.
- Provider IDs and storage IDs must start with an ASCII letter or digit and use only ASCII letters, digits, `_`, or `-`.
- Repository route components must be safe path segments. Repository names must not include `.git`.
- GitHub repository mappings must include a positive numeric `provider_repository_id` matching GitHub's stable repository ID.
- Every repository mapping must reference configured repository and storage providers.
- Duplicate repository IDs and duplicate generated route paths are rejected.
