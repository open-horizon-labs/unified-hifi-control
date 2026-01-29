# Sync Upstream

Sync the saas main branch with the open source upstream v3 branch.

## Usage

```
/sync-upstream
```

## What This Does

1. Fetches latest from all remotes
2. Checks for new commits in `origin/v3` that aren't in `main`
3. If new commits exist, merges `origin/v3` into `main`
4. Resolves common conflicts:
   - **docker.yml**: Always deleted (removed from saas fork)
   - **build.yml**: Keep HEAD (saas has `main` branch support, Docker sections removed)
   - **LMS plugin files**: Take upstream versions (version numbers)
5. Pushes updated `main` to `saas` remote

## Conflict Resolution Strategy

- The saas fork removes Docker support, so any Docker-related changes from upstream are discarded
- The saas fork adds `main` to branch triggers in CI, which upstream doesn't have
- LMS plugin version files should always match upstream releases

## Remotes

- `origin`: Open source repo (open-horizon-labs/unified-hifi-control)
- `saas`: Private fork (open-horizon-labs/unified-hifi-control-saas)

## After Syncing

If you have a PR branch that needs rebasing:
```bash
git checkout <pr-branch>
git rebase main
git push saas <pr-branch> --force-with-lease
```
