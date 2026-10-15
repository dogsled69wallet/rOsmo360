# Cross-build image for the Windows binary: Rust's x86_64-pc-windows-gnu target
# linked by MinGW. The result needs only DLLs Windows has (msvcrt, kernel32, …).
FROM rust:1.97-bookworm
RUN apt-get update && apt-get install -y --no-install-recommends gcc-mingw-w64-x86-64 && rm -rf /var/lib/apt/lists/*
RUN rustup target add x86_64-pc-windows-gnu && chmod -R a+rwX "$CARGO_HOME" "$RUSTUP_HOME"
