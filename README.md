# LFS Cloud

LFS Cloud is a self-hosted Git LFS server and companion CLI that stores large files in Google Drive while keeping the Git repository on GitHub.

> LFS Cloud is preparing for its first release. The release tooling now builds checksummed binaries, direct installers, Debian packages, Homebrew metadata, and WinGet manifests; Cloudsmith APT publication is optional. These channels will not resolve until the first release is published. Binary signatures and macOS notarization are not available yet.

Maintainers should use `yarn release:all patch` (or `minor`/`major`) as the preferred manual release entry point. See the [all-in-one fleet release guide](docs/install-release.md#all-in-one-fleet-release) for prerequisites, recovery behavior, and lower-level commands.

## What It Does

LFS Cloud keeps repository history and permissions on GitHub, proxies Git LFS uploads and downloads, and stores the object bytes in a private Google Drive folder:

```text
Git LFS client <-> LFS Cloud <-> Google Drive
                         |
                         +-> GitHub permission checks
```

It also provides a shared local object cache. On supported filesystems, checked out files can use copy-on-write materialization to reduce duplicate disk usage.

## Supported Setup

| Component           | Initial support                          |
| ------------------- | ---------------------------------------- |
| Repository provider | GitHub                                   |
| Storage provider    | Google Drive                             |
| Authentication      | Per-user GitHub PAT with repository ACLs |
| Server deployment   | Self-hosted process with SQLite metadata |

Each user logs in with their own GitHub PAT. LFS Cloud verifies that identity with GitHub and checks the user's current permission on every configured repository: read access permits LFS downloads, while write access permits uploads and migration.

## Quick Start

### 1. Install LFS Cloud And The Prerequisites

Every installation needs:

- Git and Git LFS
- Google Cloud CLI (`gcloud`)
- a Git credential helper
- a GitHub personal access token limited to the repositories you will serve
- GitHub CLI (`gh`), installed and authenticated for interactive repository setup

Choose one of the following installation methods.

#### Install Script

On macOS ARM64 or Linux x86-64/ARM64:

```bash
curl -fsSL https://github.com/Quicksaver/lfs-cloud/releases/latest/download/lfscloud-installer.sh | sh
```

On Windows x86-64:

```powershell
irm https://github.com/Quicksaver/lfs-cloud/releases/latest/download/lfscloud-installer.ps1 | iex
```

The install scripts verify the release checksum and executable version, install into `~/.local/bin` by default, and do not modify `PATH` or invoke elevated privileges. Version pinning and custom install directories are documented in [Install, build, and release details](docs/install-release.md#installing-a-published-release).

#### macOS

Install with Homebrew:

```bash
brew install Quicksaver/tap/lfscloud
```

#### Windows

Install with WinGet:

```powershell
winget install --exact --id Quicksaver.LFSCloud
```

#### From This Checkout

This option requires Rust 1.88 or newer:

```bash
cargo install --locked --path .
```

Initialize Git LFS after installing LFS Cloud:

```bash
git lfs install
```

### 2. Prepare Google Drive Access

Create a Google Cloud Desktop OAuth client, enable the Google Drive API, and download its client JSON. `lfscloud config storage add` uses that file to create an isolated Application Default Credentials directory, applies private permission modes on Unix platforms, launches the correctly scoped `gcloud` authorization flow, and creates an app-owned `.lfscloud` folder in Drive by default. See the [configuration guide](docs/configuration.md#google-drive-credentials) for the Google Cloud prerequisites and folder-access details.

### 3. Configure The Private Server

Run the configuration commands directly. Every command that needs the private server configuration creates the file and any missing parent directories automatically. The default path is `${HOME}/.config/lfscloud/config.yml`; on Windows it is `%APPDATA%\lfscloud\config.yml`, with `%USERPROFILE%\AppData\Roaming\lfscloud\config.yml` as a fallback when `APPDATA` is unavailable. An explicit `--config PATH` is initialized the same way.

New files use mode `0600` and new directories use mode `0700` on Unix. On Windows, newly created paths inherit the access controls of the per-user configuration directory. Do not commit the private config.

Add the supported providers and repository mapping interactively:

```bash
lfscloud config repository add
lfscloud config storage add
lfscloud repository add
```

The menus use Up/Down and Enter. Press Enter at the suggested `github`, `google_drive`, `github.com`, and Drive `.lfscloud` defaults. Storage setup accepts `~/...` for both the downloaded Desktop OAuth client JSON and the isolated gcloud directory, performs the `gcloud` authorization, creates or reuses the app-owned `.lfscloud` folder, and stores its actual Drive folder ID. Interactive repository setup requires GitHub CLI (`gh`) to be installed and authenticated so it can obtain GitHub's immutable numeric repository ID.

The resulting configuration has this shape; no `server` section is needed unless overriding a server setting such as `port`:

```yaml
repository_providers:
  github:
    type: github

storage_providers:
  google_drive:
    type: google_drive
    credentials:
      type: gcloud
      config_dir: ${HOME}/.config/lfscloud/gcloud-drive
    root_folder_id: DRIVE_FOLDER_ID

repositories:
  - id: github:OWNER/REPOSITORY
    repo_provider: github
    host: github.com
    owner: OWNER
    name: REPOSITORY
    provider_repository_id: '123456789'
    storage_provider: google_drive
```

`DRIVE_FOLDER_ID` is the actual ID of the `.lfscloud` folder created or reused by default setup, not the folder's display name.

On Windows, generated configuration uses `${USERPROFILE}` instead of `${HOME}` for the Google Drive credential directory.

The server defaults to port `15370` on `0.0.0.0`, so it accepts connections through loopback, LAN, and direct Tailscale IPv4 addresses without network fields in the config. Git LFS action URLs are inferred from the interface each client connected to. Set `server.public_url` only when clients should receive a different hostname or path, such as a MagicDNS name or reverse proxy URL.

GitHub providers use `https://api.github.com` by default. Set `api_url` only for an alternative REST endpoint such as GitHub Enterprise Server.

For non-interactive setup, supply entry flags. Existing IDs can be updated with only the fields that should change. For example, `--client-secret-file PATH` applies the other Google Drive defaults automatically. See [Manage Configuration From The CLI](docs/configuration.md#manage-configuration-from-the-cli) for every flag plus `list` and `remove`.

When creating a repository mapping entirely with flags, supply GitHub's stable numeric repository ID directly. GitHub CLI is one optional way to obtain it:

```bash
gh api repos/OWNER/REPOSITORY --jq .id
```

See the [configuration guide](docs/configuration.md) for the full schema, security constraints, request limits, metadata paths, and LAN/HTTPS setup.

### 4. Start The Server

Start LFS Cloud:

```bash
lfscloud serve
```

On first run, LFS Cloud generates its durable-session encryption key and stores it in the operating system's native credential store: macOS Keychain, Windows Credential Manager, or Secret Service on Linux. The server validates its configuration, native key access, Google Drive credentials, and Drive root before it begins accepting requests.

### 5. Connect A Repository

From the configured Git repository:

```bash
lfscloud init --server http://127.0.0.1:15370
lfscloud login
lfscloud status
```

`init` writes the repository-specific endpoint to `.lfsconfig`. Use `--local` to write only repository-local Git config instead. After initialization, `login`, `logout`, `status`, and migration retries infer the server from that repository URL; an explicit `--server URL` remains available as an override. The inferred URL must exactly match the current Git remote's LFS Cloud route. Non-loopback plaintext HTTP still requires `--allow-insecure-http`.

`login` prints the resolved server before requesting your GitHub PAT, creates a short-lived LFS Cloud session, and stores only the opaque local token through Git's credential helper. Because `.lfsconfig` can be committed, review it before logging in from an untrusted clone. The PAT stays encrypted on the server for current GitHub permission checks.

After setup, normal Git and Git LFS pushes and fetches use LFS Cloud. For a new LFS pattern, configure Git LFS as usual:

```bash
git lfs track "*.bin"
git add .gitattributes .lfsconfig
```

## Migrate An Existing Git LFS Repository

Use a non-shallow clone with all source branches and tags available. Keep the source LFS endpoint configured while planning and transferring:

```bash
lfscloud login --server http://127.0.0.1:15370
lfscloud migrate \
  --server http://127.0.0.1:15370 \
  --all-refs \
  --dry-run
lfscloud migrate \
  --server http://127.0.0.1:15370 \
  --all-refs
git add .lfsconfig
git commit -m "Route Git LFS through LFS Cloud"
```

Execution authenticates a write request to the repository's LFS Cloud route, refreshes the selected source remote's branches and tags, and inventories every historical LFS pointer. It asks the server which objects are already present before fetching source bytes, fetches only the target-missing subset, and uploads those bytes through server-issued Git LFS actions. The client never reads the private server config or accesses Google Drive directly. Repository configuration is updated only after the complete target inventory succeeds.

For each target-missing object, execution flushes one `uploading` line before starting its transfer. The line includes the object's sequence, SHA-256 OID, and size; the next `uploading` line or final report implies that the preceding transfer succeeded, while a failure follows the last attempted object. Objects already present at the target remain summarized in the final report instead of producing upload progress.

If any target object fails, neither target config location is changed. A retry safely asks LFS Cloud again, so objects completed by an earlier user or interrupted run are skipped. If `.lfsconfig` already names the target, rerun the same migration command; migration ignores that target as a source and falls through to the committed legacy remote URL or the selected Git remote's default LFS endpoint for any remaining target-missing objects. An explicit `--server` can use another address for the same LFS Cloud repository route, such as loopback instead of its Tailscale IP, without turning the configured target into a legacy source.

Migration writes the target to both `.lfsconfig` and repository-local `lfs.url`. Before switching, it also records the old endpoint as the standard `remote.<source>.lfsurl` field in `.lfsconfig`; follow-up users can therefore migrate their local-only objects without private server configuration. The repository-wide target remains active for normal Git LFS traffic, while migration applies the legacy URL only to its source fetch command. Git history and LFS pointers are not rewritten, and URLs containing credentials are never committed.

Follow-up migration fetches request only the target-missing object IDs. Git LFS currently resolves those through one bounded `smudge` invocation per object, so source recovery time scales with the number of missing objects rather than the complete repository inventory.

Execution requires `--all-refs`; narrower current-checkout and `--ref` scopes remain available for dry-run investigation only. `--source-remote` defaults to `origin`. Use `--allow-cross-remote` only for an intentional copy between different repository identities. `--purge-source-lfs` reports cleanup guidance but never automatically deletes source objects.

The initial migration keeps the legacy LFS URL active until all target objects succeed, so supply `--server URL` when that URL does not yet identify LFS Cloud. Once migration writes the target route, retries and follow-up users can omit `--server`.

## Commands

| Command                          | Purpose                                                         |
| -------------------------------- | --------------------------------------------------------------- |
| `lfscloud config repository`     | Add, update, list, or remove repository-provider configuration  |
| `lfscloud config storage`        | Add, update, list, or remove storage-provider configuration     |
| `lfscloud repository`            | Add, update, list, or remove served repository mappings         |
| `lfscloud sessions generate-key` | Rotate the managed session key and invalidate current sessions  |
| `lfscloud serve`                 | Run the Git LFS-compatible server                               |
| `lfscloud init`                  | Configure the current repository's LFS Cloud endpoint           |
| `lfscloud login`                 | Create and store a repository-scoped local session              |
| `lfscloud logout`                | Revoke that session and erase its Git credential                |
| `lfscloud status`                | Check repository, server, auth, storage, and cache readiness    |
| `lfscloud pull`                  | Fetch Git LFS objects and hydrate the current checkout          |
| `lfscloud hydrate <path...>`     | Replace pointer files with verified bytes from the shared cache |
| `lfscloud dehydrate <path...>`   | Replace clean LFS files with pointers after preserving bytes    |
| `lfscloud gc --dry-run`          | Preview cleanup of unreferenced shared-cache objects            |
| `lfscloud migrate`               | Migrate complete Git LFS history into LFS Cloud                 |

Run `lfscloud <command> --help` for all options.

Command failures print the complete available cause chain, concrete error values, and a Rust backtrace by default. This diagnostic output can include local paths, repository identifiers, and upstream service details, but secret-bearing credential output remains redacted. A failed local credential lookup names the affected LFS URL and the matching `lfscloud login` recovery command while retaining the suppressed helper failure in its cause chain. Pass the global `--quiet` option before or after a subcommand to retain the concise error format without enabling automatic backtrace capture:

```bash
lfscloud --quiet status
lfscloud migrate --quiet --all-refs
```

## Current Limitations

- GitHub and Google Drive are the only implemented providers.
- Users supply their own GitHub PATs; OAuth/device-flow login is not implemented.
- Uploads and downloads are proxied through the LFS Cloud process, so the host must support long-lived large-file transfers.
- The package channels will remain unavailable until the first release is published; release binaries are not signed or notarized yet.

The default listener makes the server reachable from trusted LAN and Tailscale interfaces, but it does not add TLS. Plain HTTP over a LAN exposes GitHub PATs during login, local LFS credentials, and object bytes to network observers; client commands therefore require `--allow-insecure-http` for non-loopback HTTP. Prefer HTTPS on any network you do not fully trust. Direct Tailscale traffic is encrypted by the tailnet tunnel, although the application URL remains HTTP.

## Documentation

- [Server configuration](docs/configuration.md)
- [Install, build, and release details](docs/install-release.md)
- [Historical implementation notes](docs/history/implementation.md)
- [Historical implementation review findings](docs/history/findings.md)
- [Archived pre-release README](docs/history/pre-release-readme.md)

The historical documents preserve design and implementation context; they are not the current user guide and may describe superseded plans.

## License

LFS Cloud is available under the [MIT License](LICENSE).
