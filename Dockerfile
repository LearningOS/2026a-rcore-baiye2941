FROM ubuntu:24.04

ARG DEBIAN_FRONTEND=noninteractive

ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin \
    RUST_VERSION=nightly-2024-05-02

# 国内镜像加速：只改变下载源，不改变安装的工具链版本
ENV RUSTUP_DIST_SERVER=https://rsproxy.cn \
    RUSTUP_UPDATE_ROOT=https://rsproxy.cn/rustup


# Match the GitHub grading environment, including the QEMU 8.2 security updates.
RUN apt-get update && apt-get install -y \
    ca-certificates curl git build-essential pkg-config python3 wget xz-utils \
    'qemu-system-misc=1:8.2.2+ds-*' 'qemu-user=1:8.2.2+ds-*' && \
    apt-get clean

RUN python3 -c 'import subprocess; versions = [subprocess.check_output([command, "--version"], text=True) for command in ("qemu-system-riscv64", "qemu-riscv64")]; print("\n".join(versions)); assert all(version.split("version ", 1)[1].split()[0] == "8.2.2" for version in versions), "This course requires QEMU 8.2.2."'

# crates.io 国内镜像，供 cargo-binutils 下载使用
RUN mkdir -p /usr/local/cargo && printf '%s\n' \
    '[source.crates-io]' \
    'replace-with = "tuna"' \
    '' \
    '[source.tuna]' \
    'registry = "sparse+https://mirrors.tuna.tsinghua.edu.cn/crates.io-index/"' \
    > /usr/local/cargo/config.toml


WORKDIR /root
RUN set -eux; \
    curl --proto '=https' --tlsv1.2 -fsSL --retry 3 https://sh.rustup.rs -o /root/rustup-init; \
    chmod +x /root/rustup-init; \
    /root/rustup-init -y --no-modify-path --profile minimal \
        --default-toolchain "$RUST_VERSION" \
        --component rust-src,llvm-tools-preview,rustfmt,clippy \
        --target riscv64gc-unknown-none-elf; \
    rm /root/rustup-init; \
    cargo install cargo-binutils --version 0.4.0 --locked; \
    rustc --version; \
    cargo --version; \
    rust-objcopy --version

WORKDIR /mnt
CMD ["/bin/bash"]
