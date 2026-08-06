# Server Configuration

`config.yml` is private server-owned configuration. Do not commit it to the Git repositories being served: it contains repository-to-storage routing, credential references, Drive folder IDs, and metadata database paths.

The default path is `${HOME}/.config/lfscloud/config.yml`. On Windows, the default is `%APPDATA%\lfscloud\config.yml`, with `%USERPROFILE%\AppData\Roaming\lfscloud\config.yml` as a fallback when `APPDATA` is unavailable. Pass `--config PATH` to every command that reads the private server configuration when a different location is required.

The committed repository-side `.lfsconfig` should contain only the LFS Cloud endpoint for that repository, for example:

```ini
[lfs]
    url = http://127.0.0.1:15370/github.com/octo-org/assets.git/info/lfs
```

## Manage Configuration From The CLI

Create the parent directory and an empty `config.yml` before using the configuration commands. The `server` section is optional and should be omitted unless a server default is being overridden. The commands manage the three provider and routing sections:

```text
lfscloud config repository  -> repository_providers
lfscloud config storage     -> storage_providers
lfscloud repository         -> repositories
```

Each resource supports `add`, `list`, and `remove`. With no entry flags, `add` opens arrow-key menus for provider choices and prompts only for values that cannot be inferred:

```bash
lfscloud config repository add
lfscloud config storage add
lfscloud repository add
```

Press Up/Down and Enter to select a menu item. Press Enter at text prompts to accept a displayed default or retain the displayed existing value. Repository and storage provider IDs default to `github` and `google_drive` while those IDs are available. If a default ID already exists, enter another ID explicitly.

Interactive Google Drive setup asks for a Desktop OAuth client JSON, creates `${HOME}/.config/lfscloud/gcloud-drive` (`${USERPROFILE}` on Windows), runs the isolated `gcloud auth application-default login` flow with the required scopes, and applies private modes where the platform exposes Unix permissions. Its root folder defaults to Drive's `root` alias.

Interactive repository setup lists the configured repository and storage providers, requires the owner and repository name, defaults the host from the selected GitHub provider, derives the mapping ID as `<repository-provider>:<owner>/<name>`, and obtains GitHub's immutable numeric repository ID through authenticated GitHub CLI (`gh`). Set `LFS_CLOUD_GH_EXECUTABLE` to an explicit executable path when `gh` is installed behind a wrapper or outside the normal executable search path. Interactive setup can update an existing mapping with the derived ID; use flags to update a mapping that has a custom ID.

If any entry flag is supplied, the command is non-interactive. A new entry normally requires every required field, while an existing `--id` accepts only the fields to update. Google Drive is the exception for new entries: supplying `--client-secret-file PATH` applies the current provider, credential, ID, config-directory, and root-folder defaults unless explicitly overridden. Existing entries retain omitted settings during reauthorization. Replaying the same update succeeds without changing the file.

The complete flag-based forms for the currently supported providers are:

```bash
lfscloud config repository add \
  --id github \
  --type github

lfscloud config storage add \
  --id google_drive \
  --type google-drive \
  --credentials-type gcloud \
  --config-dir '${HOME}/.config/lfscloud/gcloud-drive' \
  --client-secret-file "$HOME/Downloads/client_secret.json" \
  --executable gcloud \
  --root-folder-id root \
  --display-name 'Personal Drive LFS'

lfscloud repository add \
  --id github:OWNER/REPOSITORY \
  --repo-provider github \
  --host github.com \
  --owner OWNER \
  --name REPOSITORY \
  --provider-repository-id 123456789 \
  --storage-provider google_drive
```

The short Google Drive form is equivalent when all defaults are wanted:

```bash
lfscloud config storage add \
  --client-secret-file "$HOME/Downloads/client_secret.json"
```

Single-quote environment references so the shell does not expand them before they are written.

Every environment variable referenced anywhere in the config must be set while a changed document is validated, just as it must be set when the server loads that config.

Partial updates identify the existing entry by its stable ID. Supplying `--client-secret-file` for an existing storage provider reauthorizes its current ADC directory without resetting an omitted custom root folder or directory:

```bash
lfscloud config storage add \
  --id drive-personal \
  --display-name 'Archive Drive'

lfscloud repository add \
  --id github:OWNER/REPOSITORY \
  --storage-provider drive-archive
```

Remove a GitHub API override to return to the public-service default:

```bash
lfscloud config repository add \
  --id github \
  --clear-api-url
```

In the interactive repository-provider editor, enter `default` at the API URL prompt to perform the same removal; a blank response retains an existing override.

List commands print tab-separated, script-friendly summaries. A legacy session-secret fallback is reported only as configured or absent; its value is never printed:

```bash
lfscloud config repository list
lfscloud config storage list
lfscloud repository list
```

With no `--id`, `remove` presents an arrow-key list of existing entries. With an explicit ID, removal remains idempotent: removing an absent ID succeeds and reports that it was not found. A provider cannot be removed while a repository mapping still references it, because every changed document is validated before it replaces the original:

```bash
lfscloud repository remove \
  --id github:OWNER/REPOSITORY
lfscloud config storage remove --id drive-personal
lfscloud config repository remove --id github
```

Successful writes use a temporary file beside the config, preserve the original file permissions, and atomically replace it after validation. YAML values and environment references are preserved, but comments and custom formatting are normalized when a change is written.

## Migration Configuration

`lfscloud migrate` is an LFS Cloud client and does not read the private server config or obtain Google Drive credentials. It authenticates to the repository-specific Git LFS route, requests an upload batch to reconcile the inventory, fetches legacy bytes only for objects the server reports missing, and uploads through the returned server actions. The server performs the GitHub write-permission check and owns all storage access, locking, verification, and metadata updates.

The completed migration writes the target URL to both `.lfsconfig` and local Git config. It also preserves the prior source as `remote.<source-remote>.lfsurl` in `.lfsconfig`. This is an allowed Git LFS config key and remains dormant for normal traffic while repository-wide `lfs.url` points to LFS Cloud; a later migration deliberately gives this committed remote-scoped source precedence over a repository-local `lfs.url` and uses it only as a command-scoped legacy fetch override. Source URLs with embedded credentials, query strings, or fragments are rejected rather than committed.

This target-first protocol makes follow-up migrations idempotent. A second user can pull the committed `.lfsconfig`, log in to LFS Cloud, and run the same migration: server-present objects are skipped, and only remaining target-missing objects need local or legacy-source bytes.

## Minimal Local Config

```yaml
repository_providers:
  github:
    type: github

storage_providers:
  drive-personal:
    type: google_drive
    credentials:
      type: gcloud
      config_dir: ${HOME}/.config/lfscloud/gcloud-drive
    root_folder_id: root
    display_name: Personal Drive LFS

repositories:
  - id: github:octo-org/assets
    repo_provider: github
    host: github.com
    owner: octo-org
    name: assets
    provider_repository_id: '123456789'
    storage_provider: drive-personal
```

With that mapping, the repository LFS URL is:

```text
http://127.0.0.1:15370/github.com/octo-org/assets.git/info/lfs
```

Repository `name` omits the `.git` suffix because the route adds it. `provider_repository_id` is GitHub's immutable numeric repository ID. Interactive setup obtains it automatically; flag-based setup can use `gh api repos/OWNER/REPOSITORY --jq .id`. LFS Cloud verifies this value before every permission check so a renamed, transferred, deleted, or reused `owner/name` cannot silently switch the mapping to another repository.

Each user runs `lfscloud login --server URL` with their own GitHub PAT. Login calls GitHub's authenticated-user endpoint to establish identity; it does not grant repository access. For every LFS operation, the server uses that user's retained PAT to check the current GitHub permission on the configured repository. Read or stronger permits downloads; write or admin permits uploads and migration. Token scope, organization SSO policy, expiry, and revocation can still limit otherwise valid repository membership. The PAT is never written to Git's credential helper.

GitHub providers use `https://api.github.com` when `api_url` is omitted. Set it only to override the REST base, most commonly for GitHub Enterprise Server. The provider ID is the stable name referenced by repository mappings, so `github` is the concise default choice. The current authentication composition supports one GitHub provider per server instance.

When `server.session_encryption_secret` is omitted, LFS Cloud manages the key in the operating system's native credential store. It uses macOS Keychain, Windows Credential Manager, or Secret Service on Linux and associates the credential with a stable, non-secret installation ID in the metadata database. Moving the config and metadata files together therefore preserves the lookup identity.

The key is generated only when the native store reports that no entry exists and the database has no active sessions. A locked, denied, or unavailable native store fails startup instead of silently replacing the key. Headless services and containers without a native credential store can explicitly set `server.session_encryption_secret` to an environment reference containing at least 32 characters.

For transition compatibility, an old repository-provider `personal_access_token` still supplies encryption material when the dedicated setting is absent. It no longer authenticates users. To move to managed storage, remove that field and run `lfscloud sessions generate-key`; the confirmation makes the required session invalidation explicit.

The metadata upgrade that introduces the dedicated server secret invalidates sessions encrypted with the legacy provider PAT before loading durable session state. Existing users must log in again after that upgrade.

## Default Network Reachability

The default listener is `0.0.0.0:15370`. One server process therefore accepts IPv4 connections through loopback, LAN, and direct Tailscale addresses without enumerating interfaces. When `server.public_url` is omitted, each Git LFS batch response uses the actual local destination address of that accepted TCP connection. It does not trust the request's `Host` or forwarded headers.

From another machine on the same tailnet, use the server's Tailscale IP and explicitly acknowledge application-layer HTTP:

```bash
lfscloud init \
  --server http://100.x.y.z:15370 \
  --allow-insecure-http
lfscloud login \
  --server http://100.x.y.z:15370 \
  --allow-insecure-http
```

Direct Tailscale packets are encrypted by the tailnet tunnel, but LFS Cloud still serves HTTP rather than HTTPS. On an ordinary LAN, plaintext HTTP exposes users' GitHub PATs during login, local LFS credentials, and object bytes to network observers. Use only a trusted LAN or terminate TLS in front of the server.

`server.public_url` remains available when the socket address is not the URL clients should receive. Examples include MagicDNS, trusted TLS termination, a reverse proxy, or a path prefix:

```yaml
server:
  public_url: https://lfs-host.example.ts.net
```

If MagicDNS resolves directly to the plaintext listener instead of HTTPS termination, configure both the HTTP URL and its explicit server-side exception. Client commands still need `--allow-insecure-http`:

```yaml
server:
  public_url: http://lfs-host.example.ts.net:15370
  allow_insecure_http: true
```

Override `server.host` or `server.port`, or use `lfscloud serve --host HOST --port PORT`, only when a different bind is required.

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

Install `gcloud`, create a Desktop app OAuth client in a Google Cloud project with the Drive API enabled, and download its client JSON. Then let LFS Cloud create and authorize the isolated ADC state:

```bash
lfscloud config storage add \
  --client-secret-file "$HOME/Downloads/client_secret.json"
```

With no flags, `lfscloud config storage add` prompts for the same client JSON and lets you accept the default provider ID, credential directory, executable, and `root` folder ID. LFS Cloud creates the directory, applies private permissions where the platform exposes Unix modes, launches the browser authorization, verifies that `application_default_credentials.json` was created, applies private file permissions, and only then writes the storage entry.

Keep `--client-secret-file` when reauthorizing expired or revoked ADC. Omitting it from an existing-provider update retains the current ADC without launching authorization. If `gcloud auth application-default login` is run manually without the project-specific client file, Google Cloud CLI can instead use its shared OAuth client, causing Drive requests to fail with quota-project or `SERVICE_DISABLED` errors even though token minting succeeds.

Changing an existing provider's `config_dir` without `--client-secret-file` requires that the new directory already contain `application_default_credentials.json`; otherwise LFS Cloud rejects the update before changing the configuration.

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

The configured `root_folder_id` must be a folder that the app credential can access and create children in. Google Drive accepts `root` as the alias for the user's My Drive root, so it is the interactive default; set an explicit folder ID to isolate LFS objects in a dedicated folder. Git users never receive Drive tokens or direct Drive access.

`lfscloud serve` asks `gcloud` for an ADC access token for every configured Drive provider, then performs a non-mutating metadata probe that confirms the root is a live folder with child-write capability. These checks complete before the HTTP listener binds; a missing/invalid credential or unusable root prevents the server from reporting readiness.

## Durable LFS Sessions

Production `serve` processes persist unexpired local LFS sessions in the configured metadata database so Git credentials continue to work across server restarts. The database contains only the local token's SHA-256 digest. The private GitHub PAT and the session identity, scopes, and timestamps are authenticated together with AES-256-GCM before persistence.

The encryption key comes from the native credential store by default; its secret bytes are never stored in SQLite or printed. An explicit `server.session_encryption_secret` remains authoritative for headless deployments.

Rotate the managed key with:

```bash
lfscloud sessions generate-key
```

The command warns that all current sessions will be invalidated and requires confirmation. It refuses to run while the server owns the metadata database, deletes the durable sessions, replaces the native key, and reports only the number invalidated. Users must run `lfscloud login` again. The command also refuses configs with an explicit session secret or deprecated provider PAT because those values, rather than the native store, are authoritative.

Session deletion commits before the replacement credential-store write. If that write fails, no old session remains valid and the previous key remains stored; restore credential-store access and retry `lfscloud sessions generate-key` before accepting new logins. Deleting and recreating the metadata database also creates a new installation ID and does not remove the old native credential automatically. Remove that orphan through the operating system's credential manager if the old database will not be restored.

## Metadata Path

If `server.metadata_path` is omitted, the server stores SQLite metadata at:

```text
<config directory>/.lfscloud/metadata.sqlite3
```

Relative `metadata_path` values resolve against the directory containing the config file.

The server creates a `<metadata filename>.lock` file and an `upload-locks` directory beside the metadata database. The lifecycle lock allows only one active server process for an installation and prevents key rotation while that process still holds sessions in memory. The object-keyed locks retain crash-safe serialization of the final Drive existence check and upload across successive local server processes, and are released automatically if a process exits. Cross-host writers and simultaneous servers for one installation are not supported by the MVP. Lookup deterministically selects the smallest Drive file ID when an older race has already left multiple otherwise exact object matches.

## Validation Rules

- Explicit `server.public_url`, GitHub `api_url` overrides, and CLI `--server` route bases must be HTTP(S) URLs without credentials, query strings, fragments, trailing slashes, whitespace, control characters, backslashes, or path dot segments. Explicit config URLs must use HTTPS unless the host is an exact IPv4/IPv6 loopback address or `server.allow_insecure_http: true` is set. Client commands require their own `--allow-insecure-http` flag for non-loopback HTTP. Per-connection server URLs inferred from the direct listener use HTTP and are covered by the LAN tradeoff above.
- `server.max_batch_objects`, `server.max_provider_calls`, `server.max_concurrent_requests`, `server.max_concurrent_uploads`, and `server.max_concurrent_uploads_per_user` must be greater than zero when configured. The per-user upload limit cannot exceed the process-wide upload limit.
- Custom Google Drive API base URLs used by embedded runtimes or tests must use HTTPS except for literal loopback IP HTTP endpoints; names such as `localhost` are not accepted.
- Provider IDs and storage IDs must start with an ASCII letter or digit and use only ASCII letters, digits, `_`, or `-`.
- Repository route components must be safe path segments. Interior periods, including consecutive periods, are allowed, but complete `.` and `..` segments are rejected. Repository names must not include `.git`.
- GitHub repository mappings must include a positive numeric `provider_repository_id` matching GitHub's stable repository ID.
- Every repository mapping must reference configured repository and storage providers.
- Duplicate repository IDs and duplicate generated route paths are rejected.
