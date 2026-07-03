SERVICE := "bridge-event-parser-service"
IMAGE   := "ghcr.io/bridge-craftwork/" + SERVICE
DROPLET := "bridge-droplet"

default:
    @just --list

# dev/test go through dev-build.sh so the gitignored local-checkout patches
# in .cargo/config.toml take effect without dirtying the committed Cargo.lock
# (see CLAUDE.md).
dev:
    ./dev-build.sh run

test:
    ./dev-build.sh test

# CI parity: patches disabled, committed lock's git pins.
check:
    cargo fmt --check
    ./dev-build.sh --ci clippy -- -D warnings

release VERSION:
    git tag {{VERSION}}
    git push origin {{VERSION}}

_colima-up:
    @colima status >/dev/null 2>&1 || (echo "Starting colima..." && colima start --vz-rosetta)

build: _colima-up
    docker buildx --builder colima build --platform linux/amd64 -t {{IMAGE}}:dev --load .

push: build
    docker push {{IMAGE}}:dev

deploy: push
    ssh {{DROPLET}} '/opt/bridge-craftwork/scripts/deploy.sh {{SERVICE}}'

deploy-version VERSION:
    ssh {{DROPLET}} 'sed -i "s/^{{SERVICE}}_TAG=.*/{{SERVICE}}_TAG={{VERSION}}/" /opt/bridge-craftwork/.env && \
        /opt/bridge-craftwork/scripts/deploy.sh {{SERVICE}}'

logs:
    ssh {{DROPLET}} 'cd /opt/bridge-craftwork && docker compose logs -f --tail 100 {{SERVICE}}'

shell:
    ssh -t {{DROPLET}} 'cd /opt/bridge-craftwork && docker compose exec {{SERVICE}} /bin/sh'
