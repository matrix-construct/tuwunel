variable "TUWUNEL_VERSION" {
  default = "1.0.0-snapshot"
}
variable "RUST_VERSION" {
  default = "1.86"
}
variable "FEATURES" {
  default = "--all-features"
}

variable "REPO" {
  default = ""
  description = "must be a repository name with a trailing '/' or an empty string"
}

group "default" {
  targets = [
    "rust-base",
    "build-release",
    #"build-release-artifact"
  ]
}

target "build-release-artifact" {
  contexts = {
    build-release = "target:build-release"
  }
  output = ["type=local,dest=/tmp/target/"] # vorher $(pwd)/target, aber das könnte probleme versursachen wegen WSL und dem windows folder
  dockerfile-inline = <<EOF
  FROM scratch AS artifact
  COPY --from=build-release /opt/output/ /
  EOF
}

target "build-release" {
  description = "Optimized build container for releases with separated cache"
  contexts = {
    context = "."
    rust-base = "target:rust-base"
  }
  tags = ["${REPO}build:${RUST_VERSION}"]
  output = ["type=oci,dest=builder-image,tar=false"]

  cache_from = ["type=inline"]
  cache_to = ["type=inline"]

  labels = {
    "_group" = "build"
    "_cache" = "inline"
  }

  dockerfile-inline = <<EOF
    FROM rust-base
    ENV CARGO_HOME=/opt/cargo_home
    WORKDIR /opt/build

    # Create necessary folders
    RUN mkdir -p /opt/build/src /opt/build/target

    # Separate caches for different build profiles
    RUN --mount=type=cache,target=/opt/build/target/release \
        --mount=type=cache,target=/opt/build/target/release-debuginfo \
        --mount=type=cache,target=/opt/cargo_home,sharing=locked \
        true

    # Copy only dependency files first
    COPY Cargo.toml Cargo.lock ./

    # Fetch dependencies (so this layer can be cached separately)
    RUN cargo fetch

    # Now copy the actual source code
    COPY src/ ./src/

    # Build "release-debuginfo" profile first
    RUN --mount=type=cache,target=/opt/build/target/release-debuginfo,sharing=locked \
        --mount=type=cache,target=/opt/cargo_home,sharing=locked \
        cargo build --profile release-debuginfo ${FEATURES}

    # Then build true "release" profile
    RUN --mount=type=cache,target=/opt/build/target/release,sharing=locked \
        --mount=type=cache,target=/opt/cargo_home,sharing=locked \
        cargo build --release ${FEATURES}

    # Prepare "release" output
    RUN mkdir -p /opt/output/release && \
        cd /opt/build/target/release && \
        find . -maxdepth 2 -mindepth 2 \( -type f -o -type d \) \
          ! -path "./.fingerprint*" ! -path "./build*" ! -path "./deps*" ! -path "./incremental*" \
          -exec cp -r '{}' /opt/output/release ';'

    # Prepare "release-debuginfo" output
    RUN mkdir -p /opt/output/release-debuginfo && \
        cd /opt/build/target/release-debuginfo && \
        find . -maxdepth 2 -mindepth 2 \( -type f -o -type d \) \
          ! -path "./.fingerprint*" ! -path "./build*" ! -path "./deps*" ! -path "./incremental*" \
          -exec cp -r '{}' /opt/output/release-debuginfo ';'
EOF
}

# Todo use " release-debuginfo" instead of "dev" for debug release
target "build-release-old" {
  description = "Build Container for releases"
  contexts = {
    context = "."
    rust-base = "target:rust-base"
  }
  tags = ["${REPO}build:${RUST_VERSION}"]
  output = ["type=oci,dest=builder-image,tar=false"]
  cache_to = ["type=inline"]
  cache_from = ["type=inline"]
  labels = {
    "_group" = "build"
    "_cache" = "inline"
  }
  dockerfile-inline = <<EOF
    FROM rust-base
    ENV CARGO_HOME=/opt/cargo_home
    WORKDIR /opt/build
    RUN --mount=type=cache,dst=/opt/build/target/,sharing=locked \
        --mount=type=cache,dst=/opt/cargo_home,sharing=locked
    COPY ./ ./
    RUN cargo build --profile release-debuginfo ${FEATURES}
    RUN cargo build --release ${FEATURES}

    # Prepare "debug" target for output
    #RUN cp -r /opt/build/target/debug /opt/prepare-debug
    #RUN rm -rf /opt/prepare-debug/.fingerprint && rm -rf /opt/prepare-debug/build && rm -rf /opt/prepare-debug/deps

    # Preapre "release" target for output
    #RUN cp -r /opt/build/target/release /opt/prepare-release
    #RUN rm -rf /opt/prepare-release/.fingerprint && rm -rf /opt/prepare-release/build && rm -rf /opt/prepare-release/deps

    # release: copy folders & files except build/.fingerprint/deps/incremental
    RUN cd /opt/build/target/release && mkdir -p /opt/output/release
    RUN find -maxdepth 2 -mindepth 2 -type d -not -ipath './.fingerprint*' -not -ipath './build*' -not -ipath './deps*' -not -ipath './incremental*' -exec cp -r '{}' '/opt/output/release' ';'
    RUN find -maxdepth 2 -mindepth 2 -type f -not -ipath './.fingerprint*' -not -ipath './build*' -not -ipath './deps*' -not -ipath './incremental*' -exec cp -r '{}' '/opt/output/release' ';'

    # debuginfo : copy folders & files except build/.fingerprint/deps/incremental
    RUN cd /opt/build/target/release-debuginfo && mkdir -p /opt/output/release-debuginfo
    RUN find -maxdepth 2 -mindepth 2 -type d -not -ipath './.fingerprint*' -not -ipath './build*' -not -ipath './deps*' -not -ipath './incremental*' -exec cp -r '{}' '/opt/output/release-debuginfo' ';'
    RUN find -maxdepth 2 -mindepth 2 -type f -not -ipath './.fingerprint*' -not -ipath './build*' -not -ipath './deps*' -not -ipath './incremental*' -exec cp -r '{}' '/opt/output/release-debuginfo' ';'
EOF
}

# https://github.com/rust-lang/docker-rust/blob/master/Dockerfile-debian.template
target "rust-base" {
  description = "Base for building with rust and dependencies installed"

  contexts = {
    rust = "docker-image://rust:${RUST_VERSION}"
  }
  tags = ["${REPO}rust-base:${RUST_VERSION}"]

  # Output
  # https://docs.docker.com/reference/cli/docker/buildx/build/#output
  # https://docs.docker.com/build/exporters/oci-docker/
  #output = ["type=docker,compression=zstd,mode=min"]
  output = ["type=image"]

  # Cache
  # https://docs.docker.com/build/cache/backends/
  cache_to = ["type=inline"]
  cache_from = ["type=inline"]

  labels = {
    "_group" = "build"
    "_cache" = "inline"
  }

  dockerfile-inline = <<EOF
    FROM rust
    ENV packages="\
    bzip2 \
    ca-certificates \
    clang \
    cmake \
    curl \
    git \
    libc6-dev \
    make \
    pkg-config \
    pkgconf \
    xz-utils \
    libjemalloc-dev \
    liburing-dev \
    libzstd-dev \
    liburing2 \
    libzstd1 \
    libjemalloc2 \
    "
    RUN apt-get update && apt-get -y install --no-install-recommends $packages
  EOF

}