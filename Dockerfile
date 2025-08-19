# Use Rust official image as base
FROM rust:latest AS builder

# Set working directory
WORKDIR /app

# Copy Cargo files
COPY Cargo.toml Cargo.lock ./

# Copy source code
COPY src ./src

# Build the application
RUN cargo build --release

# Runtime stage
FROM debian:bookworm-slim

# Install runtime dependencies
#RUN apt-get update && apt-get install -y \
#    ca-certificates \
#    && rm -rf /var/lib/apt/lists/*

# Create app user
RUN useradd -m -u 1001 app

# Set working directory
WORKDIR /app

# Copy binary from builder stage
COPY --from=builder /app/target/release/Uart2Moon /app/uart2moon

# Change ownership to app user
RUN chown -R app:app /app

# Switch to app user
USER app

# Set environment variables for test mode by default
ENV TEST_MODE=true
ENV SOCKET_PATH=/tmp/printer
ENV DEVICE=/dev/ttyUSB0
ENV BAUD_RATE=115200
ENV VERBOSE=false

# Expose the socket directory
VOLUME ["/tmp"]

# Run the application
CMD ["./uart2moon"]