# LFS Cloud

LFS Cloud is a self-hosted Git LFS server and companion CLI that stores large files in Google Drive while keeping the Git repository on GitHub.

> LFS Cloud is preparing for its first release. Install it from source for now; published binaries, installers, checksums, and signatures are not available yet.

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

## Commands

| Command                        | Purpose                                                         |
| ------------------------------ | --------------------------------------------------------------- |
| `lfscloud serve`               | Run the Git LFS-compatible server                               |
| `lfscloud init`                | Configure the current repository's LFS Cloud endpoint           |
| `lfscloud login`               | Create and store a repository-scoped local session              |
| `lfscloud logout`              | Revoke that session and erase its Git credential                |
| `lfscloud status`              | Check repository, server, auth, storage, and cache readiness    |
| `lfscloud pull`                | Fetch Git LFS objects and hydrate the current checkout          |
| `lfscloud hydrate <path...>`   | Replace pointer files with verified bytes from the shared cache |
| `lfscloud dehydrate <path...>` | Replace clean LFS files with pointers after preserving bytes    |
| `lfscloud gc --dry-run`        | Preview cleanup of unreferenced shared-cache objects            |
| `lfscloud migrate --dry-run`   | Plan migration from an existing Git LFS provider                |

Run `lfscloud <command> --help` for all options.

## Current Limitations

- Migration is currently planning-only; `lfscloud migrate` requires `--dry-run` and does not transfer or reconfigure a repository.
- GitHub and Google Drive are the only implemented providers.
- Authentication represents one configured GitHub account, not independent users.
- Uploads and downloads are proxied through the LFS Cloud process, so the host must support long-lived large-file transfers.
- Published and signed release packages are not available yet.

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
