.PHONY: check fmt test build

export DOCKER_HOST := unix:///Users/fnando/.config/colima/default/docker.sock

fmt:
	cargo fmt

check:
	cargo clippy --quiet -- -Dwarnings -Dclippy::all -Dclippy::pedantic

test:
	cargo test --all --all-features

build:
	bin/build
