# LFS Cloud Implementation Notes

## Goal

This project explores a custom Git LFS provider that lets a normal Git repository continue using a Git host for source control, while storing Git LFS objects somewhere else.

The initial target is:

- Git repository hosted on GitHub.
- Git LFS endpoint hosted by this project.
- Large file contents stored outside GitHub LFS.
- Google Drive storage backend, potentially using an existing Google One account.
- Multiple repositories served by one `lfs-cloud` instance.
- Multiple storage accounts/providers configured on one `lfs-cloud` instance.
- Provider abstractions kept generic enough to add more Git hosts and storage backends later.

The core idea is:

```text
git / repository host
  stores commits, trees, refs, and small LFS pointer files

lfs-cloud
  implements the Git LFS server API, repository-host authorization,
  and storage-provider routing

storage provider
  stores the actual large file bytes
```

## Git LFS Basics

Git LFS replaces large files in Git history with small pointer files. The pointer file is committed to the Git repository and contains metadata such as:

```text
version https://git-lfs.github.com/spec/v1
oid sha256:<object-hash>
size <bytes>
```

The actual file bytes are uploaded to an LFS server. When a user clones, pulls, checks out, or fetches LFS content, the Git LFS client talks to the configured LFS server and downloads the objects by their hash and size.

The normal Git repository and the LFS storage can be separate services. A repository hosted on GitHub can use a custom LFS endpoint by configuring:

```ini
[lfs]
    url = https://lfs.example.com/owner/repo.git/info/lfs
```

This may live in local Git config or in a committed `.lfsconfig` file. The LFS URL identifies the `lfs-cloud` endpoint for the repository. It should not expose storage-provider credentials or decide directly which backend account is used. That routing should be private server configuration.

## Custom LFS Provider

A plain folder is not enough to act as a Git LFS provider. Git LFS expects a server that implements the LFS HTTP API, especially the batch API:

```text
POST /owner/repo.git/info/lfs/objects/batch
```

The batch request tells the server whether the client wants to `upload` or `download` objects. The server responds with upload/download actions or per-object errors.

The storage backend can be almost anything, as long as the LFS server can reliably store and retrieve exact bytes for a given object hash:

```text
Git LFS client
  -> lfs-cloud API
    -> storage backend
```

Initially supported storage backend:

- Google Drive.

Future storage backend candidates:

- Local filesystem.
- S3-compatible storage.
- Cloudflare R2.
- Backblaze B2.
- MinIO.
- Dropbox, OneDrive, or WebDAV, though these are less natural fits.

For production, object storage is a better fit than Google Drive. Google Drive is feasible for a prototype, but it brings OAuth complexity, quota behavior, file ID mapping, folder semantics, and API limits that are not designed around immutable blob storage.

## Provider Abstractions

`lfs-cloud` should separate repository providers from storage providers.

Repository providers answer questions about identity, repository existence, and authorization:

```text
Can this authenticated user read owner/repo?
Can this authenticated user write owner/repo?
What stable repository ID maps to this host/owner/repo?
```

Storage providers store and retrieve LFS object bytes:

```text
Do you have object sha256:<oid> for this repository/storage namespace?
Store this object.
Stream this object back.
Delete this object if it is no longer retained.
```

Initially supported repository provider:

- GitHub.

Future repository provider candidates:

- GitLab.com.
- Self-hosted GitLab.
- Bitbucket.
- Generic/custom provider later, where authorization is delegated to a configured command or HTTP endpoint.

Initially supported storage provider:

- Google Drive.

Future storage provider candidates:

- Local filesystem.
- S3-compatible storage.
- Cloudflare R2.
- Backblaze B2.
- MinIO.

The relationship should be many-to-many:

```text
GitHub repo A
  -> Google Drive account A

GitHub repo B
  -> Google Drive account B

GitLab repo C
  -> S3 bucket/account C

Bitbucket repo D
  -> local filesystem backend
```

This implies a plugin/adapter boundary for both repository providers and storage providers. The LFS protocol surface remains stable while the server dispatches to the configured repo auth adapter and storage adapter.

## Server Configuration

A hosted `lfs-cloud` instance should load private server-side configuration at boot. This configuration should not be committed to the Git repositories being served, because it may contain storage account IDs, credential references, bucket names, Drive folder IDs, and policy decisions.

A YAML file is a reasonable starting point:

```yaml
server:
  public_url: https://lfs.example.com

repository_providers:
  github-main:
    type: github
    api_url: https://api.github.com
    oauth_client_id: ${GITHUB_CLIENT_ID}
    oauth_client_secret: ${GITHUB_CLIENT_SECRET}

  gitlab-internal:
    type: gitlab
    api_url: https://gitlab.example.com/api/v4
    oauth_client_id: ${GITLAB_CLIENT_ID}
    oauth_client_secret: ${GITLAB_CLIENT_SECRET}

storage_providers:
  drive-user-a:
    type: google_drive
    credentials_ref: google-drive-user-a
    root_folder_id: 012345abcdef

  drive-user-b:
    type: google_drive
    credentials_ref: google-drive-user-b
    root_folder_id: fedcba543210

  s3-user-c:
    type: s3
    endpoint: https://s3.amazonaws.com
    bucket: lfs-cloud-user-c
    credentials_ref: s3-user-c

repositories:
  - id: github-main:owner-a/repo-a
    repo_provider: github-main
    host: github.com
    owner: owner-a
    name: repo-a
    storage_provider: drive-user-a

  - id: github-main:owner-b/repo-b
    repo_provider: github-main
    host: github.com
    owner: owner-b
    name: repo-b
    storage_provider: drive-user-b

  - id: gitlab-internal:group-c/repo-c
    repo_provider: gitlab-internal
    host: gitlab.example.com
    owner: group-c
    name: repo-c
    storage_provider: s3-user-c
```

The committed `.lfsconfig` for each repo should only point at the appropriate LFS endpoint:

```ini
[lfs]
    url = https://lfs.example.com/github.com/owner-a/repo-a.git/info/lfs
```

When a request arrives, the server should:

```text
1. Resolve the LFS URL path to a configured repository.
2. Authenticate the user for that repository provider.
3. Ask the repository provider whether the user has read/write access.
4. Resolve the repository's configured storage provider.
5. Perform the object operation through that storage provider.
```

If a repository is not listed in the server config, the server should deny it by default. This prevents arbitrary repositories from using the instance and makes storage routing explicit.

## Deployment Strategy

Because `lfs-cloud` may proxy large file uploads and downloads, deployment choice directly affects cost and feasibility.

The ideal early deployment is a local server that is reachable from the developer machine and, when needed, from the local network:

```text
local machine
  runs lfs-cloud

localhost endpoint
  used by the same machine

local-network endpoint
  used by other machines on the same trusted network

storage provider
  receives/stores bytes through lfs-cloud
```

For example:

```bash
lfs-cloud serve --config ./lfs-cloud.yml --port 8080
```

By default, the CLI can bind to `127.0.0.1` for safer single-machine use. LAN exposure should be explicit or clearly reported, for example by binding to `0.0.0.0` or a selected interface:

```bash
lfs-cloud serve --config ./lfs-cloud.yml --host 0.0.0.0 --port 8080
```

The CLI should print both addresses when possible:

```text
lfs-cloud server running
  local:   http://127.0.0.1:8080
  network: http://192.168.1.25:8080
```

Then `.lfsconfig` can point to the appropriate URL:

```ini
[lfs]
    url = http://127.0.0.1:8080/github.com/owner/repo.git/info/lfs
```

Or, for another machine on the same LAN:

```ini
[lfs]
    url = http://192.168.1.25:8080/github.com/owner/repo.git/info/lfs
```

This keeps the LFS server under local control and avoids serverless payload limits. The practical limits become the local machine, local network, and storage provider.

Public tunnels are useful for smoke tests and temporary remote access, but they are not a strong MVP default for large LFS data. Services such as ngrok can have bandwidth, request, domain, and account limits that make them unsuitable for GB-scale file transfer.

Good candidates:

- Local server exposed on `127.0.0.1` for single-machine use.
- Local server exposed on the LAN for trusted local-network users.
- Small VPS with predictable bandwidth pricing.
- Self-hosted machine with a stable domain and HTTPS.
- Hosting where large transfer traffic is included, cheap, or does not incur surprise proxy charges.
- Temporary tunnel only for demos, callbacks, or short-lived low-volume testing.

Poor candidates for proxy mode:

- Serverless function platforms with small request/response body limits.
- Platforms where every proxied upload/download byte is billed as application bandwidth.
- Hosts with short execution timeouts or poor support for long-lived streaming requests.
- Tunnel services with low included bandwidth or request quotas.

Vercel can be useful for a UI, admin dashboard, auth pages, or control-plane APIs, but it is not a good default for proxying LFS object bytes. Vercel Functions have request/response payload limits, and proxied transfers can count toward Vercel data-transfer metrics. If Vercel is used, prefer keeping it out of the data path:

```text
Vercel
  UI / admin / metadata / signed URL generation

separate transfer service
  actual LFS object upload/download streaming
```

For storage providers that support narrow, short-lived signed URLs, such as S3-compatible storage, `lfs-cloud` can eventually authorize the request and return a direct upload/download URL. For Google Drive, the safer default is proxy mode because Drive does not provide the same clean object-specific signed URL model.

For production, prefer one of:

```text
self-hosted server
  bandwidth is already paid for or operationally acceptable

VPS / dedicated host
  bandwidth is predictable and priced for large file transfer

object-storage direct transfer mode
  server authorizes requests, clients transfer directly to storage
```

The production goal is not necessarily zero bandwidth cost. The goal is controlled, expected transfer behavior with no hidden serverless payload limits or surprise proxy billing.

## Google Drive Backend

Using Google Drive means `lfs-cloud` should own the Drive integration. End users should not receive direct write access to the Drive folder.

Recommended model:

```text
User
  authenticates to lfs-cloud using the configured repository provider

lfs-cloud
  checks repository permissions
  stores/retrieves bytes through the Google Drive API

Google Drive
  remains private backend storage
```

A Google One Premium 2 TB subscription should be enough for a personal prototype using the account owner's Drive as storage. A Google Cloud project and OAuth credentials are still required to use the Drive API.

Important limits and constraints:

- Google One storage is shared across Drive, Gmail, Photos, backups, and other Google storage usage.
- Drive supports very large individual files, but the account's storage plan is the practical ceiling.
- Drive API quota units measure request cost, not bytes.
- Byte-side constraints such as daily upload/copy and egress limits matter more than request-count quotas for LFS workloads.
- A single-owner Drive backend makes that account the central storage owner, quota owner, and availability dependency.
- A single `lfs-cloud` instance may configure multiple Google Drive accounts as separate storage providers.

## Authorization Strategy

Repository-host permissions should be the source of truth. GitHub is the first target, but the authorization model should be expressed through a repository-provider adapter so GitLab, self-hosted GitLab, Bitbucket, and other hosts can provide equivalent checks.

The LFS server should enforce access before allowing any object transfer:

```text
download request
  require read-level repository access

upload request
  require write-level repository access
```

Suggested GitHub mapping:

```text
GitHub pull / triage
  may download LFS objects

GitHub push / maintain / admin
  may upload and download LFS objects
```

Suggested generic mapping:

```text
repo read
  may download LFS objects

repo write
  may upload and download LFS objects

repo admin/maintain
  may upload, download, and manage repository-level LFS settings
```

For GitHub, the LFS server can query the repository permission API:

```text
GET /repos/{owner}/{repo}/collaborators/{username}/permission
```

For GitLab, Bitbucket, and self-hosted providers, the repository-provider adapter should expose the same internal authorization result even if the host API uses different role names or endpoints.

The Git LFS batch API can include a `ref`, such as:

```json
{
  "operation": "upload",
  "ref": {
    "name": "refs/heads/main"
  }
}
```

This can support ref-aware authorization later. For the initial implementation, repository-level read/write authorization is simpler and probably sufficient. Branch protection and exact branch-level push permissions are more complex because this custom LFS server is not automatically inside the Git host's own Git push authorization flow.

## Authentication Strategy

Preferred flow:

```text
1. User runs an lfs-cloud login command or visits a login URL.
2. User authenticates with the repository provider, such as GitHub OAuth or GitLab OAuth.
3. lfs-cloud identifies the repository-provider user.
4. lfs-cloud issues a short-lived LFS credential/token.
5. The local Git credential helper stores that credential for the LFS URL.
6. Git LFS uses that credential for batch/upload/download requests.
```

Avoid asking users to paste personal access tokens into the LFS server if possible. That can work for a quick prototype, but it means `lfs-cloud` receives and handles powerful user repository-host credentials.

Storage-provider credentials should be backend credentials controlled by the service owner or instance administrator, not by every Git user. For a single-owner prototype, the service can use one Google account's OAuth refresh token to access the backing Drive folder. For a multi-tenant or shared instance, each configured storage provider should have its own credential reference and policy boundary.

## CLI Initialization Strategy

The `lfs-cloud` CLI should provide an `init` command that prepares a repository for this system, similar in spirit to `git lfs install`, but with project-specific configuration and local storage behavior.

Expected behavior:

```bash
lfs-cloud init
```

Could perform:

```text
1. Verify the current directory is inside a Git repository.
2. Configure Git LFS filters and hooks if needed.
3. Write or update .lfsconfig with the lfs-cloud LFS server URL.
4. Configure authentication for the lfs-cloud endpoint.
5. Configure or register a shared local object cache.
6. Register this repo/worktree in local lfs-cloud metadata.
7. Install hooks or helper behavior that materializes files as CoW clones.
```

From the user's perspective, the goal is:

```text
git clone / git pull / git checkout
  should produce usable files
  while avoiding duplicate local disk blocks where possible
```

The CLI should make deduplicated checkout the default rather than requiring users to remember a separate cleanup step.

## Migration Strategy

The `lfs-cloud` CLI should provide a `migrate` command that converts an existing Git LFS-enabled clone to use `lfs-cloud` with as little friction as possible.

Expected command:

```bash
lfs-cloud migrate --server http://127.0.0.1:8080
```

Possible optional purge request:

```bash
lfs-cloud migrate --server http://127.0.0.1:8080 --purge-source-lfs
```

The migration should not need to rewrite Git history in the common case. Existing Git LFS pointer files already contain stable object IDs and sizes:

```text
oid sha256:<object-hash>
size <bytes>
```

The migration should copy the referenced objects from the current LFS provider into the configured `lfs-cloud` storage provider, then repoint the repository's LFS URL to `lfs-cloud`. The objects do not need to be present in the working tree; LFS pointer files in Git history contain the object IDs needed to fetch them from the source provider.

Suggested flow:

```text
1. Verify the current directory is a Git repository with Git LFS configured.
2. Read existing LFS configuration, tracked patterns, and current LFS endpoint.
3. Discover required LFS objects for the current checkout, selected refs, or all refs.
4. For selected-ref or all-ref migration, enumerate LFS pointers from Git history, not only the working tree.
5. Ensure each object exists locally, fetching from the source LFS provider when needed.
6. Upload each object to the configured lfs-cloud server/storage provider.
7. Verify uploaded object SHA-256 and size.
8. Write or update .lfsconfig to point to the lfs-cloud endpoint.
9. Configure local lfs-cloud cache and CoW materialization behavior.
10. Optionally run a local dedup/materialization pass.
11. Report source-provider cleanup options.
```

Migration modes:

```text
current checkout
  migrate only objects needed by the current working tree

selected refs
  migrate objects reachable from specific branches/tags

all refs
  migrate every LFS object reachable from local refs, including objects not
  currently checked out in the working tree
```

The safest default should be to migrate the current checkout and warn if other refs still reference objects that have not been copied. For a full provider move, the user should choose an explicit all-refs mode. In all-refs mode, the command should fetch refs first, enumerate LFS pointers across those refs, fetch missing object bytes from the source LFS provider, and upload every discovered object to `lfs-cloud`.

The `--purge-source-lfs` option should be best-effort and provider-dependent. It should never claim to guarantee deletion unless the source provider exposes a supported object-deletion API and the operation succeeds.

For GitHub specifically, this is an important limitation:

```text
GitHub does not provide a normal self-service API for deleting arbitrary
remote Git LFS objects from storage after they have been uploaded.
```

Removing LFS pointers from Git history or changing `.lfsconfig` does not necessarily remove the old LFS objects from GitHub's LFS storage or quota. GitHub's documented options are generally to delete and recreate the repository or contact GitHub Support for object purging. Therefore, for GitHub, `--purge-source-lfs` should probably:

```text
1. Detect that the source provider is GitHub.
2. Explain that automatic purge is not available.
3. Optionally disable GitHub LFS for the repository if appropriate and supported.
4. Produce a report of migrated LFS object IDs and sizes.
5. Provide instructions for support/manual cleanup.
```

The command should print GitHub-specific helper text similar to:

```text
GitHub LFS purge requires GitHub Support.

1. Open:
   https://support.github.com/contact-next/product-selection/repositories
2. Use the subject:
   remove git lfs file
3. The Virtual Agent button should appear.
4. Use the Virtual Agent flow to remove file(s) from LFS.
5. Provide the migrated LFS object IDs from this report if requested.

GitHub's support UI may change. If this flow is unavailable, use the
general GitHub Support repository contact flow and attach this migration
report.
```

For source providers that do support LFS object deletion, `--purge-source-lfs` can call the provider-specific purge API after the objects have been verified in `lfs-cloud`.

## Storage Layout

LFS objects should be stored by content hash, not by branch.

Avoid this as the primary layout:

```text
.lfs-cloud/
  owner/repo/
    branches/main/...
    branches/feature-x/...
```

The same LFS object may be referenced by many branches, tags, commits, or forks. Storing objects under branches creates duplication and ambiguity.

Better initial layout:

```text
.lfs-cloud/
  repos/
    github.com__owner__repo/
      objects/
        aa/
          bb/
            <full-sha256>
      index.json
```

Better long-term layout:

```text
.lfs-cloud/
  objects/
    aa/
      bb/
        <full-sha256>

database:
  repo_id
  oid
  size
  backend_file_id
  created_by
  created_at
  last_verified_at
```

The authoritative mapping should eventually live in a database, not in Google Drive folder traversal. Google Drive folders are useful for inspection and cleanup, but they should not be the source of truth for object ownership, permissions, or reachability.

## Local Storage Deduplication

Stock Git LFS keeps a local object cache under `.git/lfs/objects` and also writes full files into the working tree. This can double local disk usage for the currently checked-out LFS files, before accounting for older cached objects.

Example:

```text
.git/lfs/objects/<oid>
  immutable cached object

working-tree/path/to/file
  normal checked-out file
```

The `lfs-cloud` client should reduce this by using a shared content-addressed cache and copy-on-write materialization.

Target local model:

```text
~/.lfs-cloud/objects/aa/bb/<sha256>
  canonical cached object

repo/path/to/file
  copy-on-write clone of the cached object
```

On filesystems that support copy-on-write cloning, such as APFS on macOS, the working tree file and cached object can share disk blocks until the working tree file is modified. Editing the file should only duplicate changed blocks.

This reduces:

- Cache + working tree duplication inside one repo.
- Multiplication of the same object across multiple repositories.
- Repeated downloads of identical content-addressed objects.

Useful CLI commands:

```bash
lfs-cloud pull
lfs-cloud hydrate path/to/file
lfs-cloud dehydrate path/to/file
lfs-cloud gc
lfs-cloud status
```

Expected behavior:

```text
lfs-cloud pull
  fetch missing objects
  verify SHA-256 and size
  materialize required working-tree files as CoW clones

lfs-cloud hydrate
  replace pointer/placeholders with usable file contents

lfs-cloud dehydrate
  replace selected large files with pointers/placeholders when not needed

lfs-cloud gc
  remove local cached objects not referenced by any registered repo/worktree
```

There are two likely implementation strategies:

```text
Git LFS compatible path:
  use normal Git LFS protocol compatibility
  use custom LFS server
  add lfs-cloud helper behavior after checkout/pull
  possibly run a dedup/materialization pass automatically

Custom filter path:
  replace or augment Git LFS smudge/filter-process behavior
  materialize files directly from the lfs-cloud cache
  gives more control but increases Git compatibility risk
```

The pragmatic first version should favor Git LFS compatibility and automatic CoW materialization after pull/checkout. A deeper custom filter-process implementation can be considered later if the compatibility path leaves too much transient duplication or awkward behavior.

## Object Metadata

At minimum, track:

- Repository host, owner, and repo name.
- Repository provider ID.
- Stable repository identifier if available.
- Storage provider ID.
- Object SHA-256 OID.
- Object size.
- Backend file ID or storage key.
- Uploading repository-provider user.
- Creation timestamp.
- Verification status.

Optional later metadata:

- Referencing refs or commits.
- Last access timestamp.
- Download count.
- Storage backend name.
- Checksum verification history.
- Garbage collection eligibility.

## Upload Flow

Initial upload flow:

```text
1. Git LFS sends an upload batch request.
2. lfs-cloud authenticates the user.
3. lfs-cloud checks repository-provider write access for the repository.
4. lfs-cloud checks whether each object already exists.
5. For missing objects, lfs-cloud returns upload actions.
6. Client uploads bytes to lfs-cloud.
7. lfs-cloud verifies size and SHA-256.
8. lfs-cloud writes the object to the configured storage provider.
9. lfs-cloud records the object mapping and metadata.
```

For Google Drive, direct signed upload URLs are not as natural as S3-style object storage. The simplest implementation is to proxy uploads through `lfs-cloud`.

## Download Flow

Initial download flow:

```text
1. Git LFS sends a download batch request.
2. lfs-cloud authenticates the user.
3. lfs-cloud checks repository-provider read access for the repository.
4. lfs-cloud resolves each object OID to a backend object.
5. lfs-cloud returns download actions.
6. Client downloads bytes from lfs-cloud.
7. lfs-cloud streams bytes from the configured storage provider.
```

For Google Drive, avoid exposing public or broadly shared Drive links unless there is a strong reason. Proxying downloads through `lfs-cloud` keeps authorization centralized and avoids relying on Drive sharing semantics.

## Multi-Repository Strategy

One hosted `lfs-cloud` instance should support multiple repositories and multiple storage providers. A repository-specific server configuration decides which storage provider is used for that repository.

Client-side repository configuration:

```text
repo .lfsconfig
  points Git LFS to the lfs-cloud endpoint for that repo
```

Server-side private configuration:

```text
lfs-cloud.yml
  maps repo endpoint -> repository provider -> storage provider
```

One Google Drive can hold LFS objects for multiple repositories, but the server should still isolate repositories at the authorization and metadata level. Conversely, one server can use multiple Google Drive accounts, S3 buckets, or local filesystem roots for different repositories.

Initial structure:

```text
.lfs-cloud/
  repos/
    github.com__owner-a__repo-a/
    github.com__owner-b__repo-b/
```

With provider-aware naming:

```text
.lfs-cloud/
  repos/
    github-main__owner-a__repo-a/
    github-main__owner-b__repo-b/
    gitlab-internal__group-c__repo-c/
```

Longer-term deduplication may store identical objects only once globally:

```text
.lfs-cloud/
  objects/
    <hash-sharded-objects>
```

With a database mapping each repository to the objects it is allowed to access. Global deduplication saves space but increases the importance of correct authorization checks, because object existence in one repository must not imply access from another repository.

The storage provider is an implementation detail of the configured repository. Users with read/write access to a repository should not automatically learn or control which backend account stores that repository's LFS objects.

## MVP Scope

A practical MVP could implement:

- Git LFS batch API for upload and download.
- Basic authenticated HTTP transfers.
- Repository-provider abstraction with GitHub as the first implementation.
- GitHub OAuth login.
- Repository-level permission checks through the provider abstraction.
- Storage-provider abstraction with Google Drive as the first implementation.
- Private YAML server configuration mapping repositories to storage providers.
- Google Drive backend using one or more configured service-owner accounts.
- SQLite or simple database-backed object mapping.
- Hash and size verification on upload.
- Private proxy downloads through the LFS server.
- Local `lfs-cloud serve` deployment that prints localhost and LAN addresses.
- `lfs-cloud init` for repository setup.
- `lfs-cloud migrate` for converting an existing Git LFS clone to `lfs-cloud`.
- Shared local content-addressed cache.
- CoW materialization on filesystems that support it.
- Manual `hydrate`, `dehydrate`, `gc`, and `status` commands.

Defer:

- Branch/ref-aware permission enforcement.
- File locking.
- Shared Drive support.
- Garbage collection.
- Global deduplication.
- Admin UI.
- Billing or quota management.
- Public OAuth app verification flow.
- Full virtual filesystem or on-demand range-based file streaming.
- Custom Git filter-process replacement, unless needed after the compatibility path is tested.
- Additional repository providers beyond GitHub.
- Additional storage providers beyond Google Drive.
- Fully hosted production deployment with managed transfer infrastructure.
- Guaranteed source-provider LFS purge where the provider does not expose a supported purge API.

## Open Questions

- What is the smallest GitHub OAuth permission set needed for login and repository permission checks?
- What is the smallest Google Drive OAuth scope needed for storing and retrieving LFS objects under the configured root folder?
- Should object storage be per-repository first, then deduplicated later?
- How much should the service depend on committed `.lfsconfig` versus local user configuration?
- Should this be a hosted service, a self-hosted server, or both?
- Which self-hosted or VPS option gives the best balance of stable URLs, cost, bandwidth, and operational simplicity?
- Should the first auth implementation use GitHub OAuth only, or also support personal access tokens for local/private prototypes?
- Should the first local implementation wrap stock Git LFS behavior, or replace the smudge/filter-process path from the start?
- Should local cache objects be shared globally across all repos by default, or scoped per host/account for simpler permission reasoning?
- Should repository-to-storage mappings be static boot-time YAML at first, or should the server support runtime registration through an admin API?
- Should one repository be allowed to write to multiple storage providers for redundancy, or exactly one provider in the MVP?
- How should config validation detect unsafe mappings, such as two repos sharing a storage namespace without explicit deduplication policy?
- Should `lfs-cloud migrate` default to current-checkout migration or require the user to choose current checkout, selected refs, or all refs?
- How should migration reports represent objects that were copied successfully but cannot be purged from the source provider?

## Key Risks

- Google Drive is not designed as immutable object storage.
- Google OAuth verification may become a product issue if the app is distributed publicly.
- A single Google account can become a storage, quota, and availability bottleneck.
- Branch protection semantics are hard to mirror exactly outside the repository host's Git push path.
- Exposing backend links directly can bypass authorization if not handled carefully.
- Folder structure alone is not a reliable database.
- Garbage collection requires understanding which LFS objects are still reachable from Git refs, tags, and retained history.
- CoW materialization is filesystem-dependent and needs a fallback path for filesystems without clone/reflink support.
- A shared local cache must be garbage-collected carefully so one repository does not delete objects another worktree still needs.
- Misconfigured many-to-many mappings could leak object access across repositories or storage accounts.
- Provider adapters may have subtly different permission semantics, so the internal read/write/admin model must be conservative.
- Tunnels such as ngrok may have changing URLs, bandwidth limits, request limits, or account limits, making them unsuitable for normal GB-scale LFS usage.
- Hosting the proxy path on bandwidth-metered/serverless platforms can make large LFS transfers expensive or technically impossible.
- Source LFS providers may not expose APIs to purge already-uploaded LFS objects, so migration cannot guarantee storage/quota cleanup on every provider.
