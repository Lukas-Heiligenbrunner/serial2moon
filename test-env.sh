#!/bin/bash

# Test script for the development environment
echo "Testing uart2moon development environment..."

echo "1. Testing uart2moon binary in test mode..."
# Test the application directly
./target/release/Uart2Moon --help 2>/dev/null || echo "Binary not found - please run 'cargo build --release' first"

echo ""
echo "2. Testing Docker compose configuration..."
if command -v docker-compose &> /dev/null; then
    echo "Docker Compose is available"
    docker-compose config --quiet && echo "docker-compose.yml is valid" || echo "docker-compose.yml has syntax errors"
else
    echo "Docker Compose not available - install it to test the full environment"
fi

echo ""
echo "3. Testing config files..."
if [ -f "config/moonraker.conf" ]; then
    echo "✓ Moonraker configuration exists"
else
    echo "✗ Moonraker configuration missing"
fi

if [ -f "Dockerfile" ]; then
    echo "✓ Dockerfile exists"
else
    echo "✗ Dockerfile missing"
fi

echo ""
echo "4. Checking CI workflow..."
if [ -f ".github/workflows/ci.yml" ]; then
    echo "✓ GitHub Actions CI workflow exists"
else
    echo "✗ CI workflow missing"
fi

echo ""
echo "To start the development environment, run:"
echo "  docker-compose up -d"
echo ""
echo "Then access:"
echo "  - Mainsail: http://localhost:8080"
echo "  - Moonraker API: http://localhost:7125"