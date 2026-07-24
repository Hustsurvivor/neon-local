FROM ubuntu:24.04

SHELL ["/bin/bash", "-o", "pipefail", "-c"]

ARG DEBIAN_FRONTEND=noninteractive
ARG DEV_UID=1000
ARG DEV_GID=1000
ARG POETRY_VERSION=1.8.5
ARG RUST_VERSION=1.88.0

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        bison \
        build-essential \
        ca-certificates \
        clang \
        cmake \
        curl \
        flex \
        git \
        gosu \
        libcurl4-openssl-dev \
        libicu-dev \
        libpq-dev \
        libprotobuf-dev \
        libreadline-dev \
        libseccomp-dev \
        libssl-dev \
        libtool \
        lsof \
        pkg-config \
        postgresql-client \
        procps \
        protobuf-compiler \
        python3 \
        python3-pip \
        python3-venv \
        zlib1g-dev \
    && rm -rf /var/lib/apt/lists/*

RUN if getent passwd "${DEV_UID}" >/dev/null; then \
        existing_user="$(getent passwd "${DEV_UID}" | cut -d: -f1)"; \
        existing_group="$(getent group "${DEV_GID}" | cut -d: -f1)"; \
        groupmod --new-name neon "${existing_group}"; \
        usermod --login neon --home /home/neon --move-home --shell /bin/bash "${existing_user}"; \
    else \
        groupadd --gid "${DEV_GID}" neon; \
        useradd --uid "${DEV_UID}" --gid "${DEV_GID}" --create-home --shell /bin/bash neon; \
    fi

ENV PATH="/home/neon/.local/bin:/opt/cargo/bin:${PATH}" \
    CARGO_HOME="/home/neon/.cargo" \
    RUSTUP_HOME="/opt/rustup" \
    CARGO_TARGET_DIR="/workspace/neon/target" \
    POETRY_HOME="/home/neon/.local"

RUN mkdir -p /opt/cargo /opt/rustup \
    && CARGO_HOME=/opt/cargo RUSTUP_HOME=/opt/rustup \
       curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
       | CARGO_HOME=/opt/cargo RUSTUP_HOME=/opt/rustup sh -s -- \
           -y --profile minimal --default-toolchain "${RUST_VERSION}" \
    && CARGO_HOME=/opt/cargo RUSTUP_HOME=/opt/rustup \
       /opt/cargo/bin/rustup component add llvm-tools rustfmt clippy \
    && chown -R neon:neon /opt/cargo /opt/rustup

USER neon
RUN curl -sSL https://install.python-poetry.org | python3 - --version "${POETRY_VERSION}"

USER root
COPY docker/local/entrypoint.sh /usr/local/bin/neon-entrypoint
RUN chmod 0755 /usr/local/bin/neon-entrypoint

WORKDIR /workspace/neon
ENTRYPOINT ["/usr/local/bin/neon-entrypoint"]
CMD ["keepalive"]
