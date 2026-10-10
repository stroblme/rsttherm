# Thin wrapper around cargo; see README.md for what the binaries do.
PREFIX ?= $(HOME)/.local
RULE := /etc/udev/rules.d/70-rsttherm.rules
# Where eframe keeps the viewer's settings and window position.
DATA := $(or $(XDG_DATA_HOME),$(HOME)/.local/share)/rsttherm

# pkg-config names the egui stack links against, and the packages providing them.
GUI_LIBS := xkbcommon wayland-client gtk+-3.0 xcb-render xcb-shape xcb-xfixes
DEB_DEPS := pkg-config libxkbcommon-dev libwayland-dev libgtk-3-dev \
	libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev
RPM_DEPS := pkgconf-pkg-config libxkbcommon-devel wayland-devel gtk3-devel libxcb-devel
ARCH_DEPS := pkgconf libxkbcommon wayland gtk3 libxcb

# Android Studio's default SDK location. Gradle finds the SDK through this, cargo-ndk the newest
# NDK under it.
export ANDROID_HOME ?= $(HOME)/Android/Sdk

.PHONY: all dep udev build headless install uninstall check fmt clippy test run clean \
	android-tools android apk apk-release apk-install

all: dep build install

# ponytail: one package list per package manager; extend the case for other distros.
dep:
	@command -v cargo >/dev/null || { echo "cargo not found, install Rust from https://rustup.rs"; exit 1; }
	@[ "$$(uname)" = Linux ] || exit 0
	@pkg-config --exists $(GUI_LIBS) 2>/dev/null && { echo "GUI dependencies present"; exit 0; }; \
	if command -v apt-get >/dev/null; then sudo apt-get update && sudo apt-get install -y $(DEB_DEPS); \
	elif command -v dnf >/dev/null; then sudo dnf install -y $(RPM_DEPS); \
	elif command -v zypper >/dev/null; then sudo zypper install -y $(RPM_DEPS); \
	elif command -v pacman >/dev/null; then sudo pacman -S --needed $(ARCH_DEPS); \
	else echo "unknown package manager, install the libraries for: $(GUI_LIBS)"; exit 1; fi

# The camera is a raw USB device; without this only root can open it.
udev:
	@[ "$$(uname)" != Linux ] || { \
		sudo install -Dm644 70-rsttherm.rules $(RULE) && \
		sudo udevadm control --reload-rules && \
		sudo udevadm trigger --subsystem-match=usb --attr-match=idVendor=3474; }

build:
	cargo build --release

headless:
	cargo build --release --no-default-features

install: build udev
	install -Dm755 target/release/rsttherm $(PREFIX)/bin/rsttherm
	@echo "installed $(PREFIX)/bin/rsttherm"

# Everything install put on the system, plus the viewer's saved settings.
# Snapshots stay: they are wherever the viewer was started from.
uninstall:
	rm -f $(PREFIX)/bin/rsttherm
	rm -rf $(DATA)
	@[ "$$(uname)" != Linux ] || [ ! -e $(RULE) ] || { \
		sudo rm -f $(RULE) && \
		sudo udevadm control --reload-rules && \
		sudo udevadm trigger --subsystem-match=usb --attr-match=idVendor=3474; }
	@echo "uninstalled rsttherm"

check: fmt clippy test

fmt:
	cargo fmt --check

clippy:
	cargo clippy --all-targets -- -D warnings

test:
	cargo test

run:
	cargo run --release -- gui

android-tools:
	rustup target add aarch64-linux-android x86_64-linux-android
	@command -v cargo-ndk >/dev/null || cargo install cargo-ndk --locked

# The viewer as a shared library, where Gradle packs it: arm64-v8a for phones, x86_64 for the
# emulator, both in one APK so sideloading has a single file.
android:
	cargo ndk -t arm64-v8a -t x86_64 -o android/app/src/main/jniLibs build --release -p rsttherm-android

# Debug-signed, which is all sideloading needs. Gradle runs `make android` itself.
apk:
	cd android && ./gradlew assembleDebug

# Signed when RSTTHERM_KEYSTORE and RSTTHERM_KEYSTORE_PASSWORD are set, as CI does for a tag;
# unsigned (app-release-unsigned.apk) otherwise.
apk-release:
	cd android && ./gradlew assembleRelease

apk-install: apk
	$(ANDROID_HOME)/platform-tools/adb install -r android/app/build/outputs/apk/debug/app-debug.apk

clean:
	cargo clean
	rm -rf android/build android/app/build android/app/src/main/jniLibs
