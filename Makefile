.PHONY: check test lint build-cluster build-cluster-gnu clean-highs sync
TARGET := x86_64-unknown-linux-musl
SSH ?= ssh
REMOTE ?= lido
REMOTE_DIR ?= gisard

lint:
	cargo clippy --workspace --all-targets
	cargo clippy --workspace --all-targets --features visualizer
	cargo clippy --workspace --all-targets --features dashboard

test:
	cargo test --workspace
	cargo test --workspace --features dashboard

check: lint test

clean-highs:
	rm -rf target/$(TARGET)/release/build/highs-sys-* target/release/build/highs-sys-*

build-cluster: clean-highs
	CC_x86_64_unknown_linux_musl=$(PWD)/tools/zig-musl-cc \
	CXX_x86_64_unknown_linux_musl=$(PWD)/tools/zig-musl-cxx \
	RUSTFLAGS="-C target-cpu=broadwell" \
	  cargo zigbuild --release --target $(TARGET) --bins --features dashboard
	@git rev-parse HEAD > target/$(TARGET)/release/campaign.source-revision
	@if git diff --quiet --ignore-submodules HEAD && \
	  test -z "$$(git ls-files --others --exclude-standard)"; then \
	  printf '0\n' > target/$(TARGET)/release/campaign.source-dirty; \
	else \
	  printf '1\n' > target/$(TARGET)/release/campaign.source-dirty; \
	fi
	@file target/$(TARGET)/release/campaign
	@file target/$(TARGET)/release/campaign_dashboard
	@ls -la target/$(TARGET)/release/campaign
	@ls -la target/$(TARGET)/release/campaign_dashboard

build-cluster-gnu: clean-highs
	RUSTFLAGS="-C target-cpu=broadwell" \
	  cargo zigbuild --release --target x86_64-unknown-linux-gnu.2.17 --bins

sync: build-cluster
	$(SSH) "$(REMOTE)" mkdir -p "$(REMOTE_DIR)/bin" "$(REMOTE_DIR)/lido"
	RSYNC_RSH="$(SSH)" rsync -av target/$(TARGET)/release/campaign $(REMOTE):$(REMOTE_DIR)/bin/
	RSYNC_RSH="$(SSH)" rsync -av target/$(TARGET)/release/campaign_dashboard $(REMOTE):$(REMOTE_DIR)/bin/
	RSYNC_RSH="$(SSH)" rsync -av target/$(TARGET)/release/campaign.source-revision target/$(TARGET)/release/campaign.source-dirty $(REMOTE):$(REMOTE_DIR)/bin/
	RSYNC_RSH="$(SSH)" rsync -av lido/ $(REMOTE):$(REMOTE_DIR)/lido/
