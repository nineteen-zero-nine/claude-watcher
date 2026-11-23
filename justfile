# List the recipes.
default:
    @just --list

# Build the release binary and install it into cargo's bin directory.
install:
    cargo install --path . --locked
