ARG RUST_VERSION=1.66.0
ARG CARGO_CHEF_VERSION=0.1.52
ARG BUILD_TYPE=--release

FROM rust:${RUST_VERSION} as planner
ARG CARGO_CHEF_VERSION

WORKDIR app
RUN cargo install cargo-chef --version ${CARGO_CHEF_VERSION}
COPY . .
RUN cargo chef prepare  --recipe-path recipe.json

FROM rust:${RUST_VERSION} as cacher
WORKDIR app
RUN cargo install cargo-chef
COPY --from=planner /app/recipe.json recipe.json
RUN apt-get update && apt-get -y install clang protobuf-compiler
RUN cargo chef cook ${BUILD_TYPE} --recipe-path recipe.json

FROM rust:${RUST_VERSION} as builder
WORKDIR app
COPY . .
COPY --from=cacher /app/target target
COPY --from=cacher /usr/local/cargo /usr/local/cargo
RUN cargo build ${BUILD_TYPE}

FROM rust:${RUST_VERSION} as runtime
WORKDIR app
COPY --from=builder /app/target/release/sui-data-loader /usr/local/bin
ENV RUST_BACKTRACE=full
ENTRYPOINT ["/usr/local/bin/sui-data-loader"]