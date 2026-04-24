#!/usr/bin/env bash
set -Eeuo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ROOT_PARENT="$(dirname "$ROOT")"
PATH="$HOME/.cargo/bin:$PATH"
export CARGO_BUILD_JOBS=1

MAIN_BRANCH="main"
TOPIC_BRANCH="x-thin"
UPSTREAM_REMOTE="upstream"
ORIGIN_REMOTE="origin"
CCC_AGENT_ALIAS="@x-thin-codex-bot"
WORKTREE_DIR="${WORKTREE_DIR:-$ROOT_PARENT/$(basename "$ROOT")-maint}"
PUSH_MAIN=0
PUSH_TOPIC=1
INSTALL=1
SMOKE=1
LIVE_SMOKE=0
DRY_RUN=0
AGENT_REPAIR="off"
SETUP_ONLY=0
MAIN_WORK_BRANCH=""
TOPIC_WORK_BRANCH=""

usage() {
  cat <<'USAGE'
Usage: scripts/maintain-x-thin.sh [options]

Sync main from upstream, rebase x-thin in a dedicated worktree, build, install,
smoke test, and push x-thin.

Options:
  --main-branch NAME       Local branch to sync from upstream/main (default: main)
  --topic-branch NAME      Fork branch to rebase and push (default: x-thin)
  --upstream NAME          Upstream remote name (default: upstream)
  --origin NAME            Fork remote name (default: origin)
  --worktree-dir PATH      Dedicated maintenance worktree directory
  --push-main              Push the synced main branch to origin
  --skip-push              Do not push topic branch
  --skip-install           Build and test but do not install local binaries
  --skip-smoke             Do not run smoke checks
  --live-smoke             Run a real bridge turn after install
  --agent-repair MODE      off or ccc; ccc uses @x-thin-codex-bot with -y
  --setup-only             Create or reuse the maintenance worktree and exit
  --dry-run                Print commands without running them
  -h, --help               Show this help

The script intentionally stops on rebase or build failures unless --agent-repair ccc
is provided, in which case it invokes ccc with @x-thin-codex-bot and -y.
The active development checkout is not touched; all branch work happens in the
dedicated maintenance worktree.
USAGE
}

log() {
  printf '\n==> %s\n' "$*"
}

die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

run() {
  printf '+'
  printf ' %q' "$@"
  printf '\n'
  if [[ "$DRY_RUN" == 0 ]]; then
    "$@"
  fi
}

run_cargo() {
  CARGO_BUILD_JOBS=1 run cargo "$@"
}

require_command() {
  command -v "$1" >/dev/null 2>&1 || die "missing required command: $1"
}

parse_args() {
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --main-branch)
        MAIN_BRANCH="${2:?missing value for --main-branch}"
        shift 2
        ;;
      --topic-branch)
        TOPIC_BRANCH="${2:?missing value for --topic-branch}"
        shift 2
        ;;
      --upstream)
        UPSTREAM_REMOTE="${2:?missing value for --upstream}"
        shift 2
        ;;
      --origin)
        ORIGIN_REMOTE="${2:?missing value for --origin}"
        shift 2
        ;;
      --worktree-dir)
        WORKTREE_DIR="${2:?missing value for --worktree-dir}"
        shift 2
        ;;
      --push-main)
        PUSH_MAIN=1
        shift
        ;;
      --skip-push)
        PUSH_TOPIC=0
        shift
        ;;
      --skip-install)
        INSTALL=0
        shift
        ;;
      --skip-smoke)
        SMOKE=0
        shift
        ;;
      --live-smoke)
        LIVE_SMOKE=1
        shift
        ;;
      --agent-repair)
        AGENT_REPAIR="${2:?missing value for --agent-repair}"
        shift 2
        ;;
      --setup-only)
        SETUP_ONLY=1
        shift
        ;;
      --dry-run)
        DRY_RUN=1
        shift
        ;;
      -h|--help)
        usage
        exit 0
        ;;
      *)
        die "unknown argument: $1"
        ;;
    esac
  done

  case "$AGENT_REPAIR" in
    off|ccc) ;;
    *) die "--agent-repair must be off or ccc" ;;
  esac
}

ensure_clean_worktree() {
  if [[ "$DRY_RUN" == 1 ]]; then
    if [[ -n "$(git status --porcelain=v1)" ]]; then
      log "Dry run: ignoring dirty worktree"
    fi
    return
  fi

  [[ -z "$(git status --porcelain=v1)" ]] || die "worktree is dirty; commit or stash changes before maintenance"
  [[ ! -d .git/rebase-merge && ! -d .git/rebase-apply ]] || die "a rebase is already in progress"
  [[ ! -f .git/MERGE_HEAD ]] || die "a merge is already in progress"
}

ensure_maintenance_worktree() {
  if [[ ! -e "$WORKTREE_DIR" ]]; then
    log "Creating maintenance worktree at $WORKTREE_DIR"
    run git worktree add --detach "$WORKTREE_DIR" "$UPSTREAM_REMOTE/$MAIN_BRANCH"
    return
  fi

  git -C "$WORKTREE_DIR" rev-parse --is-inside-work-tree >/dev/null 2>&1 ||
    die "maintenance path exists but is not a git worktree: $WORKTREE_DIR"
}

ccc_repair() {
  local phase="$1"
  local details="$2"

  if [[ "$AGENT_REPAIR" != "ccc" ]]; then
    return 1
  fi

  local prompt=""
  case "$phase" in
    rebase)
      prompt="We are maintaining the $TOPIC_BRANCH fork of Codex. $details Inspect the repository, resolve the rebase conflicts, keep existing user changes intact, stage the result, and summarize what you changed. Do not push."
      ;;
    build)
      prompt="We are maintaining the $TOPIC_BRANCH fork of Codex. $details Inspect the repository, fix the build failure at the root cause, run the minimal relevant checks, and summarize the fix. Do not push."
      ;;
    *)
      prompt="We are maintaining the $TOPIC_BRANCH fork of Codex. $details Inspect the repository, fix the issue, run the minimal relevant checks, and summarize the outcome. Do not push."
      ;;
  esac

  require_command ccc

  cat >&2 <<EOF

ccc repair for phase: $phase

Running:
  ccc -y $CCC_AGENT_ALIAS -- "$prompt"

If ccc cannot finish the repair, resolve it manually, then rerun:
  scripts/maintain-x-thin.sh --agent-repair ccc

EOF
  run ccc -y "$CCC_AGENT_ALIAS" -- "$prompt"
  return 0
}

rebase_in_progress() {
  [[ -d .git/rebase-merge || -d .git/rebase-apply ]]
}

fetch_remote_refs() {
  log "Fetching remotes"
  run git fetch "$UPSTREAM_REMOTE" "$MAIN_BRANCH"
  run git fetch "$ORIGIN_REMOTE" "$TOPIC_BRANCH"
}

prepare_maintenance_branch() {
  log "Preparing maintenance worktree"
  ensure_maintenance_worktree
  MAIN_WORK_BRANCH="${MAIN_BRANCH}-maint"
  TOPIC_WORK_BRANCH="${TOPIC_BRANCH}-maint"

  if [[ "$DRY_RUN" == 1 ]]; then
    log "Dry run: would use worktree at $WORKTREE_DIR"
    return
  fi

  cd "$WORKTREE_DIR"
  ensure_clean_worktree

  log "Resetting $MAIN_WORK_BRANCH to $UPSTREAM_REMOTE/$MAIN_BRANCH"
    run git checkout -B "$MAIN_WORK_BRANCH" "$UPSTREAM_REMOTE/$MAIN_BRANCH"

  if [[ "$PUSH_MAIN" == 1 ]]; then
    log "Pushing $MAIN_WORK_BRANCH to $ORIGIN_REMOTE/$MAIN_BRANCH"
    run git push --force-with-lease "$ORIGIN_REMOTE" "$MAIN_WORK_BRANCH:$MAIN_BRANCH"
  fi

  log "Resetting $TOPIC_WORK_BRANCH to $ORIGIN_REMOTE/$TOPIC_BRANCH"
  run git checkout -B "$TOPIC_WORK_BRANCH" "$ORIGIN_REMOTE/$TOPIC_BRANCH"
}

rebase_topic() {
  log "Rebasing $TOPIC_WORK_BRANCH onto $MAIN_WORK_BRANCH"

  if ! run git rebase "$MAIN_WORK_BRANCH"; then
    if ! ccc_repair "rebase" "The rebase of $TOPIC_WORK_BRANCH onto $MAIN_WORK_BRANCH has conflicts."; then
      die "rebase failed; resolve conflicts and continue or abort before rerunning"
    fi

    if rebase_in_progress; then
      die "ccc returned but the rebase is still in progress; finish it and rerun the script"
    fi
  fi
}

build_branch() {
  log "Building local runtime crates with Bazel"
  if ! run bazel build //codex-rs/cli:cli //codex-rs/turn-start-bridge:codex-turn-start-bridge; then
    if ! ccc_repair "build" "The build failed after rebasing $TOPIC_WORK_BRANCH onto $MAIN_WORK_BRANCH."; then
      die "build failed"
    fi

    log "Retrying build after ccc repair"
    if ! run bazel build //codex-rs/cli:cli //codex-rs/turn-start-bridge:codex-turn-start-bridge; then
      die "build still failing after ccc repair"
    fi
  fi
}

install_branch() {
  if [[ "$INSTALL" == 0 ]]; then
    log "Skipping install"
    return
  fi

  log "Installing local runtime binaries"
  run just x-install
}

smoke_test() {
  if [[ "$SMOKE" == 0 ]]; then
    log "Skipping smoke checks"
    return
  fi

  log "Running non-network smoke checks"
  run codex --version
  run codex-turn-start-bridge --help
  run codex app-server --help

  if [[ "$LIVE_SMOKE" == 1 ]]; then
    log "Running live XML bridge smoke check"
    if [[ "$DRY_RUN" == 1 ]]; then
      printf '+ printf ... | codex-turn-start-bridge --stdin-format xml --codex-bin %q\n' "$(command -v codex || printf codex)"
    else
      printf '<system_prompt>You are terse.</system_prompt><message type="user">Reply with exactly: ok</message>' \
        | codex-turn-start-bridge --stdin-format xml --codex-bin "$(command -v codex)"
    fi
  fi
}

push_topic() {
  if [[ "$PUSH_TOPIC" == 0 ]]; then
    log "Skipping push"
    return
  fi

  log "Pushing $TOPIC_WORK_BRANCH to $ORIGIN_REMOTE/$TOPIC_BRANCH"
  run git push --force-with-lease "$ORIGIN_REMOTE" "$TOPIC_WORK_BRANCH:$TOPIC_BRANCH"
}

main() {
  parse_args "$@"
  cd "$ROOT"

  require_command git
  fetch_remote_refs

  if [[ "$SETUP_ONLY" == 1 ]]; then
    ensure_maintenance_worktree
    log "Maintenance worktree ready at $WORKTREE_DIR"
    exit 0
  fi

  require_command bazel
  require_command just

  prepare_maintenance_branch
  rebase_topic
  build_branch
  install_branch
  smoke_test

  push_topic

  log "Maintenance complete"
}

main "$@"
