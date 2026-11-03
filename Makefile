.PHONY: help build build-cli build-ui build-ui-rust build-ui-frontend \
       dev dev-cli dev-ui clean clean-cli clean-ui \
       install run-cli run-ui check release release-fast link-cli \
       app app-adhoc dmg sign-setup

# 本地构建说明
# - 只需 Command Line Tools，无需完整 Xcode：项目不编译 Swift，只链接系统 Swift 运行时。
# - CLT 的 Swift 运行时目录不在默认搜索路径，下面会自动追加到 RUSTFLAGS（完整 Xcode 下不需要）。
# - 固定签名：首次运行 `make sign-setup` 生成自签名证书（专用 keychain，不需要 Apple 账号），
#   之后每次构建都用同一个身份签名，重装/升级不会丢失屏幕录制、辅助功能授权。
# - 没有证书或不想用固定签名时：`make app-adhoc && make dmg`（ad-hoc，每次都要重新授权）。
# - tauri 自带的 dmg 打包依赖 Finder/AppleScript，无图形会话时会失败；`make dmg` 改用 hdiutil。
DEV_DIR := $(shell xcode-select -p 2>/dev/null)
ifeq ($(findstring CommandLineTools,$(DEV_DIR)),CommandLineTools)
export RUSTFLAGS := $(RUSTFLAGS) -L $(DEV_DIR)/usr/lib/swift/macosx
endif

ARCH := $(shell uname -m | sed 's/arm64/aarch64/')

# Default target
help:
	@echo "macrdp Makefile"
	@echo ""
	@echo "  开发:"
	@echo "    make install          安装前端依赖"
	@echo "    make build-cli        编译 CLI"
	@echo "    make dev-ui           启动 UI 开发模式 (Tauri)"
	@echo "    make dev-front        仅启动前端 dev server"
	@echo "    make run-cli          运行 CLI (端口 3389)"
	@echo "    make run-cli-ipc      运行 CLI + IPC socket"
	@echo "    make check            检查所有编译"
	@echo ""
	@echo "  发布:"
	@echo "    make release          完整发布构建 (CLI+UI, 优化, dmg; 需要 macrdp-dev 证书)"
	@echo "    make release-fast     快速发布构建 (无优化, dmg; 需要 macrdp-dev 证书)"
	@echo "    make app              编译 .app 并用固定证书签名"
	@echo "    make app-adhoc        仅编译 .app (ad-hoc 签名, 无需证书)"
	@echo "    make sign-setup       创建/解锁固定签名证书（只需一次）"
	@echo "    make dmg              hdiutil 打包 .app 为 dmg (无需 Finder)"
	@echo ""
	@echo "  清理:"
	@echo "    make clean            清理所有构建产物"
	@echo "    make clean-cli        仅清理 CLI"
	@echo "    make clean-ui         仅清理 UI"

# === CLI ===

link-cli:
	@ln -sf ../../target/debug/macrdp-server macrdp-ui/src-tauri/macrdp-server
	@mkdir -p macrdp-ui/src-tauri/target/debug
	@ln -sf ../../../../target/debug/macrdp-server macrdp-ui/src-tauri/target/debug/macrdp-server

build-cli: link-cli
	cargo build -p macrdp-server

run-cli:
	cargo run -p macrdp-server

run-cli-ipc:
	cargo run -p macrdp-server -- --ipc-socket /tmp/macrdp.sock

clean-cli:
	cargo clean

# === UI ===

install:
	cd macrdp-ui && npm install

build-ui-rust:
	cargo build --manifest-path macrdp-ui/src-tauri/Cargo.toml

build-ui-front:
	cd macrdp-ui && npm run build

dev-ui: build-cli
	cd macrdp-ui && npm run tauri dev

dev-front:
	cd macrdp-ui && npm run dev

clean-ui:
	rm -rf macrdp-ui/dist macrdp-ui/src-tauri/target

# === 发布 ===

BUNDLE_ID := com.macrdp.app
SIGN_IDENTITY := macrdp-dev
SIGN_KEYCHAIN := $(HOME)/Library/Keychains/macrdp-signing.keychain-db
SIGN_PASS_FILE := $(HOME)/.macrdp/signing/keychain.pass
APP_BUNDLE := macrdp-ui/src-tauri/target/release/bundle/macos/macrdp.app

# Tauri 自动签名：设置此环境变量后 tauri build 会自动 codesign
export APPLE_SIGNING_IDENTITY := $(SIGN_IDENTITY)

# 一次性创建自签名证书（专用 keychain，不需要 Apple 开发者账号）
sign-setup:
	@bash scripts/setup-signing.sh

# 预检固定签名：解锁专用 keychain 并确认身份可用
check-sign:
	@if [ ! -f "$(SIGN_PASS_FILE)" ]; then \
		echo "错误: 未找到固定签名证书，请先运行 make sign-setup"; exit 1; fi
	@security unlock-keychain -p "$$(cat $(SIGN_PASS_FILE))" "$(SIGN_KEYCHAIN)"
	@security find-identity -p codesigning | grep -q "$(SIGN_IDENTITY)" \
		|| (echo "错误: 未找到签名证书 $(SIGN_IDENTITY)，请运行 make sign-setup"; exit 1)
	@echo "签名证书就绪 (固定签名: $(SIGN_IDENTITY))."

# npm 的 @tauri-apps/api 比 Rust tauri crate 落后一个 minor，tauri CLI 会直接报版本不匹配；
# 两者都还是 v2、功能正常，用这个开关放行（升级 npm 包后可去掉）。
TAURI_BUILD_FLAGS := --ignore-version-mismatches

# 完整发布：全优化编译 + 自动签名 + 打包 dmg
release: check-sign
	@echo "=== 完整发布构建 (签名: $(SIGN_IDENTITY)) ==="
	cd macrdp-ui && MACRDP_CLI_PROFILE=release npm run tauri -- build $(TAURI_BUILD_FLAGS)
	@echo ""
	@echo "=== 构建完成 ==="
	@ls -lh macrdp-ui/src-tauri/target/release/bundle/dmg/*.dmg

# 快速发布：关闭优化 + 自动签名 + 打包 dmg
release-fast: check-sign
	@echo "=== 快速发布构建 (签名: $(SIGN_IDENTITY)) ==="
	cd macrdp-ui && MACRDP_CLI_PROFILE=debug npm run tauri -- build $(TAURI_BUILD_FLAGS)
	@echo ""
	@echo "=== 构建完成 ==="
	@ls -lh macrdp-ui/src-tauri/target/release/bundle/dmg/*.dmg

# 仅编译 .app，ad-hoc 签名（tauri.conf.json 的 signingIdentity "-"），不需要证书
app-adhoc:
	cd macrdp-ui && npm run tauri -- build --bundles app $(TAURI_BUILD_FLAGS)

# 编译 .app 并用固定自签名证书签名：后续重装/升级保持 TCC 授权
app:
	cd macrdp-ui && APPLE_SIGNING_IDENTITY=- npm run tauri -- build --bundles app $(TAURI_BUILD_FLAGS)
	@security unlock-keychain -p "$$(cat $(SIGN_PASS_FILE))" "$(SIGN_KEYCHAIN)"
	codesign --force --deep --options runtime --sign $(SIGN_IDENTITY) --identifier $(BUNDLE_ID) $(APP_BUNDLE)
	@codesign -dvv $(APP_BUNDLE) 2>&1 | grep -E "Identifier|Authority"

# 用 hdiutil 打包已编译的 .app，不依赖 Finder/AppleScript
dmg:
	@APP=$(APP_BUNDLE); \
	VERSION=$$(/usr/libexec/PlistBuddy -c "Print :CFBundleShortVersionString" "$$APP/Contents/Info.plist"); \
	OUT=macrdp-ui/src-tauri/target/release/bundle/dmg; \
	STAGE=$$(mktemp -d /tmp/macrdp-dmg.XXXXXX); \
	cp -R "$$APP" "$$STAGE/"; \
	ln -s /Applications "$$STAGE/Applications"; \
	mkdir -p "$$OUT"; \
	FINAL="$$OUT/macrdp_$${VERSION}_$(ARCH).dmg"; \
	rm -f "$$FINAL"; \
	hdiutil create -volname macrdp -srcfolder "$$STAGE" -ov -format UDZO "$$FINAL"; \
	rm -rf "$$STAGE"; \
	@echo ""; \
	shasum -a 256 "$$FINAL"

# === 全局 ===

build: build-cli build-ui-front build-ui-rust

clean: clean-cli clean-ui

check:
	@echo "检查 CLI..."
	@cargo build -p macrdp-server 2>&1 | tail -1
	@echo "检查 UI Rust..."
	@cargo build --manifest-path macrdp-ui/src-tauri/Cargo.toml 2>&1 | tail -1
	@echo "检查 UI 前端..."
	@cd macrdp-ui && npm run build 2>&1 | tail -1
	@echo "全部通过."
