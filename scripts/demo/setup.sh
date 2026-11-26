#!/usr/bin/env bash
# Builds tam's demo world under $TAM_DEMO (default /tmp/tam-demo): three
# made-up repositories, tasks in every status, chats and shells, run by a
# separate daemon. See README.md.
#
#   scripts/demo/setup.sh [--real-claude]
#
# --real-claude: the first chat (the one the dashboard opens on) runs the
# real Claude Code on the demo repository, so the pane shows the real
# thing. It costs one short turn, and needs you logged in (your
# credentials file is copied into the demo, which teardown.sh deletes).
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
real_claude=''
for arg in "$@"; do
  case $arg in
    --real-claude) real_claude=1 ;;
    -h | --help) sed -n '2,11s/^# \{0,1\}//p' "$0"; exit 0 ;;
    *) echo "unknown argument: $arg" >&2; exit 2 ;;
  esac
done

demo=${TAM_DEMO:-/tmp/tam-demo}
if [ -e "$demo" ]; then
  echo "$demo already exists: run $here/teardown.sh first." >&2
  exit 1
fi
real_claude_bin=$(command -v claude || true)
real_home=$HOME
for tool in tam git; do
  command -v "$tool" >/dev/null || { echo "$tool is not on the PATH" >&2; exit 1; }
done

# shellcheck source=env.sh
. "$here/env.sh"
say() { printf '\033[1m==>\033[0m %s\n' "$*"; }
say "Demo world in $TAM_DEMO, with $(command -v tam)"

# ------------------------------------------------------------ home

mkdir -p "$HOME/src" "$CLAUDE_CONFIG_DIR" "$XDG_CONFIG_HOME/tam"
cat >"$HOME/.gitconfig" <<'EOF'
[user]
	name = Dana Demo
	email = dana@example.com
[init]
	defaultBranch = main
[commit]
	gpgsign = false
[advice]
	detachedHead = false
EOF
printf "PS1='\\\\W \$ '\n" >"$HOME/.bashrc"
printf "PROMPT='%%1~ \$ '\n" >"$HOME/.zshrc"
: >"$XDG_CONFIG_HOME/tam/config.toml"
cd "$HOME"

# ------------------------------------------------------------ repositories

# put REPO PATH: write stdin to the file. commit REPO MESSAGE.
put() { mkdir -p "$(dirname "$1/$2")"; cat >"$1/$2"; }
commit() { git -C "$1" add -A && git -C "$1" commit -qm "$2"; }
# log REPO FILE N PREFIX: N commits, each a line added to FILE.
log() {
  local i
  for i in $(seq 1 "$3"); do
    echo "- $4 $i" >>"$1/$2"
    commit "$1" "$4 ($i)"
  done
}

say "Repositories: api, web, infra"
api=$HOME/src/api web=$HOME/src/web infra=$HOME/src/infra
git init -q "$api" && git init -q "$web" && git init -q "$infra"

put "$api" README.md <<'EOF'
# api

The shop's HTTP API: catalog, cart, checkout.
EOF
put "$api" package.json <<'EOF'
{ "name": "api", "private": true, "scripts": { "start": "node dist/server.js", "test": "vitest run" } }
EOF
put "$api" src/server.ts <<'EOF'
import express from "express";
import { rateLimit } from "./middleware/rateLimit";
import { checkout } from "./routes/checkout";

const app = express();
app.use(rateLimit({ perSecond: 100 }));
app.use("/checkout", checkout);
app.listen(8080);
EOF
put "$api" src/middleware/rateLimit.ts <<'EOF'
export function rateLimit({ perSecond }: { perSecond: number }) {
  let windowStart = Date.now();
  let count = 0;
  return (_req: unknown, res: any, next: () => void) => {
    if (Date.now() - windowStart > 1000) { windowStart = Date.now(); count = 0; }
    if (++count > perSecond) return res.status(429).end();
    next();
  };
}
EOF
put "$api" src/cache.ts <<'EOF'
const products = new Map<string, unknown>();
export const getProduct = (id: string) => products.get(id);
EOF
put "$api" src/routes/checkout.ts <<'EOF'
import { Router } from "express";
export const checkout = Router();
checkout.post("/", (_req, res) => res.json({ ok: true }));
EOF
put "$api" test/rateLimit.test.ts <<'EOF'
import { test } from "vitest";
test("limits requests", () => {});
EOF
put "$api" CHANGELOG.md <<'EOF'
# Changelog
EOF
commit "$api" "Initial API: catalog, cart, checkout"
log "$api" CHANGELOG.md 3 "Catalog: pagination fixes"
# spike-graphql forked long ago: 5 commits of its own, 12 behind main.
git -C "$api" checkout -qb spike-graphql
put "$api" src/graphql/schema.graphql <<'EOF'
type Product { id: ID! name: String! price: Int! }
type Query { product(id: ID!): Product }
EOF
commit "$api" "GraphQL: product schema"
log "$api" src/graphql/NOTES.md 4 "GraphQL spike: resolver"
git -C "$api" checkout -q main
log "$api" CHANGELOG.md 12 "Cart: small fixes"

put "$web" README.md <<'EOF'
# web

The shop's web front end.
EOF
put "$web" package.json <<'EOF'
{ "name": "web", "private": true, "scripts": { "dev": "vite", "test:e2e": "playwright test" } }
EOF
put "$web" src/App.tsx <<'EOF'
import { Checkout } from "./pages/Checkout";
export const App = () => <Checkout />;
EOF
put "$web" src/pages/Checkout.tsx <<'EOF'
export const Checkout = () => <button id="pay-button">Pay</button>;
EOF
put "$web" src/pages/Pricing.tsx <<'EOF'
export const Pricing = () => <h1>Pricing</h1>;
EOF
put "$web" src/components/Banner.tsx <<'EOF'
export const Banner = () => <div className="banner">Summer sale</div>;
EOF
put "$web" src/theme.css <<'EOF'
:root { --bg: #fff; --fg: #111; }
EOF
put "$web" tests/e2e/checkout.spec.ts <<'EOF'
import { test } from "@playwright/test";
test("checkout", async ({ page }) => { await page.click("#pay-button"); });
EOF
commit "$web" "Initial web app: catalog and checkout pages"
log "$web" CHANGELOG.md 4 "Checkout: copy changes"

put "$infra" README.md <<'EOF'
# infra

Terraform for the shop: network, database, logs.
EOF
put "$infra" main.tf <<'EOF'
module "db" { source = "./modules/db" }
module "logs" { source = "./modules/logs" }
EOF
put "$infra" modules/db/main.tf <<'EOF'
resource "aws_db_instance" "main" {
  engine         = "postgres"
  engine_version = "15.6"
  instance_class = "db.r6g.xlarge"
}
EOF
put "$infra" modules/logs/main.tf <<'EOF'
resource "aws_cloudwatch_log_group" "api" {
  name              = "/shop/api"
  retention_in_days = 0
}
EOF
commit "$infra" "Initial infrastructure"
# terraform-upgrade: 3 commits of its own, 1 behind main.
git -C "$infra" checkout -qb terraform-upgrade
log "$infra" versions.tf 3 "AWS provider v6"
git -C "$infra" checkout -q main
log "$infra" CHANGELOG.md 1 "Network: tag subnets"

# ------------------------------------------------------------ phase 1
# What must look old is made first, then the daemon is stopped and its
# ledger lines are dated back. Chats are listed in the order they were
# made, so all of them are made here; their agents start again later.

# chat SCENE REPO [ARGS...]: make a chat, print its id.
chat() {
  local scene=$1 repo=$2
  shift 2
  DEMO_SCENE=$scene tam chat "$repo" -d --json "$@" |
    sed -n 's/^ *"chat": "\(c-[0-9a-f]*\)".*/\1/p'
}

say "Chats"
if [ -n "$real_claude" ]; then
  [ -n "$real_claude_bin" ] || { echo "--real-claude: no claude on the PATH" >&2; exit 1; }
  logged_in=''
  if [ -f "$real_home/.claude/.credentials.json" ]; then
    install -m 600 "$real_home/.claude/.credentials.json" "$CLAUDE_CONFIG_DIR/.credentials.json"
    logged_in=1
  fi
  # Onboarding done, and the demo's checkouts and worktrees trusted.
  cat >"$CLAUDE_CONFIG_DIR/.claude.json" <<EOF
{
  "hasCompletedOnboarding": true,
  "projects": {
    "$(cd "$api" && pwd -P)": { "hasTrustDialogAccepted": true }
  }
}
EOF
  release=$(PATH=${PATH//"$TAM_DEMO_SCRIPTS/bin:"/} tam chat "$api" -d --json \
    -p 'In three short bullets: how does src/middleware/rateLimit.ts limit requests, and why can a burst of twice the limit get through? Do not change anything.' |
    sed -n 's/^ *"chat": "\(c-[0-9a-f]*\)".*/\1/p')
  if [ -n "$logged_in" ]; then
    say "Waiting for the real Claude's turn in chat $release"
    tam wait "$release" -t 5m >/dev/null || echo "  (it did not finish: tam attach $release)"
  else
    echo "  No ~/.claude/.credentials.json to copy (on macOS, Claude keeps it in the"
    echo "  Keychain): log in once with tam attach $release, once setup is done."
  fi
else
  release=$(chat release-checklist "$api")
fi
e2e=$(chat flaky-e2e "$web")
pg=$(chat pg-upgrade "$infra" --keep)
pricing=$(chat pricing-copy "$web")

say "Tasks with nothing running"
tam new spike-graphql -r "$api" --no-start >/dev/null
tam new dark-mode -r "$web" --no-start >/dev/null
tam new old-banner -r "$web" --no-start >/dev/null
sleep 2 # the chats' first hooks

say "Dating back"
tam shutdown >/dev/null
for _ in $(seq 1 100); do [ -S "$XDG_RUNTIME_DIR/tam/sock" ] || break; sleep 0.1; done
ledger=$XDG_DATA_HOME/tam/ledger.jsonl
now=$(date +%s)
# back AGE_SECONDS PATTERN...: lines matching a pattern happened AGE ago.
back() {
  local age=$1 p
  shift
  for p in "$@"; do
    sed -E "/$p/s/\"timestamp\":[0-9]+/\"timestamp\":$((now - age))/" "$ledger" >"$ledger.tmp"
    mv "$ledger.tmp" "$ledger"
  done
}
back $((45 * 86400)) '"(name|task)":"spike-graphql"'
back $((2 * 86400 + 3 * 3600)) '"(name|task)":"dark-mode"'
back $((3 * 86400 + 5 * 3600)) '"(name|task)":"old-banner"'
back $((2 * 86400 + 7 * 3600)) "\"$pricing\""
rm -rf "$HOME/worktrees/web--old-banner"

# ------------------------------------------------------------ phase 2

say "Agents"
# scene SCENE TAM-ARGS...: start an agent playing SCENE.
scene() { local s=$1; shift; DEMO_SCENE=$s tam "$@" -d >/dev/null; }

tam new fix-rate-limit -r "$api" --no-start >/dev/null
wt=$HOME/worktrees/api--fix-rate-limit
echo "// sliding window" >>"$wt/src/middleware/rateLimit.ts" && commit "$wt" "Rate limiter: sliding window"
echo 'test("bursts", () => {});' >>"$wt/test/rateLimit.test.ts" && commit "$wt" "Rate limiter: burst test"
echo "// TODO: per-route limits" >>"$wt/src/server.ts"
scene fix-rate-limit run fix-rate-limit

scene perf-cache new perf-cache -r "$api"

tam new checkout-v2 -r "$api" -r "$web" --no-start >/dev/null
wt=$HOME/worktrees/api--checkout-v2
echo "// v2" >>"$wt/src/routes/checkout.ts" && commit "$wt" "Checkout v2: route"
put "$wt" src/routes/checkoutV2.ts <<<'export {};'
put "$wt" migrations/0035_checkout_v2.sql <<<'-- checkout v2'
echo "// v2" >>"$wt/src/server.ts"
wt=$HOME/worktrees/web--checkout-v2
put "$wt" src/pages/CheckoutV2.tsx <<<'export {};' && commit "$wt" "Checkout v2: page"
scene checkout-api run checkout-v2/api
scene checkout-web run checkout-v2/web

scene terraform-upgrade new terraform-upgrade -r "$infra"

tam new log-retention -r "$infra" --no-start >/dev/null
wt=$HOME/worktrees/infra--log-retention
sed 's/= 0/= 30/' "$wt/modules/logs/main.tf" >"$wt/x" && mv "$wt/x" "$wt/modules/logs/main.tf"
commit "$wt" "Logs: 30 days of retention"
scene log-retention run log-retention

if [ -n "$real_claude" ]; then
  PATH=${PATH//"$TAM_DEMO_SCRIPTS/bin:"/} tam run "$release" -d >/dev/null
else
  scene release-checklist run "$release"
fi
scene flaky-e2e run "$e2e"
scene pg-upgrade run "$pg"

say "Shells"
tam shell fix-rate-limit -d >/dev/null 2>&1
tam shell fix-rate-limit -d >/dev/null 2>&1
tam shell checkout-v2/api -d >/dev/null 2>&1
tam shell -r "$api" -d >/dev/null 2>&1
tam shell -r "$web" -d >/dev/null 2>&1

say "Ready. In a terminal of about 160x45:"
echo
echo "    . $TAM_DEMO_SCRIPTS/env.sh && tam"
echo
echo "  The agents are scripted: what they say is made up, and typing into"
echo "  one gets a canned answer. $TAM_DEMO_SCRIPTS/teardown.sh removes it all."
