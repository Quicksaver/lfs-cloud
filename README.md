# LFS Cloud

LFS Cloud is a self-hosted Git LFS server and companion CLI that stores large files in Google Drive while keeping the Git repository on GitHub.

> LFS Cloud is preparing for its first release. The release tooling now builds checksummed binaries, direct installers, Homebrew and APT packages, and WinGet manifests, but those channels will not resolve until the first release is published. Binary signatures and macOS notarization are not available yet.

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
| Authentication      | One configured GitHub account using PAT  |
| Server deployment   | Self-hosted process with SQLite metadata |

LFS Cloud is best suited to a private, single-operator installation. It is not currently a multi-user identity service.

## Quick Start

### 1. Install The Prerequisites

You need:

- Rust 1.88 or newer
- Git and Git LFS
- Google Cloud CLI (`gcloud`)
- a Git credential helper
- a GitHub personal access token limited to the repositories you will serve
- GitHub CLI (`gh`) for the repository ID lookup shown below (optional)

Install LFS Cloud from this checkout:

```bash
cargo install --locked --path .
git lfs install
```

After the first release is published, a direct install or update will be available with:

```bash
curl -fsSL https://github.com/Quicksaver/lfs-cloud/releases/latest/download/lfscloud-installer.sh | sh
```

```powershell
irm https://github.com/Quicksaver/lfs-cloud/releases/latest/download/lfscloud-installer.ps1 | iex
```

The direct installers verify the release checksum and executable version, install into `~/.local/bin` by default, and do not modify `PATH` or invoke elevated privileges. Homebrew, WinGet, and APT commands are documented in [Install, build, and release details](docs/install-release.md).

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

Export the same PAT referenced by the configuration and start LFS Cloud:

```bash
export LFS_CLOUD_GITHUB_PAT="your-personal-access-token"
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

`init` writes the repository-specific endpoint to `.lfsconfig`. Use `--local` to write only repository-local Git config instead. `login` exchanges the configured GitHub PAT for a short-lived LFS Cloud token and stores only that local token through Git's credential helper.

After setup, normal Git and Git LFS pushes and fetches use LFS Cloud. For a new LFS pattern, configure Git LFS as usual:

```bash
git lfs track "*.bin"
git add .gitattributes .lfsconfig
```

## Migrate An Existing Git LFS Repository

Use a non-shallow clone with all source branches and tags available. Keep the source LFS endpoint configured while planning and transferring:

```bash
lfscloud login --server http://127.0.0.1:8080
lfscloud --config /path/to/lfscloud.yml migrate \
  --server http://127.0.0.1:8080 \
  --all-refs \
  --dry-run
lfscloud --config /path/to/lfscloud.yml migrate \
  --server http://127.0.0.1:8080 \
  --all-refs
git add .lfsconfig
git commit -m "Route Git LFS through LFS Cloud"
```

Execution authenticates a repository-scoped request to the target, refreshes the selected source remote's branches and tags, inventories every historical LFS pointer, fetches missing bytes from the source provider, and uploads each verified object to the configured Google Drive backend. Every successful destination check is synchronized to a durable JSON Lines receipt; an interrupted run revalidates recorded objects against the active Drive target before resuming them. Repository configuration is updated only after the complete object inventory succeeds.

If any target object fails, neither repository config location is changed and a retry resumes from the durable receipt. If a later repository-config write itself fails, the uploaded objects and receipt remain safe. When `.lfsconfig` still names the source endpoint, rerun the same migration command. When `.lfsconfig` names the target but `git config --local --get lfs.url` is missing or stale, repair the local override with `lfscloud init --server URL --local`. Verify both locations before removing or purging source LFS data.

Migration writes both `.lfsconfig` and a repository-local `lfs.url`. The local override keeps this checkout on LFS Cloud when checking out commits older than the new `.lfsconfig`. In a fresh clone, run `lfscloud init --server URL --local` before checking out such older commits. Git history and LFS pointers are not rewritten.

Execution requires `--all-refs`; narrower current-checkout and `--ref` scopes remain available for dry-run investigation only. `--source-remote` defaults to `origin`. Use `--allow-cross-remote` only for an intentional copy between different repository identities. `--purge-source-lfs` reports cleanup guidance and the verified receipt but never automatically deletes source objects.

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
- Authentication represents one configured GitHub account, not independent users.
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
