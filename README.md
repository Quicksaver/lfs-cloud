# LFS Cloud

LFS Cloud is a self-hosted Git LFS server and companion CLI that stores large files in Google Drive while keeping the Git repository on GitHub.

> LFS Cloud is preparing for its first release. The release tooling now builds checksummed binaries, direct installers, Debian packages, Homebrew metadata, and WinGet manifests; Cloudsmith APT publication is optional. These channels will not resolve until the first release is published. Binary signatures and macOS notarization are not available yet.

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
- GitHub CLI (`gh`) for the repository ID lookup shown below (optional)

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

Create a Google Cloud Desktop OAuth client, enable the Google Drive API, and authorize an isolated Application Default Credentials directory with the `cloud-platform` and `drive.file` scopes. The complete setup commands and folder-access requirements are in the [configuration guide](docs/configuration.md#google-drive-credentials).

Create or choose the private Drive folder that will hold LFS objects, then keep its folder ID for the server configuration.

### 3. Create `lfscloud.yml`

Create a private server configuration file. Do not commit it.

```yaml
server:
  host: 127.0.0.1
  port: 8080
  public_url: http://127.0.0.1:8080
  session_encryption_secret: ${LFS_CLOUD_SESSION_SECRET}

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
    root_folder_id: YOUR_DRIVE_FOLDER_ID

repositories:
  - id: github-main:OWNER/REPOSITORY
    repo_provider: github-main
    host: github.com
    owner: OWNER
    name: REPOSITORY
    provider_repository_id: '123456789'
    storage_provider: drive-personal
```

The same provider and repository entries can be added interactively:

```bash
lfscloud --config ./lfscloud.yml config repository add
lfscloud --config ./lfscloud.yml config storage add
lfscloud --config ./lfscloud.yml repository add
```

Create the file and its `server` section first. Passing no entry flags prompts for the complete entry; passing any entry flag makes `add` non-interactive. Existing IDs can be updated with only the fields that should change. See [Manage Configuration From The CLI](docs/configuration.md#manage-configuration-from-the-cli) for every flag plus `list` and `remove`.

Get GitHub's stable numeric repository ID with:

```bash
gh api repos/OWNER/REPOSITORY --jq .id
```

See the [configuration guide](docs/configuration.md) for the full schema, security constraints, request limits, metadata paths, and LAN/HTTPS setup.

### 4. Start The Server

Export the server-owned session encryption secret and start LFS Cloud:

```bash
export LFS_CLOUD_SESSION_SECRET="replace-with-at-least-32-random-characters"
lfscloud serve --config ./lfscloud.yml
```

The server validates its configuration, Google Drive credentials, and Drive root before it begins accepting requests.

### 5. Connect A Repository

From the configured Git repository:

```bash
lfscloud init --server http://127.0.0.1:8080
lfscloud login --server http://127.0.0.1:8080
lfscloud --config /path/to/lfscloud.yml status
```

`init` writes the repository-specific endpoint to `.lfsconfig`. Use `--local` to write only repository-local Git config instead. `login` verifies your GitHub PAT, creates a short-lived LFS Cloud session, and stores only the opaque local token through Git's credential helper. The PAT stays encrypted on the server for current GitHub permission checks.

After setup, normal Git and Git LFS pushes and fetches use LFS Cloud. For a new LFS pattern, configure Git LFS as usual:

```bash
git lfs track "*.bin"
git add .gitattributes .lfsconfig
```

## Migrate An Existing Git LFS Repository

Use a non-shallow clone with all source branches and tags available. Keep the source LFS endpoint configured while planning and transferring:

```bash
lfscloud login --server http://127.0.0.1:8080
lfscloud migrate \
  --server http://127.0.0.1:8080 \
  --all-refs \
  --dry-run
lfscloud migrate \
  --server http://127.0.0.1:8080 \
  --all-refs
git add .lfsconfig
git commit -m "Route Git LFS through LFS Cloud"
```

Execution authenticates a write request to the repository's LFS Cloud route, refreshes the selected source remote's branches and tags, and inventories every historical LFS pointer. It asks the server which objects are already present before fetching source bytes, fetches only the target-missing subset, and uploads those bytes through server-issued Git LFS actions. The client never reads the private server config or accesses Google Drive directly. Repository configuration is updated only after the complete target inventory succeeds.

If any target object fails, neither target config location is changed. A retry safely asks LFS Cloud again, so objects completed by an earlier user or interrupted run are skipped. If `.lfsconfig` already names the target, rerun the same migration command; migration ignores that target as a source and falls through to the committed legacy remote URL or the selected Git remote's default LFS endpoint for any remaining target-missing objects.

Migration writes the target to both `.lfsconfig` and repository-local `lfs.url`. Before switching, it also records the old endpoint as the standard `remote.<source>.lfsurl` field in `.lfsconfig`; follow-up users can therefore migrate their local-only objects without private server configuration. The repository-wide target remains active for normal Git LFS traffic, while migration applies the legacy URL only to its source fetch command. Git history and LFS pointers are not rewritten, and URLs containing credentials are never committed.

Follow-up migration fetches request only the target-missing object IDs. Git LFS currently resolves those through one bounded `smudge` invocation per object, so source recovery time scales with the number of missing objects rather than the complete repository inventory.

Execution requires `--all-refs`; narrower current-checkout and `--ref` scopes remain available for dry-run investigation only. `--source-remote` defaults to `origin`. Use `--allow-cross-remote` only for an intentional copy between different repository identities. `--purge-source-lfs` reports cleanup guidance but never automatically deletes source objects.

## Commands

| Command                        | Purpose                                                         |
| ------------------------------ | --------------------------------------------------------------- |
| `lfscloud config repository`   | Add, update, list, or remove repository-provider configuration  |
| `lfscloud config storage`      | Add, update, list, or remove storage-provider configuration     |
| `lfscloud repository`          | Add, update, list, or remove served repository mappings         |
| `lfscloud serve`               | Run the Git LFS-compatible server                               |
| `lfscloud init`                | Configure the current repository's LFS Cloud endpoint           |
| `lfscloud login`               | Create and store a repository-scoped local session              |
| `lfscloud logout`              | Revoke that session and erase its Git credential                |
| `lfscloud status`              | Check repository, server, auth, storage, and cache readiness    |
| `lfscloud pull`                | Fetch Git LFS objects and hydrate the current checkout          |
| `lfscloud hydrate <path...>`   | Replace pointer files with verified bytes from the shared cache |
| `lfscloud dehydrate <path...>` | Replace clean LFS files with pointers after preserving bytes    |
| `lfscloud gc --dry-run`        | Preview cleanup of unreferenced shared-cache objects            |
| `lfscloud migrate`             | Migrate complete Git LFS history into LFS Cloud                 |

Run `lfscloud <command> --help` for all options.

## Current Limitations

- GitHub and Google Drive are the only implemented providers.
- Users supply their own GitHub PATs; OAuth/device-flow login is not implemented.
- Uploads and downloads are proxied through the LFS Cloud process, so the host must support long-lived large-file transfers.
- The package channels will remain unavailable until the first release is published; release binaries are not signed or notarized yet.

Use HTTPS for every non-loopback deployment. Plaintext LAN mode is an explicit development-only opt-in and exposes credentials and object bytes to network observers.

## Documentation

- [Server configuration](docs/configuration.md)
- [Install, build, and release details](docs/install-release.md)
- [Historical implementation notes](docs/history/implementation.md)
- [Historical implementation review findings](docs/history/findings.md)
- [Archived pre-release README](docs/history/pre-release-readme.md)

The historical documents preserve design and implementation context; they are not the current user guide and may describe superseded plans.

## License

LFS Cloud is available under the [MIT License](LICENSE).
