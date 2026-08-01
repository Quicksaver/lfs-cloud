ARG NODE_IMAGE=node:22-trixie-slim
FROM ${NODE_IMAGE}

ARG RUST_TARGET
ARG RUST_VERSION

ENV CARGO_HOME=/cargo-cache
ENV PATH=/opt/rust/cargo/bin:/opt/rust/rustup/bin:/usr/local/bin:${PATH}
ENV RUSTUP_HOME=/opt/rust/rustup

SHELL ["/bin/bash", "-o", "pipefail", "-c"]

RUN apt-get update \
    && apt-get install --yes --no-install-recommends \
      build-essential \
      ca-certificates \
      cmake \
      curl \
      dpkg-dev \
      git \
      git-lfs \
      gnupg \
      jq \
      musl-tools \
      perl \
      python3 \
    && curl -fsSL https://packages.cloud.google.com/apt/doc/apt-key.gpg \
      | gpg --dearmor --yes --output /usr/share/keyrings/cloud.google.gpg \
    && echo "deb [signed-by=/usr/share/keyrings/cloud.google.gpg] https://packages.cloud.google.com/apt cloud-sdk main" \
      > /etc/apt/sources.list.d/google-cloud-sdk.list \
    && apt-get update \
    && apt-get install --yes --no-install-recommends google-cloud-cli \
    && rm -rf /var/lib/apt/lists/*

RUN mkdir -p /opt/rust/cargo /opt/rust/rustup /cargo-cache \
    && CARGO_HOME=/opt/rust/cargo RUSTUP_HOME=/opt/rust/rustup \
      curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
      | CARGO_HOME=/opt/rust/cargo RUSTUP_HOME=/opt/rust/rustup sh -s -- \
        --default-toolchain "$RUST_VERSION" \
        --profile minimal \
        --component clippy \
        --component rustfmt \
        --target "$RUST_TARGET" \
        --no-modify-path \
        -y \
    && CARGO_HOME=/opt/rust/cargo RUSTUP_HOME=/opt/rust/rustup \
      /opt/rust/cargo/bin/cargo install \
        cargo-audit \
        --locked \
        --root /usr/local \
        --version 0.22.2 \
    && rm -rf /opt/rust/cargo/registry /opt/rust/cargo/git \
    && corepack enable

WORKDIR /workspace
